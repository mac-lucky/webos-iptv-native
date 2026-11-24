//! Bitmap subtitles through the FFmpeg the app ships (`cargo xtask ffmpeg --host` stages it in
//! `pkg/`; `WEBOSIPTV_FFMPEG_DIR` names another directory). Each fixture's cues are written here
//! as a PGS file and, beside H.264 by the `ffmpeg` command line, converted to DVB subtitles in an
//! MPEG-TS or copied as they are into a Matroska film, so the bitmaps, their times and their
//! colours are known. The tests skip, loudly, without the staged FFmpeg, without the decoder they
//! need (`dvbsub`, `pgssub`) or without the command line tool. Run alone with
//! `cargo test -p tv_media --lib ff::subdec_tests -- --nocapture`.
//!
//! [`picture`]'s caps and what each decoder is let keep need no FFmpeg: they are driven with
//! display sets and segments built in this file.

use super::late_audio_tests::{tool_present, FileServer, Scratch};
use super::subdec::{
    self, picture, pgs_segments, span, AVSubtitle, AVSubtitleRect, Kind, ObjectBudget, RegionBudget, DISPLAY_COST,
    MAX_EPOCH_OBJECTS, REGION_BUDGET,
};
use super::test_support::{bind_host_ffmpeg, host_ffmpeg_dir, HostFfmpeg};
use super::*;
use crate::subtitles::{self, Cue, Track, MAX_CUE_PIXELS, MAX_RECTS};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

const NS: i64 = 1_000_000_000;

/// The host FFmpeg and the subtitle decoders bound; undone however the test leaves.
pub(super) struct Bound {
    _ffmpeg: HostFfmpeg,
}

impl Bound {
    /// `None`, after a SKIP line, when the staged FFmpeg, its decoder for `need` or the `ffmpeg`
    /// tool is missing. Every decoder the build has is listed, as the app would.
    pub(super) fn new(need: Kind) -> Option<Bound> {
        let dir = host_ffmpeg_dir();
        if !avcodec::CANDIDATES.iter().any(|n| dir.join(n).is_file()) {
            eprintln!("SKIP: no host FFmpeg in {} - run `cargo xtask ffmpeg --host`", dir.display());
            return None;
        }
        if !tool_present("ffmpeg") {
            eprintln!("SKIP: no ffmpeg command line tool to write the fixture");
            return None;
        }
        let ffmpeg = bind_host_ffmpeg();
        assert!(subdec::load_from(&dir), "the subtitle decoder did not bind from {}", dir.display());
        let present = subdec::decoders_present();
        if present & need.bit() == 0 {
            eprintln!("SKIP: the FFmpeg in {} has no {need:?} decoder - run `cargo xtask ffmpeg --host`", dir.display());
            return None;
        }
        subdec::PRESENT.store(present, Ordering::Release);
        Some(Bound { _ffmpeg: ffmpeg })
    }
}

impl Drop for Bound {
    fn drop(&mut self) {
        subdec::PRESENT.store(0, Ordering::Release);
    }
}

/// One cue of a fixture: shown from `from` to `to` seconds, a `w`x`h` bitmap at (`x`, `y`) of a
/// 1920x1080 canvas, its left half white (palette index 1) and its right half red (index 2).
pub(super) struct PgsCue {
    pub(super) from: f64,
    pub(super) to: f64,
    pub(super) x: u16,
    pub(super) y: u16,
    pub(super) w: u16,
    pub(super) h: u16,
}

/// A PGS (`.sup`) file of `cues`, an empty composition at 0 first so its timeline starts there.
pub(super) fn pgs(cues: &[PgsCue]) -> Vec<u8> {
    pgs_lagged(cues, 0.0)
}

/// [`pgs`] with every segment after each presentation segment stamped `lag` seconds later, so a
/// demuxer that hands over one segment per packet puts the display set's end at another time
/// than its start.
fn pgs_lagged(cues: &[PgsCue], lag: f64) -> Vec<u8> {
    fn seg(out: &mut Vec<u8>, t: f64, kind: u8, body: &[u8]) {
        let pts = (t * 90_000.0) as u32;
        out.extend_from_slice(b"PG");
        out.extend_from_slice(&pts.to_be_bytes());
        out.extend_from_slice(&pts.to_be_bytes());
        out.push(kind);
        out.extend_from_slice(&(body.len() as u16).to_be_bytes());
        out.extend_from_slice(body);
    }
    let be = |v: u16| v.to_be_bytes();
    let clear = |out: &mut Vec<u8>, t: f64, n: u16| {
        let pcs = [&be(1920)[..], &be(1080), &[0x10], &be(n), &[0x00, 0, 0, 0]].concat();
        seg(out, t, 0x16, &pcs);
        seg(out, t + lag, 0x17, &[&[1, 0][..], &be(0), &be(0), &be(1), &be(1)].concat());
        seg(out, t + lag, 0x80, &[]);
    };
    let mut out = Vec::new();
    clear(&mut out, 0.0, 0);
    for (i, c) in cues.iter().enumerate() {
        let n = 1 + 2 * i as u16;
        let pcs = [&be(1920)[..], &be(1080), &[0x10], &be(n), &[0x80, 0, 0, 1], &be(0), &[0, 0], &be(c.x), &be(c.y)].concat();
        seg(&mut out, c.from, 0x16, &pcs);
        let later = c.from + lag;
        seg(&mut out, later, 0x17, &[&[1, 0][..], &be(c.x), &be(c.y), &be(c.w), &be(c.h)].concat());
        // Y, Cr, Cb, alpha: white, then a saturated red.
        seg(&mut out, later, 0x14, &[0, 0, 1, 235, 128, 128, 255, 2, 81, 240, 90, 255]);
        let half = c.w / 2;
        let mut rle = Vec::new();
        for _ in 0..c.h {
            for (len, colour) in [(half, 1u8), (c.w - half, 2)] {
                rle.extend_from_slice(&[0, 0xC0 | (len >> 8) as u8, len as u8, colour]);
            }
            rle.extend_from_slice(&[0, 0]);
        }
        let data = [&be(c.w)[..], &be(c.h), &rle].concat();
        let len = data.len() as u32;
        let ods = [&be(0)[..], &[0, 0xC0], &len.to_be_bytes()[1..], &data].concat();
        seg(&mut out, later, 0x15, &ods);
        seg(&mut out, later, 0x80, &[]);
        clear(&mut out, c.to, n + 1);
    }
    out
}

