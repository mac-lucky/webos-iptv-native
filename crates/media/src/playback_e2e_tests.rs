//! **[`Playback`] end to end on the desktop simulator path**: the real demuxer over an FFmpeg of the
//! bundled ABI (majors 63/63/61), the host clock sink (`player::ffi_host`), the real curl
//! transport, and a loopback origin serving real MPEG-TS segments: a live channel as a sliding
//! window of six 6 s segments, and archive playlists (`EVENT` + `ENDLIST`).
//!
//! Ignored by default: they run in real time (about twelve minutes together with the defaults
//! below). They provision themselves and skip, saying why, when they cannot:
//!
//! - The demuxer's libraries: `WEBOSIPTV_APP_DIR` when set; else the bundled FFmpeg that
//!   `cargo xtask ffmpeg --host` stages in `pkg/` (the exact build the TV ships); else `-wi` links
//!   under `target/` to an FFmpeg 9's dylibs in `FFMPEG_LIB_DIR`, `/opt/homebrew/lib` or
//!   `/usr/local/lib`.
//! - The segments: `WEBOSIPTV_E2E_HLS` / `WEBOSIPTV_E2E_HLS_2AUDIO` when set, else generated once
//!   by the `ffmpeg` on `PATH` under `target/e2e-fixtures/` (35 min of 320x180 H.264 + AAC in 6 s
//!   segments; 3 min with two AAC tracks tagged `pol` and `eng`) and reused.
//! - `WEBOSIPTV_E2E_MINUTES`: how long the live channel plays (default 2; the soak is 30).
//! - `WEBOSIPTV_E2E_PCM`: the memory tests decode the audio to PCM, as webOS 10+ does (macOS only:
//!   they read libmalloc's heap in use).
//!
//! `cargo xtask ffmpeg --host`, then
//! `cargo test -p tv_media --features hostsim e2e_ -- --ignored --test-threads=1 --nocapture`

use super::*;
use crate::hls::loopback_tests::{Answer, Origin};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The live window: six segments, as the provider's live playlists list them.
const WINDOW: usize = 6;
const SEG_S: f64 = 6.0;
const PROGRAMME: i64 = 1_760_000_000;

struct Fixture {
    segments: Vec<Vec<u8>>,
}

/// The fixture and a booted FFmpeg, or `None` (with the reason printed) to skip.
fn fixture() -> Option<std::sync::Arc<Fixture>> {
    fixture_from("WEBOSIPTV_E2E_HLS", false, 220)
}

fn fixture_from(var: &str, two_audio: bool, at_least: usize) -> Option<std::sync::Arc<Fixture>> {
    // Before anything reads the app directory: `paths::app_dir` resolves once per process.
    static BOOT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    if let Err(why) = BOOT.get_or_init(boot) {
        eprintln!("skipped: {why}");
        return None;
    }
    let dir = match std::env::var_os(var) {
        Some(d) => PathBuf::from(d),
        None => match generated(two_audio) {
            Ok(d) => d,
            Err(why) => {
                eprintln!("skipped: {var} is not set and no fixture could be generated ({why})");
                return None;
            }
        },
    };
    // A relative path is read from the crate's directory, where cargo runs the tests.
    let list = match std::fs::read_to_string(dir.join("full.m3u8")) {
        Ok(list) => list,
        Err(e) => {
            eprintln!("skipped: no fixture playlist at {} ({e})", dir.join("full.m3u8").display());
            return None;
        }
    };
    let segments: Vec<Vec<u8>> = list
        .lines()
        .filter(|l| l.ends_with(".ts"))
        .map(|name| std::fs::read(dir.join(name)).expect("a segment the playlist names"))
        .collect();
    if segments.len() < at_least {
        eprintln!("skipped: the fixture has {} segments, {at_least} are needed", segments.len());
        return None;
    }
    Some(std::sync::Arc::new(Fixture { segments }))
}

/// Stage the libraries (unless `WEBOSIPTV_APP_DIR` names them) and boot curl and FFmpeg.
fn boot() -> Result<(), String> {
    if std::env::var_os("WEBOSIPTV_APP_DIR").is_none() {
        let dir = stage_libraries()?;
        // Edition 2021: a safe call; this runs once, before any thread reads the variable.
        std::env::set_var("WEBOSIPTV_APP_DIR", &dir);
    }
    tv_net::net::global_init();
    crate::ff::boot();
    if !crate::ff::ready() {
        return Err(format!("no FFmpeg of the bundled ABI (63/63/61) in {}", tv_base::paths::app_dir().display()));
    }
    if !crate::curlio::available() {
        return Err("no libcurl".into());
    }
    Ok(())
}

/// The repository's `target/`.
fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| repo().join("target"), PathBuf::from)
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `pkg/` when it holds the bundled host build, else a directory of `-wi` links to a host
/// FFmpeg 9's dylibs.
fn stage_libraries() -> Result<PathBuf, String> {
    const LIBS: [(&str, &str); 4] = [("avutil", "61"), ("swresample", "7"), ("avcodec", "63"), ("avformat", "63")];
    let pkg = repo().join("pkg");
    if LIBS.iter().all(|(n, m)| pkg.join(format!("lib{n}-wi.{m}.dylib")).exists()) {
        return Ok(pkg);
    }
    let candidates: Vec<PathBuf> = std::env::var_os("FFMPEG_LIB_DIR")
        .map(PathBuf::from)
        .into_iter()
        .chain(["/opt/homebrew/lib", "/usr/local/lib"].map(PathBuf::from))
        .collect();
    let src = candidates
        .iter()
        .find(|d| LIBS.iter().all(|(n, m)| d.join(format!("lib{n}.{m}.dylib")).exists()))
        .ok_or_else(|| "no FFmpeg 9 dylibs in FFMPEG_LIB_DIR, /opt/homebrew/lib or /usr/local/lib".to_owned())?;
    let dir = target_dir().join("e2e-fixtures/appdir");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for (n, m) in LIBS {
        let link = dir.join(format!("lib{n}-wi.{m}.dylib"));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(src.join(format!("lib{n}.{m}.dylib")), &link).map_err(|e| e.to_string())?;
    }
    Ok(dir)
}

