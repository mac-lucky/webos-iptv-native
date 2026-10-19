//! **The probe before the Load**: open a stream with the bundled demuxer, read what the container
//! says about every stream, close it.
//!
//! The Starfish Load must declare the video codec, the raster, the frame rate, the Dolby Vision
//! layering and the audio codec BEFORE the first access unit is fed (`stream.rs`), and Stremio gives
//! none of it. So a start without a caller's declaration is opened here first, on the same
//! transports the demuxer uses (curl for https or any stream carrying request headers, the socket
//! with redirects followed otherwise), through the same AVIO callbacks, and `avformat_open_input` +
//! `avformat_find_stream_info` read the streams exactly as `demux` will read them a moment later.
//! What comes back is plain data ([`crate::stream::ProbedStreams`]); the declaration is derived
//! from it by the pure `stream::derive_declaration`.
//!
//! **Never a hang.** The probe runs on its own thread (`player::probe`), owns its transports, and
//! is cancelled through [`ProbeIo::cancel`]: the AU-lane abort flag the AVIO callbacks check, a
//! `shutdown` of the socket, and the private curl abort handle published before the first byte is
//! requested. `player::probe` bounds the whole probe in time and turns an overrun into
//! `ProbeFailure::Timeout`.
//!
//! **No read-out side effects.** The probe writes nothing into `SHARED`'s per-playback diagnostics
//! (the demuxer publishes those for the playback that actually runs), and its curl transfer never
//! registers as teardown's wake target (`curlio::ACTIVE`), so a probe still finishing after a
//! cancel can neither clobber nor deregister a demuxer's.

use super::*;
use crate::stream::{Dovi, NotVideo, ProbeFailure, ProbedAudio, ProbedStreams, ProbedVideo};
use std::sync::{Arc, Mutex};

/// The probe's transports and their cancellation, shared between the probe thread and whoever
/// may cancel it. Freed when the last holder drops it — which may be the probe thread, after a
/// cancel that did not wait for it — so nothing here is ever closed under a thread still using it.
pub struct ProbeIo {
    /// A private AU lane, used only for its abort flag: the AVIO callbacks check it on entry.
    lane: *mut AuQueue,
    /// The socket for a plaintext stream (unused, fd -1, for curl).
    hs: *mut HttpStream,
    /// The curl transfer's private abort handle, published before its first byte is requested.
    curl: Mutex<Option<Arc<crate::curlio::Abort>>>,
}

// The lane and the socket are built for cross-thread signalling (`aq_abort` and `http_shutdown`
// take their own locks); the probe thread is their only other user.
unsafe impl Send for ProbeIo {}
unsafe impl Sync for ProbeIo {}

impl ProbeIo {
    pub fn new() -> Arc<ProbeIo> {
        Arc::new(ProbeIo {
            lane: Box::into_raw(crate::aq::aq_new(1)),
            hs: Box::into_raw(tv_net::stream::http_stream_boxed()),
            curl: Mutex::new(None),
        })
    }

    /// Stop the probe wherever it is: the callbacks answer EOF from here on, a blocked socket read
    /// is woken by `shutdown`, a blocked `curl_multi_wait` by its abort pipe. Returns at once.
    pub fn cancel(&self) {
        crate::aq::aq_abort(self.lane);
        tv_net::stream::http_shutdown(self.hs);
        if let Some(a) = self.curl.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            a.signal();
        }
    }

    fn cancelled(&self) -> bool {
        unsafe { crate::aq::aq_is_aborted(self.lane) }
    }
}

impl Drop for ProbeIo {
    fn drop(&mut self) {
        // Last holder: no thread reads either transport any more.
        tv_net::stream::http_close(self.hs);
        unsafe {
            drop(Box::from_raw(self.hs));
            drop(Box::from_raw(self.lane));
        }
    }
}

/// Open `url` (with its `headers` on every request), read its streams, close it. Blocking: run it
/// off the main thread (`player::probe`).
/// How the probe reaches a stream: the demuxer's own transport rule (`demux`'s `via_curl` and its
/// playlist test), so the probe reads what the demuxer will.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeTransport {
    /// An HLS playlist, read through [`crate::hls`] as one byte stream.
    Hls,
    /// TLS, or a stream whose requests carry its own headers.
    Curl,
    /// Plaintext without headers.
    Socket,
}

fn probe_transport(url: &str, headers: &crate::headers::RequestHeaders) -> ProbeTransport {
    let (origin, _) = tv_net::net::origin::split(url);
    if crate::hls::is_playlist_url(url) {
        ProbeTransport::Hls
    } else if origin.is_tls() || !headers.is_empty() {
        ProbeTransport::Curl
    } else {
        ProbeTransport::Socket
    }
}

/// Open `url` as HLS for the probe. Unlike the demuxer's open it leaves `SHARED` alone (the armed
/// seek target belongs to the demuxer that follows) and cancels on the probe's own lane.
fn open_hls_for_probe(url: &str, headers: &crate::headers::RequestHeaders, io: &ProbeIo) -> Result<(Src, i64), ()> {
    let lane = io.lane as usize;
    let cancel = Box::new(move || unsafe { crate::aq::aq_is_aborted(lane as *mut AuQueue) });
    let transport = crate::hls::CurlTransport::new(headers.clone(), cancel);
    crate::hls::CurlHls::open(transport, crate::hls::SystemClock::new(), url, None, tv_platform::devcaps::caps())
        .map(|mut h| {
            h.record();
            (Src::Hls(Box::new(h)), -1)
        })
        .map_err(|e| {
            crate::player::log(&format!("probe: hls open FAILED: {e}"));
            // Which HLS failure refused the start (`hls::last_failure`): the no-archive stub and a
            // failed archive window are told apart from it.
            if e != crate::hls::HlsError::Aborted {
                crate::hls::note_failure(Some(e));
            }
        })
}