/// A 6 s MPEG-TS: 640x360 H.264, AAC, and two DVB subtitle tracks (`pol`, then `eng`) made from
/// `pol` and `eng`.
fn generate(dir: &Path, pol: &[PgsCue], eng: &[PgsCue]) -> PathBuf {
    let (p, e, out) = (dir.join("pol.sup"), dir.join("eng.sup"), dir.join("subs.ts"));
    std::fs::write(&p, pgs(pol)).unwrap();
    std::fs::write(&e, pgs(eng)).unwrap();
    let status = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc=s=640x360:r=25:d=6"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=6"])
        .args(["-fix_sub_duration", "-i"])
        .arg(&p)
        .args(["-fix_sub_duration", "-i"])
        .arg(&e)
        .args(["-map", "0:v", "-map", "1:a", "-map", "2:s", "-map", "3:s"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "25"])
        .args(["-c:a", "aac", "-c:s", "dvbsub"])
        .args(["-metadata:s:s:0", "language=pol", "-metadata:s:s:1", "language=eng"])
        .args(["-f", "mpegts"])
        .arg(&out)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg could not write the fixture");
    out
}

fn cue(from: f64, to: f64, w: u16) -> PgsCue {
    PgsCue { from, to, x: 400, y: 900, w, h: 60 }
}

/// The fixture both tests read: Polish cues 300 px wide, English ones 500 px wide.
fn fixture(scratch: &Scratch) -> PathBuf {
    let pol = [cue(1.0, 2.0, 300), cue(3.0, 4.5, 300)];
    let eng = [cue(1.0, 2.0, 500), cue(3.0, 4.5, 500)];
    generate(&scratch.0, &pol, &eng)
}

/// One subtitle track of a Matroska fixture: its PGS file, how it is written (`copy`: PGS as it
/// is; `dvbsub`: converted) and its stream flags.
struct FilmTrack<'a> {
    sup: Vec<u8>,
    codec: &'a str,
    language: &'a str,
    forced: bool,
}

/// A 6 s Matroska film: 640x360 H.264, AAC, and `tracks` in order.
fn film(dir: &Path, tracks: &[FilmTrack]) -> PathBuf {
    container(dir, tracks, "film.mkv", &["-f", "matroska"])
}

/// [`film`] in the container `format` names, written to `name`.
fn container(dir: &Path, tracks: &[FilmTrack], name: &str, format: &[&str]) -> PathBuf {
    let out = dir.join(name);
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-v", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc=s=640x360:r=25:d=6"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=6"]);
    for (i, t) in tracks.iter().enumerate() {
        let sup = dir.join(format!("track{i}.sup"));
        std::fs::write(&sup, &t.sup).unwrap();
        if t.codec == "dvbsub" {
            cmd.arg("-fix_sub_duration");
        }
        cmd.args(["-f", "sup", "-i"]).arg(sup);
    }
    cmd.args(["-map", "0:v", "-map", "1:a"]);
    for i in 0..tracks.len() {
        cmd.args(["-map", &format!("{}:s", i + 2)]);
    }
    cmd.args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "25", "-c:a", "aac"]);
    for (i, t) in tracks.iter().enumerate() {
        cmd.args([format!("-c:s:{i}"), t.codec.to_owned()]);
        cmd.args([format!("-metadata:s:s:{i}"), format!("language={}", t.language)]);
        cmd.args([format!("-disposition:s:{i}"), (if t.forced { "forced" } else { "0" }).to_owned()]);
    }
    let status = cmd.args(format).arg(&out).status().expect("run ffmpeg");
    assert!(status.success(), "ffmpeg could not write the {name} fixture");
    out
}

/// The PGS film both Matroska tests read: a forced French track, cues 300 px wide, then the full
/// one, 500 px wide. (FFmpeg's Matroska muxer, like mkvmerge, puts each display set in one block
/// at its presentation segment's time.)
fn pgs_film(scratch: &Scratch) -> PathBuf {
    let forced = [cue(1.0, 2.0, 300), cue(3.0, 4.5, 300)];
    let full = [cue(1.0, 2.0, 500), cue(3.0, 4.5, 500)];
    let track = |sup, forced| FilmTrack { sup, codec: "copy", language: "fre", forced };
    film(&scratch.0, &[track(pgs(&forced), true), track(pgs(&full), false)])
}

/// Every cue in the store, without disturbing it.
fn stored() -> Vec<Arc<Cue>> {
    subtitles::cues_for_test()
}

/// `ff::demux` over `file`: the cues it stored, and the first video pts.
fn demux_cues(file: &Path) -> (Vec<Arc<Cue>>, i64) {
    let server = FileServer::start(file);
    SHARED.seek_to_ns.store(-1, Ordering::Relaxed);
    SHARED.desired_audio_idx.store(-1, Ordering::Relaxed);
    SHARED.demux_failed.store(false, Ordering::Relaxed);
    FEED_AUDIO.store(true, Ordering::Relaxed);
    set_soft_pcm(0);
    let (video_cap, audio_cap) = crate::player::engine::aq_caps();
    let mut video = crate::aq::aq_new(video_cap as c_long);
    let mut audio = crate::aq::aq_new(audio_cap as c_long);
    let mut hs = tv_net::stream::http_stream_boxed();
    let (vp, ap, hp) = (
        SendPtr(&mut *video as *mut AuQueue),
        SendPtr(&mut *audio as *mut AuQueue),
        SendPtr(&mut *hs as *mut HttpStream),
    );
    let name = file.file_name().unwrap().to_string_lossy();
    let url = format!("http://127.0.0.1:{}/media/{name}", server.port);
    let th = std::thread::spawn(move || {
        let off = tv_base::task::OffFrame::for_test();
        demux(&off, url, crate::headers::RequestHeaders::none(), "aac".into(), vp, ap, hp)
    });
    let pop_all = |q: &mut AuQueue, pts: &mut Vec<i64>| loop {
        let node = crate::aq::aq_pop(q, std::ptr::null_mut());
        if node.is_null() {
            break;
        }
        pts.push(unsafe { crate::aq::au_fields(node) }.2);
        unsafe { libc::free(node as *mut c_void) };
    };
    let (mut vids, mut auds) = (Vec::new(), Vec::new());
    let deadline = Instant::now() + Duration::from_secs(20);
    while !th.is_finished() {
        assert!(Instant::now() < deadline, "the demuxer did not finish a 6 s file");
        pop_all(&mut video, &mut vids);
        pop_all(&mut audio, &mut auds);
        std::thread::sleep(Duration::from_millis(5));
    }
    th.join().unwrap();
    pop_all(&mut video, &mut vids);
    assert!(!SHARED.demux_failed.load(Ordering::Acquire), "the demux must not fail");
    assert!(vids.len() >= 140, "the video plays as before ({} AUs)", vids.len());
    (stored(), vids.into_iter().min().unwrap())
}

