//! [`get_tls`] against loopback TLS doubles: the head reaches the caller with its fields before the
//! body, a `304` and a `302` come back as heads (nothing followed), an interim `100` is skipped,
//! the head cap holds, the budget cuts a slow body, cancel ends a silent one, an unverified peer
//! hands over nothing, and [`crate::get::get`] takes the same path for an https URL.
//!
//! Every test that needs libcurl skips (loudly) when none binds, as the sibling suites do.

use super::*;
use crate::get::{Head, MAX_HEAD};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn pem_file(cert: &TestCert, tag: &str) -> std::path::PathBuf {
    let path =
        std::env::temp_dir().join(format!("webosiptv-get-ca-{tag}-{}.pem", std::process::id()));
    std::fs::write(&path, &cert.pem).expect("write scratch CA bundle");
    path
}

/// What one [`get_tls`] produced.
struct Got {
    end: StreamEnd,
    head: Option<Head>,
    body: Vec<u8>,
}

fn run(
    port: u16,
    path: &str,
    cert: &TestCert,
    tag: &str,
    budget: Duration,
    cancel: &AtomicBool,
) -> Got {
    let ca = pem_file(cert, tag);
    let (mut head, mut body) = (None, Vec::new());
    let end = get_tls(
        &format!("https://127.0.0.1:{port}{path}"),
        &["If-None-Match: \"old\"".to_owned()],
        Tls::CaBundle(ca.to_str().unwrap()),
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
    let _ = std::fs::remove_file(&ca);
    Got { end, head, body }
}

fn quick(port: u16, path: &str, cert: &TestCert, tag: &str) -> Got {
    run(
        port,
        path,
        cert,
        tag,
        Duration::from_secs(10),
        &AtomicBool::new(false),
    )
}

/// A double answering every request with `reply`, verbatim.
fn answering(cert: &Arc<TestCert>, reply: &'static [u8]) -> Observed {
    loopback_server::spawn_tls_double(Arc::clone(cert), false, move |_| reply.to_vec())
}

#[test]
fn a_200_hands_over_the_head_then_the_body() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() {
        return;
    }
    let cert = Arc::new(mint_cert(&["127.0.0.1"]));
    let srv = answering(
        &cert,
        b"HTTP/1.1 200 OK\r\nETag: \"e1\"\r\nLast-Modified: Mon, 01 Jan 2024 00:00:00 GMT\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
    );
    let got = quick(srv.port, "/list.m3u?x=1", &cert, "ok");
    assert_eq!(got.end, StreamEnd::Closed);
    let head = got.head.expect("a head");
    assert_eq!(head.status, 200);
    assert_eq!(head.field("etag"), Some("\"e1\""));
    assert_eq!(
        head.field("last-modified"),
        Some("Mon, 01 Jan 2024 00:00:00 GMT")
    );
    assert_eq!(got.body, b"hello");
    let request = String::from_utf8_lossy(&srv.requests.lock().unwrap()[0]).into_owned();
    assert!(request.starts_with("GET /list.m3u?x=1 "), "{request}");
    assert!(request.contains("If-None-Match: \"old\""), "{request}");
}

#[test]
fn a_304_and_a_302_are_heads_and_nothing_is_followed() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() {
        return;
    }
    let cert = Arc::new(mint_cert(&["127.0.0.1"]));
    let moved = spawn_scripted(
        Arc::clone(&cert),
        [
            (
                "/old",
                Reply::redirect(302, "/new").with_body(b"move".to_vec()),
            ),
            ("/new", Reply::ok(b"new".to_vec())),
        ],
    );
    let got = quick(moved.port, "/old", &cert, "302");
    assert_eq!(got.end, StreamEnd::Status(302));
    assert_eq!(
        got.head.as_ref().and_then(|h| h.field("location")),
        Some("/new")
    );
    assert!(got.body.is_empty(), "a redirect's body is not handed over");
    assert_eq!(
        moved.requests.lock().unwrap().len(),
        1,
        "the 302 was not followed"
    );

    let same = answering(
        &cert,
        b"HTTP/1.1 304 Not Modified\r\nETag: \"e1\"\r\nConnection: close\r\n\r\n",
    );
    let got = quick(same.port, "/same", &cert, "304");
    assert_eq!(got.end, StreamEnd::Status(304));
    assert_eq!(
        got.head.as_ref().and_then(|h| h.field("etag")),
        Some("\"e1\"")
    );
}

#[test]
fn an_interim_100_is_not_the_head() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() {
        return;
    }
    let cert = Arc::new(mint_cert(&["127.0.0.1"]));
    let srv = answering(&cert, b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nX-Final: 1\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    let got = quick(srv.port, "/x", &cert, "100");
    assert_eq!(got.end, StreamEnd::Closed);
    let head = got.head.expect("a head");
    assert_eq!((head.status, head.field("x-final")), (200, Some("1")));
    assert_eq!(got.body, b"ok");
}

