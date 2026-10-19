//! player::threads — the worker threads beside the demuxer (load / progress) + the
//! SendPtr spawn seam. The demux thread body is `ff::demux` (libavformat over a custom
//! AVIO on stream.rs); its HttpStream box lives in the Engine (main owns it, closes it
//! to interrupt, and outlives the threads).
use super::SHARED;
use std::os::raw::c_char;
use std::sync::atomic::Ordering;

/// raw ptr we assert is Send for the spawn (the boxes/queue outlive the thread).
pub struct SendPtr<T>(pub *mut T);
unsafe impl<T> Send for SendPtr<T> {}

/// media/load thread: construct + Load (uid=NULL). The library owns its own GMainContext + loop,
/// and callbacks arrive on its thread — but Load itself is SYNCHRONOUS here and on some chassis
/// blocks for a long time inside video-output init (issue #74, k5lp/k3lp), which is why the C seam
/// refuses every other verb until it returns (`g_load_returned`) and the pump bounds the wait
/// with `NATIVE_LOAD_BUDGET`.
pub fn load_thread(payload: SendPtr<c_char>, native_epoch: u32) {
    super::log("SMP: calling Load (uid=NULL)");
    let ok = unsafe { super::sink().load(payload.0, native_epoch) };
    super::log(&format!("SMP: Load returned ok={ok}"));
    // `/tmp/webosiptv-holdload[=ms]`: on demand, hold the flip below so issue #74 D.1's budget
    // (the pump's "deferring" line, then, past `NATIVE_LOAD_BUDGET`, the failure read-out) is
    // observable on a real television without a set that hangs here for real. Read AFTER
    // `sf_load` returns and BEFORE `mark_native_load_returned`, matching where this needs to bite.
    if let Some(ms) = tv_base::devtrig::holdload_delay_ms() {
        super::log(&format!(
            "holdload: armed — holding the Load-returned flag for {ms}ms"
        ));
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
    // issue #74 D.1: flip the epoch-scoped gate BEFORE `load_failed` is set below — `pump.rs`'s
    // loadCompleted arm (and every other Starfish/ACB verb) must never observe "Load has
    // returned" any later than this.
    if let Some(elapsed) = SHARED.mark_native_load_returned(native_epoch) {
        // Captured HERE, at the moment the flag actually flips, so the "native: Load returned
        // after Nms" log measures the real in-flight duration rather than however long it took
        // the main-thread pump to next run and notice. Logged unconditionally, right at the gate
        // transition, rather than from `pump.rs`'s `loadCompleted` arm — that arm only runs once
        // `loadCompleted` also arrives, so on a session where Load returns but `loadCompleted`
        // never does, the old placement left this line unlogged forever, with no way to tell
        // "Load is still on the stack" from "Load returned and the pipeline went quiet". See
        // finding `load-returned-log-is-conditional-on-loadcompleted-and-unbounded-after`.
        let elapsed_ms = elapsed.as_millis() as u64;
        SHARED
            .native_load_elapsed_ms
            .store(elapsed_ms, Ordering::Relaxed);
        super::log(&format!("native: Load returned after {elapsed_ms}ms"));
    }
    if ok == 0 {
        // Publish it. This used to be logged and discarded, so a refused payload was
        // indistinguishable from a slow one and the pump waited on a `loadCompleted` that could
        // never come.
        SHARED.publish_native_load_failure(native_epoch);
    }
}

/// How long the progress reporter waits between reports.
const REPORT_INTERVAL_S: u64 = 10;

/// One reporter's stop signal — **owned by that reporter and its Engine**, not shared.
///
/// A flag in `SHARED` would be cleared by `reset_session` at the end of teardown, so a reporter
/// still alive at that moment (inside a slow observer call) would come back to a cleared flag and
/// loop forever, which would force teardown to JOIN it on the main thread. Per-session ownership
/// makes the stop unambiguous: a detached reporter always sees ITS OWN flag set and exits, no
/// matter what the next session does to `SHARED`, which is what lets the join move off the main
/// thread.
pub struct ReportStop {
    flag: std::sync::Mutex<bool>,
    cv: std::sync::Condvar,
}

impl ReportStop {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(ReportStop {
            flag: std::sync::Mutex::new(false),
            cv: std::sync::Condvar::new(),
        })
    }

    /// Tell this reporter to exit, and wake it so it notices now rather than up to 10 s from now.
    pub fn stop(&self) {
        *self.flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.cv.notify_all();
    }

    /// Wait up to `secs`, returning `true` as soon as [`stop`](Self::stop) has been called.
    ///
    /// The loop re-checks the predicate because `wait_timeout` may wake spuriously, and a spurious
    /// wake must not shorten the interval into an early extra report.
    fn wait_or_stop(&self, secs: u64) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        let mut g = self.flag.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if *g {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            g = self
                .cv
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// The playback's position, paused flag and duration as an observer is told them.
pub fn progress_now() -> crate::session::Progress {
    crate::session::Progress {
        position_ms: SHARED.playpos_ns.load(Ordering::Relaxed).max(0) / 1_000_000,
        duration_ms: SHARED.duration_ns.load(Ordering::Relaxed).max(0) / 1_000_000,
        paused: super::TX.paused.load(Ordering::Relaxed),
    }
}

/// The ~10 s progress reporter: hands the playback's position to its observer
/// ([`crate::session::PlaybackObserver::progress`]) until the Engine's teardown stops it. Nothing
/// is reported before the first frame (the position would be the pre-roll's 0).
pub fn progress_thread(
    observer: std::sync::Arc<dyn crate::session::PlaybackObserver>,
    stop: std::sync::Arc<ReportStop>,
) {
    loop {
        if stop.wait_or_stop(REPORT_INTERVAL_S) {
            return;
        }
        if !SHARED.seen_frame.load(Ordering::Relaxed) {
            continue;
        }
        observer.progress(progress_now());
    }
}
