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

/// MP2 `eng` at 192 kbit/s: 576-byte frames.
const ENG: Track = ("eng", "mp2", "192k", 2, 660);
const MP2_ENG_FRAME: usize = 576;

/// One part of a fixture: six 6 s MPEG-TS segments of 320x180 H.264 from `offset` s on, with
/// these audio `tracks`; the video on PID 0x200 and the audio from 0x201 when `new_pids`, else
/// on ffmpeg's own from 0x100. Its audio starts `lag` s after its video, and the muxer holds up
/// to half of `muxdelay` s of a track's audio for one PES (ffmpeg's `-muxdelay`, 0.7 by default).
#[derive(Clone, Copy)]
pub(crate) struct Part {
    name: &'static str,
    offset: i64,
    tracks: &'static [Track],
    new_pids: bool,
    lag: f64,
    muxdelay: f64,
}

const PART: Part = Part { name: "", offset: 0, tracks: &[POL], new_pids: false, lag: 0.0, muxdelay: 0.7 };

/// **The fixture**, written by the `ffmpeg` on PATH into `dir`: three parts, their timestamps
/// going on from part to part.
/// - `a`: the video on PID 0x100, one MP2 track (`pol`) on 0x101;
/// - `c`, the break: the video on 0x200, MP2 `eng` at 192 kbit/s, MP2 `pol` and AC-3 5.1 `eng`
///   on 0x201..=0x203, so the first track is not the one in the played language;
/// - `b`: the first map again, its PIDs and its one track.
pub(crate) fn programme_change(dir: &Path) -> Result<Parts, String> {
    write_parts(dir, PROGRAMME_CHANGE)
}

/// **A fixture whose break shares no audio codec with the programme around it**, written as
/// [`programme_change`]: `a` and `b` AAC stereo only, the break `c` AC-3 5.1 only, on new PIDs.
pub(crate) fn codec_change(dir: &Path) -> Result<Parts, String> {
    const AAC: Track = ("pol", "aac", "128k", 2, 440);
    let aac = Part { tracks: &[AAC], ..PART };
    let ac3: &[Track] = &[("pol", "ac3", "384k", 6, 880)];
    write_parts(
        dir,
        [
            Part { name: "a", ..aac },
            Part { name: "c", offset: PART_S, tracks: ac3, new_pids: true, ..aac },
            Part { name: "b", offset: 2 * PART_S, ..aac },
        ],
    )
}

const PROGRAMME_CHANGE: [Part; 3] = [
    Part { name: "a", ..PART },
    Part { name: "c", offset: PART_S, tracks: &[ENG, POL, ("eng", "ac3", "192k", 6, 880)], new_pids: true, ..PART },
    Part { name: "b", offset: 2 * PART_S, ..PART },
];