#[test]
fn a_head_over_the_cap_is_refused_before_it_is_handed_over() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() {
        return;
    }
    let cert = Arc::new(mint_cert(&["127.0.0.1"]));
    let srv = loopback_server::spawn_tls_double(Arc::clone(&cert), false, |_| {
        let mut reply = b"HTTP/1.1 200 OK\r\n".to_vec();
        while reply.len() <= MAX_HEAD {
            reply.extend_from_slice(format!("X-Pad: {}\r\n", "p".repeat(1000)).as_bytes());
        }
        reply.extend_from_slice(b"Content-Length: 2\r\nConnection: close\r\n\r\nok");
        reply
    });
    let got = quick(srv.port, "/big", &cert, "cap");
    assert_eq!(got.end, StreamEnd::HeadTooLarge);
    assert!(got.head.is_none() && got.body.is_empty());
}

#[test]
fn the_budget_cuts_a_slow_body() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() {
        return;
    }
    let cert = Arc::new(mint_cert(&["127.0.0.1"]));
    let drip: Vec<StreamStep> = (0..40)
        .flat_map(|_| [StreamStep::Chunk(b"x".to_vec()), StreamStep::Sleep(100)])
        .collect();
    let srv = spawn_streaming(Arc::clone(&cert), drip);
    let started = Instant::now();
    let got = run(
        srv.port,
        "/drip",
        &cert,
        "budget",
        Duration::from_millis(1200),
        &AtomicBool::new(false),
    );
    assert!(
        matches!(got.end, StreamEnd::Transport(f) if f.cause == RequestError::TimedOut),
        "{:?}",
        got.end
    );
    assert!(got.head.is_some() && !got.body.is_empty());
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn cancel_ends_a_silent_response() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() {
        return;
    }
    let cert = Arc::new(mint_cert(&["127.0.0.1"]));
    let srv = spawn_streaming(Arc::clone(&cert), vec![StreamStep::Hang]);
    let cancel = Arc::new(AtomicBool::new(false));
    let setter = {
        let cancel = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            cancel.store(true, Ordering::Release);
        })
    };
    let started = Instant::now();
    let got = run(
        srv.port,
        "/hang",
        &cert,
        "cancel",
        Duration::from_secs(30),
        &cancel,
    );
    setter.join().unwrap();
    assert_eq!(got.end, StreamEnd::Cancelled);
    assert!(got.head.is_some(), "the head came before the silence");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn an_unverified_peer_hands_over_nothing() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() {
        return;
    }
    let cert = Arc::new(mint_cert(&["127.0.0.1"]));
    let srv = answering(
        &cert,
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nno",
    );
    let mut heads = 0;
    // `Tls::Ca` is the device store, which does not trust a throwaway self-signed certificate.
    let end = get_tls(
        &format!("https://127.0.0.1:{}/x", srv.port),
        &[],
        Tls::Ca,
        Duration::from_secs(10),
        Duration::from_secs(5),
        &AtomicBool::new(false),
        &mut |_| {
            heads += 1;
            true
        },
        &mut |_| true,
    );
    assert!(
        matches!(end, StreamEnd::Transport(f) if f.curl_rc == Some(60)),
        "{end:?}"
    );
    assert_eq!(heads, 0);
}

/// One [`crate::get::get`] of `https://127.0.0.1:{port}/x`; the head's status and the body.
fn dispatch(port: u16) -> (StreamEnd, Option<u16>, Vec<u8>) {
    let (mut status, mut body) = (None, Vec::new());
    let end = crate::get::get(
        &format!("https://127.0.0.1:{port}/x"),
        &[],
        Duration::from_secs(10),
        Duration::from_secs(5),
        &AtomicBool::new(false),
        &mut |h| {
            status = Some(h.status);
            true
        },
        &mut |bytes| {
            body.extend_from_slice(bytes);
            true
        },
    );
    (end, status, body)
}

#[test]
fn get_takes_this_path_for_https() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() {
        return;
    }
    let cert = Arc::new(mint_cert(&["127.0.0.1"]));
    let srv = answering(
        &cert,
        b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nyes",
    );
    let _device = TestCaGuard::install(&cert.pem, "get-dispatch");
    assert_eq!(
        dispatch(srv.port),
        (StreamEnd::Closed, Some(200), b"yes".to_vec())
    );
}

/// The one fallback every request has: a certificate whose issuer the device store lacks is
/// verified again against the bundled roots, before any head reached the caller.
#[test]
fn a_missing_issuer_is_verified_through_the_bundled_roots() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() {
        return;
    }
    let cert = Arc::new(mint_ca_issued_cert(
        &["127.0.0.1"],
        ymd_from_now(-1),
        ymd_from_now(30),
    ));
    let srv = answering(
        &cert,
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
    );
    let _scoped = roots::Scoped(roots::key_of("127.0.0.1", i32::from(srv.port)));
    let _device = TestCaGuard::install(&mint_cert(&["unrelated.invalid"]).pem, "get-roots");
    let (end, status, _) = dispatch(srv.port);
    assert!(
        matches!(end, StreamEnd::Transport(f) if f.curl_rc == Some(60)),
        "{end:?}"
    );
    assert_eq!(
        status, None,
        "no bundle installed: the strict failure stands"
    );

    let _roots = roots::test_roots::Guard::install(&cert.pem, "get-roots");
    assert_eq!(
        dispatch(srv.port),
        (StreamEnd::Closed, Some(200), b"ok".to_vec())
    );
    assert_eq!(
        srv.requests.lock().unwrap().len(),
        1,
        "one request reached the server"
    );
}
