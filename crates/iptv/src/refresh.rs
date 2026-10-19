//! Playlist and guide fetching with background refresh.
//!
//! [`fetch`] is one conditional GET with the policy every source needs: http and https only, a
//! redirect limit, no https-to-http downgrade, a byte cap and a wall-clock budget on the body, and
//! `ETag` / `Last-Modified` validators. The body is streamed into the consumer, never buffered
//! whole: a guide can exceed 100 MB.
//!
//! [`refresh_once`] runs one refresh (the playlist, then every guide) and [`Refresher`] runs it on
//! a worker thread at start, on request and on an interval, publishing a [`Status`] the UI polls.
//!
//! The network is behind [`Transport`] (one hop, no redirects followed) and storage behind
//! [`Store`]; the application implements both, the transport over `tv_net`'s libcurl. Nothing here
//! formats a URL without [`redact`]: every source in a [`Status`] and every error string is masked.

use std::fmt;
use std::io::{self, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::model::{unix_now, Playlist, Timestamp};
use crate::url::{http_scheme, split_url, Scheme};
use crate::{m3u, redact};

/// Bounds on one fetch. `total` bounds the whole fetch: each [`Transport::get`] is handed what is
/// left of it, and the body is read on the same clock, so slow headers, a chain of slow redirects
/// and a server dripping one byte at a time all end. Idle timeouts are the transport's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Body bytes as received on the wire (a gzip guide counts compressed).
    pub max_bytes: u64,
    pub max_redirects: u8,
    pub total: Duration,
}

/// The byte cap is the M3U parser's: a playlist it would refuse is not downloaded either.
pub const PLAYLIST: Limits = Limits {
    max_bytes: m3u::Limits::DEFAULT.max_total_bytes,
    max_redirects: 5,
    total: Duration::from_secs(120),
};

pub const GUIDE: Limits = Limits {
    max_bytes: 512 << 20,
    max_redirects: 5,
    total: Duration::from_secs(30 * 60),
};

/// Guides fetched per refresh; a playlist naming more is cut here.
const MAX_GUIDES: usize = 8;

/// Longest `Location` followed, as `tv_net`'s plaintext stream keeps.
const MAX_LOCATION: usize = 8192;

/// Longest validator kept and sent back.
const MAX_VALIDATOR: usize = 512;

/// Delays before retrying after consecutive runs with a network failure; past the last, only the
/// interval (or a request) refreshes again.
const RETRIES: [Duration; 3] = [
    Duration::from_secs(15),
    Duration::from_secs(60),
    Duration::from_secs(300),
];

/// Why a request failed below HTTP. No text, so no URL can ride along.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetError {
    /// Name resolution or connect failed.
    Unreachable,
    /// The TLS handshake or certificate verification failed.
    Tls,
    TimedOut,
    Failed,
}

/// The cache validators of one source. A server's values are untrusted: only those that are safe
/// in a header line are kept from a response and sent back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Validators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl Validators {
    /// The values that are safe to echo in a header line.
    fn safe(self) -> Self {
        let keep = |v: Option<String>| v.filter(|v| header_safe(v));
        Validators {
            etag: keep(self.etag),
            last_modified: keep(self.last_modified),
        }
    }

    /// The conditional request lines. Validators come back from the store too, so they are
    /// checked again here.
    fn request_headers(&self) -> Vec<String> {
        let safe = self.clone().safe();
        let etag = safe.etag.map(|v| format!("If-None-Match: {v}"));
        let date = safe
            .last_modified
            .map(|v| format!("If-Modified-Since: {v}"));
        etag.into_iter().chain(date).collect()
    }
}

/// Printable ASCII and tabs only, bounded: no CR or LF to end the header and start another.
fn header_safe(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= MAX_VALIDATOR
        && v.bytes().all(|b| b == b'\t' || (b' '..=b'~').contains(&b))
}

