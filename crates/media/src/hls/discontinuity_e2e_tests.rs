//! **An archive with ad breaks**, end to end on the desktop simulator path (`playback_e2e_tests.rs`
//! has the rig and how to run it): runs of fixture segments spliced with `EXT-X-DISCONTINUITY`, so
//! the timestamps jump back 48 s at the first break (as an archive's ad break did on the C5) and
//! forward 30 s at the second. The demuxer puts every run on one timeline (`hls::timeline`).

use super::*;

/// The programme: segments 100..105 (timestamps from 601 s), the "ad" 97..100 (from 583 s: 48 s
/// back), then 105..110 (from 631 s: 30 s forward). Thirteen 6 s segments, breaks at 30 s and 48 s.
const RUNS: [std::ops::Range<usize>; 3] = [100..105, 97..100, 105..110];
const LENGTH_S: i64 = 78;
const FIRST_BREAK_MS: i64 = 30_000;
const SECOND_BREAK_MS: i64 = 48_000;

fn programme(o: &Origin, window: &str, offset_secs: i64) -> Catchup {
    Catchup {
        start: PROGRAMME,
        end: PROGRAMME + LENGTH_S,
        offset_secs,
        window: Some(o.url(window)),
        shift: None,
        live_url: String::new(),
        headers: crate::headers::RequestHeaders::none(),
        at_end: AtEnd::Continue,
    }
}

/// The programme position, ms.
fn position(s: &Status) -> i64 {
    s.catchup.map_or(0, |c| c.position_ms)
}

/// Play the programme of `length_s` on to its end: no freeze (the playhead never stands still for
/// long), the watchdog never needed, and the position goes on past every break to the end.
fn plays_through_to_the_end(rig: &mut Rig, length_s: i64) {
    let (mut last, mut moved_at, mut worst_gap, mut furthest) = (position(&rig.pb.status()), Instant::now(), Duration::ZERO, 0);
    let ended = rig.run_until(Duration::from_secs(length_s as u64 + 30), |s, ev| {
        let pos = position(s);
        if pos != last {
            worst_gap = worst_gap.max(moved_at.elapsed());
            (last, moved_at) = (pos, Instant::now());
        }
        furthest = furthest.max(pos);
        ev.iter().any(|(_, e)| *e == Event::ProgrammeEnded) || recovered(ev) > 0
    });
    eprintln!("e2e: played to {furthest} ms, worst playhead gap {} ms, {} restarts", worst_gap.as_millis(), recovered(&rig.events));
    assert_eq!(recovered(&rig.events), 0, "the watchdog never fired");
    assert!(ended, "the programme never ended");
    assert!(furthest >= (length_s - 6) * 1000, "played on past every break to the end: {furthest} ms");
    assert!(worst_gap < Duration::from_secs(3), "no freeze at a break: the playhead stood still for {} ms", worst_gap.as_millis());
}

/// **Played from its start, it plays through both breaks** ([`plays_through_to_the_end`]).
#[test]
#[ignore]
fn e2e_catchup_plays_through_discontinuities_whose_timestamps_jump() {
    let Some(fx) = fixture() else { return };
    let _serial = tv_base::testlock::serial();
    let _sink = ClockSink::arm();
    let o = Origin::spawn();
    serve_segments(&o, &fx, 97..110);
    let window = format!("/ch5/mono-{PROGRAMME}-{LENGTH_S}.m3u8?token=e2e-secret");
    o.put(&window, archive_with_breaks(&RUNS));
    let mut rig = Rig::new();
    rig.pb.play_catchup(programme(&o, &window, 0));
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.state == State::Playing), "no picture within 20 s");
    plays_through_to_the_end(&mut rig, LENGTH_S);
    rig.stop();
}

/// The offset of the first audio PES (stream id 0xC0..=0xDF) that starts at or after byte `from`
/// of the MPEG-TS `seg`, and its PES_packet_length.
fn audio_pes(seg: &[u8], from: usize) -> Option<(usize, usize)> {
    seg.as_chunks::<188>().0.iter().enumerate().skip(from.div_ceil(188)).find_map(|(i, p)| {
        let payload = match (p[3] >> 4) & 3 {
            1 => 4,
            3 => 5 + usize::from(p[4]),
            _ => return None,
        };
        let pes = p.get(payload..payload + 6)?;
        let starts = p[1] & 0x40 != 0 && pes[..3] == [0, 0, 1] && (0xC0..=0xDF).contains(&pes[3]);
        starts.then(|| (i * 188, usize::from(u16::from_be_bytes([pes[4], pes[5]]))))
    })
}