/// Write `parts` into `dir` with the `ffmpeg` on PATH: each its six segments.
fn write_parts(dir: &Path, parts: [Part; 3]) -> Result<Parts, String> {
    let mut out: Parts = Default::default();
    for (n, Part { name, offset, tracks, new_pids, lag, muxdelay }) in parts.into_iter().enumerate() {
        let mut args: Vec<String> =
            ["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=25"].map(String::from).to_vec();
        for t in tracks {
            args.extend(["-itsoffset".into(), lag.to_string(), "-f".into(), "lavfi".into(), "-i".into()]);
            args.push(format!("sine=frequency={}:sample_rate=48000", t.4));
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
        args.extend(["-muxdelay".into(), muxdelay.to_string(), "-output_ts_offset".into(), offset.to_string()]);
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

/// `parts` written into a scratch directory of its own, `tag`: `None` (after a SKIP line) without
/// the `ffmpeg` command line tool.
fn written(tag: &str, parts: [Part; 3]) -> Option<Parts> {
    if !tool_present("ffmpeg") {
        eprintln!("SKIP: no ffmpeg command line tool to write the fixture");
        return None;
    }
    let dir = Scratch::new(tag);
    Some(write_parts(&dir.0, parts).expect("the fixture"))
}

/// The fixture, written once per test binary; `None` (after a SKIP line) without the `ffmpeg`
/// command line tool.
fn fixture() -> Option<&'static Parts> {
    static FIXTURE: std::sync::OnceLock<Option<Parts>> = std::sync::OnceLock::new();
    FIXTURE.get_or_init(|| written("programme-change", PROGRAMME_CHANGE)).as_ref()
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
    demux_hls_track(url, acodec, pcm, -1)
}

/// [`demux_hls`] with the audio track menu's choice: the `track`th audio stream (-1: none).
fn demux_hls_track(url: &str, acodec: &str, pcm: bool, track: i32) -> Pushed {
    SHARED.seek_to_ns.store(-1, Ordering::Relaxed);
    SHARED.desired_audio_idx.store(track, Ordering::Relaxed);
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
    SHARED.desired_audio_idx.store(-1, Ordering::Relaxed);
    Pushed { audio: aus, video: vids.into_iter().map(|(_, pts)| pts).collect() }
}

/// From the first timestamp to the last, ms, and the longest step between two in a row.
fn span_ms(ts: &[i64]) -> (i64, i64) {
    let span = ts.last().zip(ts.first()).map_or(0, |(l, f)| l - f);
    let step = ts.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(0);
    (span / 1_000_000, step / 1_000_000)
}

/// The host FFmpeg with its audio decoder and libcurl, under the crate-wide lock; `None` (after a
/// SKIP line) for either missing.
fn gate() -> Option<(tv_base::testlock::Serial, Bound)> {
    let Some(g) = curl_gate() else {
        eprintln!("SKIP: no libcurl");
        return None;
    };
    Some((g, Bound::new()?))
}

/// [`gate`] and the shared fixture.
fn ready() -> Option<(tv_base::testlock::Serial, Bound, &'static Parts)> {
    let (g, bound) = gate()?;
    Some((g, bound, fixture()?))
}

/// **Played through the break and out of it, video and audio go on**: the video from its new PID
/// and back, the audio in the played language (the second of the break's three tracks), as it is
/// fed to a television that decodes it, and decoded to PCM as webOS 10+ plays it. Each move of the
/// audio misses [`AUDIO_GONE_NS`] of it.
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
    assert!(span >= length_ms - 1_000 && step < 1_500, "the audio goes on through both breaks: {span} ms, a step of {step} ms");
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
    assert!(span >= length_ms - 1_000 && step < 1_500, "the PCM goes on through both breaks: {span} ms, a step of {step} ms");
}

/// **A break whose streams the open already saw is followed as well**: opened 1.5 s before the
/// break, with no keyframe in between (a short segment, or an open late in one), the open's
/// stream-info probe reads on into the break for the video's parameters and adds the break's
/// streams before a packet is played: they are not new to the demuxer when the break begins, and
/// the first map's, which the last part plays again, are not the ones the open chose. The video
/// and the audio go on through both breaks.
#[test]
fn a_break_whose_streams_the_open_already_saw_keeps_its_video_and_audio_going() {
    let Some((_g, _ffmpeg, fx)) = ready() else { return };
    let o = Origin::spawn();
    // The segment before the break: its SDT, PAT and PMT, then its last quarter, cut at a TS packet.
    let last = &fx[0][5];
    let tail = [&last[..3 * 188], &last[last.len() - last.len() / 188 / 4 * 188..]].concat();
    let url = archive(&o, &[("a", &[tail]), ("c", &fx[1]), ("b", &fx[2])]);
    let length_ms = 2 * PART_S * 1000;
    let fed = demux_hls(&url, "mp2", false);
    let seen = SHARED.track_names.lock().unwrap().audio.len();
    assert_eq!(seen, 4, "the open saw the break's three audio tracks besides the played one");
    assert!(!SHARED.demux_failed.load(Ordering::Acquire), "the demux did not fail");
    let (span, step) = span_ms(&fed.video);
    eprintln!("video: {} AUs over {span} ms, longest step {step} ms", fed.video.len());
    assert!(span >= length_ms - 1_000 && step < 200, "the video goes on through both breaks: {span} ms, a step of {step} ms");
    let (span, step) = span_ms(&audio_pts(&fed));
    assert!(span >= length_ms - 1_000 && step < 1_500, "the audio goes on through both breaks: {span} ms, a step of {step} ms");
}

/// **A break with no track the pipeline was set up for asks for another Load of one the television
/// plays**, as the C1 met it after a restart had opened the AC-3 programme: AC-3 fed as it is, from
/// the break, has nothing to go on with after it, where the only track is MP2, which the television
/// plays decoded to PCM. The demuxer asks for that track (the programme's first; the pump reloads
/// for it once the playhead is where the AC-3 fed ends, from the programme's first segment), the
/// video goes on meanwhile, and nothing fails.
#[test]
fn a_programme_change_with_no_track_of_the_fed_codec_asks_for_another_load() {
    let Some((_g, _ffmpeg, fx)) = ready() else { return };
    let o = Origin::spawn();
    let url = archive(&o, &[("c", &fx[1]), ("b", &fx[2])]);
    *SHARED.programme_audio.lock().unwrap() = None;
    let fed = demux_hls(&url, "ac3", false);
    assert!(!SHARED.demux_io_failed.load(Ordering::Acquire), "the source did not fail");
    let (span, step) = span_ms(&fed.video);
    assert!(span >= 2 * PART_S * 1000 - 1_000 && step < 200, "the video goes on through the break: {span} ms, a step of {step} ms");
    assert!(!fed.audio.is_empty() && fed.audio.iter().all(|a| a.0.len() == 768), "AC-3 frames until then: {}", fed.audio.len());
    let asked = SHARED.programme_audio.lock().unwrap().take().expect("another Load asked for");
    assert_eq!(
        (asked.track, asked.codec.as_str(), asked.channels, asked.seq),
        (0, "mp2", 2, Some(6)),
        "the break's MP2, from its first segment"
    );
    // A PES of the AC-3 that mpegts completes in the break is fed after the question.
    let ac3_end = fed.audio.last().map_or(0, |a| a.1);
    assert!((ac3_end - 100_000_000..=ac3_end).contains(&asked.at_ns), "from where the AC-3 fed ends: {} of {ac3_end}", asked.at_ns);
}

/// **A pick still waiting for the playhead is not lost to a later break**: an AAC Load reads ahead
/// through an AC-3 break, back into AAC and into a second AC-3 break before the pump has taken the
/// first break's pick. That pick stands (the first break would otherwise play with no audio); the
/// demuxer the reload opens asks for the second one again when it gets there.
#[test]
fn a_pick_waiting_for_the_playhead_stands_through_a_later_break() {
    let Some((_g, _ffmpeg)) = gate() else { return };
    if !tool_present("ffmpeg") {
        eprintln!("SKIP: no ffmpeg command line tool to write the fixture");
        return;
    }
    let dir = Scratch::new("codec-change-twice");
    let [a, c, b] = codec_change(&dir.0).expect("the fixture");
    let o = Origin::spawn();
    let url = archive(&o, &[("a", &a), ("c", &c), ("b", &b), ("d", &c)]);
    *SHARED.programme_audio.lock().unwrap() = None;
    let fed = demux_hls(&url, "aac", false);
    assert!(!SHARED.demux_io_failed.load(Ordering::Acquire), "the source did not fail");
    assert!(span_ms(&fed.video).0 >= 4 * PART_S * 1000 - 1_000, "the video goes on through both breaks");
    let asked = SHARED.programme_audio.lock().unwrap().take().expect("another Load asked for");
    assert_eq!((asked.codec.as_str(), asked.channels, asked.seq), ("ac3", 6, Some(6)), "the first break's AC-3, from its first segment");
    // The AAC fed before the first break: up to the gap the break leaves in it.
    let a_end = fed.audio.windows(2).find(|w| w[1].1 - w[0].1 > PART_S * 1_000_000_000 / 2).map_or(0, |w| w[0].1);
    assert!((a_end - 100_000_000..=a_end + 100_000_000).contains(&asked.at_ns), "from where the AAC fed ends: {} of {a_end}", asked.at_ns);
}

/// **An open with no audio track of the Load's codec fails the source instead of feeding another
/// one**: an AC-3 Load over the MP2 programme (a reload for the programme after a break that opened
/// before it) would feed MP2 to a pipeline set up for AC-3, which wedges a television. Decoded to
/// PCM, any track the decoder takes plays, as before.
#[test]
fn an_open_with_no_track_of_the_loads_codec_fails_the_source() {
    let Some((_g, _ffmpeg, fx)) = ready() else { return };
    let o = Origin::spawn();
    let url = archive(&o, &[("a", &fx[0])]);
    let fed = demux_hls(&url, "ac3", false);
    assert!(SHARED.demux_io_failed.load(Ordering::Acquire), "the source failed");
    assert!(fed.audio.is_empty() && fed.video.is_empty(), "nothing fed: {} audio, {} video AUs", fed.audio.len(), fed.video.len());
    SHARED.demux_failed.store(false, Ordering::Relaxed);
    SHARED.demux_io_failed.store(false, Ordering::Relaxed);

    let decoded = demux_hls(&url, "ac3", true);
    assert!(!SHARED.demux_failed.load(Ordering::Acquire), "the PCM demux did not fail");
    assert!(span_ms(&audio_pts(&decoded)).0 >= PART_S * 1000 - 1_000, "the MP2 decoded to PCM plays");
}

/// **A reload for a programme's audio opens at the segment the programme begins with**, and reads
/// positions from there: started at the break's first segment, the AC-3 Load finds its track and
/// feeds it, where an open at the start would fail on the MP2 programme before it.
#[test]
fn a_reload_for_a_programmes_audio_opens_where_the_programme_begins() {
    let Some((_g, _ffmpeg, fx)) = ready() else { return };
    let o = Origin::spawn();
    let url = archive(&o, &[("a", &fx[0]), ("c", &fx[1])]);
    SHARED.hls_start_seq.store(6, Ordering::Relaxed);
    let fed = demux_hls_track(&url, "ac3", false, 2);
    assert_eq!(SHARED.hls_start_seq.load(Ordering::Relaxed), -1, "taken by the open");
    assert!(!SHARED.demux_failed.load(Ordering::Acquire), "the demux did not fail");
    assert_eq!(SHARED.hls_vod_base_ns.load(Ordering::Acquire), PART_S * 1_000_000_000, "positions from the break's start");
    assert!(span_ms(&fed.video).0 >= PART_S * 1000 - 1_000, "the break's video");
    assert!(span_ms(&audio_pts(&fed)).0 >= PART_S * 1000 - 1_500, "its audio");
    assert!(fed.audio.iter().all(|a| a.0.len() == 768), "every frame the break's AC-3");
}

/// The audio AUs' timestamps.
fn audio_pts(p: &Pushed) -> Vec<i64> {
    p.audio.iter().map(|a| a.1).collect()
}

/// **A programme change that drops the played track and keeps another one from the open**: the
/// audio goes on in the kept one, and back in the chosen language once its track returns. The
/// open's map has MP2 `eng` and `pol` on 0x101 and 0x102, `pol` chosen in the track menu; the break
/// keeps the video and `eng` on their PIDs and drops `pol`; then the first map again, where `pol`
/// plays, from where `eng` had got to, as fed to a television that decodes it and decoded to PCM. A
/// track that is not played must still be read, or no packet of it shows that the played one has
/// gone.
#[test]
fn a_programme_change_that_drops_the_played_track_moves_the_audio_to_one_it_keeps() {
    let Some((_g, _ffmpeg)) = gate() else { return };
    let two = Part { tracks: &[ENG, POL], ..PART };
    let parts = [
        Part { name: "x", ..two },
        Part { name: "y", offset: PART_S, tracks: &[ENG], ..PART },
        Part { name: "z", offset: 2 * PART_S, ..two },
    ];
    let Some([x, y, z]) = written("drops-played", parts) else { return };
    let o = Origin::spawn();
    let url = archive(&o, &[("x", &x), ("y", &y), ("z", &z)]);
    let length_ms = 3 * PART_S * 1000;
    for pcm in [false, true] {
        let fed = demux_hls_track(&url, "mp2", pcm, 1);
        assert!(!SHARED.demux_failed.load(Ordering::Acquire), "the demux did not fail");
        assert!(span_ms(&fed.video).0 >= length_ms - 1_000, "the video goes on");
        let pts = audio_pts(&fed);
        let (span, step) = span_ms(&pts);
        let back = pts.windows(2).map(|w| w[1] - w[0]).min().unwrap_or(0) / 1_000_000;
        eprintln!("audio (pcm {pcm}): {} AUs over {span} ms, steps from {back} to {step} ms", fed.audio.len());
        assert!(span >= length_ms - 1_000 && step < 1_500, "the audio goes on through both breaks: {span} ms, a step of {step} ms");
        assert!(back >= 0, "no audio fed twice where the track changed: a step of {back} ms");
        if !pcm {
            let mut tracks: Vec<usize> = fed.audio.iter().map(|a| a.0.len()).collect();
            tracks.dedup();
            assert_eq!(tracks, [MP2_POL_FRAME, MP2_ENG_FRAME, MP2_POL_FRAME], "pol as chosen, eng in the break, pol once it is back");
        }
    }
}

/// **A track the break keeps is not taken for gone while its muxer holds its audio back**: a muxer
/// fills one PES of a low-rate track for up to half its delay (600 ms here, ffmpeg's `-muxdelay
/// 1.2`) while it writes the break's added AC-3 a frame at a time, so the break begins with more
/// than half a second of AC-3 only. The played MP2 goes on, nothing fails.
#[test]
fn a_kept_track_whose_audio_comes_late_in_the_break_goes_on() {
    let Some((_g, _ffmpeg)) = gate() else { return };
    // MP2 at 32 kbit/s mono: 96-byte frames.
    const LOW: Track = ("pol", "mp2", "32k", 1, 440);
    let low = Part { tracks: &[LOW], muxdelay: 1.2, ..PART };
    let ac3: &[Track] = &[LOW, ("eng", "ac3", "384k", 6, 880)];
    let parts =
        [Part { name: "a", ..low }, Part { name: "c", offset: PART_S, tracks: ac3, ..low }, Part { name: "b", offset: 2 * PART_S, ..low }];
    let Some([a, c, b]) = written("late-pes", parts) else { return };
    let o = Origin::spawn();
    let url = archive(&o, &[("a", &a), ("c", &c), ("b", &b)]);
    let length_ms = 3 * PART_S * 1000;
    let fed = demux_hls(&url, "mp2", false);
    assert!(!SHARED.demux_failed.load(Ordering::Acquire), "the demux did not fail");
    let (span, step) = span_ms(&audio_pts(&fed));
    eprintln!("audio: {} AUs over {span} ms, longest step {step} ms", fed.audio.len());
    assert!(span >= length_ms - 1_000 && step < 600, "the audio goes on through both breaks: {span} ms, a step of {step} ms");
    assert!(fed.audio.iter().all(|a| a.0.len() == 96), "every frame the played track's");
}

/// **A break whose audio starts after its video, on new PIDs, leaves its video where it is**: the
/// audio moves to the break's track a while after the video did, and its first packet there is no
/// reason to place the run again. The video never steps back.
#[test]
fn audio_that_starts_late_on_new_pids_does_not_move_the_video_back() {
    let Some((_g, _ffmpeg)) = gate() else { return };
    let parts = [
        Part { name: "a", ..PART },
        Part { name: "c", offset: PART_S, new_pids: true, lag: 0.4, ..PART },
        Part { name: "b", offset: 2 * PART_S, ..PART },
    ];
    let Some([a, c, b]) = written("late-audio", parts) else { return };
    let o = Origin::spawn();
    let url = archive(&o, &[("a", &a), ("c", &c), ("b", &b)]);
    let length_ms = 3 * PART_S * 1000;
    let fed = demux_hls(&url, "mp2", false);
    assert!(!SHARED.demux_failed.load(Ordering::Acquire), "the demux did not fail");
    let (span, step) = span_ms(&fed.video);
    let back = fed.video.windows(2).map(|w| w[1] - w[0]).min().unwrap_or(0) / 1_000_000;
    eprintln!("video: {} AUs over {span} ms, steps from {back} to {step} ms", fed.video.len());
    assert!(back >= 0 && span >= length_ms - 1_000, "the video never steps back: {back} ms, over {span} ms");
    assert!(span_ms(&audio_pts(&fed)).0 >= length_ms - 1_500, "the audio goes on");
}

/// **A break whose only track the television cannot play fails the source**, so the restart refuses
/// it as undecodable: Opus, after MP2 decoded to PCM, and after MP2 fed as it is, where no other
/// Load would play it either.
#[test]
fn a_break_with_only_a_track_the_television_cannot_play_fails_the_source() {
    let Some((_g, _ffmpeg)) = gate() else { return };
    let opus: &[Track] = &[("eng", "libopus", "96k", 2, 660)];
    let parts = [
        Part { name: "a", ..PART },
        Part { name: "c", offset: PART_S, tracks: opus, new_pids: true, ..PART },
        Part { name: "b", offset: 2 * PART_S, ..PART },
    ];
    let Some([a, c, _]) = written("opus-only", parts) else { return };
    let o = Origin::spawn();
    let url = archive(&o, &[("a", &a), ("c", &c)]);
    for pcm in [true, false] {
        *SHARED.programme_audio.lock().unwrap() = None;
        let fed = demux_hls(&url, "mp2", pcm);
        assert!(SHARED.demux_io_failed.load(Ordering::Acquire), "the source failed at the break (pcm {pcm})");
        assert_eq!(*SHARED.programme_audio.lock().unwrap(), None, "no other Load asked for");
        let (span, _) = span_ms(&fed.video);
        assert!((PART_S * 1000 - 2_000..PART_S * 1000 + 2_000).contains(&span), "the first part played to its end: {span} ms");
        assert!(span_ms(&audio_pts(&fed)).0 >= PART_S * 1000 - 2_000, "audio until then");
        SHARED.demux_failed.store(false, Ordering::Relaxed);
        SHARED.demux_io_failed.store(false, Ordering::Relaxed);
    }
}
