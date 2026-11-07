//! **A programme map that changes at an HLS discontinuity** (a break spliced in from another
//! encoder): the video and the audio move to other PIDs, which libavformat's mpegts adds as new
//! streams under new indices, and the audio track set changes (one track, then three, then one).
//! Through the real demuxer over the FFmpeg the app ships (`cargo xtask ffmpeg --host`), the curl
//! transport and a loopback origin; skipped without that FFmpeg, libcurl or the `ffmpeg` command
//! line tool that writes the fixture.

use super::adec_tests::Bound;
use super::late_audio_tests::{tool_present, Scratch};
use super::*;
use crate::hls::loopback_tests::{curl_gate, Answer, Origin};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// One part of the fixture: six 6 s segments.
pub(crate) const PART_S: i64 = 36;

/// The fixture's three parts, each its six segments.
pub(crate) type Parts = [Vec<Vec<u8>>; 3];

/// One audio track: language, encoder, bit rate, channels, tone (Hz).
type Track = (&'static str, &'static str, &'static str, u32, u32);

/// The played track before and after the break: MP2 at 128 kbit/s, 384-byte frames at 48 kHz.
const POL: Track = ("pol", "mp2", "128k", 2, 440);
const MP2_POL_FRAME: usize = 384;

/// **The fixture**, written by the `ffmpeg` on PATH into `dir`: three parts of six 6 s MPEG-TS
/// segments of 320x180 H.264, their timestamps going on from part to part.
/// - `a`: the video on PID 0x100, one MP2 track (`pol`) on 0x101;
/// - `c`, the break: the video on 0x200, MP2 `eng` at 192 kbit/s, MP2 `pol` and AC-3 5.1 `eng`
///   on 0x201..=0x203, so the first track is not the one in the played language;
/// - `b`: the first map again, its PIDs and its one track.
pub(crate) fn programme_change(dir: &Path) -> Result<Parts, String> {
    let parts: [(&str, i64, &[Track], bool); 3] = [
        ("a", 0, &[POL], false),
        ("c", PART_S, &[("eng", "mp2", "192k", 2, 660), POL, ("eng", "ac3", "192k", 6, 880)], true),
        ("b", 2 * PART_S, &[POL], false),
    ];
    let mut out: Parts = Default::default();
    for (n, (name, offset, tracks, new_pids)) in parts.into_iter().enumerate() {
        let mut args: Vec<String> =
            ["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=25"].map(String::from).to_vec();
        for t in tracks {
            args.extend(["-f".into(), "lavfi".into(), "-i".into(), format!("sine=frequency={}:sample_rate=48000", t.4)]);
        }
        args.extend(["-t", "36", "-map", "0:v"].map(String::from));
        for k in 1..=tracks.len() {
            args.extend(["-map".into(), format!("{k}:a")]);
        }
        args.extend(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-b:v", "120k"].map(String::from));
        args.extend(["-g", "50", "-keyint_min", "50", "-sc_threshold", "0"].map(String::from));
        for (k, (lang, codec, rate, ch, _)) in tracks.iter().enumerate() {
            args.extend([
                format!("-metadata:s:a:{k}"),
                format!("language={lang}"),
                format!("-c:a:{k}"),
                (*codec).into(),
                format!("-b:a:{k}"),
                (*rate).into(),
                format!("-ac:a:{k}"),
                ch.to_string(),
            ]);
            if new_pids {
                args.extend(["-streamid".into(), format!("{}:{:#x}", k + 1, 0x201 + k)]);
            }
        }
        if new_pids {
            args.extend(["-streamid", "0:0x200"].map(String::from));
        }
        args.extend(["-output_ts_offset".into(), offset.to_string()]);
        args.extend(["-f", "hls", "-hls_time", "6", "-hls_list_size", "0", "-hls_segment_filename"].map(String::from));
        let path = |p: String| dir.join(p).to_str().map(str::to_owned).ok_or_else(|| "a path that is not UTF-8".to_owned());
        args.push(path(format!("{name}%02d.ts"))?);
        args.push(path(format!("{name}.m3u8"))?);
        let ok = Command::new("ffmpeg").args(&args).status().map_err(|e| format!("ffmpeg: {e}"))?.success();
        if !ok {
            return Err(format!("ffmpeg could not write part {name}"));
        }
        out[n] = (0..6)
            .map(|i| std::fs::read(dir.join(format!("{name}{i:02}.ts"))).map_err(|e| format!("segment {name}{i:02}: {e}")))
            .collect::<Result<_, _>>()?;
    }
    Ok(out)
}

/// The fixture, written once per test binary; `None` (after a SKIP line) without the `ffmpeg`
/// command line tool.
fn fixture() -> Option<&'static Parts> {
    static FIXTURE: std::sync::OnceLock<Option<Parts>> = std::sync::OnceLock::new();
    FIXTURE
        .get_or_init(|| {
            if !tool_present("ffmpeg") {
                eprintln!("SKIP: no ffmpeg command line tool to write the fixture");
                return None;
            }
            let dir = Scratch::new("programme-change");
            Some(programme_change(&dir.0).expect("the fixture"))
        })
        .as_ref()
}

/// An archive playlist (`EVENT` + `ENDLIST`) of `parts` on `o`, an `EXT-X-DISCONTINUITY` between
/// two parts; its URL.
fn archive(o: &Origin, parts: &[(&str, &[Vec<u8>])]) -> String {
    let mut p = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:EVENT\n");
    for (n, (name, segments)) in parts.iter().enumerate() {
        if n > 0 {
            p.push_str("#EXT-X-DISCONTINUITY\n");
        }
        for (i, seg) in segments.iter().enumerate() {
            let path = format!("/p/{name}{i:02}.ts");
            o.put(&path, Answer::ok(seg.clone()));
            p.push_str(&format!("#EXTINF:6.000,\n{path}\n"));
        }
    }
    p.push_str("#EXT-X-ENDLIST\n");
    o.put("/programme.m3u8", Answer::playlist(p));
    o.url("/programme.m3u8")
}

/// What one demux of an HLS stream pushed: the audio AUs (bytes, pts) and the video pts.
struct Pushed {
    audio: Vec<(Vec<u8>, i64)>,
    video: Vec<i64>,
}

/// `ff::demux` over the HLS stream `url` to its end, the Load having declared `acodec`; `pcm`
/// decodes the audio to stereo PCM, as webOS 10+ does, else it is fed as it is.
fn demux_hls(url: &str, acodec: &str, pcm: bool) -> Pushed {
    SHARED.seek_to_ns.store(-1, Ordering::Relaxed);
    SHARED.desired_audio_idx.store(-1, Ordering::Relaxed);
    SHARED.demux_failed.store(false, Ordering::Relaxed);
    SHARED.demux_io_failed.store(false, Ordering::Relaxed);
    follow_archive(None);
    FEED_AUDIO.store(true, Ordering::Relaxed);
    set_soft_pcm(if pcm { adec::PCM_CHANNELS } else { 0 });
    let (video_cap, audio_cap) = crate::player::engine::aq_caps();
    let mut video = crate::aq::aq_new(video_cap as c_long);
    let mut audio = crate::aq::aq_new(audio_cap as c_long);
    let mut hs = tv_net::stream::http_stream_boxed();
    let (vp, ap, hp) = (SendPtr(&mut *video as *mut AuQueue), SendPtr(&mut *audio as *mut AuQueue), SendPtr(&mut *hs as *mut HttpStream));
    let (url, acodec) = (url.to_owned(), acodec.to_owned());
    let th = std::thread::spawn(move || {
        let off = tv_base::task::OffFrame::for_test();
        demux(&off, url, crate::headers::RequestHeaders::none(), acodec, vp, ap, hp)
    });
    let pop_all = |q: &mut AuQueue, out: &mut Vec<(Vec<u8>, i64)>| loop {
        let node = crate::aq::aq_pop(q, std::ptr::null_mut());
        if node.is_null() {
            break;
        }
        let (_es, _key, pts, len, data) = unsafe { crate::aq::au_fields(node) };
        out.push((unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec(), pts));
        unsafe { libc::free(node as *mut c_void) };
    };
    let (mut aus, mut vids) = (Vec::new(), Vec::new());
    let deadline = Instant::now() + Duration::from_secs(60);
    while !th.is_finished() {
        assert!(Instant::now() < deadline, "the demuxer did not finish the stream");
        pop_all(&mut audio, &mut aus);
        pop_all(&mut video, &mut vids);
        std::thread::sleep(Duration::from_millis(5));
    }
    th.join().unwrap();
    pop_all(&mut audio, &mut aus);
    pop_all(&mut video, &mut vids);
    set_soft_pcm(0);
    Pushed { audio: aus, video: vids.into_iter().map(|(_, pts)| pts).collect() }
}

/// From the first timestamp to the last, ms, and the longest step between two in a row.
fn span_ms(ts: &[i64]) -> (i64, i64) {
    let span = ts.last().zip(ts.first()).map_or(0, |(l, f)| l - f);
    let step = ts.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(0);
    (span / 1_000_000, step / 1_000_000)
}

/// The host FFmpeg with its audio decoder, libcurl and the fixture, under the crate-wide lock;
/// `None` (after a SKIP line) for any missing.
fn ready() -> Option<(tv_base::testlock::Serial, Bound, &'static Parts)> {
    let Some(g) = curl_gate() else {
        eprintln!("SKIP: no libcurl");
        return None;
    };
    let bound = Bound::new()?;
    Some((g, bound, fixture()?))
}

/// **Played through the break and out of it, video and audio go on**: the video from its new PID
/// and back, the audio in the played language (the second of the break's three tracks), as it is
/// fed to a television that decodes it, and decoded to PCM as webOS 10+ plays it.
#[test]
fn a_programme_map_that_changes_at_a_discontinuity_keeps_its_video_and_audio_going() {
    let Some((_g, _ffmpeg, fx)) = ready() else { return };
    let o = Origin::spawn();
    let url = archive(&o, &[("a", &fx[0]), ("c", &fx[1]), ("b", &fx[2])]);
    let length_ms = 3 * PART_S * 1000;

    let fed = demux_hls(&url, "mp2", false);
    assert!(!SHARED.demux_failed.load(Ordering::Acquire), "the demux did not fail");
    let (span, step) = span_ms(&fed.video);
    eprintln!("video: {} AUs over {span} ms, longest step {step} ms", fed.video.len());
    assert!(span >= length_ms - 1_000 && step < 200, "the video goes on through both breaks: {span} ms, a step of {step} ms");
    let (span, step) = span_ms(&fed.audio.iter().map(|a| a.1).collect::<Vec<_>>());
    eprintln!("audio: {} AUs over {span} ms, longest step {step} ms", fed.audio.len());
    assert!(span >= length_ms - 1_000 && step < 600, "the audio goes on through both breaks: {span} ms, a step of {step} ms");
    let other: Vec<usize> = fed.audio.iter().map(|a| a.0.len()).filter(|&n| n != MP2_POL_FRAME).collect();
    assert!(
        other.is_empty(),
        "every audio frame is of the played language's track, none of the break's first: {:?}",
        &other[..other.len().min(8)]
    );

    let decoded = demux_hls(&url, "mp2", true);
    assert!(!SHARED.demux_failed.load(Ordering::Acquire), "the PCM demux did not fail");
    assert!(span_ms(&decoded.video).0 >= length_ms - 1_000, "the video goes on");
    let (span, step) = span_ms(&decoded.audio.iter().map(|a| a.1).collect::<Vec<_>>());
    eprintln!("pcm: {} AUs over {span} ms, longest step {step} ms", decoded.audio.len());
    assert!(span >= length_ms - 1_000 && step < 600, "the PCM goes on through both breaks: {span} ms, a step of {step} ms");
}

/// **A break with no track the pipeline was set up for fails the source**, so the playback restarts
/// and decides its audio afresh (an undecodable codec is refused there): AC-3 fed as it is, from the
/// break, has nothing to go on with after it, where the only track is MP2.
#[test]
fn a_programme_change_with_no_track_to_feed_fails_the_source() {
    let Some((_g, _ffmpeg, fx)) = ready() else { return };
    let o = Origin::spawn();
    let url = archive(&o, &[("c", &fx[1]), ("b", &fx[2])]);
    let fed = demux_hls(&url, "ac3", false);
    assert!(SHARED.demux_io_failed.load(Ordering::Acquire), "the source failed at the break");
    let (span, _) = span_ms(&fed.video);
    assert!((PART_S * 1000 - 2_000..PART_S * 1000 + 1_000).contains(&span), "the break played to its end: {span} ms");
    assert!(fed.audio.iter().all(|a| a.0.len() == 768), "AC-3 frames until then");
    SHARED.demux_failed.store(false, Ordering::Relaxed);
    SHARED.demux_io_failed.store(false, Ordering::Relaxed);
}