/// **A PES the next run's bytes complete stays on its own run's timeline**: the run before the
/// first break ends in a segment cut 5 TS packets into an audio PES, so the demuxer completes
/// that PES only from the ad's bytes, and mpegts gives its frames after the first no position.
/// Shifted as the ad's, they moved the run after the ad 25 s too far: the programme froze at the
/// second break and went back to live. On their own run's timeline it plays to its end.
#[test]
#[ignore]
fn e2e_a_pes_cut_at_a_break_keeps_its_runs_timeline() {
    let Some(fx) = fixture() else { return };
    let _serial = tv_base::testlock::serial();
    let _sink = ClockSink::arm();
    let o = Origin::spawn();
    serve_segments(&o, &fx, 97..110);
    let (pes, len) = audio_pes(&fx.segments[104], 30_000).expect("an audio PES in segment 104");
    assert!(len > 5 * 184, "the cut lands inside the PES of {len} bytes");
    o.put("/seg/cut.ts", Answer::ok(fx.segments[104][..pes + 5 * 188].to_vec()));
    // 100..104 and the cut segment (1.5 s), the ad 97..100, then 105..110: breaks at 25.5 s and 43.5 s.
    let six = |run: std::ops::Range<usize>| run.map(|i| format!("#EXTINF:6.000,\n{}\n", seg_path(i))).collect::<String>();
    let playlist = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:100\n#EXT-X-PLAYLIST-TYPE:EVENT\n\
         {}#EXTINF:1.500,\n/seg/cut.ts\n#EXT-X-DISCONTINUITY\n{}#EXT-X-DISCONTINUITY\n{}#EXT-X-ENDLIST\n",
        six(100..104),
        six(97..100),
        six(105..110)
    );
    let length_s = 74;
    let window = format!("/ch8/mono-{PROGRAMME}-{length_s}.m3u8?token=e2e-secret");
    o.put(&window, Answer::playlist(playlist));
    let mut rig = Rig::new();
    rig.pb.play_catchup(Catchup { end: PROGRAMME + length_s, ..programme(&o, &window, 0) });
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.state == State::Playing), "no picture within 20 s");
    plays_through_to_the_end(&mut rig, length_s);
    rig.stop();
}

/// **A resume and a restart past a break land where they were asked**: the programme resumed
/// inside the ad (after the first break) starts there and plays through the second break; stopped
/// past it (the app in the background) and started again, it restarts at the position it had
/// reached, past both breaks, the way a History resume or a watchdog restart does.
#[test]
#[ignore]
fn e2e_a_resume_and_a_restart_past_a_discontinuity_land_at_their_position() {
    let Some(fx) = fixture() else { return };
    let _serial = tv_base::testlock::serial();
    let _sink = ClockSink::arm();
    let o = Origin::spawn();
    serve_segments(&o, &fx, 97..110);
    let window = format!("/ch6/mono-{PROGRAMME}-{LENGTH_S}.m3u8?token=e2e-secret");
    o.put(&window, archive_with_breaks(&RUNS));
    let mut rig = Rig::new();
    rig.pb.play_catchup(programme(&o, &window, 40));
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.state == State::Playing), "no picture within 20 s");
    let resumed = position(&rig.pb.status());
    eprintln!("e2e: resumed at 40 s, playing at {resumed} ms");
    assert!((FIRST_BREAK_MS..SECOND_BREAK_MS).contains(&resumed), "starts at the segment holding 40 s: {resumed}");

    assert!(
        rig.run_until(Duration::from_secs(25), |s, _| position(s) >= SECOND_BREAK_MS + 7_000),
        "did not play through the second break: at {} ms",
        position(&rig.pb.status())
    );
    let before = position(&rig.pb.status());
    rig.pb.suspend();
    let _ = rig.run_until(Duration::from_secs(2), |_, _| false);
    rig.pb.resume();
    assert!(
        rig.run_until(Duration::from_secs(20), |s, _| s.state == State::Playing && s.first_frame_ms.is_some()),
        "no picture after the restart"
    );
    let restarted = position(&rig.pb.status());
    eprintln!("e2e: stopped at {before} ms, restarted at {restarted} ms");
    assert!(restarted >= SECOND_BREAK_MS, "the restart lands past the break, not before it: {restarted}");
    assert!(
        restarted + 7_000 >= before && restarted <= before + 2_000,
        "the restart lands at its position (by segment): {before} -> {restarted}"
    );
    assert!(rig.run_until(Duration::from_secs(40), |_, ev| ev.iter().any(|(_, e)| *e == Event::ProgrammeEnded)), "played on to the end");
    assert_eq!(recovered(&rig.events), 0, "the watchdog never fired");
    rig.stop();
}