fn rgba_at(cue: &Cue, x: usize, y: usize) -> [u8; 4] {
    let r = &cue.rects[0];
    r.palette[usize::from(r.index[y * r.w as usize + x])]
}

/// The picked track's cues come out of the demuxer on the video's clock, a second after it starts
/// and with the clears where the fixture put them, as the bitmaps that were written: its size, its
/// place on the stream's own canvas, its two colours.
#[test]
fn the_picked_dvb_track_decodes_on_the_video_clock() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = Bound::new(Kind::Dvb) else { return };
    let scratch = Scratch::new("dvbsub");
    let file = fixture(&scratch);
    subtitles::set_auto(false, "");
    subtitles::begin();
    subtitles::select(Some(1));
    let (cues, video_start) = demux_cues(&file);
    subtitles::begin();

    let at = |c: &Cue| (c.start_ns - video_start) as f64 / NS as f64;
    let english: Vec<_> = cues.iter().filter(|c| c.track == 1).collect();
    let shown: Vec<_> = english.iter().filter(|c| !c.rects.is_empty()).collect();
    let clears: Vec<_> = english.iter().filter(|c| c.rects.is_empty()).collect();
    eprintln!("English cues at {:?}", english.iter().map(|c| (at(c), c.rects.len())).collect::<Vec<_>>());
    assert_eq!(shown.len(), 2, "two pictures on the English track");
    assert_eq!(clears.len(), 2, "each cleared at its end");
    for (c, want) in shown.iter().zip([1.0, 3.0]) {
        assert!((at(c) - want).abs() < 0.05, "a picture at {want} s, got {} s", at(c));
    }
    for (c, want) in clears.iter().zip([2.0, 4.5]) {
        assert!((at(c) - want).abs() < 0.05, "a clear at {want} s, got {} s", at(c));
    }
    let first = shown[0];
    assert_eq!(first.canvas, (1920, 1080), "the stream's display definition names its canvas");
    assert!(!first.square, "a DVB display is stretched over the picture");
    assert_eq!(first.rects.len(), 1);
    let r = &first.rects[0];
    assert_eq!((r.x, r.y, r.w, r.h), (400, 900, 500, 60), "the English bitmap, where it was put");
    let (white, red) = (rgba_at(first, 10, 30), rgba_at(first, 490, 30));
    assert!(white.iter().all(|&v| v > 220), "left half white: {white:?}");
    assert!(red[0] > 180 && red[1] < 90 && red[2] < 90 && red[3] == 255, "right half red: {red:?}");
    assert_eq!(rgba_at(first, 0, 0)[3], 255, "opaque where drawn");

    let polish: Vec<_> = cues.iter().filter(|c| c.track == 0 && !c.rects.is_empty()).collect();
    assert_eq!(polish.len(), 2, "the track not picked is decoded too");
    assert!(polish.iter().all(|c| c.rects[0].w == 300));

    // The store answers by the presented position: on screen between 1 and 2 s, not after.
    let mut store = subtitles::Store::default();
    for c in &cues {
        store.push(Cue::clone(c));
    }
    assert!(store.active(video_start + NS / 2, 1).is_none());
    assert_eq!(store.active(video_start + 3 * NS / 2, 1).map(|c| c.rects[0].w), Some(500));
    assert!(store.active(video_start + 5 * NS / 2, 1).is_none(), "cleared at 2 s");
}

/// Both tracks are listed by language and decoded with subtitles off, and with the setting on a
/// new playback starts on the UI language's track.
#[test]
fn the_tracks_are_listed_and_auto_picks_the_ui_language() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = Bound::new(Kind::Dvb) else { return };
    let scratch = Scratch::new("dvbsub-auto");
    let file = fixture(&scratch);
    subtitles::set_auto(false, "");
    subtitles::begin();
    let (cues, _) = demux_cues(&file);
    let track = |lang: &str| Track { language: lang.into(), codec: "dvb_subtitle".into(), ..Track::default() };
    assert_eq!(subtitles::tracks(), vec![track("pol"), track("eng")]);
    assert_eq!(subtitles::selected(), None);
    assert_eq!(cues.iter().filter(|c| !c.rects.is_empty()).count(), 4, "off: decoded, not shown");

    subtitles::set_auto(true, "pl");
    subtitles::begin();
    demux_cues(&file);
    assert_eq!(subtitles::selected(), Some(0), "the Polish track");
    subtitles::set_auto(false, "");
    subtitles::begin();
}

/// A pick shows the other track's cue at once, without a reload: both were decoded, and what
/// [`subtitles::current`] answers at the presented position follows the pick alone.
#[test]
fn a_pick_shows_the_other_track_at_once() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = Bound::new(Kind::Dvb) else { return };
    let scratch = Scratch::new("dvbsub-pick");
    let file = fixture(&scratch);
    subtitles::set_auto(false, "");
    subtitles::begin();
    subtitles::select(Some(0));
    let (_, video_start) = demux_cues(&file);
    let (playpos, base) = (SHARED.playpos_ns.load(Ordering::Relaxed), SHARED.disp_base.load(Ordering::Relaxed));
    // The picture at 1.5 s, as the sink reports it.
    SHARED.disp_base.store(0, Ordering::Relaxed);
    SHARED.playpos_ns.store(video_start + 3 * NS / 2, Ordering::Relaxed);
    let width = || subtitles::current().map(|c| c.rects[0].w);
    assert_eq!(width(), Some(300), "the Polish picture");
    subtitles::select(Some(1));
    assert_eq!(width(), Some(500), "the English one, the next frame");
    subtitles::select(None);
    assert_eq!(width(), None, "off");
    SHARED.playpos_ns.store(playpos, Ordering::Relaxed);
    SHARED.disp_base.store(base, Ordering::Relaxed);
    subtitles::begin();
}