/// One response, its body not yet read.
pub struct Response {
    pub status: u16,
    pub validators: Validators,
    /// `Location`, for a redirect.
    pub location: Option<String>,
    pub body: Box<dyn Read>,
}

/// One HTTP GET, blocking. Implementations must:
///
/// * not follow redirects ([`fetch`] does, with its own checks);
/// * send only http and https, with TLS peer and host verification on;
/// * return within `budget` (for libcurl, `CURLOPT_TIMEOUT_MS`, which also bounds a body it is
///   still receiving) and soon after `cancel` is set (a progress-callback abort), body reads
///   included: the body keeps a clone of `cancel`;
/// * bound idle time (no byte for a while) and report an idle body read as an
///   [`io::ErrorKind::TimedOut`] error;
/// * send `headers` (full `Name: value` lines) as given.
pub trait Transport: Send {
    fn get(
        &self,
        url: &str,
        headers: &[String],
        budget: Duration,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Response, NetError>;
}

/// Why a fetch failed. Its `Display` is the text the UI shows; it never holds a URL or a token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchError {
    /// Not an http or https URL, or a redirect to one that is not.
    BadUrl,
    /// An https URL redirected to plain http.
    Downgrade,
    TooManyRedirects,
    Status(u16),
    /// The body went over [`Limits::max_bytes`].
    TooLarge,
    Net(NetError),
    /// The consumer refused the body; the message is masked.
    Parse(String),
    Cancelled,
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::BadUrl => f.write_str("not an http or https address"),
            FetchError::Downgrade => f.write_str("the server redirected from https to plain http"),
            FetchError::TooManyRedirects => f.write_str("too many redirects"),
            FetchError::Status(code) => write!(f, "the server answered HTTP {code}"),
            FetchError::TooLarge => f.write_str("the download is too large"),
            FetchError::Net(NetError::Unreachable) => f.write_str("the server cannot be reached"),
            FetchError::Net(NetError::Tls) => f.write_str("the secure connection failed"),
            FetchError::Net(NetError::TimedOut) => f.write_str("the server did not answer in time"),
            FetchError::Net(NetError::Failed) => f.write_str("the download failed"),
            FetchError::Parse(message) => f.write_str(message),
            FetchError::Cancelled => f.write_str("cancelled"),
        }
    }
}

/// A successful fetch.
#[derive(Debug, PartialEq, Eq)]
pub enum Fetched<T> {
    New(T, Validators),
    /// `304 Not Modified` to a conditional request: what is stored is current.
    NotModified,
}

/// `location` resolved against `base`, for the reference shapes servers send: absolute,
/// scheme-relative, absolute-path, query and relative-path. Dot segments are left to the server.
fn resolve(base: &str, location: &str) -> Option<String> {
    let loc = location.trim();
    if loc.is_empty()
        || loc.len() > MAX_LOCATION
        || loc.bytes().any(|b| b.is_ascii_control() || b == b' ')
    {
        return None;
    }
    if split_url(loc).is_some() {
        return Some(loc.to_owned());
    }
    let (scheme, authority, tail) = split_url(base)?;
    let origin = format!("{scheme}://{authority}");
    let path = &tail[..tail.find(['?', '#']).unwrap_or(tail.len())];
    Some(if let Some(r) = loc.strip_prefix("//") {
        format!("{scheme}://{r}")
    } else if loc.starts_with('/') {
        format!("{origin}{loc}")
    } else if loc.starts_with(['?', '#']) {
        format!("{origin}{path}{loc}")
    } else {
        let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
        format!("{origin}{}{loc}", if dir.is_empty() { "/" } else { dir })
    })
}

/// The body as the consumer reads it: capped, on the clock, cancellable. The first failure is
/// kept so [`fetch`] reports it rather than whatever the consumer made of the read error.
struct Body<'a> {
    inner: Box<dyn Read>,
    read: u64,
    max: u64,
    deadline: Option<Instant>,
    cancel: &'a AtomicBool,
    failed: Option<FetchError>,
}

