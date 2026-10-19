//! `iptv::refresh::Transport` over `tv_net`: one GET per call, never following a redirect, the
//! response head read for the validators and `Location`.
//!
//! `tv_net::get::get` pushes (it calls back with the head, then with each body chunk, from inside
//! the request) while `refresh::fetch` pulls (it wants the status before it reads the body). So
//! each [`Transport::get`] runs the request on a worker thread and the two meet in a bounded
//! channel: the head first, then the body chunks, then how the request ended. A full channel
//! blocks the worker inside the transport's write callback, so at most [`CHUNKS`] chunks are held
//! at a time and a guide of hundreds of megabytes streams through into its parser.
//!
//! One exception: on a television whose libcurl requests are serialised behind one lock (an old
//! OpenSSL without its lock callbacks, `tv_net::net::threaded_tls_ready` false), a libcurl request
//! holds that lock for as long as it runs, so blocking it at the parser's pace would stall every
//! other https request. There the body goes at network speed into a spool file instead ([`Spool`]:
//! in the directory the application hands over, its data dir on flash rather than the RAM-backed
//! `/tmp`; a random name, created 0600 and unlinked at once; at most the body cap plus one byte),
//! and the reader gets the file once the request, and the lock, are done. Without a spool dir such
//! a request fails rather than stall the lock or fill RAM.
//!
//! Cancel: `get` and every body read wait on the worker [`POLL`] at a time and look at the
//! caller's flag in between; when it is set they stop the worker and return, a read with an
//! error `fetch` names as the cancel. The worker sees the stop at its transport's next poll (about
//! a second on libcurl). Dropping the body stops the worker and joins it, within about a second.
//!
//! Nothing here formats a URL or a header value: failures are `NetError` codes and io kinds.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use iptv::refresh::{NetError, Response, Transport, Validators};
use tv_net::get::Head;
use tv_net::net::{RequestError, StreamEnd};

/// Silence from the server (no head or body byte for this long) that ends a request.
pub const IDLE: Duration = Duration::from_secs(30);

/// Body chunks held between the worker and the reader. libcurl writes at most 16 KiB at a time
/// and the socket stream reads 16 KiB, so this holds about 256 KiB of a body.
const CHUNKS: usize = 16;

/// How often `get` and a body read look at the caller's cancel flag while they wait.
const POLL: Duration = Duration::from_millis(50);

/// How long past the budget `get` still waits for a head: the transports end on the budget
/// themselves, except inside a synchronous name lookup, which nothing can interrupt.
const GRACE: Duration = Duration::from_secs(2);

/// The production transport: https through the television's libcurl (peer and host verified, the
/// bundled Let's Encrypt roots as the one fallback), plain http through `tv_net`'s socket stream.
pub struct NetTransport {
    idle: Duration,
    /// The largest body a caller reads through this transport: what a spool may hold.
    max_body: u64,
    /// Where spools go ([`NetTransport::with_spool_dir`]).
    spool_dir: Option<PathBuf>,
}

impl NetTransport {
    pub fn new() -> Self {
        Self::with_idle(IDLE)
    }

    pub fn with_idle(idle: Duration) -> Self {
        NetTransport {
            idle,
            max_body: iptv::refresh::GUIDE.max_bytes,
            spool_dir: None,
        }
    }

    /// The largest body its callers read (their `Limits::max_bytes`); a guide's by default.
    pub fn with_max_body(mut self, bytes: u64) -> Self {
        self.max_body = bytes;
        self
    }