pub fn probe(url: &str, headers: &crate::headers::RequestHeaders, io: &ProbeIo) -> Result<ProbedStreams, ProbeFailure> {
    if !ABI_OK.load(Ordering::Acquire) {
        crate::player::log("probe: refusing — FFmpeg is not loaded, or not the ABI built for");
        return Err(ProbeFailure::Unavailable);
    }
    if tv_net::net::origin::Origin::parse(url).is_none() {
        crate::player::log("probe: the URL is not an http(s) URL");
        return Err(ProbeFailure::Open);
    }
    ensure_registered();
    // `ff::open_failure_is_transient`: the same retry, for the same reason, as the demuxer's. A
    // debrid link opened for the first time can answer junk for a moment while the service
    // prepares it (measured on the C1: two immediate attempts failed INVALIDDATA, a start a few
    // seconds later played), so the retries wait first.
    let mut result = probe_once(url, headers, io);
    for (attempt, wait_ms) in PROBE_RETRY_WAITS_MS.iter().enumerate() {
        match result {
            Err((_, true)) if !io.cancelled() => {
                crate::player::log(&format!("probe: open_input retry #{} from the original URL in {wait_ms} ms", attempt + 1));
                if !wait_unless_cancelled(*wait_ms, io) {
                    return Err(ProbeFailure::Open);
                }
                result = probe_once(url, headers, io);
            }
            _ => break,
        }
    }
    result.map_err(|(f, _)| f)
}

/// How long each retry of a transient open failure waits first.
const PROBE_RETRY_WAITS_MS: [u64; 2] = [1500, 3000];

/// Sleep `ms` in short slices; `false` when the probe was cancelled meanwhile.
fn wait_unless_cancelled(ms: u64, io: &ProbeIo) -> bool {
    let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    while std::time::Instant::now() < until {
        if io.cancelled() {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    !io.cancelled()
}

/// One probe attempt from `url`. `Err`'s flag: `avformat_open_input` failed transiently, worth one
/// retry from the original URL.
fn probe_once(
    url: &str,
    headers: &crate::headers::RequestHeaders,
    io: &ProbeIo,
) -> Result<ProbedStreams, (ProbeFailure, bool)> {
    let began = std::time::Instant::now();
    let (origin, path) = tv_net::net::origin::split(url);
    let transport = probe_transport(url, headers);
    let publish = |a: &Arc<crate::curlio::Abort>| {
        *io.curl.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(a));
    };
    let opened = match transport {
        ProbeTransport::Hls => open_hls_for_probe(url, headers, io),
        ProbeTransport::Curl => crate::curlio::CurlSource::open_private_with(url, 0, headers, &publish)
            .map_err(|e| crate::player::log(&format!("probe: open FAILED: {e:?}")))
            .and_then(|cs| {
                // A playlist served from a URL that does not say so: the demuxer recognises it by
                // its Content-Type after the curl open, and so does the probe.
                if cs.content_type().is_some_and(crate::hls::is_playlist_content_type) {
                    drop(cs);
                    open_hls_for_probe(url, headers, io)
                } else {
                    let size = cs.size();
                    Ok((Src::Curl(cs), size))
                }
            }),
        ProbeTransport::Socket => {
        let mut hop = |hop_url: &str| crate::curlio::CurlSource::open_private_with(hop_url, 0, &crate::headers::RequestHeaders::none(), &publish);
        open_plain_following(io.hs, &origin, path, io.lane, &mut hop)
            .map(|(src, size, _)| (src, size))
            .map_err(|_| ())
        }
    };
    let Ok((src, size)) = opened else {
        return Err((ProbeFailure::Open, false));
    };
    if io.cancelled() {
        return Err((ProbeFailure::Open, false));
    }
    let mut state = Box::new(AvioState {
        src,
        aq: io.lane,
        off: 0,
        size,
        io_failed: false,
        bounce: Vec::new(),
        bounce_pos: 0,
        parked_at_end: false,
    });
    // An HLS stream goes through its own client, which reads playlists, not a body to name.
    let not_video = if matches!(state.src, Src::Hls(_)) {
        None
    } else {
        let head = unsafe { read_head(&mut state) };
        let content_type = match &state.src {
            Src::Curl(cs) => cs.content_type(),
            _ => None,
        };
        sniff(&head, content_type).map(|(kind, label)| (kind, label.to_owned()))
    };
    let result = unsafe { read_streams(&mut state, not_video) };
    crate::player::log(&format!(
        "probe: {} in {} ms",
        match &result {
            Ok(p) => format!(
                "v={} a={} ({})",
                p.video.as_ref().map_or("-", |v| v.codec.as_str()),
                p.audio.len(),
                p.audio.iter().map(|a| format!("{}/{}ch{}", a.codec, a.channels, if a.atmos { "/atmos" } else { "" })).collect::<Vec<_>>().join(" ")
            ),
            Err((f, _)) => format!("FAILED {f:?}"),
        },
        began.elapsed().as_millis()
    ));
    // A playlist read from its start goes on to the demuxer with what was read: it continues the
    // stream instead of fetching it again (`ff::hls_handoff`).
    super::hls_handoff::clear();
    if let (Ok(_), Src::Hls(mut h)) = (&result, state.src) {
        match h.take_tape() {
            Some(bytes) => {
                crate::player::log(&format!("probe: hls source handed on with {} bytes", bytes.len()));
                super::hls_handoff::put(url, h, bytes);
            }
            None => crate::player::log("probe: hls source not handed on (read past the recording cap)"),
        }
    }
    // the curl source closed with `state`; the socket closes with `ProbeIo`
    result
}

/// The first bytes the probe reads ahead of libavformat, to name a body that is not a video.
const SNIFF_BYTES: usize = 512;

/// Read up to [`SNIFF_BYTES`] through `read_cb` and put them back as the state's bounce, so
/// libavformat reads exactly the bytes it would have read (the transport sits `bounce` ahead of
/// `off`, as when the demuxer parks).
unsafe fn read_head(state: &mut AvioState) -> Vec<u8> {
    let mut head = vec![0u8; SNIFF_BYTES];
    let mut got = 0usize;
    while got < head.len() {
        let r = read_cb(state as *mut AvioState as *mut c_void, head.as_mut_ptr().add(got), (head.len() - got) as c_int);
        if r <= 0 {
            break;
        }
        got += r as usize;
    }
    head.truncate(got);
    state.off = 0;
    state.bounce = head.clone();
    state.bounce_pos = 0;
    head
}

/// PURE: **a body that is plainly not a video**, and the media type to log it as.
///
/// Real media is never named: Matroska/WebM, MP4/MOV (`ftyp` and the other top-level boxes at 4),
/// AVI, ASF, FLV and an HLS playlist are recognised first; then images and archives by their
/// magic numbers; then MPEG-TS (sync at 0 and 188, or M2TS at 4 and 196); then text: an HTML
/// page, JSON, plain text. Only
/// when the bytes say nothing does the `Content-Type` decide. Read only when libavformat has
/// already refused the open, so a misreading here can never refuse a file that opens.
fn sniff<'a>(head: &[u8], content_type: Option<&'a str>) -> Option<(NotVideo, &'a str)> {
    let at = |i: usize, sig: &[u8]| head.get(i..i + sig.len()) == Some(sig);
    let ts = |first: usize, stride: usize| head.get(first) == Some(&0x47) && head.get(first + stride).is_none_or(|&b| b == 0x47);
    let media = at(0, &[0x1A, 0x45, 0xDF, 0xA3])
        || [b"ftyp", b"moov", b"mdat", b"free", b"wide", b"skip"].iter().any(|b| at(4, *b))
        || (at(0, b"RIFF") && at(8, b"AVI "))
        || at(0, &[0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11])
        || at(0, b"FLV")
        || at(0, b"#EXTM3U");
    if media {
        return None;
    }
    let mime = content_type.map(|t| t.split(';').next().unwrap_or("").trim().to_ascii_lowercase());
    let is_html = mime.as_deref() == Some("text/html");
    let by_bytes = if at(0, b"\x89PNG\r\n\x1a\n") {
        Some((NotVideo::Image, "image/png"))
    } else if at(0, &[0xFF, 0xD8, 0xFF]) {
        Some((NotVideo::Image, "image/jpeg"))
    } else if at(0, b"GIF87a") || at(0, b"GIF89a") {
        Some((NotVideo::Image, "image/gif"))
    } else if at(0, b"RIFF") && at(8, b"WEBP") {
        Some((NotVideo::Image, "image/webp"))
    } else if at(0, b"PK\x03\x04") {
        Some((NotVideo::Archive, "application/zip"))
    } else if at(0, b"Rar!\x1a\x07") {
        Some((NotVideo::Archive, "application/vnd.rar"))
    } else if ts(0, 188) || ts(4, 192) {
        // a one-byte sync, so only after the magic numbers ("GIF8" opens with 0x47 too)
        return None;
    } else {
        sniff_text(head).map(|(kind, label)| if is_html { (NotVideo::WebPage, "text/html") } else { (kind, label) })
    };
    if by_bytes.is_some() {
        return by_bytes;
    }
    let kind = match mime.as_deref()? {
        m if m.starts_with("image/") => NotVideo::Image,
        "application/zip" | "application/x-zip-compressed" | "application/vnd.rar" | "application/x-rar-compressed" => NotVideo::Archive,
        "text/html" => NotVideo::WebPage,
        _ => return None,
    };
    content_type.map(|t| (kind, t))
}