impl Body<'_> {
    fn fail(&mut self, why: FetchError) -> io::Result<usize> {
        self.failed.get_or_insert(why);
        Err(io::Error::other("download stopped"))
    }
}

impl Read for Body<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(why) = self.failed.clone() {
            return self.fail(why);
        }
        if self.cancel.load(Ordering::Acquire) {
            return self.fail(FetchError::Cancelled);
        }
        if self.deadline.is_some_and(|at| Instant::now() >= at) {
            return self.fail(FetchError::Net(NetError::TimedOut));
        }
        // One byte past the cap is asked for, so a body of exactly the cap is not mistaken for more.
        let room = usize::try_from(self.max - self.read)
            .unwrap_or(usize::MAX)
            .saturating_add(1);
        let want = buf.len().min(room);
        match self.inner.read(&mut buf[..want]) {
            Ok(n) => {
                self.read += n as u64;
                if self.read > self.max {
                    return self.fail(FetchError::TooLarge);
                }
                Ok(n)
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Err(e),
            // A transport aborts a read in flight when the cancel flag is set.
            Err(_) if self.cancel.load(Ordering::Acquire) => self.fail(FetchError::Cancelled),
            Err(e) => {
                let timed_out = matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                );
                self.fail(FetchError::Net(if timed_out {
                    NetError::TimedOut
                } else {
                    NetError::Failed
                }))
            }
        }
    }
}

/// GET `url`, conditionally when `prev` holds validators, following redirects up to the limit,
/// and stream a 2xx body into `consume`. A consumer that stores must keep its previous data when
/// it returns an error or when a read fails (cap, budget, cancel, network): the body may stop at
/// any byte.
pub fn fetch<T>(
    transport: &dyn Transport,
    url: &str,
    prev: &Validators,
    limits: &Limits,
    cancel: &Arc<AtomicBool>,
    consume: impl FnOnce(&mut dyn Read) -> Result<T, String>,
) -> Result<Fetched<T>, FetchError> {
    // `None`: a budget too large to add to the clock, i.e. unbounded.
    let deadline = Instant::now().checked_add(limits.total);
    let headers = prev.request_headers();
    let mut scheme = http_scheme(url).ok_or(FetchError::BadUrl)?;
    let mut url = url.to_owned();
    let mut redirects = 0;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(FetchError::Cancelled);
        }
        let budget = deadline.map_or(Duration::MAX, |at| {
            at.saturating_duration_since(Instant::now())
        });
        if budget.is_zero() {
            return Err(FetchError::Net(NetError::TimedOut));
        }
        let resp = transport.get(&url, &headers, budget, cancel).map_err(|e| {
            if cancel.load(Ordering::Acquire) {
                FetchError::Cancelled
            } else {
                FetchError::Net(e)
            }
        })?;
        match resp.status {
            301 | 302 | 303 | 307 | 308 => {
                if redirects == limits.max_redirects {
                    return Err(FetchError::TooManyRedirects);
                }
                redirects += 1;
                let next = resp
                    .location
                    .as_deref()
                    .and_then(|loc| resolve(&url, loc))
                    .ok_or(FetchError::BadUrl)?;
                let next_scheme = http_scheme(&next).ok_or(FetchError::BadUrl)?;
                if scheme == Scheme::Https && next_scheme == Scheme::Http {
                    return Err(FetchError::Downgrade);
                }
                (url, scheme) = (next, next_scheme);
            }
            304 if !headers.is_empty() => return Ok(Fetched::NotModified),
            200..=299 => {
                let mut body = Body {
                    inner: resp.body,
                    read: 0,
                    max: limits.max_bytes,
                    deadline,
                    cancel,
                    failed: None,
                };
                let consumed = consume(&mut body);
                return match (body.failed, consumed) {
                    (Some(why), _) => Err(why),
                    (None, Ok(value)) => Ok(Fetched::New(value, resp.validators.safe())),
                    (None, Err(message)) => Err(FetchError::Parse(redact::text(&message))),
                };
            }
            code => return Err(FetchError::Status(code)),
        }
    }
}

