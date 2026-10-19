//! The playback policies, pure: which archive URL plays which part of a programme, what happens
//! when a part or the programme ends, and when the stall watchdog restarts. The engine underneath
//! is exercised end to end by `playback_e2e_tests.rs`.

use super::*;

const START: i64 = 1_760_000_000;

fn shift() -> ShiftUrl {
    Arc::new(|utc: i64| format!("http://tv.example/ch1/mono.m3u8?utc={utc}&lutc=1760099999"))
}

fn programme(len_s: i64, window: bool, with_shift: bool, at_end: AtEnd) -> Catchup {
    Catchup {
        start: START,
        end: START + len_s,
        offset_secs: 0,
        window: window.then(|| format!("http://tv.example/ch1/mono-{START}-{len_s}.m3u8")),
        shift: with_shift.then(shift),
        live_url: "http://tv.example/ch1/mono.m3u8".into(),
        headers: RequestHeaders::none(),
        at_end,
    }
}

fn view(pos_s: i64, part_s: i64, part_ended: bool) -> PartView {
    PartView { pos_s, part_s, part_len_s: 0, part_ended, now_unix: START + 86_400, end_reported: false, have_frame: true }
}

#[test]
fn the_window_plays_the_whole_programme_and_shift_plays_it_in_three_hour_parts() {
    let c = programme(5 * 3600, true, true, AtEnd::ReturnToLive);
    assert_eq!(part_for(&c, false, 4 * 3600), Some((Source::Window, 0)), "the window has no 3 h cap");
    assert!(url_of(&c, Source::Window, 0).unwrap().contains(&format!("mono-{START}-18000.m3u8")));
    assert_eq!(part_for(&c, true, 4 * 3600), Some((Source::Shift, SHIFT_SPAN_S)), "after the fallback: the part holding 4 h");
    assert_eq!(url_of(&c, Source::Shift, SHIFT_SPAN_S).unwrap(), format!("http://tv.example/ch1/mono.m3u8?utc={}&lutc=1760099999", START + SHIFT_SPAN_S));
    let only_shift = programme(5 * 3600, false, true, AtEnd::ReturnToLive);
    assert_eq!(part_for(&only_shift, false, 600), Some((Source::Shift, 0)));
    assert_eq!(part_for(&only_shift, false, SHIFT_SPAN_S - 1), Some((Source::Shift, 0)));
    assert_eq!(part_for(&only_shift, false, SHIFT_SPAN_S), Some((Source::Shift, SHIFT_SPAN_S)));
    let only_window = programme(3600, true, false, AtEnd::ReturnToLive);
    assert_eq!(part_for(&only_window, true, 600), Some((Source::Window, 0)), "nothing to fall back to");
    assert_eq!(url_of(&only_window, Source::Shift, 0), None);
    assert_eq!(part_for(&programme(3600, false, false, AtEnd::ReturnToLive), false, 0), None);
}

#[test]
fn a_shift_part_that_ends_inside_a_long_programme_requests_the_next_part_from_there() {
    let c = programme(4 * 3600, false, true, AtEnd::ReturnToLive);
    assert_eq!(next_step(&c, view(SHIFT_SPAN_S - 3, 0, true)), Step::NextPart(SHIFT_SPAN_S - 3));
    let known = PartView { part_len_s: SHIFT_SPAN_S, ..view(SHIFT_SPAN_S - 1, 0, true) };
    assert_eq!(next_step(&c, known), Step::NextPart(SHIFT_SPAN_S), "from where the playlist ended, not where the playhead stopped");
    assert_eq!(next_step(&c, view(SHIFT_SPAN_S - 3, 0, false)), Step::Play, "still playing the first part");
    assert_eq!(next_step(&c, view(SHIFT_SPAN_S, SHIFT_SPAN_S, true)), Step::ReturnToLive, "a part that ended where it began has no next");
    // The next part starts exactly where the last one ended, whatever span the server answered.
    assert!(url_of(&c, Source::Shift, 1800).unwrap().contains(&format!("utc={}", START + 1800)));
    let window_only = programme(4 * 3600, true, false, AtEnd::ReturnToLive);
    assert_eq!(next_step(&window_only, view(3600, 0, true)), Step::ReturnToLive, "a short window with no shift is the end");
}

#[test]
fn the_programme_end_returns_to_live_or_is_reported_once() {
    let c = programme(3600, false, true, AtEnd::ReturnToLive);
    assert_eq!(next_step(&c, view(3599, 0, false)), Step::Play);
    assert_eq!(next_step(&c, view(3600, 0, false)), Step::ReturnToLive, "shift plays past the end; the end is the programme's");
    let window = programme(3600, true, true, AtEnd::ReturnToLive);
    assert_eq!(next_step(&window, view(3598, 0, true)), Step::ReturnToLive, "the window's own end is the programme's");
    let next = programme(3600, true, true, AtEnd::Continue);
    assert_eq!(next_step(&next, view(3600, 0, false)), Step::ProgrammeEnded);
    let mut reported = view(3700, 0, false);
    reported.end_reported = true;
    assert_eq!(next_step(&next, reported), Step::Play, "reported once, and the archive plays on");
    let mut no_frame = view(3600, 0, false);
    no_frame.have_frame = false;
    assert_eq!(next_step(&c, no_frame), Step::Play, "an armed position is not the playhead yet");
}