/// Text: an HTML page (a `<!doctype`, `<html`, `<head`, `<body` or `<!--` opening, any case,
/// after whitespace and a BOM), JSON (`{`), or anything else that is printable UTF-8.
fn sniff_text(head: &[u8]) -> Option<(NotVideo, &'static str)> {
    let text = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    let t = &text[text.iter().position(|b| !b.is_ascii_whitespace())?..];
    let opens = |p: &[u8]| t.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p));
    if [&b"<!doctype"[..], b"<html", b"<head", b"<body", b"<!--"].iter().any(|p| opens(p)) {
        return Some((NotVideo::WebPage, "text/html"));
    }
    let printable = text.iter().all(|&b| (b >= 0x20 && b != 0x7F) || matches!(b, b'\t' | b'\n' | b'\r'));
    // a multi-byte character cut by the sniff window still reads as text
    let utf8 = std::str::from_utf8(text).map_or_else(|e| e.error_len().is_none(), |_| true);
    if !(printable && utf8) {
        return None;
    }
    Some(if t[0] == b'{' { (NotVideo::Document, "application/json") } else { (NotVideo::Document, "text/plain") })
}

/// The libavformat half: an AVIO over `state`, open, find the streams, copy them out, close.
/// `not_video` is what [`sniff`] made of the first bytes, read only if the open fails.
unsafe fn read_streams(
    state: &mut Box<AvioState>,
    not_video: Option<(NotVideo, String)>,
) -> Result<ProbedStreams, (ProbeFailure, bool)> {
    let buf = av_malloc(65536) as *mut u8;
    if buf.is_null() {
        return Err((ProbeFailure::Unavailable, false));
    }
    let avio = avio_alloc_context(
        buf,
        65536,
        0,
        &mut **state as *mut AvioState as *mut c_void,
        Some(read_cb),
        None,
        // An HLS stream is one forward byte stream with no positions to seek to, as in `demux`.
        if matches!(state.src, Src::Hls(_)) { None } else { Some(seek_cb) },
    );
    if avio.is_null() {
        free_ptr(buf as *mut c_void);
        return Err((ProbeFailure::Unavailable, false));
    }
    let mut fmt = avformat_alloc_context();
    if fmt.is_null() {
        free_avio(avio);
        return Err((ProbeFailure::Unavailable, false));
    }
    (*fmt).pb = avio;
    if matches!(state.src, Src::Hls(_)) {
        super::cap_hls_probe(fmt);
    }
    let r = avformat_open_input(&mut fmt, std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut());
    if r < 0 || fmt.is_null() {
        crate::player::log(&format!("probe: open_input failed r={r}"));
        free_avio(avio);
        // Not a video at all: an image, an archive or a document will not change on a retry; a
        // page might (a debrid link still being prepared), so only a page waits for one.
        if let Some((kind, label)) = not_video {
            let size = if state.size >= 0 { format!(", {} bytes", state.size) } else { String::new() };
            crate::player::log(&format!("probe: not a video ({label}{size})"));
            // a page or a JSON/text error may be a debrid link still being prepared: retried
            return Err((ProbeFailure::NotVideo(kind), matches!(kind, NotVideo::WebPage | NotVideo::Document)));
        }
        // An HLS stream is not reopened here: its source has retries of its own.
        return Err((ProbeFailure::Open, !matches!(state.src, Src::Hls(_)) && open_failure_is_transient(r)));
    }
    let r = avformat_find_stream_info(fmt, std::ptr::null_mut());
    if r < 0 {
        crate::player::log(&format!("probe: find_stream_info failed r={r}"));
        avformat_close_input(&mut fmt);
        free_avio(avio);
        return Err((ProbeFailure::StreamInfo, false));
    }
    let mut probed = collect(fmt);
    fill_from_first_packets(fmt, &mut probed);
    avformat_close_input(&mut fmt);
    free_avio(avio);
    Ok(probed)
}