/// Move the PTS of the PES whose first TS packet is at byte `at` of `seg` by `ticks` (90 kHz).
fn move_pts(seg: &mut [u8], at: usize, ticks: i64) {
    let p = &mut seg[at..at + 188];
    let payload = if (p[3] >> 4) & 3 == 3 { 5 + usize::from(p[4]) } else { 4 };
    let f = &mut p[payload + 9..payload + 14];
    assert!((f[0] >> 4) & 2 != 0, "the PES carries a PTS");
    let pts = (u64::from((f[0] >> 1) & 7) << 30)
        | (u64::from(f[1]) << 22)
        | (u64::from(f[2] >> 1) << 15)
        | (u64::from(f[3]) << 7)
        | u64::from(f[4] >> 1);
    let pts = (pts as i64 + ticks).rem_euclid(1 << 33) as u64;
    f[0] = (f[0] & 0xF0) | ((pts >> 29) as u8 & 0x0E) | 1;
    f[1] = (pts >> 22) as u8;
    f[2] = ((pts >> 14) as u8 & 0xFE) | 1;
    f[3] = (pts >> 7) as u8;
    f[4] = ((pts << 1) as u8) | 1;
}

/// A live channel at `path` whose media sequence `q` is `segments[q]`, an `EXT-X-DISCONTINUITY`
/// before each sequence in `breaks`, counted by `EXT-X-DISCONTINUITY-SEQUENCE`: a window of
/// [`WINDOW`] segments, its edge advancing one segment per 6 s of wall time.
fn serve_live_with_breaks(o: &Origin, path: &str, segments: Vec<Vec<u8>>, breaks: Vec<usize>) {
    let seg = |q: usize| format!("/lv/{q:04}.ts");
    for (q, bytes) in segments.iter().enumerate() {
        o.put(&seg(q), Answer::ok(bytes.clone()));
    }
    let (t0, last) = (Instant::now(), segments.len() - 1);
    o.route(path, move || {
        let newest = (WINDOW - 1 + (t0.elapsed().as_secs_f64() / SEG_S) as usize).min(last);
        let oldest = newest + 1 - WINDOW;
        let dseq = breaks.iter().filter(|&&b| b <= oldest).count();
        let mut p = format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:{oldest}\n#EXT-X-DISCONTINUITY-SEQUENCE:{dseq}\n"
        );
        for q in oldest..=newest {
            if q > oldest && breaks.contains(&q) {
                p.push_str("#EXT-X-DISCONTINUITY\n");
            }
            p.push_str(&format!("#EXTINF:6.000,\n{}\n", seg(q)));
        }
        if newest == last {
            p.push_str("#EXT-X-ENDLIST\n");
        }
        Answer::playlist(p)
    });
}