#[test]
fn start_over_on_the_programme_on_air_returns_to_live_once_it_catches_up() {
    let c = programme(2 * 3600, false, true, AtEnd::Continue);
    let mut v = view(1800, 0, true);
    v.now_unix = START + 1800 + 20;
    assert_eq!(next_step(&c, v), Step::ReturnToLive, "the archive stops at now: live is next");
    v.now_unix = START + 1800 + 600;
    assert_eq!(next_step(&c, v), Step::NextPart(1800), "ten minutes behind: the archive has more by now");
}

fn obs(now: u32, pos: i64) -> Observation {
    Observation { now, failed: false, frame: true, held: false, position_ns: pos }
}

#[test]
fn the_watchdog_restarts_a_playhead_that_stood_still_for_ten_seconds() {
    let mut w = Watchdog::default();
    w.restarted(0);
    assert_eq!(w.observe(Observation { frame: false, ..obs(60_000, 0) }), Verdict::Wait, "never shown a frame: not armed");
    for t in (0..30_000).step_by(200) {
        assert_eq!(w.observe(obs(t, i64::from(t) * 1_000_000)), Verdict::Wait, "moving at {t}");
    }
    assert_eq!(w.observe(obs(29_800 + STALL_MS - 1, 29_800_000_000)), Verdict::Wait);
    assert_eq!(w.observe(obs(29_800 + STALL_MS, 29_800_000_000)), Verdict::Restart);
    // The restart's start has no frame yet: it gets the same ten seconds, then another restart.
    w.restarted(40_000);
    let fresh = |now| Observation { frame: false, ..obs(now, 29_800_000_000) };
    assert_eq!(w.observe(fresh(40_000 + STALL_MS - 1)), Verdict::Wait);
    assert_eq!(w.observe(fresh(40_000 + STALL_MS)), Verdict::Restart);
    w.restarted(60_000);
    assert_eq!(w.observe(fresh(60_000 + STALL_MS)), Verdict::Restart);
    w.restarted(80_000);
    assert_eq!(w.observe(fresh(80_000 + STALL_MS)), Verdict::Wait, "MAX_RECOVERIES in a row without a frame: the failure stands");
    // Ten seconds of playback after them earn a fresh budget.
    for t in (100_000..100_000 + STALL_MS + 200).step_by(200) {
        assert_eq!(w.observe(obs(t, i64::from(t) * 1_000_000)), Verdict::Wait);
    }
    let last = i64::from(100_000 + STALL_MS) * 1_000_000;
    assert_eq!(w.observe(obs(100_000 + 2 * STALL_MS, last)), Verdict::Restart);
}

/// A restart that shows one frame and stalls again has not recovered: the budget is not reset,
/// so a stream that manages a frame per start cannot restart forever.
#[test]
fn one_frame_after_a_restart_does_not_refill_the_budget() {
    let mut w = Watchdog::default();
    w.restarted(0);
    let (mut now, mut restarts) = (0, 0);
    for round in 0..10 {
        // one frame (the playhead moves once), then it stands for a stall's length
        assert_eq!(w.observe(obs(now, round)), Verdict::Wait);
        now += STALL_MS;
        if w.observe(obs(now, round)) == Verdict::Restart {
            restarts += 1;
            w.restarted(now);
        }
        now += 200;
    }
    assert_eq!(restarts, MAX_RECOVERIES, "{restarts}");
}

#[test]
fn a_paused_or_ended_playhead_is_not_a_stall_and_a_failure_after_frames_is() {
    let mut w = Watchdog::default();
    w.restarted(0);
    assert_eq!(w.observe(obs(0, 5)), Verdict::Wait);
    let held = |now| Observation { held: true, ..obs(now, 5) };
    assert_eq!(w.observe(held(STALL_MS * 6)), Verdict::Wait, "paused for a minute");
    assert_eq!(w.observe(obs(STALL_MS * 6 + 100, 5)), Verdict::Wait, "resumed: the clock starts again");
    let failed = |now| Observation { failed: true, ..obs(now, 5) };
    let t0 = STALL_MS * 6; // the playhead last moved (was held) then
    assert_eq!(w.observe(failed(t0 + FAIL_RETRY_MS - 1)), Verdict::Wait);
    assert_eq!(w.observe(failed(t0 + FAIL_RETRY_MS)), Verdict::Restart, "a dead live playlist after frames is recovered");
}

#[test]
fn a_catchup_prints_no_url() {
    let c = programme(3600, true, true, AtEnd::ReturnToLive);
    let shown = format!("{c:?}");
    assert!(!shown.contains("tv.example"), "{shown}");
}

