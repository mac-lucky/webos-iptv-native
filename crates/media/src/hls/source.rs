//! **The HLS client**: a master or media playlist URL in, ONE continuous byte stream out - the
//! initialization section (fMP4) and then every segment's bytes, decrypted, in media-sequence
//! order - read through the same `>0 / 0 / error` contract the demuxer's AVIO callbacks already
//! speak (`ff::read_cb`).
//!
//! It fetches through a [`Transport`] - the curl media plane ([`CurlTransport`], which applies
//! the stream's request headers to every request, playlists, keys and segments alike) in the
//! application, an in-memory double in the tests - and waits on a [`Clock`], which the tests fake.
//! One request at a time: a segment is streamed to the demuxer as it arrives, and the next is
//! requested when it is done. Nothing prefetches; the demuxer's own AU queues are the buffer.
//!
//! **Failures are explicit** ([`HlsError`]), and every wait is bounded:
//! - the first playlist load is retried once for a transient failure (a timeout, a reset, a 5xx)
//!   and never for a 4xx: a 404 playlist is an error at once;
//! - a segment (or key, or init section) that fails transiently is re-requested up to
//!   [`ATTEMPTS`] times, with the bytes already handed on skipped, so the stream never repeats a
//!   byte; a live segment that is gone (4xx, or retries spent) is skipped - at most
//!   [`MAX_SKIPS_IN_ROW`] in a row - since the live edge has moved on; a VOD one ends the stream
//!   with an error;
//! - a live playlist reload that fails is retried on the §6.3.4 cadence, at most
//!   [`MAX_RELOAD_FAILURES`] times in a row, and a live playlist that stops growing is declared
//!   dead ([`super::live::Schedule::stalled`]);
//! - a finite archive playlist followed past its end ([`HlsSource::follow_until`]) is asked for
//!   again on the same cadence, and its end becomes the stream's, without an error, once it stops
//!   growing or its reloads keep failing.
//!
//! Once a read has failed, every later read fails with the same error: the demuxer may retry a
//! read, and it must see the same answer.

use super::aes::{sequence_iv, Block, CbcDecryptor, CbcError};
use super::live::{self, Locate, Schedule};
use super::playlist::{self, Encryption, InitSection, MediaPlaylist, ParseError, Playlist, Segment};
use super::select::{self, NoPlayableVariant};
use super::timeline;
use crate::headers::RequestHeaders;
use std::collections::VecDeque;
use std::sync::Arc;

/// Requests of one segment, key or init section before it is given up on.
pub const ATTEMPTS: u32 = 3;
/// Consecutive live segments that may be skipped (gone, or failing) before the stream fails.
pub const MAX_SKIPS_IN_ROW: u32 = 3;
/// Consecutive failed reloads of a live media playlist before the stream fails.
pub const MAX_RELOAD_FAILURES: u32 = 4;
/// The largest playlist read. A live playlist is kilobytes; a VOD one of a long film with byte
/// ranges is well under a megabyte.
pub const PLAYLIST_CAP: usize = 4 << 20;
/// The largest initialization section read (an fMP4 `moov` is kilobytes).
pub const INIT_CAP: usize = 4 << 20;
/// The largest segment read, bytes: a 10 s segment at 80 Mbit/s is 100 MB, a live TV segment a
/// few MB. A body past it is an endless response, not a segment: dropped like a failed one.
pub const SEGMENT_CAP: u64 = 128 << 20;
/// Backoff between attempts, multiplied by the attempt number.
const RETRY_BACKOFF_MS: u64 = 500;
/// The longest a wait sleeps before it looks at the cancel flag again.
const WAIT_SLICE_MS: u64 = 50;

// ---- transport and clock ---------------------------------------------------------------------

/// What a request is for: the transport bounds a playlist's or key's whole body by a deadline,
/// and a segment's only by its low-speed guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Playlist,
    Key,
    Init,
    Segment,
}

/// Why a request produced no (complete) body. Carries codes, never a URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchError {
    /// The server answered with a status that is not a 2xx.
    Status(u16),
    /// No answer, or no complete body, in time.
    Timeout,
    /// Teardown.
    Aborted,
    /// A transport failure: the `CURLcode` (or `-1`).
    Transport(i32),
    /// The body ended before the byte range it was asked for.
    Truncated,
    /// The body is larger than anything of its kind can be.
    TooLarge,
    /// A byte range was asked for and the server answered from elsewhere.
    RangeIgnored,
    /// This device cannot make media requests at all (no libcurl multi).
    Unavailable,
    /// Local setup failed.
    Local,
}

impl FetchError {
    /// Worth asking again: a timeout, a dropped connection, a short body, a server error.
    pub fn transient(self) -> bool {
        match self {
            FetchError::Status(s) => s >= 500 || s == 408 || s == 429,
            FetchError::Timeout | FetchError::Transport(_) | FetchError::Truncated => true,
            _ => false,
        }
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Status(s) => write!(f, "HTTP {s}"),
            FetchError::Timeout => f.write_str("timed out"),
            FetchError::Aborted => f.write_str("aborted"),
            FetchError::Transport(rc) => write!(f, "transport failure rc={rc}"),
            FetchError::Truncated => f.write_str("the body ended early"),
            FetchError::TooLarge => f.write_str("the body is too large"),
            FetchError::RangeIgnored => f.write_str("the server ignored the byte range"),
            FetchError::Unavailable => f.write_str("media requests are unavailable on this device"),
            FetchError::Local => f.write_str("local setup failed"),
        }
    }
}

/// One response body.
pub trait Body {
    /// Bytes of the body: `Ok(0)` is its end.
    fn read(&mut self, dst: &mut [u8]) -> Result<usize, FetchError>;
    /// The URL the body was answered from, after redirects: what its relative URIs resolve against.
    fn effective_url(&self) -> String;
}

/// Where requests go.
pub trait Transport {
    /// `GET url` from byte `offset` (0: the whole resource, no `Range`).
    fn get(&mut self, url: &str, offset: u64, purpose: Purpose) -> Result<Box<dyn Body>, FetchError>;
    /// Teardown has been asked for: no new request may start, and a wait must end.
    fn cancelled(&self) -> bool;
}