    /// The directory a body is spooled into under the serialised-TLS fallback: the application's
    /// private data dir (flash, owner-only), never `/tmp`, which is RAM on the television.
    pub fn with_spool_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.spool_dir = Some(dir.into());
        self
    }

    /// How a body is spooled when it must be: `Ok(None)` streams, `Ok(Some)` is the directory
    /// and the cap, `Err` a spool is needed and there is no directory for it.
    fn spool(&self, spooled: bool) -> Result<Option<(PathBuf, u64)>, NetError> {
        if !spooled {
            return Ok(None);
        }
        let dir = self.spool_dir.clone().ok_or(NetError::Failed)?;
        // One byte past the cap, so the caller's own cap still sees the body is too large.
        Ok(Some((dir, self.max_body.saturating_add(1))))
    }

    /// [`Transport::get`], the body spooled when `spooled`.
    fn get_with(
        &self,
        url: &str,
        headers: &[String],
        budget: Duration,
        cancel: &Arc<AtomicBool>,
        spooled: bool,
    ) -> Result<Response, NetError> {
        let spool = self.spool(spooled)?;
        let (tx, rx) = mpsc::sync_channel(CHUNKS);
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (url, headers, idle) = (url.to_owned(), headers.to_vec(), self.idle);
            let stop = Arc::clone(&stop);
            tv_base::task::spawn("iptv fetch", move || {
                request(&url, &headers, budget, idle, &stop, spool, &tx)
            })
            .ok_or(NetError::Failed)?
        };
        let give_up = Instant::now()
            .checked_add(budget)
            .and_then(|at| at.checked_add(GRACE));
        // Every early return stops the worker; one left behind ends at its next poll, or when the
        // transport gives up, and writes to no one.
        loop {
            // `fetch` reports a cancel as such, whatever this returns.
            if cancel.load(Ordering::Acquire) {
                stop.store(true, Ordering::Release);
                return Err(NetError::Failed);
            }
            if give_up.is_some_and(|at| Instant::now() >= at) {
                stop.store(true, Ordering::Release);
                return Err(NetError::TimedOut);
            }
            match rx.recv_timeout(POLL) {
                Ok(Msg::Head(head)) => {
                    let body = Body {
                        rx: Some(rx),
                        chunk: Vec::new(),
                        at: 0,
                        file: None,
                        end: None,
                        cancel: Arc::clone(cancel),
                        stop,
                        worker: Some(worker),
                    };
                    return Ok(response(head, body));
                }
                Ok(Msg::End(end)) => {
                    tv_base::task::join("iptv fetch", worker);
                    return Err(net_error(end));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Ok(Msg::Chunk(_) | Msg::Spooled(_)) | Err(RecvTimeoutError::Disconnected) => {
                    stop.store(true, Ordering::Release);
                    return Err(NetError::Failed);
                }
            }
        }
    }
}

impl Default for NetTransport {
    fn default() -> Self {
        Self::new()
    }
}

/// What the worker sends, in this order: one head, the body (as chunks, or as one spool file),
/// one end.
enum Msg {
    Head(Head),
    Chunk(Vec<u8>),
    Spooled(File),
    End(StreamEnd),
}

/// Whether a body must be spooled: a libcurl request while libcurl requests are serialised.
fn needs_spool(libcurl: bool, threaded_tls: bool) -> bool {
    libcurl && !threaded_tls
}

