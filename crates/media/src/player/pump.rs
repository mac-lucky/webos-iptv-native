//! player::pump — the main-thread pump. Runs each frame from the app loop: the failure gates, the
//! pending-seek handler, the ACB-bind state machine (Stage), and the feed dispatch. All
//! ACB/Starfish control calls happen here on the main thread.
use super::engine::{drain_aq, feed_both_lanes, feed_sample, Engine, Source};
use super::shared::{PauseCompletion, PrimeKind, SeekPause, Stage};
use super::{seek_mode, SeekMode, ACB_OK, SHARED, TX};
use tv_base::task::MainThread;
use std::os::raw::c_char;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release, SeqCst};
use std::time::Duration;

/// issue #74 D.1.4: the longest the `loadCompleted` arm will wait for `sf_load` to actually
/// return before giving up and publishing `load_failed`. Kodi's own precedent
/// (`MediaPipelineWebOS.cpp:795-801`) is a 1s `wait_for` then `PLAYER_ABORT`; this budget is
/// deliberately far larger because a real `Load` on the dev set measures in the HUNDREDS OF
/// MILLISECONDS for a cold pipeline, and the point of this timeout is to catch a genuine hang (the
/// k5lp `DirectVoInit` block), not to interrupt an ordinary slow one. It exists so a Load that
/// never returns becomes a failure read-out instead of a permanent Connecting spinner.
pub const NATIVE_LOAD_BUDGET: Duration = Duration::from_secs(20);

/// The issued→return gate shared by the constructor wait and the loadCompleted arm.
/// Ready objects retain the existing demux-failure precedence before this gate is evaluated.
fn native_load_gate_ready(epoch: u32) -> bool {
    if SHARED.native_load_returned(epoch) {
        return true;
    }
    // Log the deferral exactly once per session (a fast Load may never hit this arm at all,
    // in which case nothing here fires and the ordinary loadCompleted path below runs
    // immediately once the gate is already open).
    if SHARED.native_load_deferred_logged_epoch.swap(epoch, Relaxed) != epoch {
        super::log("native: loadCompleted while Load in flight — deferring");
    }
    // D.1.4: bound the wait. A Load that never returns must not leave the player in Connecting
    // forever — past the budget, publish the same signal an outright Load refusal sets, so the
    // failure arm at the top of `pump` raises the failure read-out on the next tick.
    match SHARED.native_load_elapsed(epoch) {
        Some(elapsed) if elapsed >= NATIVE_LOAD_BUDGET => {
            super::log(&format!(
                "native: Load did not return within {}s — publishing load_failed",
                NATIVE_LOAD_BUDGET.as_secs()
            ));
            SHARED.load_failed.store(true, Release);
            SHARED.load_timed_out.store(true, Release);
        }
        Some(_) => {}
        // `native_load_elapsed` shares its precondition with `native_load_returned` — both
        // require the epoch to still own `NativeSessionPhase::Active`. A synchronous
        // UnloadCompleted callback (firmware type=23) for this epoch flips the phase to
        // `Unloaded` *without* this arm ever having seen `Returned`, which would otherwise
        // make the budget above unreachable at exactly the moment the gate can never open
        // again. Treat losing `Active` while still deferred as a fired timeout.
        None => {
            super::log(
                "native: Load epoch left Active while loadCompleted was still deferred — \
                 publishing load_failed",
            );
            SHARED.load_failed.store(true, Release);
            SHARED.load_timed_out.store(true, Release);
        }
    }
    false
}

/// Publish the one value the HUD renders from. Pure derivation off signals the workers already
/// maintain — no new cross-thread plumbing, and it runs on every path out of `pump` (including
/// the early returns) so the state can never go stale behind a spinner that never resolves.
fn set_state(s: super::shared::PlaybackState) {
    SHARED.pb_state.store(s as u8, Relaxed);
}

/// How many seek requests this pump seek MERGED — i.e. requests that arrived while an earlier
/// seek was still resolving and so never got a seek of their own. Reset as it's read, so each
/// logged seek reports only the requests it consumed.
///
/// The count is what makes coalescing observable. `seek_to_ns` only ever holds the newest target,
/// so a burst of six taps and a single tap leave identical state behind; and the burst is applied
/// by whichever of two paths wins a race with the stuck-watchdog, which log different lines.
/// Counting either line therefore measures reopen latency, not merging — `coalesced=` measures
/// merging directly, and both paths report it.
/// Whether a direct-play seek in `mode` is a full fresh `Load` (`engine::reload_at`) rather than an
/// in-place one. Only `Reload` is: `Segment` and `NativeSeek` (LG's public `Seek`, issue #568) both
/// seek in place.
pub(super) fn seek_reloads(mode: SeekMode) -> bool {
    mode == SeekMode::Reload
}

/// The stuck watchdog's floor: a real reopen + `av_seek` + first keyframe on 4K HEVC over a LAN is
/// ~700 ms (see the coalesce note in `pump`).
pub(super) const SEEK_STUCK_MS: u32 = 1200;
/// Its ceiling, however slow the server measured: past it a retry is cheaper than waiting.
pub(super) const SEEK_STUCK_CAP_MS: u32 = 10_000;

/// The stuck watchdog's timeout for this playback's server: the floor plus three measured range
/// reopens (`SHARED.reopen_ms_est`, from `ff::seek_cb`) — a landed seek may still need a reopen or
/// two before its keyframe (an MP4's first read after the seek; a heal) — capped.
pub(super) fn seek_stuck_ms() -> u32 {
    stuck_ms_for(SHARED.reopen_ms_est.load(Relaxed))
}

fn stuck_ms_for(reopen_ms_est: u32) -> u32 {
    SEEK_STUCK_MS.saturating_add(reopen_ms_est.saturating_mul(3)).min(SEEK_STUCK_CAP_MS)
}

/// The demuxer has taken the last published av_seek target and returned from it. The mailbox is
/// read FIRST: `ff::demux` raises `demux_seeking` before emptying it, so an empty mailbox seen
/// before a lowered flag cannot be a seek that has not started yet.
fn retarget_ready() -> bool {
    SHARED.seek_to_ns.load(SeqCst) < 0 && !SHARED.demux_seeking.load(SeqCst)
}

fn take_coalesced() -> u32 {
    TX.seek_reqs.swap(0, Relaxed).saturating_sub(1)
}



/// Place the webOS 5+ exported window: source frame -> on-screen rect.
///
/// No-op on the ACB path. `src` is the frame being FED and `dst` where it lands, and the pair is
/// what expresses scaling — so a 4K direct play must not be described as 1080p, which passing the
/// authoring canvas for both would do. The coded size comes from the demuxer, the only thing that
/// knows it for certain; before it has published, the canvas is the least-wrong fallback and the
/// caller re-places once the real one arrives.
fn place_exported(mt: &MainThread, eng: &mut super::engine::Engine) {
    if super::sink().window_mode() != super::VP_EXPORTED {
        return;
    }
    let (w, h) = SHARED.video_raster();
    let src = if w > 0 && h > 0 { (w, h) } else { (1920, 1080) };
    // The viewer's aspect mode crops the SOURCE; the window is always the screen. A window larger
    // than the screen at a negative origin (the first cut of the zooms) twitched the picture.
    let mode = super::aspect_mode();
    let screen = super::video_geometry::Aspect::from_raster(1920, 1080).expect("a positive raster");
    let part = mode.crop(super::picture_aspect(src), src.0, src.1, screen);
    // A forced ratio shrinks the window inside the video rectangle, never past it.
    let (dst, dst_key) = (mode.dest(super::video_rect()), super::video_rect_key());
    let rv = unsafe { super::sink().place_window(mt, part.x, part.y, part.width, part.height, dst.x, dst.y, dst.width, dst.height) };
    eng.placed_src = src;
    PLACED_MODE.store(mode.index(), Relaxed);
    PLACED_RECT.store(dst_key, Relaxed);
    // RECORD it, do not merely log it. These three fields had no writer anywhere in the tree, so
    // `dg_place_rv` sat at its `i32::MIN` "never called" sentinel for the life of the process and
    // the diagnostics read-out rendered `Placed: not placed` in DANGER bold on every webOS 5+ set
    // — a fabricated fault, on the one firmware family nobody here can test, pointing every reader
    // at the video plane. The dev TV takes the ACB path, so no amount of looking at it would have
    // caught this.
    SHARED.dg_place_rv.store(rv, Relaxed);
    SHARED.dg_placed_w.store(src.0, Relaxed);
    SHARED.dg_placed_h.store(src.1, Relaxed);
    super::log(&format!(
        "vplane: exported window placed src={}x{} mode={mode:?} part={},{} {}x{} -> {},{} {}x{} rv={rv}",
        src.0, src.1, part.x, part.y, part.width, part.height, dst.x, dst.y, dst.width, dst.height
    ));
}

/// The aspect mode the exported window was last placed in, so a change re-places it.
static PLACED_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
/// The rectangle it was last placed in (`player::video_rect_key`), likewise.
static PLACED_RECT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Mirror Engine-confined observations into `Shared` for diagnostics and handled-error context.
///
/// The read-out cannot reach the `Engine` itself: the draw runs inside a frame where the pump
/// holds the adapter's `&mut`, and it holds no `&mut PlayerAdapter` of its own (before phase 9 the
/// slot was a `static mut` handing out `&'static mut`, so the same call was possible and was
/// instant UB). This is the only bridge, it is one-way, and nothing in the playback state machine
/// may read these fields back.
///
/// The stage is a cheap scalar and is always published so an opted-in terminal report does not
/// depend on whether Stats for Nerds happened to be open. `aq_bytes` takes each queue's pthread
/// mutex, which is why the rest is sampled HERE, once per visible diagnostics tick, and never from
/// a draw.
/// Last `frames` we saw, so a CHANGE can be stamped. A decrease counts: `frames` is seek-scoped
/// and the pump zeroes it applying a seek, which is motion, not a freeze.
static LAST_FRAMES: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

fn publish_diag(eng: &Engine, now: u32) {
    SHARED.dg_stage.store(eng.stage as u8, Relaxed);
    // Nobody is looking: skip it. `aq_bytes` takes each queue's pthread mutex, and the read-out
    // samples at 2 Hz, so publishing at 60 Hz is 30x more often than anything can observe. Costs
    // no freshness — the loop order is pump → `Diagnostics::update` → `Diagnostics::draw`
    // (`app/run.rs`), so the frame the panel
    // is switched on has already republished.
    if !super::DIAG_READOUT_ON.load(Relaxed) {
        return;
    }
    let qv = eng.aq_video.as_ref().map_or(0, |q| {
        crate::aq::aq_bytes(&**q as *const _ as *mut _) as i64
    });
    let qa = eng.aq_audio.as_ref().map_or(0, |q| {
        crate::aq::aq_bytes(&**q as *const _ as *mut _) as i64
    });
    SHARED.dg_aq_video.store(qv, Relaxed);
    SHARED.dg_aq_audio.store(qa, Relaxed);
    SHARED.dg_fed_v_pts.store(eng.max_fed_video_pts, Relaxed);
    SHARED.dg_fed_a_pts.store(eng.max_fed_audio_pts, Relaxed);
    // Stamp when the frame count MOVES. The panel needs "how long has it been stuck" and a
    // photograph has no time axis; stamping here rather than in `app::diagnostics` is what makes the
    // clock measure the STALL rather than how long the panel has been open.
    let f = SHARED.frames.load(Relaxed);
    if LAST_FRAMES.swap(f, Relaxed) != f {
        SHARED.dg_frame_at.store(now, Relaxed);
    }
}

/// A preview's failed open must not land on the player error read-out. On the host seam with no
/// clock sink, `sf_load` returns 0 because there is no video path. That is not an admitted
/// Starfish failure, so it must not open the preview breaker.
fn finish_preview_open_failure(ps: &mut crate::session::PlaybackSession, pa: &mut super::adapter::PlayerAdapter) {
    // Already retired, and its Load has not returned yet: `preview::after_pump` finishes it.
    // The failure flag stays set until then, so this arm is reached every frame in between.
    if crate::player::preview::abandoning() {
        return;
    }
    let seam_absent = cfg!(feature = "hostsim") && !tv_base::devtrig::flag("clocksink");
    if seam_absent {
        crate::player::preview::note_admission_refused();
    } else {
        crate::player::preview::note_failed(601);
    }
    crate::player::preview::retire(ps, pa, "open failed");
}

/// A source or pipeline failure: there is no other source to fall back to, so the Engine stays
/// as it is (BACK tears it down) and the viewer gets the read-out. Always the frame's last act.
fn fail_engine() {
    set_state(super::shared::PlaybackState::Error);
}

