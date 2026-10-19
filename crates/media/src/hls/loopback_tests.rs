//! The HLS client end to end over the REAL media transport: libcurl-multi (`curlio`) against a
//! plaintext loopback HTTP server, on the system clock. What `hls/source_tests.rs` proves on a fake
//! transport is proved here where it can only be proved with sockets: the stream's request headers
//! reach every request (master, media playlist, key, every segment, every reload), redirects are
//! resolved against the URL that answered, an HTTP 404 playlist is the explicit error it should
//! be, and the bytes that leave are exactly the segments' bytes.
//!
//! The demuxer itself cannot run here (the bundled FFmpeg is not staged on the host), so this stops
//! at the byte stream `ff::demux` reads.

use crate::headers::RequestHeaders;
use crate::hls::aes::{cbc_encrypt_pkcs7, sequence_iv};
use crate::hls::source::{CurlTransport, FetchError, HlsError, HlsSource, SystemClock};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

/// One answer: status, extra header lines, body.
#[derive(Clone)]
pub(crate) struct Answer {
    status: u16,
    headers: Vec<String>,
    body: Vec<u8>,
}

impl Answer {
    pub(crate) fn ok(body: impl Into<Vec<u8>>) -> Answer {
        Answer { status: 200, headers: Vec::new(), body: body.into() }
    }
    pub(crate) fn playlist(text: impl Into<String>) -> Answer {
        Answer { status: 200, headers: vec!["Content-Type: application/vnd.apple.mpegurl".into()], body: text.into().into_bytes() }
    }
    pub(crate) fn redirect(to: &str) -> Answer {
        Answer { status: 302, headers: vec![format!("Location: {to}")], body: Vec::new() }
    }
    #[cfg(feature = "hostsim")]
    pub(crate) fn not_found() -> Answer {
        Answer::status(404)
    }
    #[cfg(feature = "hostsim")]
    pub(crate) fn status(status: u16) -> Answer {
        Answer { status, headers: Vec::new(), body: Vec::new() }
    }
}

/// A routed answer may change with each request to its path (a live playlist).
type Routes = Arc<Mutex<HashMap<String, Box<dyn FnMut() -> Answer + Send>>>>;

/// A plaintext loopback server that records every request head it reads.
pub(crate) struct Origin {
    port: u16,
    routes: Routes,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Origin {
    pub(crate) fn spawn() -> Origin {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let routes: Routes = Arc::default();
        let requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let (r, q) = (Arc::clone(&routes), Arc::clone(&requests));
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let (r, q) = (Arc::clone(&r), Arc::clone(&q));
                std::thread::spawn(move || serve(&mut conn, &r, &q));
            }
        });
        Origin { port, routes, requests }
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    pub(crate) fn put(&self, path: &str, answer: Answer) {
        self.routes.lock().unwrap().insert(path.to_owned(), Box::new(move || answer.clone()));
    }

    pub(crate) fn route(&self, path: &str, f: impl FnMut() -> Answer + Send + 'static) {
        self.routes.lock().unwrap().insert(path.to_owned(), Box::new(f));
    }

    pub(crate) fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    pub(crate) fn paths(&self) -> Vec<String> {
        self.requests().iter().map(|r| r.split_whitespace().nth(1).unwrap_or("").to_owned()).collect()
    }
}