/// (sample rate, channels) from an ADTS header at the start of `data`; `None` when there is no
/// header or its channel configuration is 0 (carried in-band, unknown here). ISO 14496-3 1.A.2.
const RATES: [i64; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// The CORE sampling rate an AudioSpecificConfig names (`aot(5) freqIdx(4) …`): for HE-AAC, half
/// what FFmpeg reports as `sample_rate`, and the rate `ff::adts_params` frames each packet with.
/// `None` for an escape-coded object type or an explicit 24-bit rate (exotic; codecpar stands).
fn asc_core_rate(asc: &[u8]) -> Option<i64> {
    let (b0, b1) = (*asc.first()?, *asc.get(1)?);
    if b0 >> 3 == 31 {
        return None;
    }
    RATES.get(usize::from(((b0 & 0x07) << 1) | (b1 >> 7))).copied()
}

fn adts_audio(data: &[u8]) -> Option<(i64, i64)> {
    if data.len() < 7 || data[0] != 0xFF || data[1] & 0xF6 != 0xF0 {
        return None;
    }
    let rate = *RATES.get(usize::from((data[2] >> 2) & 0x0F))?;
    let config = ((data[2] & 0x01) << 2) | (data[3] >> 6);
    let channels = match config {
        0 => return None,
        7 => 8,
        c => i64::from(c),
    };
    Some((rate, channels))
}

/// **Dolby Atmos in E-AC-3 (JOC)**, read from the first independent substream-0 frame of
/// `data`: `Some(true)` when its `addbsi` sets `flag_ec3_extension_type_a`, `Some(false)` when that
/// frame carries no such extension, `None` when `data` holds no E-AC-3 frame this can read.
///
/// The bundled FFmpeg has the `ac3` PARSER, which reads only the first 54 header bits, and no
/// E-AC-3 decoder — the only thing that sets `AV_PROFILE_EAC3_DDP_ATMOS` — so the bit walk is
/// ported here, field for field, from FFmpeg 9.0 `libavcodec/ac3_parser.c`'s
/// `ff_ac3_parse_header` + `eac3_parse_header` (ETSI TS 102 366 Annex E.1.2 `bsi()`): the first
/// `addbsi` byte's least significant bit is the extension flag, as `ac3dec.c` reads it.
fn eac3_joc(data: &[u8]) -> Option<bool> {
    let mut at = 0usize;
    // a packet can hold an independent frame followed by its dependent ones
    while at + 6 <= data.len() {
        let frame = &data[at..];
        let (size, joc) = eac3_frame(frame)?;
        if let Some(joc) = joc {
            return Some(joc);
        }
        at += size;
    }
    None
}

/// One E-AC-3 frame at the start of `f`: its size, and its JOC flag when it is an independent
/// substream-0 frame (`None` for a frame that is not). `None` overall when `f` is not E-AC-3.
fn eac3_frame(f: &[u8]) -> Option<(usize, Option<bool>)> {
    struct Bits<'a>(&'a [u8], usize);
    impl Bits<'_> {
        fn get(&mut self, n: usize) -> Option<u32> {
            let mut v = 0u32;
            for _ in 0..n {
                let b = self.0.get(self.1 / 8)?;
                v = (v << 1) | u32::from((b >> (7 - self.1 % 8)) & 1);
                self.1 += 1;
            }
            Some(v)
        }
        fn skip(&mut self, n: usize) -> Option<()> {
            self.get(n.min(32))?;
            if n > 32 {
                self.1 += n - 32;
                (self.1 <= self.0.len() * 8).then_some(())
            } else {
                Some(())
            }
        }
    }
    let mut b = Bits(f, 0);
    if b.get(16)? != 0x0B77 {
        return None;
    }
    let frame_type = b.get(2)?; // strmtyp: 0 independent, 1 dependent, 2 AC-3 convert
    if frame_type == 3 {
        return None;
    }
    let substream = b.get(3)?;
    let size = (b.get(11)? as usize + 1) * 2;
    let fscod = b.get(2)?;
    let num_blocks = if fscod == 3 {
        b.get(2)?; // fscod2
        6
    } else {
        [1, 2, 3, 6][b.get(2)? as usize]
    };
    let acmod = b.get(3)?;
    let lfe = b.get(1)? == 1;
    let bsid = b.get(5)?;
    if !(11..=16).contains(&bsid) {
        return None; // AC-3, not E-AC-3
    }
    if frame_type == 1 || substream != 0 {
        return Some((size, None));
    }
    let nch = if acmod == 0 { 2 } else { 1 };
    for _ in 0..nch {
        b.skip(5)?; // dialnorm
        if b.get(1)? == 1 {
            b.skip(8)?; // compr
        }
    }
    if b.get(1)? == 1 {
        // mixmdate
        if acmod > 2 {
            b.skip(2)?; // dmixmod
            if acmod & 1 == 1 {
                b.skip(6)?; // ltrtcmixlev, lorocmixlev
            }
            if acmod & 4 == 4 {
                b.skip(6)?; // ltrtsurmixlev, lorosurmixlev
            }
        }
        if lfe && b.get(1)? == 1 {
            b.skip(5)?; // lfemixlevcod
        }
        if frame_type == 0 {
            for _ in 0..nch {
                if b.get(1)? == 1 {
                    b.skip(6)?; // pgmscl
                }
            }
            if b.get(1)? == 1 {
                b.skip(6)?; // extpgmscl
            }
            match b.get(2)? {
                1 => b.skip(5)?,
                2 => b.skip(12)?,
                3 => {
                    let len = (b.get(5)? as usize + 2) * 8;
                    b.skip(len)?;
                }
                _ => {}
            }
            if acmod < 2 {
                for _ in 0..nch {
                    if b.get(1)? == 1 {
                        b.skip(14)?; // panmean, paninfo
                    }
                }
            }
            if b.get(1)? == 1 {
                // frmmixcfginfoe
                for _ in 0..num_blocks {
                    if num_blocks == 1 || b.get(1)? == 1 {
                        b.skip(5)?;
                    }
                }
            }
        }
    }
    if b.get(1)? == 1 {
        // infomdate
        b.skip(5)?; // bsmod, copyrightb, origbs
        if acmod == 2 {
            b.skip(4)?; // dsurmod, dheadphonmod
        }
        if acmod >= 6 {
            b.skip(2)?; // dsurexmod
        }
        for _ in 0..nch {
            if b.get(1)? == 1 {
                b.skip(8)?; // mixlevel, roomtyp, adconvtyp
            }
        }
        if fscod != 3 {
            b.skip(1)?; // sourcefscod
        }
    }
    if frame_type == 0 && num_blocks != 6 {
        b.skip(1)?; // convsync
    }
    if frame_type == 2 && (num_blocks == 6 || b.get(1)? == 1) {
        b.skip(6)?; // frmsizecod
    }
    if b.get(1)? == 0 {
        return Some((size, Some(false))); // no addbsi
    }
    b.skip(6)?; // addbsil
    b.skip(7)?;
    Some((size, Some(b.get(1)? == 1)))
}