/// Settle a synchronous native reload. A reload that builds no Engine leaves no pump to publish
/// the failure, so it is published here.
pub(super) fn settle_reload(outcome:super::engine::ReloadOutcome, operation: &str) -> bool {
    if outcome == super::engine::ReloadOutcome::Started {
        return true;
    }
    super::log(&format!(
        "reload: {operation} failed synchronously ({outcome:?})",
    ));
    // The reload's teardown parked a Load that had already passed its budget and cleared the
    // session flags with the rest of that session's state, then the start was refused for the
    // outstanding object. Without the cause the report says `unspecified`; the cause is the
    // timeout, so publish it again beside the Error.
    if outcome == super::engine::ReloadOutcome::StartFailed && SHARED.native_load_abandoned() {
        SHARED.load_failed.store(true, Release);
        SHARED.load_timed_out.store(true, Release);
    }
    set_state(super::shared::PlaybackState::Error);
    false
}

/// A seek the pump is about to dispatch (in place or as a reload) while the VIEWER holds a Pause
/// can only land if the feed gate is open for its keyframe (`TX.feed_allowed`), so the pump opens
/// the bounded paused-seek preroll itself and owns closing it (`settle_paused_seek`). The
/// ownership is transport-wide (`TX.own_paused_seek`), so it survives the reload this call may be
/// followed by. A preroll a caller already opened (the foreground restore) stays the caller's.
fn own_seek_under_pause(target_ns: i64) {
    if TX.paused.load(Acquire) {
        TX.own_paused_seek(target_ns);
    }
}

/// Test-only: how many times `settle_paused_seek` took the "viewer paused mid-seek" branch.
#[cfg(test)]
static MID_SEEK_CLAIMS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The pump's half of "a paused film stays paused across a scrub", run at the head of every tick:
///
/// * a viewer Pause pressed AFTER a playing seek was dispatched shut the feed gate under it, so
///   the pump opens its preroll now (and restarts the stuck watchdog's clock from the open);
/// * a session with no picture yet under a viewer Pause (any reload, or a Pause before the first
///   frame) gets the same preroll, so it can present one frame (see the branch below);
/// * once the preroll the pump owns has LANDED on whichever Engine is live (in place: the keyframe
///   re-anchored; a reload: the fresh session re-anchored, and `frames` —
///   zeroed by the arm and by every session reset — counts a POST-seek presentation), re-pause with
///   [`super::finish_paused_seek`] (on the ACB route only once the plane is bound and a tick has
///   followed the bind: [`plane_shows_the_picture`]). A refused re-pause leaves the preroll and its ownership in
///   place, so the next tick retries; the film plays only for that long.
///
/// `playpos + 15 s >= target` is the guard the loop's old re-pause had: a presented frame far
/// before the target is not the landing. Runs before `pa` is split, because the re-pause takes the
/// whole adapter.
fn settle_paused_seek(pa: &mut super::adapter::PlayerAdapter, now: u32) {
    if TX.paused.load(Acquire)
        && !TX.seek_preroll_active()
        && SHARED.seeking.load(Relaxed)
    {
        let target = SHARED.seek_target_ns.load(Relaxed);
        let target = if target >= 0 { target } else { SHARED.playpos_ns.load(Relaxed) };
        // Only a seek the pump already DISPATCHED and that is still in flight: no newer tap waiting
        // (`request_seek` sets `SHARED.seeking` and `TX.seek_to_ns` together, ahead of the
        // dispatcher, and the dispatcher's `own_seek_under_pause` owns THAT seek at its own target),
        // and the Engine flushed or awaiting its keyframe's rebase.
        if let Some(eng) = pa.engine().filter(|e| TX.seek_to_ns.load(Relaxed) < 0 && (e.flushed || e.rebase_pending)) {
            // The Pause marked the clock the seek prime owns as the viewer's, which fences the
            // prime's Play: give the hold back to the prime first (it Plays one frame, then
            // `finish_paused_seek` re-pauses).
            if SHARED.hand_hold_to_seek_prime() && TX.own_paused_seek(target) {
                eng.seek_armed_at = now;
                #[cfg(test)]
                MID_SEEK_CLAIMS.fetch_add(1, Relaxed);
                super::log("seek: viewer paused mid-seek; the pump opened the preroll");
            }
        }
    }
    // A session with no picture under a viewer Pause. A reload keeps the Pause and zeroes `frames`,
    // the fresh Engine arms an Initial hold the Pause marks user-held, and the feed gate
    // (`TX.feed_allowed`) stays shut: no keyframe is fed, nothing is ever presented, and the HUD
    // reads Buffering over a blank plane until Play. Derived from the state, so it covers every
    // reload (a seek reload, an audio switch, a Pause pressed before the first frame) without a
    // call at each site: open the same bounded preroll a paused seek gets. Once a frame is
    // presented `frames >= 1` closes this branch for good and the landed check below re-pauses.
    if TX.paused.load(Acquire)
        && !TX.seek_preroll_active()
        && TX.seek_to_ns.load(Relaxed) < 0
        && SHARED.frames.load(Relaxed) == 0
        // A prime still owed: only then does the seek hold hand back a Play. Once the prime's Play
        // has fired (`prime_play` cleared, the clock Running) a Pause before the first position
        // update is a plain `Held(User)`; converting it would clear `user_held` with nothing left
        // to Play, and the viewer's Play would then find no hold to release.
        && pa.engine().is_some_and(|e| e.prime_play)
        && SHARED.hand_hold_to_seek_prime()
    {
        // Owned at 0, not at `playpos_ns`: `frames` is per-session (zeroed by every reset), so the
        // first presented frame IS this session's landing, while `playpos_ns` is the PRE-reload
        // playhead (a reload does not rewrite it). A target taken from it makes the `+ 15 s >=
        // target` guard fail for a reload that lands behind the old playhead, and the film then
        // plays on under a Paused transport.
        if TX.own_paused_seek(0) {
            super::log("reload: viewer paused with no picture yet; the pump opened the preroll");
        }
    }
    let Some(target) = TX.owned_paused_seek() else { return };
    let Some(eng) = pa.engine() else { return };
    let landed = !eng.flushed
        && !eng.rebase_pending
        && TX.seek_to_ns.load(Relaxed) < 0
        && SHARED.frames.load(Relaxed) >= 1
        && plane_shows_the_picture(eng)
        && SHARED.playpos_ns.load(Relaxed) + 15 * 1_000_000_000 >= target;
    if landed {
        super::finish_paused_seek(pa);
    }
}

/// Is a picture the Engine has presented actually on the panel? Without ACB (webOS 5+) the sink is
/// placed at Load, so any presented tick is. On the ACB route (webOS 4) the video plane connects
/// only through `setMediaVideoData` + `acb_start` (the `Stage::Streaming` transition below), and a
/// tick counted before that is a picture the plane never showed. A paused preroll re-paused on such
/// a tick leaves a black panel (device-measured: a VIDEO-only capture read "no signal" until
/// Resume), because the pause stops the ticks the bind is waiting on. So the landing needs the
/// bind complete AND one tick after it (`Engine::bound_at_frames`).
fn plane_shows_the_picture(eng: &Engine) -> bool {
    if !ACB_OK.load(Relaxed) {
        return true;
    }
    eng.stage >= Stage::Streaming && eng.bound_at_frames.is_none_or(|at| SHARED.frames.load(Relaxed) != at)
}

/// Ticks `SHARED.frames` must have reached before the ACB bind (`setMediaVideoData`) may be sent.
/// The pipeline's sink is bound to MAIN once the decoder produces output (the order the original
/// C engine documented: "once the decoder is producing frames so the videosink is bound to
/// MAIN"), which is why a normal start waits for a second tick (about 400 ms of playback). While
/// a paused preroll is open the pipeline is playing ONE picture for the viewer and every tick it
/// waits is a tick of audible playback, so one tick (the decoder demonstrably running) is enough.
fn acb_bind_frames_needed(preroll_open: bool) -> i32 {
    if preroll_open {
        1
    } else {
        2
    }
}

/// The viewer is paused and the pump is doing no work for a picture: the feed gate is shut (no
/// preroll open) and no seek is waiting. "Buffering…" over a player in that state is a spinner for
/// nothing; the ladder reports `PlaybackState::Paused` (the normal paused transport) instead.
/// A Load still opening (`Stage::Loading` -> Connecting) is real work and is decided before this
/// is asked.
fn paused_and_idle() -> bool {
    TX.paused.load(Acquire) && !TX.feed_allowed() && TX.seek_to_ns.load(Relaxed) < 0
}