/// What a [`Store`] made of a body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stored {
    /// New data was stored. Without `cache_validators` the response's validators are not kept,
    /// so the next fetch is a full download: what was stored needs a second pass (a guide that
    /// listed programmes before their channel).
    New { cache_validators: bool },
    /// The body holds what was stored already: nothing changed.
    Same,
    /// The body was not stored (the reason: an empty or a cut guide); the previous data and its
    /// validators stay.
    Kept(&'static str),
}

/// Why a [`Store`] failed. The text is masked ([`redact::text`]): no URL or token rides along.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreError(String);

impl StoreError {
    pub fn new(message: impl fmt::Display) -> Self {
        StoreError(redact::text(&message.to_string()))
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where fetched data goes: the application implements it over `storage` (and `xmltv`). Each
/// method that stores keeps the previous data when it fails or when the body read fails.
pub trait Store: Send {
    /// Validators saved by [`Store::save_validators`] for `url`, or none. None forces a full
    /// download: right for a guide whose channel filter changed since it was stored.
    fn validators(&mut self, url: &str) -> Validators;
    /// Empty validators forget the stored ones.
    fn save_validators(&mut self, url: &str, validators: &Validators);
    fn save_playlist(&mut self, playlist: Playlist) -> Result<Stored, StoreError>;
    /// Guide URLs the stored playlist declares (`url-tvg`, ...), in order.
    fn guide_urls(&mut self) -> Vec<String>;
    /// Parse and store one guide from its raw body (plain or gzip).
    fn import_guide(&mut self, url: &str, body: &mut dyn Read) -> Result<Stored, StoreError>;
    /// A refresh stored something new. Called on the refresher's thread right before
    /// [`Status::generation`] moves, so what the UI reloads then can be read here first.
    fn refreshed(&mut self) {}
}

/// What to refresh and how often. No `Debug`: the URLs carry tokens.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub playlist_url: String,
    /// Guides fetched besides the ones the playlist declares.
    pub extra_guides: Vec<String>,
    /// Time between automatic refreshes; `None` (or zero) refreshes on start and on request only.
    pub interval: Option<Duration>,
}

/// Which kind of source a [`SourceResult`] is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Playlist,
    Guide,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Updated,
    Unchanged,
    /// The download was not stored, for the reason given ([`Stored::Kept`]); what was stays.
    Kept(&'static str),
}

/// How one source fared in a refresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceResult {
    pub kind: Kind,
    /// The source URL, masked ([`redact::url`]).
    pub source: String,
    pub outcome: Result<Change, FetchError>,
}

impl fmt::Display for SourceResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            Kind::Playlist => "playlist",
            Kind::Guide => "guide",
        };
        match &self.outcome {
            Ok(Change::Updated) => write!(f, "{kind} {}: updated", self.source),
            Ok(Change::Unchanged) => write!(f, "{kind} {}: unchanged", self.source),
            Ok(Change::Kept(why)) => {
                write!(f, "{kind} {}: {why}, kept the stored one", self.source)
            }
            Err(e) => write!(f, "{kind} {}: {e}", self.source),
        }
    }
}

/// Fetch one source and record the result; `store_body` consumes a 2xx body.
fn refresh_source(
    transport: &dyn Transport,
    store: &mut dyn Store,
    kind: Kind,
    url: &str,
    cancel: &Arc<AtomicBool>,
    store_body: impl FnOnce(&mut dyn Store, &mut dyn Read) -> Result<Stored, StoreError>,
) -> SourceResult {
    let limits = if kind == Kind::Playlist {
        &PLAYLIST
    } else {
        &GUIDE
    };
    let prev = store.validators(url);
    let fetched = fetch(transport, url, &prev, limits, cancel, |body| {
        store_body(&mut *store, body).map_err(|e| e.to_string())
    });
    let outcome = fetched.map(|fetched| match fetched {
        Fetched::NotModified => Change::Unchanged,
        Fetched::New(Stored::New { cache_validators }, validators) => {
            let keep = if cache_validators {
                validators
            } else {
                Validators::default()
            };
            store.save_validators(url, &keep);
            Change::Updated
        }
        Fetched::New(Stored::Same, validators) => {
            store.save_validators(url, &validators);
            Change::Unchanged
        }
        Fetched::New(Stored::Kept(why), _) => Change::Kept(why),
    });
    SourceResult {
        kind,
        source: redact::url(url),
        outcome,
    }
}