/// The generated fixture's directory, made by the `ffmpeg` on `PATH` the first time.
fn generated(two_audio: bool) -> Result<PathBuf, String> {
    let dir = target_dir().join(if two_audio { "e2e-fixtures/hls-2audio" } else { "e2e-fixtures/hls" });
    if dir.join("full.m3u8").exists() {
        return Ok(dir);
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    eprintln!("e2e: generating the {} fixture in {} (once)", if two_audio { "two-audio" } else { "35 min" }, dir.display());
    let video = ["-f", "lavfi", "-i", "testsrc2=size=320x180:rate=25"];
    let tone = |hz: u32| ["-f".to_owned(), "lavfi".to_owned(), "-i".to_owned(), format!("sine=frequency={hz}:sample_rate=48000")];
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error"].iter().chain(video.iter()).map(|a| a.to_string()).collect();
    args.extend(tone(440));
    if two_audio {
        args.extend(tone(880));
        args.extend(["-t", "180", "-map", "0:v", "-map", "1:a", "-map", "2:a", "-metadata:s:a:0", "language=pol", "-metadata:s:a:1", "language=eng"].map(String::from));
    } else {
        args.extend(["-t", "2100"].map(String::from));
    }
    args.extend(
        [
            "-c:v", "libx264", "-preset", "ultrafast", "-g", "50", "-keyint_min", "50", "-sc_threshold", "0", "-b:v", "120k",
            "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "48k", "-ac", "2", "-f", "hls", "-hls_time", "6", "-hls_list_size",
            "0", "-hls_segment_filename",
        ]
        .map(String::from),
    );
    args.push(path_str(&dir.join("seg%04d.ts"))?);
    args.push(path_str(&dir.join("full.tmp.m3u8"))?);
    let ok = std::process::Command::new("ffmpeg").args(&args).status().map_err(|e| format!("ffmpeg: {e}"))?.success();
    if !ok {
        return Err("ffmpeg failed".into());
    }
    std::fs::rename(dir.join("full.tmp.m3u8"), dir.join("full.m3u8")).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn path_str(p: &Path) -> Result<String, String> {
    p.to_str().map(str::to_owned).ok_or_else(|| "a temp path that is not UTF-8".into())
}

fn seg_path(i: usize) -> String {
    format!("/seg/{i:04}.ts")
}

fn serve_segments(o: &Origin, fx: &Fixture, range: std::ops::Range<usize>) {
    for i in range {
        o.put(&seg_path(i), Answer::ok(fx.segments[i].clone()));
    }
}

/// A finite archive playlist of segments `range` (the provider's `EVENT` + `ENDLIST` shape).
fn archive(range: std::ops::Range<usize>) -> Answer {
    archive_with_breaks(&[range])
}

/// [`archive`] of `runs`, an `EXT-X-DISCONTINUITY` before each run after the first.
fn archive_with_breaks(runs: &[std::ops::Range<usize>]) -> Answer {
    let mut p = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:{}\n#EXT-X-PLAYLIST-TYPE:EVENT\n", runs[0].start);
    for (n, run) in runs.iter().enumerate() {
        if n > 0 {
            p.push_str("#EXT-X-DISCONTINUITY\n");
        }
        for i in run.clone() {
            p.push_str(&format!("#EXTINF:6.000,\n{}\n", seg_path(i)));
        }
    }
    p.push_str("#EXT-X-ENDLIST\n");
    Answer::playlist(p)
}

/// A live channel at `path`: a window of [`WINDOW`] segments from `first`, its edge advancing one
/// segment per 6 s of wall time, ending (`ENDLIST`) when the fixture runs out.
fn serve_live(o: &Origin, path: &str, fx: &Fixture, first: usize) {
    serve_live_freezable(o, path, fx, first, Arc::default());
}

/// [`serve_live`] whose playlist stops growing while `frozen` is set (a packager that hung), and
/// catches up with wall time once it is cleared.
fn serve_live_freezable(o: &Origin, path: &str, fx: &Fixture, first: usize, frozen: Arc<std::sync::atomic::AtomicBool>) {
    serve_segments(o, fx, first..fx.segments.len());
    let (t0, last) = (Instant::now(), fx.segments.len() - 1);
    let mut held: Option<usize> = None;
    o.route(path, move || {
        let edge = (first + WINDOW - 1 + (t0.elapsed().as_secs_f64() / SEG_S) as usize).min(last);
        held = frozen.load(std::sync::atomic::Ordering::Relaxed).then(|| held.unwrap_or(edge));
        let newest = held.unwrap_or(edge);
        let oldest = newest + 1 - WINDOW;
        let mut p = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:{oldest}\n");
        for i in oldest..=newest {
            p.push_str(&format!("#EXTINF:6.000,\n{}\n", seg_path(i)));
        }
        if newest == last {
            p.push_str("#EXT-X-ENDLIST\n");
        }
        Answer::playlist(p)
    });
}

/// Arms the host clock sink for the test and disarms it on the way out.
struct ClockSink;
impl ClockSink {
    fn arm() -> ClockSink {
        crate::player::ffi_host::force_clocksink_for_test(true);
        ClockSink
    }
}
impl Drop for ClockSink {
    fn drop(&mut self) {
        crate::player::ffi_host::force_clocksink_for_test(false);
    }
}

/// The playback and a frame clock.
struct Rig {
    pb: Playback,
    t0: Instant,
    events: Vec<(u32, Event)>,
}

impl Rig {
    fn new() -> Rig {
        // SAFETY: this test thread is the only one that drives the player, as the main thread does.
        let mt = unsafe { MainThread::assume() };
        Rig { pb: Playback::new(mt), t0: Instant::now(), events: Vec::new() }
    }
    fn now(&self) -> u32 {
        self.t0.elapsed().as_millis() as u32
    }
    /// Tick at 60 Hz until `done` or `limit`; `true` when `done` held.
    fn run_until(&mut self, limit: Duration, mut done: impl FnMut(&Status, &[(u32, Event)]) -> bool) -> bool {
        let until = Instant::now() + limit;
        while Instant::now() < until {
            let now = self.now();
            self.pb.tick(now);
            while let Some(e) = self.pb.take_event() {
                eprintln!("e2e: {:>6} ms {e:?}", now);
                self.events.push((now, e));
            }
            if done(&self.pb.status(), &self.events) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        false
    }
    fn stop(&mut self) {
        self.pb.stop();
        let _ = self.run_until(Duration::from_millis(500), |_, _| false);
    }
}

fn recovered(events: &[(u32, Event)]) -> usize {
    events.iter().filter(|(_, e)| *e == Event::Recovered).count()
}

/// **A live channel plays through its sliding window**: the first frame within seconds of the
/// zap, the playhead following wall time, the playlist reloaded on the RFC 8216 cadence (one
/// target duration after a changed load), and the watchdog never needed.
#[test]
#[ignore]
fn e2e_live_channel_plays_through_its_sliding_window_without_a_stall() {
    let Some(fx) = fixture() else { return };
    let _serial = tv_base::testlock::serial();
    let _sink = ClockSink::arm();
    let minutes: u64 = std::env::var("WEBOSIPTV_E2E_MINUTES").ok().and_then(|m| m.parse().ok()).unwrap_or(2);
    let o = Origin::spawn();
    let live = "/ch7/mono.m3u8?token=e2e-secret";
    serve_live(&o, live, &fx, 0);
    let mut rig = Rig::new();
    rig.pb.play_live(&o.url(live), crate::headers::RequestHeaders::none());
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.first_frame_ms.is_some()), "no first frame within 20 s");
    let st = rig.pb.status();
    let ttff = st.first_frame_ms.unwrap();
    eprintln!("e2e: first frame after {ttff} ms, {:?}", st.media);
    assert_eq!((st.mode, st.media.width, st.media.height), (Mode::Live, 320, 180));
    assert_eq!((st.media.video_codec.as_str(), st.media.audio_codec.as_str(), st.media.audio_channels), ("h264", "aac", 2));
    let (pos0, wall0) = (player::playpos_ns(), Instant::now());
    let mut worst_gap = Duration::ZERO;
    let (mut last_pos, mut moved_at) = (pos0, Instant::now());
    rig.run_until(Duration::from_secs(minutes * 60), |s, _| {
        let pos = player::playpos_ns();
        if pos != last_pos {
            worst_gap = worst_gap.max(moved_at.elapsed());
            (last_pos, moved_at) = (pos, Instant::now());
        }
        s.state != State::Playing
    });
    let st = rig.pb.status();
    let played = Duration::from_nanos((player::playpos_ns() - pos0).max(0) as u64);
    let wall = wall0.elapsed();
    let reloads = o.paths().iter().filter(|p| p.as_str() == live).count();
    eprintln!(
        "e2e: live {} s of wall time, playhead +{} ms, worst playhead gap {} ms, {reloads} playlist loads, state {:?}",
        wall.as_secs(),
        played.as_millis(),
        worst_gap.as_millis(),
        st.state
    );
    assert_eq!(st.state, State::Playing, "still playing at the end");
    assert_eq!(recovered(&rig.events), 0, "the watchdog never fired");
    assert!(played + Duration::from_secs(3) >= wall && played <= wall + Duration::from_secs(3), "the playhead follows wall time");
    assert!(worst_gap < Duration::from_secs(2), "no stall: the playhead never stood still");
    let cadence = wall.as_secs_f64() / SEG_S;
    assert!((reloads as f64) >= cadence - 2.0 && (reloads as f64) <= 2.0 * cadence + 4.0, "reloads follow the target duration");
    rig.stop();
}

/// **A past programme from the archive**: it starts at the segment holding its offset, reads in
/// programme time although its segments' timestamps start ten minutes in, seeks by segment, and
/// returns to live at its end.
#[test]
#[ignore]
fn e2e_catchup_starts_at_its_offset_seeks_by_segment_and_returns_to_live_at_the_end() {
    let Some(fx) = fixture() else { return };
    let _serial = tv_base::testlock::serial();
    let _sink = ClockSink::arm();
    let o = Origin::spawn();
    // Ten segments from 10 min into the recording: a 60 s programme.
    serve_segments(&o, &fx, 100..110);
    let window = format!("/ch7/mono-{PROGRAMME}-60.m3u8?token=e2e-secret");
    o.put(&window, archive(100..110));
    let live = "/ch7/mono.m3u8?token=e2e-secret";
    serve_live(&o, live, &fx, 200);
    let mut rig = Rig::new();
    let shift_base = o.url("/ch7/mono.m3u8?token=e2e-secret");
    rig.pb.play_catchup(Catchup {
        start: PROGRAMME,
        end: PROGRAMME + 60,
        offset_secs: 20,
        window: Some(o.url(&window)),
        shift: Some(Arc::new(move |utc| format!("{shift_base}&utc={utc}&lutc={utc}"))),
        live_url: o.url(live),
        headers: crate::headers::RequestHeaders::from_pairs(&[("User-Agent", "e2e-agent/1.0")]).unwrap(),
        at_end: AtEnd::ReturnToLive,
    });
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.state == State::Playing), "no picture within 20 s");
    let at = rig.pb.status().catchup.expect("a catch-up position");
    eprintln!("e2e: catch-up first frame after {:?} ms at {} ms of {}", rig.pb.status().first_frame_ms, at.position_ms, at.duration_ms);
    assert_eq!(at.duration_ms, 60_000);
    assert!((17_000..23_000).contains(&at.position_ms), "starts at the segment holding 20 s: {}", at.position_ms);

    rig.pb.seek(45_000);
    let landed = rig.run_until(Duration::from_secs(15), |s, _| {
        s.state == State::Playing && s.catchup.is_some_and(|c| c.position_ms >= 41_000)
    });
    let at = rig.pb.status().catchup.unwrap();
    eprintln!("e2e: seek to 45 s landed at {} ms", at.position_ms);
    assert!(landed && at.position_ms < 50_000, "the seek lands on the segment holding 45 s: {}", at.position_ms);

    assert!(rig.run_until(Duration::from_secs(40), |_, ev| ev.iter().any(|(_, e)| *e == Event::ReturnedToLive)), "no return to live at the end");
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.mode == Mode::Live && s.state == State::Playing), "live after the programme");
    assert_eq!(recovered(&rig.events), 0);
    let requests = o.requests();
    assert!(
        requests.iter().all(|r| r.lines().any(|l| l.eq_ignore_ascii_case("user-agent: e2e-agent/1.0"))),
        "the channel's headers on every archive and live request"
    );
    rig.stop();
}