/// A Matroska film's PGS track decodes on the video's clock like a DVB one: each picture shown
/// until the clear that follows it (PGS gives no end time), as the bitmap that was written, on
/// the 1920x1080 canvas its presentation names, which keeps its shape.
#[test]
fn a_matroska_pgs_track_decodes_from_its_presentation_on_the_video_clock() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = Bound::new(Kind::Pgs) else { return };
    let scratch = Scratch::new("pgssub");
    let file = pgs_film(&scratch);
    subtitles::set_auto(false, "");
    subtitles::begin();
    subtitles::select(Some(1));
    let (cues, video_start) = demux_cues(&file);
    subtitles::begin();

    let at = |c: &Cue| (c.start_ns - video_start) as f64 / NS as f64;
    let full: Vec<_> = cues.iter().filter(|c| c.track == 1).collect();
    eprintln!("full French cues at {:?}", full.iter().map(|c| (at(c), c.rects.len())).collect::<Vec<_>>());
    let shown: Vec<_> = full.iter().filter(|c| !c.rects.is_empty()).collect();
    let clears: Vec<_> = full.iter().filter(|c| c.rects.is_empty() && at(c) > 0.5).collect();
    assert_eq!(shown.len(), 2, "two pictures on the full track");
    assert_eq!(clears.len(), 2, "each cleared by the next display set");
    for (c, want) in shown.iter().zip([1.0, 3.0]) {
        assert!((at(c) - want).abs() < 0.05, "a picture at {want} s, got {} s", at(c));
        assert_eq!(c.end_ns, i64::MAX, "no end of its own: the clear ends it");
    }
    for (c, want) in clears.iter().zip([2.0, 4.5]) {
        assert!((at(c) - want).abs() < 0.05, "a clear at {want} s, got {} s", at(c));
    }
    let first = shown[0];
    assert_eq!(first.canvas, (1920, 1080), "the presentation's video size");
    assert!(first.square, "a Blu-ray HD canvas keeps its shape over the picture");
    assert_eq!(first.rects.len(), 1);
    let r = &first.rects[0];
    assert_eq!((r.x, r.y, r.w, r.h), (400, 900, 500, 60), "the full track's bitmap, where it was put");
    let (white, red) = (rgba_at(first, 10, 30), rgba_at(first, 490, 30));
    assert!(white.iter().all(|&v| v > 220), "left half white: {white:?}");
    assert!(red[0] > 180 && red[1] < 90 && red[2] < 90 && red[3] == 255, "right half red: {red:?}");

    let forced: Vec<_> = cues.iter().filter(|c| c.track == 0 && !c.rects.is_empty()).collect();
    assert_eq!(forced.len(), 2, "the track not picked is decoded too");
    assert!(forced.iter().all(|c| c.rects[0].w == 300));

    let mut store = subtitles::Store::default();
    for c in &cues {
        store.push(Cue::clone(c));
    }
    assert!(store.active(video_start + NS / 2, 1).is_none());
    assert_eq!(store.active(video_start + 3 * NS / 2, 1).map(|c| c.rects[0].w), Some(500));
    assert!(store.active(video_start + 5 * NS / 2, 1).is_none(), "cleared at 2 s");
    assert_eq!(store.active(video_start + 7 * NS / 2, 1).map(|c| c.rects[0].w), Some(500));
    assert!(store.active(video_start + 5 * NS, 1).is_none(), "cleared at 4.5 s");
}

/// A Blu-ray transport stream (M2TS) carries each PGS segment in a PES of its own, stamped with
/// its own time: a display set shows from its presentation segment, not from its end, here a
/// quarter second later.
#[test]
fn an_m2ts_pgs_display_set_shows_from_its_presentation_segment() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = Bound::new(Kind::Pgs) else { return };
    let scratch = Scratch::new("pgssub-m2ts");
    let track = FilmTrack { sup: pgs_lagged(&[cue(1.0, 2.0, 500), cue(3.0, 4.5, 500)], 0.25), codec: "copy", language: "eng", forced: false };
    let file = container(&scratch.0, &[track], "film.m2ts", &["-f", "mpegts", "-mpegts_m2ts_mode", "1"]);
    subtitles::set_auto(false, "");
    subtitles::begin();
    let (cues, video_start) = demux_cues(&file);
    assert_eq!(subtitles::tracks().len(), 1, "the PGS track of the M2TS is listed");
    subtitles::begin();
    let at = |c: &Cue| (c.start_ns - video_start) as f64 / NS as f64;
    let times: Vec<_> = cues.iter().map(|c| (at(c), c.rects.len())).collect();
    eprintln!("M2TS cues at {times:?}");
    let shown: Vec<_> = cues.iter().filter(|c| !c.rects.is_empty()).map(|c| at(c)).collect();
    assert_eq!(shown.len(), 2, "two pictures: {times:?}");
    for (t, want) in shown.iter().zip([1.0, 3.0]) {
        assert!((t - want).abs() < 0.05, "a picture at {want} s, got {t} s");
    }
}

/// The video size `avformat_find_stream_info` left on the first PGS stream of `file`, opened by
/// path with the stream-info guard or without it: 0 when it never decoded the stream.
fn stream_info_pgs_width(file: &Path, guarded: bool) -> i32 {
    let path = std::ffi::CString::new(file.to_str().unwrap()).unwrap();
    unsafe {
        let mut fmt = avformat_alloc_context();
        assert!(!fmt.is_null());
        if guarded {
            subdec::guard_stream_info(fmt);
        }
        assert!(avformat_open_input(&mut fmt, path.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut()) >= 0);
        assert!(avformat_find_stream_info(fmt, std::ptr::null_mut()) >= 0);
        let width = (0..(*fmt).nb_streams as usize)
            .map(|i| stream_codecpar(*(*fmt).streams.add(i)))
            .find(|&cp| (*cp).codec_type == 3)
            .map(|cp| (*cp).width)
            .expect("a PGS stream");
        avformat_close_input(&mut fmt);
        width
    }
}

