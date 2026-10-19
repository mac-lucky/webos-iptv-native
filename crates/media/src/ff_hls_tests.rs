//! The HLS source at the demuxer's AVIO seam (`ff::read_cb`) and through `ff::open_hls`, over the
//! real curl transport and a loopback origin. The demuxer itself needs the bundled FFmpeg, which
//! the host suite does not stage, so these stop where libavformat would start reading: bytes in
//! order, the clean end of a VOD stream as `AVERROR_EOF`, a failure as `AVERROR_IO` with its cause
//! published, teardown as EOF, and the armed seek target consumed at open.

use super::*;
use crate::hls::loopback_tests::{curl_gate, ts, Answer, Origin};

/// Restores the `SHARED` fields these tests write.
struct RestoreShared;

impl Drop for RestoreShared {
    fn drop(&mut self) {
        SHARED.seek_to_ns.store(-1, Ordering::Relaxed);
        SHARED.duration_ns.store(0, Ordering::Relaxed);
        SHARED.dg_http_status.store(0, Ordering::Relaxed);
        SHARED.file_size.store(0, Ordering::Relaxed);
        crate::hls::note_failure(None);
    }
}

fn vod(o: &Origin, segments: &[&str]) -> String {
    let mut p = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PLAYLIST-TYPE:VOD\n");
    for s in segments {
        p.push_str(&format!("#EXTINF:4.0,\n{s}\n"));
    }
    p.push_str("#EXT-X-ENDLIST\n");
    o.put("/vod.m3u8", Answer::playlist(p));
    o.url("/vod.m3u8")
}

fn state(src: Src, aq: &mut AuQueue) -> AvioState {
    AvioState { src, aq, off: 0, size: -1, io_failed: false, bounce: Vec::new(), bounce_pos: 0, parked_at_end: false }
}

/// Every byte `read_cb` hands over until it stops, and the result it stopped with.
fn drain(st: &mut AvioState) -> (Vec<u8>, c_int) {
    let op = st as *mut AvioState as *mut c_void;
    let mut out = Vec::new();
    let mut dst = vec![0u8; 4096];
    loop {
        let r = read_cb(op, dst.as_mut_ptr(), dst.len() as c_int);
        if r <= 0 {
            return (out, r);
        }
        out.extend_from_slice(&dst[..r as usize]);
    }
}

#[test]
fn an_hls_vod_stream_reads_through_avio_to_a_clean_eof_and_publishes_its_duration() {
    let Some(_g) = curl_gate() else { return };
    let _restore = RestoreShared;
    let o = Origin::spawn();
    let url = vod(&o, &["a.ts", "b.ts", "c.ts"]);
    o.put("/a.ts", Answer::ok(ts(1)));
    o.put("/b.ts", Answer::ok(ts(2)));
    o.put("/c.ts", Answer::ok(ts(3)));
    let mut aq = crate::aq::aq_new(1 << 20);
    let (src, size) = open_hls(&url, &crate::headers::RequestHeaders::none(), &mut *aq, &mut Vec::new()).ok().expect("opens");
    assert_eq!(size, -1, "an HLS stream has no byte size");
    assert_eq!(SHARED.duration_ns.load(Ordering::Relaxed), 12_000_000_000, "the playlist's duration");
    let mut st = state(src, &mut aq);
    let (got, end) = drain(&mut st);
    assert_eq!(got, [ts(1), ts(2), ts(3)].concat());
    assert_eq!(end, AVERROR_EOF);
    assert!(!st.io_failed);
    assert_eq!(crate::hls::last_failure(), None);
    crate::aq::aq_destroy(&mut *aq);
}

#[test]
fn an_armed_seek_opens_a_vod_stream_at_its_segment_and_is_consumed() {
    let Some(_g) = curl_gate() else { return };
    let _restore = RestoreShared;
    let o = Origin::spawn();
    let url = vod(&o, &["a.ts", "b.ts", "c.ts"]);
    o.put("/c.ts", Answer::ok(ts(3)));
    SHARED.seek_to_ns.store(9_000_000_000, Ordering::Release);
    let mut aq = crate::aq::aq_new(1 << 20);
    let (src, _) = open_hls(&url, &crate::headers::RequestHeaders::none(), &mut *aq, &mut Vec::new()).ok().expect("opens");
    assert_eq!(SHARED.seek_to_ns.load(Ordering::Acquire), -1, "the unseekable AVIO must not see the target");
    let mut st = state(src, &mut aq);
    let (got, end) = drain(&mut st);
    assert_eq!((got, end), (ts(3), AVERROR_EOF), "9 s is in the third 4 s segment");
    assert_eq!(o.paths(), ["/vod.m3u8", "/c.ts"]);
    crate::aq::aq_destroy(&mut *aq);
}

#[test]
fn a_404_playlist_fails_the_open_with_its_status_published() {
    let Some(_g) = curl_gate() else { return };
    let _restore = RestoreShared;
    let o = Origin::spawn();
    let mut aq = crate::aq::aq_new(1 << 20);
    let r = open_hls(&o.url("/nope.m3u8"), &crate::headers::RequestHeaders::none(), &mut *aq, &mut Vec::new());
    assert!(matches!(r, Err(MediaOpenFail::Failed)));
    assert_eq!(SHARED.dg_http_status.load(Ordering::Relaxed), 404);
    assert_eq!(
        crate::hls::last_failure(),
        Some(crate::hls::HlsError::Playlist(crate::hls::FetchError::Status(404)))
    );
    crate::aq::aq_destroy(&mut *aq);
}