/// **A window that does not load falls back to shift**, and a redirect to the no-archive stub is
/// reported as such rather than as a failure.
#[test]
#[ignore]
fn e2e_a_failed_window_falls_back_to_shift_and_the_no_archive_stub_is_reported() {
    let Some(fx) = fixture() else { return };
    let _serial = tv_base::testlock::serial();
    let _sink = ClockSink::arm();
    let o = Origin::spawn();
    serve_segments(&o, &fx, 100..110);
    let window = format!("/ch7/index-{PROGRAMME}-60.m3u8?token=e2e-secret");
    o.put(&window, Answer::not_found());
    let shift = format!("/ch7/index.m3u8?token=e2e-secret&utc={PROGRAMME}&lutc={PROGRAMME}");
    o.put(&shift, archive(100..110));
    let base = o.url("/ch7/index.m3u8?token=e2e-secret");
    let shift_of: ShiftUrl = Arc::new(move |utc| format!("{base}&utc={utc}&lutc={utc}"));
    let mut rig = Rig::new();
    let programme = |window: String| Catchup {
        start: PROGRAMME,
        end: PROGRAMME + 60,
        offset_secs: 0,
        window: Some(window),
        shift: Some(Arc::clone(&shift_of)),
        live_url: String::new(),
        headers: crate::headers::RequestHeaders::none(),
        at_end: AtEnd::Continue,
    };
    rig.pb.play_catchup(programme(o.url(&window)));
    assert!(rig.run_until(Duration::from_secs(25), |s, _| s.state == State::Playing), "the shift fallback never played");
    assert!(o.paths().contains(&shift), "played from the shift URL");
    let at = rig.pb.status().catchup.unwrap();
    assert!(at.position_ms < 5_000, "from the programme's start: {}", at.position_ms);

    let gone = format!("/ch7/index-{}-60.m3u8?token=e2e-secret", PROGRAMME - 30 * 86_400);
    o.put(&gone, Answer::redirect("/new_stub/noarchive.m3u8"));
    o.put("/new_stub/noarchive.m3u8", archive(100..101));
    rig.pb.play_catchup(Catchup { shift: None, ..programme(o.url(&gone)) });
    assert!(rig.run_until(Duration::from_secs(15), |s, _| s.state == State::NoArchive), "the stub was not reported: {:?}", rig.pb.status().state);
    rig.stop();
}