impl Transport for NetTransport {
    fn get(
        &self,
        url: &str,
        headers: &[String],
        budget: Duration,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Response, NetError> {
        let libcurl = tv_net::get::uses_libcurl(url);
        let spooled = needs_spool(libcurl, tv_net::net::threaded_tls_ready());
        self.get_with(url, headers, budget, cancel, spooled)
    }
}

/// The worker: one `tv_net` GET into the channel, or with `spool` (its directory and cap) into a
/// [`Spool`] that goes to the reader once the request is over. A closed channel (the reader went
/// away) makes the next callback refuse, which ends the request.
fn request(
    url: &str,
    headers: &[String],
    budget: Duration,
    idle: Duration,
    stop: &AtomicBool,
    spool: Option<(PathBuf, u64)>,
    tx: &SyncSender<Msg>,
) {
    let failed = || Msg::End(StreamEnd::Transport(RequestError::Transport.into()));
    let spool = spool.map(|(dir, cap)| Spool::create(&dir, cap)).transpose();
    let Ok(mut spool) = spool else {
        let _ = tx.send(failed());
        return;
    };
    let end = tv_net::get::get(
        url,
        headers,
        budget,
        idle,
        stop,
        &mut |head| tx.send(Msg::Head(head)).is_ok(),
        &mut |bytes| match &mut spool {
            Some(spool) => spool.write(bytes),
            None => tx.send(Msg::Chunk(bytes.to_vec())).is_ok(),
        },
    );
    if let Some(spool) = spool {
        match spool.rewound() {
            Ok(file) => {
                let _ = tx.send(Msg::Spooled(file));
            }
            Err(_) => {
                let _ = tx.send(failed());
                return;
            }
        }
    }
    let _ = tx.send(Msg::End(end));
}

/// Names a spool tries before it gives up: a taken one is someone else's file.
const SPOOL_TRIES: usize = 4;

/// A body written to a file at network speed. The file has a random name, is created exclusively
/// and 0600 (a file already there under that name is never opened, the next name is tried) and is
/// unlinked at once: only the descriptor names it, and it is gone with the descriptor, whatever
/// happens to the process.
struct Spool {
    file: File,
    len: u64,
    cap: u64,
}

impl Spool {
    fn create(dir: &Path, cap: u64) -> io::Result<Spool> {
        Spool::create_named(dir, cap, std::iter::repeat_with(spool_name))
    }

    /// A spool under the first of `names` (at most [`SPOOL_TRIES`]) not already taken in `dir`.
    fn create_named(
        dir: &Path,
        cap: u64,
        names: impl IntoIterator<Item = String>,
    ) -> io::Result<Spool> {
        let mut taken = io::Error::from(io::ErrorKind::AlreadyExists);
        for name in names.into_iter().take(SPOOL_TRIES) {
            match Spool::open(&dir.join(name), cap) {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => taken = e,
                other => return other,
            }
        }
        Err(taken)
    }

    fn open(path: &Path, cap: u64) -> io::Result<Spool> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        std::fs::remove_file(path)?;
        Ok(Spool { file, len: 0, cap })
    }

    /// Append what fits under the cap; `false`, which ends the request, past it or on a failed
    /// write (a full disk).
    fn write(&mut self, bytes: &[u8]) -> bool {
        let room = usize::try_from(self.cap - self.len).unwrap_or(usize::MAX);
        let take = bytes.len().min(room);
        if self.file.write_all(&bytes[..take]).is_err() {
            return false;
        }
        self.len += take as u64;
        take == bytes.len()
    }

    fn rewound(mut self) -> io::Result<File> {
        self.file.seek(SeekFrom::Start(0))?;
        Ok(self.file)
    }
}

/// `webosiptv-spool-` and 32 hex digits of [`random_bytes`].
fn spool_name() -> String {
    let hex: String = random_bytes().iter().map(|b| format!("{b:02x}")).collect();
    format!("webosiptv-spool-{hex}")
}

/// 16 random bytes: the kernel's `getrandom` on Linux (the app jail may have no `/dev/urandom`,
/// and glibc 2.12 no wrapper), else `/dev/urandom`. Failing both, the clock, the pid and a
/// counter: a name that is harder to guess but not secret, which is enough because a taken name
/// is only a retry.
fn random_bytes() -> [u8; 16] {
    let mut buf = [0u8; 16];
    #[cfg(target_os = "linux")]
    {
        // SAFETY: the kernel writes at most `buf.len()` bytes into `buf`.
        let n = unsafe { libc::syscall(libc::SYS_getrandom, buf.as_mut_ptr(), buf.len(), 0) };
        if n == buf.len() as libc::c_long {
            return buf;
        }
    }
    let urandom = File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf));
    if urandom.is_ok() {
        return buf;
    }
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let count = NEXT.fetch_add(1, Ordering::Relaxed);
    let mix =
        nanos ^ (u64::from(std::process::id()) << 32) ^ count.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    buf[..8].copy_from_slice(&mix.to_le_bytes());
    buf[8..].copy_from_slice(&nanos.rotate_left(29).to_le_bytes());
    buf
}

