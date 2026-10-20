//! player::engine - the main-thread-confined session object (Engine) + lifecycle
//! (acb_init / start_bufferfeed / stop_bufferfeed) + the feed loops. No worker
//! thread ever names an Engine field: race-free by confinement (like the C
//! main-thread-only flags). The Engine owns the HttpStream box + the AuQueue
//! boxes; it hands raw ptrs to the workers and outlives them (drops after join).
//!
//! That confinement is **enforced, not just asserted**: the slot is `App.adapters.player`, a
//! [`crate::player::adapter::PlayerAdapter`] that consumes a [`MainThread`] at construction, so a
//! `&mut PlayerAdapter` borrow is what confines it now (see `player::adapter`). Which is also the
//! rule for the rest of this file - a function here takes the token **iff** it reaches the slot
//! or the ACB/Starfish seam, so the parameter carries information. `arm_seek` and `begin_resume`
//! run on the main thread too and deliberately do not take one: they only publish to `SHARED`.
use super::shared::{PlayCompletion, PrimeKind, Stage};
use super::SeekMode;
use super::{log, sink, threads, ACB_OK, PTYPE, SHARED, TX};
use crate::aq::{AuNode, AuQueue};
use tv_net::stream::HttpStream;
use tv_base::task::MainThread;
use tv_platform::tv::sink::UnloadTicket;

use std::os::raw::{c_char, c_int, c_long, c_void};
use std::sync::atomic::{AtomicI64, Ordering};

// BUFFERSTREAM Load payloads (ss4s shape). Video-only for the local sample path;
// video+AC3 for streaming. Copied VERBATIM from playback.c.
//
// `@APPID@` is substituted by `with_app_id` at the single choke point every variant passes
// through. It is a PLACEHOLDER rather than the literal it used to be because two installs can now
// sit on one television (`paths::app_id`), and a developer build announcing the SHIPPED app's id
// would be a false statement to the media pipeline about which application it is.
//
// WHAT THIS FIELD IS ACTUALLY FOR IS NOT KNOWN HERE, and the distinction matters because the two
// app ids this app sends travel different paths into different libraries:
//
//   * the ACB id (`acb_create` -> `AcbAPI_initialize`) has real evidence behind it. LG's own
//     `libcbe` keys media metadata on `{"appId":...,"pipelineId":...}` - recovered from this
//     television's binaries, `docs/dolby-vision.md` §3 - so the id reaches a subsystem that reads
//     it. What it does with it there is still inferred, not traced.
//   * `option.appId` in the Load payload, this key, has NOT been traced into `libpf` at all.
//     `tools/fwcompat.py` is a symbol inventory and a JSON key path lives in `.rodata`, so no tool
//     in this repository can answer it; `.agents/skills/decompile-tv-lib/` is the only route, and
//     `docs/two-installs.md` §7 states the question rather than an answer.
//
// So: sending the true id is the conservative move on a field whose consumer is unknown, not a fix
// for a mechanism anyone here has read. The failure it guards against - if it guards against one -
// is a black video plane with working audio and no error line anywhere, which is why it is worth
// making correct by construction rather than finding out on a television.
//
// A placeholder rather than `format!`-ing the constant: `with_window_id` splices `option.windowId`
// onto this exact key, and deriving both from one source is what makes it impossible for the
// anchor and the payload to drift apart. For the shipped app the composed bytes and the key order
// are identical to what every release so far sent - asserted in the tests below, because the
// webOS 5+ splice path is one this project's 4.5 dev set cannot exercise.
const PAYLOAD_V: &str = r#"{"args":[{"mediaTransportType":"BUFFERSTREAM","option":{"appId":"@APPID@","externalStreamingInfo":{"contents":{"codec":{"video":"H264"},"esInfo":{"pauseAtDecodeTime":false,"ptsToDecode":0,"seperatedPTS":true},"format":"RAW","provider":"webosiptv"},"streamQualityInfo":true,"streamQualityInfoNonFlushable":true,"audioSync":true,"restartStreaming":false,"bufferingCtrInfo":{"bufferMaxLevel":0,"bufferMinLevel":0,"preBufferByte":0,"qBufferLevelAudio":0,"qBufferLevelVideo":0,"srcBufferLevelAudio":{"minimum":1,"maximum":32768},"srcBufferLevelVideo":{"minimum":1,"maximum":8388608}}},"needAudio":false,"queryPosition":false,"lowDelayMode":true,"transmission":{"contentsType":"LIVE"},"adaptiveStreaming":{"audioOnly":false,"maxWidth":1920,"maxHeight":1080,"maxFrameRate":30}}}]}"#;
// NB: pauseAtDecodeTime stays FALSE here. Kodi uses true, but only alongside its decode-time
// trigger machinery (setTimeToDecode); with true and no trigger the decoder never starts
// (verified on-device: Load+Play OK but zero frames decoded). The feed-ahead throttle
// (MAX_FEED_AHEAD_NS in feed_stream) is the anti-stall mechanism; the other Kodi payload
// flags are being re-introduced one at a time.
const PAYLOAD_AV: &str = r#"{"args":[{"mediaTransportType":"BUFFERSTREAM","option":{"appId":"@APPID@","externalStreamingInfo":{"contents":{"codec":{"video":"H264","audio":"AC3"},"esInfo":{"pauseAtDecodeTime":false,"ptsToDecode":0,"seperatedPTS":true},"format":"RAW","provider":"webosiptv"},"streamQualityInfo":true,"streamQualityInfoNonFlushable":true,"audioSync":true,"restartStreaming":false,"bufferingCtrInfo":{"bufferMaxLevel":0,"bufferMinLevel":0,"preBufferByte":0,"qBufferLevelAudio":0,"qBufferLevelVideo":0,"srcBufferLevelAudio":{"minimum":1,"maximum":1048576},"srcBufferLevelVideo":{"minimum":1,"maximum":8388608}}},"needAudio":true,"queryPosition":false,"lowDelayMode":false,"transmission":{"contentsType":"LIVE"},"adaptiveStreaming":{"audioOnly":false,"maxWidth":1920,"maxHeight":1080,"maxFrameRate":30}}}]}"#;
// Phase 0 HEVC probe payload - identical to PAYLOAD_V but codec video "H265", to isolate
// the single variable: does StarfishMediaAPIs BUFFERSTREAM decode HEVC on this panel?
/// **`streamQualityInfoNonFlushable: true` is the presented-frame counter, and it is the only
/// per-frame instrument this app has on a non-Dolby path.** Decompiled from libpf 2026-09-03
/// (`CustomPipeline::updatePeriodicalInfo`, the same 200 ms timer as the position tick):
/// with `streamQualityInfo` the pipeline reads the sink's `dropped-frames` every 200 ms and
/// forwards a non-zero count as callback type 46; with this second key it also reads
/// `non-flushable-displayed-frames` and forwards it as type 47. Both arrive at `sf_on_event`
/// and are logged as `smp_cb type=46/47 num=<count>`, so consecutive samples against the log's
/// millisecond stamps ARE the presented cadence - the number the 5 Hz `vtick` and our GL `fps=`
/// could never give. Whether this firmware's sink exposes the property is answered by the first
/// run: libpf logs a pmlog error naming the property if it does not.
const PAYLOAD_H265: &str = r#"{"args":[{"mediaTransportType":"BUFFERSTREAM","option":{"appId":"@APPID@","externalStreamingInfo":{"contents":{"codec":{"video":"H265"},"esInfo":{"pauseAtDecodeTime":false,"ptsToDecode":0,"seperatedPTS":true},"format":"RAW","provider":"webosiptv"},"streamQualityInfo":true,"streamQualityInfoNonFlushable":true,"audioSync":true,"restartStreaming":false,"bufferingCtrInfo":{"bufferMaxLevel":0,"bufferMinLevel":0,"preBufferByte":0,"qBufferLevelAudio":0,"qBufferLevelVideo":0,"srcBufferLevelAudio":{"minimum":1,"maximum":32768},"srcBufferLevelVideo":{"minimum":1,"maximum":8388608}}},"needAudio":false,"queryPosition":false,"lowDelayMode":true,"transmission":{"contentsType":"LIVE"},"adaptiveStreaming":{"audioOnly":false,"maxWidth":3840,"maxHeight":2160,"maxFrameRate":60}}}]}"#;

// ACCEPTED AUs - what the pipeline took. This is what `app::diagnostics` reports, because a count of
// attempts reads as healthy throughput through a stall: a full sink retains the AU and it is
// re-offered every tick.
static VTOT: AtomicI64 = AtomicI64::new(0);
static ATOT: AtomicI64 = AtomicI64::new(0);
// ATTEMPTS - the log's own cadence, kept separate because its `reply=` field is the only record
// of a REJECTED feed and `tests/run.py` greps `feed v#` / `feed a#`.
static VATT: AtomicI64 = AtomicI64::new(0);

/// Set by the probe when the stream's timeline starts far from 0 (`stream::needs_fresh_rebase`);
/// the next engine consumes it into `rebase_pending`.
pub(crate) static FRESH_REBASE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static AATT: AtomicI64 = AtomicI64::new(0);

/// AUs fed to the Starfish pipeline this SESSION, video and audio.
///
/// Zeroed by `start_bufferfeed`, not cumulative since boot: the diagnostics read-out has to answer
/// "is this playback feeding?", and a number carried over from the last item answers a question
/// nobody asked. The log's `v <= 4 || v % 100 == 0` cadence restarting per session is the behaviour
/// that was wanted there too.
pub fn fed_totals() -> (i64, i64) {
    (VTOT.load(Ordering::Relaxed), ATOT.load(Ordering::Relaxed))
}

/// The AU queues' BASE byte caps - the denominators the read-out shows `dg_aq_*` against, so a viewer
/// can see backpressure (a video lane pinned at its cap is the demuxer outrunning the decoder;
/// both lanes empty while Stage is Playing is the opposite).
///
/// **Base, not the cap in force.** For the first seconds of a file whose audio starts so late that
/// the video lane fills to this cap before the first audio packet, the demuxer lifts the video
/// lane's cap by `ff::PRE_AUDIO_VIDEO_HEADROOM` (24 MiB) and then walks it back down as the
/// consumer drains, so the read-out can show a video lane ABOVE this number then. That is the
/// headroom doing its job, not an accounting error. An ordinary file never sees it: the grant is
/// taken only when a video push would park at this cap with no audio yet.
pub const fn aq_caps() -> (i64, i64) {
    (AQ_VIDEO_BYTES as i64, AQ_AUDIO_BYTES as i64)
}

/// **The feed-ahead throttle's two leads, milliseconds** - video, then audio.
///
/// The other half of the reachable-reserve geometry: `B_max = lead + queue_bytes/rate`, and
/// `aq_caps` above exposes only the `queue_bytes`. Published as functions so a reader computes
/// the reserve from the pipeline's own values rather than from a transcription of them.
pub const fn feed_leads_ms() -> (i64, i64) {
    (
        MAX_FEED_AHEAD_NS / 1_000_000,
        (MAX_FEED_AHEAD_NS + AUDIO_SLACK_NS) / 1_000_000,
    )
}

// Per-lane queue byte caps (two-lane feed). Audio is kept small (the TV is RAM-tight and audio
// frames are tiny) yet large enough to cushion the single demux thread briefly blocking on a full
// video lane.
//
// **The video cap is 48 MiB, and it stopped matching the Load payload's `srcBufferLevelVideo`
// (8 MiB) on purpose.** Those are two different buffers: this one sits between our demux thread
// and our pump, and that one is what Starfish will hold. They were equal because 8 MiB was copied
// from the payload, not because anything requires it - feeding past Starfish's own level returns
// `'B'` and the pump retries, which is the backpressure path every playback already uses.
//
// **Why 48 MiB.** The reserve the pipeline can ever hold ahead of the playhead is
// `B_max = feed_lead + queue_bytes*8 / R_elementary`, a queue term that falls as `1/R`. At the old
// 10 MiB a 30 Mbit/s H.264 remux held ~2.8 s, and the beta log (2026-10-18) showed it run dry in
// the last minute of an anime episode - the high-bitrate ending outran the debrid link, the lane
// fell from 10.5 MB to 0.3 MB and the sink dropped 180 frames. 48 MiB is ~13 s at 30 Mbit/s and
// ~27 s at 15 Mbit/s (a 4K HEVC web-dl); a 60-80 Mbit/s UHD remux still gets ~5-6 s.
// ponytail: fixed bytes, not seconds; size it from the probed bitrate if RAM gets tight.
const AQ_VIDEO_BYTES: c_long = 48 * 1024 * 1024;
const AQ_AUDIO_BYTES: c_long = 1024 * 1024;

pub struct SampleBuf {
    pub data: Vec<u8>,
    pub au: Vec<usize>, // AU start offsets
    pub next: usize,
    pub loops: i64,
}

pub enum Source {
    Stream, // host/port/path are consumed by the demux thread at spawn
    Sample(Box<SampleBuf>),
}

/// owns a popped au_node; frees on drop (paired with the malloc in aq_push).
pub struct AuBox(pub *mut AuNode);
impl Drop for AuBox {
    fn drop(&mut self) {
        unsafe { libc::free(self.0 as *mut c_void) }
    }
}

/// MAIN-THREAD-CONFINED. No worker thread ever names an Engine field.
pub struct Engine {

    /// Firmware callback context for this exact native `Load`.
    pub native_epoch: u32,
    pub stage: Stage,
    pub video_info_sent: bool, // videoInfoSent
    /// ACB route: `SHARED.frames` when the plane bind completed (`Stage::Streaming`), until a tick
    /// beyond it is seen. A paused preroll re-pauses only after that tick: a picture presented
    /// before the bind is not on the panel.
    pub bound_at_frames: Option<i32>,
    /// webOS 5+ only: the source size the exported window was last placed with, so a corrective
    /// re-place happens exactly once if the demuxer publishes the real one later. 0 = never placed.
    pub placed_src: (c_int, c_int),
    pub eos_pushed: bool, // Kodi VIDEO_DRAIN: pushEOS() sent once at true EOF
    /// Once the demux has ended: the playhead last seen and when it was first seen there (pump
    /// `now`, ms). The pump ends a playback whose playhead stands still for `EOS_STALL_MS`.
    pub eos_stall: Option<(i64, u32)>,
    pub rebase_pending: bool, // g_rebase_pending
    // In-place seek only: keyframes this far AHEAD of the seek target are stale frames the demuxer
    // produced from its pre-flush read position before the reopen+av_seek won the race (playback
    // drifts forward during a long scrub) - reject them so the rebase anchors on the REAL post-seek
    // keyframe, not the drifted one. `rebase_drops` caps the rejections so a sparse-keyframe file or
    // a genuinely-failed av_seek can't hang the rebase.
    pub rebase_drops: i32,
    pub seek_armed_at: u32, // SDL-ticks when the current in-place seek was armed (stuck watchdog)
    pub seek_retries: i32,  // cheap reopen-retries attempted before escalating to a full reload
    pub flushed: bool,      // Kodi m_flushed: set on an in-place seek flush; the first
    // post-flush keyframe triggers setTimeToDecode + sendSegmentEvent (the fresh GStreamer
    // segment a bare flush() omits), then clears this.
    /// The post-seek keyframe has installed its segment but has not yet been accepted by
    /// Starfish. Native presentation callbacks remain fenced until that exact AU returns `O`.
    pub presentation_rearm_pending: bool,
    pub max_fed_video_pts: i64, // high-water fed pts, VIDEO lane (g_max_fed_pts)
    pub max_fed_audio_pts: i64, // high-water fed pts, AUDIO lane (two-lane feed)
    /// Fed pts of the AU that opened this timeline's audio - the one the demuxer marked as its
    /// first since the open or seek (`SHARED.first_audio_pts`) - once Starfish accepted it.
    /// `None` until then; reset with `max_fed_audio_pts`. See [`audio_lead_in`].
    pub audio_start_fp: Option<i64>,
    /// The feed-ahead throttle's reference position, run on between the pipeline's 5 Hz
    /// position reports (see [`FeedPace`]).
    pub feed_pace: FeedPace,
    /// One video frame at the declared rate, in ns; 0 when the declaration names no rate. Sets the
    /// video lane's floor in frames ([`video_span_ns`]).
    pub video_frame_ns: i64,
    pub seek_base_pts: i64,     // fed pts of the first post-seek keyframe (prime measures buffer
    // depth as max_fed_video_pts - seek_base_pts, since the in-place seek feeds REAL pts, not 0-based)
    // prime-then-play: after a seek/resume the pipeline is PAUSED and data is buffered before
    // Play, so the clock doesn't free-run through the demux reopen gap (that
    // gap is what makes video "fast-forward" to catch the audio clock on resume). feed_stream
    // fires Play once max_fed_pts reaches PRIME_NS.
    pub prime_play: bool,
    // Two-lane feed (Kodi m_messageQueueVideo/Audio): the demuxer routes es=1 video to aq_video
    // and es=2 audio to aq_audio; each lane is fed independently so a video BufferFull can't stall
    // the audio lane (the audioSync master clock). Both are allocated for a stream. (None only
    // pre-start and on the local-sample source.)
    pub aq_video: Option<Box<AuQueue>>, // g_aq (M owns; ptr handed to D)
    pub aq_audio: Option<Box<AuQueue>>, // audio lane
    // hs/payload are RAII: held alive for the workers (which hold raw ptrs into
    // them) and freed only after join - never read back through the field.
    #[allow(dead_code)]
    pub hs: Box<HttpStream>, // demux socket (M owns; D uses via raw ptr)
    pub pending_video: Option<AuBox>, // bf_pending, VIDEO lane (held across BufferFull)
    pub pending_audio: Option<AuBox>, // bf_pending, AUDIO lane
    #[allow(dead_code)]
    pub payload: std::ffi::CString, // bf_payload (kept alive for the session)
    pub source: Source,
    pub stream_th: Option<std::thread::JoinHandle<()>>,
    pub load_th: Option<std::thread::JoinHandle<()>>,
    pub report_th: Option<std::thread::JoinHandle<()>>, // observer progress reporter
    pub report_stop: Option<std::sync::Arc<threads::ReportStop>>, // ITS stop signal, not SHARED's
    /// Preview Load that must land unbound. The media join waits until the thread has returned.
    pub preview_abandon: bool,
}

impl Engine {
    pub fn uses_stream_queues(&self) -> bool {
        matches!(self.source, Source::Stream)
    }
}

// The `ENGINE` slot is `App.adapters.player` since phase 9 - a field, reached as
// `pa: &mut PlayerAdapter` from the loop. `player::adapter` carries the confinement argument the
// four `static mut` accessors used to carry here, and says what the owned slot proves that the
// `&MainThread` argument could only assert.

/// Owns the explicit `Held(Initial)` clock state until the matching Engine is installed. Every
/// early return after worker creation runs this guard only after its local handles have been
/// stopped/joined, so a failed start cannot poison the next attempt with a phantom held clock.
struct ClockStartGuard {
    armed: bool,
}

impl ClockStartGuard {
    fn arm() -> Option<Self> {
        SHARED
            .arm_initial_clock_hold(TX.paused.load(Ordering::Acquire), TX.seek_preroll_active())
            .then_some(Self { armed: true })
    }

    fn commit(mut self) {
        self.armed = false;
    }
}

impl Drop for ClockStartGuard {
    fn drop(&mut self) {
        if self.armed {
            SHARED.cancel_clock_session_start();
        }
    }
}

/// Own the native callback generation until the matching Engine has been installed.  Every early
/// return then retires it automatically, and retirement is also a barrier for a callback already
/// executing on Starfish's library thread.
struct NativeSessionStartGuard {
    epoch: u32,
    armed: bool,
}

impl NativeSessionStartGuard {
    fn arm() -> Option<Self> {
        SHARED
            .begin_native_session()
            .map(|epoch| Self { epoch, armed: true })
    }

    fn epoch(&self) -> u32 {
        self.epoch
    }

    fn commit(mut self) {
        self.armed = false;
    }
}

impl Drop for NativeSessionStartGuard {
    fn drop(&mut self) {
        if self.armed {
            SHARED.retire_native_session(self.epoch);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeObjectDisposition {
    Destroy,
    Quarantine,
}

/// D1 frees private state which a late firmware hook dereferences before it reaches our Rust
/// callback. Every independent proof is therefore mandatory; this deliberately has no
/// best-effort or timeout branch.
fn native_object_disposition(
    unload_completed: bool,
    callback_gate_proven: bool,
    rust_epoch_retired: bool,
) -> NativeObjectDisposition {
    if unload_completed && callback_gate_proven && rust_epoch_retired {
        NativeObjectDisposition::Destroy
    } else {
        NativeObjectDisposition::Quarantine
    }
}

/// A native `Load` that teardown could not wait for.
///
/// The D.1.4 budget turns a `Load` that never returns into a failure read-out, but the object that
/// `Load` is running on cannot be touched until the call comes back - `Unload`/`Destroy` on it
/// while the load thread is still inside is the issue #74 race - and joining the media thread on
/// the SDL main thread froze the whole app on exactly the set where the Load was hung. So teardown
/// parks the thread, the payload it reads and the epoch here instead, and
/// [`reap_abandoned_load`] runs the ordinary native release (`Unload` → callback gate → epoch
/// retirement → D1 or quarantine) on the main thread once the thread has finished. Until then every
/// start is refused rather than handed the same object; if the `Load` never returns the object is
/// leaked for the life of the process, which is what the C seam would have to do anyway.
pub struct AbandonedLoad {
    epoch: u32,
    load_th: std::thread::JoinHandle<()>,
    /// The Load payload `sf_load` was handed as a raw pointer. Must outlive the thread.
    _payload: std::ffi::CString,
    abandoned_at: std::time::Instant,
}

/// Release a parked [`AbandonedLoad`] if its `Load` has returned. Cheap when there is nothing to
/// do; called once per app frame and at the top of every native start. `true` when the parked
/// object was released by this call.
pub fn reap_abandoned_load(pa: &mut super::adapter::PlayerAdapter) -> bool {
    let finished = match pa.abandoned_load_mut() {
        Some(parked) => parked.load_th.is_finished(),
        None => return false,
    };
    if !finished {
        return false;
    }
    let parked = pa.take_abandoned_load().expect("checked above");
    // The thread has already returned, so this join cannot block.
    tv_base::task::join("media", parked.load_th);
    log(&format!(
        "native lifecycle: abandoned Load epoch={} returned {}ms after teardown; releasing it",
        parked.epoch,
        parked.abandoned_at.elapsed().as_millis(),
    ));
    release_native_object(pa.mt(), parked.epoch);
    // `parked._payload` drops here, after the thread that read it has been joined.
    true
}

/// **A real stop's native object, handed to an off-frame `Unload`.** `StarfishMediaAPIs::Unload`
/// blocks for 110-150 ms on the dev set (webOS 4.5, release build, measured 2026-10-15 with
/// `teardown spans:`; the kernel sched trace of the same returns shows the main thread ~31-36 ms in
/// D state inside a driver `ioctl` and then ~72-74 ms in a futex until the pipeline thread woke it),
/// and on the main thread that was the whole of the frame that handled BACK: 140-170 ms on 8 of 8
/// returns. So a real stop issues it on a worker and keeps everything else where it was: the
/// callback gate, the epoch retirement, the ACB `setState(UNLOADED)`, D1 and the exported window
/// still run on the main thread, in the same order, once `Unload` has returned
/// ([`settle_release`], polled once a frame by `app::run` and forced by the adapter before anyone
/// takes the token again). A reload, an app-switch suspend and the decoder-revoked park keep the
/// synchronous release: each starts the next Load at once, so there is no frame to save.
///
/// While the worker owns the object its epoch is [`Shared::begin_native_release`]'s `Releasing`
/// phase, which admits only UNLOADCOMPLETED (the main thread has already reset the state any other
/// callback would write into), and no new native session may begin.
pub struct PendingRelease {
    epoch: u32,
    unload_th: std::thread::JoinHandle<()>,
    handed_off_at: std::time::Instant,
    /// The Engine the object belonged to. It used to drop at the end of `teardown`, after the
    /// release; it still drops after it (the Load payload in particular outlives the Unload).
    _engine: Engine,
}

/// Test seam: make [`release_off_frame`] see the OS refuse the `sf-unload` thread.
#[cfg(test)]
static FAIL_UNLOAD_SPAWN_FOR_TEST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Start the off-frame release of `epoch`'s object, or release it here when it cannot be handed
/// off (no dispatchable object, an epoch that no longer owns `Active`, or the OS refusing the
/// thread). `Some` = the worker is running and the caller must park it with the Engine.
fn release_off_frame(mt: &MainThread, epoch: u32, sp: &mut TeardownSpans) -> Option<std::thread::JoinHandle<()>> {
    if unsafe { sink().ready(mt) } == 0 || !SHARED.begin_native_release(epoch) {
        release_native_object_timed(mt, epoch, sp);
        return None;
    }
    let ticket = UnloadTicket::issue(mt);
    #[cfg(test)]
    let refused = FAIL_UNLOAD_SPAWN_FOR_TEST.load(Ordering::Relaxed);
    #[cfg(not(test))]
    let refused = false;
    let th = if refused {
        None
    } else {
        tv_base::task::spawn("sf-unload", move || unsafe { sink().unload(&ticket) })
    };
    sp.mark("unload_handoff");
    if th.is_none() {
        log("native lifecycle: no thread for the off-frame Unload; unloading on the main thread");
        unsafe { sink().unload(&UnloadTicket::issue(mt)) };
        sp.mark("sf_unload");
        finish_unloaded_object(mt, epoch, sp);
        destroy_native_window(mt, sp);
    }
    th
}

/// Finish a parked [`PendingRelease`]: join its `Unload` (only once it has returned, unless
/// `wait`) and run the main-thread tail. `true` when this call finished it. Reached through
/// `PlayerAdapter::{poll_release, finish_release}`.
pub(crate) fn settle_release(slot: &mut Option<PendingRelease>, mt: &MainThread, wait: bool) -> bool {
    let finished = match slot.as_ref() {
        Some(p) => p.unload_th.is_finished(),
        None => return false,
    };
    if !finished && !wait {
        return false;
    }
    let p = slot.take().expect("checked above");
    let mut sp = TeardownSpans::new();
    // A thread that has finished joins at once; one that has not is a caller about to use the seam
    // within ~150 ms of the stop, and `task::join` names the wait if it stalls.
    tv_base::task::join("sf-unload", p.unload_th);
    sp.mark(if finished { "join" } else { "wait_unload" });
    finish_unloaded_object(mt, p.epoch, &mut sp);
    destroy_native_window(mt, &mut sp);
    sp.emit("release");
    log(&format!(
        "native lifecycle: off-frame Unload epoch={} released {} ms after the stop{}",
        p.epoch,
        p.handed_off_at.elapsed().as_millis(),
        if finished { "" } else { " (a caller waited for it)" },
    ));
    // `p._engine` drops here, after the object it belonged to is gone.
    true
}

/// Unload and retire the native object of `epoch`, then destroy it only with complete evidence.
/// The engine's own teardown and the abandoned-Load release share this so the order below is one
/// definition.
fn release_native_object(mt: &MainThread, epoch: u32) {
    release_native_object_timed(mt, epoch, &mut TeardownSpans::new());
}

/// **Where a teardown's frame goes, one step at a time.** Each step's wall time is appended as
/// `label:ms` and the whole run is logged as ONE `teardown spans:` line at the end, so a slow BACK
/// frame names the call that held the main thread instead of being inferred from a sched trace.
/// Measuring costs two `Instant::now()` per step; the line is written once per teardown.
pub(crate) struct TeardownSpans {
    t0: std::time::Instant,
    last: std::time::Instant,
    out: String,
}

impl TeardownSpans {
    pub(crate) fn new() -> Self {
        let now = std::time::Instant::now();
        Self { t0: now, last: now, out: String::new() }
    }

    /// Close the step that ran since the previous mark under `label`.
    pub(crate) fn mark(&mut self, label: &str) {
        use std::fmt::Write as _;
        let now = std::time::Instant::now();
        let ms = now.duration_since(self.last).as_secs_f64() * 1000.0;
        self.last = now;
        let _ = write!(self.out, "{}{label}:{ms:.1}", if self.out.is_empty() { "" } else { "," });
    }

    pub(crate) fn emit(&self, what: &str) {
        log(&format!(
            "teardown spans: {what} total={:.1} {}",
            self.t0.elapsed().as_secs_f64() * 1000.0,
            self.out,
        ));
    }
}

fn release_native_object_timed(mt: &MainThread, epoch: u32, sp: &mut TeardownSpans) {
    // Stop the pipeline, close native callback admission, then drain the Rust epoch.
    //
    // `Unload` waits for the GStreamer state transition, but neither libpf's raw callback fields nor
    // type=23 is an exported producer/in-flight barrier. The executable therefore interposes the
    // exact callbackFunctionHook JUMP_SLOT. Each Load owns a never-reused object address; after
    // Unload the C gate rejects new calls and waits for calls already inside the real hook. Only
    // then can the Rust callback mutex be retired and D1 considered:
    //
    //     Unload -> gate inactive/drained -> retire/drain epoch -> D1.
    //
    // Both the synchronous type=23 observation and a non-zero per-object interposer counter are
    // runtime evidence that this firmware followed the audited path. If any proof is missing, C
    // retains the constructed object forever and permanently refuses another Load. Leaking one
    // object is preferable to letting D1 turn a late producer callback into a use-after-free.
    if unsafe { sink().ready(mt) } != 0 {
        sp.mark("pre_unload");
        unsafe { sink().unload(&UnloadTicket::issue(mt)) };
        sp.mark("sf_unload");
        finish_unloaded_object(mt, epoch, sp);
    } else if !SHARED.retire_native_session(epoch) {
        log(&format!(
            "native lifecycle: no dispatchable object and epoch={epoch} was already retired",
        ));
    }
    destroy_native_window(mt, sp);
}

/// Everything after `Unload` returns, in the audited order: close and drain the C callback gate,
/// retire the Rust epoch, unload the ACB plane, then D1 with complete evidence or quarantine. The
/// synchronous release and the off-frame release's main-thread tail ([`settle_release`]) share
/// it, so the order is one definition.
fn finish_unloaded_object(mt: &MainThread, epoch: u32, sp: &mut TeardownSpans) {
    {
        window_step(WindowStage::NativeUnload);
        let callback_gate_proven = unsafe { sink().callback_gate_retire(mt) } != 0;
        sp.mark("gate_retire");
        let callback_intercepts = unsafe { sink().callback_intercepts(mt) };
        let unload_completed = SHARED.native_unload_completed(epoch);
        let rust_epoch_retired = SHARED.retire_native_session(epoch);
        if !unload_completed {
            log(&format!(
                "native lifecycle: Unload returned without type=23 epoch={epoch}",
            ));
        }
        if !callback_gate_proven {
            log(&format!(
                "native lifecycle: callback interposition unproven epoch={epoch} intercepted={callback_intercepts}",
            ));
        }
        if !rust_epoch_retired {
            log(&format!(
                "native lifecycle: failed to retire epoch={epoch} after Unload",
            ));
        }
        sp.mark("retire");
        if ACB_OK.load(Ordering::Relaxed) {
            unsafe { sink().plane_unload(mt) };
            sp.mark("acb_unload");
            window_step(WindowStage::AcbPlaneUnload);
        }
        match native_object_disposition(unload_completed, callback_gate_proven, rust_epoch_retired)
        {
            NativeObjectDisposition::Destroy => {
                let destroyed = unsafe { sink().destroy(mt) } != 0;
                sp.mark("sf_destroy");
                if !destroyed {
                    log("native lifecycle: C seam rejected D1 and quarantined the object");
                    window_step(WindowStage::NativeQuarantined);
                } else {
                    window_step(WindowStage::NativeDestroyed);
                }
            }
            NativeObjectDisposition::Quarantine => {
                unsafe { sink().quarantine(mt) };
                sp.mark("quarantine");
                window_step(WindowStage::NativeQuarantined);
            }
        }
    }
}

fn destroy_native_window(mt: &MainThread, sp: &mut TeardownSpans) {
    // The webOS 5+ counterpart of acb_unload, and the other half of `with_window_id`'s create.
    // Outside the sf_ready guard because readiness means dispatchable, not "object exists", and
    // because the window is created BEFORE Load - a session that failed between the two would
    // otherwise leak it, and the next Load would ask for another. Unguarded because the seam
    // already no-ops in the other modes (see starfish.h).
    unsafe { sink().destroy_window(mt) };
    sp.mark("destroy_window");
    if sink().window_mode() == super::VP_EXPORTED {
        window_step(WindowStage::ExportedWindowDestroy);
    }
}

/// One native-object / exported-window teardown step that has RETURNED.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowStage {
    AcbPlaneUnload,
    NativeUnload,
    NativeDestroyed,
    NativeQuarantined,
    ExportedWindowCreate,
    ExportedWindowDestroy,
}

impl WindowStage {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            WindowStage::AcbPlaneUnload => "acb_plane_unload",
            WindowStage::NativeUnload => "native_unload",
            WindowStage::NativeDestroyed => "native_destroyed",
            WindowStage::NativeQuarantined => "native_quarantined",
            WindowStage::ExportedWindowCreate => "exported_window_create",
            WindowStage::ExportedWindowDestroy => "exported_window_destroy",
        }
    }
}

/// One event-log breadcrumb per teardown/exported-window step that returned, so a native crash in
/// the next step shows which step had already completed. Carries nothing beyond the label.
fn window_step(stage: WindowStage) {
    log(&format!("window: {} returned", stage.label()));
    #[cfg(any(test, feature = "test-support"))]
    WINDOW_STEPS.lock().unwrap_or_else(|e| e.into_inner()).push(stage.label());
}

/// Every [`window_step`] label since the last [`take_window_steps_for_test`] (test probe).
#[cfg(any(test, feature = "test-support"))]
static WINDOW_STEPS: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());