/// **The stall watchdog**: a live packager that hangs leaves the playhead standing once the
/// buffered segments run out; the channel is restarted at its live edge and plays on.
#[test]
#[ignore]
fn e2e_a_stalled_live_channel_is_restarted_at_its_live_edge() {
    let Some(fx) = fixture() else { return };
    let _serial = tv_base::testlock::serial();
    let _sink = ClockSink::arm();
    let o = Origin::spawn();
    let live = "/ch9/mono.m3u8?token=e2e-secret";
    let frozen: Arc<std::sync::atomic::AtomicBool> = Arc::default();
    serve_live_freezable(&o, live, &fx, 0, Arc::clone(&frozen));
    let mut rig = Rig::new();
    rig.pb.play_live(&o.url(live), crate::headers::RequestHeaders::none());
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.state == State::Playing));
    let _ = rig.run_until(Duration::from_secs(5), |_, _| false);
    frozen.store(true, std::sync::atomic::Ordering::Relaxed);
    let froze = rig.now();
    assert!(rig.run_until(Duration::from_secs(60), |_, ev| recovered(ev) > 0), "the watchdog never restarted the stalled channel");
    let restarted = rig.events.iter().find(|(_, e)| *e == Event::Recovered).unwrap().0;
    eprintln!("e2e: frozen at {froze} ms, restarted at {restarted} ms");
    frozen.store(false, std::sync::atomic::Ordering::Relaxed);
    assert!(rig.run_until(Duration::from_secs(40), |s, _| s.state == State::Playing && s.first_frame_ms.is_some()), "no picture after the restart");
    let p0 = player::playpos_ns();
    let _ = rig.run_until(Duration::from_secs(15), |_, _| false);
    let moved = player::playpos_ns() - p0;
    eprintln!("e2e: after the restart the playhead moved {} ms in 15 s; {} restart(s)", moved / 1_000_000, recovered(&rig.events));
    assert!(moved >= 13_000_000_000, "playing again: {moved}");
    assert_eq!(rig.pb.status().mode, Mode::Live);
    rig.stop();
}

