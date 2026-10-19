//! `refresh` against a loopback HTTP server: the fetch policy (status, validators, redirects,
//! caps, deadlines, downgrade) and the background refresher. The transport here is a plain
//! HTTP/1.0 client that also dials `https://` URLs in plaintext: TLS belongs to the real
//! transport, and what is graded is the policy above it.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use iptv::model::Playlist;
use iptv::refresh::{
    fetch, refresh_once, Change, Config, FetchError, Fetched, Kind, Limits, NetError, Refresher,
    Response, Store, StoreError, Stored, Transport, Validators,
};

/// Synthetic account tokens; none may appear in any status or error text.
const TOKEN: &str = "a1B2c3D4e5F6g7H8i9J0kLmN";
const GUIDE_TOKEN: &str = "Z9y8X7w6V5u4T3s2R1q0PoNm";

struct Loopback {
    idle: Duration,
}

/// How often the client wakes while waiting for headers, to notice the budget and `cancel` the
/// way libcurl's progress callback does.
const SLICE: Duration = Duration::from_millis(20);

impl Transport for Loopback {
    fn get(
        &self,
        url: &str,
        headers: &[String],
        budget: Duration,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Response, NetError> {
        let deadline = Instant::now().checked_add(budget);
        let rest = url.split_once("://").ok_or(NetError::Failed)?.1;
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let addr: SocketAddr = authority.parse().map_err(|_| NetError::Unreachable)?;
        let mut stream = TcpStream::connect_timeout(&addr, self.idle.min(budget))
            .map_err(|_| NetError::Unreachable)?;
        stream
            .set_read_timeout(Some(SLICE))
            .map_err(|_| NetError::Failed)?;
        let mut request = format!(
            "GET {} HTTP/1.0\r\nHost: {authority}\r\n",
            if path.is_empty() { "/" } else { path }
        );
        for line in headers {
            request.push_str(line);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        stream
            .write_all(request.as_bytes())
            .map_err(|_| NetError::Failed)?;

        let mut head = Vec::new();
        let mut byte = [0u8];
        let mut last_byte = Instant::now();
        while !head.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(1) if head.len() < 16 << 10 => {
                    head.push(byte[0]);
                    last_byte = Instant::now();
                }
                Ok(_) => return Err(NetError::Failed),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    if cancel.load(Ordering::Acquire) {
                        return Err(NetError::Failed);
                    }
                    if last_byte.elapsed() >= self.idle
                        || deadline.is_some_and(|at| Instant::now() >= at)
                    {
                        return Err(NetError::TimedOut);
                    }
                }
                Err(_) => return Err(NetError::Failed),
            }
        }
        // The body is bounded by idle time here and by the budget in `fetch`.
        stream
            .set_read_timeout(Some(self.idle))
            .map_err(|_| NetError::Failed)?;
        let head = String::from_utf8_lossy(&head);
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|l| l.split(' ').nth(1))
            .and_then(|c| c.parse().ok())
            .ok_or(NetError::Failed)?;
        let (mut validators, mut location) = (Validators::default(), None);
        for (name, value) in lines.filter_map(|l| l.split_once(':')) {
            let value = Some(value.trim().to_owned());
            match name.to_ascii_lowercase().as_str() {
                "etag" => validators.etag = value,
                "last-modified" => validators.last_modified = value,
                "location" => location = value,
                _ => {}
            }
        }
        Ok(Response {
            status,
            validators,
            location,
            body: Box::new(stream),
        })
    }
}

fn transport() -> Loopback {
    Loopback {
        idle: Duration::from_secs(2),
    }
}

