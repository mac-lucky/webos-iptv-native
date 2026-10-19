//! The head reader, and [`get`] over the plaintext socket against a loopback server: the head and
//! its fields, a `304` and a `302` handed back as heads (nothing followed), the size cap, the
//! body framings, the budget, cancel, and no event-log line naming a path. The libcurl arm is
//! graded in `net_get_tests.rs`.

use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn read_all(lines: &[&[u8]]) -> Vec<Step> {
    let mut reader = HeadReader::default();
    lines.iter().map(|line| reader.line(line)).collect()
}

#[test]
fn a_head_is_its_status_and_fields_in_order() {
    let steps = read_all(&[
        b"HTTP/1.1 200 OK\r\n",
        b"ETag:  \"v1\" \r\n",
        b"last-modified:\tMon, 01 Jan 2024 00:00:00 GMT\r\n",
        b"X-Folded: one\r\n",
        b"\t two\r\n",
        b"no colon here\r\n",
        b"\r\n",
        b"Trailer: ignored\r\n",
    ]);
    let Some(Step::Head(head)) = steps.get(6) else {
        panic!("{steps:?}");
    };
    assert_eq!(head.status, 200);
    assert_eq!(head.field("etag"), Some("\"v1\""));
    assert_eq!(
        head.field("Last-Modified"),
        Some("Mon, 01 Jan 2024 00:00:00 GMT")
    );
    assert_eq!(head.field("x-folded"), Some("one two"));
    assert_eq!(head.fields.len(), 3);
    assert_eq!(steps[7], Step::More);
}

#[test]
fn an_interim_head_is_skipped_for_the_final_one() {
    let steps = read_all(&[
        b"HTTP/1.1 100 Continue\r\n",
        b"X-Interim: 1\r\n",
        b"\r\n",
        b"HTTP/2 304\r\n",
        b"ETag: W/\"x\"\r\n",
        b"\r\n",
    ]);
    let Some(Step::Head(head)) = steps.last() else {
        panic!("{steps:?}");
    };
    assert_eq!(head.status, 304);
    assert_eq!(head.field("x-interim"), None);
    assert_eq!(head.field("etag"), Some("W/\"x\""));
}

#[test]
fn a_head_without_a_usable_status_is_malformed() {
    for status in [
        &b"HTTP/1.1 20 OK\r\n"[..],
        b"HTTP/1.1 2000\r\n",
        b"HTTP/1.1 600 X\r\n",
        b"Nope: 1\r\n",
    ] {
        let steps = read_all(&[status, b"\r\n"]);
        assert_eq!(
            steps.last(),
            Some(&Step::Malformed),
            "{}",
            String::from_utf8_lossy(status)
        );
    }
}

/// Only a head's first line is its status line: a server cannot restart the head part-way.
#[test]
fn a_status_line_inside_a_head_is_not_one() {
    let steps = read_all(&[
        b"HTTP/1.1 404 Not Found\r\n",
        b"HTTP/1.1 200 OK\r\n",
        b"X-After: 1\r\n",
        b"\r\n",
    ]);
    let Some(Step::Head(head)) = steps.last() else {
        panic!("{steps:?}");
    };
    assert_eq!((head.status, head.field("x-after")), (404, Some("1")));
    let steps = read_all(&[b"HTTP/1.1 2000\r\n", b"HTTP/1.1 200 OK\r\n", b"\r\n"]);
    assert_eq!(steps.last(), Some(&Step::Malformed));
}

#[test]
fn the_cap_counts_every_head_byte_interim_heads_included() {
    let mut reader = HeadReader::default();
    let filler = format!("X-Pad: {}\r\n", "a".repeat(1000));
    let mut steps = vec![
        reader.line(b"HTTP/1.1 100 Continue\r\n"),
        reader.line(b"\r\n"),
    ];
    steps.push(reader.line(b"HTTP/1.1 200 OK\r\n"));
    while steps.last() == Some(&Step::More) {
        steps.push(reader.line(filler.as_bytes()));
    }
    assert_eq!(steps.last(), Some(&Step::TooLarge));
    let read: usize = 23 + 2 + 17 + (steps.len() - 4) * filler.len();
    assert!(
        read <= MAX_HEAD && read + filler.len() > MAX_HEAD,
        "cut at {read}"
    );
    assert_eq!(reader.line(b"\r\n"), Step::More, "nothing after the cut");
}