pub fn pump(ps: &mut crate::session::PlaybackSession, pa: &mut super::adapter::PlayerAdapter, now: u32) {
    use super::shared::PlaybackState;
    // A preroll closed by a path with no adapter in hand (`abandon_seek`) leaves its mute behind;
    // the one place every closer funnels through is the next tick.
    if !TX.seek_preroll_active() {
        super::restore_preroll_volume(pa.mt());
    }
    settle_paused_seek(pa, now);
    // An unresolved start is being probed: nothing native exists yet. Its answer launches the
    // native start (or refuses it) and the rest of this tick sees whatever that built.
    if pa.has_probe() {
        match pa.poll_probe() {
            None => {
                set_state(PlaybackState::Connecting);
                return;
            }
            Some((url, result)) => super::finish_probe(ps, pa, &url, result),
        }
    }
    // ONE `&mut PlayerAdapter`, split into the live session and the seam token. Everything below
    // that replaces the session (`reload_*`) takes `pa` back, which the borrow checker only allows
    // where neither half is read afterwards.
    let (eng, mt) = pa.split();
    let eng = match eng {
        Some(e) => e,
        None => {
            set_state(PlaybackState::Idle);
            return;
        }
    };
    // Republish the Engine-confined values the diagnostics read-out needs, BEFORE the early
    // returns below, so it keeps reporting through the `sf_ready` and producer-died bail-outs —
    // which is precisely when a stalled user is looking at the panel.
    publish_diag(eng, now);
    // `sf_load == 0` may leave no callable object, so this must precede the sf_ready wait below;
    // otherwise the pump returns Connecting forever and never consumes the explicit failure.
    if SHARED.load_failed.load(Acquire) {
        if ps.is_preview() {
            finish_preview_open_failure(ps, pa);
            return;
        }
        fail_engine();
        return;
    }
    // wait for the media-thread ctor
    if unsafe { super::sink().ready(mt) } == 0 {
        // Same `!eng.preview_abandon` reasoning as the loadCompleted arms below: an abandoned
        // trailer-preview Load must not drive the D.1.4 deferral logging/budget clock just
        // because this earlier ctor-wait poll happens to run first.
        if eng.stage == Stage::Loading && !eng.preview_abandon {
            native_load_gate_ready(eng.native_epoch);
        }
        set_state(PlaybackState::Connecting);
        return;
    }
    if SHARED.frames.load(Relaxed) > 0 {
        // The first decoded frame: the observer hears the playback started (once per start).
        ps.report_started(SHARED.playpos_ns.load(Relaxed));
    }

    // The producer died before publishing a duration: the EOS path is gated on `duration_ns > 0`
    // so it can NEVER fire, and the player would sit on a black screen forever with no error and
    // no exit. Surface it instead — BACK is already the escape. A transfer that died after open
    // (`demux_io_failed`) is an error too, rather than a successful early EOF.
    if SHARED.demux_io_failed.load(Acquire) || (SHARED.demux_failed.load(Acquire) && SHARED.frames.load(Relaxed) == 0) {
        if ps.is_preview() {
            finish_preview_open_failure(ps, pa);
            return;
        }
        fail_engine();
        return;
    }
    let stream = matches!(eng.source, Source::Stream);

    // ---------- a viewer's audio-track pick (`player::request_audio_track`) ----------
    // The session already re-decided the feed for the new track; the pipeline must be re-Loaded
    // to configure the new audio decoder. A simultaneous seek supplies the reload's target, so the
    // two become ONE reload.
    if stream
        && eng.stage >= Stage::Playing
        && !(eng.flushed && eng.rebase_pending)
        && TX.audio_reload.swap(false, Relaxed)
    {
        let pending_seek = TX.seek_to_ns.swap(-1, Relaxed);
        let target = if pending_seek >= 0 { pending_seek } else { SHARED.playpos_ns.load(Relaxed).max(0) };
        let idx = SHARED.desired_audio_idx.load(Relaxed);
        own_seek_under_pause(target);
        settle_reload(super::engine::switch_audio_native(ps, pa, idx, target), "audio switch");
        return; // REPLACED the engine
    }

    // ---------- an in-place seek is still resolving (flushed, not yet re-anchored): COALESCE.
    // Firing another now would race the demux reopen (rapid tap-seeks) and the rebase guard would
    // then eat everything → stuck. Hold the latest requested target (it stays in TX.seek_to_ns) and
    // process it once this seek anchors. If it hasn't anchored within SEEK_STUCK_MS the reopen was
    // lost — first RETRY the reopen cheaply (re-interrupt the demuxer + re-arm), and only fall back
    // to a full (slow) reload if the retries also fail.
    // SEEK_STUCK_MS must comfortably exceed a real reopen+av_seek+first-keyframe on 4K HEVC over a
    // LAN (~700ms): the retry closes the demux socket, so a premature watchdog KILLS an open that
    // was about to succeed and restarts it — at 500ms a rapid tap-burst self-DoS'd into the
    // reload fallback every time. It is the FLOOR: `seek_stuck_ms` scales it by the server's
    // measured reopen latency.
    //
    // The watchdog measures time the seek COULD progress. While the feed gate is shut (a viewer
    // Pause with no preroll open) the keyframe can never be fed, so the reopen is not "lost" and
    // neither a retry nor a reload (itself gated the same way) could help: keep the clock at zero.
    // The same while the demux thread is inside `av_seek_frame` (`demux_seeking`): it is making
    // progress, and a retry or reload then only aborts the reopen that was about to land.
    if eng.flushed && eng.rebase_pending && (!TX.feed_allowed() || SHARED.demux_seeking.load(Acquire)) {
        eng.seek_armed_at = now;
    }
    // A newer target arrived while this seek was in flight: once the demuxer has LANDED (taken the
    // mailbox and left `av_seek_frame`), point it at the newest target. Nothing is aborted, and
    // the landed position's AUs are dropped (the rebase guard keys on the new target).
    if eng.flushed && eng.rebase_pending && !seek_reloads(seek_mode()) && retarget_ready() {
        let pending = TX.seek_to_ns.swap(-1, Relaxed);
        if pending >= 0 {
            let coalesced = take_coalesced();
            if pending != SHARED.seek_target_ns.load(Relaxed) {
                drain_aq(eng);
                SHARED.seek_target_ns.store(pending, Relaxed);
                SHARED.seek_to_ns.store(pending, Release);
                SHARED.playpos_ns.store(pending, Relaxed);
                eng.seek_armed_at = now;
                eng.seek_retries = 0;
                eng.rebase_drops = 0;
                super::log(&format!(
                    "seek: retarget to {}s after the in-flight seek landed coalesced={coalesced}",
                    pending / 1_000_000_000
                ));
            }
        }
    }
    if eng.flushed && eng.rebase_pending && now.wrapping_sub(eng.seek_armed_at) > seek_stuck_ms() {
        // Adopt the NEWEST coalesced target if later taps landed while this seek was resolving
        // (TX.seek_to_ns holds the latest request): retrying/reloading at the original armed
        // target would land where the user already tapped away from, then seek AGAIN.
        let pending = TX.seek_to_ns.swap(-1, Relaxed);
        let tgt = if pending >= 0 {
            pending
        } else {
            SHARED.seek_target_ns.load(Relaxed)
        }
        .max(0);
        // A refused native `Seek` leaves the engine flushed with the target re-posted for the
        // reload: never retry the reopen (it would re-open the feed and re-publish an av_seek
        // into a session whose mode no longer anchors in place), go straight to `reload_at`.
        if eng.seek_retries < 2 && !seek_reloads(seek_mode()) {
            eng.seek_retries += 1;
            SHARED.seek_to_ns.store(tgt, Release); // re-publish; the demux thread seeks on it
            SHARED.seek_target_ns.store(tgt, Relaxed); // rebase guard keys on the retried target
            eng.seek_armed_at = now;
            super::log(&format!(
                "seek: in-place stuck → retry reopen at {}s (#{}) coalesced={}",
                tgt / 1_000_000_000,
                eng.seek_retries,
                take_coalesced()
            ));
        } else {
            super::log(&format!(
                "seek: in-place stuck → reload at {}s",
                tgt / 1_000_000_000
            ));
            settle_reload(
                super::engine::reload_at(ps, pa, tgt),
                "stuck in-place seek reload",
            ); // REPLACES the engine — eng dangles, return
            return;
        }
    }
    // ---------- an HLS VOD seek (a catch-up programme): a fresh Load at the target ----------
    // The playlist has no byte addresses, so the reload's demuxer opens it at the segment holding
    // the target (`ff::open_hls`) and the rebase reads playlist time from there. Reload-per-seek
    // assumes no pipeline layout, which is what a finite IPTV archive needs.
    let t = TX.seek_to_ns.load(Relaxed);
    if stream && t >= 0 && eng.stage >= Stage::Playing && SHARED.hls_vod_base_ns.load(Acquire) >= 0 {
        TX.seek_to_ns.store(-1, Relaxed);
        let coalesced = take_coalesced();
        super::log(&format!("seek(hls): reload at {}s coalesced={coalesced}", t / 1_000_000_000));
        own_seek_under_pause(t);
        settle_reload(super::engine::reload_at(ps, pa, t), "hls seek reload");
        return; // REPLACED the engine
    }
    // ---------- a seek on a stream that has no positions (live): dropped ----------
    // No duration, no playlist time and no byte addresses: nothing could ever serve it, and
    // feeding waits while a seek is pending, so holding it would freeze the picture.
    if stream
        && t >= 0
        && SHARED.seen_frame.load(Relaxed)
        && SHARED.duration_ns.load(Relaxed) <= 0
        && SHARED.file_size.load(Relaxed) <= 0
        && SHARED.hls_vod_base_ns.load(Acquire) < 0
    {
        TX.seek_to_ns.store(-1, Relaxed);
        let _ = take_coalesced();
        super::abandon_seek();
        super::log("seek: a live stream has no positions; the request is dropped");
    }
    // ---------- pending seek: flush, drop queued AUs, re-point the demux ----------
    // A stream with no Content-Length (a live or chunked response: file_size stays -1) cannot be
    // byte-seeked; its seek waits until it can be (it never is), exactly as before.
    if stream
        && t >= 0
        // coalesce: don't stack in-place seeks. Not in `Reload` mode: a refused native `Seek`
        // leaves the engine flushed and re-posts the target for exactly this branch to reload at.
        && !(eng.flushed && eng.rebase_pending && !seek_reloads(seek_mode()))
        && eng.stage >= Stage::Playing
        && SHARED.duration_ns.load(Relaxed) > 0
        && SHARED.file_size.load(Relaxed) > 0
    {
        TX.seek_to_ns.store(-1, Relaxed);
        let coalesced = take_coalesced(); // read here so BOTH branches consume this seek's requests
        let t = t.max(0);
        // IN-PLACE seek — pause, av_seek the demuxer and, in feed_stream on the first
        // post-seek keyframe, re-anchor the GStreamer segment WITHOUT a reload/decoder re-init (no
        // A/V-resync glitch): `Segment` = flush here + setTimeToDecode + sendSegmentEvent (Kodi),
        // `NativeSeek` = LG's public Seek(millis), which flushes by itself. A plain flush()+refeed
        // with NO fresh segment left a STALE GStreamer segment → the HW sink stops draining ~48 s
        // later → permanent BufferFull + "Playing error" (root-caused by decompiling
        // libpipeline/lxvideosink; see engine::reload_at). `Reload` mode — selected for old-ABI
        // libraries, or set by feed_stream when sendSegmentEvent can't find the pipeline or a
        // native Seek is refused — is reload-per-seek. reload_at REPLACES the ENGINE, so `eng`
        // dangles after it: return immediately.
        if seek_reloads(seek_mode()) {
            own_seek_under_pause(t);
            settle_reload(super::engine::reload_at(ps, pa, t), "direct seek reload");
            return;
        }
        // Pause, user-held seek reuse and eventual prime all share one actuator state. In
        // particular a seek from Paused must not issue a redundant native Pause, and a result from
        // an older transition must not authorize flushing this session's queues.
        match SHARED.prepare_seek_pause() {
            Some(SeekPause::AlreadyHeld) => {}
            Some(SeekPause::Issue(token)) => {
                let accepted = unsafe { super::sink().pause(mt) } != 0;
                match SHARED.complete_seek_pause(token, accepted) {
                    PauseCompletion::Accepted => {}
                    PauseCompletion::Refused => {
                        super::log("seek(in-place): Starfish refused Pause → reload fallback");
                        own_seek_under_pause(t);
                        settle_reload(
                            super::engine::reload_at(ps, pa, t),
                            "seek reload after native Pause refusal",
                        );
                        return;
                    }
                    PauseCompletion::Stale => {
                        super::log("seek(in-place): stale Pause result → reload fallback");
                        own_seek_under_pause(t);
                        settle_reload(
                            super::engine::reload_at(ps, pa, t),
                            "seek reload after stale Pause",
                        );
                        return;
                    }
                }
            }
            None => {
                // The main-thread transition in flight will settle before the next pump tick. Keep
                // the newest seek mailbox intact rather than consuming it into no operation.
                TX.seek_to_ns.store(t, Release);
                TX.seek_reqs.store(1, Release);
                super::log("seek(in-place): clock transition busy; retrying");
                return;
            }
        }
        if !SHARED.begin_native_media_discontinuity(eng.native_epoch) {
            super::log("seek(in-place): native callback epoch retired → reload fallback");
            own_seek_under_pause(t);
            settle_reload(
                super::engine::reload_at(ps, pa, t),
                "seek reload after retired native epoch",
            );
            return;
        }
        // `NativeSeek` does not flush here: LG's public `Seek` flushes the pipeline itself when
        // `feed_stream` calls it on the landed keyframe, and a `flush()` is itself a seek to 0, so
        // flushing after (or instead of re-anchoring) leaves the stale segment at 0 (#568).
        if seek_mode() != SeekMode::NativeSeek {
            unsafe {
                super::sink().flush(mt); // drop decoded/queued frames only after the clock is really held
            }
        }
        drain_aq(eng);
        // in-place direct-play seek: publish the target and let the DEMUX THREAD av_seek_frame
        // between two reads (ff.rs's inner loop). Nothing here interrupts the socket: the pump
        // used to shutdown(2) it to break the read so the outer loop would reopen+seek, but our
        // AVIO is seekable, so libavformat just healed the broken read through `seek_cb` and read
        // on — no reopen, no seek, and every direct-play seek escalated to a reload. See ff.rs.
        // eng.flushed → feed_stream fires setTimeToDecode+sendSegmentEvent on the first post-seek
        // keyframe. disp_base=0 (rebase to the landed keyframe).
        eng.flushed = true;
        eng.presentation_rearm_pending = false;
        SHARED.seek_to_ns.store(t, Release); // the demux thread's av_seek target
        SHARED.seek_target_ns.store(t, Relaxed); // rebase guard: reject stale drifted keyframes
        SHARED.disp_base.store(0, Relaxed);
        super::log(&format!(
            "seek(in-place): av_seek t={t} coalesced={coalesced}"
        ));
        // zero-base the fed timeline on the first post-seek keyframe (feed_stream), so it
        // presents against the flush-reset clock immediately — no catch-up freeze
        eng.rebase_pending = true;
        eng.rebase_drops = 0;
        // A seek armed under a viewer Pause: the pump opens the preroll and closes it on the landing.
        own_seek_under_pause(t);
        eng.seek_armed_at = now; // stuck-watchdog start (see the coalesce/escape above)
        eng.seek_retries = 0;
        eng.max_fed_video_pts = 0;
        eng.max_fed_audio_pts = 0;
        eng.audio_start_fp = None;
        // Drop any AU held from BEFORE the seek (the per-lane BufferFull retry). drain_aq cleared the
        // QUEUES but not these pending AUs; feeding a stale pre-seek AU after the seek re-bases it to
        // the NEW pts_shift → a big A/V desync — the stale audio pts advances max_fed_audio_pts, then
        // the STALE_BACKJUMP guard drops the fresh post-seek audio as "too far back" → no sound. The
        // reopened stream supplies fresh post-seek AUs.
        eng.pending_video = None;
        eng.pending_audio = None;
        eng.prime_play = true; // paused above; Play once PRIME_NS is buffered (no fast-forward)
                               // "no post-seek frame presented yet" — the feed-ahead throttle feeds freely until the first
                               // real presented pts lands, instead of comparing the new fed pts against the STALE pre-seek
                               // presented position (which would wrongly break feeding on a forward in-place seek).
        SHARED.pres_fed.store(super::engine::PRES_NONE, Relaxed);
        eng.feed_pace = super::engine::FeedPace::default(); // the run-on belongs to the pre-seek report
        SHARED.frames.store(0, Relaxed); // count only POST-seek frames (rebind + resume re-pause gate)
                                         // …and forget the last presentation stamp with it. The plane goes dark across a seek by
                                         // design, so the first frame after one would otherwise post a gap the size of the seek and
                                         // read as a stutter (`Shared`'s `dg_vpres_*` block).
        SHARED.dg_vpres_at.store(0, Relaxed);
        SHARED.playpos_ns.store(t, Relaxed); // displayed position jumps; wall clock takes over
    }

    // ---------- load -> Play (decode fed frames as soon as loaded) ----------
    // issue #74 D.1: the gate below requires `SHARED.native_load_returned(eng.native_epoch)`
    // BEFORE this arm may even poll `sf_is_load_completed` — that poll is itself a concurrent
    // Starfish call, and on v0.6.0 it was the FIRST thing this arm did while `sf_load` was still
    // executing (synchronously) on the load thread. See `src/starfish.c`'s `g_load_returned` for
    // the matching seam-side half; `threads::load_thread` flips the Rust side right after the
    // real, blocking `sf_load` call returns. Gated on `!eng.preview_abandon` too, same as the
    // completion arm below: an abandoned trailer-preview Load must not drive any of this — no
    // deferral logging, no budget clock, nothing that reacts to it as if it were live. That is
    // why the guard is checked BEFORE `native_load_gate_ready` is even called: the helper is
    // where the deferral logging and D.1.4 budget actually live (see its own doc).
    if eng.stage == Stage::Loading
        && !eng.preview_abandon
        && native_load_gate_ready(eng.native_epoch)
        && (SHARED.load_completed.load(Relaxed) || unsafe { super::sink().is_load_completed(mt) } != 0)
    {
        // "native: Load returned after Nms" is logged unconditionally at the gate transition
        // itself (`threads::load_thread`, right where `mark_native_load_returned` flips it), not
        // here — see `load-returned-log-is-conditional-on-loadcompleted-and-unbounded-after`. It
        // used to live in this arm, which meant it only ever appeared paired with the deferral
        // line above, and never at all on the path this fix is most likely to be read on: Load
        // eventually returns but `loadCompleted` goes quiet, so this arm never runs.
        SHARED.load_completed.store(true, Relaxed);
        // when it completed, so the panel can say how long we have been waiting for a frame since
        SHARED.dg_load_at.store(now, Relaxed);
        super::log("SMP loadCompleted");
        eng.stage = Stage::Playing;
        // webOS 5+: place the exported window HERE, the instant Load reports success — which is
        // exactly where ss4s does it (smp_player.c calls StarfishResourcePostLoad synchronously
        // on the line after StarfishMediaAPIs_load returns true).
        //
        // It used to wait for `frames >= 2`, copied from the ACB block below, and that is a
        // DEADLOCK on this path: the pipeline does not emit frames until its sink is bound, and
        // the sink is not placed until frames arrive. The symptom is a player stuck in buffering
        // forever, which is what webOS 6 and 10 reported. The gate is right for ACB and only for
        // ACB — `setMediaVideoData` there needs the sourceInfo envelope, which cannot exist until
        // the pipeline has produced something. The exported window has no such dependency: the
        // BINDING already happened via `option.windowId` in the Load payload, and this call is
        // pure geometry.
        place_exported(mt, eng);
        if super::sink().window_mode() == super::VP_EXPORTED {
            // Playing -> Streaming directly: there is no bind sequence to sit in the middle of.
            // The transition is load-bearing beyond bookkeeping — `pushEOS` is gated on
            // `>= Streaming`, so without it the last frames never drain and Up Next never fires.
            eng.stage = Stage::Streaming;
        }
        // A fresh Load for a seek/resume primes before Play so the clock does not free-run through
        // the demux reopen gap. Initial play from the start keeps its immediate-start behavior.
        if eng.rebase_pending {
            eng.prime_play = true;
            super::log("SMP loadCompleted (priming before Play)");
        } else {
            if let Some(token) = SHARED.reserve_prime_play(PrimeKind::Fresh) {
                let accepted = unsafe { super::sink().play(mt) } != 0;
                match SHARED.complete_prime_play(token, accepted) {
                    super::shared::PlayCompletion::Accepted { resume_acb } => {
                        if resume_acb {
                            super::acb_mirror_playstate_at(mt, eng.stage, true);
                        }
                        super::log("SMP Play");
                    }
                    super::shared::PlayCompletion::Refused => {
                        eng.prime_play = true;
                        super::log("SMP Play refused; priming for retry");
                    }
                    super::shared::PlayCompletion::Stale => {
                        eng.prime_play = true;
                        super::log("SMP Play result stale; priming for retry");
                    }
                }
            } else {
                eng.prime_play = true;
                super::log("SMP Play fenced; priming before retry");
            }
        }
    } else if eng.stage == Stage::Loading
        && !eng.preview_abandon
        && SHARED.native_load_returned(eng.native_epoch)
    {
        // Load HAS returned here (the first arm's `native_load_gate_ready` failed) and
        // `loadCompleted` has NOT arrived (the previous arm's condition also failed) — the other
        // half of issue #74 D.1's budget, and the one `load-returned-log-is-conditional-on-
        // loadcompleted-and-unbounded-after` found unbounded: on v0.6.0 and unchanged until now,
        // a Load that returns ok=1 and then never signals `loadCompleted` left the player parked
        // in Connecting forever, with no counterpart to the D.1.4 timeout above. Gated on
        // `!eng.preview_abandon` too: an abandoned trailer-preview Load has nobody watching a
        // failure read-out for, and must not publish `load_failed`/`load_timed_out` either.
        // Budget it from
        // the moment Load RETURNED, not from when it was ISSUED — `native_load_elapsed` (used
        // above) never resets, so a Load that used most of its own budget getting here would
        // otherwise leave this second wait almost none of its own.
        match SHARED.native_load_returned_elapsed(eng.native_epoch) {
            Some(elapsed) if elapsed >= NATIVE_LOAD_BUDGET => {
                super::log(&format!(
                    "native: loadCompleted did not arrive within {}s after Load returned — \
                     publishing load_failed",
                    NATIVE_LOAD_BUDGET.as_secs()
                ));
                SHARED.load_failed.store(true, Release);
                SHARED.load_timed_out.store(true, Release);
            }
            Some(_) => {}
            // Same rationale as the `None` arm above: losing `Active` (a synchronous
            // UnloadCompleted for this epoch) while still waiting on `loadCompleted` must fire
            // the timeout rather than silently going quiet forever.
            None => {
                super::log(
                    "native: Load epoch left Active while still waiting on loadCompleted after \
                     Load returned — publishing load_failed",
                );
                SHARED.load_failed.store(true, Release);
                SHARED.load_timed_out.store(true, Release);
            }
        }
    }

    // ---------- ACB bind, Kodi/ss4s order: setSinkType(MAIN)+setMediaId+setState(LOADED) ----------
    if eng.stage == Stage::Playing && ACB_OK.load(Relaxed) {
        let id = SHARED.media_id.lock().unwrap().clone();
        if let Some(id) = id {
            unsafe { super::sink().plane_bind(mt, id.as_ptr()) };
            super::log(&format!("SMP ACB bound id={}", id.to_string_lossy()));
            // **Dolby Atmos, to ACB, exactly here** — this is where LG's own client fires it
            // (`libcbe` 0x1b98d78, on LOADCOMPLETED): after setMediaId, and BEFORE the
            // frame-gated setMediaVideoData below. It needs no decoded frame and reads no
            // state; `context` is the `id` we have in hand, which is the whole reason the call
            // exists rather than a forward of the pipeline's own AUDIO_INFO callback (that
            // string carries `track` and `dualMono` and no context). See `acb_send_atmos`.
            //
            // **This looks like it breaks the "never feed audio to ACB" rule and it does not** —
            // the rule is about the audio ELEMENTARY STREAM, which the pipeline owns and which we
            // still never hand ACB. This is a two-key metadata descriptor, and the distinction is
            // now readable rather than remembered (see `acb_send_atmos` in `src/starfish.c` for
            // the addresses). The rule's stated consequence, `SOUND_ERROR_019`, is a literal that
            // exists in NO library on this device.
            //
            // Ran behind `/tmp/webosiptv-atmosacb` first, then measured, then made the default:
            // `rv=1` accepted, 1600 audio AUs fed with `reply=O` and no error of any kind, and the
            // television's own read-out — "Dolby Vision / Dolby Atmos", both lines — photographed
            // in a DISPLAY capture at 11 s. `/tmp/webosiptv-noatmosacb` is the way back out.
            if ps.stream().atmos && !tv_base::devtrig::flag("noatmosacb") {
                let rv = unsafe { super::sink().plane_send_atmos(mt, id.as_ptr()) };
                super::log(&format!("atmos: acb setMediaAudioData rv={rv}"));
            }
            eng.stage = Stage::Bound;
        }
    }

    // ---------- send the WHOLE sourceInfo envelope once frames flow (verbatim, except a
    // container-PQ VP9 stream, see below), then
    // window + PLAYING (setMediaVideoData -> setDisplayWindow -> setState PLAYING) ----------
    if let Some(at) = eng.bound_at_frames {
        // The tick the landing owes after the bind has arrived (or `frames` was reset since).
        if SHARED.frames.load(Relaxed) != at {
            eng.bound_at_frames = None;
        }
    }
    if eng.stage == Stage::Bound
        && !eng.video_info_sent
        && SHARED.frames.load(Relaxed) >= acb_bind_frames_needed(TX.seek_preroll_active())
    {
        let bytes = SHARED.source_info.lock().unwrap().clone();
        if let Some(mut bytes) = bytes {
            // A VP9 stream's PQ lives in the container, which the pipeline never sees, so its own
            // envelope says "hdrType":"none". Say HDR10 here when the demuxer found PQ (see
            // `vp9_hdr`). `/tmp/webosiptv-novp9hdr` sends the envelope as the pipeline made it.
            let container_hdr = *SHARED.container_hdr.lock().unwrap();
            if let Some(h) = container_hdr {
                if !tv_base::devtrig::flag("novp9hdr") {
                    if let Some(rewritten) = super::vp9_hdr::rewrite_source_info(&bytes, &h) {
                        super::log(&super::vp9_hdr::log_line(&h));
                        bytes = rewritten;
                    }
                }
            }
            let rv = unsafe { super::sink().plane_send_video_data(mt, bytes.as_ptr() as *const c_char) };
            super::log(&format!(
                "setMediaVideoData rv={rv} frames={}",
                SHARED.frames.load(Relaxed)
            ));
            if rv != -1 {
                // -1 = client-side isJsonError reject; else accepted
                eng.video_info_sent = true;
                let r = super::video_rect();
                unsafe { super::sink().plane_start(mt, r.x.into(), r.y.into(), r.width.into(), r.height.into()) };
                eng.stage = Stage::Streaming;
                eng.bound_at_frames = Some(SHARED.frames.load(Relaxed));
                // `acb_start` sets ACB PLAYING. Under a pipeline that is really paused (a late
                // tick bound the plane after the re-pause; an open preroll is a PLAYING pipeline,
                // and its re-pause mirrors the pause itself) ACB must follow it, or it is left
                // PLAYING over a paused pipeline and nothing ever tells it otherwise.
                if TX.paused.load(Acquire) && !TX.seek_preroll_active() {
                    unsafe { super::sink().plane_pause(mt) };
                    super::log("setMediaVideoData sent under a paused pipeline → ACB PAUSED");
                }
                super::log("setMediaVideoData sent → window+PLAYING");
            }
        }
    }

    // ---------- webOS 5+ (VP_EXPORTED): correct the placement if the real frame size only
    // became known after Load. The demuxer publishes the coded size when it opens the stream,
    // which races the load thread — so the placement above may have used the fallback. Re-placing
    // costs one call, and Kodi re-places on every render-area change anyway. The viewer's aspect
    // mode moving (`…` > Aspect ratio) is the same kind of change.
    // ----------
    if eng.stage >= Stage::Playing && eng.placed_src != (0, 0) {
        let now = SHARED.video_raster();
        let raster_moved = now.0 > 0 && now.1 > 0 && now != eng.placed_src;
        let rect_moved = PLACED_RECT.load(Relaxed) != super::video_rect_key();
        if raster_moved || rect_moved || PLACED_MODE.load(Relaxed) != super::aspect_mode().index() {
            place_exported(mt, eng);
        }
    }

    // ---------- feed AUs after the initial Load has reached Playing. The native clock may itself
    // be Running or Paused: ResumePrime deliberately fills both lanes before issuing Play. NOT
    // while a seek is armed: on a resume the seek is armed before PLAYING, so feeding first would
    // present the file start for a frame before the seek repositions — a visible jump. ----------
    if eng.stage >= Stage::Playing && TX.feed_allowed() && TX.seek_to_ns.load(Relaxed) < 0 {
        if stream {
            // Two-lane feed, then the prime attempt — the ordering and the reason both live in
            // `feed_both_lanes`, so this call site cannot drift from them.
            tv_base::diag::spans::span("feed", || feed_both_lanes(mt, eng));
        } else {
            tv_base::diag::spans::span("feed", || feed_sample(mt, eng));
        }
    }

    // ---------- end-of-stream: the producer hit file EOF (eos_pushed → all AUs to the end were
    // fed) and the pipeline has now played out to within EOS_TAIL of the duration. Mark ended so
    // app.rs tears the player down at the credits instead of freezing on the last frame. Paused
    // at the end stays paused (playpos won't climb) — correct; it fires when resumed. Any seek
    // clears the flag (request_seek), so seeking back from the end doesn't re-trigger. ----------
    // ---------- publish the derived UI state (see PlaybackState's doc) ----------
    // Order matters: a seek in flight outranks "we have frames", because the frames on the panel
    // are the PRE-seek ones and the HUD must show the target, not them.
    set_state(if SHARED.seeking.load(Relaxed) {
        PlaybackState::Seeking
    } else if eng.stage == Stage::Loading {
        PlaybackState::Connecting
    } else if SHARED.frames.load(Relaxed) == 0 {
        // No picture yet. Work pending for one is Buffering; a paused player with nothing in
        // flight is waiting on the viewer, which is neither a spinner nor `Playing` (no frame is
        // presented, so `is_playing` and `PlaybackStarted` must stay false).
        if paused_and_idle() { PlaybackState::Paused } else { PlaybackState::Buffering }
    } else {
        PlaybackState::Playing
    });

    const EOS_TAIL_NS: i64 = 1_000_000_000;
    if !SHARED.ended.load(Relaxed) {
        let dur = SHARED.duration_ns.load(Relaxed);
        let pos = SHARED.playpos_ns.load(Relaxed);
        let demux_done = eng.aq_video.as_mut().is_some_and(|q| crate::aq::aq_producer_done(&mut **q));
        if eng.eos_pushed && dur > 0 && pos >= dur - EOS_TAIL_NS {
            SHARED.ended.store(true, Relaxed);
            super::log(&format!(
                "EOS reached: playpos={}s/{}s → ended",
                pos / 1_000_000_000,
                dur / 1_000_000_000
            ));
        } else if !demux_done || TX.paused.load(Acquire) || SHARED.seeking.load(Relaxed) {
            eng.eos_stall = None; // still reading, or the playhead is held by the viewer
        } else if eos_stalled(&mut eng.eos_stall, pos, now) {
            // The whole file has been read and the playhead has stopped short of its end: nothing
            // more will present. End to the stream list rather than show "Buffering" forever.
            SHARED.ended.store(true, Relaxed);
            super::log(&format!(
                "EOS stalled: playpos={}ms/{}ms unchanged for {}s after the demux ended (eos_pushed={}) → ended",
                pos / 1_000_000,
                dur / 1_000_000,
                EOS_STALL_MS / 1000,
                eng.eos_pushed
            ));
        }
    }
}