/// The TV taking the decoder back suspends the playback instead of letting the watchdog fight
/// for it; resume starts it again. Driven through the real API with no FFmpeg on the host, so the
/// start itself is refused (the probe has no demuxer): the suspend does not depend on a picture.
#[cfg(feature = "hostsim")]
#[test]
fn a_decoder_taken_back_suspends_and_resume_starts_again() {
    let _serial = tv_base::testlock::serial();
    // SAFETY: this test thread is the only one driving the player.
    let mut pb = Playback::new(unsafe { MainThread::assume() });
    pb.play_live("http://127.0.0.1:1/ch1/mono.m3u8", crate::headers::RequestHeaders::none());
    pb.tick(0);
    player::note_decoder_revoked_for_test();
    pb.tick(16);
    assert_eq!(pb.take_event(), Some(Event::DecoderTaken));
    assert_eq!((pb.status().mode, pb.status().state), (Mode::Live, State::Suspended));
    for t in 1..20 {
        pb.tick(16 + t * STALL_MS);
        assert_eq!(pb.status().state, State::Suspended, "nothing restarts a suspended playback");
    }
    pb.resume();
    assert_ne!(pb.status().state, State::Suspended);
    pb.stop();
    assert_eq!((pb.status().mode, pb.status().state), (Mode::Idle, State::Idle));
}

/// The picture's rectangle round-trips, is clipped to the plane, and full screen is the default.
#[test]
fn the_video_rect_round_trips_and_is_clipped_to_the_plane() {
    let _serial = tv_base::testlock::serial();
    let full = Viewport { x: 0, y: 0, width: 1920, height: 1080 };
    let preview = Viewport { x: 1180, y: 96, width: 640, height: 360 };
    player::set_video_rect(Some(preview));
    assert_eq!(player::video_rect(), preview);
    player::set_video_rect(Some(Viewport { x: 1600, y: 900, width: 640, height: 360 }));
    let clipped = player::video_rect();
    assert!(clipped.x + clipped.width <= 1920 && clipped.y + clipped.height <= 1080, "{clipped:?}");
    player::set_video_rect(Some(Viewport { x: 10, y: 10, width: 0, height: 0 }));
    assert_eq!(player::video_rect(), full, "an empty rect is full screen");
    player::set_video_rect(None);
    assert_eq!(player::video_rect(), full);
}

/// Only a catch-up that played to its end holds the watchdog: a live stream that ended (a clean
/// EOF, a packager's ENDLIST) is restarted like any stall.
#[test]
fn an_ended_live_stream_is_a_stall_and_an_ended_catchup_a_hold() {
    assert!(is_hold(true, false, false), "paused live");
    assert!(is_hold(false, true, true), "a catch-up at its end waits for its policy");
    assert!(!is_hold(false, true, false), "a live stream that ended is restarted");
    assert!(!is_hold(false, false, true));
}

fn play(c: Catchup) -> CatchupPlay {
    CatchupPlay { req: c, source: Source::Window, part_s: 0, fell_back: false, end_reported: false, no_archive: false, resume_s: 0 }
}

/// A restart or resume reopens the part already playing: a shift part that continued a
/// programme past its window is not the window again (which would jump back and loop).
#[test]
fn a_restart_of_a_continued_shift_part_stays_on_shift() {
    let mut cp = play(programme(4 * 3600, true, true, AtEnd::ReturnToLive));
    let next = cp.enter((Source::Shift, 3600)).unwrap();
    assert!(next.contains(&format!("utc={}", START + 3600)));
    let again = cp.enter((cp.source, cp.part_s)).unwrap();
    assert_eq!(again, next, "the same shift part, not the window");
    assert_eq!((cp.source, cp.part_s), (Source::Shift, 3600));
}

/// Every new request leaves the no-archive state: a seek elsewhere in the programme gets its
/// own answer.
#[test]
fn a_new_request_clears_no_archive() {
    let mut cp = play(programme(3600, true, false, AtEnd::ReturnToLive));
    cp.no_archive = true;
    assert!(cp.enter((Source::Window, 0)).is_some());
    assert!(!cp.no_archive);
    assert_eq!(cp.enter((Source::Shift, 0)), None, "no shift URL: nothing entered");
}

/// The shell paces its event loop by this: every frame while anything plays, nothing when idle.
#[cfg(feature = "hostsim")]
#[test]
fn the_tick_interval_is_a_frame_while_playing_and_none_when_idle() {
    let _serial = tv_base::testlock::serial();
    // SAFETY: this test thread is the only one driving the player.
    let mut pb = Playback::new(unsafe { MainThread::assume() });
    assert_eq!(pb.tick_interval_ms(), None);
    pb.play_live("http://127.0.0.1:1/ch1/mono.m3u8", RequestHeaders::none());
    assert_eq!(pb.tick_interval_ms(), Some(16));
    pb.suspend();
    assert_eq!(pb.tick_interval_ms(), None);
    pb.stop();
    pb.tick(0);
    assert_eq!(pb.tick_interval_ms(), None);
}