/// The packets [`fill_from_first_packets`] reads at most, looking for each track's first packet.
const ADTS_SCAN_PACKETS: usize = 400;

/// What only a track's first packets say: an AAC track the container left without a rate or a
/// channel count (MPEG-TS, when find_stream_info decoded no frame of it) takes them from
/// its ADTS header, and one still unknown is declared stereo 48 kHz, which is what nearly every
/// broadcast AAC is, rather than 0 channels, which the TV's decoder plays as silence. An E-AC-3
/// track is Atmos when its first frame says JOC ([`eac3_joc`]).
unsafe fn fill_from_first_packets(fmt: *mut AVFormatContext, probed: &mut ProbedStreams) {
    let streams = (*fmt).streams;
    let audio_streams: Vec<c_int> = (0..(*fmt).nb_streams as c_int)
        .filter(|&i| (*stream_codecpar(*streams.add(i as usize))).codec_type == AVMEDIA_TYPE_AUDIO)
        .collect();
    let missing = |p: &ProbedStreams| -> Vec<usize> {
        p.audio.iter().enumerate().filter(|(_, a)| a.codec == "aac" && (a.channels == 0 || a.sample_rate == 0)).map(|(k, _)| k).collect()
    };
    // E-AC-3 tracks whose JOC flag is still to be read (the profile already said Atmos: done)
    let mut eac3: Vec<usize> = probed.audio.iter().enumerate().filter(|(_, a)| a.codec == "eac3" && !a.atmos).map(|(k, _)| k).collect();
    if missing(probed).is_empty() && eac3.is_empty() {
        return;
    }
    let pkt = av_packet_alloc();
    if !pkt.is_null() {
        for _ in 0..ADTS_SCAN_PACKETS {
            if (missing(probed).is_empty() && eac3.is_empty()) || av_read_frame(fmt, pkt) < 0 {
                break;
            }
            if let Some(k) = audio_streams.iter().position(|&i| i == (*pkt).stream_index) {
                let a = &mut probed.audio[k];
                if (*pkt).size > 0 && !(*pkt).data.is_null() {
                    let data = std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize);
                    if a.codec == "aac" && (a.channels == 0 || a.sample_rate == 0) {
                        if let Some((rate, channels)) = adts_audio(data) {
                            crate::player::log(&format!("probe: aac track {k} from ADTS: {rate} Hz {channels}ch"));
                            a.sample_rate = rate;
                            a.channels = channels;
                        }
                    }
                    if let Some(pos) = eac3.iter().position(|&e| e == k) {
                        if let Some(joc) = eac3_joc(data) {
                            crate::player::log(&format!("probe: eac3 track {k} JOC (Atmos) {}", if joc { "yes" } else { "no" }));
                            a.atmos = joc;
                            eac3.swap_remove(pos);
                        }
                    }
                }
            }
            av_packet_unref(pkt);
        }
        let mut p = pkt;
        av_packet_free(&mut p);
    }
    for k in missing(probed) {
        crate::player::log(&format!("probe: aac track {k} has no ADTS header in reach; declaring 48000 Hz 2ch"));
        let a = &mut probed.audio[k];
        a.sample_rate = 48000;
        a.channels = 2;
    }
}

/// A rational as a rate, or 0.0 when it is unknown (`0/x`, `x/0`, negative).
fn rate_of(r: AVRational) -> f64 {
    if r.num > 0 && r.den > 0 {
        f64::from(r.num) / f64::from(r.den)
    } else {
        0.0
    }
}

