//! The HLS client against an in-memory server on a fake clock: what it requests, in what order and
//! when, and what bytes it delivers — the whole state machine, with no socket and no sleeping.
//! `hls/loopback_tests.rs` drives the real curl transport.

use super::*;
use crate::hls::aes::cbc_encrypt_pkcs7;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// A clock whose sleeps advance it instantly.
#[derive(Clone, Default)]
struct FakeClock(Rc<Cell<u64>>);

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
    fn sleep_ms(&self, ms: u64) {
        self.0.set(self.0.get() + ms.max(1));
    }
}

/// One canned answer: a body, or a failure, given the time of the request.
type Route = Box<dyn FnMut(u64) -> Result<Vec<u8>, FetchError>>;

/// What a test server saw: `(time, url, offset)` per request.
#[derive(Clone, Default)]
struct Seen(Rc<RefCell<Vec<(u64, String, u64)>>>);

impl Seen {
    fn urls(&self) -> Vec<String> {
        self.0.borrow().iter().map(|(_, u, _)| u.clone()).collect()
    }
    fn times_of(&self, url: &str) -> Vec<u64> {
        self.0.borrow().iter().filter(|(_, u, _)| u == url).map(|(t, _, _)| *t).collect()
    }
    fn count(&self, url: &str) -> usize {
        self.0.borrow().iter().filter(|(_, u, _)| u == url).count()
    }
}

struct FakeTransport {
    routes: Rc<RefCell<HashMap<String, Route>>>,
    clock: FakeClock,
    seen: Seen,
    cancel: Rc<Cell<bool>>,
    /// Bodies are served in pieces of this size, so the decryptor and the byte accounting see
    /// arbitrary chunking.
    chunk: usize,
    /// A request for a URL with an entry here fails mid-body after that many bytes, once each.
    cut_after: Rc<RefCell<HashMap<String, usize>>>,
}

struct FakeBody {
    data: Vec<u8>,
    pos: usize,
    chunk: usize,
    url: String,
    cut_at: Option<usize>,
}

impl Body for FakeBody {
    fn read(&mut self, dst: &mut [u8]) -> Result<usize, FetchError> {
        if self.cut_at.is_some_and(|c| self.pos >= c) {
            return Err(FetchError::Transport(18));
        }
        let mut n = dst.len().min(self.chunk).min(self.data.len() - self.pos);
        if let Some(c) = self.cut_at {
            n = n.min(c - self.pos);
        }
        dst[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
    fn effective_url(&self) -> String {
        self.url.clone()
    }
}

impl Transport for FakeTransport {
    fn get(&mut self, url: &str, offset: u64, _purpose: Purpose) -> Result<Box<dyn Body>, FetchError> {
        let now = self.clock.now_ms();
        self.seen.0.borrow_mut().push((now, url.to_owned(), offset));
        let mut routes = self.routes.borrow_mut();
        let route = routes.get_mut(url).ok_or(FetchError::Status(404))?;
        let data = route(now)?;
        let data = data.get(offset as usize..).ok_or(FetchError::Status(416))?.to_vec();
        let cut_at = self.cut_after.borrow_mut().remove(url);
        Ok(Box::new(FakeBody { data, pos: 0, chunk: self.chunk, url: url.to_owned(), cut_at }))
    }
    fn cancelled(&self) -> bool {
        self.cancel.get()
    }
}

/// A test server: routes, a clock, and what was requested.
struct Server {
    routes: Rc<RefCell<HashMap<String, Route>>>,
    clock: FakeClock,
    seen: Seen,
    cancel: Rc<Cell<bool>>,
    cut_after: Rc<RefCell<HashMap<String, usize>>>,
}

const HOST: &str = "https://tv.example:443";

fn url(path: &str) -> String {
    format!("{HOST}{path}")
}

impl Server {
    fn new() -> Server {
        Server {
            routes: Rc::default(),
            clock: FakeClock::default(),
            seen: Seen::default(),
            cancel: Rc::default(),
            cut_after: Rc::default(),
        }
    }

    fn put(&self, path: &str, body: impl Into<Vec<u8>>) {
        let body = body.into();
        self.routes.borrow_mut().insert(url(path), Box::new(move |_| Ok(body.clone())));
    }

    fn route(&self, path: &str, f: impl FnMut(u64) -> Result<Vec<u8>, FetchError> + 'static) {
        self.routes.borrow_mut().insert(url(path), Box::new(f));
    }

    fn transport(&self, chunk: usize) -> FakeTransport {
        FakeTransport {
            routes: Rc::clone(&self.routes),
            clock: self.clock.clone(),
            seen: self.seen.clone(),
            cancel: Rc::clone(&self.cancel),
            chunk,
            cut_after: Rc::clone(&self.cut_after),
        }
    }

    fn open(&self, path: &str, start_us: Option<u64>) -> Result<HlsSource<FakeTransport, FakeClock>, HlsError> {
        HlsSource::open(self.transport(1000), self.clock.clone(), &url(path), start_us, &tv_platform::devcaps::Caps::assumed())
    }
}

/// A fake MPEG-TS segment: recognisable bytes, not a multiple of 16 or of 188.
fn ts(n: u64) -> Vec<u8> {
    (0..1500 + n as usize * 37).map(|i| (i as u64 * 31 + n * 7) as u8).collect()
}

fn read_all<T: Transport, C: Clock>(src: &mut HlsSource<T, C>) -> Result<Vec<u8>, HlsError> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 777];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
    }
}