/// `avformat_find_stream_info` decodes a PGS stream with a decoder of its own, out of the
/// budget's sight, until a display set names its size; guarded, it never opens one, and the
/// app's own decoder still reads every picture.
#[test]
fn the_stream_info_search_never_decodes_pgs() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = Bound::new(Kind::Pgs) else { return };
    let scratch = Scratch::new("pgssub-stream-info");
    let file = pgs_film(&scratch);
    assert_eq!(stream_info_pgs_width(&file, false), 1920, "unguarded, the search decodes the stream");
    assert_eq!(stream_info_pgs_width(&file, true), 0, "guarded, it does not");
    subtitles::set_auto(false, "");
    subtitles::begin();
    let (cues, _) = demux_cues(&file);
    assert_eq!(cues.iter().filter(|c| !c.rects.is_empty()).count(), 4, "the demuxer's own decoders still do");
    subtitles::begin();
}

/// A first display set of 64 objects, each declaring a 16 MiB RLE for a 16x16 bitmap: what would
/// have the decoder zero 1.1 GB.
fn pgs_bomb() -> Vec<u8> {
    let mut out = Vec::new();
    let mut seg = |kind: u8, body: &[u8]| {
        out.extend_from_slice(b"PG");
        out.extend_from_slice(&45_000u32.to_be_bytes());
        out.extend_from_slice(&45_000u32.to_be_bytes());
        out.push(kind);
        out.extend_from_slice(&(body.len() as u16).to_be_bytes());
        out.extend_from_slice(body);
    };
    seg(0x16, &[0x07, 0x80, 0x04, 0x38, 0x10, 0, 0, 0x80, 0, 0, 0]);
    for id in 0..64u16 {
        seg(0x15, &[&id.to_be_bytes()[..], &[0, 0xC0, 0xFF, 0xFF, 0xFF, 0, 16, 0, 16], &[0; 8]].concat());
    }
    seg(0x80, &[]);
    out
}

/// That display set through the demuxer: the search does not decode it, and the app's decoder is
/// never given it (the budget refuses it, a fresh decoder too), so nothing shows and the film
/// plays on.
#[test]
fn a_pgs_display_set_past_the_budget_is_never_decoded() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = Bound::new(Kind::Pgs) else { return };
    let scratch = Scratch::new("pgssub-bomb");
    let film = film(&scratch.0, &[FilmTrack { sup: pgs_bomb(), codec: "copy", language: "eng", forced: false }]);
    assert_eq!(stream_info_pgs_width(&film, true), 0);
    let mut budget = ObjectBudget::default();
    let sup = pgs_bomb();
    let mut set = Vec::new();
    let mut rest = &sup[..];
    while rest.len() >= 13 {
        let len = usize::from(u16::from_be_bytes([rest[11], rest[12]]));
        set.extend_from_slice(&rest[10..13 + len]);
        rest = &rest[13 + len..];
    }
    assert!(!budget.admit(&set), "the display set, as the Matroska block carries it, is refused");
    subtitles::set_auto(false, "");
    subtitles::begin();
    let (cues, _) = demux_cues(&film);
    assert_eq!(subtitles::tracks().len(), 1);
    assert!(cues.iter().all(|c| c.rects.is_empty()), "nothing decoded from it");
    subtitles::begin();
}

/// The menu lists both French tracks by language, the forced one marked, and with subtitles on a
/// new playback starts on the full one, whatever their order.
#[test]
fn matroska_pgs_tracks_are_listed_and_auto_skips_the_forced_one() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = Bound::new(Kind::Pgs) else { return };
    let scratch = Scratch::new("pgssub-auto");
    let file = pgs_film(&scratch);
    subtitles::set_auto(true, "fr");
    subtitles::begin();
    demux_cues(&file);
    let track = |forced| Track { language: "fre".into(), codec: "hdmv_pgs_subtitle".into(), forced, ..Track::default() };
    assert_eq!(subtitles::tracks(), vec![track(true), track(false)]);
    assert_eq!(subtitles::selected(), Some(1), "the full French track");
    subtitles::set_auto(false, "");
    subtitles::begin();
}

/// A bundled FFmpeg without `pgssub` (any release before it was added) still lists and decodes
/// a film's DVB track, and lists none of its PGS ones: each codec stands on its own decoder.
#[test]
fn a_build_without_one_decoder_still_lists_the_other_codecs_tracks() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = Bound::new(Kind::Dvb) else { return };
    let present = subdec::PRESENT.load(Ordering::Acquire);
    let scratch = Scratch::new("subdec-mixed");
    let cues = [cue(1.0, 2.0, 500), cue(3.0, 4.5, 500)];
    let dvb = FilmTrack { sup: pgs(&cues), codec: "dvbsub", language: "pol", forced: false };
    let pgs = FilmTrack { sup: pgs(&cues), codec: "copy", language: "fre", forced: false };
    let file = film(&scratch.0, &[dvb, pgs]);
    let track = |language: &str, codec: &str| Track { language: language.into(), codec: codec.into(), ..Track::default() };
    subtitles::set_auto(false, "");

    subdec::PRESENT.store(Kind::Dvb.bit(), Ordering::Release);
    subtitles::begin();
    let (stored, _) = demux_cues(&file);
    assert_eq!(subtitles::tracks(), vec![track("pol", "dvb_subtitle")], "the DVB track alone");
    assert!(stored.iter().all(|c| c.track == 0));

    if present & Kind::Pgs.bit() != 0 {
        subdec::PRESENT.store(present, Ordering::Release);
        subtitles::begin();
        demux_cues(&file);
        assert_eq!(subtitles::tracks(), vec![track("pol", "dvb_subtitle"), track("fre", "hdmv_pgs_subtitle")]);
    } else {
        eprintln!("SKIP (second half): the FFmpeg in {} has no pgssub decoder", host_ffmpeg_dir().display());
    }
    subtitles::begin();
}

/// A display set the decoder filled, from rects owned by the test.
fn display_set(rects: &mut [AVSubtitleRect], ptrs: &mut Vec<*mut AVSubtitleRect>) -> AVSubtitle {
    *ptrs = rects.iter_mut().map(|r| r as *mut AVSubtitleRect).collect();
    AVSubtitle { format: 0, start_display_time: 0, end_display_time: 0, num_rects: ptrs.len() as c_uint, rects: ptrs.as_mut_ptr(), pts: 0 }
}