fn serve(conn: &mut std::net::TcpStream, routes: &Routes, requests: &Mutex<Vec<String>>) {
    let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    loop {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match conn.read(&mut byte) {
                Ok(1) => head.push(byte[0]),
                _ => return,
            }
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        requests.lock().unwrap().push(head.clone());
        let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
        let answer = match routes.lock().unwrap().get_mut(&path) {
            Some(f) => f(),
            None => Answer { status: 404, headers: Vec::new(), body: b"not found".to_vec() },
        };
        let start = header(&head, "range")
            .and_then(|v| v.strip_prefix("bytes=").map(str::to_owned))
            .and_then(|v| v.split('-').next().and_then(|n| n.parse::<usize>().ok()));
        let mut out = match (answer.status, start) {
            (200, Some(at)) if at < answer.body.len() => {
                let tail = &answer.body[at..];
                let mut h = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {at}-{}/{}\r\n",
                    tail.len(),
                    answer.body.len() - 1,
                    answer.body.len()
                );
                for l in &answer.headers {
                    h.push_str(l);
                    h.push_str("\r\n");
                }
                h.push_str("\r\n");
                [h.into_bytes(), tail.to_vec()].concat()
            }
            (status, _) => {
                let mut h = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\n", answer.body.len());
                for l in &answer.headers {
                    h.push_str(l);
                    h.push_str("\r\n");
                }
                h.push_str("\r\n");
                [h.into_bytes(), answer.body.clone()].concat()
            }
        };
        if conn.write_all(&out).is_err() {
            return;
        }
        out.clear();
    }
}

fn header(request: &str, name: &str) -> Option<String> {
    request.lines().find_map(|l| {
        let (n, v) = l.split_once(':')?;
        n.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
    })
}

/// The curl gate every test here opens with: net's easy table up and the multi table bound, under
/// the crate-wide serial lock (curlio's registry holds ONE live transfer). `None` on a host
/// without libcurl, where the test is vacuous.
pub(crate) fn curl_gate() -> Option<tv_base::testlock::Serial> {
    let g = tv_base::testlock::serial();
    (tv_net::net::curl_ready() && crate::curlio::available()).then_some(g)
}

fn addon_headers() -> RequestHeaders {
    RequestHeaders::from_pairs(&[("User-Agent", "LiveTvClient/2.0 (test)"), ("Referer", "https://addon.example/")]).unwrap()
}

fn open(url: &str) -> Result<HlsSource<CurlTransport, SystemClock>, HlsError> {
    let t = CurlTransport::new(addon_headers(), Box::new(|| false));
    HlsSource::open(t, SystemClock::new(), url, None, &tv_platform::devcaps::Caps::assumed())
}

fn read_all(src: &mut HlsSource<CurlTransport, SystemClock>) -> Result<Vec<u8>, HlsError> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
    }
}

/// A fake TS segment of 188-byte packets, distinct per `n`.
pub(crate) fn ts(n: u8) -> Vec<u8> {
    (0..40).flat_map(|i: u32| {
        let mut p = vec![0x47u8, n, i as u8];
        p.resize(188, n ^ (i as u8));
        p
    }).collect()
}

fn assert_headers_everywhere(o: &Origin) {
    let reqs = o.requests();
    assert!(!reqs.is_empty());
    for r in &reqs {
        let line = r.lines().next().unwrap_or_default();
        assert_eq!(header(r, "user-agent").as_deref(), Some("LiveTvClient/2.0 (test)"), "{line}");
        assert_eq!(header(r, "referer").as_deref(), Some("https://addon.example/"), "{line}");
    }
}