/// Milliseconds on a monotonic clock, and a way to let them pass.
pub trait Clock {
    fn now_ms(&self) -> u64;
    fn sleep_ms(&self, ms: u64);
}

/// The process's monotonic clock.
pub struct SystemClock {
    epoch: std::time::Instant,
}

impl SystemClock {
    pub fn new() -> SystemClock {
        SystemClock { epoch: std::time::Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }
    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// DNS, TLS and the response headers of any one request must complete within this.
pub const OPEN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);
/// A playlist's, key's or init section's whole body must arrive within this.
pub const SMALL_BODY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// **The application's transport**: the curl media plane ([`crate::curlio`]), one
/// [`CurlSource`](crate::curlio::CurlSource) per request, each carrying the stream's
/// [`RequestHeaders`] on every request and every redirect hop.
///
/// Teardown: each request publishes its abort handle in curlio's registry
/// ([`CurlSource::reserve_open`](crate::curlio::CurlSource::reserve_open)) BEFORE `cancel` is
/// checked, the order `ff::demux` keeps, so `curlio::abort_active` either wakes the transfer or
/// the check refuses to start it.
pub struct CurlTransport {
    headers: RequestHeaders,
    /// [`Self::headers`] without the credential lines, for a URI on another origin.
    cross_origin: RequestHeaders,
    /// The stream's URL: the first one requested (its playlist). Each playlist URI is a chain of
    /// its own, so curlio's redirect rule cannot judge it; this does: the credential headers
    /// (`headers::CREDENTIAL`) go only to URIs on the stream's own origin.
    stream: Option<String>,
    cancel: Box<dyn Fn() -> bool>,
}

impl CurlTransport {
    pub fn new(headers: RequestHeaders, cancel: Box<dyn Fn() -> bool>) -> CurlTransport {
        CurlTransport { cross_origin: headers.without_credentials(), headers, stream: None, cancel }
    }

    /// Hand the transport to another thread's teardown signal (the probe's source moving to the
    /// demuxer: `ff::hls_handoff`).
    pub fn set_cancel(&mut self, cancel: Box<dyn Fn() -> bool>) {
        self.cancel = cancel;
    }
}

fn from_open(e: crate::curlio::OpenErr) -> FetchError {
    use crate::curlio::OpenErr as E;
    match e {
        E::Unavailable => FetchError::Unavailable,
        E::Aborted | E::Stopped => FetchError::Aborted,
        E::Deadline | E::Transport(28) => FetchError::Timeout,
        E::Transport(rc) => FetchError::Transport(rc),
        E::Multi(rc) => FetchError::Transport(-rc.abs()),
        E::Status(s) => FetchError::Status(u16::try_from(s).unwrap_or(0)),
        E::RangeIgnored => FetchError::RangeIgnored,
        E::Local => FetchError::Local,
    }
}

struct CurlBody {
    src: Box<crate::curlio::CurlSource>,
    deadline: Option<std::time::Instant>,
}

impl Body for CurlBody {
    fn read(&mut self, dst: &mut [u8]) -> Result<usize, FetchError> {
        let n = self.src.read_until(dst, self.deadline, &mut tv_base::checkpoint::NoCheckpoint);
        match n {
            n if n >= 0 => Ok(n as usize),
            crate::curlio::READ_DEADLINE => Err(FetchError::Timeout),
            _ => Err(match self.src.body_failure().map(|f| f.rc) {
                Some(28) => FetchError::Timeout,
                Some(rc) => FetchError::Transport(rc),
                None => FetchError::Transport(-1),
            }),
        }
    }
    fn effective_url(&self) -> String {
        self.src.effective_url()
    }
}

impl Transport for CurlTransport {
    fn get(&mut self, url: &str, offset: u64, purpose: Purpose) -> Result<Box<dyn Body>, FetchError> {
        if (self.cancel)() {
            return Err(FetchError::Aborted);
        }
        let reservation = crate::curlio::CurlSource::reserve_open().map_err(from_open)?;
        if (self.cancel)() {
            return Err(FetchError::Aborted);
        }
        let at = i64::try_from(offset).map_err(|_| FetchError::Local)?;
        let open_by = std::time::Instant::now() + OPEN_DEADLINE;
        let stream = self.stream.get_or_insert_with(|| url.to_owned());
        let headers = if crate::curlio::same_origin_urls(stream, url) { &self.headers } else { &self.cross_origin };
        let src = crate::curlio::CurlSource::open_reserved_until(url, at, headers, reservation, open_by)
            .map_err(from_open)?;
        let deadline = match purpose {
            Purpose::Segment => None,
            Purpose::Playlist | Purpose::Key | Purpose::Init => Some(std::time::Instant::now() + SMALL_BODY_DEADLINE),
        };
        Ok(Box::new(CurlBody { src, deadline }))
    }
    fn cancelled(&self) -> bool {
        (self.cancel)()
    }
}

// ---- errors ----------------------------------------------------------------------------------

/// Why an HLS stream cannot be (or can no longer be) played. Codes and counts, never a URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HlsError {
    /// The master or media playlist could not be fetched.
    Playlist(FetchError),
    /// It was fetched and is not a playlist this client can read.
    Parse(ParseError),
    /// A variant's URI answered with another master playlist.
    NestedMaster,
    /// No variant of the master is playable on this television.
    NoPlayableVariant(NoPlayableVariant),
    /// A VOD playlist with no segment.
    Empty,
    /// Encryption this client cannot decrypt (`SAMPLE-AES`, a DRM key format).
    Unsupported(String),
    /// The AES-128 key could not be fetched.
    Key(FetchError),
    /// The key is not 16 bytes.
    KeyLength(usize),
    /// A segment or init section did not decrypt (a wrong key or IV, or corruption).
    Decrypt(CbcError),
    /// The initialization section could not be fetched.
    Init(FetchError),
    /// A segment could not be fetched, attempts spent.
    Segment { sequence: u64, error: FetchError },
    /// The live playlist has added no segment for this long.
    LiveStalled { since_ms: u64 },
    /// The request was redirected to an archive server's "no archive" stub
    /// ([`super::is_no_archive_stub`]): nothing is recorded for that time.
    NoArchive,
    /// The URL is not `http` or `https`.
    Scheme,
    /// An https playlist names a plaintext (http) URI: a variant, key, init section or segment
    /// anyone on the path could read or replace, which a redirect may never do either.
    Insecure,
    /// Teardown.
    Aborted,
}