/// How long a playhead may stand still once the demux has ended before the playback is ended anyway.
const EOS_STALL_MS: u32 = 10_000;

/// Whether the playhead `pos` has stood still for [`EOS_STALL_MS`] at `now`; `seen` is the
/// position last seen and when it first was.
fn eos_stalled(seen: &mut Option<(i64, u32)>, pos: i64, now: u32) -> bool {
    match *seen {
        Some((p, at)) if p == pos => now.wrapping_sub(at) >= EOS_STALL_MS,
        _ => {
            *seen = Some((pos, now));
            false
        }
    }
}

#[cfg(test)]
mod eos_stall_tests {
    #[test]
    fn a_playhead_standing_still_after_eos_ends_only_after_the_stall_window() {
        let mut seen = None;
        assert!(!super::eos_stalled(&mut seen, 5, 1_000));
        assert!(!super::eos_stalled(&mut seen, 5, 1_000 + super::EOS_STALL_MS - 1));
        assert!(super::eos_stalled(&mut seen, 5, 1_000 + super::EOS_STALL_MS));
        // a moving playhead restarts the window
        assert!(!super::eos_stalled(&mut seen, 6, 20_000));
        assert!(!super::eos_stalled(&mut seen, 6, 20_000 + super::EOS_STALL_MS - 1));
    }
}