fn vod_playlist(seq0: u64, n: u64, extra: &str) -> String {
    let mut p = format!("#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:{seq0}\n#EXT-X-PLAYLIST-TYPE:VOD\n{extra}");
    for s in seq0..seq0 + n {
        p.push_str(&format!("#EXTINF:6.0,\nseg{s}.ts\n"));
    }
    p.push_str("#EXT-X-ENDLIST\n");
    p
}

#[test]
fn a_master_resolves_to_the_best_variant_and_the_segments_concatenate() {
    let s = Server::new();
    s.put(
        "/master.m3u8",
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\"\nlow/index.m3u8\n\
         #EXT-X-STREAM-INF:BANDWIDTH=4000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\"\nhd/index.m3u8\n\
         #EXT-X-STREAM-INF:BANDWIDTH=9000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,fLaC\"\nflac/index.m3u8\n",
    );
    s.put("/hd/index.m3u8", vod_playlist(0, 3, ""));
    for n in 0..3 {
        s.put(&format!("/hd/seg{n}.ts"), ts(n));
    }
    let mut src = s.open("/master.m3u8", None).unwrap();
    assert!(!src.is_live());
    assert_eq!(src.duration_us(), 18_000_000);
    assert_eq!(src.container(), Container::MpegTs);
    assert_eq!(src.variant().map(|v| (v.index, v.of, v.bandwidth)), Some((1, 3, 4_000_000)), "FLAC is not playable");
    let got = read_all(&mut src).unwrap();
    assert_eq!(got, [ts(0), ts(1), ts(2)].concat());
    assert_eq!(
        s.seen.urls(),
        [url("/master.m3u8"), url("/hd/index.m3u8"), url("/hd/seg0.ts"), url("/hd/seg1.ts"), url("/hd/seg2.ts")]
    );
    assert_eq!(src.read(&mut [0; 8]), Ok(0), "the end stays the end");
}

#[test]
fn a_vod_start_position_begins_at_the_segment_that_holds_it() {
    let s = Server::new();
    s.put("/v.m3u8", vod_playlist(0, 5, ""));
    for n in 0..5 {
        s.put(&format!("/seg{n}.ts"), ts(n));
    }
    let mut src = s.open("/v.m3u8", Some(13_500_000)).unwrap();
    assert_eq!(src.start_us(), 12_000_000);
    assert_eq!(read_all(&mut src).unwrap(), [ts(2), ts(3), ts(4)].concat());
}