#[test]
fn a_master_media_and_encrypted_ts_segments_arrive_as_one_stream_with_the_headers_on_every_request() {
    let Some(_g) = curl_gate() else { return };
    let o = Origin::spawn();
    let key: [u8; 16] = *b"loopback-key-016";
    // The master redirects to its real location; relative URIs resolve against THAT.
    o.put("/channel.m3u8", Answer::redirect("/cdn/master.m3u8"));
    o.put(
        "/cdn/master.m3u8",
        Answer::playlist(
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=600000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\"\nlo/index.m3u8\n\
             #EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720,CODECS=\"avc1.64001f,mp4a.40.2\"\nhi/index.m3u8\n",
        ),
    );
    o.put(
        "/cdn/hi/index.m3u8",
        Answer::playlist(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:20\n\
             #EXTINF:4.0,\nseg20.ts\n#EXT-X-KEY:METHOD=AES-128,URI=\"../keys/k1\"\n#EXTINF:4.0,\nseg21.ts\n#EXTINF:3.5,\nseg22.ts\n#EXT-X-ENDLIST\n",
        ),
    );
    o.put("/cdn/keys/k1", Answer::ok(key.to_vec()));
    o.put("/cdn/hi/seg20.ts", Answer::ok(ts(20)));
    o.put("/cdn/hi/seg21.ts", Answer::ok(cbc_encrypt_pkcs7(&key, &sequence_iv(21), &ts(21))));
    o.put("/cdn/hi/seg22.ts", Answer::ok(cbc_encrypt_pkcs7(&key, &sequence_iv(22), &ts(22))));

    let mut src = open(&o.url("/channel.m3u8")).expect("opens");
    assert!(!src.is_live());
    assert_eq!(src.duration_us(), 11_500_000);
    assert_eq!(src.variant().map(|v| v.bandwidth), Some(3_000_000));
    let got = read_all(&mut src).expect("reads to the end");
    assert_eq!(got, [ts(20), ts(21), ts(22)].concat(), "exactly the segments' plaintext, in order");
    assert_eq!(
        o.paths(),
        ["/channel.m3u8", "/cdn/master.m3u8", "/cdn/hi/index.m3u8", "/cdn/hi/seg20.ts", "/cdn/keys/k1", "/cdn/hi/seg21.ts", "/cdn/hi/seg22.ts"]
    );
    assert_headers_everywhere(&o);
}

#[test]
fn a_live_playlist_is_reloaded_with_the_headers_until_it_ends() {
    let Some(_g) = curl_gate() else { return };
    let o = Origin::spawn();
    let mut loads = 0u32;
    // One-second targets so the reload waits stay short. Load 1 lists 0..=2, load 2 adds 3, load 3
    // adds 4 and ends the stream.
    o.route("/live/index.m3u8", move || {
        loads += 1;
        let newest = (loads + 1).min(4);
        let mut p = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:0\n");
        for q in 0..=newest {
            p.push_str(&format!("#EXTINF:1.0,\ns{q}.ts\n"));
        }
        if newest == 4 {
            p.push_str("#EXT-X-ENDLIST\n");
        }
        Answer::playlist(p)
    });
    for q in 0..=4u8 {
        o.put(&format!("/live/s{q}.ts"), Answer::ok(ts(q)));
    }
    let began = std::time::Instant::now();
    let mut src = open(&o.url("/live/index.m3u8")).expect("opens");
    assert!(src.is_live());
    let got = read_all(&mut src).expect("reads to the end the playlist announced");
    // Three one-second targets behind the edge of 0..=2 is segment 0.
    assert_eq!(got, (0..=4).flat_map(ts).collect::<Vec<u8>>());
    let reloads = o.paths().iter().filter(|p| *p == "/live/index.m3u8").count();
    assert_eq!(reloads, 3);
    assert!(began.elapsed() >= std::time::Duration::from_millis(1900), "two reloads, each a target after the last");
    assert_headers_everywhere(&o);
}

#[test]
fn a_404_playlist_is_an_explicit_error_and_a_404_segment_of_a_vod_stream_too() {
    let Some(_g) = curl_gate() else { return };
    let o = Origin::spawn();
    let err = open(&o.url("/gone.m3u8")).err().expect("refused");
    assert_eq!(err, HlsError::Playlist(FetchError::Status(404)));
    assert_eq!(err.http_status(), Some(404));
    assert_eq!(o.paths(), ["/gone.m3u8"], "a 4xx playlist is not retried");

    o.put("/v.m3u8", Answer::playlist("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\na.ts\n#EXTINF:2,\nmissing.ts\n#EXT-X-ENDLIST\n"));
    o.put("/a.ts", Answer::ok(ts(1)));
    let mut src = open(&o.url("/v.m3u8")).expect("opens");
    let mut buf = vec![0u8; 1 << 16];
    let mut got = Vec::new();
    let err = loop {
        match src.read(&mut buf) {
            Ok(n) => {
                assert!(n > 0, "a VOD stream with a missing segment does not end cleanly");
                got.extend_from_slice(&buf[..n]);
            }
            Err(e) => break e,
        }
    };
    assert_eq!(got, ts(1));
    assert_eq!(err, HlsError::Segment { sequence: 1, error: FetchError::Status(404) });
}