#[test]
fn debug_shows_field_names_never_values() {
    let head = Head {
        status: 302,
        fields: vec![("Location".into(), "http://h/x?token=SECRET".into())],
    };
    let text = format!("{head:?}");
    assert!(text.contains("302") && text.contains("Location"), "{text}");
    assert!(!text.contains("SECRET"), "{text}");
}

/// A loopback server answering every connection with `answer(request head, stream)`, recording
/// each request head.
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
}

/// What one [`get`] produced.
struct Got {
    end: StreamEnd,
    head: Option<Head>,
    body: Vec<u8>,
}

fn fetch(url: &str, headers: &[String], budget: Duration, cancel: &AtomicBool) -> Got {
    let (mut head, mut body) = (None, Vec::new());
    let end = get(
        url,
        headers,
        budget,
        Duration::from_secs(5),
        cancel,
        &mut |h| {
            head = Some(h);
            true
        },
        &mut |bytes| {
            body.extend_from_slice(bytes);
            true
        },
    );
    Got { end, head, body }
}

fn plain(url: &str) -> Got {
    fetch(url, &[], Duration::from_secs(10), &AtomicBool::new(false))
}

#[test]
fn a_200_hands_over_the_head_then_the_body() {
    let srv = Server::start(|_, s| {
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nETag: \"abc\"\r\nContent-Length: 5\r\n\r\nhello");
    });
    let headers = vec!["If-None-Match: \"old\"".to_owned()];
    let got = fetch(
        &srv.url("/list.m3u?x=1#frag"),
        &headers,
        Duration::from_secs(10),
        &AtomicBool::new(false),
    );
    assert_eq!(got.end, StreamEnd::Closed);
    let head = got.head.expect("a head");
    assert_eq!((head.status, head.field("etag")), (200, Some("\"abc\"")));
    assert_eq!(got.body, b"hello");
    let request = srv.requests.lock().unwrap()[0].clone();
    assert!(
        request.starts_with("GET /list.m3u?x=1 HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(
        request.contains("\r\nIf-None-Match: \"old\"\r\n"),
        "{request}"
    );
}

#[test]
fn chunked_and_close_delimited_bodies_end_cleanly_and_a_short_one_does_not() {
    let srv = Server::start(|head, s| {
        let reply: &[u8] = if head.starts_with("GET /chunked ") {
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n"
        } else if head.starts_with("GET /close ") {
            b"HTTP/1.0 200 OK\r\n\r\nuntil the end"
        } else {
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nshort"
        };
        let _ = s.write_all(reply);
    });
    let chunked = plain(&srv.url("/chunked"));
    assert_eq!(
        (chunked.end, chunked.body),
        (StreamEnd::Closed, b"abcde".to_vec())
    );
    let close = plain(&srv.url("/close"));
    assert_eq!(
        (close.end, close.body),
        (StreamEnd::Closed, b"until the end".to_vec())
    );
    let short = plain(&srv.url("/short"));
    assert!(
        matches!(short.end, StreamEnd::Transport(f) if f.cause == RequestError::Transport),
        "{:?}",
        short.end
    );
    assert_eq!(short.body, b"short");
}

#[test]
fn a_304_and_a_302_are_heads_and_nothing_is_followed() {
    let srv = Server::start(|head, s| {
        let reply: &[u8] = if head.starts_with("GET /old ") {
            b"HTTP/1.1 302 Found\r\nLocation: /new\r\nContent-Length: 4\r\n\r\nmove"
        } else {
            b"HTTP/1.1 304 Not Modified\r\nETag: \"abc\"\r\n\r\n"
        };
        let _ = s.write_all(reply);
    });
    let moved = plain(&srv.url("/old"));
    assert_eq!(moved.end, StreamEnd::Status(302));
    assert_eq!(
        moved.head.as_ref().and_then(|h| h.field("location")),
        Some("/new")
    );
    assert!(moved.body.is_empty());
    let same = plain(&srv.url("/same"));
    assert_eq!(same.end, StreamEnd::Status(304));
    assert_eq!(
        same.head.as_ref().and_then(|h| h.field("etag")),
        Some("\"abc\"")
    );
    assert_eq!(
        srv.requests.lock().unwrap().len(),
        2,
        "the 302 was not followed"
    );
}