impl HlsError {
    /// The HTTP status behind the failure, for the diagnostics read-out.
    pub fn http_status(&self) -> Option<u16> {
        match self {
            HlsError::Playlist(FetchError::Status(s))
            | HlsError::Key(FetchError::Status(s))
            | HlsError::Init(FetchError::Status(s))
            | HlsError::Segment { error: FetchError::Status(s), .. } => Some(*s),
            _ => None,
        }
    }
}

impl std::fmt::Display for HlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HlsError::Playlist(e) => write!(f, "the playlist could not be loaded: {e}"),
            HlsError::Parse(e) => write!(f, "the playlist is unreadable: {e}"),
            HlsError::NestedMaster => f.write_str("a variant playlist is itself a master playlist"),
            HlsError::NoPlayableVariant(n) => {
                write!(f, "none of the {} variants is playable on this TV", n.variants)?;
                if let Some(why) = &n.first_reason {
                    write!(f, " (first: {why:?})")?;
                }
                Ok(())
            }
            HlsError::Empty => f.write_str("the playlist lists no segment"),
            HlsError::Unsupported(m) => write!(f, "unsupported encryption ({m})"),
            HlsError::Key(e) => write!(f, "the decryption key could not be loaded: {e}"),
            HlsError::KeyLength(n) => write!(f, "the decryption key is {n} bytes, not 16"),
            HlsError::Decrypt(e) => write!(f, "a segment did not decrypt: {e:?}"),
            HlsError::Init(e) => write!(f, "the initialization section could not be loaded: {e}"),
            HlsError::Segment { sequence, error } => write!(f, "segment {sequence} could not be loaded: {error}"),
            HlsError::LiveStalled { since_ms } => write!(f, "the live playlist has not grown for {} s", since_ms / 1000),
            HlsError::NoArchive => f.write_str("the archive has no recording for that time"),
            HlsError::Scheme => f.write_str("not an http or https URL"),
            HlsError::Insecure => f.write_str("an https playlist names a plaintext URI"),
            HlsError::Aborted => f.write_str("aborted"),
        }
    }
}

// ---- the source ------------------------------------------------------------------------------

/// What the segments are, as the demuxer will find them. Informational: the demuxer probes the
/// bytes (mpegts and mov are both in the bundled FFmpeg), so this only names the stream in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    MpegTs,
    Fmp4,
}

/// A summary of the chosen variant, for the log.
#[derive(Clone, Debug, PartialEq)]
pub struct VariantInfo {
    pub index: usize,
    pub of: usize,
    pub bandwidth: u64,
    pub resolution: Option<(u32, u32)>,
    pub codecs: Vec<String>,
}

/// One segment being streamed.
struct Active {
    seg: Segment,
    body: Option<Box<dyn Body>>,
    /// Bytes of the byte range still to read (`None`: to the end of the body).
    remaining: Option<u64>,
    dec: Option<CbcDecryptor>,
    key: Option<(Block, Block)>,
    /// Plaintext bytes of this segment already handed on, across attempts.
    emitted: u64,
    /// Bytes of the current attempt's body read so far, against [`SEGMENT_CAP`].
    fetched: u64,
    /// Plaintext bytes of the current attempt still to drop (a retry re-reads from the start).
    skip: u64,
    attempt: u32,
}

/// What [`HlsSource::step`] did.
enum Step {
    /// Something changed; look again.
    Progress,
    /// The stream is over (a VOD or ended live playlist, fully delivered).
    End,
}

/// The client. `T` fetches, `C` tells time.
pub struct HlsSource<T: Transport, C: Clock> {
    t: T,
    clock: C,
    /// The media playlist's URL as first requested: every reload requests it again (a redirect's
    /// target may be presigned and expire; the server that redirected can issue the next).
    media_url: String,
    playlist: MediaPlaylist,
    last_text: String,
    schedule: Schedule,
    /// The media sequence number of the next segment to start.
    next_seq: u64,
    out: Vec<u8>,
    out_pos: usize,
    scratch: Vec<u8>,
    active: Option<Active>,
    /// The initialization section last put on the stream.
    map_now: Option<Arc<InitSection>>,
    keys: Vec<(String, Block)>,
    reload_failures: u32,
    skips_in_row: u32,
    failed: Option<HlsError>,
    live: bool,
    container: Container,
    variant: Option<VariantInfo>,
    start_us: u64,
    delivered: u64,
    /// Every byte delivered, while [`Self::record`] is on: the probe's reads, replayed to the
    /// demuxer so it does not fetch the same segments again. None when off or past [`TAPE_CAP`].
    tape: Option<Vec<u8>>,
    /// [`SEGMENT_CAP`]; a field so a test can lower it.
    segment_cap: u64,
    /// The stream is read in playlist time from [`Self::start_us`] (VOD, or an EVENT playlist
    /// opened at a position).
    playlist_time: bool,
    /// The end was logged (the demuxer asks again after it).
    end_logged: bool,
    /// Where each run of segments between two discontinuities begins in the delivered stream,
    /// `(byte offset, run)`, oldest first: the last [`timeline::RUNS_KEPT`] (see [`Self::run_at`]).
    runs: VecDeque<(u64, u64)>,
    /// The discontinuity sequence of the segment begun last.
    dseq_now: Option<u64>,
    /// A reload found the media sequence gone backwards: the next segment begun begins a run.
    restarted: bool,
    /// A finite archive playlist is followed past its end up to this playlist time (see
    /// [`Self::follow_until`]); `None` once its end is the stream's.
    follow_us: Option<u64>,
    /// The end was reached once and the playlist is being asked for again.
    following: bool,
    /// A reload grew the finite playlist since [`Self::take_grown`] was last asked.
    grown: bool,
}