fn response(head: Head, body: Body) -> Response {
    let field = |name: &str| head.field(name).map(str::to_owned);
    Response {
        status: head.status,
        validators: Validators {
            etag: field("etag"),
            last_modified: field("last-modified"),
        },
        location: field("location"),
        body: Box::new(body),
    }
}

/// Why a request that produced no head failed, in `refresh`'s terms.
fn net_error(end: StreamEnd) -> NetError {
    match end {
        StreamEnd::Idle => NetError::TimedOut,
        StreamEnd::Transport(f) if f.cause == RequestError::TimedOut => NetError::TimedOut,
        StreamEnd::Transport(f) => match f.curl_rc {
            // CURLE_COULDNT_RESOLVE_PROXY, CURLE_COULDNT_RESOLVE_HOST, CURLE_COULDNT_CONNECT.
            Some(5..=7) => NetError::Unreachable,
            // The handshake, the certificate or the trust store (libcurl-errors.html).
            Some(35 | 51 | 53 | 54 | 58 | 59 | 60 | 64 | 66 | 77 | 80 | 82 | 83 | 90 | 91 | 98) => {
                NetError::Tls
            }
            _ => NetError::Failed,
        },
        // No libcurl this device can bind: https cannot be spoken at all.
        StreamEnd::Unsupported => NetError::Tls,
        _ => NetError::Failed,
    }
}

/// How a body read ends once the worker's end arrives.
fn body_end(end: StreamEnd) -> Result<(), io::ErrorKind> {
    match end {
        StreamEnd::Closed => Ok(()),
        StreamEnd::Idle => Err(io::ErrorKind::TimedOut),
        StreamEnd::Transport(f) if f.cause == RequestError::TimedOut => {
            Err(io::ErrorKind::TimedOut)
        }
        _ => Err(io::ErrorKind::Other),
    }
}