/// #568, at the PUMP level: a direct-play seek on a `VP_EXPORTED` (webOS 5+) session, driven by
/// the real `pump` ticks and the real `feed_stream` against the host sink with the clock sink
/// armed (which reports `VP_EXPORTED`). `reload_at` has no route to reload in a unit rig (an empty
/// URL is ignored), so each call is read off `engine::RELOAD_AT_LOG`; the rig stands in for the
/// reload's EFFECT (a new Engine) where the next seek needs one.
#[cfg(test)]
mod stuck_timeout_tests {
    use super::*;

    #[test]
    fn the_stuck_timeout_is_the_floor_plus_three_reopens_capped() {
        assert_eq!(stuck_ms_for(0), SEEK_STUCK_MS, "no sample yet: today's value");
        assert_eq!(stuck_ms_for(100), SEEK_STUCK_MS + 300);
        assert_eq!(stuck_ms_for(330), 2_190, "a ~330 ms debrid reopen");
        assert_eq!(stuck_ms_for(5_000), SEEK_STUCK_CAP_MS);
        assert_eq!(stuck_ms_for(u32::MAX), SEEK_STUCK_CAP_MS);
    }
}

#[cfg(all(test, feature = "hostsim"))]
mod native_seek_pump_tests {
    use super::*;
    use crate::player::ffi_host;
    use crate::player::SeekMode;
    use std::os::raw::c_int;

    const TARGET_NS: i64 = 905_000_000_000;
    const NEXT_NS: i64 = 1_200_000_000_000;
    const V_STEP_NS: i64 = 41_708_333;

    struct Rig {
        ps: crate::session::PlaybackSession,
        pa: super::super::adapter::PlayerAdapter,
        now: u32,
    }

    impl Rig {
        fn new(force_seek: Option<c_int>) -> Rig {
            SHARED.reset_session();
            TX.reset();
            ffi_host::reset_native_lifecycle_for_test();
            ffi_host::force_clocksink_for_test(true);
            ffi_host::force_object_ready_for_test(true);
            ffi_host::reset_seek_probe_for_test(force_seek);
            super::super::engine::RELOAD_AT_LOG.lock().unwrap_or_else(|e| e.into_inner()).clear();
            super::super::engine::RELOAD_PREROLL_LOG.lock().unwrap_or_else(|e| e.into_inner()).clear();
            crate::player::set_native_seek_refused(false);
            let mut pa = super::super::adapter::PlayerAdapter::new(unsafe { MainThread::assume() });
            let epoch = SHARED.begin_native_session().expect("native session");
            let mut eng = super::super::engine::streaming_engine_for_test(epoch);
            eng.stage = Stage::Playing;
            pa.install(eng);
            SHARED.duration_ns.store(7_200_000_000_000, Relaxed);
            SHARED.file_size.store(50_000_000_000, Relaxed);
            SHARED.playpos_ns.store(60_000_000_000, Relaxed);
            crate::player::set_seek_mode(SeekMode::NativeSeek);
            Rig { ps: crate::session::PlaybackSession::IDLE, pa, now: 1_000 }
        }

        fn frame(&mut self) {
            let _frame = tv_base::task::FrameScope::enter();
            self.now += 16;
            pump(&mut self.ps, &mut self.pa, self.now);
        }

        fn eng(&mut self) -> &mut Engine {
            self.pa.engine().expect("engine")
        }

        /// The demux thread's job: the first keyframe at `ns` and the AUs after it.
        fn land_keyframe(&mut self, ns: i64) {
            self.land_keyframe_aus(ns, 60);
        }

        /// [`Self::land_keyframe`] with `n` AUs: the prime wants `PRIME_VIDEO_MAX_NS` (2.5 s) of
        /// accepted video on an audioless stream, which is 61 of these.
        fn land_keyframe_aus(&mut self, ns: i64, n: i64) {
            // The demux thread took the av_seek mailbox before it produced the keyframe.
            SHARED.seek_to_ns.store(-1, Release);
            let au = [0u8; 64];
            let qv = &mut **self.eng().aq_video.as_mut().unwrap() as *mut crate::aq::AuQueue;
            for i in 0..n {
                crate::aq::aq_push(qv, au.as_ptr(), au.len() as c_int, ns + i * V_STEP_NS, c_int::from(i == 0), 1);
            }
        }

        /// Request a seek and frame until the pump has armed it (flushed, awaiting the keyframe).
        fn arm(&mut self, ns: i64) {
            crate::player::request_seek(ns);
            for _ in 0..20 {
                self.frame();
                if self.eng().flushed {
                    return;
                }
            }
            panic!("the pump never armed the seek");
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            crate::player::set_native_seek_refused(false);
            crate::player::set_seek_mode(SeekMode::Segment);
            ffi_host::reset_seek_probe_for_test(None);
            ffi_host::reset_native_lifecycle_for_test();
            ffi_host::force_clocksink_for_test(false);
            ffi_host::force_object_ready_for_test(false);
            SHARED.test_force_native_idle();
            SHARED.reset_session();
            TX.reset();
        }
    }

    fn reloads() -> Vec<i64> {
        super::super::engine::RELOAD_AT_LOG.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// An HLS VOD (a catch-up programme) has no byte addresses: its seek is one reload at the
    /// target, in playlist time.
    #[test]
    fn an_hls_vod_seek_is_a_reload_at_its_playlist_time() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        SHARED.file_size.store(-1, Relaxed);
        SHARED.duration_ns.store(3_600_000_000_000, Relaxed);
        SHARED.hls_vod_base_ns.store(0, Relaxed);
        crate::player::request_seek(42_000_000_000);
        rig.frame();
        assert_eq!(reloads(), vec![42_000_000_000]);
        assert_eq!(TX.seek_to_ns.load(Relaxed), -1, "consumed");
    }