#[test]
fn a_failed_segment_crosses_avio_as_an_io_error_and_teardown_as_eof() {
    let Some(_g) = curl_gate() else { return };
    let _restore = RestoreShared;
    let o = Origin::spawn();
    let url = vod(&o, &["a.ts", "gone.ts"]);
    o.put("/a.ts", Answer::ok(ts(1)));
    let mut aq = crate::aq::aq_new(1 << 20);
    let (src, _) = open_hls(&url, &crate::headers::RequestHeaders::none(), &mut *aq, &mut Vec::new()).ok().expect("opens");
    let mut st = state(src, &mut aq);
    let (got, end) = drain(&mut st);
    assert_eq!(got, ts(1));
    assert_eq!(end, AVERROR_IO, "a missing VOD segment is not the end of the stream");
    assert!(st.io_failed);
    assert_eq!(SHARED.dg_http_status.load(Ordering::Relaxed), 404);
    assert!(matches!(crate::hls::last_failure(), Some(crate::hls::HlsError::Segment { sequence: 1, .. })));

    // Teardown: an aborted lane reads as EOF and requests nothing more.
    let url = vod(&o, &["a.ts", "a.ts"]);
    let mut aq2 = crate::aq::aq_new(1 << 20);
    let (src, _) = open_hls(&url, &crate::headers::RequestHeaders::none(), &mut *aq2, &mut Vec::new()).ok().expect("opens");
    let mut st = state(src, &mut aq2);
    crate::aq::aq_abort(&mut *aq2);
    let before = o.paths().len();
    let (got, end) = drain(&mut st);
    assert_eq!((got.len(), end), (0, AVERROR_EOF));
    assert_eq!(o.paths().len(), before, "no segment is requested after teardown");
    crate::aq::aq_destroy(&mut *aq);
    crate::aq::aq_destroy(&mut *aq2);
}

/// The probe's source and the bytes it read go on to the demuxer: the open fetches nothing and
/// the replay carries what the probe read (a live channel downloaded its first segment twice).
#[test]
fn a_probes_source_is_continued_not_refetched() {
    let Some(_g) = curl_gate() else { panic!("no curl on this host") };
    let _restore = RestoreShared;
    let o = Origin::spawn();
    let url = vod(&o, &["a.ts", "b.ts"]);
    o.put("/a.ts", Answer::ok(ts(1)));
    o.put("/b.ts", Answer::ok(ts(2)));
    let transport = crate::hls::CurlTransport::new(crate::headers::RequestHeaders::none(), Box::new(|| false));
    let mut h = crate::hls::CurlHls::open(transport, crate::hls::SystemClock::new(), &url, None, &tv_platform::devcaps::Caps::assumed()).expect("opens");
    h.record();
    let mut head = vec![0u8; 100];
    assert_eq!(h.read(&mut head), Ok(100));
    let tape = h.take_tape().expect("recorded");
    assert_eq!(tape, head);
    hls_handoff::put(&url, Box::new(h), tape);
    let seen = o.paths().len();
    let mut aq = crate::aq::aq_new(1 << 20);
    let mut replay = Vec::new();
    let before = SHARED.dg_net_rx.load(Ordering::Relaxed);
    let (src, _) = open_hls(&url, &crate::headers::RequestHeaders::none(), &mut *aq, &mut replay).ok().expect("opens");
    assert_eq!(o.paths().len(), seen, "nothing fetched again");
    let mut st = state(src, &mut aq);
    st.bounce = replay;
    let (got, _) = drain(&mut st);
    assert_eq!(got, [ts(1), ts(2)].concat());
    let counted = SHARED.dg_net_rx.load(Ordering::Relaxed) - before;
    assert!(counted >= got.len() as i64, "the replayed bytes are delivered bytes too: {counted} of {}", got.len());
    crate::aq::aq_destroy(&mut *aq);
}

/// The read-out's Transfer and Throughput rows read `dg_net_rx`: every byte an HLS source hands
/// the demuxer must advance it, as the socket and curl sources' bytes do.
#[test]
fn an_hls_fed_read_advances_the_delivered_byte_counter() {
    let Some(_g) = curl_gate() else { return };
    let _restore = RestoreShared;
    let o = Origin::spawn();
    let url = vod(&o, &["a.ts", "b.ts"]);
    o.put("/a.ts", Answer::ok(ts(1)));
    o.put("/b.ts", Answer::ok(ts(2)));
    let mut aq = crate::aq::aq_new(1 << 20);
    let (src, _) = open_hls(&url, &crate::headers::RequestHeaders::none(), &mut *aq, &mut Vec::new()).ok().expect("opens");
    let mut st = state(src, &mut aq);
    let before = SHARED.dg_net_rx.load(Ordering::Relaxed);
    let (got, _) = drain(&mut st);
    let counted = SHARED.dg_net_rx.load(Ordering::Relaxed) - before;
    assert!(counted >= got.len() as i64, "{counted} counted for {} delivered", got.len());
    crate::aq::aq_destroy(&mut *aq);
}