#[test]
fn the_media_plane_reports_a_playlists_content_type_and_where_it_was_answered_from() {
    let Some(_g) = curl_gate() else { return };
    let o = Origin::spawn();
    o.put("/play", Answer::redirect("/real/list"));
    o.put("/real/list", Answer::playlist("#EXTM3U\n"));
    let cs = crate::curlio::CurlSource::open_with_headers(&o.url("/play"), 0, &addon_headers()).expect("opens");
    assert_eq!(cs.content_type(), Some("application/vnd.apple.mpegurl"));
    assert!(crate::hls::is_playlist_content_type(cs.content_type().unwrap()));
    assert_eq!(cs.effective_url(), o.url("/real/list"));
}

/// An archive request outside the recording is redirected to a short VOD "no archive" stub with
/// a 200: the client names it by where the redirect went, before reading a segment of it.
#[test]
fn a_redirect_to_the_no_archive_stub_is_an_explicit_error() {
    let Some(_g) = curl_gate() else { return };
    let o = Origin::spawn();
    o.put("/ch1/mono-1760000000-3600.m3u8", Answer::redirect("/new_stub/noarchive.m3u8"));
    o.put("/new_stub/noarchive.m3u8", Answer::playlist("#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-TARGETDURATION:10\n#EXTINF:10,\nstub0.ts\n#EXT-X-ENDLIST\n"));
    assert_eq!(open(&o.url("/ch1/mono-1760000000-3600.m3u8")).err(), Some(HlsError::NoArchive));
    assert!(!o.paths().iter().any(|p| p.ends_with(".ts")), "no stub segment was fetched");
}

/// A playlist's URIs each start a request chain of their own, so the credential headers
/// (Cookie, Authorization) go only to URIs on the stream's own origin; a segment on another host
/// gets the User-Agent and the Referer and nothing else.
#[test]
fn credential_headers_stay_on_the_streams_origin() {
    let Some(_g) = curl_gate() else { return };
    let (home, cdn) = (Origin::spawn(), Origin::spawn());
    let headers = RequestHeaders::from_pairs(&[("User-Agent", "LiveTvClient/2.0 (test)"), ("Referer", "https://addon.example/"), ("Cookie", "session=s3cr3t")]).unwrap();
    home.put(
        "/live.m3u8",
        Answer::playlist(format!("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\na.ts\n#EXTINF:2,\n{}\n#EXT-X-ENDLIST\n", cdn.url("/b.ts"))),
    );
    home.put("/a.ts", Answer::ok(ts(1)));
    cdn.put("/b.ts", Answer::ok(ts(2)));
    let t = CurlTransport::new(headers, Box::new(|| false));
    let mut src = HlsSource::open(t, SystemClock::new(), &home.url("/live.m3u8"), None, &tv_platform::devcaps::Caps::assumed()).expect("opens");
    assert_eq!(read_all(&mut src).unwrap(), [ts(1), ts(2)].concat());
    for r in home.requests() {
        assert_eq!(header(&r, "cookie").as_deref(), Some("session=s3cr3t"), "the stream's own origin: {}", r.lines().next().unwrap_or(""));
    }
    let cdn_requests = cdn.requests();
    assert_eq!(cdn_requests.len(), 1);
    for r in &cdn_requests {
        assert_eq!(header(r, "cookie"), None, "no credential to another origin");
        assert_eq!(header(r, "user-agent").as_deref(), Some("LiveTvClient/2.0 (test)"));
        assert_eq!(header(r, "referer").as_deref(), Some("https://addon.example/"));
    }
}
