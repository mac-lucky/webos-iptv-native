//! **The probe before the Load, as a resource**: its thread, its result channel and its time
//! budget. Held by the adapter ([`super::adapter::PlayerAdapter`]) like every other OS resource of a
//! playback; the decisions it feeds are the session's (`session::PlaybackSession::resolve`).
//!
//! The flow, all on the main thread except the probe itself:
//!
//! 1. `begin_playback` installs a start with no declaration; the session is UNRESOLVED.
//! 2. `engine::start_bufferfeed_tracked` sees that and spawns [`PendingProbe`] instead of a Load
//!    (`BufferfeedStartOutcome::Probing`); `player::state` reads `Connecting`.
//! 3. Every frame, `pump` polls it ([`PendingProbe::poll`]). A result resolves the session and the
//!    start goes on to the native Load; a failure, or no result inside [`PROBE_BUDGET`], is an
//!    explicit refusal (`StartRefusal::Probe`) — never a spinner.
//! 4. A stop, a new start or a suspend cancels it ([`PendingProbe::cancel`]), which returns at once.

use crate::ff::probe::ProbeIo;
use crate::headers::RequestHeaders;
use crate::stream::{ProbeFailure, ProbedStreams};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The longest a probe may take before the start is refused with `ProbeFailure::Timeout`. Opening a
/// remote Matroska file and reading its streams is a connect, a TLS handshake and a few hundred KB
/// to a few MB (`avformat_find_stream_info`'s default `probesize` is 5 MB) — seconds on a slow
/// link, not tens of them. The transports carry their own connect and stall bounds; this one is the
/// promise that the player leaves `Connecting` whatever they do.
pub const PROBE_BUDGET: Duration = Duration::from_secs(30);

type ProbeResult = Result<ProbedStreams, ProbeFailure>;

/// One probe in flight.
pub struct PendingProbe {
    /// The stream being probed: a result is only ever applied to the start it was made for.
    url: String,
    io: Arc<ProbeIo>,
    rx: Receiver<ProbeResult>,
    began: Instant,
}

impl PendingProbe {
    /// Start probing `url` on a thread of its own.
    pub fn spawn(url: String, headers: RequestHeaders) -> Result<PendingProbe, ProbeFailure> {
        let io = ProbeIo::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let (thread_io, thread_url) = (Arc::clone(&io), url.clone());
        // Detached: a cancelled probe finishes (or fails) on its own and frees its transports
        // itself, so a cancel never waits on a connect.
        let started = tv_base::task::spawn("probe", move || {
            let _ = tx.send(crate::ff::probe::probe(&thread_url, &headers, &thread_io));
        })
        .is_some();
        if !started {
            return Err(ProbeFailure::Unavailable);
        }
        Ok(PendingProbe { url, io, rx, began: Instant::now() })
    }

    /// The stream this probe reads.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Non-blocking: the result once the probe has one, `Err(Timeout)` once [`PROBE_BUDGET`] is
    /// spent (the probe is cancelled then), `None` while it is still working.
    pub fn poll(&mut self) -> Option<ProbeResult> {
        self.poll_at(Instant::now())
    }

    fn poll_at(&mut self, now: Instant) -> Option<ProbeResult> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            // The thread is gone without a word: it panicked inside the probe.
            Err(TryRecvError::Disconnected) => Some(Err(ProbeFailure::Unavailable)),
            Err(TryRecvError::Empty) if now.saturating_duration_since(self.began) >= PROBE_BUDGET => {
                super::log(&format!("probe: no answer within {}s — cancelled", PROBE_BUDGET.as_secs()));
                self.io.cancel();
                Some(Err(ProbeFailure::Timeout))
            }
            Err(TryRecvError::Empty) => None,
        }
    }

    /// Stop the probe. Returns at once; the thread unwinds its I/O and exits on its own.
    pub fn cancel(self) {
        self.io.cancel();
    }

    /// A probe whose result the test supplies, started at `began`.
    #[cfg(test)]
    fn for_test(began: Instant) -> (PendingProbe, std::sync::mpsc::Sender<ProbeResult>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (PendingProbe { url: "https://cdn.example/v.mkv".into(), io: ProbeIo::new(), rx, began }, tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A probe that has not answered is still `None` inside the budget and a TIMEOUT past it —
    /// the bound that keeps `Connecting` from being forever.
    #[test]
    fn a_silent_probe_times_out_at_the_budget_and_is_cancelled() {
        let t0 = Instant::now();
        let (mut p, _tx) = PendingProbe::for_test(t0);
        assert_eq!(p.poll_at(t0 + PROBE_BUDGET - Duration::from_millis(1)), None);
        assert_eq!(p.poll_at(t0 + PROBE_BUDGET), Some(Err(ProbeFailure::Timeout)));
    }

    /// An answer is delivered as it is; a probe thread that died without one is a failure, not a
    /// wait for an answer that cannot come.
    #[test]
    fn a_result_is_delivered_and_a_vanished_probe_is_a_failure() {
        let t0 = Instant::now();
        let (mut p, tx) = PendingProbe::for_test(t0);
        tx.send(Err(ProbeFailure::NoVideo)).unwrap();
        assert_eq!(p.poll_at(t0), Some(Err(ProbeFailure::NoVideo)));
        drop(tx);
        assert_eq!(p.poll_at(t0), Some(Err(ProbeFailure::Unavailable)));
        p.cancel();
    }

    /// The real thing, end to end, with no FFmpeg bound (the plain host suite): the probe answers
    /// with an explicit refusal, promptly.
    #[test]
    fn a_spawned_probe_without_ffmpeg_answers_unavailable() {
        let _g = tv_base::testlock::serial();
        let mut p = PendingProbe::spawn("http://127.0.0.1:1/film.mkv".into(), RequestHeaders::none()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let r = loop {
            if let Some(r) = p.poll() {
                break r;
            }
            assert!(Instant::now() < deadline, "the probe never answered");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(r, Err(ProbeFailure::Unavailable));
        assert_eq!(p.url(), "http://127.0.0.1:1/film.mkv");
    }
}