/// One refresh: the playlist, then the user's extra guides and the guides the playlist declares,
/// in that order, de-duplicated, at most `MAX_GUIDES`. A failed playlist still refreshes the
/// guides of the stored one.
pub fn refresh_once(
    transport: &dyn Transport,
    store: &mut dyn Store,
    config: &Config,
    cancel: &Arc<AtomicBool>,
) -> Vec<SourceResult> {
    let mut results = Vec::new();
    if !config.playlist_url.is_empty() {
        results.push(refresh_source(
            transport,
            store,
            Kind::Playlist,
            &config.playlist_url,
            cancel,
            |store, body| {
                let (playlist, _report) = m3u::parse(BufReader::with_capacity(64 << 10, body))
                    .map_err(StoreError::new)?;
                // An empty answer must not wipe a working channel list.
                if playlist.channels.is_empty() {
                    return Err(StoreError::new("the playlist has no channels"));
                }
                store.save_playlist(playlist)
            },
        ));
    }
    let mut guides = config.extra_guides.clone();
    guides.extend(store.guide_urls());
    let mut seen = std::collections::HashSet::new();
    guides.retain(|g| !g.is_empty() && seen.insert(g.clone()));
    for url in guides.iter().take(MAX_GUIDES) {
        results.push(refresh_source(
            transport,
            store,
            Kind::Guide,
            url,
            cancel,
            |store, body| store.import_guide(url, body),
        ));
    }
    results
}

/// What the UI reads from a [`Refresher`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub running: bool,
    /// Bumped whenever a refresh stored new data: the UI reloads when it changes.
    pub generation: u64,
    /// When the last refresh finished, Unix seconds.
    pub finished_at: Option<Timestamp>,
    /// One entry per source of the last refresh.
    pub sources: Vec<SourceResult>,
}

impl Status {
    pub fn errors(&self) -> impl Iterator<Item = &SourceResult> {
        self.sources.iter().filter(|s| s.outcome.is_err())
    }
}

struct State {
    config: Config,
    status: Status,
    pending: bool,
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    /// Ends the fetch in flight: the transport polls it, during body reads too.
    cancel: Arc<AtomicBool>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Runs [`refresh_once`] on a worker thread: at start (unless [`Refresher::start_idle`]), on
/// [`Refresher::refresh_now`], after [`Refresher::set_config`], every [`Config::interval`], and
/// after a network failure on a short backoff (`RETRIES`). Dropping it cancels the refresh in flight and waits for the worker, so
/// nothing writes the [`Store`] afterwards; the wait is the transport's cancel latency, at worst
/// the fetch budget.
pub struct Refresher {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl Refresher {
    /// Start the worker, which refreshes at once. `None` when the OS refused the thread.
    pub fn start(
        transport: Box<dyn Transport>,
        store: Box<dyn Store>,
        config: Config,
    ) -> Option<Refresher> {
        Self::spawn(transport, store, config, true)
    }

    /// [`Refresher::start`] without the refresh at start: the first one comes on request, with
    /// a new configuration or after one interval. `None` when the OS refused the thread.
    pub fn start_idle(
        transport: Box<dyn Transport>,
        store: Box<dyn Store>,
        config: Config,
    ) -> Option<Refresher> {
        Self::spawn(transport, store, config, false)
    }