/// **A catch-up whose segment fails mid-programme resumes at its position**, not at the start.
#[test]
#[ignore]
fn e2e_a_catchup_that_fails_mid_programme_resumes_at_its_position() {
    let Some(fx) = fixture() else { return };
    let _serial = tv_base::testlock::serial();
    let _sink = ClockSink::arm();
    let o = Origin::spawn();
    serve_segments(&o, &fx, 100..110);
    // Segment 107 (42 s) answers 503 for its first three requests: the source's retries run out
    // and the stream fails, a few seconds into the programme.
    let (bytes, mut fails) = (fx.segments[107].clone(), 3);
    o.route(&seg_path(107), move || {
        if fails > 0 {
            fails -= 1;
            return Answer::status(503);
        }
        Answer::ok(bytes.clone())
    });
    let window = format!("/ch9/mono-{PROGRAMME}-60.m3u8?token=e2e-secret");
    o.put(&window, archive(100..110));
    let mut rig = Rig::new();
    rig.pb.play_catchup(Catchup {
        start: PROGRAMME,
        end: PROGRAMME + 60,
        offset_secs: 30,
        window: Some(o.url(&window)),
        shift: None,
        live_url: String::new(),
        headers: crate::headers::RequestHeaders::none(),
        at_end: AtEnd::Continue,
    });
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.state == State::Playing));
    assert!(rig.run_until(Duration::from_secs(30), |_, ev| recovered(ev) > 0), "the failure was not recovered");
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.state == State::Playing && s.first_frame_ms.is_some()));
    let at = rig.pb.status().catchup.unwrap();
    eprintln!("e2e: resumed at {} ms", at.position_ms);
    assert!(at.position_ms >= 29_000, "resumed at its position, not the programme's start: {}", at.position_ms);
    assert!(rig.run_until(Duration::from_secs(40), |_, ev| ev.iter().any(|(_, e)| *e == Event::ProgrammeEnded)), "played on to the end");
    rig.stop();
}

/// **Audio tracks**: both of a bilingual stream are listed with their languages, the viewer's
/// language picks the start track, and a pick reloads at the position on the other track.
#[test]
#[ignore]
fn e2e_audio_tracks_are_listed_preferred_and_switched_at_the_position() {
    let Some(fx) = fixture_from("WEBOSIPTV_E2E_HLS_2AUDIO", true, 20) else { return };
    let _serial = tv_base::testlock::serial();
    let _sink = ClockSink::arm();
    let o = Origin::spawn();
    serve_segments(&o, &fx, 0..20);
    let window = format!("/ch2/mono-{PROGRAMME}-120.m3u8?token=e2e-secret");
    o.put(&window, archive(0..20));
    let mut rig = Rig::new();
    rig.pb.set_audio_languages(vec!["EN".into()]);
    rig.pb.play_catchup(Catchup {
        start: PROGRAMME,
        end: PROGRAMME + 120,
        offset_secs: 0,
        window: Some(o.url(&window)),
        shift: None,
        live_url: String::new(),
        headers: crate::headers::RequestHeaders::none(),
        at_end: AtEnd::Continue,
    });
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.state == State::Playing));
    let tracks = rig.pb.audio_tracks();
    eprintln!("e2e: tracks {tracks:?}");
    let langs: Vec<_> = tracks.iter().map(|t| (t.language.as_str(), t.selected, t.playable)).collect();
    assert_eq!(langs, [("pol", false, true), ("eng", true, true)], "the English preference picked track 1");
    let _ = rig.run_until(Duration::from_secs(8), |_, _| false);
    let before = rig.pb.status().catchup.unwrap().position_ms;
    rig.pb.select_audio_track(0).expect("the Polish track plays");
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.state == State::Playing && player::seen_frame() && s.catchup.is_some_and(|c| c.position_ms > 0)));
    let after = rig.pb.status().catchup.unwrap().position_ms;
    let selected: Vec<_> = rig.pb.audio_tracks().iter().map(|t| t.selected).collect();
    eprintln!("e2e: switched at {before} ms, playing again at {after} ms, selected {selected:?}");
    assert_eq!(selected, [true, false]);
    assert!(after + 7_000 >= before, "reloaded at the position (by segment), not from the start: {before} -> {after}");
    rig.stop();
}

/// **A programme longer than one shift request answers** is played in parts: each request is
/// for the archive from where the last part ended, and the playhead runs on in programme time.
/// The 3 h of the provider's shift is scaled down to 30 s parts here (the server's choice; the
/// player only follows where a part ended).
#[test]
#[ignore]
fn e2e_a_programme_longer_than_one_shift_answer_is_played_in_parts() {
    let Some(fx) = fixture() else { return };
    let _serial = tv_base::testlock::serial();
    let _sink = ClockSink::arm();
    let o = Origin::spawn();
    serve_segments(&o, &fx, 100..110);
    let base = o.url("/ch5/mono.m3u8?token=e2e-secret");
    // Each shift request answers 30 s (five segments) from its `utc`.
    for part in 0..2 {
        let utc = PROGRAMME + part * 30;
        let first = 100 + part as usize * 5;
        o.put(&format!("/ch5/mono.m3u8?token=e2e-secret&utc={utc}&lutc={utc}"), archive(first..first + 5));
    }
    let live = "/ch5/live.m3u8?token=e2e-secret";
    serve_live(&o, live, &fx, 200);
    let mut rig = Rig::new();
    rig.pb.play_catchup(Catchup {
        start: PROGRAMME,
        end: PROGRAMME + 60,
        offset_secs: 0,
        window: None,
        shift: Some(Arc::new(move |utc| format!("{base}&utc={utc}&lutc={utc}"))),
        live_url: o.url(live),
        headers: crate::headers::RequestHeaders::none(),
        at_end: AtEnd::ReturnToLive,
    });
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.state == State::Playing));
    assert!(rig.run_until(Duration::from_secs(45), |s, _| s.catchup.is_some_and(|c| c.position_ms >= 33_000)), "the second part never played");
    let at = rig.pb.status().catchup.unwrap();
    eprintln!("e2e: second part at {} ms", at.position_ms);
    assert!(o.paths().iter().any(|p| p.contains(&format!("utc={}", PROGRAMME + 30))), "the second request started at 30 s");
    assert!(rig.run_until(Duration::from_secs(40), |_, ev| ev.iter().any(|(_, e)| *e == Event::ReturnedToLive)), "no return to live at the end");
    assert_eq!(recovered(&rig.events), 0);
    rig.stop();
}