/// The most a recording keeps; a probe that read more is not replayed.
const TAPE_CAP: usize = 48 * 1024 * 1024;

/// Read a whole body: `len` bytes when given (a byte range), else to its end, at most `cap`.
fn read_body(body: &mut dyn Body, len: Option<u64>, cap: usize) -> Result<Vec<u8>, FetchError> {
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let want = match len {
            Some(l) => {
                let left = l.saturating_sub(out.len() as u64);
                if left == 0 {
                    return Ok(out);
                }
                left.min(buf.len() as u64) as usize
            }
            None => buf.len(),
        };
        let n = body.read(&mut buf[..want])?;
        if n == 0 {
            return match len {
                Some(_) => Err(FetchError::Truncated),
                None => Ok(out),
            };
        }
        if out.len() + n > cap {
            return Err(FetchError::TooLarge);
        }
        out.extend_from_slice(&buf[..n]);
    }
}

impl<T: Transport, C: Clock> HlsSource<T, C> {
    /// Load `url` (a master or a media playlist), choose a variant against `caps`, and position the
    /// stream: a live one [`live::START_TARGETS`] target durations behind its edge, a VOD one (and
    /// a growing EVENT one asked for a position) at the segment containing `start_us` of playlist
    /// time (the beginning when `None`).
    pub fn open(
        mut t: T,
        clock: C,
        url: &str,
        start_us: Option<u64>,
        caps: &tv_platform::devcaps::Caps,
    ) -> Result<HlsSource<T, C>, HlsError> {
        if !crate::curlio::media_url_allowed(url) {
            return Err(HlsError::Scheme);
        }
        let mut began = clock.now_ms();
        let (mut text, mut effective) = Self::load_playlist(&mut t, &clock, url)?;
        if super::is_no_archive_stub(url, &effective) {
            tv_base::eventlog::log("hls: redirected to the no-archive stub");
            return Err(HlsError::NoArchive);
        }
        let mut media_url = url.to_owned();
        let mut variant = None;
        let playlist = match parse_secure(&text, &effective)? {
            Playlist::Media(m) => m,
            Playlist::Master(master) => {
                let i = select::select(&master, caps).map_err(HlsError::NoPlayableVariant)?;
                let v = &master.variants[i];
                variant = Some(VariantInfo {
                    index: i,
                    of: master.variants.len(),
                    bandwidth: v.bandwidth,
                    resolution: v.resolution,
                    codecs: v.codecs.clone(),
                });
                if select::needs_separate_audio(v, &master) {
                    tv_base::eventlog::log(
                        "hls: the chosen variant's audio is a separate rendition, which one byte stream cannot carry; playing its own segments only",
                    );
                }
                media_url = v.uri.clone();
                began = clock.now_ms();
                (text, effective) = Self::load_playlist(&mut t, &clock, &media_url)?;
                match parse_secure(&text, &effective)? {
                    Playlist::Media(m) => m,
                    Playlist::Master(_) => return Err(HlsError::NestedMaster),
                }
            }
        };
        let live = playlist.is_live();
        if !live && playlist.segments.is_empty() {
            return Err(HlsError::Empty);
        }
        // An EVENT playlist that is still growing (an archive request for the programme on air)
        // keeps every segment from its start: asked for a position, it is played from there and
        // read in playlist time, while it is reloaded like a live one. Asked for none (the
        // probe), it starts at its edge like any live stream.
        let from_position = !live || (playlist.playlist_type == Some(playlist::PlaylistType::Event) && start_us.is_some());
        let index = if from_position {
            start_us.map_or(0, |at| live::vod_index_at(&playlist, at))
        } else {
            live::live_start_index(&playlist)
        };
        let container = match playlist.segments.get(index) {
            Some(s) if s.map.is_some() => Container::Fmp4,
            Some(s) => {
                let path = s.uri.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
                if [".mp4", ".m4s", ".m4v", ".cmfv", ".m4a"].iter().any(|e| path.ends_with(e)) {
                    Container::Fmp4
                } else {
                    Container::MpegTs
                }
            }
            None => Container::MpegTs,
        };
        let src = HlsSource {
            schedule: Schedule::first(&playlist, began),
            next_seq: playlist.media_sequence + index as u64,
            start_us: if from_position { live::start_of(&playlist, index) } else { 0 },
            playlist_time: from_position,
            end_logged: false,
            runs: VecDeque::new(),
            dseq_now: None,
            restarted: false,
            follow_us: None,
            following: false,
            grown: false,
            t,
            clock,
            media_url,
            last_text: text,
            out: Vec::new(),
            out_pos: 0,
            scratch: vec![0; 64 * 1024],
            active: None,
            map_now: None,
            keys: Vec::new(),
            reload_failures: 0,
            skips_in_row: 0,
            failed: None,
            live,
            container,
            variant,
            delivered: 0,
            tape: None,
            segment_cap: SEGMENT_CAP,
            playlist,
        };
        tv_base::eventlog::log(&format!(
            "hls: {} playlist, {} segments, target {} ms, starting at sequence {}{}{}",
            if live { "live" } else { "VOD" },
            src.playlist.segments.len(),
            src.playlist.target_duration_us / 1000,
            src.next_seq,
            match &src.variant {
                Some(v) => format!(
                    ", variant {}/{} ({} bit/s{}{})",
                    v.index + 1,
                    v.of,
                    v.bandwidth,
                    v.resolution.map_or(String::new(), |(w, h)| format!(", {w}x{h}")),
                    if v.codecs.is_empty() { String::new() } else { format!(", {}", v.codecs.join(",")) }
                ),
                None => String::new(),
            },
            match src.container {
                Container::MpegTs => ", MPEG-TS",
                Container::Fmp4 => ", fMP4",
            },
        ));
        Ok(src)
    }