/// Copy every video and audio stream's facts out of an open context. Everything returned is OWNED:
/// the strings and records point into structs `avformat_close_input` frees.
unsafe fn collect(fmt: *mut AVFormatContext) -> ProbedStreams {
    let streams = (*fmt).streams;
    let vi = av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, -1, -1, std::ptr::null_mut(), 0);
    let name = |id: c_int| std::ffi::CStr::from_ptr(avcodec_get_name(id)).to_string_lossy().to_ascii_lowercase();
    let video = (vi >= 0).then(|| {
        let st = *streams.add(vi as usize);
        let cp = stream_codecpar(st);
        let at = |off: usize| *((st as *const u8).add(off) as *const AVRational);
        let fps = match rate_of(at(OFF_STREAM_AVG_FRAME_RATE)) {
            f if f > 0.0 => f,
            _ => rate_of(at(OFF_STREAM_R_FRAME_RATE)),
        };
        let dovi = dovi_conf(cp).map_or(Dovi::NONE, |d| Dovi {
            present: true,
            profile: i64::from(d.dv_profile),
            bl_compat: i64::from(d.dv_bl_signal_compatibility_id),
            el_present: d.el_present_flag != 0,
        });
        ProbedVideo { codec: name((*cp).codec_id), width: (*cp).width, height: (*cp).height, fps, dovi, profile: (*cp).profile }
    });
    let mut audio = Vec::new();
    for i in 0..(*fmt).nb_streams {
        let st = *streams.add(i as usize);
        let cp = stream_codecpar(st);
        let disposition = *((st as *const u8).add(OFF_STREAM_DISPOSITION) as *const c_int);
        if (*cp).codec_type != AVMEDIA_TYPE_AUDIO {
            continue;
        }
        let codec = name((*cp).codec_id);
        // AAC with no AudioSpecificConfig (ADTS in MPEG-TS) is declared by its ADTS header, read in
        // `fill_from_first_packets`: the bundled AAC decoder makes find_stream_info report what a
        // decoded frame outputs (HE-AAC's SBR doubles the rate, its PS makes mono stereo), not
        // what the frames, and so the Load's `aacInfo`, say.
        let adts = codec == "aac" && ((*cp).extradata.is_null() || (*cp).extradata_size < 2);
        // AAC: the AudioSpecificConfig's core rate (HE-AAC's SBR doubles codecpar's)
        let sample_rate = match codec.as_str() {
            _ if adts => Some(0),
            "aac" if !(*cp).extradata.is_null() && (*cp).extradata_size >= 2 => {
                asc_core_rate(std::slice::from_raw_parts((*cp).extradata, (*cp).extradata_size as usize))
            }
            _ => None,
        }
        .unwrap_or(i64::from((*cp).sample_rate.max(0)));
        audio.push(ProbedAudio {
            atmos: codec == "eac3" && (*cp).profile == AV_PROFILE_EAC3_DDP_ATMOS,
            codec,
            channels: if adts { 0 } else { i64::from((*cp).ch_layout.nb_channels.max(0)) },
            sample_rate,
            default: disposition & AV_DISPOSITION_DEFAULT != 0,
            forced: disposition & AV_DISPOSITION_FORCED != 0,
            name: stream_name(st),
            language: stream_language(st).unwrap_or_default(),
        });
    }
    let dur = fmt_duration(fmt);
    let start = fmt_start_time(fmt);
    let start_ns = if start != AV_NOPTS_VALUE && start > 0 { start.saturating_mul(1000) } else { 0 };
    ProbedStreams { video, audio, duration_ns: if dur > 0 { dur.saturating_mul(1000) } else { 0 }, start_ns }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **HE-AAC declares its CORE rate**, the one the AudioSpecificConfig's frequency index names
    /// and the ADTS header we reframe with carries. FFmpeg's `sample_rate` is the SBR OUTPUT rate
    /// (twice the core), so a 24 kHz-core HE-AAC track was declared `aacInfo.frequency=48` over
    /// ADTS frames saying 24 — measured on the dev set from an MP4 and a Matroska file (an MPEG-TS
    /// one, read from its ADTS, declared 24).
    #[test]
    fn the_asc_core_rate_is_the_declared_rate() {
        // AAC-LC 48 kHz stereo: aot 2, idx 3
        assert_eq!(asc_core_rate(&[0x11, 0x90]), Some(48000));
        // HE-AAC explicit signalling: aot 5, idx 6 (24 kHz core), then the extension rate
        assert_eq!(asc_core_rate(&[0x2B, 0x08, 0x80]), Some(24000));
        // HE-AAC implicit (aot 2 core, idx 6)
        assert_eq!(asc_core_rate(&[0x13, 0x08]), Some(24000));
        assert_eq!(asc_core_rate(&[0xF8, 0x00]), None, "escape-coded AOT: not read");
        assert_eq!(asc_core_rate(&[0x17, 0x80]), None, "explicit 24-bit rate (idx 15): not read");
        assert_eq!(asc_core_rate(&[0x11]), None);
    }

    /// AAC in MPEG-TS (every live HLS channel) carries its rate and channel count only in each
    /// packet's ADTS header; the bundled FFmpeg has no AAC decoder to fill them in, so the probe
    /// reported `aac 0ch` and the TV's decoder was configured for silence.
    #[test]
    fn adts_headers_name_the_rate_and_channels() {
        // 48 kHz (index 3), AAC-LC, 2 channels
        assert_eq!(adts_audio(&[0xFF, 0xF1, 0x4C, 0x80, 0x2E, 0x7F, 0xFC]), Some((48000, 2)));
        // 44.1 kHz (index 4), 5.1 (config 6)
        assert_eq!(adts_audio(&[0xFF, 0xF9, 0x51, 0x80, 0x10, 0x1F, 0xFC]), Some((44100, 6)));
        // config 7 is 7.1, eight channels
        assert_eq!(adts_audio(&[0xFF, 0xF1, 0x4D, 0xC0, 0x10, 0x1F, 0xFC]), Some((48000, 8)));
        assert_eq!(adts_audio(&[0xFF, 0xF1, 0x4C, 0x00, 0x2E, 0x7F, 0xFC]), None, "config 0 is in-band, unknown here");
        assert_eq!(adts_audio(&[0x00, 0xF1, 0x4C, 0x80, 0x2E, 0x7F, 0xFC]), None, "no sync word");
        assert_eq!(adts_audio(&[0xFF, 0xF1]), None);
    }

    /// A bit writer for hand-built E-AC-3 headers, MSB first, in `bsi()` field order.
    struct W(Vec<u8>, usize);
    impl W {
        fn put(&mut self, n: usize, v: u32) -> &mut Self {
            for k in (0..n).rev() {
                if self.1 % 8 == 0 {
                    self.0.push(0);
                }
                let last = self.0.len() - 1;
                self.0[last] |= (((v >> k) & 1) as u8) << (7 - self.1 % 8);
                self.1 += 1;
            }
            self
        }
    }

    /// An E-AC-3 5.1 frame header as a Dolby Atmos (JOC) encoder lays it out — ETSI TS 102 366
    /// E.1.2: independent substream 0, 48 kHz, six blocks, 3/2 + LFE, bsid 16, then the mixing
    /// and informational metadata a real stream carries (`mixmdate`/`infomdate` set, so every
    /// conditional field the parser walks is exercised), then `addbsi`: length 1 (two bytes), the
    /// first byte's LSB `flag_ec3_extension_type_a`, the second the JOC complexity index (16 here).
    /// Built by hand because no encoder this host has emits JOC.
    fn joc_frame(strmtyp: u32, substream: u32, ext_a: Option<bool>) -> Vec<u8> {
        let mut w = W(Vec::new(), 0);
        w.put(16, 0x0B77).put(2, strmtyp).put(3, substream).put(11, 767).put(2, 0).put(2, 3);
        w.put(3, 7).put(1, 1).put(5, 16); // acmod 3/2, lfeon, bsid
        w.put(5, 31).put(1, 1).put(8, 0x80); // dialnorm, compre + compr
        if strmtyp == 1 {
            w.put(1, 0); // chanmape
        }
        w.put(1, 1); // mixmdate
        w.put(2, 1).put(3, 4).put(3, 4).put(3, 4).put(3, 4); // dmixmod, c/s mix levels
        w.put(1, 1).put(5, 10); // lfemixlevcode
        if strmtyp == 0 {
            w.put(1, 1).put(6, 33); // pgmscl
            w.put(1, 0); // extpgmscle
            w.put(2, 3).put(5, 1).put(24, 0xABCDEF); // mixdef 3: (1+2) bytes of mixdata
            w.put(1, 1); // frmmixcfginfoe: one bit + 5 per block (6 blocks)
            for _ in 0..6 {
                w.put(1, 1).put(5, 7);
            }
        }
        w.put(1, 1).put(3, 0).put(2, 0); // infomdate: bsmod, copyright, orig
        w.put(2, 2); // dsurexmod (acmod >= 6)
        w.put(1, 1).put(8, 0x55); // mixlevel/roomtyp/adconvtyp
        w.put(1, 0); // sourcefscod
        match ext_a {
            None => {
                w.put(1, 0);
            }
            Some(a) => {
                w.put(1, 1).put(6, 1).put(7, 0).put(1, a as u32).put(8, 16);
            }
        }
        w.put(8, 0); // the audio blocks would follow
        w.0
    }

    /// The JOC flag is read from the bits FFmpeg's `eac3_parse_header` reads it from, through
    /// every conditional metadata field before it.
    #[test]
    fn an_atmos_eac3_frame_says_joc_and_a_plain_one_does_not() {
        assert_eq!(eac3_joc(&joc_frame(0, 0, Some(true))), Some(true));
        assert_eq!(eac3_joc(&joc_frame(0, 0, Some(false))), Some(false), "addbsi without the extension");
        assert_eq!(eac3_joc(&joc_frame(0, 0, None)), Some(false), "no addbsi at all");
        // A real E-AC-3 5.1 frame header from the host's FFmpeg encoder (`ffmpeg -f lavfi -i
        // sine=… -ac 6 -c:a eac3 -b:a 384k -f eac3`, first 32 bytes): plain DD+, no JOC.
        let real = [
            0x0b, 0x77, 0x02, 0xff, 0x3f, 0x87, 0xc0, 0x00, 0x20, 0x80, 0x20, 0x00, 0x02, 0x00, 0x40, 0x00, 0x03, 0xe0, 0x1f,
            0xba, 0x7e, 0x1e, 0x01, 0x01, 0x07, 0xe1, 0xe1, 0xf8, 0x78, 0x7e, 0x1e, 0x10,
        ];
        assert_eq!(eac3_joc(&real), Some(false));
        // A packet holding a dependent frame first: its own flag is not the answer, the
        // independent frame after it is (the frame size, 1536 bytes, is the step).
        let mut packet = joc_frame(1, 0, Some(false));
        packet.resize(1536, 0);
        packet.extend(joc_frame(0, 0, Some(true)));
        assert_eq!(eac3_joc(&packet), Some(true));
        let mut sub1 = joc_frame(0, 1, Some(true));
        sub1.resize(1536, 0);
        assert_eq!(eac3_joc(&sub1), None, "substream 1 alone answers nothing");
        // plain AC-3 (bsid 8), a truncated header, no sync word
        assert_eq!(eac3_joc(&[0x0b, 0x77, 0x12, 0x34, 0x40, 0x40, 0x00]), None);
        assert_eq!(eac3_joc(&joc_frame(0, 0, Some(true))[..8]), None);
        assert_eq!(eac3_joc(&[0u8; 32]), None);
    }

    /// The field case: a debrid link to a torrent's HDR10+ test chart, a 3000×1200 PNG, refused
    /// INVALIDDATA three times over ~7 s and read "cannot open the stream". Every not-a-video
    /// body is named; every real container's opening bytes are not.
    #[test]
    fn the_sniff_names_bodies_that_are_not_video_and_passes_every_container() {
        use NotVideo::*;
        let pad = |b: &[u8]| [b, &[0x5Au8; 64][..]].concat();
        let named: [(&[u8], NotVideo, &str); 14] = [
            (b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR", Image, "image/png"),
            (&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F'], Image, "image/jpeg"),
            (b"GIF89a\x01\0\x01\0", Image, "image/gif"),
            (b"RIFF\x24\0\0\0WEBPVP8 ", Image, "image/webp"),
            (b"PK\x03\x04\x14\0\0\0", Archive, "application/zip"),
            (b"Rar!\x1a\x07\x01\0", Archive, "application/vnd.rar"),
            (b"<!DOCTYPE html><html><body>503</body></html>", WebPage, "text/html"),
            (b"\r\n  <HTML><head><title>Wait</title>", WebPage, "text/html"),
            (b"\xEF\xBB\xBF<!doctype html>", WebPage, "text/html"),
            (b"<!-- cdn --><div>", WebPage, "text/html"),
            (b"{\"error\":\"not cached\"}", Document, "application/json"),
            (b"Release.Name.2160p\r\nVideo: HEVC\r\n", Document, "text/plain"),
            (b"1\n00:00:01,000 --> 00:00:02,000\nCaf\xC3\xA9", Document, "text/plain"),
            // a multi-byte character cut by the sniff window
            (b"Erkl\xC3", Document, "text/plain"),
        ];
        for (head, kind, label) in named {
            assert_eq!(sniff(head, None), Some((kind, label)), "{:?}", String::from_utf8_lossy(head));
        }
        let mut ts = vec![0u8; 400];
        ts[0] = 0x47;
        ts[188] = 0x47;
        let mut m2ts = vec![0u8; 400];
        m2ts[4] = 0x47;
        m2ts[196] = 0x47;
        let media: [Vec<u8>; 12] = [
            pad(&[0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42, 0x86, 0x81]),
            pad(b"\0\0\0\x20ftypisom"),
            pad(b"\0\0\0\x14ftypqt  "),
            pad(b"\0\0\x01\0moov"),
            pad(b"\0\0\0\x08wide"),
            ts,
            m2ts,
            pad(b"RIFF\x24\0\0\0AVI LIST"),
            pad(&[0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9]),
            pad(b"FLV\x01\x05\0\0\0\x09"),
            b"#EXTM3U\n#EXT-X-VERSION:3\n".to_vec(),
            // a lone TS packet, shorter than one stride
            [&[0x47u8, 0x40, 0x11, 0x10][..], &[0xFF; 100][..]].concat(),
        ];
        for head in &media {
            assert_eq!(sniff(head, None), None, "{:02x?}", &head[..8]);
            // the bytes win over a header that says otherwise
            assert_eq!(sniff(head, Some("image/png")), None, "{:02x?}", &head[..8]);
            assert_eq!(sniff(head, Some("text/html")), None, "{:02x?}", &head[..8]);
        }
        // unknown binary: the Content-Type decides, and it is what is logged
        let junk = [0u8, 0x13, 0x88, 0xFE, 0x01];
        assert_eq!(sniff(&junk, None), None);
        assert_eq!(sniff(&junk, Some("application/octet-stream")), None);
        assert_eq!(sniff(&junk, Some("video/x-matroska")), None);
        assert_eq!(sniff(&junk, Some("image/png")), Some((Image, "image/png")));
        assert_eq!(sniff(&junk, Some("application/zip")), Some((Archive, "application/zip")));
        assert_eq!(sniff(&junk, Some("text/html; charset=utf-8")), Some((WebPage, "text/html; charset=utf-8")));
        // text served as text/html is a page, whatever it opens with
        assert_eq!(sniff(b"Service unavailable", Some("Text/HTML")), Some((WebPage, "text/html")));
        assert_eq!(sniff(b"", None), None);
        assert_eq!(sniff(b"  \r\n", None), None);
    }

    #[test]
    fn an_unknown_rate_reads_as_zero() {
        assert_eq!(rate_of(AVRational { num: 24000, den: 1001 }), 24000.0 / 1001.0);
        for r in [(0, 1), (25, 0), (-1, 1), (0, 0)] {
            assert_eq!(rate_of(AVRational { num: r.0, den: r.1 }), 0.0, "{r:?}");
        }
    }

    /// With the ABI gate closed (no FFmpeg bound, as in the plain host suite) the probe refuses
    /// before it opens anything: an explicit failure, never a socket to nowhere.
    #[test]
    fn a_probe_without_ffmpeg_is_refused_before_any_io() {
        let _g = tv_base::testlock::serial();
        assert!(!abi_ok());
        let io = ProbeIo::new();
        assert_eq!(
            probe("http://127.0.0.1:1/film.mkv", &crate::headers::RequestHeaders::none(), &io),
            Err(ProbeFailure::Unavailable)
        );
        io.cancel();
        assert!(io.cancelled(), "cancel latches the lane every callback checks");
    }

    /// The probe reads a stream through the transport the demuxer will use: a playlist through
    /// the HLS client (a curl open would hand libavformat the playlist TEXT, the probe would fail,
    /// and every HLS start would be refused before its Load), TLS or a header-carrying stream
    /// through curl, anything else through the socket.
    #[test]
    fn the_probe_picks_the_demuxers_transport() {
        let none = crate::headers::RequestHeaders::none();
        let ua = crate::headers::RequestHeaders::from_pairs(&[("User-Agent", "Live/1")]).unwrap();
        assert_eq!(probe_transport("http://live.example/ch/1/index.m3u8", &none), ProbeTransport::Hls);
        assert_eq!(probe_transport("https://live.example/ch/1/index.m3u8?token=x", &ua), ProbeTransport::Hls);
        assert_eq!(probe_transport("https://cdn.example/film.mkv", &none), ProbeTransport::Curl);
        assert_eq!(probe_transport("http://cdn.example/film.mkv", &ua), ProbeTransport::Curl);
        assert_eq!(probe_transport("http://cdn.example/film.mkv", &none), ProbeTransport::Socket);
    }
}