/// Drain the recorded teardown/exported-window steps, oldest first.
#[cfg(any(test, feature = "test-support"))]
pub fn take_window_steps_for_test() -> Vec<&'static str> {
    std::mem::take(&mut *WINDOW_STEPS.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Bind the decoded video sink to the display plane, whichever way this television does it.
///
/// **Boot-scoped.** Called once, from the app's run loop, and never again - which is why the webOS 5
/// exported window is NOT created here: that one is created per session in [`start_bufferfeed`],
/// beside the payload it has to be spliced into, and destroyed by the matching `teardown`.
///
///   - `VP_ACB` - create + initialize the ACB. We deliberately DON'T register our own
///     com.webos.media client; it collides with the pipeline's uMS connection. The handle lives
///     for the process; what teardown calls is `acb_unload`, a session-scoped state change.
///   - `VP_EXPORTED` - nothing to do at boot. There is no handle, and no state mirroring:
///     webOS 5 deleted that sequence outright rather than replacing it.
pub fn acb_init(mt: &MainThread) {
    match sink().window_mode() {
        super::VP_EXPORTED => {
            log("vplane: exported window (webOS 5+) — created per session");
            // Nothing to bind later; the pump's ACB stages are skipped by ACB_OK staying false.
            ACB_OK.store(false, Ordering::Relaxed);
        }
        super::VP_NONE => {
            log(
                "vplane: this device has NEITHER libAcbAPI nor SDL's exported window — audio \
                 will play and the picture will not appear. Please report your webOS version.",
            );
            ACB_OK.store(false, Ordering::Relaxed);
        }
        _ => acb_init_acb(mt),
    }
}

/// The webOS 4.x half of [`acb_init`].
fn acb_init_acb(mt: &MainThread) {
    if let Some(s) = tv_base::devtrig::read("ptype") {
        if let Ok(p) = s.parse::<c_int>() {
            PTYPE.store(p, Ordering::Relaxed);
        }
    }
    let pt = PTYPE.load(Ordering::Relaxed);
    log(&format!("ptype={pt}"));
    // Which app ACB is being initialized for. The third argument of `AcbAPI_initialize` is an app
    // id - that much is certain, and both reference implementations pass one (Kodi's webOS port
    // passes `getenv("APPID")`; see docs/distribution.md). What ACB does with it has not been
    // decompiled here, so the rule is simply that it must be the install SAM actually launched -
    // and with a developer build able to sit beside the shipped one, "the id compiled in" and
    // "the id we were launched as" are no longer the same question.
    //
    // It used to be `env::var("APPID")` with a NULL fallback, which `starfish.c` then turned into
    // the shipped app's literal id. That is a double fallback whose failure is invisible: on any
    // launch where SAM did not export APPID, a developer install would announce itself as the
    // shipped app. `paths::app_id` reads the install directory instead, which is the id webOS
    // registered by definition. The environment is still logged, one line at boot, as the
    // independent witness for whether SAM sets it at all on this firmware.
    let app_c = std::ffi::CString::new(tv_base::paths::app_id()).ok();
    let app_ptr = app_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
    let acb = unsafe { sink().plane_create(mt, app_ptr, pt) };
    ACB_OK.store(acb != 0, Ordering::Relaxed);
    log(&format!("acb create={acb}"));
}

/// split Annex-B into AUs on the 5-byte AUD prefix 00 00 00 01 <aud5>
/// (H264 AUD = 0x09; HEVC AUD is NAL type 35 → first header byte 0x46).
fn bf_split(data: &[u8], aud5: u8) -> Vec<usize> {
    let mut au = Vec::new();
    let mut i = 0usize;
    while i + 4 < data.len() {
        if data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
            && data[i + 4] == aud5
        {
            au.push(i);
            i += 4;
        }
        i += 1;
    }
    au
}

/// Build the streamed BUFFERSTREAM Load payload from PAYLOAD_AV, substituting the item's real
/// video/audio codecs + a sink envelope. video = "H264"|"H265", audio = "AC3"|"AC3 PLUS"|"AAC"|"PCM".
/// What the Load's `adaptiveStreaming` block declares: the sink the pipeline ALLOCATES for, on the
/// firmwares that allocate against the declaration rather than the bitstream (webOS 10.3.1,
/// measured - `docs/webos10-resource-allocation.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SinkEnvelope {
    pub w: u32,
    pub h: u32,
    pub fps: u32,
}

/// The declaration every set has ever been verified against: 4K60. Both codecs on the dev set
/// (webOS 4.10), HEVC on 10.3.1 (13 Loads, DV P8 + Atmos among them).
const ENVELOPE_UHD60: SinkEnvelope = SinkEnvelope {
    w: 3840,
    h: 2160,
    fps: 60,
};
/// The one SMALLER declaration with a device measurement behind it: the H.264 patch that loaded in
/// 117 ms on 10.3.1 where 4K60 was refused twice.
const ENVELOPE_FHD60: SinkEnvelope = SinkEnvelope {
    w: 1920,
    h: 1080,
    fps: 60,
};

/// The video codec one Load declares: what the payload's `codec.video` names, and what the
/// `dg_load_v` diagnostic code says. Written once here so the three never disagree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LoadVideo {
    H264,
    H265,
    Av1,
    Vp9,
    /// VP8: the other codec libpf's buffer feed builds working caps for (`CustomPipeline::
    /// getVideoCaps`, `findVideoCodecType`'s `VP8`), a 1080p30 decoder on the sets that list it.
    /// Its `VC1` caps carry no `wmvversion`, and a VC-1 feed stops at the first buffer (`Playing
    /// error`, measured on an LG C1 from Matroska, ASF and Blu-ray MPEG-TS), so VC-1 is not here.
    Vp8,
}

impl LoadVideo {
    /// The declared stream vcodec -> what the Load declares, case-insensitively. **An unknown codec
    /// is `None`, not H.264**: a catch-all would declare an H.264 Load over a bitstream the
    /// pipeline cannot parse, i.e. a black screen with nothing in the log naming the cause. The
    /// empty string is the one deliberate H.264: a declaration that names no codec (a dev-trigger
    /// URL, the local-sample feeds) is the pipeline tier's baseline.
    pub(crate) fn of_stream(vcodec: &str) -> Option<Self> {
        match vcodec.to_ascii_lowercase().as_str() {
            "" | "h264" => Some(Self::H264),
            "hevc" | "h265" => Some(Self::H265),
            "av1" => Some(Self::Av1),
            "vp9" => Some(Self::Vp9),
            "vp8" => Some(Self::Vp8),
            _ => None,
        }
    }
    /// The payload's `codec.video` string. libpf selects its AV1 caps for `"AV1"`; the pipeline
    /// has no av1parse, so an AV1 Load carries its configuration in-band (see `ff.rs`). `"VP9"`
    /// is the name in libpf's codec table beside `"VP8"`; its `video/x-vp9` appsrc caps carry no
    /// `codec_data`, and a VP9 frame carries its own headers, so nothing is added to the packets.
    pub(crate) fn payload_name(self) -> &'static str {
        match self {
            Self::H264 => "H264",
            Self::H265 => "H265",
            Self::Av1 => "AV1",
            Self::Vp9 => "VP9",
            Self::Vp8 => "VP8",
        }
    }
    /// `SHARED.dg_load_v`'s code (video 1 = H264, 2 = H265, 3 = AV1, 4 = VP9, 5 = VP8).
    pub(crate) fn diag_code(self) -> u8 {
        match self {
            Self::H264 => 1,
            Self::H265 => 2,
            Self::Av1 => 3,
            Self::Vp9 => 4,
            Self::Vp8 => 5,
        }
    }
}

/// True when the Load must be refused: an AV1, VP9 or VP8 stream on a set whose table lists no
/// such decoder. Only reachable when a server ignores the profile we sent, so the refusal is a refusal
/// to declare a codec the set may not have, never a fallback to H.264 for another bitstream.
pub(crate) fn load_video_refused(lv: LoadVideo, caps: &tv_platform::devcaps::Caps) -> bool {
    match lv {
        LoadVideo::Av1 => !caps.av1,
        LoadVideo::Vp9 => !caps.vp9,
        LoadVideo::Vp8 => !caps.vp8,
        LoadVideo::H264 | LoadVideo::H265 => false,
    }
}

/// **The sink envelope for this session, and the rule is deliberately narrow.** Until 2026-09-03
/// the streamed A/V Load declared `ENVELOPE_UHD60` for every codec and every source, and on
/// webOS 10.3.1 that refuses EVERY H.264 Load with `type=18 num=601 Resource Allocation Error`
/// before a frame is decoded. The lab session that found it
/// also measured the fix for the common shape and nothing beyond it, so:
///
/// * **HEVC keeps 4K60.** Dolby Vision P5/P8 and HDR10 are device-verified only under that
///   declaration, and nothing smaller has ever been declared for HEVC on any set.
/// * **H.264 declares FHD60 when the declared raster fits FHD**, which is what the lab measured
///   (a 1080p H.264 stream), and stays 4K60 otherwise - a real 4K H.264 file keeps the known
///   refusal on 10.3.1. "Fits FHD" is width ≤ 1920 and height ≤ 1088: a 1080p H.264 stream's
///   CODED height is 1088 (16-aligned), and a probe reports coded sizes, so 1088 has to read as
///   1080 or every 1080p file would be promoted to the 4K declaration it was meant to escape.
/// * **Unknown raster (0 on an axis) means 4K60**, today's value: "nobody said" must not
///   under-declare a file that might be 4K.
/// * **The device's own table clamps, per codec, only when it was actually READ.** On the dev set
///   and on 10.3.1 both rows claim ≥4K@60 and nothing changes; a set whose HEVC row says
///   1920x1088 (a Full HD panel - issue #63's hypothesis) gets FHD60 for HEVC too. The assumed
///   fallback is never a clamp. A clamped 1088 is normalised to 1080 for the same reason as
///   above, and `fps` is never raised above 60 by the table (120 is a claim nobody has tested).
///
/// `maxFrameRate`: **H.264 declares the stream's own rate class** (24/25/30/50/60, never below
/// the stream beyond one 1 ms container-timebase tick per frame, so 30.303 declares 30, issue
/// #665; 60 when the rate is unknown) - see the measurement inside; HEVC stays at 60, the
/// value every Dolby Vision verification was taken under. `/tmp/webosiptv-sinkmax=WxH@F`
/// overrides the whole result so the remaining legs are an A/B with no rebuild. Pure; the
/// wrappers feed it the route.
///
/// AV1 and VP9 copy HEVC's rule because only H.264 was measured to need the FHD declaration on
/// webOS 10; both are unmeasured on that axis (issue #593).
fn sink_envelope(
    video: LoadVideo,
    max_raster: (u16, u16),
    stream_fps: f64,
    caps: &tv_platform::devcaps::Caps,
    measured: bool,
) -> SinkEnvelope {
    let fits_fhd = |w: u32, h: u32| w > 0 && h > 0 && w <= 1920 && h <= 1088;
    let (mw, mh) = (u32::from(max_raster.0), u32::from(max_raster.1));
    let mut env = if video == LoadVideo::H264 && fits_fhd(mw, mh) {
        ENVELOPE_FHD60
    } else {
        ENVELOPE_UHD60
    };
    // **H.264 declares the stream's own rate class, and this is device-measured, not guessed.**
    // On the dev set (webOS 4.10) a 4K H.264 24p direct play declared at 60 makes the pipeline
    // announce `frameRate:24` and then, about a second later, `frameRate:30` for the same stream
    // - a 24p picture on a 30 fps lattice, i.e. pulldown judder the viewer can see. Declared at
    // 24 or at 30 it announces 24 once and holds it (2026-09-03, `webosiptv-sinkmax` A/B, three
    // legs; 4K HEVC under 60 announces 24 and holds it, and 1080p H.264 under 60 does too, so
    // the rate is left at 60 for HEVC and for an unknown stream rate). **And the sink's own
    // displayed-frame counter (callback type 47, see the payload note above) put a number on
    // what the eye saw: declared at 60 the 4K 24p stream was PRESENTED at 13.0 fps mean, worst
    // second 10; declared at 24, 24.1 fps.** The class, not the exact value: 23.976 → 24 keeps
    // the declaration a ceiling the stream fits under. The one deliberate exception is a
    // millisecond-timebase 30.303 → 30 (`fps_class`, issue #665), where the declaration sits
    // about 1% BELOW the fed rate. Suspected, NOT yet device-verified: the k6lp SoC's H.264
    // decoder is 4K@30-limited, so declaring 50 on a 30p 4K stream is refused with num=601;
    // `pipe_h264_uhd_30p_ms_timebase` and an owner retest on the 60UN7310 are the proof, with a
    // second `/tmp/webosiptv-nofps` leg to tell a rate-pair refusal from the 4K@30 limit.
    if video == LoadVideo::H264 && stream_fps > 0.0 && stream_fps.is_finite() {
        env.fps = fps_class(stream_fps);
    }
    if measured {
        let (rw, rh, rf) = match video {
            LoadVideo::H264 => caps.h264_row,
            LoadVideo::H265 => caps.hevc_row,
            LoadVideo::Av1 => caps.av1_row,
            LoadVideo::Vp9 => caps.vp9_row,
            LoadVideo::Vp8 => caps.vp8_row,
        };
        let clamp = |v: u32, bound: u32| if bound == 0 { v } else { v.min(bound) };
        env.w = clamp(env.w, rw);
        env.h = clamp(env.h, rh);
        env.fps = clamp(env.fps, rf);
        if env.h == 1088 {
            env.h = 1080;
        }
    }
    env
}

/// The sink envelope for THIS Load, from the route (main thread), with the dev override applied.
/// **What the two STATIC payloads declare**, as the numbers written into their strings - used on
/// every arm that sends one (the local-sample feeds and the streamed `/tmp/webosiptv-noaudio`
/// path), so the `load:` line's `max=` reports what was SENT rather than the streamed builder's
/// initializer. The test below pins these to the strings themselves.
fn static_envelope(video: LoadVideo) -> SinkEnvelope {
    match video {
        LoadVideo::H264 | LoadVideo::Vp8 => SinkEnvelope {
            w: 1920,
            h: 1080,
            fps: 30,
        },
        LoadVideo::H265 | LoadVideo::Av1 | LoadVideo::Vp9 => ENVELOPE_UHD60,
    }
}

/// A static template with its codec renamed: the no-audio Load for an AV1 or VP9 stream (from
/// `PAYLOAD_H265`, its 4K60 envelope) or a VP8 one (from `PAYLOAD_V`, 1080p30, the most that
/// decoder takes), keeping the template's envelope and transport.
/// The Load payload for a stream with no audio to declare: the static template for its codec,
/// plus the Dolby node for HEVC - a video-only Dolby Vision file (a test pattern) is otherwise
/// engaged as HDR10.
fn video_only_payload(video: LoadVideo, dv: crate::stream::DvPresentation) -> String {
    match video {
        LoadVideo::H264 => PAYLOAD_V.to_string(),
        LoadVideo::H265 => with_dolby_hdr_info(PAYLOAD_H265, "H265", dv),
        LoadVideo::Av1 | LoadVideo::Vp9 | LoadVideo::Vp8 => renamed_static_payload(video),
    }
}