    /// The first load of a playlist: one retry for a transient failure, none for a 4xx.
    fn load_playlist(t: &mut T, clock: &C, url: &str) -> Result<(String, String), HlsError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match Self::fetch_text(t, url) {
                Ok(ok) => return Ok(ok),
                Err(FetchError::Aborted) => return Err(HlsError::Aborted),
                Err(e) if e.transient() && attempt < 2 => {
                    tv_base::eventlog::log(&format!("hls: playlist load failed ({e}); retrying once"));
                    Self::wait(t, clock, clock.now_ms() + RETRY_BACKOFF_MS)?;
                }
                Err(e) => {
                    tv_base::eventlog::log(&format!("hls: playlist load failed ({e})"));
                    return Err(HlsError::Playlist(e));
                }
            }
        }
    }

    fn fetch_text(t: &mut T, url: &str) -> Result<(String, String), FetchError> {
        let mut body = t.get(url, 0, Purpose::Playlist)?;
        let bytes = read_body(body.as_mut(), None, PLAYLIST_CAP)?;
        // A playlist is UTF-8 (RFC 8216 §4.1); a stray byte is replaced, not fatal.
        Ok((String::from_utf8_lossy(&bytes).into_owned(), body.effective_url()))
    }

    /// Sleep until `due_ms`, in slices, ending early (with [`HlsError::Aborted`]) on teardown.
    fn wait(t: &T, clock: &C, due_ms: u64) -> Result<(), HlsError> {
        loop {
            if t.cancelled() {
                return Err(HlsError::Aborted);
            }
            let now = clock.now_ms();
            if now >= due_ms {
                return Ok(());
            }
            clock.sleep_ms((due_ms - now).min(WAIT_SLICE_MS));
        }
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    /// The VOD playlist's whole duration, microseconds; 0 for a live one.
    pub fn duration_us(&self) -> u64 {
        if self.live {
            0
        } else {
            self.playlist.duration_us()
        }
    }

    /// **Follow a finite archive playlist past its end**, up to `until_us` of its playlist time.
    /// An archive request for the programme on air (Flussonic's shift answer, `EVENT` +
    /// `ENDLIST`) ends where the archive did when it was asked, a time shift's delay behind live.
    /// Instead of ending the stream there, the same URL is asked for again on the live cadence and
    /// the segments it gained play on, so a time shift is one stream rather than one Load per
    /// delay. The stream ends as before once the playlist reaches `until_us`, stops growing
    /// ([`Schedule::follow_limit_ms`]), answers another window (or one without its end), or keeps
    /// failing to load; the playback's part logic takes over from there. No effect on a live
    /// playlist.
    pub fn follow_until(&mut self, until_us: u64) {
        if !self.live {
            self.follow_us = Some(until_us);
        }
    }

    /// The finite playlist's duration, microseconds, once after each reload that grew it.
    pub fn take_grown(&mut self) -> Option<u64> {
        std::mem::take(&mut self.grown).then(|| self.playlist.duration_us())
    }

    /// Where in the VOD playlist's time the stream begins (the start of the segment holding the
    /// requested position), microseconds; 0 for a live one.
    pub fn start_us(&self) -> u64 {
        self.start_us
    }

    /// Positions in this stream are playlist time from [`Self::start_us`]: a VOD playlist, or an
    /// EVENT playlist opened at a position. A live one has only its segments' timestamps.
    pub fn in_playlist_time(&self) -> bool {
        self.playlist_time
    }

    pub fn container(&self) -> Container {
        self.container
    }

    pub fn variant(&self) -> Option<&VariantInfo> {
        self.variant.as_ref()
    }

    /// Bytes handed to the reader so far.
    /// Start recording what [`Self::read`] delivers.
    pub fn record(&mut self) {
        self.tape = Some(Vec::new());
    }

    /// Stop recording: every byte delivered since [`Self::record`], or None when it overflowed.
    pub fn take_tape(&mut self) -> Option<Vec<u8>> {
        self.tape.take()
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.t
    }

    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// The run of segments between two discontinuities (0 at the stream's start, counting up)
    /// that delivered stream byte `pos`: the newest for a byte not delivered yet, the oldest
    /// remembered for one before it. The demuxer's packet positions are these byte offsets.
    pub fn run_at(&self, pos: u64) -> u64 {
        timeline::newest_at(&self.runs, pos).unwrap_or(0)
    }

    #[cfg(test)]
    fn set_segment_cap(&mut self, cap: u64) {
        self.segment_cap = cap;
    }

    /// Deliver up to `dst.len()` bytes of the stream: `Ok(0)` at its end (a VOD or ended live
    /// playlist), an error once and for every read after.
    pub fn read(&mut self, dst: &mut [u8]) -> Result<usize, HlsError> {
        loop {
            if let Some(e) = &self.failed {
                return Err(e.clone());
            }
            if self.t.cancelled() {
                self.failed = Some(HlsError::Aborted);
                return Err(HlsError::Aborted);
            }
            if dst.is_empty() {
                return Ok(0);
            }
            if self.out_pos < self.out.len() {
                let n = dst.len().min(self.out.len() - self.out_pos);
                dst[..n].copy_from_slice(&self.out[self.out_pos..self.out_pos + n]);
                self.out_pos += n;
                self.delivered += n as u64;
                if let Some(tape) = &mut self.tape {
                    if tape.len() + n > TAPE_CAP {
                        self.tape = None;
                    } else {
                        tape.extend_from_slice(&dst[..n]);
                    }
                }
                return Ok(n);
            }
            self.out.clear();
            self.out_pos = 0;
            match self.step() {
                Ok(Step::Progress) => {}
                Ok(Step::End) => {
                    if !std::mem::replace(&mut self.end_logged, true) {
                        tv_base::eventlog::log(&format!("hls: end of stream after {} bytes", self.delivered));
                    }
                    return Ok(0);
                }
                Err(e) => {
                    if e != HlsError::Aborted {
                        tv_base::eventlog::log(&format!("hls: stream failed — {e}"));
                    }
                    self.failed = Some(e.clone());
                    return Err(e);
                }
            }
        }
    }

    /// Advance the stream by one action: read from the active segment, or start the next one, or
    /// wait for and load the next playlist.
    fn step(&mut self) -> Result<Step, HlsError> {
        if self.active.is_some() {
            return self.pump();
        }
        match live::locate(&self.playlist, self.next_seq) {
            Locate::At(i) => {
                let seg = self.playlist.segments[i].clone();
                self.begin(seg)?;
                Ok(Step::Progress)
            }
            Locate::Behind { skipped } => {
                tv_base::eventlog::log(&format!(
                    "hls: fell behind the live window; {skipped} segment(s) expired unplayed, jumping to sequence {}",
                    self.playlist.media_sequence
                ));
                self.next_seq = self.playlist.media_sequence;
                Ok(Step::Progress)
            }
            Locate::Ahead if !self.playlist.is_live() => Ok(if self.follow()? { Step::Progress } else { Step::End }),
            Locate::Ahead => {
                // A reload that is already due runs before anything is judged: a client that
                // spent a while on one slow segment has not seen the playlist grow since.
                let now = self.clock.now_ms();
                let due = self.schedule.reload_due_ms();
                if now < due {
                    if self.schedule.stalled(now) {
                        return Err(HlsError::LiveStalled { since_ms: self.schedule.since_growth_ms(now) });
                    }
                    Self::wait(&self.t, &self.clock, due)?;
                }
                self.reload()?;
                Ok(Step::Progress)
            }
        }
    }

    /// Load the media playlist again: its text and what it says.
    fn load_media(&mut self) -> Result<(String, MediaPlaylist), HlsError> {
        let (text, eff) = Self::fetch_text(&mut self.t, &self.media_url).map_err(HlsError::Playlist)?;
        match parse_secure(&text, &eff)? {
            Playlist::Media(m) => Ok((text, m)),
            Playlist::Master(_) => Err(HlsError::NestedMaster),
        }
    }

    /// The finite playlist is played to its end: ask for it again when it is followed
    /// ([`Self::follow_until`]). `false`: its end is the stream's.
    fn follow(&mut self) -> Result<bool, HlsError> {
        let Some(until_us) = self.follow_us else { return Ok(false) };
        let now = self.clock.now_ms();
        let have_us = self.playlist.duration_us();
        if !std::mem::replace(&mut self.following, true) {
            self.schedule.watch_growth_from(now);
            if have_us < until_us {
                tv_base::eventlog::log(&format!(
                    "hls: the archive ends at {} s of the {} s wanted; following its edge",
                    have_us / 1_000_000,
                    until_us / 1_000_000
                ));
            }
        }
        // As on live, a reload that is already due runs before growth is judged: a reader held back
        // by full queues takes as long to read what a reload added as the archive took to add it.
        let due = self.schedule.reload_due_ms();
        let stop = if have_us >= until_us {
            Some("reached the end wanted".to_owned())
        } else if now < due && self.schedule.since_growth_ms(now) >= self.schedule.follow_limit_ms() {
            Some(format!("has not grown for {} s", self.schedule.since_growth_ms(now) / 1000))
        } else {
            None
        };
        if let Some(why) = stop {
            tv_base::eventlog::log(&format!("hls: the archive {why}; the stream ends at {} s", have_us / 1_000_000));
            self.follow_us = None;
            return Ok(false);
        }
        Self::wait(&self.t, &self.clock, due)?;
        let began = self.clock.now_ms();
        match self.load_media() {
            // An answer without its end is another window too: followed, it would play as live,
            // past the end wanted.
            Ok((_, m)) if m.is_live() || !extends(&self.playlist, &m) => {
                tv_base::eventlog::log("hls: the archive answered another window; the stream ends where the first one did");
                self.follow_us = None;
                Ok(false)
            }
            Ok((text, m)) => {
                self.grown |= m.segments.len() > self.playlist.segments.len();
                self.adopt(text, m, began);
                Ok(true)
            }
            Err(HlsError::Playlist(FetchError::Aborted)) => Err(HlsError::Aborted),
            // Not a transient answer: what was played stays, nothing more is asked for.
            Err(HlsError::Insecure) => {
                self.follow_us = None;
                Ok(false)
            }
            Err(e) => {
                let spent = self.reload_failed(&e, began, "archive");
                if spent {
                    self.follow_us = None;
                }
                Ok(!spent)
            }
        }
    }

    /// Reload the live media playlist.
    fn reload(&mut self) -> Result<(), HlsError> {
        let began = self.clock.now_ms();
        match self.load_media() {
            Ok((text, m)) => {
                if m.media_sequence < self.playlist.media_sequence {
                    tv_base::eventlog::log(&format!(
                        "hls: the media sequence went backwards ({} -> {}); following the new window",
                        self.playlist.media_sequence, m.media_sequence
                    ));
                    self.next_seq = m.media_sequence + live::live_start_index(&m) as u64;
                    // A packager that restarted numbers its discontinuities and timestamps afresh
                    // too, with no tag to say so.
                    self.restarted = true;
                }
                if !m.is_live() {
                    tv_base::eventlog::log("hls: the live playlist ended (EXT-X-ENDLIST)");
                }
                self.adopt(text, m, began);
                Ok(())
            }
            Err(HlsError::Playlist(FetchError::Aborted)) => Err(HlsError::Aborted),
            // Not a transient answer: the playlist itself turned unsafe.
            Err(HlsError::Insecure) => Err(HlsError::Insecure),
            Err(e) => {
                if self.reload_failed(&e, began, "playlist") {
                    return Err(e);
                }
                Ok(())
            }
        }
    }

    /// Take `m`, answered with `text` by a reload that began at `began_ms`, as the playlist.
    fn adopt(&mut self, text: String, m: MediaPlaylist, began_ms: u64) {
        self.schedule.reloaded(&m, began_ms, text != self.last_text);
        self.playlist = m;
        self.last_text = text;
        self.reload_failures = 0;
    }

    /// Count a reload of the `what` playlist that began at `began_ms` and failed with `e`; it is
    /// retried on the schedule's cadence. `true` once [`MAX_RELOAD_FAILURES`] failed in a row.
    fn reload_failed(&mut self, e: &HlsError, began_ms: u64, what: &str) -> bool {
        self.reload_failures += 1;
        self.schedule.failed(began_ms);
        tv_base::eventlog::log(&format!("hls: {what} reload failed ({e}), {} of {MAX_RELOAD_FAILURES}", self.reload_failures));
        self.reload_failures >= MAX_RELOAD_FAILURES
    }

    /// The AES-128 key at `uri`, fetched once per URI.
    fn key(&mut self, uri: &str) -> Result<Block, HlsError> {
        if let Some((_, k)) = self.keys.iter().find(|(u, _)| u == uri) {
            return Ok(*k);
        }
        let mut attempt = 0;
        let bytes = loop {
            attempt += 1;
            let got = self.t.get(uri, 0, Purpose::Key).and_then(|mut b| read_body(b.as_mut(), None, 64));
            match got {
                Ok(b) => break b,
                Err(FetchError::Aborted) => return Err(HlsError::Aborted),
                Err(e) if e.transient() && attempt < ATTEMPTS => {
                    tv_base::eventlog::log(&format!("hls: key load failed ({e}); attempt {attempt} of {ATTEMPTS}"));
                    Self::wait(&self.t, &self.clock, self.clock.now_ms() + RETRY_BACKOFF_MS * u64::from(attempt))?;
                }
                Err(FetchError::TooLarge) => return Err(HlsError::KeyLength(65)),
                Err(e) => return Err(HlsError::Key(e)),
            }
        };
        let key: Block = bytes.as_slice().try_into().map_err(|_| HlsError::KeyLength(bytes.len()))?;
        // Keys rotate on a live stream; a handful is all one playback ever needs at once.
        if self.keys.len() >= 8 {
            self.keys.remove(0);
        }
        self.keys.push((uri.to_owned(), key));
        Ok(key)
    }

    /// `(key, iv)` for content encrypted by `e`, `sequence` supplying an IV the tag did not.
    fn cipher_for(&mut self, e: &Encryption, sequence: u64) -> Result<Option<(Block, Block)>, HlsError> {
        match e {
            Encryption::Clear => Ok(None),
            Encryption::Unsupported(m) => Err(HlsError::Unsupported(m.clone())),
            Encryption::Aes128 { uri, iv } => {
                let key = self.key(uri)?;
                Ok(Some((key, iv.unwrap_or_else(|| sequence_iv(sequence)))))
            }
        }
    }

    /// Start segment `seg`: put its initialization section on the stream first when it differs
    /// from the one already there, and make it the active segment.
    fn begin(&mut self, seg: Segment) -> Result<(), HlsError> {
        if seg.gap {
            tv_base::eventlog::log(&format!("hls: segment {} is a gap; skipped", seg.sequence));
            self.next_seq = seg.sequence + 1;
            return Ok(());
        }
        if seg.discontinuity {
            tv_base::eventlog::log(&format!(
                "hls: discontinuity before segment {} (discontinuity sequence {})",
                seg.sequence, seg.discontinuity_sequence
            ));
        }
        // A new run begins at a tagged segment, and where the discontinuity sequence goes up past a
        // tag not played (on a gap, or expired), at the next byte delivered: `read` steps only once
        // `out` is drained. Not where it goes down: a live playlist without
        // `EXT-X-DISCONTINUITY-SEQUENCE` counts its tags from its first segment, so the count drops
        // as a tag leaves the window. And at the first segment after the media sequence went
        // backwards (a packager restart, [`Self::reload`]).
        let restarted = std::mem::take(&mut self.restarted);
        let new_run = self.runs.is_empty()
            || seg.discontinuity
            || restarted
            || self.dseq_now.is_some_and(|d| seg.discontinuity_sequence > d);
        self.dseq_now = Some(seg.discontinuity_sequence);
        if new_run {
            let run = self.runs.back().map_or(0, |&(_, run)| run + 1);
            timeline::remember(&mut self.runs, (self.delivered, run));
        }
        let key = self.cipher_for(&seg.key, seg.sequence)?;
        if seg.map != self.map_now {
            if let Some(map) = &seg.map {
                let init = self.init_section(map, seg.sequence)?;
                tv_base::eventlog::log(&format!("hls: initialization section, {} bytes", init.len()));
                self.out.extend_from_slice(&init);
            } else {
                tv_base::eventlog::log("hls: the stream left its initialization section behind (fMP4 to MPEG-TS)");
            }
            self.map_now = seg.map.clone();
        }
        self.active = Some(Active {
            remaining: seg.range.map(|r| r.length),
            dec: key.map(|(k, iv)| CbcDecryptor::new(&k, &iv)),
            key,
            seg,
            body: None,
            emitted: 0,
            fetched: 0,
            skip: 0,
            attempt: 0,
        });
        Ok(())
    }

    /// The bytes of initialization section `map`, decrypted.
    fn init_section(&mut self, map: &InitSection, sequence: u64) -> Result<Vec<u8>, HlsError> {
        let key = self.cipher_for(&map.key, sequence)?;
        let mut attempt = 0;
        let raw = loop {
            attempt += 1;
            let got = self
                .t
                .get(&map.uri, map.range.map_or(0, |r| r.offset), Purpose::Init)
                .and_then(|mut b| read_body(b.as_mut(), map.range.map(|r| r.length), INIT_CAP));
            match got {
                Ok(b) => break b,
                Err(FetchError::Aborted) => return Err(HlsError::Aborted),
                Err(e) if e.transient() && attempt < ATTEMPTS => {
                    tv_base::eventlog::log(&format!("hls: init section load failed ({e}); attempt {attempt} of {ATTEMPTS}"));
                    Self::wait(&self.t, &self.clock, self.clock.now_ms() + RETRY_BACKOFF_MS * u64::from(attempt))?;
                }
                Err(e) => return Err(HlsError::Init(e)),
            }
        };
        match key {
            None => Ok(raw),
            Some((k, iv)) => {
                let mut d = CbcDecryptor::new(&k, &iv);
                let mut out = Vec::with_capacity(raw.len());
                d.update(&raw, &mut out);
                d.finish(&mut out).map_err(HlsError::Decrypt)?;
                Ok(out)
            }
        }
    }

    /// Move bytes of the active segment onto the stream, or finish it, or handle its failure.
    fn pump(&mut self) -> Result<Step, HlsError> {
        let a = self.active.as_mut().expect("pump runs with an active segment");
        if a.body.is_none() {
            let offset = a.seg.range.map_or(0, |r| r.offset);
            match self.t.get(&a.seg.uri, offset, Purpose::Segment) {
                Ok(b) => a.body = Some(b),
                Err(e) => return self.segment_failed(e),
            }
            return Ok(Step::Progress);
        }
        let want = a.remaining.map_or(self.scratch.len(), |r| r.min(self.scratch.len() as u64) as usize);
        let read = if want == 0 {
            Ok(0)
        } else {
            a.body.as_mut().expect("checked above").read(&mut self.scratch[..want])
        };
        match read {
            Ok(0) => {
                if a.remaining.is_some_and(|r| r > 0) {
                    return self.segment_failed(FetchError::Truncated);
                }
                let mut tail = Vec::new();
                if let Some(d) = a.dec.take() {
                    d.finish(&mut tail).map_err(HlsError::Decrypt)?;
                }
                Self::emit(a, &tail, &mut self.out);
                let done = self.active.take().expect("active");
                self.next_seq = done.seg.sequence + 1;
                self.skips_in_row = 0;
                Ok(Step::Progress)
            }
            Ok(n) => {
                if let Some(r) = a.remaining.as_mut() {
                    *r -= n as u64;
                }
                a.fetched += n as u64;
                if a.fetched > self.segment_cap {
                    return self.segment_failed(FetchError::TooLarge);
                }
                let raw = &self.scratch[..n];
                match a.dec.as_mut() {
                    Some(d) => {
                        let mut plain = Vec::with_capacity(n);
                        d.update(raw, &mut plain);
                        Self::emit(a, &plain, &mut self.out);
                    }
                    None => Self::emit(a, raw, &mut self.out),
                }
                Ok(Step::Progress)
            }
            Err(e) => self.segment_failed(e),
        }
    }

    /// Hand on `plain`, minus whatever a retry must not repeat.
    fn emit(a: &mut Active, plain: &[u8], out: &mut Vec<u8>) {
        let drop = a.skip.min(plain.len() as u64) as usize;
        a.skip -= drop as u64;
        out.extend_from_slice(&plain[drop..]);
        a.emitted += (plain.len() - drop) as u64;
    }

    /// The active segment's request failed with `e`: retry it, skip it (live), or fail.
    fn segment_failed(&mut self, e: FetchError) -> Result<Step, HlsError> {
        if e == FetchError::Aborted || self.t.cancelled() {
            return Err(HlsError::Aborted);
        }
        let live = self.playlist.is_live();
        let a = self.active.as_mut().expect("a failure of the active segment");
        a.attempt += 1;
        let sequence = a.seg.sequence;
        if e.transient() && a.attempt < ATTEMPTS {
            tv_base::eventlog::log(&format!(
                "hls: segment {sequence} failed ({e}) after {} bytes; attempt {} of {ATTEMPTS}",
                a.emitted, a.attempt
            ));
            // Start the segment over; the bytes already handed on are dropped from the re-read.
            a.body = None;
            a.fetched = 0;
            a.remaining = a.seg.range.map(|r| r.length);
            a.dec = a.key.map(|(k, iv)| CbcDecryptor::new(&k, &iv));
            a.skip = a.emitted;
            let due = self.clock.now_ms() + RETRY_BACKOFF_MS * u64::from(a.attempt);
            Self::wait(&self.t, &self.clock, due)?;
            return Ok(Step::Progress);
        }
        if live && self.skips_in_row < MAX_SKIPS_IN_ROW {
            self.skips_in_row += 1;
            tv_base::eventlog::log(&format!(
                "hls: live segment {sequence} failed ({e}); skipped ({} in a row)",
                self.skips_in_row
            ));
            self.active = None;
            self.next_seq = sequence + 1;
            return Ok(Step::Progress);
        }
        Err(HlsError::Segment { sequence, error: e })
    }
}

