//! DVB subtitles through the FFmpeg the app ships (`cargo xtask ffmpeg --host` stages it in
//! `pkg/`; `WEBOSIPTV_FFMPEG_DIR` names another directory). Each fixture's cues are written here
//! as a PGS file and converted to DVB subtitles beside H.264 by the `ffmpeg` command line, so the
//! bitmaps, their times and their colours are known. The tests skip, loudly, without the staged
//! FFmpeg, without its `dvbsub` decoder or without the command line tool. Run alone with
//! `cargo test -p tv_media --lib ff::subdec_tests -- --nocapture`.
//!
//! [`picture`]'s caps need no FFmpeg: they are driven with display sets built in this file.

use super::late_audio_tests::{tool_present, FileServer, Scratch};
use super::subdec::{self, picture, span, AVSubtitle, AVSubtitleRect, RegionBudget, DISPLAY_COST, REGION_BUDGET};
use super::test_support::{bind_host_ffmpeg, host_ffmpeg_dir, HostFfmpeg};
use super::*;
use crate::subtitles::{self, Cue, Track, MAX_CUE_PIXELS, MAX_RECTS};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

const NS: i64 = 1_000_000_000;

/// The host FFmpeg and the subtitle decoder bound; undone however the test leaves.
struct Bound {
    _ffmpeg: HostFfmpeg,
}

impl Bound {
    /// `None`, after a SKIP line, when the staged FFmpeg, its `dvbsub` decoder or the `ffmpeg`
    /// tool is missing.
    fn new() -> Option<Bound> {
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
        if !subdec::decoders_present() {
            eprintln!("SKIP: the FFmpeg in {} has no dvbsub decoder - run `cargo xtask ffmpeg --host`", dir.display());
            return None;
        }
        subdec::AVAILABLE.store(true, Ordering::Release);
        Some(Bound { _ffmpeg: ffmpeg })
    }
}

impl Drop for Bound {
    fn drop(&mut self) {
        subdec::AVAILABLE.store(false, Ordering::Release);
    }
}

/// One cue of a fixture: shown from `from` to `to` seconds, a `w`x`h` bitmap at (`x`, `y`) of a
/// 1920x1080 canvas, its left half white (palette index 1) and its right half red (index 2).
struct PgsCue {
    from: f64,
    to: f64,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
}

/// A PGS (`.sup`) file of `cues`, an empty composition at 0 first so its timeline starts there.
fn pgs(cues: &[PgsCue]) -> Vec<u8> {
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
        seg(out, t, 0x17, &[&[1, 0][..], &be(0), &be(0), &be(1), &be(1)].concat());
        seg(out, t, 0x80, &[]);
    };
    let mut out = Vec::new();
    clear(&mut out, 0.0, 0);
    for (i, c) in cues.iter().enumerate() {
        let n = 1 + 2 * i as u16;
        let pcs = [&be(1920)[..], &be(1080), &[0x10], &be(n), &[0x80, 0, 0, 1], &be(0), &[0, 0], &be(c.x), &be(c.y)].concat();
        seg(&mut out, c.from, 0x16, &pcs);
        seg(&mut out, c.from, 0x17, &[&[1, 0][..], &be(c.x), &be(c.y), &be(c.w), &be(c.h)].concat());
        // Y, Cr, Cb, alpha: white, then a saturated red.
        seg(&mut out, c.from, 0x14, &[0, 0, 1, 235, 128, 128, 255, 2, 81, 240, 90, 255]);
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
        seg(&mut out, c.from, 0x15, &ods);
        seg(&mut out, c.from, 0x80, &[]);
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
    let url = format!("http://127.0.0.1:{}/media/subs.ts", server.port);
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
    let Some(_b) = Bound::new() else { return };
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
    let Some(_b) = Bound::new() else { return };
    let scratch = Scratch::new("dvbsub-auto");
    let file = fixture(&scratch);
    subtitles::set_auto(false, "");
    subtitles::begin();
    let (cues, _) = demux_cues(&file);
    let track = |lang: &str| Track { language: lang.into(), codec: "dvb_subtitle".into(), hearing_impaired: false };
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
    let Some(_b) = Bound::new() else { return };
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