fn renamed_static_payload(video: LoadVideo) -> String {
    let to = format!(r#""video":"{}""#, video.payload_name());
    match video {
        LoadVideo::Vp8 => PAYLOAD_V.replacen(r#""video":"H264""#, &to, 1),
        _ => PAYLOAD_H265.replacen(r#""video":"H265""#, &to, 1),
    }
}

/// The broadcast/film rate class (24, 25, 30, 50, 60) a stream's reported rate belongs to, else
/// the rate rounded up.
///
/// A rate belongs to the smallest class `c` for which either `fps <= c + 0.01` (the exact and
/// NTSC rates: 23.976, 29.97, 59.94) or its mean frame duration is within **1 ms** of the class's
/// (`1000/fps > 1000/c - 1`). The 1 ms is the Matroska timebase: timestamps are integers in
/// `TimestampScale` = 1 ms, so a frame interval estimated from them is off by less than one tick,
/// and a 30p file reports `avg_frame_rate` 1000/33 (30.303) (issue #665). The error is constant in TIME, not in rate, so the tolerance grows with the class: the
/// windows top out at 24.59, 25.64, 30.93, 52.63 and 63.83 and never overlap the next class.
///
/// This is NOT a claim that the stream is exactly the class: a constant-33 ms file really is fed
/// at 30.303 fps, so the declaration can sit about 1% below the fed rate. libpf has never been
/// measured accepting `maxFrameRate` below `videoFps`; the esInfo rational deliberately stays the
/// raw rate (it is a timing input for libpf's PTS lattice, not a ceiling), and only the ceiling
/// is snapped. Only the H.264 path reaches this; HEVC keeps 60.
fn fps_class(fps: f64) -> u32 {
    for class in [24u32, 25, 30, 50, 60] {
        let c = f64::from(class);
        if fps <= c + 0.01 || 1000.0 / fps > 1000.0 / c - 1.0 {
            return class;
        }
    }
    fps.ceil() as u32
}

fn sink_envelope_now(decl: &crate::stream::StreamDeclaration, video: LoadVideo) -> SinkEnvelope {
    if let Some(spec) = tv_base::devtrig::read("sinkmax") {
        if let Some(env) = parse_sinkmax(&spec) {
            #[cfg(feature = "devtriggers")]
            log(&format!(
                "sinkmax: envelope OVERRIDDEN to {}x{}@{} by /tmp/webosiptv-sinkmax",
                env.w, env.h, env.fps
            ));
            return env;
        }
        log(&format!("sinkmax: unparseable spec {spec:?} — ignored"));
    }
    sink_envelope(video, decl.raster, decl.fps, tv_platform::devcaps::caps(), tv_platform::devcaps::measured())
}

/// `WxH@F` - three positive integers; anything else is `None`.
fn parse_sinkmax(spec: &str) -> Option<SinkEnvelope> {
    let (wh, fps) = spec.trim().split_once('@')?;
    let (w, h) = wh.split_once('x')?;
    let env = SinkEnvelope {
        w: w.trim().parse().ok()?,
        h: h.trim().parse().ok()?,
        fps: fps.trim().parse().ok()?,
    };
    (env.w > 0 && env.h > 0 && env.fps > 0).then_some(env)
}


/// The `contents.pcmInfo` node for a `"PCM"` feed (`AudioFeed::SoftPcm`): S16LE 48 kHz in the
/// channel shape the decoder produces. PlxNative's shape (webosbrew ss4s), beside `"format"`.
fn with_pcm_info(payload: &str, feed: &crate::stream::AudioFeed) -> String {
    match feed {
        crate::stream::AudioFeed::SoftPcm { channels, .. } => payload.replacen(
            r#""format":"RAW""#,
            &format!(r#""format":"RAW","pcmInfo":{}"#, crate::ff::adec::pcm_info_json(*channels, crate::ff::adec::PCM_RATE)),
            1,
        ),
        _ => payload.to_string(),
    }
}

/// The pipeline reads the true dimensions from the SPS (Phase 0 HEVC probe), so mw/mh are only
/// the sink envelope.
fn build_av_payload(
    decl: &crate::stream::StreamDeclaration,
    dv: crate::stream::DvPresentation,
    video: &str,
    audio: &str,
    env: SinkEnvelope,
) -> String {
    let mut p = PAYLOAD_AV
        .replace(r#""video":"H264""#, &format!(r#""video":"{video}""#))
        .replace(r#""audio":"AC3""#, &format!(r#""audio":"{audio}""#))
        .replace(r#""maxWidth":1920"#, &format!(r#""maxWidth":{}"#, env.w))
        .replace(r#""maxHeight":1080"#, &format!(r#""maxHeight":{}"#, env.h))
        .replace(
            r#""maxFrameRate":30"#,
            &format!(r#""maxFrameRate":{}"#, env.fps),
        );
    // Real source frame rate (0 when the declaration has none → skip): give the pipeline the true
    // fps for A/V timing instead of the sink-envelope default, + adaptiveResolution so it adapts if
    // the coded dims change. libpf parses videoFpsValue/videoFpsScale/adaptiveResolution (verified).
    // `/tmp/webosiptv-nofps` withholds the pair, for one experiment: the Dolby Vision display
    // -management lookup misses because the LUT ring is keyed ONE 90 kHz tick above what the
    // display firmware asks for (measured 2026-08-21 - 38 of 40 misses, written key == requested
    // + 1, with LG's own level-2 KADP logging armed mid-playback). Neither derivation is ours: the
    // fed PTS provably does not move the outcome (nudge A/B, alternating unseeked legs, 163/164/165
    // misses regardless), and the pipeline timestamps by NEAREST-rounding on the 1001/24000 lattice
    // rather than passing ours through. This rational is the one input we hand it that could be
    // what it builds that lattice FROM, so it is the one remaining lever on our side.
    if tv_base::devtrig::flag("nofps") {
        #[cfg(feature = "devtriggers")]
        log("esInfo: videoFps WITHHELD by /tmp/webosiptv-nofps");
    } else if let Some((num, den)) = fps_rational(decl.fps) {
        p = p
            .replace(
                r#""seperatedPTS":true}"#,
                &format!(r#""seperatedPTS":true,"videoFpsValue":{num},"videoFpsScale":{den}}}"#),
            )
            .replace(
                r#""audioOnly":false"#,
                r#""audioOnly":false,"adaptiveResolution":true"#,
            );
        log(&format!("esInfo: videoFps {num}/{den} + adaptiveResolution (src {:.3})", decl.fps));
    }
    let p = with_dolby_hdr_info(&p, video, dv);
    let p = with_aac_info(&p, audio, decl.audio_channels, decl.audio_sample_rate);
    with_immersive(&p, audio, decl.atmos)
}

/// The `contents.aacInfo` node for an AAC feed: channels, profile 2, ADTS framing (the feed always
/// is: raw AAC is reframed, ADTS passes through) and the frequency in kHz - the node Kodi and ss4s
/// send and libpf parses (`docs`: PlxNative's Kodi-parity notes, verified in `libpf-1.0.so`).
/// Without it the TV reported a 5.1 AAC channel as `0.000000 0 ... aac 0` and its audio stuttered.
/// Spliced only when both numbers are known.
fn with_aac_info(p: &str, audio: &str, channels: i64, sample_rate: i64) -> String {
    if audio != "AAC" || channels <= 0 || sample_rate <= 0 {
        return p.to_string();
    }
    let anchor = r#""provider":"webosiptv""#;
    if !p.contains(anchor) {
        return p.to_string();
    }
    let khz = sample_rate as f64 / 1000.0;
    log(&format!("aac: contents.aacInfo channels={channels} frequency={khz}"));
    p.replace(anchor, &format!(r#"{anchor},"aacInfo":{{"channels":{channels},"profile":2,"format":"adts","frequency":{khz}}}"#))
}

/// The `contents.immersive` node - **the Dolby Atmos half of the same envelope**, and the reason
/// the television's Atmos read-out never appeared while its Dolby Vision one did.
///
/// PURE, so the splice is host-testable, and spliced at the same `provider` anchor and in the same
/// shape as [`with_dolby_hdr_info`], which is its sibling in every sense: one key inside
/// `externalStreamingInfo.contents`, one string, and the whole difference between a stream the
/// pipeline treats as ordinary E-AC3 and one it treats as immersive.
///
/// **The key and the value are both read off the television's own binaries** (2026-08-21), which
/// matters because neither is guessable and a wrong one is silent:
/// - `libpf-1.0.so.1.0.0` holds the literal key path `option.externalStreamingInfo.contents.immersive`,
///   logs it as `PF_EXT_IMMERSIVE : %s`, and carries it onto the audio caps it builds -
///   `audio mediaInfo ... channels[%d] language[%s] ... immersive[%s] role[%s]`. It is a `%s` all the
///   way down: libpf validates nothing and passes the string through.
/// - The VALUE therefore has to come from whoever fills it, and **the bare literal `ATMOS` exists in
///   exactly one library on this device: `libcbe.so`** - Chromium's media backend, i.e. the path
///   LG's own web apps take. In its string pool that literal sits immediately
///   after `immersive` and in the same run as `externalStreamingInfo`, `esInfo`, `seperatedPTS`,
///   `provider`, `DolbyHdrInfo`, `encryptionType`, `profileId` and `contents` - the payload we
///   build, key for key. So `"ATMOS"` is not our invention; it is the value the working client
///   sends, recovered from the binary that sends it.
///
/// `libplayerAPIs` injects `platformSupportDolbyATMOS` itself from its configd cache, exactly as it
/// does for Dolby Vision, so this node states a fact about the STREAM and never about the set.
fn with_immersive(p: &str, audio: &str, atmos: bool) -> String {
    if !atmos {
        return p.to_string();
    }
    if audio != "AC3 PLUS" {
        // Same consistency guard as DolbyHdrInfo's H265 check. The only Atmos elementary-stream
        // path this player feeds is E-AC3 JOC, named "AC3 PLUS" in LG's Load vocabulary; any other
        // codec cannot carry the source track's immersive declaration.
        log(&format!(
            "atmos: immersive NOT sent — payload audio codec is {audio}, not AC3 PLUS"
        ));
        return p.to_string();
    }
    let anchor = r#""provider":"webosiptv""#;
    if !p.contains(anchor) {
        log("atmos: payload has no provider anchor — immersive NOT spliced");
        return p.to_string();
    }
    log("atmos: sourceInfo contents.immersive=ATMOS");
    p.replace(anchor, &format!(r#"{anchor},"immersive":"ATMOS""#))
}

/// The `contents.DolbyHdrInfo` node, spliced into the Load payload only for a session whose frozen
/// decision declared Dolby Vision after affirmative platform capability.
///
/// PURE, so the splice is host-testable; the decision is the session's frozen
/// [`crate::stream::Dovi::presentation`].
///
/// **Why this one node is the fix, from the television's own binaries** (decompiled 2026-08-21,
/// webOS 4.10.2 `libpf`): `CustomPipeline::parseOptionStringSpi` builds the literal key
/// `option.externalStreamingInfo.contents.DolbyHdrInfo`, asks `Options::checkKeyExistance` for it,
/// and on a hit sets `hasDolbyHdrInfo` - unconditionally, before a single sub-field is read and
/// with no platform gate. `getVideoCaps` then ends by adding `dolby-vision=TRUE` (plus
/// `dolby-vision-profile` when `profileId != -1`) to the caps it was already building. Without the
/// node, appsrc gets plain `video/x-h265` and nothing downstream can engage Dolby Vision. The
/// pipeline has parsed this all along; we simply never sent it.
///
/// Three things that look like they should change and do not:
/// - **the codec string stays `"H265"`.** `getVideoCaps` maps H265 to `video/x-h265` and that
///   branch falls THROUGH into the Dolby Vision tail; there is no DVHE/DVH1 entry in its codec
///   table (those literals belong to AdaptivePipeline's RFC-6381 parser, a different pipeline).
///   LG's own Chromium client also reports `codec.video = "H265"` for a Dolby Vision stream. The
///   `video == "H265"` guard below is therefore a consistency check, not a translation.
/// - **`profileId` must be a JSON integer** (`getInt`). Quoting it would leave the `-1` sentinel.
/// - **the library's injected platform metadata is not a safety gate.** libplayerAPIs injects
///   `platformSupportDolbyVision` / `supportDolbyTVATMOS` at the root, but libpf enables DV from
///   our node's presence regardless. We do not duplicate those root fields; `webos::caps` reads
///   the same public configd key independently and route policy requires an exact `true` before
///   this function can receive `Declare`.
///
/// The anchor is `"provider":"webosiptv"` - the last key of `contents` and, by the test below,
/// present exactly once in `PAYLOAD_AV`. A `replace` that finds nothing is a silent no-node, which
/// is why the miss is logged rather than assumed away.
fn with_dolby_hdr_info(p: &str, video: &str, dv: crate::stream::DvPresentation) -> String {
    let Some(n) = dv.declared() else {
        return p.to_string();
    };
    if crate::stream::dv_node_suppressed() {
        #[cfg(feature = "devtriggers")]
        log(&format!(
            "dv: DolbyHdrInfo P{} SUPPRESSED by /tmp/webosiptv-dvnonode (direct play kept)",
            n.profile_id
        ));
        return p.to_string();
    }
    if video != "H265" {
        // Unreachable by construction - the session declares Dolby Vision for an HEVC stream
        // only - so this is the guard that says so out loud rather than declaring Dolby Vision
        // over an H264 elementary stream. A malformed sourceInfo does not fail loudly; it wedges
        // the sink.
        log(&format!(
            "dv: DolbyHdrInfo P{} NOT sent — payload video codec is {video}, not H265",
            n.profile_id
        ));
        return p.to_string();
    }
    let anchor = r#""provider":"webosiptv""#;
    if !p.contains(anchor) {
        log("dv: payload has no provider anchor — DolbyHdrInfo NOT spliced");
        return p.to_string();
    }
    let node = format!(
        r#","DolbyHdrInfo":{{"trackType":"{}","encryptionType":"{}","profileId":{}}}"#,
        n.track_type, n.encryption_type, n.profile_id
    );
    // ONE line, and it is the answer to "what did we actually send" - the only place the emitted
    // values exist as fact rather than as intent.
    log(&format!(
        "dv: sourceInfo contents.DolbyHdrInfo profileId={} trackType={} encryptionType={} (codec {video})",
        n.profile_id, n.track_type, n.encryption_type
    ));
    p.replace(anchor, &format!("{anchor}{node}"))
}

/// `"appId":"<this install's id>"` - the payload key, and the anchor `with_window_id` splices onto.
///
/// One expression, called from both places, so the two cannot disagree.
fn app_id_key() -> String {
    format!(r#""appId":"{}""#, tv_base::paths::app_id())
}

/// Substitute the `@APPID@` placeholder with the id of the install this process actually is.
///
/// Exactly once per payload, asserted at build time: a `replace` that matched twice would leave a
/// second `appId` key for `with_window_id` to splice a duplicate `windowId` onto, and one that
/// matched nothing would hand LG's JSON parser the placeholder as a literal app id. Both are
/// silent - the pipeline reports no error for either - which is why this is graded in the host
/// suite rather than left to be noticed on a television.
fn with_app_id(p: &str) -> String {
    p.replace("@APPID@", tv_base::paths::app_id())
}

/// Create the exported window and splice its id into the Load payload - the webOS 5+ binding, in
/// one place.
///
/// **Session-scoped, and that is the point.** The window is created HERE rather than at boot
/// because `teardown` destroys it, and the two ends have to sit at the same level: created once at
/// boot and destroyed per session, the second play would splice nothing and show no picture, in
/// silence. Creating it beside its only consumer also makes the "must exist before Load" ordering
/// locally visible instead of a promise made across two files.
///
/// On webOS 5 this one string IS the video-plane binding: the compositor assigned it to the window
/// we just created, and the pipeline imports that window by name and punches through to it.
/// Everything else about the BUFFERSTREAM payload is unchanged between the eras - which is what
/// both reference implementations show (ss4s compiles the same payload builder for webOS 3, 4 and
/// 5 and swaps only the resource file; Kodi adds this key and nothing else).
///
/// Inserted after `"appId":"..."` because that is a stable sibling inside `option` in every payload
/// variant here - via [`app_id_key`], so the anchor cannot drift from what `with_app_id` composed.
/// A no-op on every webOS 4.x set, where `vp_create_window` returns NULL.
fn with_window_id(mt: &MainThread, p: &str) -> String {
    if sink().window_mode() != super::VP_EXPORTED {
        return p.to_string();
    }
    let id = unsafe { sink().create_window(mt) };
    window_step(WindowStage::ExportedWindowCreate);
    if id.is_null() {
        log("windowId: no exported window — video will not bind");
        return p.to_string();
    }
    let id = unsafe { std::ffi::CStr::from_ptr(id) }.to_string_lossy();
    let anchor = app_id_key();
    if !p.contains(&anchor) {
        log("windowId: payload has no appId anchor — NOT spliced; video will not bind");
        return p.to_string();
    }
    log(&format!(
        "vplane: exported windowId={id} spliced into the Load payload"
    ));
    p.replace(&anchor, &format!(r#"{anchor},"windowId":"{id}""#))
}

/// A declared decimal fps → (value, scale) rational for the Load esInfo. Broadcast rates map to their
/// exact NTSC/film ratios; integer rates to n/1; anything else to milli-fps. None if fps is unknown.
fn fps_rational(fps: f64) -> Option<(i64, i64)> {
    if fps <= 0.0 {
        return None;
    }
    let near = |a: f64, tol: f64| (fps - a).abs() < tol;
    Some(if near(23.976, 0.01) {
        (24000, 1001)
    } else if near(29.97, 0.01) {
        (30000, 1001)
    } else if near(59.94, 0.02) {
        (60000, 1001)
    } else if near(47.952, 0.02) {
        (48000, 1001)
    } else if fps.fract().abs() < 0.001 {
        (fps.round() as i64, 1)
    } else {
        ((fps * 1000.0).round() as i64, 1000)
    })
}

/// What one native start did. A pre-existing Engine is a successful no-op: the start that built it
/// owns the native object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferfeedStartOutcome {
    AlreadyRunning,
    Launched,
    /// The start had no declaration: the stream is being probed (`player::probe`), and `pump`
    /// launches the native start when the probe resolves it - or refuses it.
    Probing,
    Failed,
}

impl BufferfeedStartOutcome {
    fn accepted(self) -> bool {
        !matches!(self, Self::Failed)
    }
}

pub fn start_bufferfeed(ps: &mut crate::session::PlaybackSession, pa: &mut super::adapter::PlayerAdapter) -> bool {
    let outcome = start_bufferfeed_tracked(ps, pa);
    ps.jail_load_blocked || outcome.accepted()
}

/// Start the native Engine for the session's installed playback.
pub fn start_bufferfeed_tracked(ps: &mut crate::session::PlaybackSession, pa: &mut super::adapter::PlayerAdapter) -> BufferfeedStartOutcome {
    if tv_platform::tv::sandbox::blocks_native_video() {
        ps.jail_load_blocked = true;
        log("start_bufferfeed: refusing native video; affected chassis cannot read /dev/rtkmem");
        return BufferfeedStartOutcome::Failed;
    }
    ps.jail_load_blocked = false;
    ps.native_start_refused = false;
    if pa.engine().is_some() {
        log("start_bufferfeed: already running (no-op)");
        return BufferfeedStartOutcome::AlreadyRunning;
    }
    // A start with no declaration cannot build a Load yet: probe the stream first. The probe's
    // answer comes back through `pump` (`player::finish_probe`), which calls this again.
    if ps.unresolved() {
        if pa.has_probe() {
            return BufferfeedStartOutcome::Probing;
        }
        let Some(start) = ps.start() else {
            return BufferfeedStartOutcome::Failed;
        };
        return match super::probe::PendingProbe::spawn(start.url.clone(), start.headers.clone()) {
            Ok(p) => {
                log("start_bufferfeed: no declaration — probing the stream before the Load");
                pa.start_probe(p);
                BufferfeedStartOutcome::Probing
            }
            Err(f) => {
                ps.refuse_probe(f);
                BufferfeedStartOutcome::Failed
            }
        };
    }
    // A fresh play (not a `reload_at`, which reaches `start_bufferfeed_for` directly) is a new
    // playback: it gets to try the native Seek again.
    super::set_native_seek_refused(false);
    let outcome = start_bufferfeed_for(ps, pa);
    // The refusal for an outstanding abandoned Load builds no Engine, so no pump will ever publish
    // why the player is empty. Record it where `player::state` derives the read-out from; the start
    // itself stays refused.
    if outcome == BufferfeedStartOutcome::Failed && pa.has_abandoned_load() {
        ps.native_start_refused = true;
    }
    outcome
}

/// The `stream:` diagnostic line for a freshly opened stream: the origin and how many segments
/// its path has, never a segment (`curlio::log_path`).
fn stream_open_log_line(origin_log_form: &str, path: &str) -> String {
    format!("stream: {origin_log_form} path={}", crate::curlio::log_path(path))
}

fn start_bufferfeed_for(ps: &mut crate::session::PlaybackSession, pa: &mut super::adapter::PlayerAdapter) -> BufferfeedStartOutcome {
    match start_bufferfeed_inner(ps, pa) {
        Ok(()) => {
            // Every start funnels through here (fresh and reload alike), so the refusal's read-out
            // ends at the first start that is NOT refused.
            ps.native_start_refused = false;
            BufferfeedStartOutcome::Launched
        }
        Err(StartFailed) => BufferfeedStartOutcome::Failed,
    }
}

/// A start that built no Engine; the reason is already in the log.
#[derive(Debug)]
struct StartFailed;

fn start_bufferfeed_inner(ps: &mut crate::session::PlaybackSession, pa: &mut super::adapter::PlayerAdapter) -> Result<(), StartFailed> {
    // The last real stop's object may still be inside its off-frame `Unload`: finish that release
    // first (waiting for it if it has not returned - only a start within ~150 ms of a stop does),
    // so the epoch is retired and the object destroyed before this start constructs the next one.
    pa.finish_release();
    // A timed-out Load from an earlier Engine may still own the one native object the C seam
    // holds. Release it now if it has returned; otherwise refuse before anything below (the
    // exported window in particular) can touch state that object is still using.
    reap_abandoned_load(pa);
    if pa.has_abandoned_load() {
        log("start_bufferfeed: an abandoned Load has not returned yet (refusing native start)");
        return Err(StartFailed);
    }
    // Per-SESSION, not per-boot: both the log's every-100th cadence and the diagnostics read-out
    // mean "this playback", and a count carried in from the last item answers neither question.
    VTOT.store(0, Ordering::Relaxed);
    ATOT.store(0, Ordering::Relaxed);
    VATT.store(0, Ordering::Relaxed);
    AATT.store(0, Ordering::Relaxed);
    // Resolve the playback, in precedence order: the session's installed start wins, then
    // /tmp/webosiptv-playurl (a URL + its declaration), then /tmp/webosiptv-url (a URL
    // alone), then a local sample. A debug build's boot (`app::boot`) also starts the playurl
    // trigger itself; read here too, a start with an empty session URL still finds it.
    if ps.url().is_empty() {
        match crate::player::playurl::playurl() {
            Some(Ok(p)) => match p.to_start() {
                // A refused declaration stays on the session as its `Error` read-out.
                Ok(start) => {
                    if ps.begin(start).is_err() {
                        return Err(StartFailed);
                    }
                }
                Err(e) => log(&format!("playurl: refused request headers ({e}) — ignored")),
            },
            // Log and fall through rather than refuse: the next candidate may well be armed.
            Some(Err(e)) => log(&format!("playurl: unreadable spec ({e}) — ignored")),
            None => {}
        }
    }
    if ps.url().is_empty() {
        if let Some(t) = tv_base::devtrig::read("url") {
            // The bare-URL trigger keeps its baseline declaration (H.264, video only) rather
            // than probing: it is the pipeline tier's "a URL and nothing else" case.
            // `webosiptv-playurl` declares the rest (or, with `"probe": true`, probes as a
            // real start does).
            let mut start = crate::session::PlaybackStart::new(t);
            start.stream = Some(crate::stream::StreamDeclaration::default());
            if !start.url.is_empty() && ps.begin(start).is_err() {
                return Err(StartFailed);
            }
        }
    }
    // A start the session refused (a codec this television cannot decode, a Dolby Vision layering
    // it cannot present, an audio track that needs a software decoder) never reaches the pipeline:
    // the refusal is the `Error` read-out, rather than a Load whose clock never starts.
    if let Some(r) = ps.refusal() {
        log(&format!("start_bufferfeed: start refused ({r:?})"));
        return Err(StartFailed);
    }
    let url = ps.url().to_string();
    let mut sample: Option<Box<SampleBuf>> = None;
    let mut is_h265 = false;
    if url.is_empty() {
        if let Some(data) = tv_base::devtrig::read_sample("sample.h264") {
            let au = bf_split(&data, 0x09);
            log(&format!("bf_split h264: {} AUs in {} bytes", au.len(), data.len()));
            if au.len() < 2 {
                return Err(StartFailed);
            }
            sample = Some(Box::new(SampleBuf { data, au, next: 0, loops: 0 }));
        } else if let Some(data) = tv_base::devtrig::read_sample("sample.h265") {
            // Feed a local HEVC Annex-B sample to test native HEVC decode.
            let au = bf_split(&data, 0x46);
            log(&format!("bf_split h265: {} AUs in {} bytes", au.len(), data.len()));
            if au.len() < 2 {
                return Err(StartFailed);
            }
            is_h265 = true;
            sample = Some(Box::new(SampleBuf { data, au, next: 0, loops: 0 }));
        } else {
            // Nothing to play: no installed start, no /tmp/webosiptv-url, no local sample.
            #[cfg(feature = "devtriggers")]
            log("start_bufferfeed: no URL — start a playback (or set /tmp/webosiptv-url)");
            return Err(StartFailed);
        }
    }
    let stream = sample.is_none();
    let decl = ps.stream();
    // For a stream, the Load codecs come from the declaration: video H264/H265/AV1/VP9/VP8, audio
    // from the session's frozen `AudioFeed` decision. (The local sample paths keep their fixed
    // payloads.) A track the session decided is `Silent` gets the video-only Load.
    // dev A/B: /tmp/webosiptv-noaudio feeds video only (needAudio:false + skip es=2).
    let audio_payload = ps.audio().payload();
    let no_audio = tv_base::devtrig::flag("noaudio") || audio_payload.is_none();
    crate::ff::set_feed_audio(!no_audio);
    // A track decoded on the CPU, fed as the PCM this Load declares.
    crate::ff::set_soft_pcm(match ps.audio() {
        crate::stream::AudioFeed::SoftPcm { channels, .. } => *channels,
        _ => 0,
    });
    let stream_payload;
    // Every arm below assigns this - the static payloads through `static_envelope`, the streamed
    // one through `sink_envelope_now` - so a new arm that forgets it fails to compile.
    let sink_env: SinkEnvelope;
    // The LG-side audio name the payload ended up carrying, for the `load:` line below. "-" is
    // the video-only case, where there is no audio ES to name.
    let mut audio_declared: &str = "-";
    let mut video_declared: &str = "-";
    let payload_str: &str = if stream {
        let Some(lv) = LoadVideo::of_stream(&decl.vcodec) else {
            log(&format!(
                "start_bufferfeed: the pipeline cannot feed video codec {:?}; refusing the Load instead of declaring it H.264",
                decl.vcodec
            ));
            SHARED.demux_failed.store(true, Ordering::Release);
            return Err(StartFailed);
        };
        if load_video_refused(lv, tv_platform::devcaps::caps()) {
            log(&format!(
                "start_bufferfeed: {} stream but the device table lists no such decoder; refusing the Load",
                lv.payload_name()
            ));
            SHARED.demux_failed.store(true, Ordering::Release);
            return Err(StartFailed);
        }
        video_declared = lv.payload_name();
        // Record what the payload ACTUALLY says, for the diagnostics read-out - including the
        // video-only case, where `dg_load_a == 0` is the whole explanation for silence.
        SHARED.dg_load_v.store(lv.diag_code(), Ordering::Relaxed);
        match audio_payload.filter(|_| !no_audio) {
            None => {
                SHARED.dg_load_a.store(0, Ordering::Relaxed);
                // A static payload declares its own envelope; say so on the `load:` line.
                sink_env = static_envelope(lv);
                stream_payload = video_only_payload(lv, ps.dv());
                stream_payload.as_str()
            }
            Some((ac, diagnostic)) => {
                SHARED.dg_load_a.store(diagnostic, Ordering::Relaxed);
                audio_declared = ac;
                // The sink envelope - `adaptiveStreaming`'s maxWidth/maxHeight/maxFrameRate. On
                // webOS 10.3.1 the pipeline allocates against the declaration, so `sink_envelope`
                // is deliberately no wider than what was measured.
                sink_env = sink_envelope_now(&decl, lv);
                stream_payload = with_pcm_info(&build_av_payload(&decl, ps.dv(), lv.payload_name(), ac, sink_env), ps.audio());
                &stream_payload
            }
        }
    } else if is_h265 {
        sink_env = static_envelope(LoadVideo::H265);
        PAYLOAD_H265
    } else {
        sink_env = static_envelope(LoadVideo::H264);
        PAYLOAD_V
    };
    // The windowId splice happens HERE, at the single point every payload variant passes through
    // - three of the four variants are static strings that never see the builder. The appId
    // substitution happens BEFORE the splice: `with_window_id`'s anchor is the composed
    // `"appId":"<id>"`.
    let payload_c = std::ffi::CString::new(with_window_id(pa.mt(), &with_app_id(payload_str))).unwrap();
    // Profile 7 as its base layer (the demuxer drops the RPU/EL units). Every Load says.
    SHARED.dv_strip_layers.store(stream && ps.dv().strips_layers(), Ordering::Relaxed);
    if stream {
        // What the payload ACTUALLY declared, once, in the event log. Carries no URL.
        let dv = decl.dovi;
        log(&format!(
            "load: v={} a={:?} fps={:.3} dv=present:{} P{}/{} el:{} mode:{} atmos:{} max={}x{}@{}",
            video_declared,
            audio_declared,
            decl.fps,
            dv.present as i32,
            dv.profile,
            dv.bl_compat,
            dv.el_present as i32,
            ps.dv().label(),
            (decl.atmos && audio_declared == "AC3 PLUS") as i32,
            sink_env.w,
            sink_env.h,
            sink_env.fps
        ));
    }

    // The native object starts stopped. Publish that physical fact before either worker can
    // observe a user command; the guard rolls it back on every construction failure.
    let clock_start = match ClockStartGuard::arm() {
        Some(guard) => guard,
        None => {
            log("start_bufferfeed: clock state is not idle (refusing overlapping start)");
            return Err(StartFailed);
        }
    };
    let native_start = match NativeSessionStartGuard::arm() {
        Some(guard) => guard,
        None => {
            log("start_bufferfeed: native session is not idle (refusing overlapping Load)");
            return Err(StartFailed);
        }
    };

    // fd = -1 (CLOSED) so a teardown before/without http_open doesn't close(0)
    let mut hs = tv_net::stream::http_stream_boxed();
    let mut aqv_box: Option<Box<AuQueue>> = None;
    let mut aqa_box: Option<Box<AuQueue>> = None;
    let mut stream_th = None;
    let source;
    let headers = ps.headers();

    if stream {
        if tv_net::net::origin::Origin::parse(&url).is_none() {
            log("stream: the URL is not an http(s) URL — refusing the Load");
            return Err(StartFailed);
        }
        let (origin, path) = tv_net::net::origin::split(&url);
        log(&stream_open_log_line(&origin.log_form(), path));
        // Two-lane feed: the demuxer routes es=1 video to aq_video and es=2 audio to
        // aq_audio, each with its own cap + feeder.
        let mut qv = crate::aq::aq_new(AQ_VIDEO_BYTES);
        let mut qa = crate::aq::aq_new(AQ_AUDIO_BYTES);
        let aqv_raw = &mut *qv as *mut AuQueue;
        let aqa_raw = &mut *qa as *mut AuQueue;
        let hs_raw = &mut *hs as *mut HttpStream;
        SHARED.hs_ptr.store(hs_raw, Ordering::Release);
        {
            let aqp = threads::SendPtr(aqv_raw);
            let aqap = threads::SendPtr(aqa_raw);
            let hsp = threads::SendPtr(hs_raw);
            // The URL, headers and audio codec are captured BY VALUE here on the main thread:
            // every writer of them is followed by `teardown(true) + start_bufferfeed()`
            // (reload_at / switch_audio_native), which respawns this thread with the new values.
            let acodec = decl.acodec.clone();
            let url = url.clone();
            let headers = headers.clone();
            stream_th = tv_base::task::spawn_off_frame_keeping("demux", move |off| {
                crate::ff::demux(off, url, headers, acodec, aqp, aqap, hsp)
            });
            if stream_th.is_none() {
                // Nothing will ever fill the AU queues, so there is no session to start. `hs` is
                // about to drop with this early return, so retract the pointer first - the pump
                // and teardown both read it straight off SHARED.
                SHARED.hs_ptr.store(std::ptr::null_mut(), Ordering::Release);
                return Err(StartFailed);
            }
        }
        aqv_box = Some(qv);
        aqa_box = Some(qa);
        source = Source::Stream;
    } else {
        source = Source::Sample(sample.unwrap());
    }

    // the media thread constructs + loads + runs the loop (owns the GMainContext)
    let payload_ptr = threads::SendPtr(payload_c.as_ptr() as *mut c_char);
    let native_epoch = native_start.epoch();
    let load_th = tv_base::task::spawn("media", move || threads::load_thread(payload_ptr, native_epoch));
    if load_th.is_none() {
        // Without the media thread nothing is ever Loaded and nothing drains the AU queues, so the
        // demuxer would park in aq_push forever holding raw pointers into locals this return is
        // about to drop. Stop it exactly the way teardown's stream branch does - abort both lanes,
        // shutdown to wake the recv, join, and only then the real close.
        for q in [aqv_box.as_mut(), aqa_box.as_mut()].into_iter().flatten() {
            crate::aq::aq_abort(&mut **q);
        }
        let p = SHARED.hs_ptr.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !p.is_null() {
            tv_net::stream::http_shutdown(p);
        }
        crate::curlio::abort_active(); // the https demuxer's equivalent - see teardown
        if let Some(t) = stream_th.take() {
            tv_base::task::join("demux", t);
        }
        if !p.is_null() {
            tv_net::stream::http_close(p); // sole owner now: the reader is joined
        }
        return Err(StartFailed);
    }
    // Progress reporter: hands the observer the play position every few seconds. Previews and
    // sessions without an observer report nothing.
    let report_stop = threads::ReportStop::new();
    let report_th = match ps.observer() {
        Some(observer) if stream => {
            let st = report_stop.clone();
            tv_base::task::spawn("progress", move || threads::progress_thread(observer, st))
        }
        _ => None,
    };

    let eng = Engine {
        native_epoch,
        stage: Stage::Loading,
        video_info_sent: false,
        bound_at_frames: None,
        placed_src: (0, 0),
        eos_pushed: false,
        eos_stall: None,
        // if a seek is armed for the FIRST open (resume, or reload_at), rebase the first
        // post-seek keyframe to fed-pts 0 so the pipeline sees a 0-based timeline identical
        // to fresh play (disp_base carries the content offset). Plain fresh play leaves this
        // false (first keyframe is already ~0).
        // ...and a fresh start whose timeline starts far from 0 (a live channel joined mid-stream,
        // `FRESH_REBASE`, set by the probe) is rebased the same way: the audio lane holds until
        // the first keyframe sets the shift both lanes then use.
        // ...and every HLS stream, so an HLS VOD (catch-up) position is in playlist time from its
        // first keyframe whatever timestamps its segments carry (`Shared::hls_vod_base_ns`).
        rebase_pending: SHARED.seek_to_ns.load(Ordering::Relaxed) >= 0
            || FRESH_REBASE.swap(false, Ordering::Relaxed)
            || crate::hls::is_playlist_url(ps.url())
            || crate::ff::hls_handoff::waiting(ps.url()),
        rebase_drops: 0,
        seek_armed_at: 0,
        seek_retries: 0,
        flushed: false,
        presentation_rearm_pending: false,
        max_fed_video_pts: 0,
        max_fed_audio_pts: 0,
        audio_start_fp: None,
        feed_pace: FeedPace::default(),
        video_frame_ns: frame_ns_of(decl.fps),
        seek_base_pts: 0,
        prime_play: false,
        aq_video: aqv_box,
        aq_audio: aqa_box,
        hs,
        pending_video: None,
        pending_audio: None,
        payload: payload_c,
        source,
        stream_th,
        load_th,
        report_th,
        report_stop: Some(report_stop),
        preview_abandon: false,
    };
    // Select this session's seek mode (`SeekMode`, mod.rs). `feed_stream` can downgrade it to
    // `Reload` mid-session (the pipeline was unreachable, or LG refused a `Seek`), and that is a
    // fact about the StarfishMediaAPIs object - constructed once per session - not about the TV,
    // so it is re-selected here for every session, after the already-running no-op above.
    let layout_known = inplace_layout_known(tv_platform::tv::device::info().major, sink().feed_abi_is_cow());
    let mode = session_seek_mode(layout_known, sink().window_mode(), dev_seek_mode());
    log(&format!("seek mode: {mode:?} (layout_known={layout_known})"));
    super::set_seek_mode(mode);
    pa.install(eng);
    native_start.commit();
    clock_start.commit();
    TX.started.store(true, Ordering::Relaxed);
    log(&format!("SMP: media thread spawned, stream={}", stream as i32));
    Ok(())
}

/// Arm the demuxer to open+seek to `target_ns` on the NEXT Load, displaying honest content
/// time. disp_base=0 and (via start_bufferfeed) rebase_pending=true, so feed_stream rebases
/// the landed keyframe K to fed-pts 0 and the presented position reads as num+K = content
/// time. Call BEFORE start_bufferfeed (resume) or via reload_at (mid-play seek).
pub fn arm_seek(target_ns: i64) {
    let t = target_ns.max(0);
    SHARED.seek_to_ns.store(t, Ordering::Release);
    SHARED.disp_base.store(0, Ordering::Relaxed);
    SHARED.playpos_ns.store(t, Ordering::Relaxed); // instant HUD feedback until frames land
}

/// Direct-play seek = tear down the pipeline and start a FRESH Load at `target_ns`. The no-arg
/// StarfishMediaAPIs::flush() leaves a STALE GStreamer segment (it is a degenerate
/// gst_element_seek to GST_CLOCK_TIME_NONE with no FLUSH_START/STOP and no fresh SEGMENT), so
/// post-seek buffers are scheduled against the pre-seek segment, the sink stops draining and the
/// upstream buffers fill → permanent BufferFull. A fresh Load re-establishes a correct segment by
/// construction. Heavier than a flush (a ~1 s re-preroll) but correct.
///
/// Synchronous result of replacing the current native Engine. `Started` means the new Engine owns
/// a Load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "a reload replaces the native Engine and its result must be handled"]
pub enum ReloadOutcome {
    Started,
    NoRoute,
    StartFailed,
}

/// Every target `reload_at` was asked for (test probe, #568): proves a refused native Seek reloads
/// exactly once, at the user's target.
#[cfg(any(test, feature = "test-support"))]
pub(super) static RELOAD_AT_LOG: std::sync::Mutex<Vec<i64>> = std::sync::Mutex::new(Vec::new());

/// Whether the paused-seek preroll was open as each `reload_at` was entered, before its teardown
/// (test probe): the reload keeps it, so what the fresh session's feed gate and prime see is
/// whatever the dispatcher left open here.
#[cfg(any(test, feature = "test-support"))]
pub(super) static RELOAD_PREROLL_LOG: std::sync::Mutex<Vec<bool>> = std::sync::Mutex::new(Vec::new());

#[cfg(any(test, feature = "test-support"))]
fn note_reload_preroll() {
    RELOAD_PREROLL_LOG.lock().unwrap_or_else(|e| e.into_inner()).push(TX.seek_preroll_active());
}

pub fn reload_at(ps: &mut crate::session::PlaybackSession, pa: &mut super::adapter::PlayerAdapter, target_ns: i64) -> ReloadOutcome {
    #[cfg(any(test, feature = "test-support"))]
    {
        RELOAD_AT_LOG.lock().unwrap_or_else(|e| e.into_inner()).push(target_ns);
        note_reload_preroll();
    }
    if ps.url().is_empty() {
        log("reload_at: no url (ignored)");
        return ReloadOutcome::NoRoute;
    }
    let live = !reopens_at_position(SHARED.duration_ns.load(Ordering::Relaxed), SHARED.hls_vod_base_ns.load(Ordering::Acquire));
    log(&format!("reload_at: fresh Load at {}", if live { "the live edge".to_owned() } else { format!("{}s", target_ns / 1_000_000_000) }));
    teardown(ps, pa, true); // reload mode: preserve the session (no observer stop, no clear)
    if live {
        FRESH_REBASE.store(true, Ordering::Relaxed);
    } else {
        arm_seek(target_ns);
    }
    if start_bufferfeed_for(ps, pa).accepted() {
        ReloadOutcome::Started
    } else {
        ReloadOutcome::StartFailed
    }
}

/// Whether a reload reopens the stream at its position: a file (it has a duration) or an HLS
/// stream read in playlist time (`hls_vod_base_ns >= 0`). A live stream has neither, and its
/// position is a timestamp of the old session, which an EVENT playlist would misread as playlist
/// time: it reopens at its live edge instead, rebased like a fresh start.
fn reopens_at_position(duration_ns: i64, hls_vod_base_ns: i64) -> bool {
    duration_ns > 0 || hls_vod_base_ns >= 0
}

#[cfg(test)]
#[test]
fn only_a_stream_with_positions_reopens_at_one() {
    assert!(reopens_at_position(7_200_000_000_000, -1), "a file");
    assert!(reopens_at_position(3_600_000_000_000, 0), "an HLS VOD");
    assert!(reopens_at_position(0, 42_000_000_000), "a growing EVENT archive read in playlist time");
    assert!(!reopens_at_position(0, -1), "live, an EVENT one opened at its edge included");
}

/// NATIVE audio-track switch: select the Nth audio stream from the same file and reload the
/// pipeline at the current position (the session's audio decision was already re-made for the
/// chosen track, so the fresh Load configures the right audio decoder). `desired_audio_idx`
/// persists across the reload, so the demuxer keeps feeding the chosen stream and the choice
/// survives later seeks.
pub fn switch_audio_native(ps: &mut crate::session::PlaybackSession, pa: &mut super::adapter::PlayerAdapter, audio_idx: i32, pos_ns: i64) -> ReloadOutcome {
    SHARED.desired_audio_idx.store(audio_idx, Ordering::Relaxed);
    log(&format!("switch_audio_native: audio_idx={audio_idx} at {}s", pos_ns / 1_000_000_000));
    reload_at(ps, pa, pos_ns) // fresh Load at the current position, new audio stream
}

/// Stop playback: unblock+join threads, reset all state so a fresh start_bufferfeed() can
/// restart, and release the pipeline and the video plane. With a ready object the native Unload is
/// handed to the `sf-unload` worker and the destroy / plane release finish later in
/// `settle_release` (`PendingRelease`), so the object may still exist when this returns; anything
/// that needs it gone goes through the adapter, which waits for it.
pub fn stop_bufferfeed(ps: &mut crate::session::PlaybackSession, pa: &mut super::adapter::PlayerAdapter) {
    teardown(ps, pa, false);
}

/// Suspend for an app-switch: tear the pipeline down (webOS reclaims the video plane while we're
/// backgrounded) but PRESERVE the playback session - keep the URL and its decisions, and don't
/// tell the observer it stopped. The foreground restore is then a clean same-item reload
/// (`arm_seek` at the saved position + `start_bufferfeed`).
pub fn suspend_bufferfeed(ps: &mut crate::session::PlaybackSession, pa: &mut super::adapter::PlayerAdapter) {
    teardown(ps, pa, true);
}

/// Why a real stop ended, as the observer is told: the drained end of the stream, a failure the
/// pump published, or the viewer's own stop.
fn stop_reason_now() -> crate::session::StopReason {
    if SHARED.ended.load(Ordering::Relaxed) {
        crate::session::StopReason::Ended
    } else if SHARED.demux_failed.load(Ordering::Relaxed)
        || SHARED.demux_io_failed.load(Ordering::Relaxed)
        || SHARED.load_failed.load(Ordering::Relaxed)
    {
        crate::session::StopReason::Failed
    } else {
        crate::session::StopReason::Stopped
    }
}

/// The teardown body. `for_reload` = this is a seek reload, an audio switch or an app-switch
/// suspend, NOT a real stop: preserve the playback session so start_bufferfeed can restart the
/// SAME item, and tell the observer nothing.
fn teardown(ps: &mut crate::session::PlaybackSession, pa: &mut super::adapter::PlayerAdapter, for_reload: bool) {
    // A probe in flight belongs to the playback being stopped or suspended. A suspended,
    // still-unresolved start probes again when it is restarted.
    pa.cancel_probe();
    if !pa.is_live() {
        // No Engine (a refused start, or nothing ever started): a real stop still ends the
        // playback the session holds.
        if !for_reload {
            if let Some(o) = ps.observer() {
                o.stopped(threads::progress_now(), stop_reason_now());
            }
            ps.clear();
            SHARED.reset_session();
            TX.reset();
        }
        return;
    }
    // A preview Load that has not returned yet must not be joined on this frame. Bind is skipped
    // while the flag is set, so the picture never reaches the plane. The next halt joins once
    // the thread has already finished.
    if ps.is_preview() {
        if let Some(eng) = pa.engine() {
            let loading = eng.stage == Stage::Loading;
            let finished = eng.load_th.as_ref().is_none_or(|t| t.is_finished());
            if crate::player::preview::defer_media_join(loading, finished) {
                eng.preview_abandon = true;
                return;
            }
        }
    }
    let mut sp = TeardownSpans::new();
    let mut unload_th = None;
    SHARED.stop_clock();
    let mut eng = pa.take().expect("main-thread live check and take are one transaction");
    let stream = matches!(eng.source, Source::Stream { .. });

    // **The one place a playback really ENDS.** `for_reload` covers a seek reload, an audio
    // switch and an app-switch suspend - each of which destroys an ENGINE and keeps the
    // playback. Read before the reset below zeroes position and duration.
    let final_report = (!for_reload).then(|| (threads::progress_now(), stop_reason_now()));

    sp.mark("begin");
    // 1. unblock every thread (abort queues, close the demux socket)
    if let Some(st) = eng.report_stop.take() {
        st.stop(); // this reporter's own signal - unaffected by the reset_session below
    }
    if stream {
        // abort BOTH lanes: unblock the demux if it's parked in aq_push on a full lane
        for q in [eng.aq_video.as_mut(), eng.aq_audio.as_mut()].into_iter().flatten() {
            crate::aq::aq_abort(&mut **q);
        }
        let p = SHARED.hs_ptr.load(Ordering::Acquire);
        if !p.is_null() {
            // shutdown, not close: the demux thread is still inside recv here and is joined
            // below. Closing now would free the fd number for another thread to claim while
            // this one is still reading it. The real close happens after the join.
            tv_net::stream::http_shutdown(p);
        }
        // ...and the same interrupt for the OTHER transport. A curl demux is parked in
        // `curl_multi_wait`, where no `shutdown(2)` of ours can reach it - this writes a byte to
        // the wake pipe it is polling. A no-op when the live source is a plaintext socket, or when
        // there is none; exactly one of these two lines ever has anything to do.
        crate::curlio::abort_active();
    }
    sp.mark("unblock");
    // 2. JOIN every worker before freeing anything they hold raw ptrs into
    // Through `task::join` so a stall leaves a number behind: this is the main thread.
    if let Some(t) = eng.stream_th.take() {
        tv_base::task::join("demux", t);
    }
    sp.mark("join_demux");
    // A Load the budget already gave up on, still inside `sf_load`: do NOT join it here. This is
    // the main thread, and on the set where the budget fires the call may never return. The
    // native object is parked instead and released by `reap_abandoned_load` once the thread has
    // returned; nothing below may touch it before then (issue #74). Every other Load keeps the
    // ordinary ordering: one that has returned joins at once, and one that is merely slow is
    // still joined.
    let abandoned_load = match eng.load_th.take() {
        Some(t) if !t.is_finished() && SHARED.load_timed_out.load(Ordering::Acquire) => Some(t),
        Some(t) => {
            tv_base::task::join("media", t);
            None
        }
        None => None,
    };
    sp.mark("join_media");
    // 2b. every reader is now joined, so this thread is the sole owner: do the real close.
    // (Before the join it could only shutdown - see step 1.)
    if stream {
        let p = SHARED.hs_ptr.load(Ordering::Acquire);
        if !p.is_null() {
            tv_net::stream::http_close(p);
        }
    }
    // The progress reporter is NOT joined: it owns its `ReportStop` (already set) and samples
    // only SHARED clock state, so it exits after its current report and cannot be revived by the
    // next session. Joining it here would park the frame loop behind the observer's callback.
    if let Some(t) = eng.report_th.take() {
        drop(t);
    }
    sp.mark("http_close");
    // 3. Release the native object (see `release_native_object` for the proof chain), unless its
    // Load is still in flight - then hand it, its thread and its payload to the adapter.
    if let Some(load_th) = abandoned_load {
        if !SHARED.abandon_native_session(eng.native_epoch) {
            log(&format!(
                "native lifecycle: abandoned Load epoch={} no longer owned the Active phase",
                eng.native_epoch,
            ));
        }
        log(&format!(
            "native lifecycle: Load epoch={} still in flight at teardown — not joining; the \
             object stays parked (and native playback refused) until Load returns",
            eng.native_epoch,
        ));
        pa.park_abandoned_load(AbandonedLoad {
            epoch: eng.native_epoch,
            load_th,
            _payload: std::mem::take(&mut eng.payload),
            abandoned_at: std::time::Instant::now(),
        });
    } else if !for_reload {
        unload_th = release_off_frame(pa.mt(), eng.native_epoch, &mut sp);
    } else {
        release_native_object_timed(pa.mt(), eng.native_epoch, &mut sp);
    }
    // 4. drain + destroy both queues (drain_aq also clears both pendings). A real stop after
    // queues that held a lot (an archive read ahead to the cap) hands the freed heap back: the
    // next session, live, needs a fraction of it (`aq::release_freed_memory`). A reload refills
    // the same queues at once, so it keeps it.
    if stream {
        let peak: c_long = [eng.aq_video.as_mut(), eng.aq_audio.as_mut()]
            .into_iter()
            .flatten()
            .map(|q| crate::aq::aq_peak_bytes(&mut **q))
            .sum();
        drain_aq(&mut eng);
        for q in [eng.aq_video.as_mut(), eng.aq_audio.as_mut()].into_iter().flatten() {
            crate::aq::aq_destroy(&mut **q);
        }
        if !for_reload && peak >= crate::aq::RELEASE_AFTER_PEAK_BYTES {
            log(&format!("memory: the queues peaked at {} MB; returning the freed heap", peak >> 20));
            crate::aq::release_freed_memory();
        }
    }
    sp.mark("drain_aq");
    // 5. reset shared + transport. The DURATION is a fact about the FILE, and a reload plays the
    // same file: `reset_session_for_reload` keeps it so the playbar has a denominator while the
    // demuxer reopens (otherwise the playhead draws at position ÷ 0 for a few hundred ms). A real
    // stop zeroes it: the next item is a new file.
    if for_reload {
        SHARED.reset_session_for_reload();
        TX.reset_for_reload();
    } else {
        SHARED.reset_session();
        TX.reset();
    }
    if let Some((at, reason)) = final_report {
        if let Some(o) = ps.observer() {
            o.stopped(at, reason);
        }
        ps.clear();
    }
    sp.mark("reset");
    sp.emit(if for_reload { "reload" } else { "stop" });
    match unload_th {
        Some(unload_th) => {
            log(&format!(
                "stop_bufferfeed: torn down (native Unload epoch={} continues off-frame)",
                eng.native_epoch,
            ));
            pa.park_release(PendingRelease {
                epoch: eng.native_epoch,
                unload_th,
                handed_off_at: std::time::Instant::now(),
                _engine: eng,
            });
        }
        // Engine (hs/aq boxes, payload) drops here - after all joins
        None => log("stop_bufferfeed: torn down"),
    }
}

/// free every queued AU + the held pending one, BOTH lanes (seek + teardown).
pub fn drain_aq(eng: &mut Engine) {
    drain_one(eng.aq_video.as_mut());
    drain_one(eng.aq_audio.as_mut());
    eng.pending_video = None;
    eng.pending_audio = None;
}

fn drain_one(q: Option<&mut Box<AuQueue>>) {
    if let Some(q) = q {
        let qp = &mut **q as *mut AuQueue;
        let mut eof: c_int = 0;
        loop {
            let n = crate::aq::aq_pop(qp, &mut eof);
            if n.is_null() {
                break;
            }
            unsafe { libc::free(n as *mut c_void) };
        }
    }
}

/// feed streamed AUs from the demux queue; hold the current AU across ticks on
/// BufferFull (backpressure); zero-base the fed timeline on the first post-seek
/// keyframe; drop stale AUs past the B-frame reorder distance.
/// prime-then-play buffer depth: how much of the post-seek stream to buffer (paused) before
/// starting the clock. Enough to cover the pipeline's decode latency so the first frame is ready.
pub(crate) const PRIME_NS: i64 = 700_000_000;
// Prime the AUDIO lane too before starting the (audioSync master) clock - else a rapid-seek drain
// can start Play on an empty audio queue and leave audio silent until the next seek. Fallback:
// start anyway once video buffers PRIME_VIDEO_MAX_NS without audio (audioless / briefly starved),
// so a genuinely audioless region can't hang.
const PRIME_AUDIO_NS: i64 = 300_000_000;
const PRIME_VIDEO_MAX_NS: i64 = 2_500_000_000;
// Feed-ahead throttle (Kodi-parity): keep the VIDEO lane at most this far ahead of the presented
// position (SHARED.pres_fed) instead of feeding greedily to BufferFull. Bounding the buffer to
// ~1.6s (was ~10-20s: aq 6MB + the pipeline's own ~8MB) makes seeks flush far less, keeps the
// clock from running ahead, and cuts latency. AUDIO gets a looser bound so it can ride slightly
// ahead (audio buffer is cheap and it's the master clock) without unbounded race on odd muxes.
pub(crate) const MAX_FEED_AHEAD_NS: i64 = 1_600_000_000;
const AUDIO_SLACK_NS: i64 = 2_000_000_000;

// A fed pts this far below a lane's high-water is a stale pre-seek AU (past the B-frame reorder
// distance) → drop it rather than feed a backward jump.
const STALE_BACKJUMP_NS: i64 = 2_000_000_000;

/// One frame at `fps`, in ns; 0 for an unknown rate.
fn frame_ns_of(fps: f64) -> i64 {
    if fps > 0.0 && fps.is_finite() { (1e9 / fps) as i64 } else { 0 }
}

/// The most frames an H.264/HEVC decoder may hold before it outputs one (the largest DPB).
const DPB_FRAMES: i64 = 16;

/// A video-lane span in media time - the feed-ahead lead or the stale-backjump distance - never
/// shorter than [`DPB_FRAMES`] frames. The fixed spans are sized for film rates, where 16 frames
/// are under 0.7 s; at 1 fps (a debrid "still downloading" clip) the B-pyramid alone reaches 4 s
/// ahead in decode order, and a 1.6 s lead held the decoder at two frames, so it never output one.
fn video_span_ns(span_ns: i64, frame_ns: i64) -> i64 {
    span_ns.max(DPB_FRAMES.saturating_mul(frame_ns))
}
// In-place-seek rebase guard: a first-keyframe this far from the seek target is a stale pre-reopen
// frame from the drifted read position, not the real post-seek keyframe. av_seek(BACKWARD) lands
// AT/BEFORE the target, so a valid anchor is never ahead and is at most ~one GOP behind - hence a
// tight AHEAD bound and a looser BEHIND bound (to tolerate sparse keyframes). Capped by MAX_REBASE_DROPS.
const SEEK_STALE_AHEAD_NS: i64 = 6_000_000_000;
const SEEK_STALE_BEHIND_NS: i64 = 30_000_000_000;
const MAX_REBASE_DROPS: i32 = 240;
// Audio counterpart of the rebase guard: an audio AU this far AHEAD of the video high-water is a
// stale drifted frame (the demuxer's pre-reopen audio after a seek). Feeding it poisons
// max_fed_audio_pts (and then the backjump guard drops all LEGIT audio until the demux catches
// up to the poisoned mark → seconds of silence). Legit audio lead is bounded by the throttle to
// ~MAX_FEED_AHEAD+AUDIO_SLACK (3.6s), so 5s has margin. It was 15s, and a stuck-retry seek fed
// stale audio 14.9s ahead - 13s of user-audible silence after a rapid 10s-back tap burst.
const AUDIO_STALE_AHEAD_NS: i64 = 5_000_000_000;
/// How far past the start of its timeline the audio lane may feed whatever the clock says.
///
/// **A track whose audio starts late must still reach Starfish before the clock can move.** An
/// MKV audio `Delay` (an 11.008 s E-AC-3 track on a WEB-DL, 2026-10) puts the first audio AU
/// seconds past the first video one. Starfish's clock waits for audio, so `pres_fed` sits at 0
/// and the video lane stops [`MAX_FEED_AHEAD_NS`] in - and the audio AU, far past both, was
/// dropped as stale by [`AUDIO_STALE_AHEAD_NS`] and would have been held by the throttle anyway.
/// Nothing moved again: the picture buffered forever, while a resume past the gap played. The
/// AU the demuxer marked as its first since the open or seek is the real start, never stale
/// pre-seek audio (that was pushed before the mark), so it and the next 2 s are fed regardless.
/// 2 s is under the throttle's own audio lead ([`MAX_FEED_AHEAD_NS`] + [`AUDIO_SLACK_NS`]), so a
/// track that starts on time is fed exactly as before.
///
/// **The demuxer has to be able to READ that much audio**, which for a late track means with the
/// video lane already past its base cap and nobody draining it (no clock yet): `ff::demux` keeps
/// the video lane's pre-audio headroom until the audio lane has been pushed this much plus a
/// margin (`ff::LEAD_IN_MARGIN_NS`), and only then releases it as the consumer drains.
pub(crate) const AUDIO_LEAD_IN_NS: i64 = 2_000_000_000;
// The lead-in is only sound between these two: under the throttle's own audio lead (a track that
// starts on time is fed exactly as before), and over the audio the clock is primed with (else
// "ahead of the clock" would not be enough audio to start it).
const _: () = assert!(AUDIO_LEAD_IN_NS < MAX_FEED_AHEAD_NS + AUDIO_SLACK_NS);
const _: () = assert!(AUDIO_LEAD_IN_NS > PRIME_AUDIO_NS);
// Sentinel for SHARED.pres_fed meaning "no post-seek frame has presented yet" - the feed-ahead
// throttle treats it as feed-freely (don't compare the new fed pts against a stale pre-seek
// presented position). Set on a seek; the first presented frame overwrites it with a real pts.
pub const PRES_NONE: i64 = i64::MIN;

/// The longest the throttle's reference runs on past the last position report, in ms. The
/// pipeline reports every ~201 ms, so against this cap the run-on bridges a report up to ~49 ms
/// late (not a missed one); a paused or stalled clock stops changing `pres_fed`, and the
/// reference then stops this far ahead of it instead of opening the lane without bound.
const FEED_PACE_CAP_MS: u32 = 250;

/// **The feed-ahead throttle's reference: the presented position, run on at 1x between reports.**
///
/// `SHARED.pres_fed` moves in ~200 ms steps (the pipeline's 5 Hz position callback). Measured
/// against that step function, the throttle stayed shut for eleven frames and then opened by
/// 200 ms of media at once, so every fifth of a second one frame fed ~5 video and ~7 audio AUs to
/// Starfish, and `Feed()` blocks: on the TV with the track menu open, `FRAMEDROP ... results=14.6`
/// with `feed:14.4` inside it, 4-15 ms on every 12th frame. Run on from the last report at 1x,
/// the same budget opens a frame's worth of media per frame, so the same AUs reach
/// Starfish one or two per frame. The bound is the same lead,
/// `MAX_FEED_AHEAD_NS`, past the ESTIMATED playhead; the reserve can therefore exceed the raw
/// report's by at most [`FEED_PACE_CAP_MS`], and that cap stops the estimate when the reports
/// stop changing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeedPace {
    /// The last distinct `pres_fed` seen, and when it was first seen (player clock, ms).
    last: Option<(i64, u32)>,
}

impl FeedPace {
    /// The position to throttle against at `now_ms`, given the current raw `pres_fed`.
    /// [`PRES_NONE`] passes through untouched (feed freely) and forgets the last report.
    pub fn reference(&mut self, pres: i64, now_ms: u32) -> i64 {
        if pres == PRES_NONE {
            self.last = None;
            return pres;
        }
        let at = match self.last {
            Some((p, at)) if p == pres => at,
            _ => {
                self.last = Some((pres, now_ms));
                now_ms
            }
        };
        let run_on = now_ms.saturating_sub(at).min(FEED_PACE_CAP_MS) as i64;
        pres.saturating_add(run_on * 1_000_000)
    }
}

/// The most wall-clock time ONE lane may spend feeding in one tick, in µs, once it has fed an AU
/// **and its lead is healthy** (see [`lane_starved`]).
///
/// `Feed()` blocks on the frame thread, 0.3-0.7 ms per AU on the TV. In steady state a lane feeds
/// one or two AUs a frame and never comes near this. The throttle does not apply while priming,
/// though, and a post-prime lane is a full lead behind, so on 2026-10-01 the first frames of
/// every playback fed the whole demuxed backlog at once: `FRAMEDROP total=89.1 ... sfvx62:19.4,
/// sfax120:50.8` (62 video and 120 audio AUs in one frame), then `sfax53:38.3` the frame after,
/// with the player's HUD and track menu on screen. A seek re-primes the same way. Two lanes at
/// this slice cost at most ~6 ms of a 16.7 ms frame plus one AU's overrun each.
/// The slice is per LANE, so the video lane cannot spend the audio lane's share. The old greedy
/// video lane did exactly that: it fed 2.5 s of video before audio got its turn, and the prime
/// then fell through to the video-only escape (`primed: v=2541ms a=0ms`).
///
/// **The slice spreads a BACKLOG; it must never throttle a lane that is short of media.** A
/// lane below its low-water mark ([`lane_starved`]) is exempt: while priming, that is until the
/// lane holds its prime depth ([`PRIME_NS`] / [`PRIME_AUDIO_NS`], 17-42 AUs) - otherwise a slow
/// `Feed()` (3 ms or more, so one AU a lane a tick) stretches time-to-first-frame by that many
/// ticks - and once playing, until it is [`FEED_LOW_WATER_NS`] ahead of the playhead - otherwise
/// at the 50 Hz present rate 60 fps content drains its lead. The exemption is bounded by the
/// low-water mark itself, so the bulk of a backlog is still fed a slice a tick, and the prime
/// finishes on the tick its depth is reached, one tick later at most than a greedy feed would.
const FEED_LANE_SLICE_US: u64 = 3_000;

/// A playing lane whose lead over the presented position is below this is exempt from
/// [`FEED_LANE_SLICE_US`]. 250 ms is [`FEED_PACE_CAP_MS`]: the position reports arrive every
/// ~200 ms, so a lead under it can be gone before the next report says so, and it is ~6 AUs of
/// 24 fps video - a handful of `Feed()` calls, bounded cost, never a backlog.
const FEED_LOW_WATER_NS: i64 = 250_000_000;

/// Whether a lane is short of media, and so must be fed past its slice.
///
/// While priming (`priming`) the lane is short until it holds `prime_ns` past the seek base;
/// otherwise it is short when its lead over the presented position `pres` ([`PRES_NONE`]: nothing
/// presented, no lead to judge, not short) is under [`FEED_LOW_WATER_NS`].
fn lane_starved(priming: bool, max_fed: i64, seek_base: i64, prime_ns: i64, pres: i64) -> bool {
    if priming {
        max_fed - seek_base < prime_ns
    } else {
        pres != PRES_NONE && max_fed - pres < FEED_LOW_WATER_NS
    }
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// The test feed clock: each read advances it by `FEED_TEST_CLOCK_STEP_US`, which is 0 unless
    /// a test sets it. At 0, the host seam's near-free `sf_feed` never runs out the slice, so a
    /// slow or preempted test machine cannot change what one tick feeds.
    static FEED_TEST_CLOCK_US: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static FEED_TEST_CLOCK_STEP_US: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Monotonic µs for [`FeedSlice`]; only ever differenced.
fn feed_clock_us() -> u64 {
    #[cfg(any(test, feature = "test-support"))]
    {
        let step = FEED_TEST_CLOCK_STEP_US.with(|s| s.get());
        FEED_TEST_CLOCK_US.with(|c| {
            c.set(c.get() + step);
            c.get()
        })
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        use std::sync::OnceLock;
        use std::time::Instant;
        static T0: OnceLock<Instant> = OnceLock::new();
        T0.get_or_init(Instant::now).elapsed().as_micros() as u64
    }
}

/// One lane's share of a tick's feeding: [`FEED_LANE_SLICE_US`] from the lane's start.
struct FeedSlice {
    start: u64,
}

impl FeedSlice {
    fn begin() -> Self {
        Self { start: feed_clock_us() }
    }

    /// Whether the lane should stop before its next AU. A lane that has fed nothing yet always
    /// gets one AU, so the backlog drains on every tick, however slow the sink is; a `starved`
    /// lane ([`lane_starved`]) is never spent.
    fn spent(&self, fed: usize, starved: bool) -> bool {
        fed > 0 && !starved && feed_clock_us().saturating_sub(self.start) >= FEED_LANE_SLICE_US
    }
}

/// VIDEO lane feeder (aq_video is video-only). Owns the seek rebase + in-place-seek handshake + prime→Play, all of
/// which key off the first post-seek VIDEO keyframe. A BufferFull/over-budget breaks THIS lane
/// only - the audio lane (feed_audio_lane) keeps flowing so the audioSync master clock advances.
/// Nanoseconds added to every fed VIDEO PTS - **a one-tick rounding repair for LG's Dolby Vision
/// display-management lookup**, and inert everywhere else.
///
/// The fault is arithmetic and it is entirely on the television's side; this is the only lever we
/// have on it. `gstdualsequencer.c:606` (DWARF-confirmed, `libgstdualsequencer.so` 0x25b0-0x25e0)
/// keys the LUT entry it hands the display firmware with a DOUBLE truncation of the buffer's
/// nanosecond PTS:
///
/// ```text
/// ulTimeStamp = trunc(trunc(pts_ns) * 9 / 100000)      // ns -> 90 kHz ticks
/// ```
///
/// and `DOVI_SWSync_SetDoviLUTnMap` (`libkadaptor` 0xe30e8) then scans all 95 slots for **exact
/// 32-bit equality** - no tolerance, no nearest match. At 24000/1001 fps neither unit is exact:
/// a frame time is a whole number of nanoseconds only every **3rd** frame and a whole number of
/// 90 kHz ticks only every **4th**, so on `n ≡ 4, 8 (mod 12)` - and on no other frame - the two
/// truncations disagree by exactly one tick, the lookup misses, and the panel reuses the previous
/// frame's tone mapping. `lcm(3,4) = 12` frames is **0.5005 s**, which is the period of the
/// stutter as seen.
///
/// Measured on this set, uninstrumented, 85 s of Profile 5: 340 misses, and **340 of 340** equal
/// the double-truncated key while **0 of 340** equal the exact tick value - residues `{4: 170,
/// 8: 170}` and nothing else. It is a derivation that reproduces the data with no free parameter,
/// not a fit.
///
/// The repair is to hand the pipeline a PTS whose truncation lands in the right bin. The loss is
/// the fractional nanosecond FFmpeg's rescale already dropped, so it is strictly less than 1 ns,
/// and **one** nanosecond recovers it. The upper bound is `100000/9 ≈ 11111 ns` - beyond that an
/// already-correct frame would be pushed a tick the other way - so 1 sits at the safe end of a
/// wide range. As an A/V offset it is nothing: 1 ns against a 41.7 ms frame.
///
/// **Video only.** The key is computed from the video buffer's own PTS; the audio lane never
/// reaches this code and shifting it would be a skew for no reason.
///
/// **THE SIGN WAS BACKWARDS, AND −1 IS THE FIX.** Read the history below for what was tried; the
/// short version is that the model was right about the mechanism, wrong about which side rounds,
/// and the device settled it. Measured with LG's own level-2 KADP logging armed mid-playback
/// (`tools/logmprobe`): for **38 of 40** misses the key written into the LUT ring is exactly the
/// key the display firmware requested **plus one**. The ring is a tick HIGH, so the fed PTS goes
/// DOWN. Alternating unseeked legs, same title, same binary:
///
/// ```text
///   nudge = -1   misses 1        nudge = 0   misses 81
///   nudge = -1   misses 1        nudge = 0   misses 81
/// ```
///
/// Reproducible, 81:1, and the arithmetic that predicts −1 also predicted +1 would help; +1 was
/// measured at 163/165 against 164 for zero - i.e. inert. **Trust the measurement here, not the
/// derivation**: our fed PTS is not passed through, the pipeline re-timestamps by NEAREST-rounding
/// on the 1001/24000 lattice (measured: alternating −0.333/+0.333 ns against the exact rational),
/// so exactly how one nanosecond on our side moves a tick on theirs is not something this comment
/// can honestly claim to model. What it can claim is 81:1, twice, with the scene controlled by
/// alternation.
///
/// # The history, kept because three of its steps were wrong and each cost a run
///
/// **AND THE DEVICE REFUTED THE FIRST ATTEMPT, which is why the default was 0 for a while.** The one step
/// the disassembly could not settle was whether the OTHER side of that exact-equality comparison
/// moves with us. It does. Controlled A/B, same title, same seek to 900 s, same 45 s window, only
/// this value differing: **nudge 0 → 118 misses, nudge 1 → 230**. Worse, not fixed. A constant
/// offset cannot repair a *relative* truncation difference when both sides derive from the same
/// fed timestamp - which is now measured rather than assumed, and which also retires the tidiest
/// explanation this investigation has produced.
///
/// What survives the refutation is the arithmetic, and it is not small: 340 of 340 misses in the
/// unseeked run equal the double-truncated key and 0 of 340 equal the exact tick value, residues
/// `{4, 8}` mod 12 and nothing else. So the key IS computed that way and the comparison IS exact.
/// What is now open is why the slot the firmware asks for - using, we measured, dualsequencer's
/// own key - is not in the ring. That points at pairing or at slot lifetime, not at rounding.
///
/// `/tmp/webosiptv-ptsnudge=<ns>` is kept because it is the instrument that produced that result
/// and the next candidate value is one run away. Anything from 1 to ~11110 is in range; beyond
/// that an already-correct frame is pushed a tick the other way.
fn pts_nudge_ns() -> i64 {
    const DEFAULT: i64 = -1;
    static NUDGE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(i64::MIN);
    let v = NUDGE.load(Ordering::Relaxed);
    if v != i64::MIN {
        return v;
    }
    // Latched at the first feed rather than read per AU: this is the hottest path in the app and
    // the trigger surface is a filesystem open. Same shape as every other `devtrig::` read here.
    let v = tv_base::devtrig::read("ptsnudge")
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(DEFAULT);
    NUDGE.store(v, Ordering::Relaxed);
    if v != DEFAULT {
        log(&format!(
            "ptsnudge: video PTS nudge overridden to {v} ns (default {DEFAULT})"
        ));
    }
    v
}

/// Arm a prime at the current physical playhead without moving the clock. A viewer Resume from a
/// queued stream is this boundary: Starfish is already Paused, accepted media may remain ahead of
/// the presented tail, and both application queues may continue the same validated timeline
/// behind it.
pub fn arm_live_clock_prime(eng: &mut Engine) {
    let presented = SHARED.pres_fed.load(Ordering::Relaxed);
    eng.seek_base_pts = if presented == PRES_NONE {
        eng.max_fed_video_pts.min(eng.max_fed_audio_pts.max(0))
    } else {
        presented
    };
    eng.prime_play = true;
}

/// **Start the clock once both lanes are buffered past the seek base** - video to [`PRIME_NS`]
/// AND audio to [`PRIME_AUDIO_NS`], with a video-only escape at [`PRIME_VIDEO_MAX_NS`] so an
/// audioless or briefly-starved stream still starts.
///
/// **This is called from TWO places, and the second one is the whole point.** It used to live
/// inline in [`feed_stream`]'s per-AU loop and nowhere else, which made it unreachable in exactly
/// the case that matters. A pump tick runs the video lane and then the audio lane, so when a large
/// burst of AUs lands at once the video lane drains all of it while `abuf` is still zero - the
/// `abuf` arm cannot be true yet, and a 2-second burst never reaches the 2500 ms video-only
/// escape. The audio lane then fills, but evaluating nothing. On every later tick `aq_pop` returns
/// null and the loop breaks before reaching the check, so Play ends up waiting on a video AU that
/// only the next burst can supply. On 2026-08-29 that held a viewer's picture for 94 seconds
/// with both lanes holding a playable buffer the entire time.
///
/// Calling it again at the end of [`feed_audio_lane`] closes that hole at the only moment both
/// lanes' extents are known. It is idempotent - `prime_play` latches false on the first success -
/// so the in-loop call is kept for the case it already served: a direct-play stream that reaches
/// the threshold mid-lane should start on that AU, not a tick later.
pub fn try_prime(mt: &MainThread, eng: &mut Engine) {
    if !eng.prime_play {
        return;
    }
    let Some(prime_kind) = SHARED.prime_kind() else {
        return;
    };
    let accepted_vbuf = eng.max_fed_video_pts - eng.seek_base_pts;
    let accepted_abuf = eng.max_fed_audio_pts - eng.seek_base_pts;
    // During a user ResumePrime, Starfish may stop accepting AUs while its own source buffers are
    // full. The already-demuxed application queues behind it are still playable media (they have
    // passed transport, demux and timestamp validation, and feeding is open while the clock
    // remains held), so the audio-presence question reads the full pipeline high-water there. An
    // initial Load/seek is a fresh clock boundary whose old queued tail is not proof about the
    // newly loaded/landed timeline, so it reads accepted data only.
    let queued_runway = prime_kind == PrimeKind::Resume;
    let shift = SHARED.pts_shift.load(Ordering::Relaxed);
    let queued_audio = SHARED.audio_tail_ns.load(Ordering::Acquire);
    let playable_audio_high = if queued_runway && queued_audio >= 0 {
        eng.max_fed_audio_pts.max(queued_audio.saturating_add(shift))
    } else {
        eng.max_fed_audio_pts
    };
    let playable_abuf = playable_audio_high - eng.seek_base_pts;
    // An A/V stream requires both lanes; only a stream with no audio buffered at all may use the
    // video-only escape.
    let audio_expected = playable_abuf > 0;
    let decoder_ready = if audio_expected {
        accepted_vbuf >= PRIME_NS && accepted_abuf >= PRIME_AUDIO_NS
    } else {
        accepted_vbuf >= PRIME_VIDEO_MAX_NS
    };
    if decoder_ready && TX.feed_allowed() {
        let Some(play_token) = SHARED.reserve_prime_play(prime_kind) else {
            return;
        };
        // A paused preroll plays one picture for a viewer who is paused: keep that window silent.
        if TX.seek_preroll_active() {
            super::mute_for_preroll(mt);
        }
        let played = unsafe { sink().play(mt) };
        match SHARED.complete_prime_play(play_token, played != 0) {
            PlayCompletion::Accepted { resume_acb } => {
                if resume_acb {
                    super::acb_mirror_playstate_at(mt, eng.stage, true);
                }
                eng.prime_play = false;
                SHARED.seeking.store(false, Ordering::Relaxed); // playback resumed at the new position → HUD spinner off
                log(&format!(
                    "primed: v={}ms a={}ms -> Play",
                    accepted_vbuf / 1_000_000,
                    playable_abuf / 1_000_000,
                ));
            }
            PlayCompletion::Refused => {
                log("primed: Starfish refused Play; keeping the clock held and retrying");
            }
            PlayCompletion::Stale => {
                log("primed: Play result lost its clock token; teardown/transition won");
            }
        }
    }
}

/// **One tick's feeding, as a unit: video lane, audio lane, then the prime attempt.**
///
/// The order is load-bearing and predates this function - the VIDEO lane owns the seek rebase, so
/// it must clear `rebase_pending` and publish `pts_shift` before the audio lane reads them in the
/// same tick. What is new is the third step.
///
/// [`try_prime`] is called HERE rather than at the end of [`feed_audio_lane`] because that
/// function returns early on `rebase_pending`, and a tail call would then be skipped on exactly
/// the ticks a reload passes through.
///
/// **The first version of this doc justified the placement with the audioless case, and that
/// argument is FALSE.** Every `Source::Stream` allocates BOTH queues whether or not audio is
/// produced, so `aq_audio == None` is not a state a real stream reaches; and with no audio a
/// 2-second segment cannot satisfy the `PRIME_VIDEO_MAX_NS` escape from this call either, so the
/// placement neither helps nor harms it. The reason for the wrapper is that one function should
/// own what a tick does to the lanes - not that it rescues a case it never sees.
/// **The two lane feeders are PRIVATE so that this is the only way in.** A review found the hole:
/// with them `pub`, reverting `pump` to two separate lane calls restored the livelock while
/// the regression test - which calls this wrapper - stayed green. Private, that revert does not
/// compile.
pub fn feed_both_lanes(mt: &MainThread, eng: &mut Engine) {
    feed_stream(mt, eng);
    feed_audio_lane(mt, eng);
    try_prime(mt, eng);
}

fn feed_stream(mt: &MainThread, eng: &mut Engine) {
    let qp = match eng.aq_video.as_mut() {
        Some(q) => &mut **q as *mut AuQueue,
        None => return,
    };
    let slice = FeedSlice::begin();
    let mut fed = 0;
    while fed < 120 {
        // Feed each AU, throttled to ~MAX_FEED_AHEAD_NS ahead of the presented position per lane
        // (see the throttle below) rather than greedily to BufferFull.
        if eng.pending_video.is_none() {
            let mut eof: c_int = 0;
            let n = crate::aq::aq_pop(qp, &mut eof);
            if n.is_null() {
                // true EOF (producer done + video lane drained): signal end-of-stream ONCE so the
                // pipeline drains its last frames instead of hanging on them (Kodi keys EOS to the
                // video drain). Keyed on the video lane only.
                if eof != 0 && !eng.eos_pushed && eng.stage >= Stage::Streaming {
                    unsafe { sink().push_eos(mt) };
                    eng.eos_pushed = true;
                    log("EOS pushed at true EOF");
                }
                // Nothing to send. THE case the read-out's Feed row exists to name: a dead
                // PRODUCER, which every other field on the panel cannot tell from a dead sink.
                SHARED.dg_feed_state.store(5, Ordering::Relaxed);
                break;
            }
            eng.pending_video = Some(AuBox(n));
        }
        let n = eng.pending_video.as_ref().unwrap().0;
        let (es, key, pts, len, data) = unsafe { crate::aq::au_fields(n) };
        if eng.rebase_pending {
            if es == 1 && key != 0 {
                let completing_in_place_seek = eng.flushed;
                let mut segment_installed = false;
                // In-place seek: drop a keyframe that landed well AHEAD of the target - it's a stale
                // frame from the pre-flush read position (the reopen+av_seek hasn't taken effect yet),
                // not the real post-seek keyframe. Capped so a failed av_seek can't hang the rebase.
                if eng.flushed {
                    let target = SHARED.seek_target_ns.load(Ordering::Relaxed);
                    let stale = target >= 0
                        && (pts - target > SEEK_STALE_AHEAD_NS
                            || target - pts > SEEK_STALE_BEHIND_NS);
                    if stale && eng.rebase_drops < MAX_REBASE_DROPS {
                        eng.rebase_drops += 1;
                        if eng.rebase_drops == 1 {
                            log(&format!(
                                "rebase: dropping stale kf pts={}s (target {}s)",
                                pts / 1_000_000_000,
                                target / 1_000_000_000
                            ));
                        }
                        eng.pending_video = None;
                        continue;
                    }
                }
                if eng.flushed && super::seek_mode() == SeekMode::NativeSeek {
                    // NATIVE in-place seek (#568): feed the REAL content PTS (pts_shift=0,
                    // disp_base=0 -> playpos = presented real pts = content time, as in the
                    // Segment branch below) and anchor the pipeline with LG's public
                    // `StarfishMediaAPIs::Seek(millis)`. Seek is a flushing `gst_element_seek`
                    // whose new segment starts at EXACTLY the millis passed, so the PTS fed from
                    // here on is on that timeline by construction; the pump skipped its own
                    // `flush()` for this mode and nothing may flush after this (a `flush()` is
                    // itself a seek to 0 and re-creates the stale-segment stall). `feed_stream`
                    // runs on the main thread and is the only Feed caller, and this keyframe is
                    // not fed yet, so Seek cannot race a Feed. Never a negative millis: Seek
                    // refuses it, and a negative PTS is unparseable by Feed.
                    SHARED.pts_shift.store(0, Ordering::Relaxed);
                    let ms = pts.max(0) / 1_000_000;
                    let rv = unsafe { sink().seek(mt, ms) };
                    log(&format!("in-place seek(native): Seek({ms}) rv={rv}"));
                    if rv == 0 {
                        // A refused Seek can leave the pipeline refusing every Feed. Drop the rest
                        // of THIS SESSION's seeks to the reload path and redo THIS one as a
                        // reload at the user's target (the pump reads the mailbox next tick;
                        // `reload_at` REPLACES the engine). `eng.flushed`/`rebase_pending` stay
                        // set so the audio lane keeps holding and no AU reaches a pipeline that
                        // is rejecting Feed.
                        super::set_seek_mode(SeekMode::Reload);
                        // Latched across the reload this very seek is about to do, so the fresh
                        // session's `select_seek_mode` does not offer the same firmware another
                        // refused Seek on every later seek of this playback.
                        super::set_native_seek_refused(true);
                        log(
                            "in-place seek DOWNGRADE: native Seek refused -> reload-per-seek for \
                             the rest of this playback",
                        );
                        let tgt = SHARED.seek_target_ns.load(Ordering::Relaxed).max(0);
                        TX.seek_reqs.store(1, Ordering::Release);
                        TX.seek_to_ns.store(tgt, Ordering::Release);
                        eng.pending_video = None;
                        break;
                    }
                    segment_installed = true;
                    eng.flushed = false;
                } else if eng.flushed && super::seek_mode() != SeekMode::Segment {
                    // Flushed but not in a mode that anchors in place: a refused native `Seek`
                    // downgraded the session to `Reload` and the pump's reload (`reload_at`)
                    // is on its way. The private Segment path (`sf_pipeline`'s decompile-derived
                    // offsets, proven on webOS 4.5 only) must NEVER run here, so this keyframe
                    // anchors nothing: drop it and leave `flushed`/`rebase_pending` set, which
                    // keeps the audio lane holding until the reload replaces the engine.
                    eng.pending_video = None;
                    break;
                } else if eng.flushed {
                    // Kodi IN-PLACE seek (exact): feed the REAL content PTS (no rebase), tell the
                    // pipeline the real decode position, then inject a fresh GStreamer SEGMENT -
                    // this re-anchors the sink WITHOUT a reload/decoder re-init. disp_base=0 +
                    // pts_shift=0 → playpos = presented real pts = content time.
                    SHARED.pts_shift.store(0, Ordering::Relaxed);
                    let ok = unsafe { sink().set_time_to_decode(mt, pts) };
                    // setTimeToDecode returns 0 on webOS<11 (it needs PausedState); fall back to
                    // the content-info path (loadSpi_getInfo + setContentInfo(ptsToDecode)), which
                    // re-anchors the decode position while Playing. Then always inject the fresh
                    // GStreamer SEGMENT so the sink re-bases instead of stalling.
                    let ci = if ok == 0 {
                        unsafe { sink().set_content_info(mt, pts) }
                    } else {
                        1
                    };
                    let seg = unsafe { sink().send_segment(mt) };
                    segment_installed = seg != 0;
                    log(&format!("in-place seek: setTimeToDecode({pts}) rv={ok} setContentInfo={ci} sendSegment={seg}"));
                    if seg == 0 {
                        // The pipeline ptr wasn't reachable, so NO fresh GStreamer segment was
                        // injected and THIS seek's buffers are scheduled against the pre-seek
                        // segment - the stale-segment state that stops the sink draining and
                        // fills ~14.7 MB of upstream buffers in ~48 s (permanent BufferFull +
                        // "Playing error"; see reload_at's decompiled ground truth). So drop the
                        // rest of THIS SESSION's seeks to the reload path, which rebuilds the
                        // segment by construction; start_bufferfeed re-arms the flag for the next
                        // session (the SCOPE note in mod.rs argues why that is the right scope).
                        // Logged loudly because this is the single most consequential state
                        // change in the seek path and it used to be invisible: instant seeks
                        // silently became multi-second reloads with nothing in the event log
                        // naming the cause - only the `sendSegment=0` above, whose consequence
                        // was not stated anywhere.
                        super::set_seek_mode(SeekMode::Reload);
                        log(
                            "in-place seek DOWNGRADE: sendSegment=0 (CustomPipeline unreachable) \
                             -> reload-per-seek for the rest of this session",
                        );
                    }
                    eng.flushed = false;
                } else {
                    // reload / initial-resume seek: rebase the landed keyframe to fed-pts 0 (the
                    // fresh Load's pipeline expects a 0-based feed; disp_base carries the offset).
                    SHARED.pts_shift.store(-pts, Ordering::Relaxed);
                    // An HLS VOD stream reads in playlist time: this keyframe is where the
                    // segment the demuxer opened at begins (`ff::open_hls`).
                    let base = SHARED.hls_vod_base_ns.load(Ordering::Acquire);
                    if base >= 0 {
                        SHARED.disp_base.store(base - pts, Ordering::Relaxed);
                    }
                }
                eng.rebase_pending = false; // releases the AUDIO lane (which holds until this clears)
                eng.seek_base_pts = pts + SHARED.pts_shift.load(Ordering::Relaxed); // fed-pts base
                                                                                    // Installing a SEGMENT is not acceptance of the AU which establishes it. Keep
                                                                                    // callbacks fenced across BufferFull/error and arm only below, after this exact
                                                                                    // retained keyframe has returned `O` from sf_feed.
                eng.presentation_rearm_pending = completing_in_place_seek && segment_installed;
                log(&format!(
                    "rebase: first post-seek keyframe pts={pts} -> pts_shift={}",
                    SHARED.pts_shift.load(Ordering::Relaxed)
                ));
            } else {
                eng.pending_video = None; // drop pre-keyframe AUs
                continue;
            }
        }
        let mut fp = pts + SHARED.pts_shift.load(Ordering::Relaxed) + pts_nudge_ns();
        if fp < eng.max_fed_video_pts - video_span_ns(STALE_BACKJUMP_NS, eng.video_frame_ns) {
            eng.pending_video = None; // stale (a big backward jump)
            continue;
        }
        if fp < 0 {
            fp = 0;
        }
        // Feed-ahead throttle: don't feed an AU that's already more than its lane's budget ahead
        // of the presented position - keep it pending and retry once the pipeline presents more.
        // Skipped while priming (to reach PRIME_NS before Play; the per-tick slice below is waived
        // until the lane holds its prime depth, then spreads the rest). Each lane's queue is
        // pts-ordered, so if the head is over budget everything behind it is too; breaking is right.
        let pres = eng.feed_pace.reference(SHARED.pres_fed.load(Ordering::Relaxed), super::vclock_ms());
        if !eng.prime_play {
            let budget = if es == 1 {
                video_span_ns(MAX_FEED_AHEAD_NS, eng.video_frame_ns)
            } else {
                MAX_FEED_AHEAD_NS + AUDIO_SLACK_NS
            };
            if pres != PRES_NONE && fp - pres > budget {
                SHARED.dg_feed_state.store(4, Ordering::Relaxed); // waiting for a frame
                break;
            }
        }
        // Out of this tick's slice: the AU stays pending, as on a throttle stop, for the next tick.
        // A lane short of media is exempt: it is priming or its lead is nearly gone.
        let starved = lane_starved(
            eng.prime_play,
            eng.max_fed_video_pts,
            eng.seek_base_pts,
            PRIME_NS,
            pres,
        );
        if slice.spent(fed, starved) {
            break;
        }
        let presentation_probe = if eng.presentation_rearm_pending {
            let armed = SHARED.begin_native_presentation_probe(eng.native_epoch);
            if !armed {
                log("rebase: native presentation epoch retired before post-seek Feed");
            }
            armed
        } else {
            false
        };
        let r = tv_base::diag::spans::span("sfv", || unsafe { sink().feed(mt, data, len as u32, fp, es) });
        if presentation_probe {
            if (r as u8) == b'O' {
                if !SHARED.commit_native_presentation_probe(eng.native_epoch, |num| {
                    super::sf_on_event_inner(0, num, std::ptr::null(), super::shared::EventScope::Live)
                }) {
                    log("rebase: native presentation probe lost its epoch after accepted Feed");
                }
            } else if !SHARED.reject_native_presentation_probe(eng.native_epoch) {
                log("rebase: native presentation probe lost its epoch after rejected Feed");
            }
        }
        // **Only an ACCEPTED feed advances the mark.** `max_fed_*_pts` is the high-water of what
        // the PIPELINE holds and `try_prime` starts the clock off it, so counting a rejected AU
        // primes against media the decoder never received. This advanced before the reply was read
        // for as long as the check lived inside this loop - where it was video-only, and one
        // frame early. `feed_both_lanes` made the same slip reachable from the AUDIO lane, which
        // the old check could not prime from at all, so the latent sloppiness became a defect.
        if (r as u8) == b'O' && fp > eng.max_fed_video_pts {
            eng.max_fed_video_pts = fp;
        }
        // prime-then-play: once PRIME_NS of the fresh (post-seek/resume) stream is buffered,
        // start the clock. The pipeline was paused through the reopen gap, so it now presents
        // from the seek point in A/V sync instead of fast-forwarding to a clock that ran ahead.
        // Start the clock once BOTH lanes are buffered past the seek base: video to PRIME_NS AND
        // audio to PRIME_AUDIO_NS. Priming on video ALONE started the audioSync MASTER clock with
        // an empty audio queue, so a rapid-seek drain could leave audio silent until the next seek.
        // The video-buffer fallback still starts an audioless/briefly-starved stream (no hang).
        try_prime(mt, eng);
        // The log cadence counts ATTEMPTS (its `reply=` field is the only record of a rejected
        // feed, and tests/run.py greps `feed v#`), so it keeps its own counter. VTOT counts what
        // was ACCEPTED - see below.
        if es == 1 {
            let v = VATT.fetch_add(1, Ordering::Relaxed) + 1;
            if v <= 4 || v % 100 == 0 {
                let qb = crate::aq::aq_bytes(qp);
                log(&format!(
                    "feed v#{v} sz={len} fed={fp} reply={} qbytes={qb}",
                    r as u8 as char
                ));
            }
        }
        if (r as u8) != b'O' {
            // 'B' BufferFull -> keep pending, retry next tick (VIDEO lane only)
            SHARED
                .dg_feed_state
                .store(if (r as u8) == b'B' { 2 } else { 3 }, Ordering::Relaxed);
            break;
        }
        if eng.presentation_rearm_pending {
            eng.presentation_rearm_pending = false;
        }
        SHARED.dg_feed_state.store(1, Ordering::Relaxed); // accepting
                                                          // COUNTED HERE, below the reply test, so `Fed` means AUs the pipeline TOOK. Counted above
                                                          // it, a permanently-full sink re-offered and re-counted the same retained AU every tick,
                                                          // and the read-out showed a brisk feed rate through a stall.
        if es == 1 {
            VTOT.fetch_add(1, Ordering::Relaxed);
        }
        eng.pending_video = None;
        fed += 1;
    }
}

/// Whether an audio AU at fed pts `fp` is in its timeline's lead-in: it is the AU the demuxer
/// marked as the first since its open or seek (`opens`), or lies within [`AUDIO_LEAD_IN_NS`] past
/// the one that was accepted (`start`). Lead-in audio is fed ahead of the clock: neither the
/// stale-ahead drop nor the feed-ahead throttle applies to it.
fn audio_lead_in(fp: i64, opens: bool, start: Option<i64>) -> bool {
    opens || start.is_some_and(|s| fp >= s && fp - s < AUDIO_LEAD_IN_NS)
}

/// A rate-limited line for an audio AU the lane did not feed: the first 8 of a session, then one
/// in 200, with the fed pts and what it was measured against (live streams have shown audio
/// timelines seconds away from their video's).
fn audio_note(why: &str, fp: i64, video_fed: i64, pres: i64) {
    static N: AtomicI64 = AtomicI64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 8 || n % 200 == 0 {
        let pres = if pres == PRES_NONE { "none".to_string() } else { format!("{}ms", pres / 1_000_000) };
        log(&format!("feed: audio #{n} {why} (fp={}ms video_fed={}ms pres={pres})", fp / 1_000_000, video_fed / 1_000_000));
    }
}

/// AUDIO lane feeder (two-lane ff path only). Independent of the video lane: its own queue, its
/// own fed-pts high-water, its own BufferFull retry - so a video BufferFull never starves audio.
/// HOLDS while a seek rebase is pending (the VIDEO lane sets pts_shift on its first post-seek
/// keyframe; feeding audio before that would use a stale shift → A/V desync). No prime/Play here -
/// only the video lane starts the clock. Called AFTER feed_stream each tick, so a same-tick rebase
/// is already visible.
fn feed_audio_lane(mt: &MainThread, eng: &mut Engine) {
    if eng.rebase_pending {
        return; // wait for the video lane to publish pts_shift
    }
    let qp = match eng.aq_audio.as_mut() {
        Some(q) => &mut **q as *mut AuQueue,
        None => return,
    };
    // hoisted out of the loop: pts_shift is stable once rebase clears (only the video lane's rebase
    // arm writes it, on this same thread), and one pres_fed sample per tick is plenty against the
    // multi-second audio budget.
    // the viewer's audio sync rides the same shift: every fed audio PTS, and so every check below
    // (the hold-ahead, the stale guards, the prime depth), is on the shifted clock
    let shift = SHARED.pts_shift.load(Ordering::Relaxed) + super::audio_offset_ns();
    let pres = eng.feed_pace.reference(SHARED.pres_fed.load(Ordering::Relaxed), super::vclock_ms());
    let slice = FeedSlice::begin();
    let mut fed = 0;
    while fed < 120 {
        if eng.pending_audio.is_none() {
            let mut eof: c_int = 0;
            let n = crate::aq::aq_pop(qp, &mut eof);
            if n.is_null() {
                break;
            }
            eng.pending_audio = Some(AuBox(n));
        }
        let n = eng.pending_audio.as_ref().unwrap().0;
        let (es, _key, pts, len, data) = unsafe { crate::aq::au_fields(n) };
        let mut fp = pts + shift;
        let first_pts = SHARED.first_audio_pts.load(Ordering::Acquire);
        let opens = first_pts != i64::MIN && pts == first_pts;
        let lead_in = audio_lead_in(fp, opens, eng.audio_start_fp);
        // Stale drifted audio from before a seek's reopen: far AHEAD of the freshly-anchored video.
        // Drop it (else it poisons max_fed_audio_pts and stalls the audio clock → playback sticks).
        // A late-starting track is far ahead too, legitimately, so it is measured from its own
        // start as well as from the video (see AUDIO_LEAD_IN_NS).
        let stale_ref = eng.max_fed_video_pts.max(eng.audio_start_fp.unwrap_or(i64::MIN));
        if !lead_in && eng.max_fed_video_pts > 0 && fp > stale_ref + AUDIO_STALE_AHEAD_NS {
            audio_note("dropped: far ahead of the video", fp, eng.max_fed_video_pts, pres);
            eng.pending_audio = None;
            continue;
        }
        if fp < eng.max_fed_audio_pts - STALE_BACKJUMP_NS {
            audio_note("dropped: behind the audio fed", fp, eng.max_fed_video_pts, pres);
            eng.pending_audio = None; // stale (a big backward jump)
            continue;
        }
        if fp < 0 {
            fp = 0;
        }
        if !eng.prime_play
            && !lead_in
            && pres != PRES_NONE
            && fp - pres > MAX_FEED_AHEAD_NS + AUDIO_SLACK_NS
        {
            audio_note("held: ahead of the presented position", fp, eng.max_fed_video_pts, pres);
            break;
        }
        let starved = lane_starved(eng.prime_play, eng.max_fed_audio_pts, eng.seek_base_pts, PRIME_AUDIO_NS, pres);
        if slice.spent(fed, starved) {
            break;
        }
        let r = tv_base::diag::spans::span("sfa", || unsafe { sink().feed(mt, data, len as u32, fp, es) });
        // Accepted only - see the video lane's note.
        if (r as u8) == b'O' && fp > eng.max_fed_audio_pts {
            eng.max_fed_audio_pts = fp;
        }
        if (r as u8) == b'O' && opens {
            if eng.audio_start_fp.is_none() && fp - eng.max_fed_video_pts > MAX_FEED_AHEAD_NS {
                log(&format!(
                    "feed: audio starts {}ms after the video fed so far; fed ahead of the clock",
                    (fp - eng.max_fed_video_pts) / 1_000_000
                ));
            }
            eng.audio_start_fp = Some(fp);
        }
        let a = AATT.fetch_add(1, Ordering::Relaxed) + 1;
        if a <= 4 || a % 200 == 0 {
            let qb = crate::aq::aq_bytes(qp);
            log(&format!(
                "feed a#{a} sz={len} fed={fp} reply={} qbytes={qb}",
                r as u8 as char
            ));
        }
        if (r as u8) == b'O' {
            ATOT.fetch_add(1, Ordering::Relaxed); // accepted, not attempted - see feed_stream
        }
        if (r as u8) != b'O' {
            break; // 'B' BufferFull -> keep pending, retry next tick (AUDIO lane only)
        }
        eng.pending_audio = None;
        fed += 1;
    }
}

/// feed the looped `sample.h264` validation sample from the install's runtime root
/// (continuous PTS @ 23.976).
pub fn feed_sample(mt: &MainThread, eng: &mut Engine) {
    let s = match &mut eng.source {
        Source::Sample(s) => s,
        _ => return,
    };
    let naus = s.au.len();
    if naus < 2 {
        return;
    }
    let mut fed = 0;
    while fed < 60 {
        if s.next >= naus - 1 {
            s.next = 0;
            s.loops += 1;
        }
        let off = s.au[s.next];
        let end = s.au[s.next + 1];
        let pts = (s.loops * (naus as i64 - 1) + s.next as i64) * 41708333;
        let r = unsafe { sink().feed(mt, s.data[off..].as_ptr(), (end - off) as u32, pts, 1) };
        if (r as u8) != b'O' {
            break;
        }
        s.next += 1;
        fed += 1;
    }
}

/// Whether the decompiled StarfishMediaAPIs offsets (see the NOR comment in `start_bufferfeed_inner`)
/// may be trusted for in-place seek. The old-ABI `Feed` is the primary gate: it proves
/// libplayerAPIs is the pre-C++11 build, so 3.9.x is excluded without listing releases. A known
/// release below 4 is refused too; `major == 0` (release unknown) defers to the Feed evidence.
fn inplace_layout_known(major: u32, feed_abi_is_cow: bool) -> bool {
    !feed_abi_is_cow && (major == 0 || major >= 4)
}

/// [`select_seek_mode`] for a session that is starting: a `NativeSeek` this playback already had
/// REFUSED stays `Reload` (the latch `feed_stream` sets, which `reload_at` preserves and a fresh
/// play clears), instead of paying reopen + keyframe + refused `Seek` + full `Load` on every seek.
pub(super) fn session_seek_mode(layout_known: bool, window_mode: c_int, dev: Option<SeekMode>) -> SeekMode {
    match select_seek_mode(layout_known, window_mode, dev) {
        SeekMode::NativeSeek if super::native_seek_refused() => SeekMode::Reload,
        mode => mode,
    }
}

/// How a direct-play seek re-anchors the pipeline in a new session (see [`SeekMode`]).
///
/// - `VP_EXPORTED` (webOS 5+) -> [`SeekMode::NativeSeek`]: LG's PUBLIC `StarfishMediaAPIs::Seek`.
///   The Kodi in-place path (`Segment`) is NOT used there: it reaches LG-private C++ objects
///   through two decompile-derived offsets (`StarfishMediaAPIs::player` at object+0x4c, then
///   `AbstractPlayer::pipeline` at +0x04, plus `MEDIA_CUSTOM_CONTENT_INFO`'s `ptsToDecode` at
///   +0x28) read off a webOS 4.5 binary, which nothing in a symbol table can confirm on another
///   firmware: a field added to StarfishMediaAPIs by any 5.x build moves +0x4c and the very next
///   seek dereferences whatever now lives there. `Seek` is an exported symbol, present on every
///   firmware 1.2 to 11.2, so it assumes no layout at all.
/// - A known layout without `VP_EXPORTED` (webOS 4.x) -> [`SeekMode::Segment`], unchanged.
/// - NOR when the library is the pre-C++11 build (`inplace_layout_known` false): its
///   StarfishMediaAPIs is a different object from the 4.5 one, and the first fast-forward on a
///   3.4.0 set crashed through those offsets. The old-ABI Feed (`sf_feed_abi_is_cow`) is the
///   direct evidence, and it covers 3.9.x too. That COW Feed ABI is also unverified for `Seek`, so
///   this is [`SeekMode::Reload`], as before. A KNOWN release below 4 is refused as well; an
///   UNKNOWN one (`major == 0`, os_info unreadable) with the C++11 Feed keeps in-place seek, as
///   before #578, because that Feed says the library is 4.x or later.
/// `dev` is the `webosiptv-seekmode` dev trigger and wins when present, so a webOS 4.5 set can
/// exercise `NativeSeek`.
pub(super) fn select_seek_mode(layout_known: bool, window_mode: c_int, dev: Option<SeekMode>) -> SeekMode {
    if let Some(forced) = dev {
        return forced;
    }
    if window_mode == super::VP_EXPORTED {
        SeekMode::NativeSeek
    } else if layout_known {
        SeekMode::Segment
    } else {
        SeekMode::Reload
    }
}

/// `/tmp/webosiptv-seekmode`: `native`, `segment` or `reload`. `None` without the `devtriggers`
/// feature (`devtrig::read` folds away), so a release build cannot be steered.
fn dev_seek_mode() -> Option<SeekMode> {
    match tv_base::devtrig::read("seekmode")?.trim() {
        "native" => Some(SeekMode::NativeSeek),
        "segment" => Some(SeekMode::Segment),
        "reload" => Some(SeekMode::Reload),
        _ => None,
    }
}

#[cfg(test)]
mod feed_pace_tests {
    use super::{FeedPace, FEED_PACE_CAP_MS, MAX_FEED_AHEAD_NS, PRES_NONE};

    const MS: i64 = 1_000_000;

    /// Video AUs (24 fps, 41.7 ms apart) the throttle admits at each 60 Hz frame of a 1x clock
    /// whose position reports arrive every 200 ms - with `reference` as the throttle's position.
    fn admitted_per_frame(reference: impl Fn(&mut FeedPace, i64, u32) -> i64) -> Vec<u32> {
        let mut pace = FeedPace::default();
        let au_ns = 41_708_333i64;
        let mut next_pts = MAX_FEED_AHEAD_NS; // the lane starts full
        let mut out = Vec::new();
        for frame in 0..120u32 {
            let now = 1_000 + frame * 1000 / 60;
            let true_pos = (now as i64 - 1_000) * MS;
            let pres = true_pos - true_pos % (200 * MS); // the 5 Hz step function
            let refpos = reference(&mut pace, pres, now);
            let mut n = 0;
            while next_pts - refpos <= MAX_FEED_AHEAD_NS {
                next_pts += au_ns;
                n += 1;
            }
            out.push(n);
        }
        out
    }

    /// **Regression: the 5 Hz feed burst.** Against the raw report the lane opens 200 ms at a
    /// time: ~5 AUs land on one frame and none on the eleven around it. Run on at 1x it never
    /// admits more than one video AU a frame, and admits the same media over the same time.
    #[test]
    fn the_reference_spreads_the_five_hertz_burst_across_frames() {
        let raw = admitted_per_frame(|_, pres, _| pres);
        let paced = admitted_per_frame(|p, pres, now| p.reference(pres, now));
        assert!(raw.iter().max().copied().unwrap_or(0) >= 4, "the raw step feeds in bursts: {raw:?}");
        assert!(paced.iter().all(|&n| n <= 1), "paced: at most one AU a frame: {paced:?}");
        let (r, p): (u32, u32) = (raw.iter().sum(), paced.iter().sum());
        assert!(p.abs_diff(r) <= 6, "same media over the same two seconds: raw {r}, paced {p}");
    }

    /// A `now` BEFORE the stamp of the last report (the throttle read its clock earlier than the
    /// report was first seen) is no run-on, not a full-cap one.
    #[test]
    fn a_clock_behind_the_report_stamp_does_not_run_on() {
        let mut pace = FeedPace::default();
        assert_eq!(pace.reference(5_000 * MS, 1_000), 5_000 * MS);
        assert_eq!(pace.reference(5_000 * MS, 900), 5_000 * MS);
    }

    /// A clock that stops reporting new positions (paused, stalled) opens the lane by at most
    /// the cap, never without bound; `PRES_NONE` passes through and forgets the last report.
    #[test]
    fn a_frozen_report_runs_on_at_most_the_cap_and_none_passes_through() {
        let mut pace = FeedPace::default();
        assert_eq!(pace.reference(5_000 * MS, 100), 5_000 * MS);
        assert_eq!(pace.reference(5_000 * MS, 150), 5_050 * MS);
        assert_eq!(pace.reference(5_000 * MS, 100_000), 5_000 * MS + FEED_PACE_CAP_MS as i64 * MS);
        assert_eq!(pace.reference(PRES_NONE, 100_001), PRES_NONE);
        assert_eq!(pace.reference(5_000 * MS, 100_002), 5_000 * MS, "a fresh report restarts the run-on");
    }
}

#[cfg(test)]
mod native_lifecycle_proof_tests {
    use super::{native_object_disposition, NativeObjectDisposition};

    #[test]
    fn destructor_requires_every_independent_runtime_proof() {
        assert_eq!(
            native_object_disposition(true, true, true),
            NativeObjectDisposition::Destroy,
        );
        for (unload_completed, callback_gate_proven, rust_epoch_retired) in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
            (false, false, false),
        ] {
            assert_eq!(
                native_object_disposition(
                    unload_completed,
                    callback_gate_proven,
                    rust_epoch_retired,
                ),
                NativeObjectDisposition::Quarantine,
            );
        }
    }
}

#[cfg(all(test, feature = "hostsim"))]
mod native_lifecycle_host_seam_tests {
    use super::*;

    struct FreshProcess;

    impl Drop for FreshProcess {
        fn drop(&mut self) {
            crate::player::ffi_host::reset_native_lifecycle_for_test();
            crate::player::ffi_host::force_clocksink_for_test(false);
            SHARED.reset_session();
        }
    }

    #[test]
    fn host_seam_destroys_only_with_evidence_and_latches_quarantine() {
        let _serial = tv_base::testlock::serial();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        SHARED.reset_session();
        let _fresh_process = FreshProcess;
        let mt = unsafe { MainThread::assume() };

        let first = SHARED.begin_native_session().expect("first native epoch");
        assert_eq!(unsafe { crate::player::sink().load(c"{}".as_ptr(), first) }, 1);
        assert!(unsafe { crate::player::sink().callback_intercepts(&mt) } > 0);
        unsafe { crate::player::sink().unload(&tv_platform::tv::sink::UnloadTicket::issue(&mt)) };
        assert!(SHARED.native_unload_completed(first));
        assert_eq!(unsafe { crate::player::sink().callback_gate_retire(&mt) }, 1);
        assert!(SHARED.retire_native_session(first));
        assert_eq!(unsafe { crate::player::sink().destroy(&mt) }, 1);
        assert_eq!(unsafe { crate::player::sink().ready(&mt) }, 0);

        SHARED.reset_session();
        let unproven = SHARED
            .begin_native_session()
            .expect("unproven native epoch");
        assert_eq!(unsafe { crate::player::sink().load(c"{}".as_ptr(), unproven) }, 1);
        crate::player::ffi_host::force_callback_intercepts_for_test(0);
        unsafe { crate::player::sink().unload(&tv_platform::tv::sink::UnloadTicket::issue(&mt)) };
        assert_eq!(unsafe { crate::player::sink().callback_gate_retire(&mt) }, 0);
        assert!(SHARED.retire_native_session(unproven));
        unsafe { crate::player::sink().quarantine(&mt) };

        SHARED.reset_session();
        let refused = SHARED.begin_native_session().expect("refused native epoch");
        assert_eq!(
            unsafe { crate::player::sink().load(c"{}".as_ptr(), refused) },
            0,
            "quarantine must remain a process-long refusal",
        );
        assert!(SHARED.retire_native_session(refused));
    }

    /// The window breadcrumbs `release_native_object` leaves, as a host can see them. Drains
    /// whatever an earlier test left so a test sees only the steps it runs.
    struct Steps;

    impl Steps {
        fn fresh() -> Self {
            take_window_steps_for_test();
            Steps
        }
    }

    impl Drop for Steps {
        fn drop(&mut self) {
            take_window_steps_for_test();
        }
    }

    fn loaded_epoch(mt: &MainThread) -> u32 {
        let epoch = SHARED.begin_native_session().expect("native epoch");
        assert_eq!(unsafe { crate::player::sink().load(c"{}".as_ptr(), epoch) }, 1);
        assert_eq!(unsafe { crate::player::sink().ready(mt) }, 1);
        epoch
    }

    #[test]
    fn a_proven_release_leaves_unload_destroyed_and_window_destroy_in_order() {
        let _serial = tv_base::testlock::serial();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        SHARED.reset_session();
        let _fresh_process = FreshProcess;
        let steps = Steps::fresh();
        let mt = unsafe { MainThread::assume() };
        let epoch = loaded_epoch(&mt);
        release_native_object(&mt, epoch);
        assert_eq!(
            take_window_steps_for_test(),
            ["native_unload", "native_destroyed", "exported_window_destroy"],
        );
        drop(steps);
    }

    #[test]
    fn an_unproven_release_records_quarantine_not_destroyed() {
        let _serial = tv_base::testlock::serial();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        SHARED.reset_session();
        let _fresh_process = FreshProcess;
        let steps = Steps::fresh();
        let mt = unsafe { MainThread::assume() };
        let epoch = loaded_epoch(&mt);
        // No callback ever crossed the gate: the evidence is incomplete and the object is kept.
        crate::player::ffi_host::force_callback_intercepts_for_test(0);
        release_native_object(&mt, epoch);
        assert_eq!(
            take_window_steps_for_test(),
            ["native_unload", "native_quarantined", "exported_window_destroy"],
        );
        drop(steps);
    }

    #[test]
    fn a_seam_rejected_destroy_is_recorded_as_quarantine() {
        let _serial = tv_base::testlock::serial();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        SHARED.reset_session();
        let _fresh_process = FreshProcess;
        let steps = Steps::fresh();
        let mt = unsafe { MainThread::assume() };
        let epoch = loaded_epoch(&mt);
        // Rust's three proofs hold, but the C seam's own evidence does not: `destroy` answers 0,
        // and the crumb must say so rather than "destroyed".
        crate::player::ffi_host::force_destroy_refusal_for_test(true);
        release_native_object(&mt, epoch);
        let recorded = take_window_steps_for_test();
        assert!(recorded.contains(&"native_quarantined"), "{recorded:?}");
        assert!(!recorded.contains(&"native_destroyed"), "{recorded:?}");
        drop(steps);
    }

    #[test]
    fn the_acb_plane_unload_is_a_step_only_when_acb_is_the_plane() {
        let _serial = tv_base::testlock::serial();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        SHARED.reset_session();
        let _fresh_process = FreshProcess;
        let steps = Steps::fresh();
        let mt = unsafe { MainThread::assume() };
        let epoch = loaded_epoch(&mt);
        ACB_OK.store(true, Ordering::Relaxed);
        release_native_object(&mt, epoch);
        ACB_OK.store(false, Ordering::Relaxed);
        let recorded = take_window_steps_for_test();
        let at = |label| recorded.iter().position(|l| *l == label);
        assert!(
            at("native_unload") < at("acb_plane_unload") && at("acb_plane_unload") < at("native_destroyed"),
            "{recorded:?}",
        );
        drop(steps);
    }

    #[test]
    fn without_a_dispatchable_object_or_an_exported_window_nothing_is_recorded() {
        let _serial = tv_base::testlock::serial();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(false);
        SHARED.reset_session();
        let _fresh_process = FreshProcess;
        let steps = Steps::fresh();
        let mt = unsafe { MainThread::assume() };
        let epoch = SHARED.begin_native_session().expect("native epoch");
        release_native_object(&mt, epoch);
        assert!(
            take_window_steps_for_test().is_empty(),
            "the seam's destroy_window is a no-op outside VP_EXPORTED, so it is not a step",
        );
        drop(steps);
    }

    #[test]
    fn creating_the_exported_window_is_a_step_only_in_that_mode() {
        let _serial = tv_base::testlock::serial();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        SHARED.reset_session();
        let _fresh_process = FreshProcess;
        let steps = Steps::fresh();
        let mt = unsafe { MainThread::assume() };
        crate::player::ffi_host::force_clocksink_for_test(false);
        let _ = with_window_id(&mt, "{}");
        assert!(take_window_steps_for_test().is_empty());
        crate::player::ffi_host::force_clocksink_for_test(true);
        let _ = with_window_id(&mt, "{}");
        assert_eq!(
            take_window_steps_for_test(),
            ["exported_window_create"],
        );
        drop(steps);
    }
}

#[cfg(test)]
mod payload_tests {
    use super::{build_av_payload, with_dolby_hdr_info, with_immersive, SinkEnvelope, PAYLOAD_AV, PAYLOAD_H265, PAYLOAD_V};
    use crate::stream::{Dovi, DvPresentation, StreamDeclaration};
    use tv_platform::devcaps::dv::DvCapability;

    fn p5() -> Dovi {
        Dovi { present: true, profile: 5, bl_compat: 0, el_present: false }
    }

    fn p8() -> Dovi {
        Dovi { present: true, profile: 8, bl_compat: 1, el_present: false }
    }

    fn hevc_p8() -> StreamDeclaration {
        StreamDeclaration { vcodec: "hevc".into(), acodec: "eac3".into(), fps: 23.976, dovi: p8(), ..StreamDeclaration::default() }
    }

    const UHD: SinkEnvelope = SinkEnvelope { w: 3840, h: 2160, fps: 60 };

    /// A soft-decoded track's Load declares `"PCM"` and carries `contents.pcmInfo` in the decoder's
    /// shape; every other feed's payload is untouched.
    #[test]
    fn a_pcm_feed_declares_pcm_with_its_pcm_info() {
        use crate::stream::AudioFeed;
        let decl = StreamDeclaration { vcodec: "h264".into(), acodec: "mp2".into(), audio_channels: 2, ..StreamDeclaration::default() };
        let feed = AudioFeed::SoftPcm { source: "mp2".into(), channels: 2 };
        let p = super::with_pcm_info(&build_av_payload(&decl, DvPresentation::NotDv, "H264", "PCM", UHD), &feed);
        let v: serde_json::Value = serde_json::from_str(&p).expect("valid JSON");
        let contents = &v["args"][0]["option"]["externalStreamingInfo"]["contents"];
        assert_eq!(contents["codec"]["audio"], "PCM");
        assert_eq!(contents["format"], "RAW");
        assert_eq!(contents["pcmInfo"]["channelMode"], "stereo");
        assert_eq!(contents["pcmInfo"]["format"], "S16LE");
        assert_eq!(contents["pcmInfo"]["sampleRate"], 48.0);
        let ac3 = build_av_payload(&decl, DvPresentation::NotDv, "H264", "AC3", UHD);
        assert_eq!(super::with_pcm_info(&ac3, &AudioFeed::Native { payload: "AC3", diag: 1 }), ac3);
    }

    #[test]
    fn dv_payload_matches_the_frozen_session_decision() {
        let decl = hevc_p8();
        let declared = decl.dovi.presentation(true, DvCapability::Supported, true);
        let payload = build_av_payload(&decl, declared, "H265", "AC3 PLUS", UHD);
        assert!(payload.contains("DolbyHdrInfo"), "{payload}");
        assert!(payload.contains(r#""profileId":8"#), "{payload}");

        let refused = decl.dovi.presentation(true, DvCapability::Unsupported, true);
        let payload = build_av_payload(&decl, refused, "H265", "AC3 PLUS", UHD);
        assert!(!payload.contains("DolbyHdrInfo"), "{payload}");
    }

    /// The payload follows the decision the session froze, not a later capability answer.
    #[test]
    fn a_late_capability_result_does_not_change_the_installed_decision() {
        let decl = hevc_p8();
        let fresh = decl.dovi.presentation(true, DvCapability::Supported, true);
        assert!(fresh.declared().is_some(), "a subsequent decision sees Supported");
        let payload = build_av_payload(&decl, DvPresentation::NotDv, "H265", "AC3 PLUS", UHD);
        assert!(!payload.contains("DolbyHdrInfo"), "{payload}");
    }

    /// The declared rate reaches esInfo as an exact rational.
    #[test]
    fn the_declared_rate_rides_es_info() {
        let payload = build_av_payload(&hevc_p8(), DvPresentation::NotDv, "H265", "AC3 PLUS", UHD);
        assert!(payload.contains(r#""videoFpsValue":24000,"videoFpsScale":1001"#), "{payload}");
        let none = StreamDeclaration { fps: 0.0, ..hevc_p8() };
        assert!(!build_av_payload(&none, DvPresentation::NotDv, "H265", "AC3 PLUS", UHD).contains("videoFps"));
    }

    /// **Every Load payload must carry the `appId` placeholder, exactly once**, because that key
    /// is what `with_window_id` splices the webOS 5+ `option.windowId` onto - without it the
    /// decoded video has nowhere to bind on every set from 5.0 up - and because a second one would
    /// splice a second `windowId`.
    ///
    /// This replaces a panel row that was proposed and rejected: the "not spliced - no anchor" arm
    /// is UNREACHABLE at runtime (the placeholder is in all three constants and `build_av_payload`
    /// never touches it), so a row reporting it would print the same constant on a working and a
    /// broken television. The real risk is a payload edited a year from now that drops it - a
    /// build-time risk, caught at build time. The placeholder is a strictly better witness than the
    /// literal id it replaced: an id can be spelled correctly by accident, `@APPID@` cannot.
    #[test]
    fn every_load_payload_carries_the_app_id_placeholder_exactly_once() {
        for (name, p) in [
            ("PAYLOAD_V", PAYLOAD_V),
            ("PAYLOAD_AV", PAYLOAD_AV),
            ("PAYLOAD_H265", PAYLOAD_H265),
        ] {
            assert_eq!(
                p.matches(r#""appId":"@APPID@""#).count(),
                1,
                "{name} must carry the appId placeholder exactly once — webOS 5+ video binds on it"
            );
        }
    }

    /// **The shipped app's payload must be byte-identical to what every release so far sent**, and
    /// the splice anchor must be the composed key rather than a second spelling of it.
    ///
    /// This is the whole safety argument for making the id dynamic. The webOS 5+ `windowId` path
    /// cannot be exercised on this project's 4.5 dev set (`vp_mode()` returns `VP_ACB` there and
    /// `with_window_id` returns early), so its failure - a black video plane with working audio,
    /// and no error line - would not be seen until somebody else's television. Pinning the
    /// composed bytes here is the only gate available for it.
    #[test]
    fn the_shipped_app_composes_the_payload_it_always_did() {
        let want = format!(r#""appId":"{}""#, tv_base::paths::STABLE_APP_ID);
        let want = want.as_str();
        for (name, p) in [
            ("PAYLOAD_V", PAYLOAD_V),
            ("PAYLOAD_AV", PAYLOAD_AV),
            ("PAYLOAD_H265", PAYLOAD_H265),
        ] {
            let composed = p.replace("@APPID@", tv_base::paths::STABLE_APP_ID);
            assert!(
                composed.contains(want),
                "{name} no longer composes the shipped key"
            );
            // key ORDER too: `appId` stays the first key of `option`, where it has always been.
            assert!(
                composed.contains(&format!(r#""option":{{{want},"#)),
                "{name} moved the appId key"
            );
        }
        // And the anchor the splice looks for is the composed key, not a restatement of it.
        assert_eq!(
            super::app_id_key(),
            format!(r#""appId":"{}""#, tv_base::paths::app_id())
        );
        assert!(super::with_app_id(PAYLOAD_V).contains(&super::app_id_key()));
        assert!(!super::with_app_id(PAYLOAD_V).contains("@APPID@"));
    }

    /// The Dolby Vision node's anchor, with the same reasoning as the one above: `provider` is the
    /// LAST key of `contents`, which is where `DolbyHdrInfo` has to land - the pipeline reads it at
    /// `option.externalStreamingInfo.contents.DolbyHdrInfo` and nowhere else. Exactly once, or a
    /// `replace` would splice two nodes into one payload.
    #[test]
    fn the_av_payload_carries_exactly_one_dolby_hdr_info_anchor() {
        assert_eq!(PAYLOAD_AV.matches(r#""provider":"webosiptv""#).count(), 1);
        // and it really is the last key of `contents` - the next character after it closes the
        // object, so appending a key there stays INSIDE `contents`
        assert!(PAYLOAD_AV.contains(r#""provider":"webosiptv"},"streamQualityInfo""#));
    }

    /// A video-only HEVC stream (a Dolby Vision test pattern has no audio) takes the static
    /// payload, which must still carry the Dolby node: without it the TV engages HDR10 only.
    #[test]
    fn a_video_only_hevc_load_still_declares_dolby_vision() {
        let dv = p8().presentation(true, tv_platform::devcaps::dv::DvCapability::Supported, true);
        let out = super::video_only_payload(super::LoadVideo::H265, dv);
        assert!(out.contains(r#""DolbyHdrInfo":{"trackType":"single","encryptionType":"clear","profileId":8}"#), "{out}");
        assert_eq!(super::video_only_payload(super::LoadVideo::H264, dv), PAYLOAD_V, "H.264 is never Dolby Vision");
    }

    /// **What we actually send for a Profile 5 direct play.** The three fields at the path the
    /// television's own parser reads, `profileId` as a bare JSON integer (`getInt` - a quoted "5"
    /// would leave the pipeline's -1 sentinel), and the whole node inside `contents`.
    #[test]
    fn a_declared_profile_5_splices_the_node_into_contents() {
        // what `build_av_payload` hands it: the AV template with the codec already set to H265,
        // which is what a native HEVC direct play - the only kind that can be Dolby Vision - sends
        let base = PAYLOAD_AV.replace(r#""video":"H264""#, r#""video":"H265""#);
        let out = with_dolby_hdr_info(&base, "H265", p5().presentation(true, tv_platform::devcaps::dv::DvCapability::Supported, true));
        assert!(
            out.contains(r#""provider":"webosiptv","DolbyHdrInfo":{"trackType":"single","encryptionType":"clear","profileId":5}}"#),
            "{out}"
        );
        // the trailing `}` above is `contents` closing: the node is the last key INSIDE it, not a
        // sibling of `contents` in `externalStreamingInfo`
        assert!(out.contains(r#""DolbyHdrInfo":{"trackType":"single","encryptionType":"clear","profileId":5}},"streamQualityInfo""#));
        assert!(
            !out.contains(r#""profileId":"5""#),
            "getInt wants an integer, not a string"
        );
        // the codec string stays `H265` - `getVideoCaps` maps it to `video/x-h265` and falls
        // THROUGH into its Dolby Vision tail; there is no DVHE/DVH1 entry in that table, and
        // inventing one would describe a stream the pipeline has no decoder row for
        assert!(out.contains(r#""video":"H265""#), "{out}");
        assert!(!out.contains("DVHE") && !out.contains("dvh1"), "{out}");
        // and NOTHING else moved: take the node back out and the payload is what came in
        const NODE: &str =
            r#","DolbyHdrInfo":{"trackType":"single","encryptionType":"clear","profileId":5}"#;
        assert_eq!(out.replace(NODE, ""), base);
    }

    /// The three ways the node is NOT sent, each of which must leave the payload byte-identical:
    /// a file with no Dolby Vision, the dual-layer P7 played by default as its HDR10 base layer
    /// (`DvPresentation::BaseLayer`), and the disarmed trigger, which is what a
    /// `RELEASE=1` build compiles in and what every boot without `/tmp/webosiptv-dv` does today.
    #[test]
    fn nothing_is_spliced_unless_the_stream_is_declared() {
        let p7 = Dovi {
            present: true,
            profile: 7,
            bl_compat: 6,
            el_present: true,
            ..Dovi::NONE
        };
        for dv in [
            Dovi::NONE.presentation(true, tv_platform::devcaps::dv::DvCapability::Supported, true),
            p7.presentation(true, tv_platform::devcaps::dv::DvCapability::Supported, true),
            p5().presentation(false, tv_platform::devcaps::dv::DvCapability::Supported, true),
        ] {
            assert_eq!(with_dolby_hdr_info(PAYLOAD_AV, "H265", dv), PAYLOAD_AV);
        }
    }

    /// Profile 7: the base layer, declared as plain HEVC (no node), while a native Profile 8
    /// declares `profileId` 8 in the single-track node; and the Atmos node rides only an E-AC-3
    /// JOC declaration.
    #[test]
    fn profile_7_declares_no_dolby_vision_and_atmos_only_for_joc() {
        use tv_platform::devcaps::dv::DvCapability::Supported;
        let p7 = Dovi { present: true, profile: 7, bl_compat: 6, el_present: true };
        let decl = StreamDeclaration { vcodec: "hevc".into(), acodec: "eac3".into(), fps: 23.976, dovi: p7, atmos: true, ..StreamDeclaration::default() };
        let hdr10 = build_av_payload(&decl, p7.presentation(true, Supported, true), "H265", "AC3 PLUS", UHD);
        assert!(!hdr10.contains("DolbyHdrInfo"), "{hdr10}");
        assert!(hdr10.contains(r#""immersive":"ATMOS""#));
        let p8 = Dovi { present: true, profile: 8, bl_compat: 1, el_present: false };
        let real_p8 = build_av_payload(&StreamDeclaration { dovi: p8, ..decl.clone() }, p8.presentation(true, Supported, true), "H265", "AC3 PLUS", UHD);
        assert!(real_p8.contains(r#""DolbyHdrInfo":{"trackType":"single","encryptionType":"clear","profileId":8}"#), "{real_p8}");
        let plain = StreamDeclaration { atmos: false, ..decl.clone() };
        assert!(!build_av_payload(&plain, DvPresentation::BaseLayer, "H265", "AC3 PLUS", UHD).contains("immersive"));
    }

    /// **What we actually send for a Dolby Atmos track**, at the key path `libpf` reads
    /// (`option.externalStreamingInfo.contents.immersive`) and with the value `libcbe` - the
    /// television's own working client - puts there. Same anchor and same shape as the Dolby
    /// Vision node, which is the point: they are one envelope with two statements in it.
    #[test]
    fn an_atmos_track_splices_immersive_into_contents() {
        let out = with_immersive(PAYLOAD_AV, "AC3 PLUS", true);
        assert!(
            out.contains(r#""provider":"webosiptv","immersive":"ATMOS"}"#),
            "{out}"
        );
        // the trailing `}` is `contents` closing - the node is INSIDE it, not a sibling of
        // `contents` in `externalStreamingInfo`, which is where libpf would never look
        assert!(
            out.contains(r#""immersive":"ATMOS"},"streamQualityInfo""#),
            "{out}"
        );
        // and nothing else moved
        assert_eq!(out.replace(r#","immersive":"ATMOS""#, ""), PAYLOAD_AV);
    }

    /// The other side of it, and the one that matters for the whole library: a track with no
    /// Atmos leaves the payload byte-identical. Every ordinary AAC/AC3 film takes this path, so a
    /// splice that fired unconditionally would tell the television that all of them are immersive.
    #[test]
    fn a_plain_track_splices_nothing() {
        assert_eq!(with_immersive(PAYLOAD_AV, "AC3 PLUS", false), PAYLOAD_AV);
    }

    /// An Atmos flag on a declaration whose fed track is not E-AC3 must still be rejected at the
    /// final payload seam. AAC and ordinary AC3 carry no E-AC3 JOC extension; telling the system
    /// player either is immersive is a false stream declaration.
    #[test]
    fn an_atmos_declaration_never_rides_a_non_eac3_payload() {
        for audio in ["AAC", "AC3"] {
            assert_eq!(
                with_immersive(PAYLOAD_AV, audio, true),
                PAYLOAD_AV,
                "{audio}"
            );
        }
    }

    /// **Both nodes at once**, which is the real case - the Profile 5 test item is Dolby Vision
    /// AND Dolby Atmos, and it is what the television shows two read-outs for. They are spliced by
    /// two independent functions at ONE anchor, so this is the test that says the second does not
    /// land inside the first: `immersive` must be a sibling KEY of `DolbyHdrInfo` inside
    /// `contents`, never a fourth field of the `DolbyHdrInfo` object.
    #[test]
    fn dolby_vision_and_atmos_are_siblings_inside_contents() {
        let base = PAYLOAD_AV.replace(r#""video":"H264""#, r#""video":"H265""#);
        let out = with_immersive(
            &with_dolby_hdr_info(&base, "H265", p5().presentation(true, tv_platform::devcaps::dv::DvCapability::Supported, true)),
            "AC3 PLUS",
            true,
        );
        assert!(
            out.contains(
                r#""provider":"webosiptv","immersive":"ATMOS","DolbyHdrInfo":{"trackType":"single","encryptionType":"clear","profileId":5}}"#
            ),
            "{out}"
        );
        // the DolbyHdrInfo object still has exactly its three fields - `immersive` did not get
        // swept inside it by an anchor both functions matched
        assert!(!out.contains(r#""profileId":5,"immersive""#), "{out}");
        assert_eq!(out.matches(r#""immersive":"ATMOS""#).count(), 1);
        assert_eq!(out.matches("DolbyHdrInfo").count(), 1);
    }

    /// The consistency guard: a Dolby Vision declaration only ever rides an HEVC elementary
    /// stream, so a payload built for H264 must not carry one. Unreachable by construction - the
    /// route records the DV record on the direct-play branch, whose codec is the file's own hevc -
    /// which is exactly why it is asserted rather than trusted: the `sourceInfo` envelope is
    /// parsed before anything decodes, and a malformed one wedges the sink instead of failing.
    #[test]
    fn a_declaration_never_rides_a_non_hevc_payload() {
        assert_eq!(
            with_dolby_hdr_info(PAYLOAD_AV, "H264", p5().presentation(true, tv_platform::devcaps::dv::DvCapability::Supported, true)),
            PAYLOAD_AV
        );
    }
}

/// The `stream:` line names the origin and the path's length in segments only: an IPTV stream
/// carries its credentials anywhere in the path as often as in the query.
#[cfg(test)]
mod stream_open_log_line_tests {
    use super::stream_open_log_line;

    #[test]
    fn no_path_segment_is_logged() {
        let line = stream_open_log_line("http://tv.example:8080", "/live/someuser/somepass/1234.ts?x=1");
        assert_eq!(line, "stream: http://tv.example:8080 path=<4 segments>");
        assert_eq!(stream_open_log_line("http://tv.example:80", "/play/AbCdEf0123456789"), "stream: http://tv.example:80 path=<2 segments>");
        assert_eq!(stream_open_log_line("http://tv.example:80", "/t0kenT0ken123"), "stream: http://tv.example:80 path=<1 segment>");
        assert_eq!(stream_open_log_line("http://tv.example:80", "/"), "stream: http://tv.example:80 path=<0 segments>");
    }
}

/// **The 94-second freeze of 2026-08-29, reduced to the mechanism that caused it.**
///
/// A viewer on a remote 4K Dolby Vision stream reloaded onto a source that delivered its media in
/// 2-second bursts, and the picture then did not start for 94 seconds. The event log shows the reason plainly once you know where to look:
/// `primed: v=1999ms a=1962ms -> Play` was eventually printed with EXACTLY the fed extents of the
/// burst that had landed 94 seconds earlier - so the condition Play waits on had been satisfied
/// the whole time and was simply never EVALUATED.
///
/// The evaluation lived in one place only: inside [`feed_stream`]'s per-AU loop, after each
/// `sf_feed` - accepted or not, which was a second bug in the same lines. A pump tick runs the video lane and then the audio lane
/// ([`super::pump::pump`]), so when a whole 2-second burst lands at once the video lane drains all of
/// it with `abuf` still at zero - `vbuf` tops out below [`PRIME_VIDEO_MAX_NS`] and the
/// `abuf >= PRIME_AUDIO_NS` arm cannot be true yet. The audio lane then fills, but it deliberately
/// carries no prime check. On every later tick `aq_pop` returns null and the loop breaks BEFORE
/// the check is reached. Play is now waiting for a video AU that only the next burst can supply.
///
/// That is why a trickling source is unaffected: it delivers AUs over many ticks, so audio keeps
/// pace and the check passes on an ordinary tick. A burst arriving whole is the shape that breaks
/// it - and since [`PRIME_VIDEO_MAX_NS`] is 2500 ms, a 2-second burst can never take the
/// video-only escape, which makes this structural rather than a rare race.
///
/// **This test drives the real [`feed_stream`] and [`feed_audio_lane`] against the real host feed
/// seam** - it is not a restatement of the predicate, which is correct and would pass on the
/// broken build. What it grades is WHERE the predicate is evaluated.
#[cfg(all(test, feature = "hostsim"))]
mod prime_livelock_tests {
    use super::*;

    /// One 2-second burst, as the device delivered it: 48 video AUs at 24 fps and 93 AAC frames -
    /// the counts the failing run recorded.
    const VIDEO_AUS: i64 = 48;
    const AUDIO_AUS: i64 = 93;
    const V_STEP_NS: i64 = 41_666_666; // 24 fps
    const A_STEP_NS: i64 = 21_333_333; // 1024 samples at 48 kHz

    /// An engine in the state a fresh `Load` leaves behind: streaming, nothing fed yet, and
    /// `prime_play` armed so the next sufficient buffer starts the clock.
    pub(super) fn engine_after_reload() -> Engine {
        SHARED.ensure_initial_clock_hold_for_test();
        Engine {
            native_epoch: 0,
            stage: Stage::Streaming,
            video_info_sent: true,
            bound_at_frames: None,
            placed_src: (0, 0),
            eos_pushed: false,
            eos_stall: None,
            rebase_pending: false,
            rebase_drops: 0,
            seek_armed_at: 0,
            seek_retries: 0,
                flushed: false,
            presentation_rearm_pending: false,
            max_fed_video_pts: 0,
            max_fed_audio_pts: 0,
            audio_start_fp: None,
            feed_pace: FeedPace::default(),
            video_frame_ns: 0,
            seek_base_pts: 0,
            prime_play: true,
            aq_video: Some(crate::aq::aq_new(AQ_VIDEO_BYTES)),
            aq_audio: Some(crate::aq::aq_new(AQ_AUDIO_BYTES)),
            hs: tv_net::stream::http_stream_boxed(),
            pending_video: None,
            pending_audio: None,
            payload: std::ffi::CString::new("").unwrap(),
            source: Source::Stream,
            stream_th: None,
            load_th: None,
            report_th: None,
            report_stop: None,
            preview_abandon: false,
        }
    }

    /// Push one whole segment into both lanes before any tick runs - which is what a segmented
    /// demux does, and the entire difference from direct play.
    fn push_one_segment(eng: &mut Engine) {
        push_segment_at(eng, 0);
    }

    fn push_segment_at(eng: &mut Engine, base_ns: i64) {
        let au = [0u8; 64];
        let qv = &mut **eng.aq_video.as_mut().unwrap() as *mut crate::aq::AuQueue;
        for i in 0..VIDEO_AUS {
            crate::aq::aq_push(
                qv,
                au.as_ptr(),
                au.len() as c_int,
                base_ns + i * V_STEP_NS,
                c_int::from(i == 0),
                1,
            );
        }
        let qa = &mut **eng.aq_audio.as_mut().unwrap() as *mut crate::aq::AuQueue;
        for i in 0..AUDIO_AUS {
            crate::aq::aq_push(
                qa,
                au.as_ptr(),
                au.len() as c_int,
                base_ns + i * A_STEP_NS,
                1,
                2,
            );
        }
    }

    /// One pump tick - the real one. [`super::pump::pump`] calls exactly this.
    ///
    /// **This comment used to claim the test therefore "cannot drift from the shipped ordering",
    /// and that was false**: the test calls the wrapper, so a `pump` reverted to two separate lane
    /// calls would have restored the bug with this test still green. What actually enforces it is
    /// that [`feed_stream`] and [`feed_audio_lane`] are private - the revert no longer compiles.
    /// The test pins the BEHAVIOUR; the visibility pins the dispatch.
    fn tick(mt: &tv_base::task::MainThread, eng: &mut Engine) {
        feed_both_lanes(mt, eng);
    }

    #[test]
    fn a_segment_that_lands_whole_still_starts_the_picture() {
        let _serial = tv_base::testlock::serial();
        // Arm the host clock sink so `sf_feed` accepts AUs. See `ffi_host::FORCE_ENABLED` for why
        // this is an explicit override and not the `webosiptv-clocksink` trigger file. RESTORED on
        // the way out, panic or not: it is a process-global, and leaving it armed would silently
        // change what every later test's `sf_feed` returns.
        struct DisarmSink;
        impl Drop for DisarmSink {
            fn drop(&mut self) {
                crate::player::ffi_host::force_clocksink_for_test(false);
            }
        }
        crate::player::ffi_host::force_clocksink_for_test(true);
        let _disarm = DisarmSink;

        let mt = unsafe { tv_base::task::MainThread::assume() };
        SHARED.pres_fed.store(PRES_NONE, Ordering::Relaxed);
        SHARED.seek_to_ns.store(-1, Ordering::Relaxed);

        let mut eng = engine_after_reload();
        push_one_segment(&mut eng);

        // Tick 1: the segment is there in full. Tick 2: the next segment has not arrived, so both
        // queues are empty - the state the device sat in for 94 seconds.
        tick(&mt, &mut eng);
        tick(&mt, &mut eng);

        let vbuf = eng.max_fed_video_pts - eng.seek_base_pts;
        let abuf = eng.max_fed_audio_pts - eng.seek_base_pts;

        // **Precondition, asserted rather than assumed.** If the host feed seam refused the AUs
        // then nothing was fed, and a `prime_play` that is still armed would mean "the sink was
        // not armed", not "the bug". Distinguishing those is the whole lesson of this project's
        // silent-instrument traps, so it gets its own message.
        assert!(
            vbuf > 0 && abuf > 0,
            "PRECONDITION FAILED, not the defect: the host feed seam accepted nothing \
             (v={vbuf}ns a={abuf}ns). The clock sink is not armed, so this test graded nothing."
        );
        assert!(
            vbuf >= PRIME_NS,
            "video lane should hold a whole segment, got {vbuf}ns"
        );
        assert!(
            abuf >= PRIME_AUDIO_NS,
            "audio lane should hold a whole segment, got {abuf}ns"
        );

        assert!(
            !eng.prime_play,
            "the picture never started: both lanes hold enough to play \
             (v={}ms >= {}ms, a={}ms >= {}ms) but Play was never called. \
             The prime check is only reachable from inside feed_stream's per-AU loop, and by the \
             time the audio lane has caught up the video queue is empty, so the check is never \
             evaluated again. This is the 94-second freeze.",
            vbuf / 1_000_000,
            PRIME_NS / 1_000_000,
            abuf / 1_000_000,
            PRIME_AUDIO_NS / 1_000_000,
        );
    }

    /// Arm the host clock sink and a per-read feed-clock cost; restored on drop (both are
    /// process-globals).
    struct SlowFeed;
    impl SlowFeed {
        fn arm(step_us: u64) -> Self {
            crate::player::ffi_host::force_clocksink_for_test(true);
            FEED_TEST_CLOCK_STEP_US.with(|s| s.set(step_us));
            SlowFeed
        }
    }
    impl Drop for SlowFeed {
        fn drop(&mut self) {
            FEED_TEST_CLOCK_STEP_US.with(|s| s.set(0));
            crate::player::ffi_host::force_clocksink_for_test(false);
        }
    }

    /// Ticks a freshly reloaded engine takes to start the clock on one whole segment.
    fn ticks_to_prime(step_us: u64) -> (u32, Engine) {
        let _slow = SlowFeed::arm(step_us);
        let mt = unsafe { tv_base::task::MainThread::assume() };
        SHARED.pres_fed.store(PRES_NONE, Ordering::Relaxed);
        SHARED.seek_to_ns.store(-1, Ordering::Relaxed);
        let mut eng = engine_after_reload();
        push_one_segment(&mut eng);
        let mut ticks = 0;
        while eng.prime_play && ticks < 100 {
            tick(&mt, &mut eng);
            ticks += 1;
        }
        (ticks, eng)
    }

    /// **A slow `Feed()` does not stretch the prime.** With each feed-clock read costing 4 ms
    /// (past [`FEED_LANE_SLICE_US`]) a sliced lane feeds one AU a tick, and the prime's 17 video
    /// and ~15 audio AUs took that many ticks to first frame. A lane short of its prime depth is
    /// exempt, so the prime takes the same ticks as a greedy feed, and still on BOTH lanes.
    #[test]
    fn a_slow_feed_does_not_stretch_the_prime() {
        let _serial = tv_base::testlock::serial();
        let (greedy, _) = ticks_to_prime(0);
        let (slow, eng) = ticks_to_prime(4_000);
        assert!(!eng.prime_play, "the prime never completed in {slow} ticks");
        assert!(
            slow <= greedy + 1,
            "a 4 ms Feed() took {slow} ticks to prime; greedy feeding takes {greedy}"
        );
        assert!(
            eng.max_fed_video_pts - eng.seek_base_pts >= PRIME_NS
                && eng.max_fed_audio_pts - eng.seek_base_pts >= PRIME_AUDIO_NS,
            "Play started without both lanes primed"
        );
    }

    /// **Once a lane holds its prime depth, the rest of the backlog is fed a slice per lane per
    /// tick, not all in one frame.** On the TV every `Feed()` blocks the frame thread, and the
    /// first tick of a playback fed 62 video and 120 audio AUs at once (an 89 ms frame). Each
    /// feed-clock read costs 1 ms here, so a lane above low-water may feed only up to
    /// [`FEED_LANE_SLICE_US`] worth per tick: the first tick primes (exempt, to the prime depth)
    /// and the clock starts on both lanes, then the leftover backlog drains a slice a tick.
    #[test]
    fn a_post_prime_backlog_is_fed_a_slice_per_lane_per_tick() {
        let _serial = tv_base::testlock::serial();
        let _slow = SlowFeed::arm(1_000);
        let mt = unsafe { tv_base::task::MainThread::assume() };
        SHARED.pres_fed.store(PRES_NONE, Ordering::Relaxed);
        SHARED.seek_to_ns.store(-1, Ordering::Relaxed);

        let mut eng = engine_after_reload();
        push_one_segment(&mut eng);
        let per_lane_max = (FEED_LANE_SLICE_US / 1_000) as i64 + 1;

        tick(&mt, &mut eng);
        assert!(!eng.prime_play, "the first tick primes: both lanes reach their depth");
        let vbuf = eng.max_fed_video_pts - eng.seek_base_pts;
        let abuf = eng.max_fed_audio_pts - eng.seek_base_pts;
        assert!(
            vbuf >= PRIME_NS && abuf >= PRIME_AUDIO_NS,
            "Play started without both lanes primed (v={}ms a={}ms)",
            vbuf / 1_000_000,
            abuf / 1_000_000,
        );
        let v_after_prime = (vbuf + V_STEP_NS - 1) / V_STEP_NS;
        assert!(
            v_after_prime <= PRIME_NS / V_STEP_NS + per_lane_max,
            "the video lane fed {v_after_prime} AUs: the slice must apply past the prime depth"
        );

        // The presented position sits at the base, so the lead (>= the prime depth) is healthy.
        SHARED.pres_fed.store(0, Ordering::Relaxed);
        let (v0, a0) = (eng.max_fed_video_pts, eng.max_fed_audio_pts);
        tick(&mt, &mut eng);
        let v_fed = (eng.max_fed_video_pts - v0) / V_STEP_NS;
        let a_fed = (eng.max_fed_audio_pts - a0) / A_STEP_NS;
        assert!(
            eng.max_fed_video_pts > v0 && eng.max_fed_audio_pts > a0,
            "both lanes must keep draining the backlog"
        );
        assert!(
            v_fed <= per_lane_max && a_fed <= per_lane_max,
            "one tick fed {v_fed} video and {a_fed} audio AUs; the slice allows {per_lane_max} per lane",
        );
        SHARED.pres_fed.store(PRES_NONE, Ordering::Relaxed);
    }

    /// **The audio sync offset shifts every AU the audio lane feeds**: the AU that opens the
    /// timeline (PTS 0) is fed at the offset, later and earlier alike, clamped to the range.
    #[test]
    fn the_audio_sync_offset_shifts_the_fed_audio_pts() {
        let _serial = tv_base::testlock::serial();
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::player::set_audio_offset(0);
                SHARED.first_audio_pts.store(i64::MIN, Ordering::Relaxed);
                SHARED.pres_fed.store(PRES_NONE, Ordering::Relaxed);
            }
        }
        let _reset = Reset;
        let _slow = SlowFeed::arm(0);
        let mt = unsafe { tv_base::task::MainThread::assume() };
        assert_eq!(crate::player::clamp_audio_offset_ms(9_000), crate::player::AUDIO_OFFSET_RANGE_MS);
        assert_eq!(crate::player::clamp_audio_offset_ms(-9_000), -crate::player::AUDIO_OFFSET_RANGE_MS);
        for offset in [0, 1_500, -2_000] {
            SHARED.pres_fed.store(PRES_NONE, Ordering::Relaxed);
            SHARED.seek_to_ns.store(-1, Ordering::Relaxed);
            SHARED.first_audio_pts.store(5_000_000_000, Ordering::Relaxed);
            crate::player::set_audio_offset(offset);
            let mut eng = engine_after_reload();
            push_segment_at(&mut eng, 5_000_000_000);
            tick(&mt, &mut eng);
            assert_eq!(eng.audio_start_fp, Some(5_000_000_000 + offset * 1_000_000), "offset {offset} ms");
        }
    }

    /// **A playing lane below low-water is fed past its slice**: at 4 ms a `Feed()` the slice
    /// alone would feed one AU a tick, which at a 50 Hz present rate cannot keep 60 fps content's
    /// lead. Below [`FEED_LOW_WATER_NS`] the lane is topped up in one tick; above it, the slice.
    #[test]
    fn a_playing_lane_below_low_water_is_fed_past_its_slice() {
        let _serial = tv_base::testlock::serial();
        let _slow = SlowFeed::arm(4_000);
        let mt = unsafe { tv_base::task::MainThread::assume() };
        const PRES: i64 = 10_000_000_000;
        SHARED.pres_fed.store(PRES, Ordering::Relaxed);
        SHARED.seek_to_ns.store(-1, Ordering::Relaxed);

        let mut eng = engine_after_reload();
        eng.prime_play = false;
        eng.seek_base_pts = 0;
        let lead0 = 100_000_000;
        eng.max_fed_video_pts = PRES + lead0;
        eng.max_fed_audio_pts = PRES + lead0;
        push_segment_at(&mut eng, PRES + lead0 + 1);

        tick(&mt, &mut eng);
        let v_fed = (eng.max_fed_video_pts - (PRES + lead0)) / V_STEP_NS;
        let want = (FEED_LOW_WATER_NS - lead0 + V_STEP_NS - 1) / V_STEP_NS;
        assert_eq!(v_fed, want, "the video lane is topped up to low-water in one tick, not one AU");
        assert!(eng.max_fed_video_pts - PRES >= FEED_LOW_WATER_NS);

        // Above low-water the slice is back in force: one AU a tick at 4 ms a Feed().
        let v0 = eng.max_fed_video_pts;
        tick(&mt, &mut eng);
        assert_eq!((eng.max_fed_video_pts - v0) / V_STEP_NS, 1, "a healthy lane is sliced");
        SHARED.pres_fed.store(PRES_NONE, Ordering::Relaxed);
    }

    /// Reproduces the device sequence: Pause, let the demux queues fill, then Resume. The user
    /// intent may reopen feeding, but it must not run the native/ACB clock until Starfish has
    /// accepted a balanced decoder prime. The measured runway may extend into the validated queue
    /// tails because a paused native source buffer is finite and cannot accept the whole runway.
    #[test]
    fn pause_to_fill_resume_primes_both_lanes_before_native_play() {
        let _serial = tv_base::testlock::serial();
        struct Restore {
            video_tail: i64,
            audio_tail: i64,
            pts_shift: i64,
            presented: i64,
            paused: bool,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                SHARED
                    .video_tail_ns
                    .store(self.video_tail, Ordering::Relaxed);
                SHARED
                    .audio_tail_ns
                    .store(self.audio_tail, Ordering::Relaxed);
                SHARED.pts_shift.store(self.pts_shift, Ordering::Relaxed);
                SHARED.pres_fed.store(self.presented, Ordering::Relaxed);
                TX.commit_paused(self.paused);
                SHARED.reset_clock_for_test();
                crate::player::ffi_host::force_clocksink_for_test(false);
            }
        }

        crate::player::ffi_host::force_clocksink_for_test(true);
        SHARED.reset_clock_for_test();
        let base = 10_000_000_000;
        let _restore = Restore {
            video_tail: SHARED
                .video_tail_ns
                .swap(base + 3_500_000_000, Ordering::Relaxed),
            audio_tail: SHARED
                .audio_tail_ns
                .swap(base + 3_500_000_000, Ordering::Relaxed),
            pts_shift: SHARED.pts_shift.swap(0, Ordering::Relaxed),
            presented: SHARED.pres_fed.swap(base, Ordering::Relaxed),
            paused: TX.paused.swap(true, Ordering::Relaxed),
        };
        let pause = SHARED.prepare_user_pause().expect("reserve user Pause");
        let pause = match pause {
            super::super::shared::UserPause::Issue(token) => token,
            other => panic!("unexpected Pause reservation: {other:?}"),
        };
        assert_eq!(
            SHARED.complete_user_pause(pause, true),
            super::super::shared::PauseCompletion::Accepted
        );
        assert_eq!(
            SHARED.prepare_user_resume(true),
            Some(super::super::shared::UserResume::Prime)
        );

        let mt = unsafe { tv_base::task::MainThread::assume() };
        let mut eng = engine_after_reload();
        eng.prime_play = false;
        arm_live_clock_prime(&mut eng);
        assert_eq!(eng.seek_base_pts, base);
        push_segment_at(&mut eng, base);
        let before = crate::player::ffi_host::play_calls_for_test();
        try_prime(&mt, &mut eng);
        assert_eq!(
            crate::player::ffi_host::play_calls_for_test(),
            before,
            "Resume intent itself issued Play before feeding reopened",
        );

        TX.commit_paused(false);
        tick(&mt, &mut eng);

        let accepted_video = eng.max_fed_video_pts - base;
        let accepted_audio = eng.max_fed_audio_pts - base;
        assert!(accepted_video >= PRIME_NS && accepted_audio >= PRIME_AUDIO_NS);
        assert!(
            accepted_video < 3_000_000_000 && accepted_audio < 3_000_000_000,
            "the fixture must leave part of the stream in the application queues",
        );
        assert_eq!(crate::player::ffi_host::play_calls_for_test(), before + 1);
        assert!(
            !eng.prime_play,
            "balanced accepted A/V did not release ResumePrime",
        );
    }

    /// **An HLS VOD (a catch-up programme) reads in playlist time.** The demuxer opened the
    /// playlist at the segment that starts 42 s in, whose MPEG-TS timestamps start 10 min into
    /// the recording: the rebase maps that keyframe to 42 s, so a position report (the
    /// `sf_on_event` formula) and every later seek or reload target are programme time.
    #[test]
    fn an_hls_vod_keyframe_reads_as_its_playlist_time() {
        let _serial = tv_base::testlock::serial();
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                crate::player::ffi_host::force_clocksink_for_test(false);
                SHARED.hls_vod_base_ns.store(-1, Ordering::Relaxed);
                SHARED.pts_shift.store(0, Ordering::Relaxed);
                SHARED.disp_base.store(0, Ordering::Relaxed);
            }
        }
        crate::player::ffi_host::force_clocksink_for_test(true);
        let _restore = Restore;
        let mt = unsafe { tv_base::task::MainThread::assume() };
        SHARED.pres_fed.store(PRES_NONE, Ordering::Relaxed);
        SHARED.seek_to_ns.store(-1, Ordering::Relaxed);
        SHARED.hls_vod_base_ns.store(42_000_000_000, Ordering::Release);

        let mut eng = engine_after_reload();
        eng.rebase_pending = true;
        push_segment_at(&mut eng, 600_000_000_000);
        tick(&mt, &mut eng);

        assert!(!eng.rebase_pending, "the keyframe rebased");
        let shift = SHARED.pts_shift.load(Ordering::Relaxed);
        let base = SHARED.disp_base.load(Ordering::Relaxed);
        assert_eq!(shift, -600_000_000_000, "fed from 0");
        let playpos = |fed: i64| fed - shift + base;
        assert_eq!(playpos(0), 42_000_000_000, "the keyframe is the segment's playlist time");
        assert_eq!(playpos(1_500_000_000), 43_500_000_000);

        // Not an HLS VOD: positions stay the stream's own timestamps, as before.
        SHARED.hls_vod_base_ns.store(-1, Ordering::Release);
        SHARED.disp_base.store(0, Ordering::Relaxed);
        let mut live = engine_after_reload();
        live.rebase_pending = true;
        push_segment_at(&mut live, 600_000_000_000);
        tick(&mt, &mut live);
        assert_eq!(SHARED.disp_base.load(Ordering::Relaxed), 0);
    }

}

#[cfg(all(test, feature = "hostsim"))]
mod lifecycle_clock_tests {
    use super::*;

    #[test]
    fn stop_without_an_engine_cannot_poison_the_next_initial_hold() {
        let mut ps = crate::session::PlaybackSession::IDLE;
        let _serial = tv_base::testlock::serial();
        let mut pa = crate::player::adapter::PlayerAdapter::new(unsafe {
            tv_base::task::MainThread::assume()
        });
        assert!(
            !pa.is_live(),
            "test requires an empty native-session slot"
        );
        SHARED.reset_clock_for_test();

        stop_bufferfeed(&mut ps, &mut pa);

        assert!(
            SHARED.arm_initial_clock_hold(false, false),
            "a no-op stop left ClockState in Stopping"
        );
        SHARED.cancel_clock_session_start();
    }
}

/// Test-only: an Engine parked at `Stage::Streaming` on the host sink, for the pump-level rigs
/// that live outside this module (`pump::native_seek_pump_tests`).
#[cfg(all(test, feature = "hostsim"))]
pub(super) fn streaming_engine_for_test(native_epoch: u32) -> Engine {
    let mut eng = prime_livelock_tests::engine_after_reload();
    eng.native_epoch = native_epoch;
    eng
}

/// **BACK's frame does not pay for `Unload`** (`PendingRelease`). On the dev set
/// `StarfishMediaAPIs::Unload` held the frame that handled BACK for 113-150 ms on every return
/// (`teardown spans: ... sf_unload:135.7`). The host sink is given a slow Unload of its own
/// (`delay_unload_for_test`) and the contract is graded where the device's cannot be: the stop
/// returns long before the Unload does, nothing the stopped object says reaches the state the next
/// session owns, no native session can begin while the object is still being released, and a start
/// issued meanwhile waits for the release instead of racing it - and the release still ends in a
/// D1 with complete evidence, not a quarantine.
#[cfg(all(test, feature = "hostsim"))]
mod off_frame_release_tests {
    use super::*;

    const UNLOAD_MS: u64 = 400;

    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            crate::player::ffi_host::reset_native_lifecycle_for_test();
            crate::player::ffi_host::force_clocksink_for_test(false);
            SHARED.test_force_native_idle();
            SHARED.reset_session();
        }
    }

    fn ready_now() -> c_int {
        unsafe { crate::player::sink().ready(&MainThread::assume()) }
    }

    /// A live Engine over a ready host object whose Load has returned: what BACK tears down.
    fn playing_engine(pa: &mut crate::player::adapter::PlayerAdapter) -> u32 {
        let epoch = SHARED.begin_native_session().expect("native session");
        assert_eq!(unsafe { crate::player::sink().load(c"{}".as_ptr(), epoch) }, 1);
        assert!(SHARED.mark_native_load_returned(epoch).is_some());
        let mut eng = streaming_engine_for_test(epoch);
        eng.load_th = None;
        pa.install(eng);
        assert_ne!(
            ready_now(),
            0,
            "PRECONDITION FAILED: the object must be dispatchable, or the stop has nothing to Unload"
        );
        epoch
    }

    fn arm(_ps: &crate::session::PlaybackSession) -> crate::player::adapter::PlayerAdapter {
        SHARED.reset_session();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        crate::player::adapter::PlayerAdapter::new(unsafe { MainThread::assume() })
    }

    #[test]
    fn a_stop_returns_before_the_unload_and_the_next_start_waits_for_it() {
        let mut ps = crate::session::PlaybackSession::IDLE;
        let _serial = tv_base::testlock::serial();
        let mut pa = arm(&ps);
        let _cleanup = Cleanup;
        let epoch = playing_engine(&mut pa);
        crate::player::ffi_host::delay_unload_for_test(UNLOAD_MS);

        let started = std::time::Instant::now();
        stop_bufferfeed(&mut ps, &mut pa);
        let stop = started.elapsed();
        assert!(
            stop < std::time::Duration::from_millis(UNLOAD_MS / 4),
            "the stop held the frame for {stop:?} on a {UNLOAD_MS} ms Unload: BACK pays for it again"
        );
        assert!(!pa.is_live(), "the stop must still complete from the route's point of view");
        assert!(pa.has_pending_release(), "the object must be parked for its main-thread tail");

        // A callback the stopped object delivers now (a late error, say) is dropped: the session
        // state was already reset for whatever plays next.
        let before = SHARED.dg_cb_count.load(Ordering::Relaxed);
        super::super::sf_on_event(epoch, 5, 0, std::ptr::null());
        assert_eq!(
            SHARED.dg_cb_count.load(Ordering::Relaxed),
            before,
            "a callback from the object being released reached the next session's state"
        );
        assert!(
            SHARED.begin_native_session().is_none(),
            "a native session began while the previous object was still inside Unload"
        );
        assert!(!pa.poll_release(), "the per-frame poll must not wait for a running Unload");

        // A start now (no URL, so it ends NoRoute) finishes the release before anything else.
        let _ = start_bufferfeed_inner(&mut ps, &mut pa);
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(UNLOAD_MS),
            "the start went ahead while the Unload was still running"
        );
        assert!(!pa.has_pending_release(), "the start must leave no release pending");
        assert_eq!(ready_now(), 0, "the released object must be destroyed before a new one may load");
        assert!(
            !crate::player::ffi_host::lifecycle_blocked_for_test(),
            "the off-frame release must end in a D1 with complete evidence, not a quarantine"
        );
        let next = SHARED.begin_native_session().expect("native sessions resume after the release");
        assert!(SHARED.retire_native_session(next));
    }

    #[test]
    fn the_frame_poll_finishes_a_returned_unload_and_exit_waits_for_a_running_one() {
        let mut ps = crate::session::PlaybackSession::IDLE;
        let _serial = tv_base::testlock::serial();
        let mut pa = arm(&ps);
        let _cleanup = Cleanup;

        // The ordinary return: the Unload comes back while Home draws; a later frame's poll runs
        // the tail.
        playing_engine(&mut pa);
        crate::player::ffi_host::delay_unload_for_test(50);
        stop_bufferfeed(&mut ps, &mut pa);
        assert!(pa.has_pending_release());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !pa.poll_release() {
            assert!(std::time::Instant::now() < deadline, "the returned Unload was never finished");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!pa.has_pending_release());
        assert_eq!(ready_now(), 0);
        assert!(!crate::player::ffi_host::lifecycle_blocked_for_test(), "the poll's tail must D1");

        // The exit (`app::run::shutdown`): a release still running is waited for, not abandoned.
        playing_engine(&mut pa);
        crate::player::ffi_host::delay_unload_for_test(UNLOAD_MS);
        stop_bufferfeed(&mut ps, &mut pa);
        assert!(pa.has_pending_release());
        pa.finish_release();
        assert!(!pa.has_pending_release(), "finish_release must not return with the Unload running");
        assert_eq!(ready_now(), 0);
        assert!(!crate::player::ffi_host::lifecycle_blocked_for_test());
    }

    #[test]
    fn the_releases_own_unloadcompleted_does_not_count_into_the_next_sessions_diagnostics() {
        let mut ps = crate::session::PlaybackSession::IDLE;
        let _serial = tv_base::testlock::serial();
        let mut pa = arm(&ps);
        let _cleanup = Cleanup;
        playing_engine(&mut pa);
        crate::player::ffi_host::delay_unload_for_test(50);
        stop_bufferfeed(&mut ps, &mut pa);
        assert!(pa.has_pending_release());
        pa.finish_release();
        assert!(!crate::player::ffi_host::lifecycle_blocked_for_test(), "the release must still D1");
        assert_eq!(
            SHARED.dg_cb_count.load(Ordering::Relaxed),
            0,
            "the stopped object's UNLOADCOMPLETED (after reset_session) counted into the next session's callback census"
        );
        assert_eq!(SHARED.dg_cb_err_at.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn taking_the_token_or_splitting_waits_for_a_running_unload() {
        for use_split in [false, true] {
            let mut ps = crate::session::PlaybackSession::IDLE;
            let _serial = tv_base::testlock::serial();
            let mut pa = arm(&ps);
            let _cleanup = Cleanup;
            playing_engine(&mut pa);
            crate::player::ffi_host::delay_unload_for_test(UNLOAD_MS);
            let started = std::time::Instant::now();
            stop_bufferfeed(&mut ps, &mut pa);
            assert!(pa.has_pending_release());
            if use_split {
                let _ = pa.split();
            } else {
                let _ = pa.mt();
            }
            assert!(
                started.elapsed() >= std::time::Duration::from_millis(UNLOAD_MS),
                "{} returned while the Unload was still running",
                if use_split { "split()" } else { "mt()" }
            );
            assert!(!pa.has_pending_release(), "nothing may be pending once the token is handed out");
            assert_eq!(ready_now(), 0);
            assert!(!crate::player::ffi_host::lifecycle_blocked_for_test());
        }
    }

    #[test]
    fn a_refused_unload_thread_unloads_on_the_main_thread_and_still_d1s() {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                FAIL_UNLOAD_SPAWN_FOR_TEST.store(false, Ordering::Relaxed);
            }
        }
        let mut ps = crate::session::PlaybackSession::IDLE;
        let _serial = tv_base::testlock::serial();
        let mut pa = arm(&ps);
        let _cleanup = Cleanup;
        let _reset = Reset;
        playing_engine(&mut pa);
        crate::player::ffi_host::delay_unload_for_test(150);
        FAIL_UNLOAD_SPAWN_FOR_TEST.store(true, Ordering::Relaxed);
        let started = std::time::Instant::now();
        stop_bufferfeed(&mut ps, &mut pa);
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(150),
            "with no thread the Unload must have run synchronously inside the stop"
        );
        assert!(!pa.has_pending_release(), "no worker exists, so nothing may be parked");
        assert_eq!(ready_now(), 0, "the object must be destroyed");
        assert!(!crate::player::ffi_host::lifecycle_blocked_for_test(), "the fallback must end in a D1");
    }

    #[test]
    fn an_unloadcompleted_from_the_wrong_epoch_during_the_release_is_not_evidence() {
        let mut ps = crate::session::PlaybackSession::IDLE;
        let _serial = tv_base::testlock::serial();
        let mut pa = arm(&ps);
        let _cleanup = Cleanup;
        let epoch = playing_engine(&mut pa);
        crate::player::ffi_host::delay_unload_for_test(UNLOAD_MS);
        stop_bufferfeed(&mut ps, &mut pa);
        assert!(pa.has_pending_release());
        let before = SHARED.dg_cb_count.load(Ordering::Relaxed);
        super::super::sf_on_event(epoch + 1, 23, 0, std::ptr::null());
        super::super::sf_on_event(0, 23, 0, std::ptr::null());
        assert_eq!(SHARED.dg_cb_count.load(Ordering::Relaxed), before, "a wrong-epoch event was processed");
        assert!(
            !SHARED.native_unload_completed(epoch),
            "a wrong-epoch UNLOADCOMPLETED counted as the release's D1 evidence"
        );
        pa.finish_release();
        assert!(!crate::player::ffi_host::lifecycle_blocked_for_test());
    }
}

/// issue #74 D.1 regression tests. Real v0.6.0 dispatches every Starfish/ACB verb through
/// `sf_ready_object()` alone (`src/starfish.c`), which went live at `SMP_SET_READY(1)` BEFORE the
/// real, blocking `sf_load` call (`g_load_with_context`) returned - nothing waited for that
/// return. So the whole duration of a Load was a window in which `sf_is_load_completed`/
/// `sf_play`/`sf_feed`/etc. could dispatch into an object whose LoadCommon ctor path was still
/// running on the load thread: investigation.md's run A (a null-arg SIGSEGV on the priming path)
/// and run B (a Play-vs-LoadCommon mutex deadlock ending in an OMX_VideoComp abort).
#[cfg(all(test, feature = "hostsim"))]
mod load_in_flight_tests {
    use super::*;

    /// The Engine itself needs no cleanup - it lives in the test's own `PlayerAdapter`, which drops
    /// with the test. What is still process-wide is `SHARED` and the native lifecycle test seams.
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            crate::player::ffi_host::reset_native_lifecycle_for_test();
            crate::player::ffi_host::force_clocksink_for_test(false);
            crate::player::ffi_host::set_load_in_flight_for_test(false);
            SHARED.test_force_native_idle();
            SHARED.reset_session();
        }
    }

    /// An engine parked in `Stage::Loading`, native session installed, not yet primed. Mirrors
    /// [`prime_livelock_tests::engine_after_reload`] but for the state a fresh `Load` is in
    /// BEFORE it returns, rather than after.
    ///
    /// `rebase_pending: true` (a seek or resume armed for the first open) drives the pump's
    /// loadCompleted arm into its prime-before-Play branch, which is the code path under test.
    fn engine_loading(epoch: u32) -> Engine {
        let mut eng = super::prime_livelock_tests::engine_after_reload();
        eng.native_epoch = epoch;
        eng.stage = Stage::Loading;
        eng.rebase_pending = true;
        eng.prime_play = false;
        eng.max_fed_video_pts = 0;
        eng.max_fed_audio_pts = 0;
        eng
    }

    /// Waits (bounded - this is a HOST test, never the television) for the host seam's
    /// `LOAD_IN_FLIGHT` window to actually open. This project's silent-instrument rule
    /// (AGENTS.md): a seam that never entered the in-flight window would score zero dispatches
    /// and prove nothing, so the precondition is its own assertion with its own message, kept
    /// separate from the real assertion below.
    fn wait_for_load_in_flight() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !crate::player::ffi_host::load_in_flight_for_test() {
            assert!(
                std::time::Instant::now() < deadline,
                "PRECONDITION FAILED: sf_load never entered its in-flight window (loadCompleted \
                 emitted, not yet returned). A run that never opens this window would score zero \
                 seam dispatches and prove nothing about the gate — see AGENTS.md's \
                 silent-instrument rule."
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// The primary test: real concurrency, a real second thread running `threads::load_thread`,
    /// a real held `sf_load`.
    #[test]
    fn nothing_reaches_the_seam_while_load_is_still_in_flight() {
        let mut ps = crate::session::PlaybackSession::IDLE;
        let _serial = tv_base::testlock::serial();
        SHARED.reset_session();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        let _cleanup = Cleanup;

        let mut pa = crate::player::adapter::PlayerAdapter::new(unsafe {
            tv_base::task::MainThread::assume()
        });
        let epoch = SHARED.begin_native_session().expect("native session");
        pa.install(engine_loading(epoch));

        // Push a whole segment's worth onto both lanes now, the way a demuxer already running
        // ahead of Load would - enough to clear both
        // `PRIME_NS` (video) and `PRIME_AUDIO_NS` (audio), so the positive half below can reach a
        // real `sf_play` once the gate opens, not just an engine that primed and stalled.
        {
            const V_STEP_NS: i64 = 41_666_666; // 24 fps
            const A_STEP_NS: i64 = 21_333_333; // 1024 samples at 48 kHz
            const VIDEO_AUS: i64 = 30; // ~1.25s > PRIME_NS (700ms)
            const AUDIO_AUS: i64 = 20; // ~427ms > PRIME_AUDIO_NS (300ms)
            let eng = pa.engine().expect("engine installed above");
            let au = [0u8; 32];
            let qv = &mut **eng.aq_video.as_mut().unwrap() as *mut crate::aq::AuQueue;
            for i in 0..VIDEO_AUS {
                crate::aq::aq_push(
                    qv,
                    au.as_ptr(),
                    au.len() as c_int,
                    i * V_STEP_NS,
                    c_int::from(i == 0),
                    1,
                );
            }
            let qa = &mut **eng.aq_audio.as_mut().unwrap() as *mut crate::aq::AuQueue;
            for i in 0..AUDIO_AUS {
                crate::aq::aq_push(qa, au.as_ptr(), au.len() as c_int, i * A_STEP_NS, 1, 2);
            }
        }

        crate::player::ffi_host::hold_load_for_test();
        let payload = std::ffi::CString::new("{}").unwrap();
        let payload_ptr = threads::SendPtr(payload.as_ptr() as *mut c_char);
        let load_thread = std::thread::Builder::new()
            .name("test-load-thread".into())
            .spawn(move || threads::load_thread(payload_ptr, epoch))
            .expect("spawn load_thread");

        wait_for_load_in_flight();

        // The primary assertion: several real pump ticks, none of which may reach the seam.
        for _ in 0..5 {
            crate::player::pump::pump(&mut ps, &mut pa, 1_000);
        }
        assert_eq!(
            crate::player::ffi_host::in_flight_calls_for_test(), 0,
            "the pump reached the Starfish/ACB seam ({:?}) while StarfishMediaAPIs::Load was \
             still executing on the load thread. In issue #74 that window is where the app died: \
             a SIGSEGV in libc with a null first argument on the priming path, and a \
             Play-vs-LoadCommon deadlock ending in an OMX_VideoComp abort on the immediate-Play \
             path.",
            crate::player::ffi_host::in_flight_verb_for_test()
        );
        assert!(
            !SHARED.native_load_returned(epoch),
            "sanity: the Rust-side gate must still read closed while sf_load has not returned"
        );
        assert!(
            pa.engine().is_some_and(|e| e.stage == Stage::Loading),
            "the pump must not have advanced past Loading while Load was still in flight"
        );

        // Release the held Load and let it actually return.
        crate::player::ffi_host::release_load_for_test();
        load_thread.join().expect("load_thread joins");

        // The positive half: the gate isn't just permanently closed. A few more ticks must now
        // advance past Loading and reach Play - proving the fix doesn't just refuse forever.
        let mut advanced = false;
        for _ in 0..20 {
            crate::player::pump::pump(&mut ps, &mut pa, 1_000);
            if pa.engine().is_some_and(|e| e.stage != Stage::Loading) {
                advanced = true;
                break;
            }
        }
        assert!(
            SHARED.native_load_returned(epoch),
            "the Rust-side gate must open once threads::load_thread has actually returned"
        );
        assert!(
            advanced,
            "the gate opening must let the pump leave Stage::Loading"
        );
        assert!(
            crate::player::ffi_host::play_calls_for_test() > 0,
            "once Load has genuinely returned, Play must eventually be reached — the gate must \
             not simply stay closed forever"
        );
    }

    /// `load-budget-skipped-when-the-gate-can-never-open` (a fix-wave finding, not a P1a
    /// acceptance line): the D.1.4 budget below shares its precondition with the gate it exists
    /// to bound - both `native_load_elapsed` and `native_load_returned` require the epoch to
    /// still own `NativeSessionPhase::Active`. A synchronous UnloadCompleted callback for the
    /// live epoch (firmware type=23) flips the phase to `Unloaded` without `load_call` ever
    /// having reached `Returned`, which - before this fix - left `pump.rs`'s loadCompleted arm
    /// deferring forever with no deadline: exactly the permanent Connecting spinner D.1.4 exists
    /// to make impossible, on the one path where the gate can never open again.
    #[test]
    fn losing_the_active_phase_while_still_deferred_fires_the_budget_instead_of_hanging() {
        let mut ps = crate::session::PlaybackSession::IDLE;
        let _serial = tv_base::testlock::serial();
        SHARED.reset_session();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        let _cleanup = Cleanup;

        let mut pa = crate::player::adapter::PlayerAdapter::new(unsafe {
            tv_base::task::MainThread::assume()
        });
        let epoch = SHARED.begin_native_session().expect("native session");
        pa.install(engine_loading(epoch));

        // `sf_ready(mt)` must already read nonzero (SMP_READY true) BEFORE the loadCompleted arm
        // is even reached - pump.rs's own `sf_ready == 0 => Connecting` early return sits ahead
        // of it - so this needs the same real held-Load rig as
        // `nothing_reaches_the_seam_while_load_is_still_in_flight`, not a bare phase flip.
        crate::player::ffi_host::hold_load_for_test();
        let payload = std::ffi::CString::new("{}").unwrap();
        let payload_ptr = threads::SendPtr(payload.as_ptr() as *mut c_char);
        let load_thread = std::thread::Builder::new()
            .name("test-load-thread-2".into())
            .spawn(move || threads::load_thread(payload_ptr, epoch))
            .expect("spawn load_thread");
        wait_for_load_in_flight();

        assert!(
            !SHARED.native_load_returned(epoch),
            "sanity: the gate starts closed"
        );

        // Simulate the firmware's synchronous UnloadCompleted callback for this epoch while the
        // pump is still deferring loadCompleted (Load is genuinely still executing on the load
        // thread) - the phase leaves Active for Unloaded without `load_call` ever reaching
        // `Returned`.
        SHARED.with_native_session(
            epoch,
            crate::player::shared::NativeEventClass::UnloadCompleted,
            23,
            || {},
        );
        assert!(
            SHARED.native_load_elapsed(epoch).is_none(),
            "PRECONDITION FAILED: the epoch must have actually left Active, or this test proves \
             nothing about the hazard (AGENTS.md's silent-instrument rule)"
        );

        assert!(!SHARED.load_failed.load(std::sync::atomic::Ordering::Acquire));
        crate::player::pump::pump(&mut ps, &mut pa, 1_000);
        assert!(
            SHARED.load_failed.load(std::sync::atomic::Ordering::Acquire),
            "losing Active while loadCompleted was still deferred must fire the D.1.4 budget \
             immediately rather than silently disabling it — the permanent Connecting spinner \
             this exists to rule out"
        );

        // Unblock and join the held load thread so the process doesn't leak it into later tests.
        crate::player::ffi_host::release_load_for_test();
        load_thread.join().expect("load_thread joins");
    }

    /// `load-budget-expiry-path-itself-has-no-test` (a fix-wave finding): of this arm's three
    /// match arms, `losing_the_active_phase_while_still_deferred_fires_the_budget_instead_of_hanging`
    /// above pins the `None` arm, but `Some(elapsed) if elapsed >= NATIVE_LOAD_BUDGET` - the arm
    /// the D.1.4 acceptance criterion is actually about - was reachable only by waiting a real 20s,
    /// so nothing drove it: inverting the comparison to `elapsed < NATIVE_LOAD_BUDGET` left the
    /// host suite green. `Shared::test_backdate_native_load_issued` (test-only) rewinds the
    /// recorded `load_issued_at` past the budget instead, so the same real held-Load rig as
    /// `nothing_reaches_the_seam_while_load_is_still_in_flight` can drive the actual comparison
    /// without a sleep.
    #[test]
    fn native_load_budget_expiry_fires_load_failed() {
        let mut ps = crate::session::PlaybackSession::IDLE;
        let _serial = tv_base::testlock::serial();
        SHARED.reset_session();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        let _cleanup = Cleanup;

        let mut pa = crate::player::adapter::PlayerAdapter::new(unsafe {
            tv_base::task::MainThread::assume()
        });
        let epoch = SHARED.begin_native_session().expect("native session");
        pa.install(engine_loading(epoch));

        crate::player::ffi_host::hold_load_for_test();
        let payload = std::ffi::CString::new("{}").unwrap();
        let payload_ptr = threads::SendPtr(payload.as_ptr() as *mut c_char);
        let load_thread = std::thread::Builder::new()
            .name("test-load-thread-budget".into())
            .spawn(move || threads::load_thread(payload_ptr, epoch))
            .expect("spawn load_thread");
        wait_for_load_in_flight();

        assert!(
            SHARED.test_backdate_native_load_issued(epoch, crate::player::pump::NATIVE_LOAD_BUDGET),
            "PRECONDITION FAILED: the epoch must still own the Active phase to backdate its \
             load_issued_at, or this test proves nothing about the arm under test"
        );
        assert!(
            SHARED
                .native_load_elapsed(epoch)
                .is_some_and(|e| e >= crate::player::pump::NATIVE_LOAD_BUDGET),
            "PRECONDITION FAILED: the backdate must actually push elapsed past the budget — a \
             silent-instrument trap (AGENTS.md) if it doesn't"
        );
        assert!(
            !SHARED.load_failed.load(std::sync::atomic::Ordering::Acquire),
            "sanity: load_failed starts clear"
        );

        crate::player::pump::pump(&mut ps, &mut pa, 1_000);

        assert!(
            SHARED.load_failed.load(std::sync::atomic::Ordering::Acquire),
            "elapsed >= NATIVE_LOAD_BUDGET must publish load_failed — an inverted or otherwise \
             broken comparison here would leave a Load that never returns spinning forever \
             instead of reaching the failure read-out"
        );

        // Unblock and join the held load thread so the process doesn't leak it into later tests.
        crate::player::ffi_host::release_load_for_test();
        load_thread.join().expect("load_thread joins");
    }

    /// `load-returned-log-is-conditional-on-loadcompleted-and-unbounded-after`: the OTHER half of
    /// D.1.4's budget. `native_load_budget_expiry_fires_load_failed` above pins "Load never
    /// returns"; this pins "Load returns, but `loadCompleted` never arrives" - a wait that was
    /// completely unbounded before this fix, since `pump.rs`'s D.1.4 arm only runs while
    /// `!native_load_returned`, and stops being reachable the instant Load returns.
    ///
    /// Drives the gate by hand (like `the_gate_is_epoch_scoped` below): no real `sf_load` call is
    /// made at all - the host stub's `sf_load` unconditionally emits the `loadCompleted` callback
    /// before it can return, so it cannot represent "Load returned but loadCompleted never
    /// arrived". `force_object_ready_for_test` gives `sf_ready()` its `true` without that call,
    /// leaving `LOADED` (and so `sf_is_load_completed()`) at its default `false` - the "quiet
    /// pipeline" this fix is about.
    #[test]
    fn native_load_returned_loadcompleted_never_arrives_fires_load_failed() {
        let mut ps = crate::session::PlaybackSession::IDLE;
        let _serial = tv_base::testlock::serial();
        SHARED.reset_session();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        crate::player::ffi_host::force_object_ready_for_test(true);
        let _cleanup = Cleanup;

        let mut pa = crate::player::adapter::PlayerAdapter::new(unsafe {
            tv_base::task::MainThread::assume()
        });
        let epoch = SHARED.begin_native_session().expect("native session");
        pa.install(engine_loading(epoch));

        assert!(
            SHARED.mark_native_load_returned(epoch).is_some(),
            "PRECONDITION FAILED: Load must actually be marked Returned, or this test proves \
             nothing about the second-half budget"
        );
        assert!(
            SHARED.native_load_returned(epoch),
            "PRECONDITION FAILED: the gate must read Returned"
        );
        assert!(
            !SHARED.load_completed.load(std::sync::atomic::Ordering::Relaxed),
            "PRECONDITION FAILED: loadCompleted must genuinely never have arrived"
        );
        assert_ne!(
            unsafe { crate::player::sink().ready(pa.mt()) },
            0,
            "PRECONDITION FAILED: sf_ready() must read true (force_object_ready_for_test), or \
             pump() bails out at its own sf_ready wait before ever reaching the arm under test"
        );
        assert_eq!(
            unsafe { crate::player::sink().is_load_completed(pa.mt()) },
            0,
            "PRECONDITION FAILED: sf_is_load_completed() must read false — if it reads true, the \
             OTHER arm (loadCompleted arrived) fires instead and this test proves nothing about \
             the arm under test"
        );

        assert!(
            SHARED.test_backdate_native_load_returned(
                epoch,
                crate::player::pump::NATIVE_LOAD_BUDGET
            ),
            "PRECONDITION FAILED: the epoch must still own the Active phase to backdate when it \
             returned, or this test proves nothing about the arm under test"
        );
        assert!(
            SHARED
                .native_load_returned_elapsed(epoch)
                .is_some_and(|e| e >= crate::player::pump::NATIVE_LOAD_BUDGET),
            "PRECONDITION FAILED: the backdate must actually push the returned-elapsed past the \
             budget — a silent-instrument trap (AGENTS.md) if it doesn't"
        );
        assert!(
            !SHARED.load_failed.load(std::sync::atomic::Ordering::Acquire),
            "sanity: load_failed starts clear"
        );

        crate::player::pump::pump(&mut ps, &mut pa, 1_000);

        assert!(
            SHARED.load_failed.load(std::sync::atomic::Ordering::Acquire),
            "loadCompleted never arriving within NATIVE_LOAD_BUDGET of Load RETURNING must \
             publish load_failed — an unbounded wait here is exactly the permanent-Connecting- \
             spinner shape D.1.4 exists to rule out, on the half that was still open"
        );
    }

    /// A narrow SIMULATION of the epoch-scoping property, not a historical reproduction of the
    /// crash - it drives both sides of the gate by hand (no real second thread, no real held
    /// `sf_load`) to prove a mark for a DIFFERENT (superseded) epoch can never open the current
    /// one's gate. See `nothing_reaches_the_seam_while_load_is_still_in_flight` for the
    /// real-concurrency test that this one deliberately does not attempt to replace.
    #[test]
    fn the_gate_is_epoch_scoped() {
        let mut ps = crate::session::PlaybackSession::IDLE;
        let _serial = tv_base::testlock::serial();
        SHARED.reset_session();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        let _cleanup = Cleanup;

        let mut pa = crate::player::adapter::PlayerAdapter::new(unsafe {
            tv_base::task::MainThread::assume()
        });
        let epoch = SHARED.begin_native_session().expect("native session");
        pa.install(engine_loading(epoch));

        // Get the host seam's OBJECT_READY (so `sf_ready(mt)` reads true and the pump reaches
        // the loadCompleted arm at all) without arming HOLD_LOAD - this call returns at once.
        let payload = std::ffi::CString::new("{}").unwrap();
        assert_eq!(unsafe { crate::player::sink().load(payload.as_ptr(), epoch) }, 1);
        // Hand-set the in-flight window and publish loadCompleted via the existing sf_on_event
        // path - simulating "Load has not returned" without a real second thread.
        crate::player::ffi_host::set_load_in_flight_for_test(true);
        super::super::sf_on_event(epoch, 2, 0, c"{\"loadCompleted\":true}".as_ptr());

        crate::player::pump::pump(&mut ps, &mut pa, 1_000);
        assert_eq!(
            crate::player::ffi_host::in_flight_calls_for_test(), 0,
            "loadCompleted alone, with the Rust-side gate not yet marked Returned for this \
             epoch, must not let any seam verb dispatch ({:?})",
            crate::player::ffi_host::in_flight_verb_for_test()
        );
        assert!(
            pa.engine().is_some_and(|e| e.stage == Stage::Loading),
            "the gate must still be closed"
        );

        // Mark a DIFFERENT epoch as returned. Since it never owns the Active phase, this must be
        // a complete no-op for the session actually running.
        let other_epoch = epoch.wrapping_add(1).max(1);
        assert_ne!(other_epoch, epoch, "the test must pick a genuinely different epoch");
        assert!(
            SHARED.mark_native_load_returned(other_epoch).is_none(),
            "marking an epoch that does not own the Active phase must be a no-op"
        );
        assert!(
            !SHARED.native_load_returned(epoch),
            "a mark for a DIFFERENT epoch must never open THIS epoch's gate — this is the \
             epoch-scoping property that stops a late thread from a superseded session opening \
             the gate for a newer one"
        );

        crate::player::pump::pump(&mut ps, &mut pa, 1_000);
        assert_eq!(
            crate::player::ffi_host::in_flight_calls_for_test(), 0,
            "the pump must still defer after a differently-epoched mark ({:?})",
            crate::player::ffi_host::in_flight_verb_for_test()
        );
        assert!(
            pa.engine().is_some_and(|e| e.stage == Stage::Loading),
            "marking an epoch other than the pump's own must never open ITS gate"
        );

        crate::player::ffi_host::set_load_in_flight_for_test(false);
    }

    #[test]
    fn native_source_correction_ctor_wait_has_an_issued_budget() {
        let _serial = tv_base::testlock::serial();
        SHARED.reset_session();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        let _cleanup = Cleanup;
        let mut pa = super::super::adapter::PlayerAdapter::new(unsafe { tv_base::task::MainThread::assume() });
        let mut ps = crate::session::PlaybackSession::IDLE;
        let epoch = SHARED.begin_native_session().unwrap();
        pa.install(engine_loading(epoch));
        assert_eq!(unsafe { crate::player::sink().ready(pa.mt()) }, 0);
        crate::player::pump::pump(&mut ps, &mut pa, 1000);
        assert!(!SHARED.load_failed.load(Ordering::Acquire));
        assert!(SHARED.test_backdate_native_load_issued(epoch, crate::player::pump::NATIVE_LOAD_BUDGET));
        crate::player::pump::pump(&mut ps, &mut pa, 1001);
        assert!(SHARED.load_failed.load(Ordering::Acquire), "constructor readiness must not bypass the issued budget");
    }
    #[test]
    fn native_source_correction_stale_load_refusal_cannot_fail_new_epoch() {
        let _serial = tv_base::testlock::serial();
        SHARED.reset_session();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        let _cleanup = Cleanup;
        let mt = unsafe { tv_base::task::MainThread::assume() };
        unsafe { crate::player::sink().quarantine(&mt); }
        let old = SHARED.begin_native_session().unwrap();
        assert!(SHARED.retire_native_session(old));
        let current = SHARED.begin_native_session().unwrap();
        threads::load_thread(threads::SendPtr(c"{}".as_ptr() as *mut c_char), old);
        assert!(!SHARED.load_failed.load(Ordering::Acquire), "a stale worker must not poison the newer epoch");
        assert!(!SHARED.native_load_returned(current));
        threads::load_thread(threads::SendPtr(c"{}".as_ptr() as *mut c_char), current);
        assert!(SHARED.load_failed.load(Ordering::Acquire), "a current synchronous refusal must still fail");
    }

    #[test]
    fn native_ready_demux_failure_keeps_precedence_over_the_load_wait() {
        let _serial = tv_base::testlock::serial();
        SHARED.reset_session();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        crate::player::ffi_host::force_object_ready_for_test(true);
        let _cleanup = Cleanup;
        let mut pa = super::super::adapter::PlayerAdapter::new(unsafe { tv_base::task::MainThread::assume() });
        let mut ps = crate::session::PlaybackSession::IDLE;
        let epoch = SHARED.begin_native_session().unwrap();
        pa.install(engine_loading(epoch));
        crate::player::pump::pump(&mut ps, &mut pa, 1000);
        assert_eq!(crate::player::state(&ps), crate::player::PlaybackState::Connecting);
        SHARED.demux_failed.store(true, Ordering::Release);
        crate::player::pump::pump(&mut ps, &mut pa, 1001);
        assert_eq!(crate::player::state(&ps), crate::player::PlaybackState::Error);
    }

    /// PR #105 review finding: the D.1.4 budget turns a `Load` that never returns into a
    /// failure read-out, but BACK from that read-out ran `teardown`, which JOINED the media
    /// thread on the SDL main thread before anything else - so a genuinely hung `sf_load` froze
    /// the whole app the moment the user tried to leave the screen that told them about it.
    ///
    /// The held host `sf_load` is the hang. A watchdog releases it after `WATCHDOG` so a broken
    /// teardown FAILS this test (elapsed ≈ WATCHDOG) instead of hanging the suite.
    #[test]
    fn teardown_after_a_load_timeout_does_not_block_on_the_hung_load() {
        const WATCHDOG: std::time::Duration = std::time::Duration::from_secs(3);
        let mut ps = crate::session::PlaybackSession::IDLE;
        let _serial = tv_base::testlock::serial();
        SHARED.reset_session();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        let _cleanup = Cleanup;

        let mut pa = crate::player::adapter::PlayerAdapter::new(unsafe {
            tv_base::task::MainThread::assume()
        });
        let epoch = SHARED.begin_native_session().expect("native session");
        let mut eng = engine_loading(epoch);
        crate::player::ffi_host::hold_load_for_test();
        let payload_ptr = threads::SendPtr(eng.payload.as_ptr() as *mut c_char);
        eng.load_th = Some(
            std::thread::Builder::new()
                .name("test-load-thread-hung".into())
                .spawn(move || threads::load_thread(payload_ptr, epoch))
                .expect("spawn load_thread"),
        );
        pa.install(eng);
        wait_for_load_in_flight();

        assert!(
            SHARED.test_backdate_native_load_issued(epoch, crate::player::pump::NATIVE_LOAD_BUDGET),
            "PRECONDITION FAILED: the epoch must still own Active to backdate it"
        );
        crate::player::pump::pump(&mut ps, &mut pa, 1_000);
        assert!(
            SHARED.load_timed_out.load(Ordering::Acquire),
            "PRECONDITION FAILED: the budget must have fired, or this test is not about a timed-out Load"
        );

        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watchdog = {
            let (done, fired) = (done.clone(), fired.clone());
            std::thread::Builder::new().name("test-load-watchdog".into()).spawn(move || {
                let deadline = std::time::Instant::now() + WATCHDOG;
                while std::time::Instant::now() < deadline {
                    if done.load(Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                fired.store(true, Ordering::Release);
                crate::player::ffi_host::release_load_for_test();
            }).expect("spawn watchdog")
        };

        let started = std::time::Instant::now();
        stop_bufferfeed(&mut ps, &mut pa);
        let elapsed = started.elapsed();
        done.store(true, Ordering::Release);
        watchdog.join().unwrap();

        assert!(
            !fired.load(Ordering::Acquire) && elapsed < std::time::Duration::from_secs(1),
            "teardown blocked the main thread for {elapsed:?} on a Load that never returned \
             (watchdog had to release it: {}) — BACK from the timeout read-out freezes the app",
            fired.load(Ordering::Acquire)
        );
        assert_eq!(
            crate::player::ffi_host::in_flight_calls_for_test(),
            0,
            "teardown reached the Starfish seam ({:?}) while Load was still in flight — the #74 race",
            crate::player::ffi_host::in_flight_verb_for_test()
        );
        assert!(!pa.is_live(), "the stop must still complete from the route's point of view");
        assert!(pa.has_abandoned_load(), "the in-flight Load must be parked, not dropped");
        assert_ne!(
            unsafe { crate::player::sink().ready(pa.mt()) },
            0,
            "the object must be neither unloaded, destroyed nor quarantined while Load is in flight"
        );
        assert!(
            SHARED.begin_native_session().is_none(),
            "no new native session may begin while the seam still owns the abandoned object"
        );

        // While Load is still in flight the per-frame release is a no-op and a start is refused.
        for _ in 0..3 {
            assert!(!reap_abandoned_load(&mut pa));
        }
        assert!(pa.has_abandoned_load());
        assert!(
            start_bufferfeed_inner(&mut ps, &mut pa).is_err(),
            "a later playback must not be handed the object an abandoned Load still owns"
        );
        assert_eq!(crate::player::ffi_host::in_flight_calls_for_test(), 0, "{:?}", crate::player::ffi_host::in_flight_verb_for_test());

        // Now the Load returns. The next reap releases the object with full evidence (D1, not a
        // quarantine), and native sessions are available again.
        crate::player::ffi_host::release_load_for_test();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !reap_abandoned_load(&mut pa) {
            assert!(std::time::Instant::now() < deadline, "the returned Load was never released");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!pa.has_abandoned_load());
        assert_eq!(unsafe { crate::player::sink().ready(pa.mt()) }, 0, "the released object must be gone");
        assert!(
            !crate::player::ffi_host::lifecycle_blocked_for_test(),
            "the release must be a D1 with complete evidence, not a process-long quarantine"
        );
        let next = SHARED.begin_native_session().expect("native sessions resume after the release");
        assert!(SHARED.retire_native_session(next));
    }

    /// Releases the held host `sf_load` when a test unwinds, so a failed assertion cannot leave
    /// the next test's Load hanging.
    struct ReleaseLoadOnDrop;
    impl Drop for ReleaseLoadOnDrop {
        fn drop(&mut self) {
            crate::player::ffi_host::release_load_for_test();
        }
    }

    /// An Engine whose `sf_load` is held past the budget: the pump's budget arm has fired
    /// (`load_failed` + `load_timed_out`), the Load thread is still inside `sf_load`, and the
    /// Engine is installed in `pa`.
    fn install_engine_with_timed_out_load(
        ps: &mut crate::session::PlaybackSession,
        pa: &mut crate::player::adapter::PlayerAdapter,
    ) {
        let epoch = SHARED.begin_native_session().expect("native session");
        let mut eng = engine_loading(epoch);
        crate::player::ffi_host::hold_load_for_test();
        let payload_ptr = threads::SendPtr(eng.payload.as_ptr() as *mut c_char);
        eng.load_th = Some(
            std::thread::Builder::new()
                .name("test-load-thread-hung".into())
                .spawn(move || threads::load_thread(payload_ptr, epoch))
                .expect("spawn load_thread"),
        );
        pa.install(eng);
        wait_for_load_in_flight();
        assert!(
            SHARED.test_backdate_native_load_issued(epoch, crate::player::pump::NATIVE_LOAD_BUDGET),
            "PRECONDITION FAILED: the epoch must still own Active to backdate it"
        );
        crate::player::pump::pump(ps, pa, 1_000);
        assert!(
            SHARED.load_timed_out.load(Ordering::Acquire),
            "PRECONDITION FAILED: the budget must have fired, or this test is not about a timed-out Load"
        );
    }

    /// A Load passes its budget, a seek reload runs while the Load is still inside `sf_load`, the
    /// reload's teardown parks that Load (clearing `load_timed_out` with the rest of the session)
    /// and the start is refused. The failure must keep the timeout as its cause, not `unspecified`.
    #[test]
    fn a_recovery_reload_refused_for_an_abandoned_load_still_reports_the_timeout() {
        let mut ps = crate::session::PlaybackSession::IDLE;
        let _serial = tv_base::testlock::serial();
        SHARED.reset_session();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        let _cleanup = Cleanup;
        let _release = ReleaseLoadOnDrop;
        let mut pa = crate::player::adapter::PlayerAdapter::new(unsafe {
            tv_base::task::MainThread::assume()
        });
        let mut start = crate::session::PlaybackStart::new("http://127.0.0.1:1/media/film.mkv");
        start.stream = Some(crate::stream::StreamDeclaration::default());
        ps.begin(start).expect("admitted");
        install_engine_with_timed_out_load(&mut ps, &mut pa);

        // The reload (whose teardown parks the still-running Load) and the settle that publishes
        // the outcome.
        let started = crate::player::pump::settle_reload(reload_at(&mut ps, &mut pa, 0), "test seek reload");
        assert!(!started, "the recovery start must be refused while the Load is outstanding");
        assert!(pa.has_abandoned_load(), "PRECONDITION FAILED: the Load must still be parked");
        assert_eq!(crate::player::state(&ps), crate::player::PlaybackState::Error);
        assert_eq!(
            crate::player::error_now(&ps).kind,
            crate::player::FailureKind::LoadTimeout,
            "the refusal must keep the timeout as its cause, not report `unspecified`"
        );
    }

    /// The fresh-start half of the same hole: a Retry or Back+Play while the parked Load is still
    /// inside `sf_load` is refused with no Engine to publish anything, so the player sat in Idle
    /// and drew nothing: no read-out, no report, no Try Again. The refusal stays (a second native
    /// object must not be started), but the viewer is told why.
    #[test]
    fn a_fresh_start_refused_for_an_abandoned_load_shows_an_error_with_try_again() {
        let mut ps = crate::session::PlaybackSession::IDLE;
        let _serial = tv_base::testlock::serial();
        SHARED.reset_session();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        let _cleanup = Cleanup;
        let _release = ReleaseLoadOnDrop;
        let mut pa = crate::player::adapter::PlayerAdapter::new(unsafe {
            tv_base::task::MainThread::assume()
        });
        install_engine_with_timed_out_load(&mut ps, &mut pa);
        stop_bufferfeed(&mut ps, &mut pa);
        assert!(pa.has_abandoned_load(), "PRECONDITION FAILED: the Load must be parked");

        // `finish_landed_play`'s entry. A refused start answers false and creates no Engine.
        assert!(!crate::player::start_bufferfeed(&mut ps, &mut pa));
        assert!(!pa.is_live(), "the start stays refused: no second native object");
        assert!(pa.has_abandoned_load());

        assert_eq!(
            crate::player::state(&ps),
            crate::player::PlaybackState::Error,
            "a refused fresh start left the player Idle: a black screen with no read-out"
        );
        let shape = crate::player::error_now(&ps);
        assert_eq!(shape.kind, crate::player::FailureKind::LoadTimeout);
        let actions = crate::player::failure_actions(
            shape.kind,
            crate::player::FailureContext { can_retry: true, repair_idle: true },
        );
        assert!(
            actions.contains(&crate::player::FailureAction::TryAgain),
            "the read-out must offer Try Again: {actions:?}"
        );

        // The Load returns and is reaped: the next start is no longer refused for it, and the
        // refusal's read-out does not outlive the refusal.
        crate::player::ffi_host::release_load_for_test();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !reap_abandoned_load(&mut pa) {
            assert!(std::time::Instant::now() < deadline, "the returned Load was never released");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let _ = crate::player::start_bufferfeed(&mut ps, &mut pa);
        assert_ne!(
            crate::player::state(&ps),
            crate::player::PlaybackState::Error,
            "a start that is no longer refused must not keep the refusal's read-out"
        );
    }

    /// The latch's other edge: a start refused for the parked Load sets `native_start_refused`,
    /// and a RELOAD that then starts (after the Load was reaped) goes through
    /// `start_bufferfeed_for`, not `start_bufferfeed_tracked`. The flag must not outlive the
    /// refusal, or `state()` answers Error over playing video.
    #[test]
    fn a_reload_that_starts_after_a_refused_fresh_start_does_not_keep_the_refusals_error() {
        let mut ps = crate::session::PlaybackSession::IDLE;
        let _serial = tv_base::testlock::serial();
        SHARED.reset_session();
        crate::player::ffi_host::reset_native_lifecycle_for_test();
        crate::player::ffi_host::force_clocksink_for_test(true);
        let _cleanup = Cleanup;
        let _release = ReleaseLoadOnDrop;
        let mut pa = crate::player::adapter::PlayerAdapter::new(unsafe {
            tv_base::task::MainThread::assume()
        });
        install_engine_with_timed_out_load(&mut ps, &mut pa);
        stop_bufferfeed(&mut ps, &mut pa);
        assert!(pa.has_abandoned_load(), "PRECONDITION FAILED: the Load must be parked");
        let mut start = crate::session::PlaybackStart::new("http://127.0.0.1:1/media/film.mkv");
        start.stream = Some(crate::stream::StreamDeclaration { acodec: "aac".into(), ..Default::default() });
        ps.begin(start).expect("an AAC stream is admitted");

        // The resumed foreground's `PlayerForegroundActuator::start_load`.
        assert_eq!(
            start_bufferfeed_tracked(&mut ps, &mut pa),
            BufferfeedStartOutcome::Failed,
            "PRECONDITION FAILED: the fresh start must be refused for the parked Load"
        );
        assert!(ps.native_start_refused, "PRECONDITION FAILED: the refusal must be recorded");

        // The parked Load returns and is reaped; the rollback's reload then starts.
        crate::player::ffi_host::release_load_for_test();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !reap_abandoned_load(&mut pa) {
            assert!(std::time::Instant::now() < deadline, "the returned Load was never released");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let reload = reload_at(&mut ps, &mut pa, 0);
        assert!(
            matches!(reload, ReloadOutcome::Started),
            "PRECONDITION FAILED: the reload must start once the Load is reaped ({reload:?})"
        );
        assert!(pa.is_live(), "the reload must have built an Engine");

        assert_ne!(
            crate::player::state(&ps),
            crate::player::PlaybackState::Error,
            "a reload that started kept the refused fresh start's Error over live video"
        );
        stop_bufferfeed(&mut ps, &mut pa);
    }
}

/// **The webOS 10.3.1 refusal, replayed.** `docs/webos10-lab-report.md` §3.2 records the exact
/// callback order the pipeline produced when it refused the 4K60 H.264 envelope: `13/1`, `14/0`,
/// the `type=5` sink echo, `15/0`, `8/0`, then `18/601 Resource Allocation Error` - with `Load()`
/// itself having returned `ok=1`. §3.5 records what the app did about it: nothing. `load_failed`
/// stayed false, the state machine sat in Connecting, and no failure read-out was ever reached.
///
/// This test is that artifact, host-side, watched RED before the fix (2026-09-03): the red is the
/// lab set's measured sequence, NOT a reproduction of issue #63, whose set nobody here has seen a
/// log from. Under `hostsim` only because it borrows `prime_livelock_tests`' engine constructor.
/// The envelope rule, cell by cell. Every value here is either a measured declaration
/// (`ENVELOPE_UHD60`, `ENVELOPE_FHD60`) or today's behaviour; the test exists so the narrow rule
/// stays narrow - a "smarter" envelope that changes HEVC or the unknown-raster case fails here.
#[cfg(test)]
mod sink_envelope_tests {
    use super::*;

    fn caps(h264: (u32, u32, u32), hevc: (u32, u32, u32)) -> tv_platform::devcaps::Caps {
        let mut c = tv_platform::devcaps::Caps::assumed();
        c.h264_row = h264;
        c.hevc_row = hevc;
        c
    }
    #[test]
    fn audio_payload_names_the_feeds_own_codecs_and_refuses_the_rest() {
        let c = tv_platform::devcaps::Caps::assumed();
        let payload = |codec: &str| crate::stream::AudioFeed::decide(codec, 2, &c, crate::stream::SoftwareAudio::NONE).payload();
        assert_eq!(payload("aac"), Some(("AAC", 3)));
        assert_eq!(payload("eac3"), Some(("AC3 PLUS", 2)));
        for codec in ["dts", "truehd", "flac", "mp2", "", "unknown"] {
            assert_eq!(payload(codec), None, "{codec}");
        }
        let decl = crate::stream::StreamDeclaration::default();
        let payload = build_av_payload(&decl, crate::stream::DvPresentation::NotDv, "H264", "AAC", ENVELOPE_FHD60);
        assert!(payload.contains(r#""audio":"AAC""#));
    }

    const DEV_SET: ((u32, u32, u32), (u32, u32, u32)) = ((4096, 2304, 60), (4096, 2176, 60));
    const FHD_SET: ((u32, u32, u32), (u32, u32, u32)) = ((1920, 1088, 60), (1920, 1088, 60));

    #[test]
    fn h264_declares_fhd_only_when_the_widest_raster_fits_fhd() {
        let c = caps(DEV_SET.0, DEV_SET.1);
        for (raster, want) in [
            ((720, 480), ENVELOPE_FHD60),
            ((1280, 720), ENVELOPE_FHD60),
            ((1920, 1080), ENVELOPE_FHD60),
            ((1920, 1088), ENVELOPE_FHD60), // coded height of a 1080p stream
            ((1918, 802), ENVELOPE_FHD60),  // scope film
            ((3840, 2160), ENVELOPE_UHD60),
            ((4096, 2176), ENVELOPE_UHD60),
            ((0, 0), ENVELOPE_UHD60),  // nobody said: today's value
            ((1920, 0), ENVELOPE_UHD60),
        ] {
            assert_eq!(sink_envelope(LoadVideo::H264, raster, 0.0, &c, true), want, "h264 {raster:?}");
            assert_eq!(
                sink_envelope(LoadVideo::H264, raster, 0.0, &c, false),
                want,
                "h264 {raster:?} unmeasured"
            );
        }
    }

    #[test]
    fn hevc_keeps_the_verified_4k60_on_a_4k_set_whatever_the_source() {
        let c = caps(DEV_SET.0, DEV_SET.1);
        for raster in [(720, 480), (1920, 1080), (3840, 2160), (0, 0)] {
            assert_eq!(sink_envelope(LoadVideo::H265, raster, 0.0, &c, true), ENVELOPE_UHD60, "{raster:?}");
            assert_eq!(sink_envelope(LoadVideo::H265, raster, 0.0, &c, false), ENVELOPE_UHD60, "{raster:?}");
        }
    }

    #[test]
    fn a_measured_full_hd_table_clamps_both_codecs_and_the_assumed_table_clamps_nothing() {
        let c = caps(FHD_SET.0, FHD_SET.1);
        assert_eq!(sink_envelope(LoadVideo::H265, (3840, 2160), 0.0, &c, true), ENVELOPE_FHD60);
        assert_eq!(sink_envelope(LoadVideo::H264, (3840, 2160), 0.0, &c, true), ENVELOPE_FHD60);
        assert_eq!(sink_envelope(LoadVideo::H265, (0, 0), 0.0, &c, true), ENVELOPE_FHD60);
        // the same rows, not measured: never a clamp
        assert_eq!(sink_envelope(LoadVideo::H265, (3840, 2160), 0.0, &c, false), ENVELOPE_UHD60);
        assert_eq!(sink_envelope(LoadVideo::H265, (0, 0), 0.0, &tv_platform::devcaps::Caps::assumed(), true), ENVELOPE_UHD60);
        // a table stating a lower frame rate clamps it; a higher one never raises it
        let slow = caps((3840, 2160, 30), (3840, 2160, 120));
        assert_eq!(sink_envelope(LoadVideo::H264, (3840, 2160), 0.0, &slow, true).fps, 30);
        assert_eq!(sink_envelope(LoadVideo::H265, (3840, 2160), 0.0, &slow, true).fps, 60);
    }

    /// The device measurement behind the rate rule: 4K H.264 declared at 60 is re-announced at
    /// 30 by the pipeline; at its own class it is not. HEVC and an unknown rate keep 60.
    #[test]
    fn h264_declares_the_streams_rate_class_and_hevc_keeps_60() {
        let c = caps(DEV_SET.0, DEV_SET.1);
        for (fps, want) in [(24.0, 24), (23.976, 24), (25.0, 25), (29.97, 30), (30.0, 30), (50.0, 50), (59.94, 60), (60.0, 60), (120.0, 60), (0.0, 60)] {
            assert_eq!(sink_envelope(LoadVideo::H264, (3840, 2160), fps, &c, true).fps, want, "h264 {fps}");
            assert_eq!(sink_envelope(LoadVideo::H264, (1920, 1080), fps, &c, true).fps, want, "h264 fhd {fps}");
            assert_eq!(sink_envelope(LoadVideo::H265, (3840, 2160), fps, &c, true).fps, 60, "hevc {fps}");
        }
        // the table still clamps a class from above
        let slow = caps((3840, 2160, 25), (3840, 2160, 60));
        assert_eq!(sink_envelope(LoadVideo::H264, (3840, 2160), 30.0, &slow, true).fps, 25);
    }

    /// Issue #665: an MKV's 1 ms timebase stores a 30p frame as 33 ms, so a probe reports 30.303 and
    /// the strict class rounded it up to 50, which a 4K@30 H.264 decoder refuses. A rate within
    /// the timebase rounding error above a class snaps to it; a genuinely higher one still rounds up.
    #[test]
    fn a_millisecond_timebase_rate_snaps_to_its_class() {
        let c = caps(DEV_SET.0, DEV_SET.1);
        for (fps, want) in [
            (30.303, 30), (24.39, 24), (23.81, 24), (25.0, 25), (50.0, 50), (58.82, 60), (59.94, 60),
            (31.0, 50), (26.0, 30),
        ] {
            assert_eq!(sink_envelope(LoadVideo::H264, (3840, 2160), fps, &c, false).fps, want, "h264 {fps}");
        }
    }

    /// Issue #665: the re-encode of a 4K source at 480p declares FHD60 for H.264.
    #[test]
    fn a_480p_ceiling_on_a_4k_source_declares_fhd60_for_h264() {
        let c = caps(DEV_SET.0, DEV_SET.1);
        assert_eq!(sink_envelope(LoadVideo::H264, (854, 480), 0.0, &c, false), ENVELOPE_FHD60);
    }

    #[test]
    fn the_payload_carries_all_three_envelope_numbers() {
        let decl = crate::stream::StreamDeclaration::default();
        let p = build_av_payload(&decl, crate::stream::DvPresentation::NotDv, "H264", "AAC", ENVELOPE_FHD60);
        assert!(p.contains(r#""maxWidth":1920"#), "{p}");
        assert!(p.contains(r#""maxHeight":1080"#), "{p}");
        assert!(p.contains(r#""maxFrameRate":60"#), "{p}");
        let p = build_av_payload(&decl, crate::stream::DvPresentation::NotDv, "H265", "AC3 PLUS", ENVELOPE_UHD60);
        assert!(p.contains(r#""maxWidth":3840"#) && p.contains(r#""maxHeight":2160"#), "{p}");
        assert!(!p.contains(r#""maxFrameRate":30"#), "the template's 30 must be replaced: {p}");
    }

    /// A 5.1 AAC live channel was declared with no format at all, and the TV reported its audio
    /// as `0.000000 0 ... aac 0` and stuttered. The AAC node Kodi and ss4s send (and libpf parses).
    #[test]
    fn an_aac_payload_carries_the_tracks_format() {
        let decl = crate::stream::StreamDeclaration { audio_channels: 6, audio_sample_rate: 48_000, ..Default::default() };
        let p = build_av_payload(&decl, crate::stream::DvPresentation::NotDv, "H264", "AAC", ENVELOPE_FHD60);
        assert!(p.contains(r#""aacInfo":{"channels":6,"profile":2,"format":"adts","frequency":48}"#), "{p}");
        let unknown = crate::stream::StreamDeclaration::default();
        assert!(!build_av_payload(&unknown, crate::stream::DvPresentation::NotDv, "H264", "AAC", ENVELOPE_FHD60).contains("aacInfo"));
        assert!(!build_av_payload(&decl, crate::stream::DvPresentation::NotDv, "H264", "AC3", ENVELOPE_FHD60).contains("aacInfo"));
        let p = build_av_payload(
            &crate::stream::StreamDeclaration { audio_channels: 2, audio_sample_rate: 44_100, ..Default::default() },
            crate::stream::DvPresentation::NotDv, "H264", "AAC", ENVELOPE_FHD60);
        assert!(p.contains(r#""frequency":44.1}"#), "{p}");
    }

    /// The static payloads' declared envelopes, read out of the strings that are actually sent.
    #[test]
    fn the_static_envelopes_are_the_static_payloads_own_numbers() {
        for (video, payload) in [(LoadVideo::H264, PAYLOAD_V), (LoadVideo::H265, PAYLOAD_H265)] {
            let e = static_envelope(video);
            assert!(payload.contains(&format!(r#""maxWidth":{}"#, e.w)), "{video:?}");
            assert!(payload.contains(&format!(r#""maxHeight":{}"#, e.h)), "{video:?}");
            assert!(payload.contains(&format!(r#""maxFrameRate":{}"#, e.fps)), "{video:?}");
        }
    }

    #[test]
    fn the_sinkmax_override_parses_exactly_wxh_at_f() {
        assert_eq!(
            parse_sinkmax("1920x1080@30\n"),
            Some(SinkEnvelope { w: 1920, h: 1080, fps: 30 })
        );
        assert_eq!(parse_sinkmax("1920x1080"), None);
        assert_eq!(parse_sinkmax("0x1080@60"), None);
        assert_eq!(parse_sinkmax("abc"), None);
    }

    #[test]
    fn load_video_maps_exactly_the_buffer_feeds_video_caps() {
        for (vcodec, want) in [
            ("vp8", LoadVideo::Vp8),
            ("hevc", LoadVideo::H265),
            ("h264", LoadVideo::H264),
            ("av1", LoadVideo::Av1),
            ("vp9", LoadVideo::Vp9),
            ("", LoadVideo::H264),
            ("HEVC", LoadVideo::H265),
            ("H264", LoadVideo::H264),
            ("h265", LoadVideo::H265),
        ] {
            assert_eq!(LoadVideo::of_stream(vcodec), Some(want), "{vcodec:?}");
        }
        // an unknown codec is refused, never declared as H.264
        for vcodec in ["mpeg2video", "vc1", "wmv3", "msmpeg4v3", "h263", "rv40", "theora", "mpeg4", "avc", "x"] {
            assert_eq!(LoadVideo::of_stream(vcodec), None, "{vcodec:?}");
        }
        let all = [LoadVideo::H264, LoadVideo::H265, LoadVideo::Av1, LoadVideo::Vp9, LoadVideo::Vp8];
        assert_eq!(all.map(LoadVideo::payload_name), ["H264", "H265", "AV1", "VP9", "VP8"]);
        assert_eq!(all.map(LoadVideo::diag_code), [1, 2, 3, 4, 5]);
    }

    #[test]
    fn vp9_envelope_follows_hevc_and_its_own_row() {
        let mut c = caps(DEV_SET.0, DEV_SET.1);
        c.vp9 = true;
        c.vp9_row = (3840, 2160, 30);
        assert_eq!(sink_envelope(LoadVideo::Vp9, (1920, 1080), 23.976, &c, false), ENVELOPE_UHD60);
        assert_eq!(
            sink_envelope(LoadVideo::Vp9, (1920, 1080), 23.976, &c, true),
            SinkEnvelope { w: 3840, h: 2160, fps: 30 }
        );
        c.vp9_row = (0, 0, 0);
        assert_eq!(sink_envelope(LoadVideo::Vp9, (1920, 1080), 23.976, &c, true), ENVELOPE_UHD60);
    }

    #[test]
    fn av1_envelope_follows_hevc_and_its_own_row() {
        let mut c = caps(DEV_SET.0, DEV_SET.1);
        c.av1 = true;
        c.av1_row = (3840, 2160, 30);
        assert_eq!(sink_envelope(LoadVideo::Av1, (1920, 1080), 23.976, &c, false), ENVELOPE_UHD60);
        assert_eq!(
            sink_envelope(LoadVideo::Av1, (1920, 1080), 23.976, &c, true),
            SinkEnvelope { w: 3840, h: 2160, fps: 30 }
        );
        c.av1_row = (0, 0, 0);
        assert_eq!(sink_envelope(LoadVideo::Av1, (1920, 1080), 23.976, &c, true), ENVELOPE_UHD60);
    }

    #[test]
    fn static_envelope_av1_and_vp9_are_uhd60() {
        assert_eq!(static_envelope(LoadVideo::Av1), ENVELOPE_UHD60);
        assert_eq!(static_envelope(LoadVideo::Vp9), ENVELOPE_UHD60);
        assert_eq!(static_envelope(LoadVideo::H265), ENVELOPE_UHD60);
        assert_eq!(static_envelope(LoadVideo::H264), SinkEnvelope { w: 1920, h: 1080, fps: 30 });
    }

    #[test]
    fn the_renamed_static_payloads_declare_their_codec() {
        assert_eq!(PAYLOAD_H265.matches(r#""video":"H265""#).count(), 1);
        for (video, name) in [(LoadVideo::Av1, "AV1"), (LoadVideo::Vp9, "VP9")] {
            let p = renamed_static_payload(video);
            assert!(p.contains(&format!(r#""video":"{name}""#)), "{p}");
            assert!(!p.contains("H265"), "{p}");
            assert!(p.contains(r#""maxWidth":3840"#) && p.contains(r#""maxHeight":2160"#), "{p}");
        }
    }

    #[test]
    fn load_video_refused_only_for_av1_and_vp9_without_their_rows() {
        let assumed = tv_platform::devcaps::Caps::assumed();
        assert!(load_video_refused(LoadVideo::Av1, &assumed));
        assert!(load_video_refused(LoadVideo::Vp9, &assumed));
        assert!(!load_video_refused(LoadVideo::H264, &assumed));
        assert!(!load_video_refused(LoadVideo::H265, &assumed));
        let with_row = tv_platform::devcaps::Caps {
            av1: true,
            vp9: true,
            ..tv_platform::devcaps::Caps::assumed()
        };
        for video in [LoadVideo::H264, LoadVideo::H265, LoadVideo::Av1, LoadVideo::Vp9] {
            assert!(!load_video_refused(video, &with_row), "{video:?}");
        }
        // one row never admits the other codec
        let av1_only = tv_platform::devcaps::Caps { av1: true, ..tv_platform::devcaps::Caps::assumed() };
        assert!(load_video_refused(LoadVideo::Vp9, &av1_only));
    }

    #[test]
    fn load_v_name_is_the_name_the_payload_declares() {
        use crate::player::load_v_name;
        assert_eq!(load_v_name(1), "H264");
        assert_eq!(load_v_name(2), "H265");
        assert_eq!(load_v_name(3), "AV1");
        assert_eq!(load_v_name(4), "VP9");
        assert_eq!(load_v_name(0), "—");
        for video in [LoadVideo::H264, LoadVideo::H265, LoadVideo::Av1, LoadVideo::Vp9, LoadVideo::Vp8] {
            assert_eq!(load_v_name(video.diag_code()), video.payload_name());
        }
    }
}

#[cfg(all(test, feature = "hostsim"))]
mod load_refusal_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// The strings the lab log carries at each callback, sanitised exactly as the report has them.
    const ECHO_SINK: &std::ffi::CStr =
        c"0 video/x-h264 (null) (null) 3840 2160 (null) 60.000000 0 0 0";
    const REFUSAL: &std::ffi::CStr = c"Resource Allocation Error";

    /// The Engine itself needs no cleanup - it lives in the test's own `PlayerAdapter`, which drops
    /// with the test. What is still process-wide is `SHARED`.
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            SHARED.reset_session();
        }
    }

    #[test]
    fn an_asynchronous_load_refusal_is_a_verdict_not_a_black_screen() {
        let mut ps = crate::session::PlaybackSession::IDLE;
        let _serial = tv_base::testlock::serial();
        SHARED.reset_session();
        let _cleanup = Cleanup;
        let mut pa = crate::player::adapter::PlayerAdapter::new(unsafe {
            tv_base::task::MainThread::assume()
        });
        let epoch = SHARED.begin_native_session().expect("native session");
        let mut eng = super::prime_livelock_tests::engine_after_reload();
        eng.native_epoch = epoch;
        pa.install(eng);

        // `Load()` returned ok=1 - nothing here says otherwise - and then the pipeline talked.
        super::super::sf_on_event(epoch, 13, 1, c"".as_ptr());
        super::super::sf_on_event(epoch, 14, 0, c"1".as_ptr());
        super::super::sf_on_event(epoch, 5, 0, ECHO_SINK.as_ptr());
        super::super::sf_on_event(epoch, 15, 0, c"1".as_ptr());
        super::super::sf_on_event(epoch, 8, 0, c"audio/mpeg".as_ptr());
        super::super::sf_on_event(epoch, 18, 601, REFUSAL.as_ptr());

        crate::player::pump::pump(&mut ps, &mut pa, 1_000);

        assert!(
            SHARED.load_failed.load(Ordering::Acquire),
            "type=18 num=601 before any picture must publish load_failed; the lab set sat on a \
             black screen for 70 s because it did not"
        );
        assert_eq!(
            super::super::state(&ps),
            crate::player::PlaybackState::Error,
            "the pump must turn a refused Load into the Error state the read-out renders from"
        );
        assert_eq!(
            super::super::error_now(&ps).kind,
            super::super::FailureKind::TvPipeline,
            "and the verdict must name the television's pipeline, not a generic stop"
        );
    }
}

#[cfg(test)]
mod inplace_layout_tests {
    use super::inplace_layout_known;

    #[test]
    fn in_place_seek_needs_the_cxx11_feed_and_a_release_that_is_unknown_or_4_plus() {
        assert!(inplace_layout_known(4, false));
        assert!(inplace_layout_known(0, false));
        assert!(inplace_layout_known(11, false));
        assert!(!inplace_layout_known(3, false));
        assert!(!inplace_layout_known(3, true));
        assert!(!inplace_layout_known(4, true));
        assert!(!inplace_layout_known(0, true));
    }
}

#[cfg(test)]
mod audio_lead_in_tests {
    use super::{audio_lead_in, AUDIO_LEAD_IN_NS};

    #[test]
    fn the_opening_au_and_the_window_after_it_are_lead_in_and_nothing_else_is() {
        let start = 11_008_000_000;
        assert!(audio_lead_in(start, true, None), "the marked first AU opens the timeline");
        assert!(!audio_lead_in(start, false, None), "an unmarked AU with no start is not");
        assert!(audio_lead_in(start + AUDIO_LEAD_IN_NS - 1, false, Some(start)));
        assert!(!audio_lead_in(start + AUDIO_LEAD_IN_NS, false, Some(start)), "the window ends");
        assert!(!audio_lead_in(start - 1, false, Some(start)), "nothing before the start");
    }
}

/// **A film whose audio starts 11 s after its video must still start.** The device log of the
/// failing WEB-DL (E-AC-3 with an MKV `Delay` of 11.008 s): `SMP Play`, `feed v#1..v#4`, the
/// demuxer's `first audio at 11008ms`, then no `feed a#` line at all and `timeline playing t=0s`
/// for good. Starfish's clock waits for audio, so `pres_fed` stays 0 and the video lane stops
/// [`MAX_FEED_AHEAD_NS`] in; the audio lane dropped every AU as stale because each was more than
/// [`AUDIO_STALE_AHEAD_NS`] past that video. These tests drive the real lanes through
/// [`feed_both_lanes`] against the host feed seam, in the state a progressive start leaves:
/// Play already issued, nothing presented (`pres_fed` 0, not [`PRES_NONE`]).
#[cfg(all(test, feature = "hostsim"))]
mod late_audio_start_tests {
    use super::*;

    const V_STEP_NS: i64 = 41_708_333; // 23.976 fps
    const A_STEP_NS: i64 = 32_000_000; // E-AC-3: 1536 samples at 48 kHz
    const AUDIO_DELAY_NS: i64 = 11_008_000_000;

    /// Process-wide state these tests set, restored on the way out, panic or not.
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            crate::player::ffi_host::force_clocksink_for_test(false);
            SHARED.first_audio_pts.store(i64::MIN, Ordering::Relaxed);
            SHARED.pres_fed.store(0, Ordering::Relaxed);
        }
    }

    /// An engine past `SMP Play` at offset 0 holding the film's first 12 s: video from 0, audio
    /// from [`AUDIO_DELAY_NS`]. `first_audio` is what the demuxer published as its first audio.
    fn started_with_late_audio(first_audio: i64) -> Engine {
        let mut eng = super::prime_livelock_tests::engine_after_reload();
        eng.prime_play = false;
        let au = [0u8; 64];
        let qv = &mut **eng.aq_video.as_mut().unwrap() as *mut crate::aq::AuQueue;
        for i in 0..(12_000_000_000 / V_STEP_NS) {
            crate::aq::aq_push(qv, au.as_ptr(), au.len() as c_int, i * V_STEP_NS, c_int::from(i == 0), 1);
        }
        let qa = &mut **eng.aq_audio.as_mut().unwrap() as *mut crate::aq::AuQueue;
        for i in 0..200 {
            crate::aq::aq_push(qa, au.as_ptr(), au.len() as c_int, AUDIO_DELAY_NS + i * A_STEP_NS, 1, 2);
        }
        SHARED.first_audio_pts.store(first_audio, Ordering::Relaxed);
        eng
    }

    fn run_ticks(eng: &mut Engine) {
        let mt = unsafe { tv_base::task::MainThread::assume() };
        for _ in 0..8 {
            feed_both_lanes(&mt, eng);
        }
    }

    fn arm() -> Restore {
        crate::player::ffi_host::force_clocksink_for_test(true);
        SHARED.pres_fed.store(0, Ordering::Relaxed);
        SHARED.seek_to_ns.store(-1, Ordering::Relaxed);
        Restore
    }

    #[test]
    fn a_track_whose_audio_starts_late_reaches_the_sink() {
        let _serial = tv_base::testlock::serial();
        let _restore = arm();
        let mut eng = started_with_late_audio(AUDIO_DELAY_NS);
        run_ticks(&mut eng);

        // Precondition: the video lane fed and then stopped at the throttle, as on the device.
        assert!(
            eng.max_fed_video_pts > 0 && eng.max_fed_video_pts < AUDIO_DELAY_NS - AUDIO_STALE_AHEAD_NS,
            "PRECONDITION FAILED, not the defect: video fed to {}ns — the host sink is not armed \
             or the throttle did not hold the video lane short of the audio",
            eng.max_fed_video_pts
        );
        assert_eq!(eng.audio_start_fp, Some(AUDIO_DELAY_NS), "the first audio AU was fed");
        assert!(
            eng.max_fed_audio_pts >= AUDIO_DELAY_NS + AUDIO_LEAD_IN_NS - A_STEP_NS,
            "the lead-in was fed ahead of the clock (audio to {}ns)",
            eng.max_fed_audio_pts
        );
        // Past the lead-in the throttle holds the audio - held, not dropped as stale.
        let next = eng.pending_audio.as_ref().map(|n| unsafe { crate::aq::au_fields(n.0) }.2);
        assert_eq!(next, Some(eng.max_fed_audio_pts + A_STEP_NS), "the next AU waits for the clock");
    }

    #[test]
    fn audio_far_ahead_that_does_not_open_the_timeline_is_still_dropped_as_stale() {
        let _serial = tv_base::testlock::serial();
        let _restore = arm();
        // The demuxer has not marked these AUs: stale pre-seek audio looks exactly like this.
        let mut eng = started_with_late_audio(i64::MIN);
        run_ticks(&mut eng);

        assert!(eng.max_fed_video_pts > 0, "PRECONDITION FAILED: the host sink fed no video");
        assert_eq!(eng.max_fed_audio_pts, 0, "stale audio must not reach the sink");
        assert_eq!(eng.audio_start_fp, None);
    }
}

/// **A 1 fps, video-only clip must reach the decoder past its reorder depth.** AIOStreams' "still
/// downloading" placeholder (H.264, 1 fps, B-pyramid, no audio) arrives in decode order
/// 0 4 2 1 3 8 6 5 7 ...: the beta log fed `v#1 fed=0`, `v#2 fed=3999999999`, `primed`, and then
/// nothing for a minute. Nothing has presented (`pres_fed` 0), so the throttle held B(2 s) at
/// 1.6 s past the playhead, while the decoder, holding two frames, never output one. At 24 fps the
/// same reorder fits in 170 ms. The lane must take the decoder's whole frame store, and a B-frame
/// three frames behind its P must not read as a stale backward jump.
#[cfg(all(test, feature = "hostsim"))]
mod low_fps_video_only_tests {
    use super::*;

    const FRAME_NS: i64 = 1_000_000_000;
    /// The clip's first 17 AUs in decode order, in frames: 16 s of media past the first.
    const DECODE_ORDER: [i64; 17] = [0, 4, 2, 1, 3, 8, 6, 5, 7, 12, 10, 9, 11, 16, 14, 13, 15];

    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            crate::player::ffi_host::force_clocksink_for_test(false);
            SHARED.pres_fed.store(0, Ordering::Relaxed);
        }
    }

    #[test]
    fn a_one_fps_clip_feeds_its_whole_reorder_before_a_frame_presents() {
        let _serial = tv_base::testlock::serial();
        crate::player::ffi_host::force_clocksink_for_test(true);
        let _restore = Restore;
        SHARED.pres_fed.store(0, Ordering::Relaxed);
        SHARED.seek_to_ns.store(-1, Ordering::Relaxed);
        let mut eng = super::prime_livelock_tests::engine_after_reload();
        eng.video_frame_ns = FRAME_NS;
        let au = [0u8; 64];
        let qv = &mut **eng.aq_video.as_mut().unwrap() as *mut crate::aq::AuQueue;
        for (i, f) in DECODE_ORDER.iter().enumerate() {
            crate::aq::aq_push(qv, au.as_ptr(), au.len() as c_int, f * FRAME_NS, c_int::from(i == 0), 1);
        }
        let before = VTOT.load(Ordering::Relaxed);
        let mt = unsafe { tv_base::task::MainThread::assume() };
        for _ in 0..8 {
            feed_both_lanes(&mt, &mut eng);
        }
        assert!(!eng.prime_play, "PRECONDITION FAILED: the video-only escape did not start the clock");
        assert_eq!(
            VTOT.load(Ordering::Relaxed) - before,
            DECODE_ORDER.len() as i64,
            "every AU of the first 16 frames reaches the decoder (fed to {} ns, next pending {:?})",
            eng.max_fed_video_pts,
            eng.pending_video.as_ref().map(|n| unsafe { crate::aq::au_fields(n.0) }.2)
        );
    }
}

/// **Issue #568: on webOS 5+ (`VP_EXPORTED`) every direct-play seek was a full fresh Starfish
/// `Load` (black screen, the DV/HDR badge re-popping).** `scrub: tap commit 905s` was followed by
/// `reload_at: fresh Load at 905s`, because the Kodi in-place path reaches LG-private objects and
/// was switched off there. The seek now goes through LG's public `StarfishMediaAPIs::Seek`.
///
/// These drive the real mode selection, the pump's reload-or-in-place decision and the real
/// [`feed_stream`] re-anchor against the host sink with the clock sink armed, which reports
/// `VP_EXPORTED` exactly as a webOS 5+ set does.
#[cfg(all(test, feature = "hostsim"))]
mod native_seek_tests {
    use super::*;
    use crate::player::ffi_host;

    const V_STEP_NS: i64 = 41_708_333;
    const TARGET_NS: i64 = 905_000_000_000;

    /// Process-wide state these tests set, restored on the way out, panic or not.
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            ffi_host::force_clocksink_for_test(false);
            ffi_host::reset_seek_probe_for_test(None);
            crate::player::set_seek_mode(SeekMode::Segment);
            SHARED.pts_shift.store(0, Ordering::Relaxed);
            SHARED.seek_target_ns.store(-1, Ordering::Relaxed);
            SHARED.pres_fed.store(0, Ordering::Relaxed);
            TX.seek_to_ns.store(-1, Ordering::Relaxed);
            TX.seek_reqs.store(0, Ordering::Relaxed);
        }
    }

    fn arm(force_seek: Option<c_int>) -> Restore {
        ffi_host::force_clocksink_for_test(true);
        ffi_host::reset_seek_probe_for_test(force_seek);
        SHARED.pres_fed.store(PRES_NONE, Ordering::Relaxed);
        TX.seek_to_ns.store(-1, Ordering::Relaxed);
        TX.seek_reqs.store(0, Ordering::Relaxed);
        Restore
    }

    /// An engine that has applied a direct-play seek and is waiting for the keyframe: flushed,
    /// rebase pending, the video queue holding a keyframe at `TARGET_NS` and the AUs after it.
    fn engine_seeking() -> Engine {
        let mut eng = super::prime_livelock_tests::engine_after_reload();
        eng.flushed = true;
        eng.rebase_pending = true;
        SHARED.seek_target_ns.store(TARGET_NS, Ordering::Relaxed);
        let au = [0u8; 64];
        let qv = &mut **eng.aq_video.as_mut().unwrap() as *mut crate::aq::AuQueue;
        for i in 0..60 {
            crate::aq::aq_push(qv, au.as_ptr(), au.len() as c_int, TARGET_NS + i * V_STEP_NS, c_int::from(i == 0), 1);
        }
        eng
    }

    fn tick(eng: &mut Engine) {
        let mt = unsafe { tv_base::task::MainThread::assume() };
        feed_both_lanes(&mt, eng);
    }

    #[test]
    fn a_webos5_session_seeks_natively_and_never_reloads() {
        let _serial = tv_base::testlock::serial();
        let _restore = arm(None);
        let sink_mode = sink().window_mode();
        assert_eq!(sink_mode, crate::player::VP_EXPORTED, "PRECONDITION: the armed host sink is the webOS 5+ one");
        let mode = select_seek_mode(inplace_layout_known(5, false), sink_mode, None);
        assert_eq!(mode, SeekMode::NativeSeek, "VP_EXPORTED selects LG's public Seek");
        assert!(
            !crate::player::pump::seek_reloads(mode),
            "a mid-play direct-play seek on VP_EXPORTED must NOT be a fresh Load (#568)"
        );
    }

    #[test]
    fn the_other_firmwares_keep_their_modes() {
        assert_eq!(select_seek_mode(true, crate::player::VP_ACB, None), SeekMode::Segment, "webOS 4.x is unchanged");
        assert_eq!(select_seek_mode(false, crate::player::VP_ACB, None), SeekMode::Reload, "the old ABI reloads, as before");
        assert_eq!(select_seek_mode(false, crate::player::VP_NONE, None), SeekMode::Reload);
        assert_eq!(select_seek_mode(true, crate::player::VP_ACB, Some(SeekMode::NativeSeek)), SeekMode::NativeSeek, "the dev trigger wins");
        assert!(crate::player::pump::seek_reloads(SeekMode::Reload));
        assert!(!crate::player::pump::seek_reloads(SeekMode::Segment));
        assert!(!crate::player::pump::seek_reloads(SeekMode::NativeSeek));
    }

    #[test]
    fn the_landed_keyframe_is_the_native_seek_and_nothing_flushes() {
        let _serial = tv_base::testlock::serial();
        let _restore = arm(None);
        crate::player::set_seek_mode(SeekMode::NativeSeek);
        let mut eng = engine_seeking();
        tick(&mut eng);

        assert_eq!(ffi_host::seek_log_for_test(), vec![TARGET_NS / 1_000_000], "Seek(<keyframe ms>), once");
        assert_eq!(ffi_host::flush_calls_for_test(), 0, "Seek flushes itself; a flush after it re-anchors at 0");
        assert!(!eng.flushed && !eng.rebase_pending, "the seek is applied");
        assert_eq!(SHARED.pts_shift.load(Ordering::Relaxed), 0, "content time, no rebase");
        assert!(eng.max_fed_video_pts >= TARGET_NS, "the keyframe and the AUs after it were fed on the Seek's timeline");
        assert_eq!(crate::player::seek_mode(), SeekMode::NativeSeek);
    }

    #[test]
    fn a_pre_zero_keyframe_seeks_to_zero_not_negative() {
        let _serial = tv_base::testlock::serial();
        let _restore = arm(None);
        crate::player::set_seek_mode(SeekMode::NativeSeek);
        let mut eng = super::prime_livelock_tests::engine_after_reload();
        eng.flushed = true;
        eng.rebase_pending = true;
        SHARED.seek_target_ns.store(0, Ordering::Relaxed);
        let au = [0u8; 64];
        let qv = &mut **eng.aq_video.as_mut().unwrap() as *mut crate::aq::AuQueue;
        crate::aq::aq_push(qv, au.as_ptr(), au.len() as c_int, -80_000_000, 1, 1);
        tick(&mut eng);
        assert_eq!(ffi_host::seek_log_for_test(), vec![0], "Seek refuses a negative millis");
    }

    #[test]
    fn a_refused_seek_reloads_this_one_and_every_later_one() {
        let _serial = tv_base::testlock::serial();
        let _restore = arm(Some(0));
        crate::player::set_seek_mode(SeekMode::NativeSeek);
        let mut eng = engine_seeking();
        tick(&mut eng);

        assert_eq!(crate::player::seek_mode(), SeekMode::Reload, "native seek is off for the rest of the session");
        assert!(crate::player::pump::seek_reloads(crate::player::seek_mode()));
        assert_eq!(
            TX.seek_to_ns.load(Ordering::Acquire),
            TARGET_NS,
            "this seek is re-posted at the user's target for the pump's reload"
        );
        assert_eq!(eng.max_fed_video_pts, 0, "nothing is fed into a pipeline that refused the seek");
        assert!(eng.pending_video.is_none());
        assert_eq!(ffi_host::flush_calls_for_test(), 0);
    }
}