    fn spawn(
        transport: Box<dyn Transport>,
        store: Box<dyn Store>,
        config: Config,
        now: bool,
    ) -> Option<Refresher> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                config,
                status: Status {
                    running: now,
                    ..Status::default()
                },
                pending: now,
                stop: false,
            }),
            wake: Condvar::new(),
            cancel: Arc::new(AtomicBool::new(false)),
        });
        let worker = Arc::clone(&shared);
        let worker =
            tv_base::task::spawn("iptv refresh", move || run(&worker, &*transport, store))?;
        Some(Refresher {
            shared,
            worker: Some(worker),
        })
    }

    pub fn status(&self) -> Status {
        self.shared.lock().status.clone()
    }

    /// [`Status::generation`] without copying the whole status: what the UI polls.
    pub fn generation(&self) -> u64 {
        self.shared.lock().status.generation
    }

    /// Refresh as soon as the one in flight, if any, ends.
    pub fn refresh_now(&self) {
        self.shared.lock().pending = true;
        self.shared.wake.notify_one();
    }

    /// Replace the configuration and refresh now, cancelling a refresh of the old one. The same
    /// configuration again changes nothing.
    pub fn set_config(&self, config: Config) {
        let mut state = self.shared.lock();
        if state.config == config {
            return;
        }
        state.config = config;
        state.pending = true;
        self.shared.cancel.store(true, Ordering::Release);
        self.shared.wake.notify_one();
    }
}

impl Drop for Refresher {
    fn drop(&mut self) {
        {
            let mut state = self.shared.lock();
            state.stop = true;
            self.shared.cancel.store(true, Ordering::Release);
        }
        self.shared.wake.notify_one();
        if let Some(worker) = self.worker.take() {
            tv_base::task::join("iptv refresh", worker);
        }
    }
}