/// A loopback server answering each connection on its own thread with `handler(request head, stream)`.
struct Server {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start(handler: impl Fn(&str, &mut TcpStream) + Send + Sync + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let handler = Arc::new(handler);
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let (seen, handler) = (Arc::clone(&seen), Arc::clone(&handler));
                thread::spawn(move || {
                    let mut head = Vec::new();
                    let mut byte = [0u8];
                    while !head.ends_with(b"\r\n\r\n")
                        && stream.read(&mut byte).is_ok_and(|n| n == 1)
                    {
                        head.push(byte[0]);
                    }
                    let head = String::from_utf8_lossy(&head).into_owned();
                    seen.lock().unwrap().push(head.clone());
                    handler(&head, &mut stream);
                });
            }
        });
        Server { port, requests }
    }

    fn url(&self, scheme: &str, path: &str) -> String {
        format!("{scheme}://127.0.0.1:{}{path}", self.port)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

fn path_of(head: &str) -> &str {
    head.split(' ').nth(1).unwrap_or("")
}

fn reply(stream: &mut TcpStream, status: &str, headers: &[&str], body: &[u8]) {
    let mut out = format!("HTTP/1.0 {status}\r\nContent-Length: {}\r\n", body.len());
    for h in headers {
        out.push_str(h);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    let _ = stream.write_all(out.as_bytes());
    let _ = stream.write_all(body);
}

fn limits() -> Limits {
    Limits {
        max_bytes: 1 << 20,
        max_redirects: 5,
        total: Duration::from_secs(10),
    }
}

fn count(body: &mut dyn Read) -> Result<usize, String> {
    let mut buf = Vec::new();
    body.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf.len())
}

fn get(server_url: &str, prev: &Validators, limits: &Limits) -> Result<Fetched<usize>, FetchError> {
    fetch(
        &transport(),
        server_url,
        prev,
        limits,
        &Arc::new(AtomicBool::new(false)),
        count,
    )
}

#[test]
fn ok_200_streams_the_body_and_keeps_the_validators() {
    let server = Server::start(|_, s| {
        reply(
            s,
            "200 OK",
            &[
                "ETag: \"v1\"",
                "Last-Modified: Mon, 19 Oct 2026 10:00:00 GMT",
            ],
            &[b'x'; 70_000],
        )
    });
    let got = get(&server.url("http", "/a"), &Validators::default(), &limits()).unwrap();
    let want = Validators {
        etag: Some("\"v1\"".into()),
        last_modified: Some("Mon, 19 Oct 2026 10:00:00 GMT".into()),
    };
    assert_eq!(got, Fetched::New(70_000, want));
    assert!(
        !server.requests()[0].contains("If-"),
        "an unconditional first request"
    );
}

#[test]
fn not_modified_304_answers_the_etag() {
    let server = Server::start(|head, s| {
        if head.contains("If-None-Match: \"v1\"\r\n")
            && head.contains("If-Modified-Since: Mon, 19 Oct 2026")
        {
            reply(s, "304 Not Modified", &["ETag: \"v1\""], b"");
        } else {
            reply(s, "200 OK", &["ETag: \"v1\""], b"body");
        }
    });
    let url = server.url("http", "/a");
    let prev = Validators {
        etag: Some("\"v1\"".into()),
        last_modified: Some("Mon, 19 Oct 2026 10:00:00 GMT".into()),
    };
    let consumed = AtomicBool::new(false);
    let got = fetch(
        &transport(),
        &url,
        &prev,
        &limits(),
        &Arc::new(AtomicBool::new(false)),
        |_| {
            consumed.store(true, Ordering::Relaxed);
            Ok(())
        },
    );
    assert_eq!(got, Ok(Fetched::NotModified));
    assert!(!consumed.load(Ordering::Relaxed));
    // A 304 to a request that sent no validator is not a cache hit.
    let server = Server::start(|_, s| reply(s, "304 Not Modified", &[], b""));
    assert_eq!(
        get(&server.url("http", "/a"), &Validators::default(), &limits()),
        Err(FetchError::Status(304))
    );
}

#[test]
fn redirects_are_followed_up_to_the_limit() {
    let server = Server::start(|head, s| match path_of(head) {
        "/start" => reply(s, "302 Found", &["Location: next?x=1"], b""),
        "/next?x=1" => reply(s, "301 Moved", &["Location: /final"], b""),
        _ => reply(s, "200 OK", &[], b"done"),
    });
    assert_eq!(
        get(
            &server.url("http", "/start"),
            &Validators::default(),
            &limits()
        ),
        Ok(Fetched::New(4, Validators::default()))
    );
    assert_eq!(server.requests().len(), 3);

    let server =
        Server::start(|_, s| reply(s, "307 Temporary Redirect", &["Location: /again"], b""));
    let three = Limits {
        max_redirects: 3,
        ..limits()
    };
    assert_eq!(
        get(&server.url("http", "/loop"), &Validators::default(), &three),
        Err(FetchError::TooManyRedirects)
    );
    assert_eq!(
        server.requests().len(),
        4,
        "the first request and three redirects"
    );
}

#[test]
fn oversize_body_is_cut_at_the_cap() {
    let server = Server::start(|head, s| {
        let n = if path_of(head) == "/exact" {
            1000
        } else {
            5000
        };
        reply(s, "200 OK", &[], &vec![b'x'; n]);
    });
    let small = Limits {
        max_bytes: 1000,
        ..limits()
    };
    let seen = Mutex::new(0);
    let got = fetch(
        &transport(),
        &server.url("http", "/big"),
        &Validators::default(),
        &small,
        &Arc::new(AtomicBool::new(false)),
        |body| {
            let mut buf = [0u8; 300];
            // A consumer that swallows read errors still cannot get past the cap.
            while let Ok(n @ 1..) = body.read(&mut buf) {
                *seen.lock().unwrap() += n;
            }
            Ok(())
        },
    );
    assert_eq!(got, Err(FetchError::TooLarge));
    assert!(*seen.lock().unwrap() <= 1000);
    assert_eq!(
        get(
            &server.url("http", "/exact"),
            &Validators::default(),
            &small
        ),
        Ok(Fetched::New(1000, Validators::default()))
    );
}

#[test]
fn a_stalled_server_times_out() {
    let server = Server::start(|head, s| {
        if path_of(head) == "/body" {
            let _ = s.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 100\r\n\r\nabc");
        }
        thread::sleep(Duration::from_secs(3));
    });
    let quick = Loopback {
        idle: Duration::from_millis(200),
    };
    for path in ["/head", "/body"] {
        let started = Instant::now();
        let got = fetch(
            &quick,
            &server.url("http", path),
            &Validators::default(),
            &limits(),
            &Arc::new(AtomicBool::new(false)),
            count,
        );
        assert_eq!(got, Err(FetchError::Net(NetError::TimedOut)), "{path}");
        assert!(started.elapsed() < Duration::from_secs(2), "{path}");
    }
}

#[test]
fn a_dripping_server_hits_the_total_budget() {
    let server = Server::start(|_, s| {
        let _ = s.write_all(b"HTTP/1.0 200 OK\r\n\r\n");
        for _ in 0..200 {
            if s.write_all(b"x").is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    let budget = Limits {
        total: Duration::from_millis(300),
        ..limits()
    };
    let started = Instant::now();
    assert_eq!(
        get(
            &server.url("http", "/drip"),
            &Validators::default(),
            &budget
        ),
        Err(FetchError::Net(NetError::TimedOut))
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn https_never_downgrades_to_http() {
    let server = Server::start(|head, s| match path_of(head) {
        "/down" => reply(s, "302 Found", &["Location: http://127.0.0.1:9/plain"], b""),
        "/up" => reply(s, "302 Found", &["Location: /tls"], b""),
        _ => reply(s, "200 OK", &[], b"ok"),
    });
    assert_eq!(
        get(
            &server.url("https", "/down"),
            &Validators::default(),
            &limits()
        ),
        Err(FetchError::Downgrade)
    );
    assert_eq!(
        server.requests().len(),
        1,
        "the plaintext target is never asked"
    );
    // https to https, and http to https, are followed.
    assert_eq!(
        get(
            &server.url("https", "/up"),
            &Validators::default(),
            &limits()
        ),
        Ok(Fetched::New(2, Validators::default()))
    );
    assert_eq!(
        get(
            &server.url("http", "/up"),
            &Validators::default(),
            &limits()
        ),
        Ok(Fetched::New(2, Validators::default()))
    );
}

#[test]
fn only_http_and_https_are_fetched() {
    let server = Server::start(|_, s| reply(s, "302 Found", &["Location: file:///etc/hosts"], b""));
    for bad in ["ftp://127.0.0.1/a", "file:///etc/hosts", "127.0.0.1/a"] {
        assert_eq!(
            get(bad, &Validators::default(), &limits()),
            Err(FetchError::BadUrl)
        );
    }
    assert_eq!(
        get(&server.url("http", "/a"), &Validators::default(), &limits()),
        Err(FetchError::BadUrl)
    );
    let server = Server::start(|_, s| reply(s, "302 Found", &[], b""));
    assert_eq!(
        get(&server.url("http", "/a"), &Validators::default(), &limits()),
        Err(FetchError::BadUrl)
    );
}

/// Answers 200 with a body whose read is aborted by a cancel landing while it waits, as a
/// transport polling its clone of the flag does.
struct Aborting;

impl Transport for Aborting {
    fn get(
        &self,
        _url: &str,
        _headers: &[String],
        _budget: Duration,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Response, NetError> {
        Ok(Response {
            status: 200,
            validators: Validators::default(),
            location: None,
            body: Box::new(AbortedRead(Arc::clone(cancel))),
        })
    }
}

struct AbortedRead(Arc<AtomicBool>);

impl Read for AbortedRead {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        self.0.store(true, Ordering::Release);
        Err(io::Error::other("aborted"))
    }
}

#[test]
fn a_read_aborted_by_cancel_is_cancelled_not_a_network_failure() {
    let cancel = Arc::new(AtomicBool::new(false));
    let got = fetch(
        &Aborting,
        "http://h.example/a",
        &Validators::default(),
        &limits(),
        &cancel,
        count,
    );
    assert_eq!(got, Err(FetchError::Cancelled));
}

#[test]
fn a_cancelled_fetch_stops_reading() {
    let server = Server::start(|_, s| reply(s, "200 OK", &[], &[b'x'; 10_000]));
    let cancel = Arc::new(AtomicBool::new(false));
    let got = fetch(
        &transport(),
        &server.url("http", "/a"),
        &Validators::default(),
        &limits(),
        &cancel,
        |body| {
            let mut buf = [0u8; 100];
            body.read(&mut buf).map_err(|e| e.to_string())?;
            cancel.store(true, Ordering::Release);
            body.read(&mut buf).map_err(|e| e.to_string())?;
            Ok(())
        },
    );
    assert_eq!(got, Err(FetchError::Cancelled));
}

#[derive(Default)]
struct Mem {
    validators: HashMap<String, Validators>,
    playlists: Vec<Playlist>,
    guide_urls: Vec<String>,
    guides: Vec<(String, usize)>,
    /// [`Store::refreshed`] calls.
    refreshed: usize,
}

#[derive(Clone, Default)]
struct MemStore(Arc<Mutex<Mem>>);

impl Store for MemStore {
    fn validators(&mut self, url: &str) -> Validators {
        self.0
            .lock()
            .unwrap()
            .validators
            .get(url)
            .cloned()
            .unwrap_or_default()
    }

    fn save_validators(&mut self, url: &str, validators: &Validators) {
        self.0
            .lock()
            .unwrap()
            .validators
            .insert(url.to_owned(), validators.clone());
    }

    fn save_playlist(&mut self, playlist: Playlist) -> Result<Stored, StoreError> {
        let mut mem = self.0.lock().unwrap();
        mem.guide_urls = playlist.epg_urls.clone();
        mem.playlists.push(playlist);
        Ok(UPDATED)
    }

    fn guide_urls(&mut self) -> Vec<String> {
        self.0.lock().unwrap().guide_urls.clone()
    }

    fn import_guide(&mut self, url: &str, body: &mut dyn Read) -> Result<Stored, StoreError> {
        let mut buf = Vec::new();
        body.read_to_end(&mut buf).map_err(StoreError::new)?;
        if !buf.starts_with(b"<tv") {
            // Echoes the URL on purpose: what a store says must come out masked.
            return Err(StoreError::new(format!("not a guide: {url}")));
        }
        self.0
            .lock()
            .unwrap()
            .guides
            .push((url.to_owned(), buf.len()));
        Ok(UPDATED)
    }

    fn refreshed(&mut self) {
        self.0.lock().unwrap().refreshed += 1;
    }
}

const UPDATED: Stored = Stored::New {
    cache_validators: true,
};

/// A provider: a playlist naming one guide, both with ETags, and a broken second guide.
fn provider() -> Server {
    Server::start(|head, s| {
        let port = head
            .split("Host: 127.0.0.1:")
            .nth(1)
            .and_then(|r| r.split("\r\n").next())
            .unwrap_or("0")
            .to_owned();
        let path = path_of(head);
        if path.starts_with("/pl/") {
            if head.contains("If-None-Match: \"pl1\"") {
                return reply(s, "304 Not Modified", &[], b"");
            }
            let body = format!(
                "#EXTM3U url-tvg=\"http://127.0.0.1:{port}/epg/{GUIDE_TOKEN}/guide.xml\"\n#EXTINF:-1 tvg-id=\"one\",One\nhttp://stream.example/live/{TOKEN}/1.ts\n"
            );
            reply(s, "200 OK", &["ETag: \"pl1\""], body.as_bytes());
        } else if path.starts_with("/epg/") {
            if head.contains("If-None-Match: \"g1\"") {
                return reply(s, "304 Not Modified", &[], b"");
            }
            reply(s, "200 OK", &["ETag: \"g1\""], b"<tv></tv>");
        } else if path == "/empty" {
            reply(s, "200 OK", &[], b"#EXTM3U\n");
        } else {
            reply(s, "200 OK", &[], b"<html>nope</html>");
        }
    })
}

#[test]
fn an_empty_playlist_keeps_the_stored_one() {
    let server = provider();
    let mut store = MemStore::default();
    let config = Config {
        playlist_url: server.url("http", "/empty"),
        ..Config::default()
    };
    let results = refresh_once(
        &transport(),
        &mut store,
        &config,
        &Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(
        results[0].outcome,
        Err(FetchError::Parse("the playlist has no channels".into()))
    );
    assert!(store.0.lock().unwrap().playlists.is_empty());
    assert!(store.0.lock().unwrap().validators.is_empty());
}

#[test]
fn refresh_once_fetches_the_playlist_then_its_guides_and_revalidates() {
    let server = provider();
    let mut store = MemStore::default();
    let config = Config {
        playlist_url: server.url("http", &format!("/pl/{TOKEN}/playlist.m3u")),
        ..Config::default()
    };
    let cancel = Arc::new(AtomicBool::new(false));

    let first = refresh_once(&transport(), &mut store, &config, &cancel);
    let outcomes: Vec<_> = first.iter().map(|r| (r.kind, r.outcome.clone())).collect();
    assert_eq!(
        outcomes,
        [
            (Kind::Playlist, Ok(Change::Updated)),
            (Kind::Guide, Ok(Change::Updated))
        ]
    );
    {
        let mem = store.0.lock().unwrap();
        assert_eq!(mem.playlists.len(), 1);
        assert_eq!(mem.playlists[0].channels.len(), 1);
        assert_eq!(mem.guides.len(), 1);
    }

    let second = refresh_once(&transport(), &mut store, &config, &cancel);
    assert!(
        second.iter().all(|r| r.outcome == Ok(Change::Unchanged)),
        "{second:?}"
    );
    assert_eq!(store.0.lock().unwrap().playlists.len(), 1);
    assert_eq!(server.requests().len(), 4);
}

#[test]
fn no_status_text_or_error_holds_a_token() {
    let server = provider();
    let mut store = MemStore::default();
    let config = Config {
        playlist_url: server.url("http", &format!("/missing/{TOKEN}?token=short")),
        extra_guides: vec![
            server.url("http", &format!("/bad/{GUIDE_TOKEN}/guide.xml?key=short")),
            format!("http://127.0.0.1:1/{TOKEN}"),
            format!("ftp://127.0.0.1/{TOKEN}"),
        ],
        interval: None,
    };
    // `/missing` is not a playlist path: the provider serves its HTML error page with a 200.
    let results = refresh_once(
        &transport(),
        &mut store,
        &config,
        &Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(results.len(), 4);
    assert!(results.iter().all(|r| r.outcome.is_err()), "{results:?}");
    for r in &results {
        for text in [
            r.to_string(),
            format!("{r:?}"),
            r.outcome.clone().unwrap_err().to_string(),
        ] {
            for secret in [TOKEN, GUIDE_TOKEN, "short"] {
                assert!(!text.contains(secret), "{secret} in {text}");
            }
        }
    }
    let parse = results[1].outcome.clone().unwrap_err();
    assert!(
        matches!(&parse, FetchError::Parse(m) if m.starts_with("not a guide: http://127.0.0.1:")),
        "{parse:?}"
    );
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn refresher_runs_on_start_on_request_and_on_an_interval() {
    let server = provider();
    let store = MemStore::default();
    let config = Config {
        playlist_url: server.url("http", &format!("/pl/{TOKEN}/playlist.m3u")),
        ..Config::default()
    };
    let refresher = Refresher::start(
        Box::new(transport()),
        Box::new(store.clone()),
        config.clone(),
    )
    .unwrap();
    wait_until("the start refresh", || {
        refresher.status().finished_at.is_some()
    });
    let status = refresher.status();
    assert!(!status.running);
    assert_eq!(status.generation, 1);
    assert_eq!(status.errors().count(), 0);
    assert_eq!(status.sources.len(), 2);
    assert!(status.sources.iter().all(|s| !s.source.contains(TOKEN)));
    assert_eq!(
        store.0.lock().unwrap().refreshed,
        1,
        "told before the generation moved"
    );

    refresher.refresh_now();
    wait_until("the requested refresh", || {
        server.requests().len() == 4 && !refresher.status().running
    });
    assert_eq!(refresher.status().generation, 1, "a 304 stores nothing new");
    assert_eq!(store.0.lock().unwrap().refreshed, 1, "nor tells the store");

    refresher.set_config(Config {
        interval: Some(Duration::from_millis(50)),
        ..config
    });
    wait_until("two interval refreshes", || server.requests().len() >= 10);
    assert_eq!(store.0.lock().unwrap().playlists.len(), 1);
}

#[test]
fn an_idle_refresher_waits_for_a_request() {
    let server = provider();
    let config = Config {
        playlist_url: server.url("http", &format!("/pl/{TOKEN}/playlist.m3u")),
        ..Config::default()
    };
    let refresher =
        Refresher::start_idle(Box::new(transport()), Box::new(MemStore::default()), config)
            .unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(server.requests().is_empty());
    let status = refresher.status();
    assert!(!status.running && status.finished_at.is_none());
    refresher.refresh_now();
    wait_until("the requested refresh", || {
        refresher.status().finished_at.is_some()
    });
    assert_eq!(refresher.generation(), 1);
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn refresher_failures_are_a_status() {
    let config = Config {
        playlist_url: format!("http://127.0.0.1:1/pl/{TOKEN}"),
        ..Config::default()
    };
    let refresher =
        Refresher::start(Box::new(transport()), Box::new(MemStore::default()), config).unwrap();
    wait_until("the failed refresh", || {
        refresher.status().finished_at.is_some()
    });
    let status = refresher.status();
    let errors: Vec<_> = status.errors().collect();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0].outcome,
        Err(FetchError::Net(NetError::Unreachable))
    );
    assert_eq!(
        errors[0].to_string(),
        "playlist http://127.0.0.1:1/pl/<masked>: the server cannot be reached"
    );
    assert_eq!(status.generation, 0);
}

#[test]
fn dropping_the_refresher_ends_the_download_in_flight() {
    let gone = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&gone);
    let server = Server::start(move |_, s| {
        let _ = s.write_all(b"HTTP/1.0 200 OK\r\n\r\n#EXTM3U\n");
        for _ in 0..500 {
            if s.write_all(b"#EXTINF:-1,x\nhttp://h.example/1.ts\n")
                .is_err()
            {
                seen.store(true, Ordering::Release);
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
    });
    let store = MemStore::default();
    let config = Config {
        playlist_url: server.url("http", "/slow"),
        ..Config::default()
    };
    let refresher =
        Refresher::start(Box::new(transport()), Box::new(store.clone()), config).unwrap();
    wait_until("the download to start", || !server.requests().is_empty());
    thread::sleep(Duration::from_millis(50));
    drop(refresher);
    // Drop waited for the worker: nothing else holds the store, so nothing can write it.
    assert_eq!(Arc::strong_count(&store.0), 1);
    wait_until("the client to hang up", || gone.load(Ordering::Acquire));
    assert!(
        store.0.lock().unwrap().playlists.is_empty(),
        "a cancelled download stores nothing"
    );
}

fn stalled_head_server() -> Server {
    Server::start(|_, _| thread::sleep(Duration::from_secs(3)))
}

#[test]
fn slow_headers_end_at_the_total_budget() {
    let server = stalled_head_server();
    let budget = Limits {
        total: Duration::from_millis(300),
        ..limits()
    };
    let started = Instant::now();
    // The transport's idle bound is 2 s: only the budget can end this in time.
    assert_eq!(
        get(&server.url("http", "/a"), &Validators::default(), &budget),
        Err(FetchError::Net(NetError::TimedOut))
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn slow_redirect_hops_end_at_the_total_budget() {
    // Each hop fits the 2 s idle bound; the second must be cut at the 1.5 s budget, not at 2 s.
    let server = Server::start(|_, s| {
        thread::sleep(Duration::from_secs(1));
        reply(s, "302 Found", &["Location: /next"], b"");
    });
    let budget = Limits {
        total: Duration::from_millis(1500),
        ..limits()
    };
    let started = Instant::now();
    assert_eq!(
        get(&server.url("http", "/a"), &Validators::default(), &budget),
        Err(FetchError::Net(NetError::TimedOut))
    );
    assert!(
        started.elapsed() < Duration::from_millis(1800),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn cancel_interrupts_a_request_waiting_for_headers() {
    let server = stalled_head_server();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        flag.store(true, Ordering::Release);
    });
    let started = Instant::now();
    let got = fetch(
        &transport(),
        &server.url("http", "/a"),
        &Validators::default(),
        &limits(),
        &cancel,
        count,
    );
    assert_eq!(got, Err(FetchError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn an_unbounded_budget_and_interval_do_not_overflow() {
    let server = provider();
    let unbounded = Limits {
        total: Duration::MAX,
        ..limits()
    };
    assert_eq!(
        get(
            &server.url("http", "/epg/x"),
            &Validators::default(),
            &unbounded
        ),
        Ok(Fetched::New(
            9,
            Validators {
                etag: Some("\"g1\"".into()),
                last_modified: None
            }
        ))
    );
    let config = Config {
        playlist_url: server.url("http", &format!("/pl/{TOKEN}/playlist.m3u")),
        interval: Some(Duration::MAX),
        ..Config::default()
    };
    let refresher =
        Refresher::start(Box::new(transport()), Box::new(MemStore::default()), config).unwrap();
    wait_until("the start refresh", || {
        refresher.status().finished_at.is_some()
    });
    assert_eq!(refresher.status().errors().count(), 0);
}

#[test]
fn unsafe_validators_are_dropped_on_arrival() {
    let long = format!("Last-Modified: {}", "x".repeat(600));
    let server =
        Server::start(move |_, s| reply(s, "200 OK", &["ETag: \"a\"\u{1}b", &long], b"ok"));
    assert_eq!(
        get(&server.url("http", "/a"), &Validators::default(), &limits()),
        Ok(Fetched::New(2, Validators::default()))
    );
}

#[test]
fn extra_guides_come_first_under_the_cap() {
    let server = provider();
    let mut store = MemStore::default();
    store.0.lock().unwrap().guide_urls = (0..10)
        .map(|i| server.url("http", &format!("/epg/g{i}")))
        .collect();
    let config = Config {
        extra_guides: vec![server.url("http", "/epg/extra")],
        ..Config::default()
    };
    let results = refresh_once(
        &transport(),
        &mut store,
        &config,
        &Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(results.len(), 8);
    assert!(results[0].source.ends_with("/epg/extra"), "{}", results[0]);
    assert!(results.iter().all(|r| r.outcome == Ok(Change::Updated)));
}

/// Answers every guide with `.1`, like a storage that kept its guide or needs a second pass.
struct Answering(MemStore, Stored);

impl Store for Answering {
    fn validators(&mut self, url: &str) -> Validators {
        self.0.validators(url)
    }

    fn save_validators(&mut self, url: &str, validators: &Validators) {
        self.0.save_validators(url, validators);
    }

    fn save_playlist(&mut self, playlist: Playlist) -> Result<Stored, StoreError> {
        self.0.save_playlist(playlist)
    }

    fn guide_urls(&mut self) -> Vec<String> {
        self.0.guide_urls()
    }

    fn import_guide(&mut self, _url: &str, _body: &mut dyn Read) -> Result<Stored, StoreError> {
        Ok(self.1)
    }
}

#[test]
fn what_the_store_made_of_a_body_decides_the_outcome_and_the_validators() {
    let server = provider();
    let config = Config {
        extra_guides: vec![server.url("http", "/epg/g")],
        ..Config::default()
    };
    let g1 = Validators {
        etag: Some("\"g1\"".into()),
        last_modified: None,
    };
    let cases = [
        (
            Stored::Kept("the guide ended early"),
            Change::Kept("the guide ended early"),
            None,
        ),
        (
            Stored::New {
                cache_validators: false,
            },
            Change::Updated,
            Some(Validators::default()),
        ),
        (Stored::Same, Change::Unchanged, Some(g1)),
    ];
    for (answer, change, saved) in cases {
        let mut store = Answering(MemStore::default(), answer);
        let results = refresh_once(
            &transport(),
            &mut store,
            &config,
            &Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(results[0].outcome, Ok(change), "{answer:?}");
        let mem = store.0 .0.lock().unwrap();
        assert_eq!(mem.validators.values().next(), saved.as_ref(), "{answer:?}");
    }
}

/// A playlist served in 30 slow lines, about 300 ms.
fn slow_playlist() -> Server {
    Server::start(|_, s| {
        let _ = s.write_all(b"HTTP/1.0 200 OK\r\n\r\n#EXTM3U\n");
        for i in 0..30 {
            let line = format!("#EXTINF:-1,Ch {i}\nhttp://h.example/{i}.ts\n");
            if s.write_all(line.as_bytes()).is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
    })
}

#[test]
fn the_same_config_again_keeps_the_run() {
    let server = slow_playlist();
    let store = MemStore::default();
    let config = Config {
        playlist_url: server.url("http", "/slow"),
        ..Config::default()
    };
    let refresher = Refresher::start(
        Box::new(transport()),
        Box::new(store.clone()),
        config.clone(),
    )
    .unwrap();
    wait_until("the download to start", || !server.requests().is_empty());
    refresher.set_config(config);
    wait_until("the refresh", || refresher.status().finished_at.is_some());
    assert_eq!(refresher.status().errors().count(), 0);
    assert_eq!(store.0.lock().unwrap().playlists[0].channels.len(), 30);
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn a_superseded_run_is_never_published() {
    let old = slow_playlist();
    let new = Server::start(|_, s| {
        thread::sleep(Duration::from_millis(300));
        reply(
            s,
            "200 OK",
            &[],
            b"#EXTM3U\n#EXTINF:-1,One\nhttp://h.example/1.ts\n",
        );
    });
    let refresher = Refresher::start(
        Box::new(transport()),
        Box::new(MemStore::default()),
        Config {
            playlist_url: old.url("http", "/slow"),
            ..Config::default()
        },
    )
    .unwrap();
    wait_until("the old download to start", || !old.requests().is_empty());
    refresher.set_config(Config {
        playlist_url: new.url("http", "/new"),
        ..Config::default()
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        let status = refresher.status();
        assert!(
            status
                .sources
                .iter()
                .all(|s| s.outcome != Err(FetchError::Cancelled)),
            "{status:?}"
        );
        if status.finished_at.is_some() {
            break status;
        }
        assert!(Instant::now() < deadline, "no refresh finished");
        thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(status.sources.len(), 1);
    assert_eq!(status.sources[0].outcome, Ok(Change::Updated));
    assert!(status.sources[0].source.ends_with("/new"));
    assert_eq!(status.generation, 1);
}

#[test]
fn a_registered_path_token_never_reaches_the_status() {
    // 13 bytes: mixed, and lowercase only (no digit to make it look random). The app registers
    // its configured URLs before the first fetch.
    for token in [
        ["k2v9x4", "q7m1z8b"].concat(),
        ["kqvxzmb", "trwpln"].concat(),
    ] {
        let config = Config {
            playlist_url: format!("http://127.0.0.1:1/pl/13/{token}/playlist.m3u8"),
            ..Config::default()
        };
        iptv::redact::register_secrets(&config.playlist_url);
        let results = refresh_once(
            &transport(),
            &mut MemStore::default(),
            &config,
            &Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(
            results[0].source,
            "http://127.0.0.1:1/pl/13/<masked>/playlist.m3u8"
        );
        assert!(!results[0].to_string().contains(&token));
    }
}