#[test]
fn a_head_over_the_cap_is_refused_before_it_is_handed_over() {
    // Past the cap inside the open's 64 KiB buffer, and past the buffer itself: one outcome.
    let srv = Server::start(|head, s| {
        let lines = if head.starts_with("GET /40 ") { 40 } else { 80 };
        let mut reply = b"HTTP/1.1 200 OK\r\n".to_vec();
        for _ in 0..lines {
            reply.extend_from_slice(format!("X-Pad: {}\r\n", "p".repeat(1000)).as_bytes());
        }
        reply.extend_from_slice(b"Content-Length: 2\r\n\r\nok");
        let _ = s.write_all(&reply);
    });
    for path in ["/40", "/80"] {
        let got = plain(&srv.url(path));
        assert_eq!(got.end, StreamEnd::HeadTooLarge, "{path}");
        assert!(got.head.is_none() && got.body.is_empty(), "{path}");
    }
}

#[test]
fn a_consumer_that_stops_ends_the_request() {
    let srv = Server::start(|_, s| {
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello");
    });
    let end = get(
        &srv.url("/x"),
        &[],
        Duration::from_secs(10),
        Duration::from_secs(5),
        &AtomicBool::new(false),
        &mut |_| false,
        &mut |_| true,
    );
    assert_eq!(end, StreamEnd::Consumer);
}

#[test]
fn the_budget_cuts_a_slow_body() {
    let srv = Server::start(|_, s| {
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n");
        for _ in 0..1000 {
            if s.write_all(b"x").is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    let started = Instant::now();
    let got = fetch(
        &srv.url("/drip"),
        &[],
        Duration::from_millis(800),
        &AtomicBool::new(false),
    );
    assert!(
        matches!(got.end, StreamEnd::Transport(f) if f.cause == RequestError::TimedOut),
        "{:?}",
        got.end
    );
    assert!(!got.body.is_empty());
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn idle_and_cancel_stop_a_server_that_says_nothing() {
    let srv = Server::start(|_, _| std::thread::sleep(Duration::from_secs(10)));
    let started = Instant::now();
    let end = get(
        &srv.url("/hang"),
        &[],
        Duration::from_secs(10),
        Duration::from_millis(500),
        &AtomicBool::new(false),
        &mut |_| true,
        &mut |_| true,
    );
    assert_eq!(end, StreamEnd::Idle);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );

    let cancel = Arc::new(AtomicBool::new(false));
    let setter = {
        let cancel = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            cancel.store(true, Ordering::Release);
        })
    };
    let started = Instant::now();
    let got = fetch(&srv.url("/hang"), &[], Duration::from_secs(10), &cancel);
    setter.join().unwrap();
    assert_eq!(got.end, StreamEnd::Cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn bad_urls_are_refused_before_anything_is_dialled() {
    for url in [
        "ftp://127.0.0.1/x",
        "http://127.0.0.1/a b",
        "http://127.0.0.1/a\r\nX: y",
        "127.0.0.1/x",
        "http:///x",
    ] {
        let got = plain(url);
        assert!(
            matches!(got.end, StreamEnd::Transport(f) if f.cause == RequestError::Transport),
            "{url}: {:?}",
            got.end
        );
        assert!(got.head.is_none(), "{url}");
    }
}

/// The plaintext open logs a non-2xx status and a failed lookup with the request's path. A refresh
/// path carries the account token, so this stream is quiet.
#[test]
fn no_log_line_names_the_path() {
    let srv = Server::start(|_, s| {
        let _ = s.write_all(b"HTTP/1.1 304 Not Modified\r\n\r\n");
    });
    let log = tv_base::eventlog::with_private_log(|| {
        let got = plain(&srv.url("/pl/1234/a1b2c3d4e5f6g/playlist.m3u"));
        assert_eq!(got.end, StreamEnd::Status(304));
        std::fs::read_to_string(tv_base::eventlog::events_log()).unwrap_or_default()
    });
    assert!(!log.contains("a1b2c3d4e5f6g"), "{log}");
}