#[test]
fn a_404_playlist_and_a_playlist_that_is_not_one_are_explicit_errors() {
    let s = Server::new();
    assert!(matches!(s.open("/missing.m3u8", None), Err(HlsError::Playlist(FetchError::Status(404)))));
    assert_eq!(s.seen.count(&url("/missing.m3u8")), 1, "a 4xx is not retried");
    assert_eq!(HlsError::Playlist(FetchError::Status(404)).http_status(), Some(404));
    s.put("/html.m3u8", "<html>blocked</html>");
    assert!(matches!(s.open("/html.m3u8", None), Err(HlsError::Parse(ParseError::NotAPlaylist))));
    s.put("/master.m3u8", "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,CODECS=\"vp09.00.10.08\"\nv.m3u8\n");
    assert!(matches!(s.open("/master.m3u8", None), Err(HlsError::NoPlayableVariant(_))));
    s.put("/empty.m3u8", "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-ENDLIST\n");
    assert_eq!(s.open("/empty.m3u8", None).err(), Some(HlsError::Empty));
    s.put("/nested.m3u8", "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nnested.m3u8\n");
    assert_eq!(s.open("/nested.m3u8", None).err(), Some(HlsError::NestedMaster));
}

#[test]
fn a_transient_first_load_is_retried_once() {
    let s = Server::new();
    let mut calls = 0;
    s.route("/flaky.m3u8", move |_| {
        calls += 1;
        if calls == 1 {
            Err(FetchError::Status(503))
        } else {
            Ok(vod_playlist(0, 1, "").into_bytes())
        }
    });
    s.put("/seg0.ts", ts(0));
    let mut src = s.open("/flaky.m3u8", None).unwrap();
    assert_eq!(read_all(&mut src).unwrap(), ts(0));
    let s = Server::new();
    s.route("/down.m3u8", |_| Err(FetchError::Timeout));
    assert_eq!(s.open("/down.m3u8", None).err(), Some(HlsError::Playlist(FetchError::Timeout)));
    assert_eq!(s.seen.count(&url("/down.m3u8")), 2, "one retry, then the error");
}

