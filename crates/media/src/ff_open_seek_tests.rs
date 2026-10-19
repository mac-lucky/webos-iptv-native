//! **A debrid stream's first open, and seeking a Matroska file whose Cues sit at its end**, through
//! the real bundled FFmpeg over a loopback server that misbehaves on cue.
//!
//! The field case: a ~100 GB UHD remux MKV from a debrid link. Its first start was refused —
//! `probe: open_input failed r=-1094995529` (`AVERROR_INVALIDDATA`) — and the second start of the
//! same stream played: the first hop answered with something that was not the file (an HTML error
//! page) or cut it short. Both opens (the probe's and the demuxer's) now retry ONCE from the
//! original URL, which re-resolves the redirect; a URL that keeps failing still fails, after that
//! one retry.
//!
//! The seek half guards what the pump's fix relies on: libavformat's matroska demuxer reads the
//! Cues on the FIRST seek of a context (`matroska_read_seek` → `matroska_parse_cues`, FFmpeg 9.0
//! `libavformat/matroskadec.c`) and keeps the index in the `AVStream` for the context's life, so
//! an in-place seek (`SHARED.seek_to_ns`) after the first never fetches the Cues again. Only a
//! reload — a fresh `AVFormatContext` — would.
//!
//! Like `ff_eof_seek_tests.rs`, these need the host build of the bundled FFmpeg and the `ffmpeg`
//! CLI, so they are `#[ignore]`d in the plain suite:
//! `SN_FFMPEG_DIR=<dir holding libav*-wi.*.dylib> CARGO_INCREMENTAL=0 cargo test -p tv_media --lib
//! ff::open_seek_tests -- --ignored`.

use super::*;
use std::io::{Read, Write};
use std::os::raw::c_long;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the server answers the n-th (0-based) request for `/file`.
#[derive(Clone, Copy, Debug)]
enum Answer {
    /// `200 text/html`: a debrid host's error page in place of the file.
    Html,
    /// `200 image/png`: the wrong file of the torrent (the field case: an HDR10+ test chart).
    Png,
    /// The file's headers (its full length), then only this many bytes, then a close.
    Cut(usize),
    /// The file, honouring `Range`.
    File,
}

/// One request the server saw: the path and the `Range` start.
type Seen = Arc<Mutex<Vec<(String, Option<u64>)>>>;

/// A loopback debrid host: `/start` redirects (307) to `/file`, which answers per `script`.
/// One thread per connection, `Connection: close`; stops on drop.
struct Debrid {
    port: u16,
    seen: Seen,
    stop: Arc<AtomicBool>,
}

impl Debrid {
    fn start(file: Arc<Vec<u8>>, script: fn(usize) -> Answer) -> Debrid {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).expect("set_nonblocking");
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (log, accept_stop) = (seen.clone(), stop.clone());
        std::thread::spawn(move || {
            let file_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            while !accept_stop.load(Ordering::Acquire) {
                match tv_base::testnet::accept(&listener) {
                    Ok((s, _)) => {
                        let (file, log, n) = (file.clone(), log.clone(), file_requests.clone());
                        std::thread::spawn(move || serve(s, port, &file, &log, &n, script));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(_) => break,
                }
            }
        });
        Debrid { port, seen, stop }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/start", self.port)
    }

    fn seen(&self) -> Vec<(String, Option<u64>)> {
        self.seen.lock().unwrap().clone()
    }

    fn hits(&self, path: &str) -> usize {
        self.seen().iter().filter(|(p, _)| p == path).count()
    }
}