    /// A seek on a live stream (no duration, no playlist time, no byte addresses) can never be
    /// served; held, it would stop the feed for good. It is dropped and the picture goes on.
    #[test]
    fn a_seek_on_a_live_stream_is_dropped_not_held() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        SHARED.file_size.store(-1, Relaxed);
        SHARED.duration_ns.store(0, Relaxed);
        SHARED.hls_vod_base_ns.store(-1, Relaxed);
        SHARED.seen_frame.store(true, Relaxed);
        crate::player::request_seek(5_000_000_000);
        rig.frame();
        assert_eq!(TX.seek_to_ns.load(Relaxed), -1, "dropped");
        assert!(!SHARED.seeking.load(Relaxed), "no spinner left behind");
        assert!(reloads().is_empty());
        assert!(TX.feed_allowed() && TX.seek_to_ns.load(Relaxed) < 0, "the feed is not held for it");
    }

    #[test]
    fn a_native_seek_lands_on_the_keyframe_without_a_flush_or_a_reload() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        rig.arm(TARGET_NS);
        assert_eq!(SHARED.seek_to_ns.load(Relaxed), TARGET_NS, "the demux thread is told to av_seek");
        rig.land_keyframe(TARGET_NS);
        for _ in 0..5 {
            rig.frame();
        }
        assert_eq!(ffi_host::seek_log_for_test(), vec![TARGET_NS / 1_000_000], "one native Seek, at the keyframe");
        assert_eq!(ffi_host::flush_calls_for_test(), 0, "Seek flushes itself; the pump must not");
        assert_eq!(ffi_host::segment_calls_for_test(), 0, "the private Segment verbs never run on VP_EXPORTED");
        assert!(reloads().is_empty(), "no fresh Load");
        assert!(!rig.eng().flushed && !rig.eng().rebase_pending, "the seek is applied");
        assert_eq!(crate::player::seek_mode(), SeekMode::NativeSeek);
    }

    #[test]
    fn a_refused_seek_reloads_exactly_once_at_the_target_even_past_the_stuck_watchdog() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(Some(0));
        rig.arm(TARGET_NS);
        rig.land_keyframe(TARGET_NS);
        rig.frame(); // the feed refuses the Seek and re-posts the target for the reload
        assert_eq!(ffi_host::seek_log_for_test().len(), 1, "one refused Seek");
        assert_eq!(TX.seek_to_ns.load(Relaxed), TARGET_NS, "re-posted for the reload");
        // Late in the window: the stuck watchdog (SEEK_STUCK_MS) is due on the very next tick, and
        // it runs BEFORE the pending-seek branch.
        rig.now += 5_000;
        for _ in 0..6 {
            rig.frame();
            if !reloads().is_empty() {
                break;
            }
        }
        assert_eq!(ffi_host::seek_log_for_test().len(), 1, "one refused Seek");
        assert_eq!(reloads(), vec![TARGET_NS], "exactly one reload_at, at the user's target");
        assert_eq!(
            SHARED.seek_to_ns.load(Relaxed),
            -1,
            "the demux feed is not re-opened with a second av_seek in Reload mode"
        );
        assert_eq!(crate::player::seek_mode(), SeekMode::Reload);
        assert!(crate::player::native_seek_refused());

        // Another keyframe reaching the flushed engine must not anchor through the private offsets.
        rig.land_keyframe(TARGET_NS + 1_000_000_000);
        for _ in 0..3 {
            rig.eng(); // the real reload replaced the engine; here it is the same, still flushed
            let mt = unsafe { MainThread::assume() };
            super::super::engine::feed_both_lanes(&mt, rig.eng());
        }
        assert_eq!(ffi_host::segment_calls_for_test(), 0, "no setTimeToDecode/setContentInfo/sendSegment, ever");
        assert_eq!(ffi_host::flush_calls_for_test(), 0);
    }

    #[test]
    fn once_refused_the_same_playback_reloads_every_later_seek_without_calling_seek() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(Some(0));
        rig.arm(TARGET_NS);
        rig.land_keyframe(TARGET_NS);
        for _ in 0..3 {
            rig.frame();
        }
        assert_eq!(ffi_host::seek_log_for_test().len(), 1);
        assert_eq!(reloads(), vec![TARGET_NS]);

        // The reload starts a NEW session in the same playback: it must not re-offer the Seek.
        let mode = super::super::engine::session_seek_mode(true, crate::player::VP_EXPORTED, None);
        assert_eq!(mode, SeekMode::Reload, "the downgrade survives reload_at");
        crate::player::set_seek_mode(mode);
        // Stand in for the reload's new Engine.
        let epoch = SHARED.begin_native_session().unwrap_or(0);
        let mut fresh = super::super::engine::streaming_engine_for_test(epoch);
        fresh.stage = Stage::Playing;
        rig.pa.install(fresh);
        crate::player::request_seek(NEXT_NS);
        for _ in 0..4 {
            rig.frame();
        }
        assert_eq!(reloads(), vec![TARGET_NS, NEXT_NS], "the next seek is a reload");
        assert_eq!(ffi_host::seek_log_for_test().len(), 1, "Seek is not called again this playback");
        assert_eq!(ffi_host::segment_calls_for_test(), 0);

        // A fresh play is a new playback and gets to try the native Seek again.
        let _ = crate::player::start_bufferfeed_tracked(&mut rig.ps, &mut super::super::adapter::PlayerAdapter::new(unsafe { MainThread::assume() }));
        assert!(!crate::player::native_seek_refused(), "a fresh play clears the latch");
    }

    // ---- a seek armed under a viewer Pause is owned by the media layer (a bare request_seek) ----

    impl Rig {
        /// The viewer pauses through the real path; the clock is fresh so the Pause is accepted.
        fn viewer_pause(&mut self) {
            SHARED.reset_clock_for_test();
            assert!(crate::player::pause(&mut self.pa), "viewer Pause accepted");
            assert!(TX.paused.load(Acquire));
        }
    }

    fn viewer_paused_seek_lands_in_place(mode: SeekMode) {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        crate::player::set_seek_mode(mode);
        rig.viewer_pause();
        rig.arm(TARGET_NS); // a bare request_seek, as the dev autoseek trigger does
        assert!(TX.seek_preroll_active(), "the pump opened the paused-seek preroll itself");
        rig.land_keyframe(TARGET_NS);
        for _ in 0..5 {
            rig.frame();
        }
        assert!(!rig.eng().flushed && !rig.eng().rebase_pending, "the keyframe reached feed_stream's rebase");
        match mode {
            SeekMode::NativeSeek => {
                assert_eq!(ffi_host::seek_log_for_test(), vec![TARGET_NS / 1_000_000], "one Seek, at the keyframe");
            }
            _ => assert!(ffi_host::segment_calls_for_test() > 0, "the segment was anchored on the keyframe"),
        }
        rig.now += 5_000; // well past SEEK_STUCK_MS
        for _ in 0..3 {
            rig.frame();
        }
        assert!(reloads().is_empty(), "no stuck-watchdog reload");
        // The first post-seek frame is presented: the pump re-pauses and restores the viewer hold.
        SHARED.frames.store(1, Relaxed);
        rig.frame();
        assert!(!TX.seek_preroll_active(), "the preroll closed on the landing");
        assert!(TX.paused.load(Acquire), "the viewer is still paused");
        assert!(!ffi_host::clock_state_for_test().1, "the physical clock is held again");
        assert!(reloads().is_empty());
    }

    #[test]
    fn a_bare_native_seek_while_viewer_paused_lands_in_place_and_ends_paused() {
        viewer_paused_seek_lands_in_place(SeekMode::NativeSeek);
    }

    #[test]
    fn a_bare_segment_seek_while_viewer_paused_lands_in_place_and_ends_paused() {
        viewer_paused_seek_lands_in_place(SeekMode::Segment);
    }

    #[test]
    fn a_bare_paused_request_seek_is_owned_by_the_dispatcher_not_the_mid_seek_branch() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        rig.viewer_pause();
        SHARED.seek_target_ns.store(123_000_000_000, Relaxed); // a stale target from an earlier seek
        MID_SEEK_CLAIMS.store(0, Relaxed);
        crate::player::request_seek(TARGET_NS);
        rig.frame();
        assert_eq!(
            MID_SEEK_CLAIMS.load(Relaxed),
            0,
            "a seek not yet dispatched is not a seek the viewer paused mid-flight"
        );
        assert_eq!(TX.owned_paused_seek(), Some(TARGET_NS), "first owned at the request's own target");
    }

    #[test]
    fn a_pause_pressed_mid_flight_takes_the_mid_seek_branch() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        rig.arm(TARGET_NS);
        MID_SEEK_CLAIMS.store(0, Relaxed);
        assert!(crate::player::pause(&mut rig.pa));
        rig.frame();
        assert_eq!(MID_SEEK_CLAIMS.load(Relaxed), 1);
    }

    /// A slow debrid host: each range reopen measured ~2 s. The watchdog waits the floor plus three
    /// of them (7.2 s) before retrying, where it used to retry after 1.2 s whatever the server.
    #[test]
    fn the_stuck_watchdog_scales_with_the_measured_reopen_latency() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        SHARED.reopen_ms_est.store(2_000, Relaxed);
        rig.arm(TARGET_NS);
        SHARED.seek_to_ns.store(-1, Release); // the demuxer took the mailbox and landed
        rig.now += 5_000;
        rig.frame();
        assert_eq!(SHARED.seek_to_ns.load(Relaxed), -1, "no retry inside the scaled timeout");
        rig.now += 3_000;
        rig.frame();
        assert_eq!(SHARED.seek_to_ns.load(Relaxed), TARGET_NS, "the retry once it has passed");
        assert!(reloads().is_empty());
    }

    /// The field log: a Matroska file's first seek spent 2966 ms inside `av_seek_frame` (Cues at
    /// 102 GB, then back), the watchdog retried twice into it and then reloaded, and the reload's
    /// teardown aborted the reopen that was landing (`curlio: seek … failed: Aborted`). A seek in
    /// flight is now never retried or reloaded, and a target that arrived meanwhile is applied
    /// once it has landed — nothing aborted, one av_seek per target.
    #[test]
    fn a_seek_in_flight_is_never_retried_or_reloaded_and_a_newer_target_follows_its_landing() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        rig.arm(TARGET_NS);
        // The demux thread takes the mailbox and enters av_seek_frame (ff::demux's order).
        SHARED.demux_seeking.store(true, SeqCst);
        SHARED.seek_to_ns.store(-1, SeqCst);
        crate::player::request_seek(NEXT_NS); // the viewer seeks again meanwhile
        for _ in 0..20 {
            rig.now += 1_000;
            rig.frame();
        }
        assert!(reloads().is_empty(), "no reload tears down the seek in flight");
        assert_eq!(SHARED.seek_to_ns.load(Relaxed), -1, "nothing is published into the seek in flight");
        assert_eq!(SHARED.seek_target_ns.load(Relaxed), TARGET_NS);
        SHARED.demux_seeking.store(false, SeqCst); // landed
        rig.frame();
        assert_eq!(SHARED.seek_to_ns.load(Relaxed), NEXT_NS, "retargeted at the newest request");
        assert_eq!(SHARED.seek_target_ns.load(Relaxed), NEXT_NS, "the rebase guard keys on it");
        assert_eq!(TX.seek_to_ns.load(Relaxed), -1, "the request was consumed");
        rig.frame();
        assert_eq!(SHARED.seek_to_ns.load(Relaxed), NEXT_NS, "and retargeted once, not again per tick");
        rig.land_keyframe(NEXT_NS);
        for _ in 0..5 {
            rig.frame();
        }
        assert_eq!(ffi_host::seek_log_for_test(), vec![NEXT_NS / 1_000_000], "anchored at the newest target");
        assert!(reloads().is_empty());
    }

    #[test]
    fn a_gate_shut_under_an_armed_seek_is_reopened_and_the_watchdog_counts_from_the_reopen() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        rig.viewer_pause();
        rig.arm(TARGET_NS);
        TX.finish_seek_preroll(); // something closed the preroll while the keyframe was still owed
        assert!(!TX.feed_allowed());
        rig.now += 5_000; // far past SEEK_STUCK_MS
        rig.frame();
        assert!(TX.feed_allowed(), "the pump reopens the gate rather than retry or reload into it");
        assert!(reloads().is_empty(), "no reload on the tick the gate reopened");
        assert_eq!(SHARED.seek_to_ns.load(Relaxed), TARGET_NS, "nor a retry re-publishing the av_seek");
        for _ in 0..3 {
            rig.frame();
        }
        assert!(reloads().is_empty(), "the clock was re-based by the reopen");
        rig.now += 5_000;
        rig.frame();
        assert_eq!(SHARED.seek_to_ns.load(Relaxed), TARGET_NS, "first stuck retry once the feed was open");
    }

    #[test]
    fn the_pump_owned_preroll_dies_with_the_playback() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        rig.viewer_pause();
        rig.arm(TARGET_NS);
        assert_eq!(TX.owned_paused_seek(), Some(TARGET_NS));
        TX.reset(); // a stop
        assert!(TX.owned_paused_seek().is_none() && !TX.seek_preroll_active());
        // …and a Resume that closes the preroll drops the ownership with it.
        TX.commit_paused(true);
        assert!(TX.own_paused_seek(1));
        TX.finish_seek_preroll();
        assert!(TX.owned_paused_seek().is_none());
    }

    #[test]
    fn a_preroll_a_caller_opened_stays_that_callers_to_close() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        rig.viewer_pause();
        assert!(TX.begin_paused_seek(), "the foreground restore opens its own");
        rig.arm(TARGET_NS);
        assert!(TX.owned_paused_seek().is_none(), "the caller's preroll is not the pump's");
        rig.land_keyframe(TARGET_NS);
        for _ in 0..5 {
            rig.frame();
        }
        SHARED.frames.store(1, Relaxed);
        rig.frame();
        assert_eq!(ffi_host::seek_log_for_test(), vec![TARGET_NS / 1_000_000]);
        assert!(TX.seek_preroll_active(), "the pump leaves a caller's preroll for the caller");
    }

    // ---- the paused-seek lifecycle across EVERY route a seek can take, and across a reload ----

    fn preroll_at_reload() -> Vec<bool> {
        super::super::engine::RELOAD_PREROLL_LOG.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    impl Rig {
        /// What the real `reload_at` does once the session has a stream (this rig has none, so the
        /// real call returns `NoRoute` after logging): `teardown(for_reload)`
        /// resets the session and keeps the transport's Pause and preroll, the fresh Engine arms its
        /// Initial hold from those same two inputs (`engine::ClockStartGuard::arm`), and the new
        /// session waits for its first keyframe at `target`.
        fn complete_reload(&mut self, target: i64) {
            TX.reset_for_reload();
            SHARED.reset_session_for_reload();
            SHARED.reset_clock_for_test();
            assert!(SHARED.arm_initial_clock_hold(TX.paused.load(Acquire), TX.seek_preroll_active()));
            super::super::engine::arm_seek(target);
            let epoch = SHARED.begin_native_session().unwrap_or(0);
            let mut fresh = super::super::engine::streaming_engine_for_test(epoch);
            fresh.stage = Stage::Playing;
            fresh.rebase_pending = true;
            self.pa.install(fresh);
            SHARED.duration_ns.store(7_200_000_000_000, Relaxed);
            SHARED.file_size.store(50_000_000_000, Relaxed);
            SHARED.seeking.store(true, Relaxed);
        }

        /// Frame until `reloads()` has an entry (the dispatcher ran `reload_at`).
        fn frame_until_reload(&mut self) {
            for _ in 0..20 {
                if !reloads().is_empty() {
                    return;
                }
                self.frame();
            }
            panic!("the pump never dispatched a reload");
        }

        /// The fresh session's keyframe lands and its first frame is presented; returns the number of
        /// physical Plays the prime issued meanwhile.
        fn land_and_present(&mut self, ns: i64) -> u64 {
            let plays = ffi_host::play_calls_for_test();
            self.land_keyframe_aus(ns, 90);
            for _ in 0..8 {
                self.frame();
            }
            let issued = ffi_host::play_calls_for_test() - plays;
            SHARED.frames.store(1, Relaxed);
            self.frame();
            issued
        }
    }

    fn assert_ended_paused() {
        assert!(!TX.seek_preroll_active(), "the preroll closed on the landing");
        assert!(TX.paused.load(Acquire), "the viewer is still paused");
        assert!(!ffi_host::clock_state_for_test().1, "the physical clock is held again: the film is not playing");
    }

    #[test]
    fn a_paused_scrub_in_reload_mode_holds_the_preroll_across_the_reload_and_ends_paused() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        crate::player::set_seek_mode(SeekMode::Reload);
        rig.viewer_pause();
        crate::player::request_seek(TARGET_NS);
        rig.frame_until_reload();
        assert_eq!(reloads(), vec![TARGET_NS]);
        assert_eq!(preroll_at_reload(), vec![true], "the fresh Load is dispatched with the feed gate open");
        rig.complete_reload(TARGET_NS);
        assert!(TX.feed_allowed(), "the new session may feed its keyframe");
        let plays = rig.land_and_present(TARGET_NS);
        assert!(plays >= 1, "the prime Play fired through the reopened gate before the frame was presented");
        assert_eq!(reloads(), vec![TARGET_NS], "no second reload");
        assert_ended_paused();
    }

    #[test]
    fn two_consecutive_paused_reload_mode_seeks_each_open_the_preroll_and_end_paused() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        crate::player::set_seek_mode(SeekMode::Reload);
        rig.viewer_pause();
        for target in [TARGET_NS, NEXT_NS] {
            super::super::engine::RELOAD_AT_LOG.lock().unwrap_or_else(|e| e.into_inner()).clear();
            super::super::engine::RELOAD_PREROLL_LOG.lock().unwrap_or_else(|e| e.into_inner()).clear();
            SHARED.frames.store(0, Relaxed);
            crate::player::request_seek(target);
            rig.frame_until_reload();
            assert_eq!(reloads(), vec![target]);
            assert_eq!(preroll_at_reload(), vec![true], "the reload at {target} is dispatched with the gate open");
            rig.complete_reload(target);
            assert!(rig.land_and_present(target) >= 1, "the prime Play fired through the reopened gate");
            assert_eq!(reloads(), vec![target], "no second reload");
            assert_ended_paused();
        }
    }

    // ---- a fresh session under a viewer Pause is primed a first frame whatever reloaded it ----

    impl Rig {
        /// What every non-seek reload does (`switch_audio_native`): the session resets keeping
        /// the transport's Pause, and the fresh Engine arms its Initial hold from it. No seek is
        /// armed and no preroll is opened by the caller.
        fn complete_plain_reload(&mut self) {
            TX.reset_for_reload();
            SHARED.reset_session_for_reload();
            SHARED.reset_clock_for_test();
            assert!(SHARED.arm_initial_clock_hold(TX.paused.load(Acquire), TX.seek_preroll_active()));
            let epoch = SHARED.begin_native_session().unwrap_or(0);
            let mut fresh = super::super::engine::streaming_engine_for_test(epoch);
            fresh.stage = Stage::Playing;
            self.pa.install(fresh);
            SHARED.duration_ns.store(7_200_000_000_000, Relaxed);
            SHARED.file_size.store(50_000_000_000, Relaxed);
            SHARED.playpos_ns.store(TARGET_NS, Relaxed);
        }
    }

    #[test]
    fn a_paused_non_seek_reload_primes_a_first_frame_and_ends_paused() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        SHARED.frames.store(1, Relaxed);
        rig.viewer_pause();
        // An audio-switch reload: no seek, the preroll not opened.
        assert!(!TX.seek_preroll_active());
        rig.complete_plain_reload();
        assert_eq!(SHARED.frames.load(Relaxed), 0, "the reload zeroed the picture");
        rig.frame();
        assert!(TX.feed_allowed(), "the fresh session must be allowed to feed a keyframe");
        let plays = rig.land_and_present(TARGET_NS);
        assert!(plays >= 1, "the prime Play fired through the opened gate before the frame was presented");
        assert_ended_paused();
    }

    /// THE reported path: a direct-play (native) session, the viewer paused, then another audio
    /// track picked. `native_audio_tail` -> `switch_audio_native` -> `reload_at` arms a demuxer seek
    /// at the playhead but is NOT a seek the pump dispatched (`SHARED.seeking` stays false), so
    /// neither seek preroll covers it.
    #[test]
    fn a_paused_native_audio_track_switch_primes_a_first_frame_and_ends_paused() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        SHARED.frames.store(1, Relaxed);
        rig.viewer_pause();
        let pos = SHARED.playpos_ns.load(Relaxed);
        // The real entry the tail calls; this rig has no route, so it logs and returns NoRoute.
        let _ = super::super::engine::switch_audio_native(&mut rig.ps, &mut rig.pa, 2, pos);
        assert_eq!(reloads(), vec![pos], "the audio switch reloaded at the playhead");
        assert_eq!(SHARED.desired_audio_idx.load(Relaxed), 2);
        // What `reload_at` leaves once its route accepts: the session reset keeping the Pause, a
        // demuxer seek armed at the playhead, a fresh Engine awaiting its keyframe's rebase.
        rig.complete_reload(pos);
        SHARED.seeking.store(false, Relaxed);
        assert_eq!(SHARED.frames.load(Relaxed), 0);
        rig.frame();
        assert!(TX.feed_allowed(), "the new audio session must be allowed to feed its first keyframe");
        let plays = rig.land_and_present(pos);
        assert!(plays >= 1, "the prime Play fired through the opened gate before the frame was presented");
        assert_eq!(reloads(), vec![pos], "no second reload");
        assert_ended_paused();
    }

    /// The prime's Play already fired (`prime_play` cleared, the clock Running) and the viewer then
    /// pressed Pause before the first position update: `frames == 0` and the clock is
    /// `Held(User)`. No prime is owed, so converting that into `Held(Seek)` with `user_held` false
    /// would strand the film: nothing plays, and Play has no user hold to take. The pump opens no
    /// preroll, and a later Play still plays.
    #[test]
    fn a_pause_after_the_prime_play_but_before_the_first_frame_opens_no_preroll_and_play_resumes() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        SHARED.frames.store(0, Relaxed);
        SHARED.reset_clock_for_test();
        assert!(SHARED.arm_initial_clock_hold(false, false));
        rig.eng().prime_play = true;
        let plays = ffi_host::play_calls_for_test();
        rig.land_keyframe_aus(60_000_000_000, 90);
        for _ in 0..8 {
            rig.frame();
        }
        assert!(ffi_host::play_calls_for_test() > plays, "the prime Play fired");
        assert!(!rig.eng().prime_play, "the prime is no longer owed");
        assert_eq!(SHARED.frames.load(Relaxed), 0, "no position update yet");
        assert!(crate::player::pause(&mut rig.pa), "viewer Pause accepted");
        for _ in 0..4 {
            rig.frame();
        }
        assert!(!TX.seek_preroll_active(), "a preroll opened with no prime owed: nothing would ever Play");
        let plays = ffi_host::play_calls_for_test();
        assert!(crate::player::resume(&mut rig.pa), "Play after the Pause is accepted");
        for _ in 0..4 {
            rig.frame();
        }
        assert!(!TX.paused.load(Acquire), "the player is unpaused");
        assert!(ffi_host::play_calls_for_test() > plays, "Play issued a native Play");
        assert!(ffi_host::clock_state_for_test().1, "the physical clock runs again");
    }

    /// A reload that lands BEHIND the old playhead while paused (the viewer scrubbed back more than
    /// 15 s and switched audio before the scrub dispatched): the preroll the pump opens for the
    /// pictureless session must close after its one frame, whatever `playpos_ns` still says.
    #[test]
    fn a_paused_reload_landing_far_behind_the_old_playhead_ends_paused_after_one_frame() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        SHARED.frames.store(1, Relaxed);
        rig.viewer_pause();
        let behind = 20_000_000_000; // the old playhead is 60 s: more than 15 s ahead of the landing
        rig.complete_plain_reload();
        SHARED.playpos_ns.store(60_000_000_000, Relaxed);
        // A reload that arms no demuxer seek keeps the OLD playhead in `playpos_ns` until the new
        // session's first frame reports.
        assert_eq!(SHARED.playpos_ns.load(Relaxed), 60_000_000_000);
        rig.frame();
        assert!(TX.feed_allowed(), "the pictureless session is primed a frame");
        let plays = ffi_host::play_calls_for_test();
        rig.land_keyframe_aus(behind, 90);
        for _ in 0..8 {
            rig.frame();
        }
        assert!(ffi_host::play_calls_for_test() > plays, "the prime Play fired");
        // The first presented frame reports the landing's position (`pump` writes `playpos_ns`).
        SHARED.playpos_ns.store(behind, Relaxed);
        SHARED.frames.store(1, Relaxed);
        for _ in 0..3 {
            rig.frame();
        }
        assert_ended_paused();
    }

    /// The owner's framing: "Buffering…" while the player does NO work is a lie. A viewer-paused
    /// session with no picture, the feed gate shut, and nothing in flight (no seek,
    /// no preroll, the Engine loaded) publishes a state the HUD draws as the normal paused
    /// transport, not the centred spinner. (The clock is left un-held here so the preroll rule
    /// cannot open: this grades the readout rule on its own.)
    #[test]
    fn a_paused_idle_session_with_no_picture_does_not_read_buffering() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        SHARED.reset_clock_for_test(); // Running: not a clock the preroll rule can take over
        TX.commit_paused(true);
        assert_eq!(SHARED.frames.load(Relaxed), 0);
        rig.frame();
        assert!(!TX.seek_preroll_active(), "sanity: nothing is working, no preroll");
        assert!(!TX.feed_allowed(), "sanity: the feed gate is shut");
        let st = crate::player::state(&rig.ps);
        assert_eq!(
            st,
            crate::player::PlaybackState::Paused,
            "neither a spinner (Buffering) nor a lie (Playing) over an idle paused player"
        );
        assert!(!st.is_busy(), "the HUD's busy surface (`Busy::None`) follows `is_busy`");
        assert!(!crate::player::is_playing(&rig.ps), "`is_playing` means frames are presented");
    }

    /// ...and the same session with real work pending still reads Buffering: a seek preroll open.
    #[test]
    fn a_paused_session_with_a_preroll_open_still_reads_buffering() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        TX.commit_paused(true);
        assert!(TX.begin_paused_seek());
        rig.frame();
        assert_eq!(crate::player::state(&rig.ps), crate::player::PlaybackState::Buffering);
    }

    #[test]
    fn a_pause_pressed_on_an_initial_hold_before_any_frame_primes_one_and_ends_paused() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        SHARED.reset_clock_for_test();
        assert!(SHARED.arm_initial_clock_hold(false, false), "a fresh Engine is Held(Initial)");
        assert!(crate::player::pause(&mut rig.pa), "Pause on the held clock is accepted as already held");
        assert_eq!(SHARED.frames.load(Relaxed), 0);
        rig.frame();
        assert!(TX.feed_allowed(), "a Pause before the first frame must not starve the first frame");
        let plays = rig.land_and_present(60_000_000_000);
        assert!(plays >= 1, "the prime Play fired through the opened gate");
        assert_ended_paused();
    }

    #[test]
    fn a_paused_scrub_whose_native_seek_is_refused_reloads_and_ends_paused() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(Some(0));
        rig.viewer_pause();
        rig.arm(TARGET_NS);
        rig.land_keyframe(TARGET_NS);
        rig.frame(); // the feed refuses the Seek and re-posts the target for the reload
        rig.now += 5_000;
        rig.frame_until_reload();
        assert_eq!(reloads(), vec![TARGET_NS]);
        assert_eq!(preroll_at_reload(), vec![true]);
        // The replacement Engine is a different owner than the one that armed the seek.
        rig.complete_reload(TARGET_NS);
        let plays = rig.land_and_present(TARGET_NS);
        assert!(plays >= 1, "the prime Play fired through the reopened gate");
        assert_ended_paused();
    }

    #[test]
    fn a_refused_repause_is_retried_until_it_lands() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        rig.viewer_pause();
        rig.arm(TARGET_NS);
        let plays = ffi_host::play_calls_for_test();
        rig.land_keyframe_aus(TARGET_NS, 90);
        for _ in 0..8 {
            rig.frame();
        }
        assert!(ffi_host::play_calls_for_test() > plays, "the prime Play fired through the opened gate");
        ffi_host::force_pause_result_for_test(Some(0)); // Starfish refuses the re-pause
        struct Unforce;
        impl Drop for Unforce {
            fn drop(&mut self) {
                ffi_host::force_pause_result_for_test(None);
            }
        }
        let _unforce = Unforce;
        SHARED.frames.store(1, Relaxed);
        for _ in 0..3 {
            rig.frame();
        }
        assert!(TX.seek_preroll_active(), "a refused re-pause leaves the preroll open and the pump retrying");
        ffi_host::force_pause_result_for_test(None);
        rig.frame();
        assert_ended_paused();
    }

    // ---- the paused preroll's Play window is silent ----

    /// A paused seek, driven to the point the prime has Played through the opened gate.
    fn paused_seek_playing_its_preroll(rig: &mut Rig) {
        rig.viewer_pause();
        ffi_host::reset_volume_probe_for_test();
        rig.arm(TARGET_NS);
        rig.land_keyframe_aus(TARGET_NS, 90);
        for _ in 0..8 {
            rig.frame();
        }
    }

    /// The preroll's Play window is audible, so the sink is muted BEFORE that Play and the volume
    /// is given back only AFTER the accepted re-pause: the order is the whole fix.
    #[test]
    fn a_paused_preroll_mutes_before_its_play_and_restores_after_its_repause() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        paused_seek_playing_its_preroll(&mut rig);
        assert_eq!(ffi_host::host_volume_for_test(), 0, "sanity: muted while the preroll plays");
        SHARED.frames.store(1, Relaxed);
        for _ in 0..3 {
            rig.frame();
        }
        assert!(!TX.seek_preroll_active(), "the preroll closed on the landing");
        let verbs = ffi_host::verb_order_for_test();
        let at = |v: &str| verbs.iter().position(|x| x == v).unwrap_or_else(|| panic!("no {v} in {verbs:?}"));
        assert!(at("volume:0") < at("play"), "muted before the Play: {verbs:?}");
        assert!(at("play") < at("pause"), "{verbs:?}");
        // No getVolume is bound (it returns an int on 4.5, not the header's string), so the restore
        // is the pipeline default by construction, not a value read back.
        let default = tv_platform::tv::sink::PIPELINE_DEFAULT_VOLUME;
        assert_eq!(default, 100, "libpf CustomPipeline::getVolume's default");
        assert!(at("pause") < at(&format!("volume:{default}")), "restored after the re-pause: {verbs:?}");
        assert_eq!(ffi_host::host_volume_for_test(), default);
        assert!(!TX.preroll_muted());
    }

    /// A normal (unpaused) seek prime is not a preroll: it never touches the volume.
    #[test]
    fn an_unpaused_seek_prime_never_sets_the_volume() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        ffi_host::reset_volume_probe_for_test();
        rig.arm(TARGET_NS);
        let plays = ffi_host::play_calls_for_test();
        rig.land_keyframe_aus(TARGET_NS, 90);
        for _ in 0..8 {
            rig.frame();
        }
        assert!(ffi_host::play_calls_for_test() > plays, "sanity: the prime Played");
        let verbs = ffi_host::verb_order_for_test();
        assert!(!verbs.iter().any(|v| v.starts_with("volume:")), "an unpaused start set the volume: {verbs:?}");
    }

    /// The viewer presses Play while the preroll's Play window is open: the volume is back no later
    /// than that Resume.
    #[test]
    fn resume_during_an_open_preroll_restores_the_volume() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        paused_seek_playing_its_preroll(&mut rig);
        assert!(TX.seek_preroll_active() && TX.preroll_muted(), "sanity: muted preroll open");
        assert!(crate::player::resume(&mut rig.pa), "viewer Resume accepted");
        assert_eq!(ffi_host::host_volume_for_test(), 100, "Resume left the sink muted");
        assert!(!TX.preroll_muted());
    }

    /// A preroll closed by anything but the accepted re-pause (an abandoned seek) still gives the
    /// volume back, on the pump's next tick.
    #[test]
    fn an_abandoned_preroll_restores_the_volume_on_the_next_tick() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        paused_seek_playing_its_preroll(&mut rig);
        assert_eq!(ffi_host::host_volume_for_test(), 0, "sanity: muted");
        crate::player::abandon_seek();
        assert!(!TX.seek_preroll_active());
        rig.frame();
        assert_eq!(ffi_host::host_volume_for_test(), 100, "an abandoned preroll left the sink muted");
        assert!(!TX.preroll_muted());
    }

    /// A stop unloads the pipeline the mute belonged to, so the next playback starts unmuted with
    /// no sink call on the dead one.
    #[test]
    fn a_stop_forgets_the_mute_with_its_pipeline() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        paused_seek_playing_its_preroll(&mut rig);
        assert!(TX.preroll_muted(), "sanity: muted");
        let sets = ffi_host::verb_order_for_test().len();
        TX.reset();
        assert!(!TX.preroll_muted());
        assert_eq!(ffi_host::verb_order_for_test().len(), sets, "a reset touched the dead pipeline");
    }

    #[test]
    fn a_pause_pressed_mid_flight_of_a_playing_seek_still_lands_and_ends_paused() {
        let _serial = tv_base::testlock::serial();
        let mut rig = Rig::new(None);
        rig.arm(TARGET_NS); // playing: no preroll, nothing to hold
        assert!(!TX.seek_preroll_active());
        assert!(crate::player::pause(&mut rig.pa), "the viewer pauses before the keyframe landed");
        rig.frame();
        assert!(TX.feed_allowed(), "the keyframe must still be able to land");
        let plays = ffi_host::play_calls_for_test();
        rig.land_keyframe_aus(TARGET_NS, 90);
        for _ in 0..8 {
            rig.frame();
        }
        assert!(ffi_host::play_calls_for_test() > plays, "the prime Play fired through the opened gate");
        assert!(!rig.eng().flushed && !rig.eng().rebase_pending, "the keyframe reached feed_stream's rebase");
        rig.now += 5_000;
        for _ in 0..3 {
            rig.frame();
        }
        assert!(reloads().is_empty(), "no stuck-watchdog reload");
        SHARED.frames.store(1, Relaxed);
        rig.frame();
        assert_ended_paused();
        assert!(!SHARED.seeking.load(Relaxed), "no endless spinner");
    }

    // ---- the ACB (webOS 4) video plane binds through setMediaVideoData + acb_start; a paused
    // preroll's picture is only on the panel once that bind has completed ----

    /// ACB in use, `setMediaVideoData` accepted, the sourceInfo envelope captured; restores the
    /// process-wide `ACB_OK` flag (default off on the host) when the test ends.
    struct AcbGuard;
    impl AcbGuard {
        fn arm() -> AcbGuard {
            ACB_OK.store(true, Relaxed);
            ffi_host::reset_acb_for_test(true);
            AcbGuard
        }
    }
    impl Drop for AcbGuard {
        fn drop(&mut self) {
            ACB_OK.store(false, Relaxed);
            ffi_host::reset_acb_for_test(false);
        }
    }

    impl Rig {
        /// A fresh ACB session that has not bound yet (what a reload installs).
        fn unbound_acb_session(&mut self) {
            // `Rig::new` reset the session, which cleared what the pipeline's callbacks captured.
            *SHARED.media_id.lock().unwrap() = Some(std::ffi::CString::new("_media_id_").unwrap());
            *SHARED.source_info.lock().unwrap() = Some(b"{\"context\":\"x\",\"video\":{}}\0".to_vec());
            let e = self.eng();
            e.video_info_sent = false;
            e.prime_play = true;
            e.bound_at_frames = None;
            e.stage = Stage::Playing;
        }
    }

    /// THE reported bug: the paused reload preroll re-paused at the first position tick, below
    /// `Stage::Streaming`, so `setMediaVideoData` + `acb_start` never ran and the panel stayed
    /// black until Resume.
    #[test]
    fn a_paused_acb_preroll_does_not_re_pause_before_the_plane_is_bound() {
        let _serial = tv_base::testlock::serial();
        let _acb = AcbGuard::arm();
        let mut rig = Rig::new(None);
        rig.unbound_acb_session();
        rig.viewer_pause();
        SHARED.frames.store(0, Relaxed);
        rig.frame();
        rig.frame();
        assert!(TX.seek_preroll_active(), "the reload preroll opened");
        ffi_host::reset_acb_for_test(true);
        let mut closed_at = None;
        for tick in 1..=6 {
            SHARED.frames.store(tick, Relaxed);
            rig.frame();
            rig.frame();
            if !TX.seek_preroll_active() {
                closed_at = Some(tick);
                break;
            }
        }
        let closed_at = closed_at.expect("the preroll closed");
        assert!(closed_at > 1, "re-paused on the first tick, before the plane could bind (tick {closed_at})");
        assert!(rig.eng().stage == Stage::Streaming, "re-paused below Streaming: the plane is not bound");
        let (video_data, start, pause, _) = ffi_host::acb_calls_for_test();
        assert_eq!((video_data, start), (1, 1), "the bind ran exactly once before the re-pause");
        assert_eq!(pause, 1, "the re-pause mirrors ACB PAUSED now that the plane is bound");
        assert!(TX.paused.load(Acquire), "the film ends paused");
    }

    /// The Play window of the preroll is as short as the bind allows: during an open preroll the
    /// decoder having produced its first tick is enough (outside one the gate stays at two).
    #[test]
    fn an_open_acb_preroll_binds_at_the_first_tick() {
        let _serial = tv_base::testlock::serial();
        let _acb = AcbGuard::arm();
        let mut rig = Rig::new(None);
        rig.unbound_acb_session();
        rig.viewer_pause();
        SHARED.frames.store(0, Relaxed);
        rig.frame();
        rig.frame();
        assert!(TX.seek_preroll_active());
        ffi_host::reset_acb_for_test(true);
        SHARED.frames.store(1, Relaxed);
        rig.frame();
        let (video_data, start, pause, _) = ffi_host::acb_calls_for_test();
        assert_eq!((video_data, start), (1, 1), "the plane bound on the preroll's first tick");
        assert_eq!(pause, 0, "the pipeline is Playing for the preroll, so ACB stays PLAYING");
        assert!(rig.eng().stage == Stage::Streaming);
    }

    #[test]
    fn a_normal_acb_start_still_waits_for_the_second_tick() {
        let _serial = tv_base::testlock::serial();
        let _acb = AcbGuard::arm();
        let mut rig = Rig::new(None);
        rig.unbound_acb_session();
        SHARED.frames.store(1, Relaxed);
        rig.frame();
        rig.frame();
        assert_eq!(ffi_host::acb_calls_for_test().1, 0, "one tick is not enough outside a preroll");
        SHARED.frames.store(2, Relaxed);
        rig.frame();
        assert_eq!(ffi_host::acb_calls_for_test().1, 1);
        assert_eq!(ffi_host::acb_calls_for_test().2, 0, "playing: ACB is left PLAYING");
    }

    /// A late tick binds the plane after the pipeline was already re-paused: ACB must not be left
    /// PLAYING under a paused pipeline.
    #[test]
    fn an_acb_start_under_a_paused_pipeline_is_followed_by_the_acb_pause() {
        let _serial = tv_base::testlock::serial();
        let _acb = AcbGuard::arm();
        let mut rig = Rig::new(None);
        rig.unbound_acb_session();
        rig.eng().prime_play = false;
        rig.eng().stage = Stage::Bound;
        SHARED.frames.store(2, Relaxed);
        rig.viewer_pause();
        assert!(!TX.seek_preroll_active(), "no preroll: the pipeline itself is paused");
        ffi_host::reset_acb_for_test(true);
        rig.frame();
        let (_, start, pause, _) = ffi_host::acb_calls_for_test();
        assert_eq!(start, 1, "the bind ran");
        assert_eq!(pause, 1, "ACB was left PLAYING under a paused pipeline");
    }
}