/// A live window of `window` segments of `seg_s` seconds that gains one segment every `seg_s`
/// seconds of the fake clock, from sequence `seq0` at time 0; ends (ENDLIST) once sequence
/// `end_seq` would be listed, when given.
fn live_route(seq0: u64, window: u64, seg_s: u64, end_seq: Option<u64>) -> impl FnMut(u64) -> Result<Vec<u8>, FetchError> {
    move |now_ms| {
        let newest = seq0 + window - 1 + now_ms / (seg_s * 1000);
        let (newest, ended) = match end_seq {
            Some(e) if newest >= e => (e, true),
            _ => (newest, false),
        };
        let first = newest + 1 - window;
        let mut p = format!("#EXTM3U\n#EXT-X-TARGETDURATION:{seg_s}\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
        for q in first..=newest {
            p.push_str(&format!("#EXTINF:{seg_s}.0,\nseg{q}.ts\n"));
        }
        if ended {
            p.push_str("#EXT-X-ENDLIST\n");
        }
        Ok(p.into_bytes())
    }
}

#[test]
fn live_starts_three_targets_behind_and_follows_the_window_at_the_rfc_cadence() {
    let s = Server::new();
    s.route("/live.m3u8", live_route(100, 6, 6, Some(112)));
    for q in 100..=112 {
        s.put(&format!("/seg{q}.ts"), ts(q));
    }
    let mut src = s.open("/live.m3u8", None).unwrap();
    assert!(src.is_live());
    assert_eq!(src.duration_us(), 0);
    let got = read_all(&mut src).unwrap();
    // The window at t=0 is 100..=105; three 6 s targets behind the edge is 103.
    let want: Vec<u8> = (103..=112).flat_map(ts).collect();
    assert_eq!(got, want, "every segment from the start to the end, once, in order");
    // Reloads: the first load at 0; a reload is due one target after a load that changed the
    // playlist, and half a target after one that did not.
    let reloads = s.seen.times_of(&url("/live.m3u8"));
    assert_eq!(reloads[0], 0);
    for w in reloads.windows(2) {
        let gap = w[1] - w[0];
        assert!(gap == 3_000 || gap == 6_000, "reload gap {gap} ms in {reloads:?}");
    }
    assert!(reloads.len() >= 7, "{reloads:?}");
}

#[test]
fn a_live_client_that_fell_behind_jumps_to_the_oldest_listed_segment() {
    let s = Server::new();
    // A 3-segment window of 2 s segments; the playlist is served as of the fake time.
    s.route("/live.m3u8", live_route(10, 3, 2, Some(30)));
    for q in 10..=30 {
        s.put(&format!("/seg{q}.ts"), ts(q));
    }
    // Every segment request costs 5 s of fake time, so the client falls behind a 2 s cadence.
    let clock = s.clock.clone();
    for q in 10..=30 {
        let body = ts(q);
        let c = clock.clone();
        s.route(&format!("/seg{q}.ts"), move |_| {
            c.sleep_ms(5_000);
            Ok(body.clone())
        });
    }
    let mut src = s.open("/live.m3u8", None).unwrap();
    let got = read_all(&mut src).unwrap();
    let fetched: Vec<u64> = s
        .seen
        .urls()
        .iter()
        .filter_map(|u| u.strip_prefix(&url("/seg"))?.strip_suffix(".ts")?.parse().ok())
        .collect();
    assert!(fetched.windows(2).all(|w| w[1] > w[0]), "only ever forward: {fetched:?}");
    assert!(fetched.windows(2).any(|w| w[1] > w[0] + 1), "some segments expired unplayed: {fetched:?}");
    let want: Vec<u8> = fetched.iter().flat_map(|&q| ts(q)).collect();
    assert_eq!(got, want);
}

#[test]
fn a_live_playlist_that_stops_growing_is_an_explicit_failure() {
    let s = Server::new();
    s.put("/stuck.m3u8", "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:7\n#EXTINF:4,\nseg7.ts\n#EXTINF:4,\nseg8.ts\n");
    s.put("/seg7.ts", ts(7));
    s.put("/seg8.ts", ts(8));
    let mut src = s.open("/stuck.m3u8", None).unwrap();
    let mut buf = vec![0u8; 1 << 16];
    let mut got = Vec::new();
    let err = loop {
        match src.read(&mut buf) {
            Ok(n) => {
                assert!(n > 0, "a live stream does not end without ENDLIST");
                got.extend_from_slice(&buf[..n]);
            }
            Err(e) => break e,
        }
    };
    assert_eq!(got, [ts(7), ts(8)].concat());
    assert!(matches!(err, HlsError::LiveStalled { since_ms } if since_ms >= 28_000), "{err:?}");
    assert!(s.clock.now_ms() >= 28_000 && s.clock.now_ms() < 40_000, "declared at 7 targets, not much later: {}", s.clock.now_ms());
    assert_eq!(src.read(&mut buf), Err(err), "the failure is sticky");
}

#[test]
fn a_live_reload_that_keeps_failing_ends_the_stream_and_one_that_recovers_does_not() {
    let s = Server::new();
    let mut calls = 0;
    s.route("/live.m3u8", move |_| {
        calls += 1;
        match calls {
            1 => Ok(b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2,\nseg0.ts\n".to_vec()),
            2 | 3 => Err(FetchError::Status(502)),
            4 => Ok(b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2,\nseg0.ts\n#EXTINF:2,\nseg1.ts\n".to_vec()),
            _ => Err(FetchError::Status(404)),
        }
    });
    s.put("/seg0.ts", ts(0));
    s.put("/seg1.ts", ts(1));
    let mut src = s.open("/live.m3u8", None).unwrap();
    let mut buf = vec![0u8; 1 << 16];
    let mut got = Vec::new();
    let err = loop {
        match src.read(&mut buf) {
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) => break e,
        }
    };
    assert_eq!(got, [ts(0), ts(1)].concat(), "two failed reloads were survived");
    assert_eq!(err, HlsError::Playlist(FetchError::Status(404)));
    assert_eq!(s.seen.count(&url("/live.m3u8")), 4 + MAX_RELOAD_FAILURES as usize);
}

#[test]
fn aes_128_segments_decrypt_with_the_tag_iv_and_the_sequence_iv_and_the_key_is_fetched_once() {
    let s = Server::new();
    let key: Block = *b"0123456789abcdef";
    let iv: Block = [9; 16];
    s.put("/k.bin", key.to_vec());
    s.put(
        "/enc.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:41\n\
         #EXT-X-KEY:METHOD=AES-128,URI=\"k.bin\",IV=0x09090909090909090909090909090909\n#EXTINF:6,\nseg41.ts\n\
         #EXT-X-KEY:METHOD=AES-128,URI=\"k.bin\"\n#EXTINF:6,\nseg42.ts\n#EXTINF:6,\nseg43.ts\n\
         #EXT-X-KEY:METHOD=NONE\n#EXTINF:6,\nseg44.ts\n#EXT-X-ENDLIST\n",
    );
    s.put("/seg41.ts", cbc_encrypt_pkcs7(&key, &iv, &ts(41)));
    s.put("/seg42.ts", cbc_encrypt_pkcs7(&key, &sequence_iv(42), &ts(42)));
    s.put("/seg43.ts", cbc_encrypt_pkcs7(&key, &sequence_iv(43), &ts(43)));
    s.put("/seg44.ts", ts(44));
    let mut src = s.open("/enc.m3u8", None).unwrap();
    assert_eq!(read_all(&mut src).unwrap(), [ts(41), ts(42), ts(43), ts(44)].concat());
    assert_eq!(s.seen.count(&url("/k.bin")), 1, "one key, fetched once");
}

#[test]
fn key_failures_and_unsupported_encryption_are_explicit() {
    let s = Server::new();
    s.put("/a.m3u8", vod_playlist(0, 1, "#EXT-X-KEY:METHOD=AES-128,URI=\"nokey\"\n"));
    s.put("/seg0.ts", ts(0));
    let mut src = s.open("/a.m3u8", None).unwrap();
    assert_eq!(read_all(&mut src), Err(HlsError::Key(FetchError::Status(404))));
    s.put("/b.m3u8", vod_playlist(0, 1, "#EXT-X-KEY:METHOD=AES-128,URI=\"short\"\n"));
    s.put("/short", vec![1u8; 15]);
    assert_eq!(read_all(&mut s.open("/b.m3u8", None).unwrap()), Err(HlsError::KeyLength(15)));
    s.put("/c.m3u8", vod_playlist(0, 1, "#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"k\"\n"));
    assert_eq!(read_all(&mut s.open("/c.m3u8", None).unwrap()), Err(HlsError::Unsupported("METHOD=SAMPLE-AES".into())));
    // The wrong key: the padding check catches it.
    s.put("/d.m3u8", vod_playlist(0, 1, "#EXT-X-KEY:METHOD=AES-128,URI=\"wrong\",IV=0x01\n"));
    s.put("/wrong", vec![7u8; 16]);
    let mut iv = [0u8; 16];
    iv[15] = 1;
    s.put("/seg0.ts", cbc_encrypt_pkcs7(b"0123456789abcdef", &iv, &ts(0)));
    assert!(matches!(read_all(&mut s.open("/d.m3u8", None).unwrap()), Err(HlsError::Decrypt(_))));
}

#[test]
fn fmp4_puts_the_init_section_first_and_again_when_it_changes() {
    let s = Server::new();
    s.put(
        "/f.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MAP:URI=\"init.mp4\"\n\
         #EXTINF:4,\na.m4s\n#EXTINF:4,\nb.m4s\n#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"init2.mp4\"\n#EXTINF:4,\nc.m4s\n#EXT-X-ENDLIST\n",
    );
    s.put("/init.mp4", b"ftypmoov-1".to_vec());
    s.put("/init2.mp4", b"ftypmoov-2".to_vec());
    s.put("/a.m4s", b"moofmdat-a".to_vec());
    s.put("/b.m4s", b"moofmdat-b".to_vec());
    s.put("/c.m4s", b"moofmdat-c".to_vec());
    let mut src = s.open("/f.m3u8", None).unwrap();
    assert_eq!(src.container(), Container::Fmp4);
    assert_eq!(read_all(&mut src).unwrap(), b"ftypmoov-1moofmdat-amoofmdat-bftypmoov-2moofmdat-c".to_vec());
    assert_eq!(s.seen.count(&url("/init.mp4")), 1);
}

#[test]
fn an_encrypted_init_section_and_byte_ranges_of_one_resource() {
    let s = Server::new();
    let key: Block = *b"fedcba9876543210";
    let iv: Block = [3; 16];
    let init = cbc_encrypt_pkcs7(&key, &iv, b"INIT-SECTION");
    // One resource: [encrypted init][seg A][seg B]
    let (a, b) = (b"segment-A-bytes".to_vec(), b"segment-B".to_vec());
    let file = [init.clone(), a.clone(), b.clone()].concat();
    s.put("/k", key.to_vec());
    s.put("/main.mp4", file);
    let (il, al, bl) = (init.len(), a.len(), b.len());
    s.put(
        "/r.m3u8",
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\",IV=0x03030303030303030303030303030303\n\
             #EXT-X-MAP:URI=\"main.mp4\",BYTERANGE=\"{il}@0\"\n#EXT-X-KEY:METHOD=NONE\n\
             #EXTINF:4,\n#EXT-X-BYTERANGE:{al}@{il}\nmain.mp4\n#EXTINF:4,\n#EXT-X-BYTERANGE:{bl}\nmain.mp4\n#EXT-X-ENDLIST\n"
        ),
    );
    let mut src = s.open("/r.m3u8", None).unwrap();
    assert_eq!(read_all(&mut src).unwrap(), [b"INIT-SECTION".to_vec(), a, b].concat());
    let offsets: Vec<u64> = s.seen.0.borrow().iter().filter(|(_, u, _)| u.ends_with("/main.mp4")).map(|(_, _, o)| *o).collect();
    assert_eq!(offsets, [0, il as u64, (il + al) as u64]);
}

#[test]
fn a_segment_cut_mid_body_is_refetched_without_repeating_a_byte() {
    for encrypted in [false, true] {
        let s = Server::new();
        let key: Block = *b"0123456789abcdef";
        let extra = if encrypted { "#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n" } else { "" };
        s.put("/k", key.to_vec());
        s.put("/v.m3u8", vod_playlist(0, 3, extra));
        for n in 0..3 {
            let body = if encrypted { cbc_encrypt_pkcs7(&key, &sequence_iv(n), &ts(n)) } else { ts(n) };
            s.put(&format!("/seg{n}.ts"), body);
        }
        s.cut_after.borrow_mut().insert(url("/seg1.ts"), 1111);
        let mut src = s.open("/v.m3u8", None).unwrap();
        assert_eq!(read_all(&mut src).unwrap(), [ts(0), ts(1), ts(2)].concat(), "encrypted={encrypted}");
        assert_eq!(s.seen.count(&url("/seg1.ts")), 2);
    }
}

#[test]
fn a_vod_segment_that_keeps_failing_is_an_error_and_a_live_one_is_skipped() {
    let s = Server::new();
    s.put("/v.m3u8", vod_playlist(0, 3, ""));
    s.put("/seg0.ts", ts(0));
    s.route("/seg1.ts", |_| Err(FetchError::Status(503)));
    s.put("/seg2.ts", ts(2));
    let mut src = s.open("/v.m3u8", None).unwrap();
    let err = read_all(&mut src).unwrap_err();
    assert_eq!(err, HlsError::Segment { sequence: 1, error: FetchError::Status(503) });
    assert_eq!(err.http_status(), Some(503));
    assert_eq!(s.seen.count(&url("/seg1.ts")), ATTEMPTS as usize, "bounded retries");
    assert_eq!(src.delivered(), ts(0).len() as u64);

    let s = Server::new();
    for n in [0, 2, 3] {
        s.put(&format!("/seg{n}.ts"), ts(n));
    }
    s.put(
        "/live2.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2,\nseg0.ts\n#EXTINF:2,\nseg1.ts\n#EXTINF:2,\nseg2.ts\n#EXTINF:2,\nseg3.ts\n",
    );
    let mut src = s.open("/live2.m3u8", None).unwrap();
    let mut buf = vec![0u8; 1 << 16];
    let mut got = Vec::new();
    while got.len() < ts(2).len() + ts(3).len() {
        let n = src.read(&mut buf).unwrap();
        got.extend_from_slice(&buf[..n]);
    }
    // Live starts three targets back: seg1, which is gone (404) and skipped.
    assert_eq!(got, [ts(2), ts(3)].concat());
    assert_eq!(s.seen.count(&url("/seg1.ts")), 1, "a 4xx is not retried");
}

#[test]
fn teardown_ends_a_wait_and_every_later_read() {
    let s = Server::new();
    s.route("/live.m3u8", live_route(0, 3, 6, None));
    for q in 0..100 {
        s.put(&format!("/seg{q}.ts"), ts(q));
    }
    let mut src = s.open("/live.m3u8", None).unwrap();
    let mut buf = vec![0u8; 1 << 16];
    while src.delivered() < (ts(0).len() * 3) as u64 {
        src.read(&mut buf).unwrap();
    }
    s.cancel.set(true);
    assert_eq!(src.read(&mut buf), Err(HlsError::Aborted));
    assert_eq!(src.read(&mut buf), Err(HlsError::Aborted));
    let before = s.seen.0.borrow().len();
    let _ = src.read(&mut buf);
    assert_eq!(s.seen.0.borrow().len(), before, "nothing is requested after teardown");
}

#[test]
fn gap_segments_are_not_fetched() {
    let s = Server::new();
    s.put("/g.m3u8", "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\nseg0.ts\n#EXT-X-GAP\n#EXTINF:2,\nseg1.ts\n#EXTINF:2,\nseg2.ts\n#EXT-X-ENDLIST\n");
    s.put("/seg0.ts", ts(0));
    s.put("/seg2.ts", ts(2));
    let mut src = s.open("/g.m3u8", None).unwrap();
    assert_eq!(read_all(&mut src).unwrap(), [ts(0), ts(2)].concat());
    assert_eq!(s.seen.count(&url("/seg1.ts")), 0);
}

/// The probe reads a stream's head, then hands its source and those bytes to the demuxer: probe
/// bytes replayed plus the rest read from the same source must be the whole stream, with every
/// segment fetched once (a live channel used to fetch its first segment twice, ~2.6 s).
#[test]
fn a_recorded_head_plus_the_rest_is_the_whole_stream_fetched_once() {
    let s = Server::new();
    s.put("/v.m3u8", vod_playlist(0, 3, ""));
    for n in 0..3 {
        s.put(&format!("/seg{n}.ts"), ts(n));
    }
    let mut src = s.open("/v.m3u8", None).unwrap();
    src.record();
    let mut head = vec![0u8; 2000]; // past the first segment's end
    let mut got = 0;
    while got < head.len() {
        got += src.read(&mut head[got..]).unwrap();
    }
    let tape = src.take_tape().expect("recorded");
    assert_eq!(tape, head);
    let rest = read_all(&mut src).unwrap();
    assert_eq!([tape, rest].concat(), [ts(0), ts(1), ts(2)].concat());
    for n in 0..3 {
        assert_eq!(s.seen.count(&url(&format!("/seg{n}.ts"))), 1);
    }
    assert_eq!(src.take_tape(), None, "recording stops at the take");
}

/// A segment body past the cap is an endless response, not a segment: a VOD stream fails on it,
/// a live one drops it and goes on with the next.
#[test]
fn a_segment_past_the_size_cap_fails_a_vod_stream_and_is_skipped_live() {
    let s = Server::new();
    s.put("/vod.m3u8", vod_playlist(0, 2, ""));
    s.put("/seg0.ts", vec![7u8; 5000]);
    s.put("/seg1.ts", ts(1));
    let mut src = s.open("/vod.m3u8", None).unwrap();
    src.set_segment_cap(4000);
    assert_eq!(read_all(&mut src), Err(HlsError::Segment { sequence: 0, error: FetchError::TooLarge }));
    assert_eq!(s.seen.count(&url("/seg0.ts")), 1, "not retried: the answer would be the same");

    let s = Server::new();
    s.route("/live.m3u8", live_route(0, 3, 6, Some(3)));
    s.put("/seg0.ts", vec![7u8; 5000]);
    s.put("/seg1.ts", ts(1));
    s.put("/seg2.ts", ts(2));
    s.put("/seg3.ts", ts(3));
    let mut src = s.open("/live.m3u8", None).unwrap();
    src.set_segment_cap(4000);
    let got = read_all(&mut src).unwrap();
    assert!(got.ends_with(&[ts(1), ts(2), ts(3)].concat()), "the oversized segment was dropped and the rest played");
}

/// Only http and https are requested: the stream URL itself, and every URI a playlist names.
#[test]
fn only_http_and_https_uris_are_requested() {
    let s = Server::new();
    for bad in ["file:///etc/passwd", "ftp://tv.example/live.m3u8", "data:application/x-mpegurl,#EXTM3U"] {
        let r = HlsSource::open(s.transport(1000), s.clock.clone(), bad, None, &tv_platform::devcaps::Caps::assumed());
        assert_eq!(r.err(), Some(HlsError::Scheme), "{bad}");
    }
    assert!(s.seen.urls().is_empty(), "nothing was requested");
    s.put("/vod.m3u8", "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\nfile:///etc/passwd\n#EXT-X-ENDLIST\n");
    assert!(matches!(s.open("/vod.m3u8", None).err(), Some(HlsError::Parse(playlist::ParseError::BadUri { .. }))));
}

/// The archive shapes: `EVENT` + `ENDLIST` is finite, seekable by segment and ends cleanly.
#[test]
fn an_ended_event_playlist_is_a_finite_seekable_stream() {
    let s = Server::new();
    let mut p = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:500\n#EXT-X-PLAYLIST-TYPE:EVENT\n");
    for i in 500..510 {
        p.push_str(&format!("#EXTINF:6.000,\nseg{i}.ts\n"));
        s.put(&format!("/seg{i}.ts"), ts(i));
    }
    p.push_str("#EXT-X-ENDLIST\n");
    s.put("/archive.m3u8", p);
    let mut src = s.open("/archive.m3u8", Some(31_000_000)).unwrap();
    assert!(!src.is_live());
    assert_eq!((src.duration_us(), src.start_us()), (60_000_000, 30_000_000), "starts at the segment holding 31 s");
    let got = read_all(&mut src).unwrap();
    assert_eq!(got, (505..510).map(ts).collect::<Vec<_>>().concat());
    assert_eq!(s.seen.count(&url("/archive.m3u8")), 1, "a finite playlist is never reloaded");
}

/// An archive request for the programme on air answers an EVENT playlist that is still growing:
/// asked for a position, it plays from there (not from its live edge) and goes on reloading as
/// it grows; asked for none, it is a live stream like any other.
#[test]
fn a_growing_event_playlist_asked_for_a_position_plays_from_there_and_follows_its_growth() {
    let s = Server::new();
    let route = |now_ms: u64| {
        let n = 10 + now_ms / 6000;
        let mut p = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:EVENT\n");
        for i in 0..n.min(12) {
            p.push_str(&format!("#EXTINF:6.0,\nseg{i}.ts\n"));
        }
        if n >= 12 {
            p.push_str("#EXT-X-ENDLIST\n");
        }
        Ok(p.into_bytes())
    };
    s.route("/event.m3u8", route);
    for i in 0..12 {
        s.put(&format!("/seg{i}.ts"), ts(i));
    }
    let mut src = s.open("/event.m3u8", Some(13_000_000)).unwrap();
    assert!(src.is_live() && src.in_playlist_time());
    assert_eq!(src.start_us(), 12_000_000, "the segment holding 13 s");
    assert_eq!(read_all(&mut src).unwrap(), (2..12).map(ts).collect::<Vec<_>>().concat(), "from there to the end it grew to");
    let fresh = Server::new();
    fresh.route("/event.m3u8", route);
    let src = fresh.open("/event.m3u8", None).unwrap();
    assert!(!src.in_playlist_time(), "no position: the live edge");
}

/// An https playlist may name only https URIs: a plaintext variant, segment, key or init section
/// is refused before anything is fetched from it (the rule curlio keeps for redirects). A
/// plaintext playlist may name https ones.
#[test]
fn an_https_playlist_naming_a_plaintext_uri_is_refused() {
    let key = "#EXT-X-KEY:METHOD=AES-128,URI=\"http://keys.example/k\"\n";
    let map = "#EXT-X-MAP:URI=\"http://cdn.example/init.mp4\"\n";
    for (name, text) in [
        ("segment", "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\nseg0.ts\n#EXTINF:6,\nhttp://cdn.example/seg1.ts\n#EXT-X-ENDLIST\n".to_owned()),
        ("key", format!("#EXTM3U\n#EXT-X-TARGETDURATION:6\n{key}#EXTINF:6,\nseg0.ts\n#EXT-X-ENDLIST\n")),
        ("init", format!("#EXTM3U\n#EXT-X-TARGETDURATION:6\n{map}#EXTINF:6,\nseg0.m4s\n#EXT-X-ENDLIST\n")),
        ("variant", "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nhttp://cdn.example/low.m3u8\n".to_owned()),
    ] {
        let s = Server::new();
        s.put("/p.m3u8", text);
        assert_eq!(s.open("/p.m3u8", None).err(), Some(HlsError::Insecure), "{name}");
        assert_eq!(s.seen.urls(), [url("/p.m3u8")], "{name}: nothing fetched from it");
    }
}