/// **Live, a break whose timestamps go on is not shifted by a bad timestamp before it, and a break
/// that jumps back goes on where the stream was.** As on the C1: an audio PES timed 55013 s past
/// the stream in the segment before a break, whose frames FFmpeg times from it; the break began
/// at an audio packet that went on as before, and was measured against those frames: shifted
/// 55013 s, the picture stopped and the watchdog restarted the channel. Here the segment after the
/// first break begins at an audio PES, so that audio begins the run, and the second break jumps
/// 30 s back.
#[test]
#[ignore]
fn e2e_live_discontinuities_play_through_after_a_bad_audio_timestamp() {
    let Some(fx) = fixture() else { return };
    let _serial = tv_base::testlock::serial();
    let _sink = ClockSink::arm();
    let o = Origin::spawn();
    let live = "/ch3/mono.m3u8?token=e2e-secret";
    // Sequences 0..12 are segments 0..12, the timestamps going on across the break before 8; then
    // segments 7.. again, 30 s back, from the break before 12.
    let mut segments: Vec<Vec<u8>> = (0..12).chain(7..30).map(|i| fx.segments[i].clone()).collect();
    let (pes, _) = audio_pes(&segments[7], 10_000).expect("an audio PES in segment 7");
    move_pts(&mut segments[7], pes, 55_013_434 * 90);
    let (first_audio, _) = audio_pes(&segments[8], 0).expect("an audio PES in segment 8");
    segments[8].drain(..first_audio);
    serve_live_with_breaks(&o, live, segments, vec![8, 12]);
    let mut rig = Rig::new();
    rig.pb.play_live(&o.url(live), crate::headers::RequestHeaders::none());
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.first_frame_ms.is_some()), "no first frame within 20 s");
    // Past both breaks, about 30 s and 54 s into the playback.
    plays_live_through(&mut rig, 70);
    rig.stop();
}

/// **Live, a break begun by an audio PES that still goes on from the run before, its video and the
/// rest of it 30 s on, plays through.** The audio decided that the run went on, and the video the
/// demuxer read before the audio's next PES decided again stayed 30 s ahead: the lanes held it,
/// the picture froze and the watchdog restarted the channel. Here the segment after the break
/// begins at an audio PES moved 30 s back.
#[test]
#[ignore]
fn e2e_live_a_break_begun_by_audio_that_goes_on_moves_its_video_with_the_run() {
    let Some(fx) = fixture() else { return };
    let _serial = tv_base::testlock::serial();
    let _sink = ClockSink::arm();
    let o = Origin::spawn();
    let live = "/ch4/mono.m3u8?token=e2e-secret";
    // Sequences 0..12 are segments 0..12; from the break before 12, segments 17.., 30 s on.
    let mut segments: Vec<Vec<u8>> = (0..12).chain(17..30).map(|i| fx.segments[i].clone()).collect();
    let (first_audio, _) = audio_pes(&segments[12], 0).expect("an audio PES in segment 17");
    segments[12].drain(..first_audio);
    move_pts(&mut segments[12], 0, -30 * 90_000);
    serve_live_with_breaks(&o, live, segments, vec![12]);
    let mut rig = Rig::new();
    rig.pb.play_live(&o.url(live), crate::headers::RequestHeaders::none());
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.first_frame_ms.is_some()), "no first frame within 20 s");
    // Past the break, about 54 s into the playback.
    plays_live_through(&mut rig, 70);
    rig.stop();
}

/// Play the live channel for `secs` of wall time: the watchdog never needed, and the playhead
/// following wall time across every break, never standing still for long.
fn plays_live_through(rig: &mut Rig, secs: u64) {
    let (pos0, wall0) = (player::playpos_ns(), Instant::now());
    let (mut last_pos, mut moved_at, mut worst_gap) = (pos0, Instant::now(), Duration::ZERO);
    rig.run_until(Duration::from_secs(secs), |s, ev| {
        let pos = player::playpos_ns();
        if pos != last_pos {
            worst_gap = worst_gap.max(moved_at.elapsed());
            (last_pos, moved_at) = (pos, Instant::now());
        }
        s.state != State::Playing || recovered(ev) > 0
    });
    let played = Duration::from_nanos((player::playpos_ns() - pos0).max(0) as u64);
    let wall = wall0.elapsed();
    eprintln!(
        "e2e: live {} s of wall time, playhead +{} ms, worst playhead gap {} ms, {} restarts",
        wall.as_secs(),
        played.as_millis(),
        worst_gap.as_millis(),
        recovered(&rig.events)
    );
    assert_eq!(recovered(&rig.events), 0, "the watchdog never fired");
    assert_eq!(rig.pb.status().state, State::Playing);
    assert!(played + Duration::from_secs(3) >= wall, "the playhead follows wall time across every break");
    assert!(worst_gap < Duration::from_secs(2), "no freeze at a break: the playhead stood still for {} ms", worst_gap.as_millis());
}