fn bitmap_rect(w: c_int, h: c_int, stride: c_int, pixels: &mut [u8], palette: &mut [u32; 256], colors: c_int) -> AVSubtitleRect {
    AVSubtitleRect {
        x: 10,
        y: 20,
        w,
        h,
        nb_colors: colors,
        data: [pixels.as_mut_ptr(), palette.as_mut_ptr() as *mut u8, std::ptr::null_mut(), std::ptr::null_mut()],
        linesize: [stride, 0, 0, 0],
        flags: 0,
        type_: 1,
        text: std::ptr::null_mut(),
        ass: std::ptr::null_mut(),
    }
}

/// The rect fields are network data: a picture is measured before anything is copied, and every
/// shape the caps refuse comes back `None` rather than a huge or out-of-bounds copy.
#[test]
fn a_picture_past_the_caps_is_refused_before_it_is_copied() {
    let mut pixels = vec![1u8, 2, 9, 3, 1, 2, 9, 3];
    let mut palette = [0u32; 256];
    palette[1] = 0xFF10_2030;
    palette[2] = 0x80FF_0000;
    palette[3] = 0xFFFF_FFFF; // past nb_colors: must read transparent
    let mut ptrs = Vec::new();

    // A 3x2 bitmap in rows of 4 (the stride's padding dropped).
    let mut ok = [bitmap_rect(3, 2, 4, &mut pixels, &mut palette, 3)];
    let sub = display_set(&mut ok, &mut ptrs);
    let rects = unsafe { picture(&sub) }.expect("a sane picture");
    assert_eq!((rects[0].x, rects[0].y, rects[0].w, rects[0].h), (10, 20, 3, 2));
    assert_eq!(rects[0].index, [1, 2, 9, 1, 2, 9]);
    assert_eq!(rects[0].palette[1], [0x10, 0x20, 0x30, 0xFF]);
    assert_eq!(rects[0].palette[2], [0xFF, 0, 0, 0x80]);
    assert_eq!(rects[0].palette[3], [0, 0, 0, 0], "past the decoder's colours");

    let refused = |rect: AVSubtitleRect, why: &str| {
        let mut one = [rect];
        let mut ptrs = Vec::new();
        let sub = display_set(&mut one, &mut ptrs);
        assert!(unsafe { picture(&sub) }.is_none(), "{why}");
    };
    refused(bitmap_rect(-3, 2, 4, &mut pixels, &mut palette, 3), "a negative width");
    refused(bitmap_rect(5000, 2, 5000, &mut pixels, &mut palette, 3), "a side past MAX_SIDE");
    refused(bitmap_rect(3, 2, 2, &mut pixels, &mut palette, 3), "rows shorter than the width");
    let mut no_data = bitmap_rect(3, 2, 4, &mut pixels, &mut palette, 3);
    no_data.data[0] = std::ptr::null_mut();
    refused(no_data, "no pixels");
    let mut text = bitmap_rect(3, 2, 4, &mut pixels, &mut palette, 3);
    text.type_ = 2;
    refused(text, "not a bitmap");
    // 4096 x 4096 is a legal side each way but sixteen million pixels: refused on the product,
    // and the pointers are never read.
    refused(bitmap_rect(4096, 4096, 4096, &mut pixels, &mut palette, 3), "past MAX_CUE_PIXELS");
    const { assert!(4096 * 4096 > MAX_CUE_PIXELS) };

    // More rects than a picture may carry.
    let mut many: Vec<AVSubtitleRect> = (0..=MAX_RECTS).map(|_| bitmap_rect(3, 2, 4, &mut pixels, &mut palette, 3)).collect();
    let sub = display_set(&mut many, &mut ptrs);
    assert!(unsafe { picture(&sub) }.is_none(), "{} rects", MAX_RECTS + 1);

    // An empty display set is a clear, and an empty rect is skipped.
    let empty = AVSubtitle { format: 0, start_display_time: 0, end_display_time: 0, num_rects: 0, rects: std::ptr::null_mut(), pts: 0 };
    assert_eq!(unsafe { picture(&empty) }.map(|r| r.len()), Some(0));
    let mut zero = [bitmap_rect(0, 0, 0, &mut pixels, &mut palette, 3)];
    let sub = display_set(&mut zero, &mut ptrs);
    assert_eq!(unsafe { picture(&sub) }.map(|r| r.len()), Some(0));
}

#[test]
fn a_cue_spans_its_display_times_after_the_packet() {
    assert_eq!(span(10 * NS, 0, 5_000), (10 * NS, 15 * NS));
    assert_eq!(span(10 * NS, 200, 1_200), (10 * NS + 200_000_000, 11 * NS + 200_000_000), "both after the packet");
    assert_eq!(span(10 * NS, 0, 0), (10 * NS, i64::MAX), "no end: until the next cue");
    assert_eq!(span(i64::MAX - 1, 0, 5_000).1, i64::MAX, "no overflow");
}