/// **A time shift of the programme on air plays on in one stream**. The shift answer
/// ends at the archive's edge as it was when asked for (`EVENT` + `ENDLIST`, the edge advancing a
/// segment per 6 s of wall time); 90 s behind live, the C1 played that as one Load per ~95 s
/// part. Followed, the one part plays past its first answer's end with no second start.
#[test]
#[ignore]
fn e2e_a_time_shift_of_the_programme_on_air_plays_past_the_archive_edge_without_a_reload() {
    let Some(fx) = fixture() else { return };
    let _serial = tv_base::testlock::serial();
    let _sink = ClockSink::arm();
    let o = Origin::spawn();
    serve_segments(&o, &fx, 0..fx.segments.len());
    // Ten minutes into a programme on air: segment 99 (594-600 s) is the archive's edge now.
    let start = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 - 600;
    let base = o.url("/ch3/mono.m3u8?token=e2e-secret");
    let (t0, last) = (Instant::now(), fx.segments.len() - 1);
    o.route(&format!("/ch3/mono.m3u8?token=e2e-secret&utc={start}"), move || {
        let edge = (99 + (t0.elapsed().as_secs_f64() / SEG_S) as usize).min(last);
        let mut p =
            String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:EVENT\n");
        for i in 0..=edge {
            p.push_str(&format!("#EXTINF:6.000,\n{}\n", seg_path(i)));
        }
        p.push_str("#EXT-X-ENDLIST\n");
        Answer::playlist(p)
    });
    let live = "/ch3/live.m3u8?token=e2e-secret";
    serve_live(&o, live, &fx, 100);
    let mut rig = Rig::new();
    rig.pb.play_catchup(Catchup {
        start,
        end: start + 3600,
        offset_secs: 510,
        window: None,
        shift: Some(Arc::new(move |utc| format!("{base}&utc={utc}"))),
        live_url: o.url(live),
        headers: crate::headers::RequestHeaders::none(),
        at_end: AtEnd::Continue,
    });
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.state == State::Playing), "no picture within 20 s");
    let played_on = rig.run_until(Duration::from_secs(120), |s, _| s.catchup.is_some_and(|c| c.position_ms >= 615_000));
    let at = rig.pb.status().catchup.unwrap();
    eprintln!("e2e: time shift at {} ms, {:?}", at.position_ms, rig.pb.status().state);
    assert!(played_on, "did not play past the first answer's end (600 s): {} ms", at.position_ms);
    let starts = rig.events.iter().filter(|(_, e)| matches!(e, Event::FirstFrame { .. })).count();
    assert_eq!(starts, 1, "one start, no part reloads: {:?}", rig.events);
    let playlists: Vec<String> = o.paths().into_iter().filter(|p| p.starts_with("/ch3/mono.m3u8")).collect();
    assert!(playlists.iter().all(|p| p.ends_with(&format!("utc={start}"))), "only the one part asked for");
    assert!(playlists.len() >= 3, "its playlist asked for again as the edge grew: {}", playlists.len());
    assert_eq!(recovered(&rig.events), 0);
    rig.stop();
}

/// **Zapping**: twenty channel changes between two channels, each a clean teardown and a fresh
/// start with its own first frame; the last one plays on.
#[test]
#[ignore]
fn e2e_twenty_zaps_each_tear_down_and_start_with_a_first_frame() {
    let Some(fx) = fixture() else { return };
    let _serial = tv_base::testlock::serial();
    let _sink = ClockSink::arm();
    let o = Origin::spawn();
    let (a, b) = ("/za/mono.m3u8?token=e2e-secret", "/zb/mono.m3u8?token=e2e-secret");
    serve_live(&o, a, &fx, 0);
    serve_live(&o, b, &fx, 100);
    let mut rig = Rig::new();
    let mut ttff = Vec::new();
    for i in 0..20 {
        rig.pb.play_live(&o.url(if i % 2 == 0 { a } else { b }), crate::headers::RequestHeaders::none());
        assert!(rig.run_until(Duration::from_secs(15), |s, _| s.first_frame_ms.is_some()), "zap {i}: no first frame");
        ttff.push(rig.pb.status().first_frame_ms.unwrap());
    }
    eprintln!("e2e: time to first frame per zap (ms): {ttff:?}");
    let p0 = player::playpos_ns();
    let _ = rig.run_until(Duration::from_secs(5), |_, _| false);
    assert!(player::playpos_ns() - p0 >= 4_000_000_000, "the last channel plays on");
    assert_eq!(recovered(&rig.events), 0);
    rig.stop();
}

/// The process's malloc heap in use, in kB: every zone, so FFmpeg's and curl's allocations as well
/// as ours. `None` off macOS, where the memory tests skip.
fn heap_kb() -> Option<usize> {
    heap_stats_kb().map(|(used, _)| used)
}

/// The heap in use and what the allocator holds from the system for it (in use plus freed and
/// kept), kB.
fn heap_stats_kb() -> Option<(usize, usize)> {
    #[cfg(target_os = "macos")]
    {
        #[repr(C)]
        struct MStats {
            bytes_total: usize,
            chunks_used: usize,
            bytes_used: usize,
            chunks_free: usize,
            bytes_free: usize,
        }
        extern "C" {
            fn mstats() -> MStats;
        }
        // SAFETY: libmalloc's statistics call, no arguments; it sums every malloc zone.
        let m = unsafe { mstats() };
        Some((m.bytes_used / 1024, m.bytes_total / 1024))
    }
    #[cfg(not(target_os = "macos"))]
    None
}