/// When the next automatic refresh is due: after `interval`, or sooner on the retry backoff when
/// the last `net_failures` runs failed on the network. `None`: only on request.
fn next_due(now: Instant, interval: Option<Duration>, net_failures: usize) -> Option<Instant> {
    let interval = interval
        .filter(|d| !d.is_zero())
        .and_then(|d| now.checked_add(d));
    let retry = net_failures
        .checked_sub(1)
        .and_then(|i| RETRIES.get(i))
        .and_then(|d| now.checked_add(*d));
    match (interval, retry) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn run(shared: &Shared, transport: &dyn Transport, mut store: Box<dyn Store>) {
    let mut net_failures = 0;
    let mut state = shared.lock();
    loop {
        let due = next_due(Instant::now(), state.config.interval, net_failures);
        while !state.pending && !state.stop {
            state = match due.map(|at| at.saturating_duration_since(Instant::now())) {
                None => shared.wake.wait(state).unwrap_or_else(|e| e.into_inner()),
                Some(left) if left.is_zero() => {
                    state.pending = true;
                    state
                }
                Some(left) => {
                    shared
                        .wake
                        .wait_timeout(state, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
            };
        }
        if state.stop {
            return;
        }
        state.pending = false;
        state.status.running = true;
        shared.cancel.store(false, Ordering::Release);
        let config = state.config.clone();
        drop(state);

        let sources = refresh_once(transport, &mut *store, &config, &shared.cancel);
        // A cancelled run was superseded (a new configuration) or abandoned (drop): its results
        // say "cancelled", not how the sources fared, so the last real status stays.
        let cancelled = shared.cancel.load(Ordering::Acquire);
        if !cancelled {
            for source in &sources {
                tv_base::eventlog::log(&format!("refresh: {source}"));
            }
            let net_failed = sources
                .iter()
                .any(|s| matches!(s.outcome, Err(FetchError::Net(_))));
            net_failures = if net_failed { net_failures + 1 } else { 0 };
        }

        let updated = sources.iter().any(|s| s.outcome == Ok(Change::Updated));
        if updated {
            store.refreshed();
        }
        state = shared.lock();
        let status = &mut state.status;
        if updated {
            status.generation += 1;
        }
        if !cancelled {
            status.running = false;
            status.finished_at = Some(unix_now());
            status.sources = sources;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_resolve_against_the_base() {
        let base = "https://h.example:8443/pl/13/list.m3u?a=1";
        let cases = [
            ("http://o.example/x", "http://o.example/x"),
            ("//cdn.example/y", "https://cdn.example/y"),
            ("/abs?q=2", "https://h.example:8443/abs?q=2"),
            ("?b=2", "https://h.example:8443/pl/13/list.m3u?b=2"),
            ("other.m3u", "https://h.example:8443/pl/13/other.m3u"),
        ];
        for (loc, want) in cases {
            assert_eq!(resolve(base, loc).as_deref(), Some(want), "{loc}");
        }
        assert_eq!(
            resolve("http://h.example", "x").as_deref(),
            Some("http://h.example/x")
        );
        assert_eq!(
            resolve("http://h.example?q", "x").as_deref(),
            Some("http://h.example/x")
        );
        assert_eq!(resolve(base, "/a\r\nX-Injected: 1"), None);
        assert_eq!(resolve(base, ""), None);
        assert_eq!(
            resolve(base, &format!("/{}", "a".repeat(MAX_LOCATION))),
            None
        );
    }

    #[test]
    fn validators_never_carry_a_line_break() {
        let v = Validators {
            etag: Some("\"abc\"\r\nX-Injected: 1".into()),
            last_modified: Some("Mon, 19 Oct 2026 10:00:00 GMT".into()),
        };
        assert_eq!(
            v.request_headers(),
            ["If-Modified-Since: Mon, 19 Oct 2026 10:00:00 GMT"]
        );
        assert!(Validators {
            etag: Some("x".repeat(MAX_VALIDATOR + 1)),
            last_modified: None
        }
        .request_headers()
        .is_empty());
        let received = Validators {
            etag: Some("\"a\"\u{1}b".into()),
            last_modified: Some("Mon, 19 Oct 2026 10:00:00 GMT".into()),
        };
        assert_eq!(received.clone().safe().etag, None);
        assert_eq!(
            received.safe().last_modified.as_deref(),
            Some("Mon, 19 Oct 2026 10:00:00 GMT")
        );
    }

    #[test]
    fn retries_back_off_then_leave_it_to_the_interval() {
        let now = Instant::now();
        let hour = Duration::from_secs(3600);
        assert_eq!(next_due(now, None, 0), None);
        assert_eq!(next_due(now, Some(hour), 0), Some(now + hour));
        assert_eq!(next_due(now, None, 1), Some(now + RETRIES[0]));
        assert_eq!(next_due(now, Some(hour), 3), Some(now + RETRIES[2]));
        assert_eq!(
            next_due(now, Some(Duration::from_secs(5)), 2),
            Some(now + Duration::from_secs(5))
        );
        assert_eq!(next_due(now, None, RETRIES.len() + 1), None);
        assert_eq!(
            next_due(now, Some(hour), RETRIES.len() + 1),
            Some(now + hour)
        );
        assert_eq!(next_due(now, Some(Duration::ZERO), 0), None);
    }

    #[test]
    fn a_huge_interval_means_no_automatic_refresh() {
        let now = Instant::now();
        assert_eq!(next_due(now, Some(Duration::MAX), 0), None);
        assert_eq!(
            next_due(now, Some(Duration::MAX), 1),
            Some(now + RETRIES[0])
        );
    }

    #[test]
    fn error_text_never_holds_a_url() {
        let errors = [
            FetchError::BadUrl,
            FetchError::Downgrade,
            FetchError::TooManyRedirects,
            FetchError::Status(404),
            FetchError::TooLarge,
            FetchError::Net(NetError::Unreachable),
            FetchError::Net(NetError::Tls),
            FetchError::Net(NetError::TimedOut),
            FetchError::Net(NetError::Failed),
            FetchError::Cancelled,
        ];
        for e in errors {
            let text = e.to_string();
            assert!(!text.contains("://") && !text.is_empty(), "{text}");
        }
    }
}