/// Does `new` list every segment `old` did, at the same playlist times, and perhaps more: the same
/// archive window asked for again later. Another window, or one numbered afresh, is not followed.
fn extends(old: &MediaPlaylist, new: &MediaPlaylist) -> bool {
    new.media_sequence == old.media_sequence
        && new.segments.len() >= old.segments.len()
        && live::start_of(new, old.segments.len()).abs_diff(old.duration_us()) <= 500_000
}

/// Parse a playlist answered from `effective`, refusing an https one that names a plaintext URI
/// (the mixed content curlio refuses for a redirect: a TLS request never crosses into plaintext).
fn parse_secure(text: &str, effective: &str) -> Result<Playlist, HlsError> {
    let p = playlist::parse(text, effective).map_err(HlsError::Parse)?;
    let https = |u: &str| u.get(..8).is_some_and(|s| s.eq_ignore_ascii_case("https://"));
    if !https(effective) {
        return Ok(p);
    }
    fn key(e: &Encryption) -> Option<&str> {
        match e {
            Encryption::Aes128 { uri, .. } => Some(uri),
            _ => None,
        }
    }
    let plaintext = match &p {
        Playlist::Master(m) => {
            m.variants.iter().map(|v| v.uri.as_str()).chain(m.renditions.iter().filter_map(|r| r.uri.as_deref())).any(|u| !https(u))
        }
        Playlist::Media(m) => m.segments.iter().any(|s| {
            !https(&s.uri)
                || key(&s.key).is_some_and(|u| !https(u))
                || s.map.as_ref().is_some_and(|map| !https(&map.uri) || key(&map.key).is_some_and(|u| !https(u)))
        }),
    };
    if plaintext {
        tv_base::eventlog::log("hls: an https playlist names a plaintext URI; refused");
        return Err(HlsError::Insecure);
    }
    Ok(p)
}

/// The application's HLS source: curl, the system clock.
pub type CurlHls = HlsSource<CurlTransport, SystemClock>;

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;