/// The memory tests' fixture (a short one will do), or `None` to skip, also off macOS.
fn memory_fixture() -> Option<std::sync::Arc<Fixture>> {
    let fx = fixture_from("WEBOSIPTV_E2E_HLS", false, 20)?;
    if heap_kb().is_none() {
        eprintln!("skipped: the heap is read on macOS only");
        return None;
    }
    Some(fx)
}

/// Decode every audio track to PCM, as webOS 10+ does (`AudioFeed::SoftPcm`), when
/// `WEBOSIPTV_E2E_PCM` is set: webOS 10 is published before anything reads the codec table.
fn pcm_like_webos10() -> bool {
    if std::env::var_os("WEBOSIPTV_E2E_PCM").is_none() {
        return false;
    }
    tv_platform::tv::device::publish_info(tv_platform::tv::device::Info { major: 10, ..Default::default() });
    tv_platform::devcaps::probe();
    let feed = AudioFeed::decide("aac", 2, tv_platform::devcaps::caps(), SoftwareAudio::detected());
    assert!(matches!(feed, AudioFeed::SoftPcm { .. }), "the AAC track decodes to PCM here: {feed:?}");
    true
}

/// Stop, let the release finish, and read the heap ([`heap_stats_kb`]: in use, and held for it):
/// what is left with nothing playing, so the queues' fill level (megabytes, varying by the
/// second) is not in the number.
fn stopped_heap_kb(rig: &mut Rig) -> (usize, usize) {
    rig.stop();
    let _ = rig.run_until(Duration::from_secs(2), |_, _| false);
    heap_stats_kb().expect("measured on macOS")
}

/// What a session may leave behind on the heap: well under one segment of the C5's live stream,
/// and far under the ~3 MB a 10-minute run there grew by.
const RETAINED_BUDGET_KB: usize = 512;

/// **Memory over a live run**: once a first play has made the one-time allocations, a
/// live channel played for `WEBOSIPTV_E2E_MINUTES` leaves the heap where it was (stopped against
/// stopped), and the heap in use while it plays is printed every 30 s. `WEBOSIPTV_E2E_PCM=1` plays
/// it through the webOS 10+ software audio path. A fixture at the C5's rate (1080p25, ~6 Mbit/s)
/// goes in `WEBOSIPTV_E2E_HLS`; it must outlast a minute plus the run.
#[test]
#[ignore]
fn e2e_live_playback_retains_no_memory() {
    let Some(fx) = memory_fixture() else { return };
    let _serial = tv_base::testlock::serial();
    let pcm = pcm_like_webos10();
    let _sink = ClockSink::arm();
    let minutes: u64 = std::env::var("WEBOSIPTV_E2E_MINUTES").ok().and_then(|m| m.parse().ok()).unwrap_or(2);
    let o = Origin::spawn();
    let live = "/ch9/mono.m3u8?token=e2e-secret";
    serve_live(&o, live, &fx, 0);
    let mut rig = Rig::new();
    rig.pb.play_live(&o.url(live), crate::headers::RequestHeaders::none());
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.first_frame_ms.is_some()), "no first frame within 20 s");
    let _ = rig.run_until(Duration::from_secs(60), |_, _| false);
    let (before, _) = stopped_heap_kb(&mut rig);
    rig.pb.play_live(&o.url(live), crate::headers::RequestHeaders::none());
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.first_frame_ms.is_some()), "no first frame within 20 s");
    let (t0, mut next) = (Instant::now(), Duration::ZERO);
    rig.run_until(Duration::from_secs(minutes * 60), |s, _| {
        if t0.elapsed() >= next {
            eprintln!("e2e: mem t={:>4} s heap={} kB pcm={pcm}", t0.elapsed().as_secs(), heap_kb().unwrap_or(0));
            next += Duration::from_secs(30);
        }
        s.state != State::Playing
    });
    assert_eq!(rig.pb.status().state, State::Playing, "still playing at the end");
    let (after, _) = stopped_heap_kb(&mut rig);
    eprintln!("e2e: mem stopped before {before} kB, after {minutes} min {after} kB ({:+} kB) pcm={pcm}", after as i64 - before as i64);
    assert!(after < before + RETAINED_BUDGET_KB, "{minutes} min of live play retained {} kB", after - before);
}

/// **Memory over a zap series** (the C5's ten-zap check): after a first zap, ten more
/// between two channels, each played to its first frame and on for 3 s, leave the heap where the
/// first left it (stopped against stopped). `WEBOSIPTV_E2E_PCM=1` as above.
#[test]
#[ignore]
fn e2e_ten_zaps_retain_no_memory() {
    let Some(fx) = memory_fixture() else { return };
    let _serial = tv_base::testlock::serial();
    let pcm = pcm_like_webos10();
    let _sink = ClockSink::arm();
    let o = Origin::spawn();
    let (a, b) = ("/za/mono.m3u8?token=e2e-secret", "/zb/mono.m3u8?token=e2e-secret");
    serve_live(&o, a, &fx, 0);
    serve_live(&o, b, &fx, fx.segments.len() / 2);
    let mut rig = Rig::new();
    let zap = |rig: &mut Rig, i: usize| {
        rig.pb.play_live(&o.url(if i.is_multiple_of(2) { a } else { b }), crate::headers::RequestHeaders::none());
        assert!(rig.run_until(Duration::from_secs(15), |s, _| s.first_frame_ms.is_some()), "zap {i}: no first frame");
        let _ = rig.run_until(Duration::from_secs(3), |_, _| false);
    };
    zap(&mut rig, 0);
    let (before, _) = stopped_heap_kb(&mut rig);
    for i in 1..=10 {
        zap(&mut rig, i);
        eprintln!("e2e: mem zap {i} heap={} kB", heap_kb().unwrap_or(0));
    }
    let (after, _) = stopped_heap_kb(&mut rig);
    eprintln!("e2e: mem stopped after the first zap {before} kB, after ten more {after} kB ({:+} kB) pcm={pcm}", after as i64 - before as i64);
    assert!(after < before + RETAINED_BUDGET_KB, "ten zaps retained {} kB", after - before);
}