/// A DVB segment: sync byte, `kind`, page 1, the payload's length, the payload.
fn segment(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![0x0f, kind, 0, 1];
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn region(id: u8, w: u16, h: u16) -> Vec<u8> {
    segment(0x11, &[&[id, 0][..], &w.to_be_bytes(), &h.to_be_bytes(), &[0, 0, 0, 0]].concat())
}

fn page(state: u8) -> Vec<u8> {
    segment(0x10, &[10, state << 2])
}

/// The regions a decoder could hold are counted before it is given the packet: across packets,
/// by id (a region sent again is resized, not added), and a packet past the budget is refused
/// without being counted.
#[test]
fn the_region_budget_counts_what_the_decoder_would_keep() {
    let mut b = RegionBudget::default();
    let hd = (1920u16, 1080u16);
    assert!(b.admit(&[page(2), region(1, hd.0, hd.1)].concat()), "one full HD region");
    assert!(b.admit(&region(1, hd.0, hd.1)), "the same id again: resized, not added");
    assert!(b.admit(&[page(0), region(2, hd.0, hd.1)].concat()), "a second one fits the budget");
    assert!(!b.admit(&region(3, 16, 16)), "a third region of any size does not");
    assert!(!b.admit(&[region(3, 16, 16), page(2)].concat()), "a reset after it comes too late");
    // The decoder skips a page that repeats its version or names another page id, and stops at
    // a segment it refuses: a page that starts over never empties the count.
    assert!(!b.admit(&[page(1), region(3, 16, 16)].concat()), "an acquisition point does not");
    assert!(!b.admit(&[page(2), region(3, 16, 16)].concat()), "nor a mode change");

    // A region sent again smaller, or refused (0 x 0), may leave the old buffer: the largest size
    // stays counted. Repeated pages between such packets change nothing.
    let mut b = RegionBudget::default();
    let big = (1280u16, 1024u16);
    for id in 0..3 {
        assert!(b.admit(&[region(id, big.0, big.1), page(1)].concat()), "region {id}");
    }
    assert!(b.admit(&[region(0, 0, 0), region(1, 1, 1), page(2)].concat()));
    assert!(!b.admit(&region(3, big.0, big.1)), "the shrunk ones still count");

    // Each object placement in a region composition is a list entry the decoder keeps.
    let mut b = RegionBudget::default();
    let placements = |id: u8, n: usize| {
        let body = [&[id, 0][..], &1u16.to_be_bytes(), &1u16.to_be_bytes(), &[0, 0, 0, 0]].concat();
        segment(0x11, &[body, vec![0; 6 * n]].concat())
    };
    let per_region = 10_000;
    let fits = REGION_BUDGET / (1 + per_region * DISPLAY_COST);
    for id in 0..fits {
        assert!(b.admit(&placements(id as u8, per_region)), "region {id}");
    }
    assert!(!b.admit(&placements(fits as u8, per_region)), "the placements add up");

    // 256 small regions over many packets add up, whatever each packet holds alone.
    let mut b = RegionBudget::default();
    let side = 400u16;
    let fits = REGION_BUDGET / (usize::from(side) * usize::from(side));
    for id in 0..fits {
        assert!(b.admit(&region(id as u8, side, side)), "region {id}");
    }
    assert!(!b.admit(&region(fits as u8, side, side)), "the next one is past the budget");

    // A truncated or foreign packet counts what it holds and no more.
    let mut b = RegionBudget::default();
    let mut cut = region(1, hd.0, hd.1);
    cut.truncate(9);
    assert!(b.admit(&cut));
    assert!(b.admit(&[0x20, 0x00, 0xff]));
    assert!(b.admit(&[]));
    // The largest region a segment can declare is refused, and the sum does not wrap.
    assert!(!b.admit(&region(2, u16::MAX, u16::MAX)));
    assert!(b.admit(&region(2, 16, 16)), "and nothing of it was counted");
}

#[test]
fn a_pgs_display_set_has_no_end_of_its_own() {
    assert_eq!(span(10 * NS, 0, u32::MAX), (10 * NS, i64::MAX), "pgssubdec's UINT32_MAX: until the next cue");
    assert_eq!(span(10 * NS, 0, u32::MAX - 1).1, 10 * NS + i64::from(u32::MAX - 1) * 1_000_000);
}

#[test]
fn each_rendered_codec_has_its_kind_bit_and_canvas_rule() {
    assert_eq!(Kind::of("dvb_subtitle"), Some(Kind::Dvb));
    assert_eq!(Kind::of("hdmv_pgs_subtitle"), Some(Kind::Pgs));
    assert_eq!(Kind::of("subrip"), None, "text subtitles are not drawn");
    assert_eq!(Kind::of("dvd_subtitle"), None);
    assert_ne!(Kind::Dvb.bit(), Kind::Pgs.bit());
    assert!(Kind::Pgs.square((1920, 1080)) && Kind::Pgs.square((1280, 720)));
    assert!(!Kind::Pgs.square((720, 480)), "SD graphics are anamorphic");
    assert!(!Kind::Dvb.square((1920, 1080)), "a DVB display covers the picture");
}

/// A PGS segment: type, length, payload.
fn pgs_seg(kind: u8, body: &[u8]) -> Vec<u8> {
    [&[kind][..], &(body.len() as u16).to_be_bytes(), body].concat()
}

/// A presentation segment of a `w`x`h` video in composition `state` (0: normal, 0x40: an
/// acquisition point, 0x80: an epoch start), naming no object.
fn pcs(w: u16, h: u16, state: u8) -> Vec<u8> {
    pgs_seg(0x16, &[&w.to_be_bytes()[..], &h.to_be_bytes(), &[0x10, 0, 1, state, 0, 0, 0]].concat())
}

/// An object segment for object `id`: the first of its RLE (declaring `declared` bytes, width and
/// height included) or one that appends to it.
fn ods(id: u16, declared: u32, first: bool) -> Vec<u8> {
    let head = [&id.to_be_bytes()[..], &[0, if first { 0xC0 } else { 0x40 }]].concat();
    if first {
        pgs_seg(0x15, &[&head[..], &declared.to_be_bytes()[1..], &[0, 16, 0, 16], &[0; 8]].concat())
    } else {
        pgs_seg(0x15, &[&head[..], &[0; 8]].concat())
    }
}

/// The segments are read the way the decoder steps through them: a segment running past the
/// packet ends the walk unless it ends the display set, which the decoder still reads.
#[test]
fn pgs_segments_are_walked_as_the_decoder_does() {
    let kinds = |p: &[u8]| pgs_segments(p).map(|(k, b)| (k, b.len())).collect::<Vec<_>>();
    let set = [pcs(1920, 1080, 0x80), pgs_seg(0x17, &[0; 10]), pgs_seg(0x80, &[])].concat();
    assert_eq!(kinds(&set), [(0x16, 11), (0x17, 10), (0x80, 0)]);
    let mut cut = pcs(1920, 1080, 0x80);
    cut.truncate(8);
    assert_eq!(kinds(&[&pgs_seg(0x14, &[1, 2])[..], &cut].concat()), [(0x14, 2)], "a cut segment ends the walk");
    assert_eq!(kinds(&[0x80, 0, 9, 1, 2]), [(0x80, 2)], "an end segment is read even cut short");
    assert_eq!(kinds(&[0x80]), [(0x80, 0)], "its header read from the padding");
    assert_eq!(kinds(&[0x16, 0]), [], "any other is not");
    assert_eq!(kinds(&[]), []);
}

/// What the PGS decoder would keep is counted before it is given a packet: each object's largest
/// declared RLE length, by id, across packets, until a packet opening with an epoch start or an
/// acquisition point of a size the decoder takes, which frees them all.
#[test]
fn the_object_budget_counts_what_the_pgs_decoder_would_keep() {
    // Counted with the allocator's slack, two of these fit the budget and three do not.
    let big = 1_500_000;
    let mut b = ObjectBudget::default();
    assert!(b.admit(&[pcs(1920, 1080, 0x80), ods(0, big, true)].concat()), "one large object");
    assert!(b.admit(&ods(0, big, true)), "the same id again: its buffer is reused");
    assert!(b.admit(&ods(0, 64, true)), "and kept at its largest when declared smaller");
    assert!(b.admit(&ods(1, big, true)), "a second one fits");
    assert!(b.admit(&ods(2, 64, false)), "appended RLE allocates nothing");
    assert!(!b.admit(&ods(2, big, true)), "a third does not");
    assert!(!b.admit(&[pcs(1920, 1080, 0), ods(2, big, true)].concat()), "a normal composition frees nothing");
    assert!(!b.admit(&[ods(3, 16, true), pcs(1920, 1080, 0x80), ods(2, big, true)].concat()), "nor one not opening its packet");
    assert!(!b.admit(&[pcs(3840, 2160, 0x80), ods(2, big, true)].concat()), "nor one of a size past max_pixels");
    assert!(!b.admit(&[pcs(0, 1080, 0x80), ods(2, big, true)].concat()), "nor one of no size");
    assert!(b.admit(&[pcs(1920, 1080, 0x80), ods(2, big, true)].concat()), "an epoch start does");
    assert!(b.admit(&[pcs(1920, 1080, 0x40), ods(3, big, true)].concat()), "and so does an acquisition point");
    assert!(b.admit(&ods(0, big, true)), "after which two objects fit again");
    assert!(!b.admit(&ods(1, big, true)));

    // The decoder's own allocation for a length it reads past 16 MiB, or that wraps below the 4
    // bytes of width and height, never fits.
    let mut b = ObjectBudget::default();
    assert!(!b.admit(&ods(0, 0xFF_FFFF, true)));
    assert!(!b.admit(&ods(0, 3, true)), "a length that wraps");
    assert!(b.admit(&ods(0, 4, true)), "an empty RLE costs only the padding");

    // Many ids of small objects add up whatever each packet holds alone.
    let mut b = ObjectBudget::default();
    let each = 100_000u32;
    let rle = each as usize - 4;
    let fits = REGION_BUDGET / (rle + rle / 16 + 100);
    for id in 0..fits as u16 {
        assert!(b.admit(&ods(id, each, true)), "object {id}");
    }
    assert!(!b.admit(&ods(fits as u16, each, true)), "the next one is past the budget");

    // The fixtures' display sets, segment by segment, never run into it.
    let mut b = ObjectBudget::default();
    let sup = pgs(&[cue(1.0, 2.0, 1800), cue(3.0, 4.5, 1800)]);
    let mut rest = &sup[..];
    let mut n = 0;
    while rest.len() >= 13 {
        let len = usize::from(u16::from_be_bytes([rest[11], rest[12]]));
        assert!(b.admit(&rest[10..13 + len]));
        rest = &rest[13 + len..];
        n += 1;
    }
    assert_eq!(n, 3 + 2 * (5 + 3), "every segment of the file was read");
}

/// An epoch holds at most as many objects as the decoder keeps: a packet past that is refused,
/// one of thousands of tiny object segments too, and counting a packet costs one lookup a
/// segment whatever it holds.
#[test]
fn the_object_budget_refuses_an_epoch_of_more_ids_than_the_decoder_keeps() {
    let max = MAX_EPOCH_OBJECTS as u16;
    let many: Vec<u8> = (0..10_000u16).flat_map(|id| ods(id, 4, true)).collect();
    let repeated: Vec<u8> = (0..10_000u16).flat_map(|i| ods(i % max, 4, true)).collect();
    assert!(many.len() <= subdec::MAX_PACKET && repeated.len() <= subdec::MAX_PACKET);
    let started = Instant::now();
    assert!(!ObjectBudget::default().admit(&many), "10000 object ids in one packet");
    assert!(ObjectBudget::default().admit(&repeated), "10000 segments over the most ids an epoch holds");
    let took = started.elapsed();
    assert!(took < Duration::from_millis(200), "{took:?}");

    // A multi-object epoch, across packets, up to the limit and no further.
    let mut b = ObjectBudget::default();
    assert!(b.admit(&[pcs(1920, 1080, 0x80), ods(0, 1000, true), ods(1, 1000, true)].concat()), "two objects");
    for id in 2..max {
        assert!(b.admit(&ods(id, 1000, true)), "object {id}");
    }
    assert!(b.admit(&ods(5, 2000, true)), "an id the epoch has");
    assert!(!b.admit(&ods(max, 16, true)), "one more id");
    assert!(!b.admit(&[ods(0, 16, true), ods(max, 16, true)].concat()), "behind a known one too");
    assert!(b.admit(&ods(max - 1, 1000, true)), "nothing of the refused packets was counted");
    assert!(b.admit(&[pcs(1920, 1080, 0x80), ods(max, 16, true)].concat()), "a new epoch starts the ids over");
}

/// The stream-info search may open every decoder the app bundles but `pgssub`: the list follows
/// xtask's `--enable-decoder`.
#[test]
fn the_stream_info_decoders_are_the_bundled_ones_but_pgs() {
    let xtask = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../xtask/src/ffmpeg.rs")).unwrap();
    let line = xtask.lines().find_map(|l| l.trim().strip_prefix("\"--enable-decoder=")).expect("xtask's decoder list");
    let line = line.trim_end_matches("\",");
    let mut bundled: Vec<&str> = line.split(',').filter(|d| *d != "pgssub").collect();
    let mut allowed: Vec<&str> = subdec::STREAM_INFO_DECODERS.to_str().unwrap().split(',').collect();
    bundled.sort_unstable();
    allowed.sort_unstable();
    assert_eq!(allowed, bundled);
    assert!(line.split(',').any(|d| d == "pgssub"), "and the build has pgssub: {line}");
}