impl Drop for Debrid {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn serve(
    mut s: std::net::TcpStream,
    port: u16,
    file: &[u8],
    log: &Mutex<Vec<(String, Option<u64>)>>,
    file_requests: &std::sync::atomic::AtomicUsize,
    script: fn(usize) -> Answer,
) {
    let _ = s.set_nonblocking(false);
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => return,
            Ok(k) => buf.extend_from_slice(&tmp[..k]),
        }
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
    let range = head.lines().find_map(|l| {
        l.to_ascii_lowercase()
            .strip_prefix("range: bytes=")
            .and_then(|r| r.split('-').next().and_then(|a| a.trim().parse::<u64>().ok()))
    });
    log.lock().unwrap().push((path.clone(), range));
    if path == "/start" {
        let _ = write!(
            s,
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:{port}/file\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return;
    }
    let answer = script(file_requests.fetch_add(1, Ordering::AcqRel));
    let n = file.len() as u64;
    let page: Option<(&str, Vec<u8>)> = match answer {
        Answer::Html => Some((
            "text/html",
            b"<!DOCTYPE html><html><head><title>503</title></head><body>Service unavailable, try again.</body></html>".to_vec(),
        )),
        // a PNG signature and IHDR, then filler: libavformat finds no container in it
        Answer::Png => Some(("image/png", [&b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\x0b\xb8\0\0\x04\xb0"[..], &[0x55; 20_000][..]].concat())),
        _ => None,
    };
    if let Some((content_type, body)) = page {
        let _ = write!(
            s,
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = s.write_all(&body);
        return;
    }
    let at = range.unwrap_or(0).min(n);
    let h = match range {
        Some(_) => format!(
            "HTTP/1.1 206 Partial Content\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {at}-{}/{n}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            n - 1,
            n - at
        ),
        None => format!("HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {n}\r\nConnection: close\r\n\r\n"),
    };
    let _ = s.write_all(h.as_bytes());
    let end = match answer {
        Answer::Cut(k) => (at as usize + k).min(file.len()),
        _ => file.len(),
    };
    // ~8 MB/s: a remote host, not loopback. Unthrottled, the demuxer read the whole fixture into
    // its lanes before the first seek and found the Cues still in its AVIO buffer.
    for chunk in file[at as usize..end].chunks(64 * 1024) {
        if s.write_all(chunk).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    let _ = s.flush();
}

fn tool_present(name: &str) -> bool {
    Command::new(name)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|st| st.success())
}

/// A scratch directory removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("sn-open-seek-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 10 s of 320x240 H.264 (a keyframe a second; noise, so it really is ~6 Mbit/s and a seek leaves
/// the AVIO buffer) + AAC,
/// in Matroska as FFmpeg's muxer writes it — Cues at the END — with three chapters.
fn generate_mkv(dir: &Path) -> Vec<u8> {
    let meta = dir.join("chapters.txt");
    std::fs::write(
        &meta,
        ";FFMETADATA1\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=3000\ntitle=Opening\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=3000\nEND=7000\ntitle=Middle\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=7000\nEND=10000\n",
    )
    .expect("write the chapter metadata");
    let out = dir.join("film.mkv");
    let status = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc=s=320x240:r=25:d=10,noise=alls=60:allf=t"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=10"])
        .arg("-i")
        .arg(&meta)
        .args(["-map", "0:v", "-map", "1:a", "-map_chapters", "2"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "25"])
        .args(["-b:v", "6M", "-c:a", "aac", "-b:a", "64k"])
        .arg(&out)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg could not write the fixture");
    std::fs::read(&out).expect("read the fixture")
}

/// The byte offset of the Matroska `Cues` element (ID 1C 53 BB 6B) — the occurrence that is not
/// the SeekHead's `SeekID` value naming it (preceded by 53 AB 84).
fn cues_offset(file: &[u8]) -> u64 {
    const ID: [u8; 4] = [0x1C, 0x53, 0xBB, 0x6B];
    let at = (3..file.len() - 4)
        .find(|&i| file[i..i + 4] == ID && file[i - 3..i] != [0x53, 0xAB, 0x84])
        .expect("fixture: no Cues element");
    assert!(at as f64 > file.len() as f64 * 0.9, "fixture: the Cues are not at the end ({at} of {})", file.len());
    at as u64
}

fn fixture(tag: &str) -> Option<(Scratch, Arc<Vec<u8>>)> {
    if !tool_present("ffmpeg") {
        eprintln!("SKIP {tag}: the ffmpeg command line tool is not installed");
        return None;
    }
    let scratch = Scratch::new(tag);
    let file = Arc::new(generate_mkv(&scratch.0));
    Some((scratch, file))
}

// -- the probe ---------------------------------------------------------------------------

fn probe(url: &str) -> Result<crate::stream::ProbedStreams, crate::stream::ProbeFailure> {
    let io = super::probe::ProbeIo::new();
    super::probe::probe(url, &crate::headers::RequestHeaders::none(), &io)
}

#[test]
#[ignore = "needs the host build of the bundled FFmpeg and the ffmpeg CLI — see the module doc"]
fn a_probe_whose_first_hop_serves_an_error_page_retries_once_and_opens() {
    let _g = tv_base::testlock::serial();
    let Some((_s, file)) = fixture("probe-html") else { return };
    let _ffmpeg = super::test_support::bind_host_ffmpeg();
    let server = Debrid::start(file, |n| if n == 0 { Answer::Html } else { Answer::File });
    let probed = probe(&server.url()).expect("the retry from the original URL opens the file");
    assert!(probed.video.is_some(), "the file's video stream was read");
    assert_eq!(server.hits("/start"), 2, "the retry re-resolved the redirect: {:?}", server.seen());
}

#[test]
#[ignore = "needs the host build of the bundled FFmpeg and the ffmpeg CLI — see the module doc"]
fn a_probe_whose_first_transfer_is_cut_inside_the_header_retries_once_and_opens() {
    let _g = tv_base::testlock::serial();
    let Some((_s, file)) = fixture("probe-cut") else { return };
    let _ffmpeg = super::test_support::bind_host_ffmpeg();
    let server = Debrid::start(file, |n| if n == 0 { Answer::Cut(40) } else { Answer::File });
    let probed = probe(&server.url()).expect("the retry opens the file");
    assert!(probed.video.is_some());
    assert_eq!(server.hits("/start"), 2, "{:?}", server.seen());
}

#[test]
#[ignore = "needs the host build of the bundled FFmpeg and the ffmpeg CLI — see the module doc"]
fn a_probe_of_a_url_that_never_serves_the_file_fails_after_exactly_two_retries() {
    let _g = tv_base::testlock::serial();
    let Some((_s, file)) = fixture("probe-bad") else { return };
    let _ffmpeg = super::test_support::bind_host_ffmpeg();
    let server = Debrid::start(file, |_| Answer::Html);
    // a page that is still a page after the retries says so
    assert_eq!(
        probe(&server.url()).err(),
        Some(crate::stream::ProbeFailure::NotVideo(crate::stream::NotVideo::WebPage))
    );
    assert_eq!(server.hits("/start"), 3, "one attempt and two retries: {:?}", server.seen());
}

/// The field case: the link answers an image (the torrent's HDR10+ test chart). An image will
/// not turn into the film, so the probe refuses at once, without the ~4.5 s of waiting retries.
#[test]
#[ignore = "needs the host build of the bundled FFmpeg and the ffmpeg CLI — see the module doc"]
fn a_probe_whose_link_serves_an_image_fails_at_once_without_retrying() {
    let _g = tv_base::testlock::serial();
    let Some((_s, file)) = fixture("probe-png") else { return };
    let _ffmpeg = super::test_support::bind_host_ffmpeg();
    let server = Debrid::start(file, |_| Answer::Png);
    let began = Instant::now();
    assert_eq!(
        probe(&server.url()).err(),
        Some(crate::stream::ProbeFailure::NotVideo(crate::stream::NotVideo::Image))
    );
    assert_eq!(server.hits("/start"), 1, "no retry: {:?}", server.seen());
    assert!(began.elapsed() < Duration::from_millis(1500), "no retry wait: {:?}", began.elapsed());
}

/// The C1's case: a debrid link answers junk twice while the service prepares it, then serves
/// the file. The waiting retries reach it.
#[test]
#[ignore = "needs the host build of the bundled FFmpeg and the ffmpeg CLI — see the module doc"]
fn a_probe_whose_link_answers_junk_twice_opens_on_the_waited_retry() {
    let _g = tv_base::testlock::serial();
    let Some((_s, file)) = fixture("probe-twice") else { return };
    let _ffmpeg = super::test_support::bind_host_ffmpeg();
    let server = Debrid::start(file, |n| if n < 2 { Answer::Html } else { Answer::File });
    let probed = probe(&server.url()).expect("the second, waited retry opens the file");
    assert!(probed.video.is_some());
    assert_eq!(server.hits("/start"), 3, "{:?}", server.seen());
}

// -- the demuxer -------------------------------------------------------------------------

/// What one demux run produced.
struct Run {
    /// The pts (ns) of every video AU, in pop order.
    video_pts: Vec<i64>,
    demux_failed: bool,
    chapters: Vec<crate::player::Chapter>,
}

/// Run the real `ff::demux` over `url`. `seeks` are published one at a time through
/// `SHARED.seek_to_ns`, each after a video AU at least 1 s past the previous landing has arrived;
/// the run ends after `tail` AUs at the last landing, or when the demuxer stops.
fn demux_run(url: String, seeks: &[i64], tail: usize) -> Run {
    SHARED.reset_session();
    SHARED.seek_to_ns.store(-1, Ordering::Relaxed);
    SHARED.desired_audio_idx.store(-1, Ordering::Relaxed);
    SHARED.demux_failed.store(false, Ordering::Relaxed);
    SHARED.demux_io_failed.store(false, Ordering::Relaxed);
    FEED_AUDIO.store(true, Ordering::Relaxed);
    let (video_cap, audio_cap) = crate::player::engine::aq_caps();
    let mut video = crate::aq::aq_new(video_cap as c_long);
    let mut audio = crate::aq::aq_new(audio_cap as c_long);
    let mut hs = tv_net::stream::http_stream_boxed();
    let (vp, ap, hp) = (
        SendPtr(&mut *video as *mut AuQueue),
        SendPtr(&mut *audio as *mut AuQueue),
        SendPtr(&mut *hs as *mut HttpStream),
    );
    let thread = std::thread::spawn(move || {
        let off = tv_base::task::OffFrame::for_test();
        demux(&off, url, crate::headers::RequestHeaders::none(), "aac".into(), vp, ap, hp)
    });
    let until = Instant::now() + Duration::from_secs(20);
    let mut video_pts = Vec::new();
    let mut next_seek = 0usize;
    // the target the AUs are expected around, and how many have arrived there
    let mut landing: Option<(i64, usize)> = None;
    let mut first = 0usize;
    loop {
        if Instant::now() > until {
            break;
        }
        let node = crate::aq::aq_pop(&mut *video, std::ptr::null_mut());
        if !node.is_null() {
            let (_, _, pts, _, _) = unsafe { crate::aq::au_fields(node) };
            unsafe { libc::free(node as *mut c_void) };
            video_pts.push(pts);
            match landing {
                None => first += 1,
                Some((t, ref mut n)) if (pts - t).abs() < 1_000_000_000 => *n += 1,
                Some(_) => {}
            }
            let settled = match landing {
                None => first >= tail,
                Some((_, n)) => n >= tail,
            };
            if settled {
                if next_seek == seeks.len() {
                    break;
                }
                SHARED.seek_to_ns.store(seeks[next_seek], Ordering::Release);
                landing = Some((seeks[next_seek], 0));
                next_seek += 1;
            }
            continue;
        }
        let a = crate::aq::aq_pop(&mut *audio, std::ptr::null_mut());
        if !a.is_null() {
            unsafe { libc::free(a as *mut c_void) };
            continue;
        }
        if thread.is_finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    crate::aq::aq_abort(&mut *video);
    crate::aq::aq_abort(&mut *audio);
    tv_net::stream::http_shutdown(&mut *hs);
    let stop_by = Instant::now() + Duration::from_secs(10);
    while !thread.is_finished() && Instant::now() < stop_by {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(thread.is_finished(), "the demuxer did not stop within 10 s of its lanes aborting");
    let _ = thread.join();
    let run = Run {
        video_pts,
        demux_failed: SHARED.demux_failed.load(Ordering::Acquire),
        chapters: crate::player::chapters(),
    };
    tv_net::stream::http_close(&mut *hs);
    crate::aq::aq_destroy(&mut *video);
    crate::aq::aq_destroy(&mut *audio);
    run
}

#[test]
#[ignore = "needs the host build of the bundled FFmpeg and the ffmpeg CLI — see the module doc"]
fn a_demux_whose_first_hop_serves_an_error_page_retries_once_and_plays() {
    let _g = tv_base::testlock::serial();
    let Some((_s, file)) = fixture("demux-html") else { return };
    let _ffmpeg = super::test_support::bind_host_ffmpeg();
    let server = Debrid::start(file, |n| if n == 0 { Answer::Html } else { Answer::File });
    let run = demux_run(server.url(), &[], 25);
    assert!(run.video_pts.len() >= 25, "only {} video AUs ({:?})", run.video_pts.len(), server.seen());
    assert!(!run.demux_failed);
    assert_eq!(server.hits("/start"), 2, "the retry re-resolved the redirect: {:?}", server.seen());
}

#[test]
#[ignore = "needs the host build of the bundled FFmpeg and the ffmpeg CLI — see the module doc"]
fn a_demux_of_a_url_that_never_serves_the_file_fails_after_exactly_one_retry() {
    let _g = tv_base::testlock::serial();
    let Some((_s, file)) = fixture("demux-bad") else { return };
    let _ffmpeg = super::test_support::bind_host_ffmpeg();
    let server = Debrid::start(file, |_| Answer::Html);
    let run = demux_run(server.url(), &[], 25);
    assert!(run.video_pts.is_empty());
    assert!(run.demux_failed, "a stream that never opens is still a failure");
    assert_eq!(server.hits("/start"), 2, "one attempt and one retry: {:?}", server.seen());
}

/// Two in-place seeks fetch the Cues once, and both land; the file's chapters are published at
/// open (start ms on the content timeline, the title tag, empty when the chapter has none).
#[test]
#[ignore = "needs the host build of the bundled FFmpeg and the ffmpeg CLI — see the module doc"]
fn two_in_place_seeks_read_the_cues_once_and_the_chapters_are_published() {
    let _g = tv_base::testlock::serial();
    let Some((_s, file)) = fixture("cues") else { return };
    let cues = cues_offset(&file);
    let _ffmpeg = super::test_support::bind_host_ffmpeg();
    let server = Debrid::start(file, |_| Answer::File);
    let run = demux_run(server.url(), &[6_000_000_000, 3_000_000_000], 10);
    let near = |t: i64| run.video_pts.iter().filter(|&&p| (p - t).abs() < 1_000_000_000).count();
    assert!(near(6_000_000_000) >= 10 && near(3_000_000_000) >= 10, "both seeks landed: {:?}", run.video_pts);
    let cue_reads = server.seen().iter().filter(|(_, r)| *r == Some(cues)).count();
    assert_eq!(cue_reads, 1, "the Cues are read on the first seek only: {:?}", server.seen());
    assert!(SHARED.reopen_ms_est.load(Ordering::Relaxed) > 0, "the demuxer's reopens feed the estimate");
    let chapters: Vec<_> = run.chapters.iter().map(|c| (c.start_ms, c.title.as_str())).collect();
    assert_eq!(chapters, [(0, "Opening"), (3000, "Middle"), (7000, "")]);
}

// -- a live channel's probe ---------------------------------------------------------------

/// A live channel's MPEG-TS carries AAC the bundled FFmpeg has no decoder for, so its stream info
/// search never completes and ran to FFmpeg's 5 MB default `probesize`: 20–30 s of a 1–2 Mbit/s
/// channel read — and on a live window, waited for — before the first picture (TvVoo, 4.5 s
/// probes). An HLS probe reads at most [`super::HLS_PROBE_BYTES`]; the AAC parameters come from
/// the ADTS headers as before.
#[test]
#[ignore = "needs the host build of the bundled FFmpeg and the ffmpeg CLI — see the module doc"]
fn an_hls_probe_of_ts_with_aac_stops_at_its_byte_cap() {
    let Some(_c) = crate::hls::loopback_tests::curl_gate() else { return };
    if !tool_present("ffmpeg") {
        eprintln!("SKIP: no ffmpeg CLI");
        return;
    }
    let _ffmpeg = super::test_support::bind_host_ffmpeg();
    let dir = super::late_audio_tests::Scratch::new("hls-probe");
    // 24 s of incompressible 640x360 video at 3 Mbit/s plus AAC, in 4 s TS segments: ~9 MB
    let status = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y"])
        .args(["-f", "lavfi", "-i", "color=c=gray:s=640x360:r=25:d=24,noise=alls=100:allf=t+u"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=24"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "25", "-b:v", "3M", "-maxrate", "3M", "-bufsize", "3M"])
        .args(["-c:a", "aac", "-b:a", "128k", "-f", "hls", "-hls_time", "4", "-hls_playlist_type", "vod"])
        .arg("-hls_segment_filename")
        .arg(dir.0.join("s%d.ts"))
        .arg(dir.0.join("p.m3u8"))
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg could not write the fixture");
    let o = crate::hls::loopback_tests::Origin::spawn();
    for entry in std::fs::read_dir(&dir.0).expect("fixture dir") {
        let path = entry.expect("entry").path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let body = std::fs::read(&path).expect("read");
        let answer = if name.ends_with(".m3u8") {
            crate::hls::loopback_tests::Answer::playlist(String::from_utf8(body).expect("utf-8"))
        } else {
            crate::hls::loopback_tests::Answer::ok(body)
        };
        o.put(&format!("/{name}"), answer);
    }
    let url = o.url("/p.m3u8");
    super::hls_handoff::clear();
    let probed = probe(&url).expect("probes");
    assert!(probed.video.is_some(), "the picture is found");
    let a = probed.audio.first().expect("the audio track is found");
    assert_eq!((a.sample_rate, a.channels), (48000, 1), "AAC parameters from the ADTS headers");
    let (_, read) = super::hls_handoff::take(&url).expect("the probe's bytes are handed on");
    assert!(
        read.len() as i64 <= super::HLS_PROBE_BYTES + (1 << 20),
        "the probe read {} bytes; its cap is {}",
        read.len(),
        super::HLS_PROBE_BYTES
    );
    super::hls_handoff::clear();
}