/// **Memory after a catch-up** (the C1 stayed ~80 MB higher after one): an archive is
/// read ahead to the video lane's cap (48 MiB), then the playback returns to live. Stopped, the
/// heap in use is back where a live-only session left it, so nothing holds the queue's memory.
/// What the allocator keeps mapped for it (`held`, about the queue's peak here) is printed beside
/// it: on the TV that is the RSS glibc's arenas kept, which `aq::release_freed_memory` trims at
/// the stop. The queue only fills with a
/// fixture at a TV's bitrate in `WEBOSIPTV_E2E_HLS` (a 1080p one, say); the default
/// one fills it to a few MB.
#[test]
#[ignore]
fn e2e_a_catchup_and_the_return_to_live_retain_no_memory() {
    let Some(fx) = memory_fixture() else { return };
    let _serial = tv_base::testlock::serial();
    let _sink = ClockSink::arm();
    let o = Origin::spawn();
    let live = "/ch8/mono.m3u8?token=e2e-secret";
    serve_live(&o, live, &fx, 0);
    let n = fx.segments.len().min(60);
    serve_segments(&o, &fx, 0..n);
    let window = format!("/ch8/mono-{PROGRAMME}-{}.m3u8?token=e2e-secret", n * 6);
    o.put(&window, archive(0..n));
    let mut rig = Rig::new();
    let play_live = |rig: &mut Rig| {
        rig.pb.play_live(&o.url(live), crate::headers::RequestHeaders::none());
        assert!(rig.run_until(Duration::from_secs(20), |s, _| s.first_frame_ms.is_some()), "no first frame within 20 s");
        let _ = rig.run_until(Duration::from_secs(15), |_, _| false);
    };
    play_live(&mut rig);
    let _ = stopped_heap_kb(&mut rig);
    play_live(&mut rig);
    let (before, before_held) = stopped_heap_kb(&mut rig);
    rig.pb.play_catchup(Catchup {
        start: PROGRAMME,
        end: PROGRAMME + n as i64 * 6,
        offset_secs: 0,
        window: Some(o.url(&window)),
        shift: None,
        live_url: o.url(live),
        headers: crate::headers::RequestHeaders::none(),
        at_end: AtEnd::ReturnToLive,
    });
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.state == State::Playing), "no picture within 20 s");
    let mut peak = 0;
    let _ = rig.run_until(Duration::from_secs(30), |_, _| {
        peak = peak.max(heap_kb().unwrap_or(0));
        false
    });
    let (during, during_held) = heap_stats_kb().unwrap();
    rig.pb.go_live();
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.mode == Mode::Live && s.first_frame_ms.is_some()), "live again");
    let _ = rig.run_until(Duration::from_secs(15), |_, _| false);
    let (live_again, live_held) = heap_stats_kb().unwrap();
    let (after, after_held) = stopped_heap_kb(&mut rig);
    eprintln!(
        "e2e: mem catch-up: stopped before {before} kB (held {before_held}), catch-up peak {peak} kB, at 30 s {during} kB (held {during_held}), \
         live again {live_again} kB (held {live_held}), stopped after {after} kB (held {after_held}); {:+} kB in use, {:+} kB held",
        after as i64 - before as i64,
        after_held as i64 - before_held as i64
    );
    assert!(after < before + RETAINED_BUDGET_KB, "a catch-up and the return to live retained {} kB", after - before);
}

/// **Suspend and resume** (the app in the background): the pipeline goes, the programme keeps
/// its position, and resume plays on from there.
#[test]
#[ignore]
fn e2e_a_suspended_catchup_resumes_at_its_position() {
    let Some(fx) = fixture() else { return };
    let _serial = tv_base::testlock::serial();
    let _sink = ClockSink::arm();
    let o = Origin::spawn();
    serve_segments(&o, &fx, 100..110);
    let window = format!("/ch4/mono-{PROGRAMME}-60.m3u8?token=e2e-secret");
    o.put(&window, archive(100..110));
    let mut rig = Rig::new();
    rig.pb.play_catchup(Catchup {
        start: PROGRAMME,
        end: PROGRAMME + 60,
        offset_secs: 20,
        window: Some(o.url(&window)),
        shift: None,
        live_url: String::new(),
        headers: crate::headers::RequestHeaders::none(),
        at_end: AtEnd::Continue,
    });
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.state == State::Playing));
    let _ = rig.run_until(Duration::from_secs(6), |_, _| false);
    let before = rig.pb.status().catchup.unwrap().position_ms;
    rig.pb.suspend();
    let _ = rig.run_until(Duration::from_secs(3), |_, _| false);
    let held = rig.pb.status();
    assert_eq!(held.state, State::Suspended);
    assert_eq!(held.catchup.unwrap().position_ms / 1000, before / 1000, "the position is kept");
    rig.pb.resume();
    assert!(rig.run_until(Duration::from_secs(20), |s, _| s.state == State::Playing && s.first_frame_ms.is_some()));
    let after = rig.pb.status().catchup.unwrap().position_ms;
    eprintln!("e2e: suspended at {before} ms, resumed at {after} ms");
    assert!(after + 7_000 >= before && after <= before + 2_000, "resumed at the position (by segment): {before} -> {after}");
    rig.stop();
}

/// An archive whose ad breaks make its timestamps jump (`hls::timeline`), on this file's rig.
#[path = "hls/discontinuity_e2e_tests.rs"]
mod discontinuity;