/// A response body as `refresh` reads it: the worker's chunks, or its spool file, in order, then
/// how the request ended.
struct Body {
    rx: Option<Receiver<Msg>>,
    chunk: Vec<u8>,
    at: usize,
    file: Option<File>,
    /// How the request ended, once a read reached it.
    end: Option<Result<(), io::ErrorKind>>,
    /// The caller's flag: a read waiting for the worker stops when it is set.
    cancel: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Body {
    /// The worker's next message, waiting for it [`POLL`] at a time; `None` when the worker is
    /// gone, or when the caller cancelled, which also stops the worker.
    fn next(&self) -> Option<Msg> {
        let rx = self.rx.as_ref()?;
        loop {
            if self.cancel.load(Ordering::Acquire) {
                self.stop.store(true, Ordering::Release);
                return None;
            }
            match rx.recv_timeout(POLL) {
                Ok(msg) => return Some(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.at < self.chunk.len() {
                let n = buf.len().min(self.chunk.len() - self.at);
                buf[..n].copy_from_slice(&self.chunk[self.at..self.at + n]);
                self.at += n;
                return Ok(n);
            }
            if let Some(file) = &mut self.file {
                match file.read(buf) {
                    Ok(0) => self.file = None,
                    Ok(n) => return Ok(n),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => (self.file, self.end) = (None, Some(Err(io::ErrorKind::Other))),
                }
                continue;
            }
            if let Some(end) = self.end {
                return end.map(|()| 0).map_err(io::Error::from);
            }
            match self.next() {
                Some(Msg::Chunk(chunk)) => (self.chunk, self.at) = (chunk, 0),
                Some(Msg::Spooled(file)) => self.file = Some(file),
                Some(Msg::End(end)) => self.end = Some(body_end(end)),
                // Cancelled (`fetch` names it), a second head, or a worker gone without an end.
                Some(Msg::Head(_)) | None => self.end = Some(Err(io::ErrorKind::Other)),
            }
        }
    }
}

impl Drop for Body {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // The receiver first: a worker blocked on a full channel wakes to a closed one.
        drop(self.rx.take());
        if let Some(worker) = self.worker.take() {
            tv_base::task::join("iptv fetch", worker);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iptv::refresh::{fetch, FetchError, Fetched, Limits};
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;
    use tv_net::net::RequestFailure;

    /// A loopback server answering each connection on its own thread with `answer(request head,
    /// stream)`, recording every request head.
    struct Server {
        port: u16,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl Server {
        fn start(answer: impl Fn(&str, &mut TcpStream) + Send + Sync + 'static) -> Server {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let (seen, answer) = (Arc::clone(&requests), Arc::new(answer));
            std::thread::spawn(move || {
                for mut stream in listener.incoming().flatten() {
                    let (seen, answer) = (Arc::clone(&seen), Arc::clone(&answer));
                    std::thread::spawn(move || {
                        let mut head = Vec::new();
                        let mut byte = [0u8];
                        while !head.ends_with(b"\r\n\r\n")
                            && stream.read(&mut byte).is_ok_and(|n| n == 1)
                        {
                            head.push(byte[0]);
                        }
                        let head = String::from_utf8_lossy(&head).into_owned();
                        seen.lock().unwrap().push(head.clone());
                        answer(&head, &mut stream);
                    });
                }
            });
            Server { port, requests }
        }

        fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{path}", self.port)
        }

        fn paths(&self) -> Vec<String> {
            let requests = self.requests.lock().unwrap();
            let path = |head: &String| head.split(' ').nth(1).unwrap_or("").to_owned();
            requests.iter().map(path).collect()
        }
    }

    fn reply(stream: &mut TcpStream, status: &str, headers: &[&str], body: &[u8]) {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for h in headers {
            out.push_str(h);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        let _ = stream.write_all(out.as_bytes());
        let _ = stream.write_all(body);
    }

    fn transport() -> NetTransport {
        NetTransport::with_idle(Duration::from_secs(2))
    }

    fn limits(total: Duration) -> Limits {
        Limits {
            max_bytes: 1 << 20,
            max_redirects: 5,
            total,
        }
    }

    fn text(body: &mut dyn Read) -> Result<String, String> {
        let mut out = String::new();
        body.read_to_string(&mut out).map_err(|e| e.to_string())?;
        Ok(out)
    }

    fn get_text(url: &str, prev: &Validators) -> Result<Fetched<String>, FetchError> {
        let cancel = Arc::new(AtomicBool::new(false));
        let limits = limits(Duration::from_secs(10));
        fetch(&transport(), url, prev, &limits, &cancel, text)
    }

    #[test]
    fn a_200_streams_its_body_with_its_validators() {
        let body = "#EXTM3U\n".repeat(20_000);
        let served = body.clone();
        let srv = Server::start(move |_, s| {
            let validators = [
                "ETag: \"v1\"",
                "Last-Modified: Mon, 01 Jan 2024 00:00:00 GMT",
            ];
            reply(s, "200 OK", &validators, served.as_bytes())
        });
        let got = get_text(&srv.url("/list.m3u"), &Validators::default());
        let validators = Validators {
            etag: Some("\"v1\"".into()),
            last_modified: Some("Mon, 01 Jan 2024 00:00:00 GMT".into()),
        };
        assert_eq!(got, Ok(Fetched::New(body, validators)));
    }

    #[test]
    fn a_304_to_an_etag_is_not_modified() {
        let srv = Server::start(|head, s| {
            if head.contains("\r\nIf-None-Match: \"v1\"\r\n") {
                reply(s, "304 Not Modified", &["ETag: \"v1\""], b"");
            } else {
                reply(s, "200 OK", &["ETag: \"v2\""], b"new");
            }
        });
        let prev = Validators {
            etag: Some("\"v1\"".into()),
            last_modified: None,
        };
        assert_eq!(
            get_text(&srv.url("/list.m3u"), &prev),
            Ok(Fetched::NotModified)
        );
        assert_eq!(srv.paths(), ["/list.m3u"]);
    }

    #[test]
    fn a_redirect_hop_comes_back_to_the_caller() {
        let srv = Server::start(|head, s| {
            if head.starts_with("GET /old ") {
                reply(s, "302 Found", &["Location: /new"], b"moved");
            } else {
                reply(s, "200 OK", &[], b"here");
            }
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let hop = transport()
            .get(&srv.url("/old"), &[], Duration::from_secs(10), &cancel)
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!((hop.status, hop.location.as_deref()), (302, Some("/new")));
        drop(hop);
        assert_eq!(srv.paths(), ["/old"], "the transport followed nothing");

        let got = get_text(&srv.url("/old"), &Validators::default());
        assert_eq!(got, Ok(Fetched::New("here".into(), Validators::default())));
        assert_eq!(srv.paths(), ["/old", "/old", "/new"]);
    }

    #[test]
    fn a_slow_body_is_cut_by_the_budget() {
        let srv = Server::start(|_, s| {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n");
            for _ in 0..1000 {
                if s.write_all(b"x").is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let limits = limits(Duration::from_secs(1));
        let got = fetch(
            &transport(),
            &srv.url("/drip"),
            &Validators::default(),
            &limits,
            &cancel,
            text,
        );
        assert_eq!(got, Err(FetchError::Net(NetError::TimedOut)));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }

    /// A cancel ends a fetch within about a second wherever it waits: on the head, on a body
    /// that went silent part-way, and on a spool still filling. The idle bound (10 s) is not what
    /// ends any of them.
    #[test]
    fn cancel_ends_a_fetch_wherever_it_waits() {
        let srv = Server::start(|head, s| {
            if !head.starts_with("GET /hang ") {
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\npart");
            }
            std::thread::sleep(Duration::from_secs(10));
        });
        let patient = NetTransport::with_idle(Duration::from_secs(10));
        let spooling = Spooling(
            NetTransport::with_idle(Duration::from_secs(10)).with_spool_dir(scratch_dir("cancel")),
        );
        let paths: [(&str, &dyn Transport, &str); 3] = [
            ("head", &patient, "/hang"),
            ("body", &patient, "/part"),
            ("spool", &spooling, "/part"),
        ];
        for (what, transport, path) in paths {
            let cancel = Arc::new(AtomicBool::new(false));
            let setter = {
                let cancel = Arc::clone(&cancel);
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(300));
                    cancel.store(true, Ordering::Release);
                })
            };
            let started = Instant::now();
            let limits = limits(Duration::from_secs(30));
            let url = srv.url(path);
            let got = fetch(
                transport,
                &url,
                &Validators::default(),
                &limits,
                &cancel,
                text,
            );
            setter.join().unwrap();
            assert_eq!(got, Err(FetchError::Cancelled), "{what}");
            let took = started.elapsed();
            assert!(took < Duration::from_millis(1500), "{what}: {took:?}");
        }
    }

    /// The worker stops at a full channel instead of reading the body ahead into memory, and
    /// dropping the body ends the request: the server's writes start failing.
    #[test]
    fn the_body_is_never_buffered_whole() {
        const TOTAL: usize = 64 << 20;
        let written = Arc::new(AtomicUsize::new(0));
        let cut = Arc::new(AtomicBool::new(false));
        let srv = {
            let (written, cut) = (Arc::clone(&written), Arc::clone(&cut));
            Server::start(move |_, s| {
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {TOTAL}\r\n\r\n");
                let _ = s.write_all(head.as_bytes());
                let block = vec![b'x'; 64 << 10];
                while written.load(Ordering::Acquire) < TOTAL {
                    if s.write_all(&block).is_err() {
                        cut.store(true, Ordering::Release);
                        return;
                    }
                    written.fetch_add(block.len(), Ordering::AcqRel);
                }
            })
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let resp = transport()
            .get(&srv.url("/big"), &[], Duration::from_secs(30), &cancel)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let mut body = resp.body;
        let mut buf = vec![0u8; 1 << 20];
        body.read_exact(&mut buf).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let ahead = written.load(Ordering::Acquire);
        assert!(
            ahead < 16 << 20,
            "the server got {ahead} bytes ahead of a paused reader"
        );
        drop(body);
        let started = Instant::now();
        while !cut.load(Ordering::Acquire) && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            cut.load(Ordering::Acquire),
            "the request went on after its body was dropped"
        );
    }

    #[test]
    fn failures_before_a_head_map_to_refresh_terms() {
        let failed = |cause, curl_rc| {
            StreamEnd::Transport(RequestFailure {
                cause,
                status: None,
                body_limit: None,
                curl_rc,
                verify: None,
            })
        };
        let cases = [
            (failed(RequestError::TimedOut, Some(28)), NetError::TimedOut),
            (
                failed(RequestError::Transport, Some(6)),
                NetError::Unreachable,
            ),
            (
                failed(RequestError::Transport, Some(7)),
                NetError::Unreachable,
            ),
            (failed(RequestError::Transport, Some(60)), NetError::Tls),
            (failed(RequestError::Transport, Some(35)), NetError::Tls),
            (failed(RequestError::Transport, None), NetError::Failed),
            (StreamEnd::Idle, NetError::TimedOut),
            (StreamEnd::Unsupported, NetError::Tls),
            (StreamEnd::HeadTooLarge, NetError::Failed),
        ];
        for (end, want) in cases {
            assert_eq!(net_error(end), want, "{end:?}");
        }
        // The socket stream does not tell a refused connect from other failures.
        let dead = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/x", dead.local_addr().unwrap().port());
        drop(dead);
        let cancel = Arc::new(AtomicBool::new(false));
        let got = transport().get(&url, &[], Duration::from_secs(5), &cancel);
        assert_eq!(got.err(), Some(NetError::Failed));
    }

    /// A transport as it runs under the serialised-TLS fallback, on plain http.
    struct Spooling(NetTransport);

    impl Transport for Spooling {
        fn get(
            &self,
            url: &str,
            headers: &[String],
            budget: Duration,
            cancel: &Arc<AtomicBool>,
        ) -> Result<Response, NetError> {
            self.0.get_with(url, headers, budget, cancel, true)
        }
    }

    fn spooling() -> Spooling {
        Spooling(transport().with_spool_dir(scratch_dir("spool")))
    }

    #[test]
    fn only_a_serialised_libcurl_request_spools() {
        assert!(needs_spool(true, false));
        assert!(!needs_spool(true, true));
        assert!(
            !needs_spool(false, false),
            "the socket stream never holds the lock"
        );
        assert!(!needs_spool(false, true));
    }

    /// A fresh owner-only directory standing in for the application's data dir.
    fn scratch_dir(tag: &str) -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "webosiptv-transport-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        dir
    }

    #[test]
    fn a_spool_is_owner_only_and_never_on_disk_by_name() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir("mode");
        let spool = Spool::create(&dir, 10).unwrap();
        let mode = spool.file.metadata().unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let left = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(left, 0, "the spool is unlinked as soon as it is open");
        // It is made in the directory it was given: one it may not write refuses it.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        assert!(Spool::create(&dir, 10).is_err());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Another uid can plant a file under a name it guesses: a spool name is random, and a taken
    /// one is never opened, the next is tried.
    #[test]
    fn a_planted_spool_name_is_skipped_and_names_are_random() {
        let dir = scratch_dir("planted");
        std::fs::write(dir.join("taken"), b"planted").unwrap();
        let names = ["taken", "taken", "free"].map(String::from);
        let spool = Spool::create_named(&dir, 10, names).unwrap();
        drop(spool);
        assert_eq!(std::fs::read(dir.join("taken")).unwrap(), b"planted");
        let all_taken = Spool::create_named(&dir, 10, std::iter::repeat("taken".to_owned()));
        let err = all_taken.err().map(|e| e.kind());
        assert_eq!(
            err,
            Some(io::ErrorKind::AlreadyExists),
            "gives up after a few"
        );
        std::fs::remove_dir_all(&dir).unwrap();

        let (a, b) = (spool_name(), spool_name());
        assert_ne!(a, b);
        let suffix = a.strip_prefix("webosiptv-spool-").unwrap();
        assert_eq!(suffix.len(), 32, "{a}");
        assert!(suffix.bytes().all(|c| c.is_ascii_hexdigit()), "{a}");
    }

    /// With no spool dir the transport neither stalls the lock nor writes the RAM-backed `/tmp`:
    /// a request that must spool fails before anything is dialled.
    #[test]
    fn without_a_spool_dir_a_request_that_must_spool_is_not_sent() {
        let srv = Server::start(|_, s| reply(s, "200 OK", &[], b"never"));
        let transport = Spooling(transport());
        let cancel = Arc::new(AtomicBool::new(false));
        let got = transport.get(&srv.url("/x"), &[], Duration::from_secs(5), &cancel);
        assert_eq!(got.err(), Some(NetError::Failed));
        assert!(srv.paths().is_empty(), "{:?}", srv.paths());
    }

    /// A body is read at network speed while the reader waits, then handed over whole from the
    /// spool.
    #[test]
    fn a_serialised_body_is_spooled_before_the_reader_sees_it() {
        const TOTAL: usize = 8 << 20;
        let done = Arc::new(AtomicBool::new(false));
        let srv = {
            let done = Arc::clone(&done);
            Server::start(move |_, s| {
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {TOTAL}\r\n\r\n");
                let _ = s.write_all(head.as_bytes());
                let body: Vec<u8> = (0..TOTAL).map(|i| (i % 251) as u8).collect();
                if s.write_all(&body).is_ok() {
                    done.store(true, Ordering::Release);
                }
            })
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let resp = spooling()
            .get(&srv.url("/big"), &[], Duration::from_secs(30), &cancel)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let started = Instant::now();
        while !done.load(Ordering::Acquire) && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            done.load(Ordering::Acquire),
            "the body did not go out while nobody read it"
        );
        let mut body = resp.body;
        let mut got = Vec::new();
        body.read_to_end(&mut got).unwrap();
        assert_eq!(got.len(), TOTAL);
        assert!(got.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
    }

    /// The spool holds the cap plus one byte, so the caller's cap fails the body, and the
    /// request ends there.
    #[test]
    fn a_spool_stops_one_byte_past_the_cap() {
        const TOTAL: usize = 64 << 20;
        let cut = Arc::new(AtomicBool::new(false));
        let srv = {
            let cut = Arc::clone(&cut);
            Server::start(move |_, s| {
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {TOTAL}\r\n\r\n");
                let _ = s.write_all(head.as_bytes());
                let block = vec![b'x'; 64 << 10];
                for _ in 0..TOTAL / block.len() {
                    if s.write_all(&block).is_err() {
                        cut.store(true, Ordering::Release);
                        return;
                    }
                }
            })
        };
        let cap = 1 << 20;
        let transport = Spooling(
            transport()
                .with_spool_dir(scratch_dir("spool"))
                .with_max_body(cap),
        );
        let limits = Limits {
            max_bytes: cap,
            ..limits(Duration::from_secs(30))
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let got = fetch(
            &transport,
            &srv.url("/big"),
            &Validators::default(),
            &limits,
            &cancel,
            text,
        );
        assert_eq!(got, Err(FetchError::TooLarge));
        let started = Instant::now();
        while !cut.load(Ordering::Acquire) && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            cut.load(Ordering::Acquire),
            "the request went on past the spool's cap"
        );
    }
}
