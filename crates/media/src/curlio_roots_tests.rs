//! The bundled public roots fallback, media plane: `curlio::CurlSource` opens, reopens and seeks a
//! server whose certificate chains to a root the device's trust store lacks the same way the
//! control plane does (`net_roots_tests.rs`'s half), through `net::roots`'s shared decision. "The
//! device store" is `test_ca_bundle`, the shipped bundle is `roots::test_roots`, and the host name
//! reaches loopback through a resolve pin. Every test holds `testlock::serial()`: both overrides
//! are process-global.

use crate::curlio::test_fixtures::{media_body, pin_to_loopback, PINNED_HOST};
use std::sync::Arc;

use tv_net::net::{curl_ready, roots, mint_ca_issued_cert, mint_cert, spawn_observed, spawn_redirecting, ymd_from_now, TestCaGuard};

/// A TLS double for `host` serving a fresh CA's leaf: the CA's PEM, the port, the accept counter.
fn serve(host: &str) -> (String, u16, Arc<std::sync::atomic::AtomicUsize>) {
    let cert = Arc::new(mint_ca_issued_cert(&[host], ymd_from_now(-1), ymd_from_now(30)));
    let pem = cert.pem.clone();
    let served = spawn_observed(cert, media_body());
    (pem, served.port, served.accepted)
}

#[test]
fn a_media_open_on_a_host_the_device_store_cannot_verify_reads_bytes_through_the_bundle() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let (ca, port, accepted) = serve(PINNED_HOST);
    let _device = TestCaGuard::install(&mint_cert(&["unrelated.invalid"]).pem, "roots-media-ok");
    let _roots = roots::test_roots::Guard::install(&ca, "roots-media-ok");
    pin_to_loopback(PINNED_HOST, port);
    let key = roots::key_of(PINNED_HOST, i32::from(port));
    let _watch = roots::Scoped(key.clone());
    let url = format!("https://{PINNED_HOST}:{port}/video.mkv");

    let mut src = crate::curlio::CurlSource::open(&url, 0).expect("the bundle holds the issuing root");
    assert_eq!(src.status(), 200);
    let mut head = [0u8; 64];
    assert_eq!(src.read(&mut head), 64);
    assert_eq!(head[..], media_body()[..64]);
    assert!(roots::is_latched(&key), "a success through the bundle latches the host");
    assert_eq!(accepted.load(std::sync::atomic::Ordering::Acquire), 2, "the failed strict handshake and the bundle one");

    // A reopen of a latched host starts in roots mode: ONE handshake, not a doomed strict one first.
    drop(src);
    let again = crate::curlio::CurlSource::open(&url, 0).expect("the latched host opens");
    assert_eq!(again.status(), 200);
    assert_eq!(accepted.load(std::sync::atomic::Ordering::Acquire), 3, "exactly one handshake");
}

#[test]
fn a_media_open_without_the_issuing_root_in_the_bundle_fails_as_before() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let (_ca, port, _accepted) = serve(PINNED_HOST);
    let _device = TestCaGuard::install(&mint_cert(&["unrelated.invalid"]).pem, "roots-media-lacks");
    let _roots = roots::test_roots::Guard::install(&mint_cert(&["another-root.invalid"]).pem, "roots-media-lacks");
    pin_to_loopback(PINNED_HOST, port);
    let key = roots::key_of(PINNED_HOST, i32::from(port));
    let _watch = roots::Scoped(key.clone());
    let err = crate::curlio::CurlSource::open(&format!("https://{PINNED_HOST}:{port}/video.mkv"), 0)
        .err()
        .expect("the bundle does not hold the issuing root");
    assert_eq!(err, crate::curlio::OpenErr::Transport(60));
    assert!(!roots::is_latched(&key));
}

#[test]
fn a_media_open_with_no_bundle_on_disk_fails_as_before() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let (_ca, port, _accepted) = serve(PINNED_HOST);
    let _device = TestCaGuard::install(&mint_cert(&["unrelated.invalid"]).pem, "roots-media-absent");
    roots::test_roots::set(Some("/nonexistent/le-roots.pem"));
    pin_to_loopback(PINNED_HOST, port);
    let key = roots::key_of(PINNED_HOST, i32::from(port));
    let _watch = roots::Scoped(key.clone());
    let err = crate::curlio::CurlSource::open(&format!("https://{PINNED_HOST}:{port}/video.mkv"), 0)
        .err()
        .expect("nothing to verify it by");
    roots::test_roots::set(None);
    assert_eq!(err, crate::curlio::OpenErr::Transport(60));
    assert!(!roots::is_latched(&key));
}

/// A TLS double for `host` serving a CA-issued leaf whose dates ended a month ago (a wrong
/// television clock): the certificate, the port, the accept counter.
fn serve_expired(host: &str) -> (Arc<tv_net::net::TestCert>, u16, Arc<std::sync::atomic::AtomicUsize>) {
    let cert = Arc::new(mint_ca_issued_cert(&[host], ymd_from_now(-90), ymd_from_now(-30)));
    let served = spawn_observed(Arc::clone(&cert), media_body());
    (cert, served.port, served.accepted)
}

/// **A wrong clock behind an old trust store.** The device store lacks the issuer (strict fails on
/// it, so roots mode engages), and the clock is wrong too, so the bundle verifies the chain and
/// then refuses the dates. Nothing else may answer for the server: the open fails and the host is
/// not latched.
#[test]
fn a_media_open_whose_bundle_attempt_fails_on_the_date_fails_and_does_not_latch() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let (cert, port, _accepted) = serve_expired(PINNED_HOST);
    let _device = TestCaGuard::install(&mint_cert(&["unrelated.invalid"]).pem, "roots-media-date-nokey");
    let _roots = roots::test_roots::Guard::install(&cert.pem, "roots-media-date-nokey");
    pin_to_loopback(PINNED_HOST, port);
    let key = roots::key_of(PINNED_HOST, i32::from(port));
    let _watch = roots::Scoped(key.clone());
    let err = crate::curlio::CurlSource::open(&format!("https://{PINNED_HOST}:{port}/video.mkv"), 0)
        .err()
        .expect("a date failure is refused by both stores");
    assert_eq!(err, crate::curlio::OpenErr::Transport(60));
    assert!(!roots::is_latched(&key));
}

/// **A latched roots start the bundle then refuses.** The host was served through the bundle, and its
/// certificate has since moved to an issuer the device store trusts but the bundle lacks. The next
/// open or seek starts in roots mode, is refused, and goes strict once in the same open.
#[test]
fn a_media_open_on_a_latched_host_the_bundle_now_refuses_goes_strict_once() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let (ca, port, accepted) = serve(PINNED_HOST);
    let _device = TestCaGuard::install(&ca, "roots-media-latched");
    let _roots = roots::test_roots::Guard::install(&mint_cert(&["another-root.invalid"]).pem, "roots-media-latched");
    pin_to_loopback(PINNED_HOST, port);
    let key = roots::key_of(PINNED_HOST, i32::from(port));
    let _watch = roots::Scoped(key.clone());
    roots::roots_established(&key, Some(20));
    assert!(roots::is_latched(&key));
    let src = crate::curlio::CurlSource::open(&format!("https://{PINNED_HOST}:{port}/video.mkv"), 0)
        .expect("the device store holds the issuer, so the strict retry verifies");
    assert_eq!(src.status(), 200);
    assert!(!roots::is_latched(&key), "the refused latch is gone");
    assert_eq!(accepted.load(std::sync::atomic::Ordering::Acquire), 2, "the refused bundle handshake and the strict one");
}

/// The retry is once: refused by the bundle and by the device store, the open fails.
#[test]
fn a_media_open_on_a_latched_host_both_stores_refuse_fails_after_one_strict_retry() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let (_ca, port, accepted) = serve(PINNED_HOST);
    let _device = TestCaGuard::install(&mint_cert(&["unrelated.invalid"]).pem, "roots-media-latched-both");
    let _roots = roots::test_roots::Guard::install(&mint_cert(&["another-root.invalid"]).pem, "roots-media-latched-both");
    pin_to_loopback(PINNED_HOST, port);
    let key = roots::key_of(PINNED_HOST, i32::from(port));
    let _watch = roots::Scoped(key.clone());
    roots::roots_established(&key, Some(20));
    let err = crate::curlio::CurlSource::open(&format!("https://{PINNED_HOST}:{port}/video.mkv"), 0)
        .err()
        .expect("neither store holds the issuer");
    assert_eq!(err, crate::curlio::OpenErr::Transport(60));
    assert!(!roots::is_latched(&key));
    assert_eq!(accepted.load(std::sync::atomic::Ordering::Acquire), 2, "bundle then strict, and the bundle is not offered again");
}

/// `https://<name>:443@127.0.0.1:<port>/` dials the loopback server, not the name in the userinfo;
/// an authority with userinfo has no latch key, so the bundle is never offered for it
/// (`net::roots::key_of_url`).
#[test]
fn a_media_open_with_a_host_name_in_the_userinfo_never_uses_the_bundle() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let (ca, port, _accepted) = serve("127.0.0.1");
    let _device = TestCaGuard::install(&mint_cert(&["unrelated.invalid"]).pem, "roots-media-userinfo");
    let _roots = roots::test_roots::Guard::install(&ca, "roots-media-userinfo");
    let key = roots::key_of(PINNED_HOST, 443);
    let _watch = roots::Scoped(key.clone());
    let err = crate::curlio::CurlSource::open(&format!("https://{PINNED_HOST}:443@127.0.0.1:{port}/video.mkv"), 0)
        .err()
        .expect("an authority with userinfo is never offered the bundle");
    assert_eq!(err, crate::curlio::OpenErr::Transport(60));
    assert!(!roots::is_latched(&key));
}

/// **A hop takes its own roots decision, not the one of the host that sent it.** An open that went
/// through the bundle (strict failed on the missing issuer) is redirected to another host, and that
/// host is a request of its own: it begins strict, against the device store, fails on the same
/// missing issuer, and is then verified against the bundle for ITSELF. Both hosts end latched, each
/// by its own handshake, and each was dialled exactly twice.
#[test]
fn a_redirect_hop_takes_its_own_roots_decision() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let cert = Arc::new(mint_ca_issued_cert(&[PINNED_HOST, "127.0.0.1"], ymd_from_now(-1), ymd_from_now(30)));
    let target = spawn_observed(Arc::clone(&cert), media_body());
    let redirecting = spawn_redirecting(Arc::clone(&cert), &format!("https://127.0.0.1:{}/video.mkv", target.port));
    pin_to_loopback(PINNED_HOST, redirecting.port);
    let key = roots::key_of(PINNED_HOST, i32::from(redirecting.port));
    let _watch = roots::Scoped(key.clone());
    let target_key = roots::key_of("127.0.0.1", i32::from(target.port));
    let _target_watch = roots::Scoped(target_key.clone());
    let _device = TestCaGuard::install(&mint_cert(&["unrelated.invalid"]).pem, "roots-media-redirect-own");
    let _roots = roots::test_roots::Guard::install(&cert.pem, "roots-media-redirect-own");

    let mut src = crate::curlio::CurlSource::open(&format!("https://{PINNED_HOST}:{}/video.mkv", redirecting.port), 0)
        .expect("each hop is verified through the bundle by its own ladder");
    assert_eq!(src.status(), 200);
    let mut head = [0u8; 64];
    assert_eq!(src.read(&mut head), 64);
    assert!(roots::is_latched(&key), "the first host's own handshake was verified by the bundle");
    assert!(roots::is_latched(&target_key), "and so was the target's, by its own");
    assert_eq!(redirecting.accepted(), 2, "the failed strict handshake and the bundle one");
    assert_eq!(target.accepted(), 2, "the target's own strict failure and bundle handshake");
}

/// **A hop that fails verification is the hop's failure, not its server's.** A server the device
/// store verifies (strict, no roots mode) answers with a redirect to a host whose issuer
/// the device store lacks. The hop's certificate failure belongs to the hop: it must not send the
/// SERVER into roots mode (a retry of the server's own handshake against the bundle, and a log line
/// saying the server's certificate failed the device store, which it did not). The origin server.s
/// connection count is the tell: one, never a second for a retry.
#[test]
fn a_redirect_hop_that_fails_verification_does_not_send_its_server_into_roots_mode() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let origin_cert = Arc::new(mint_ca_issued_cert(&[PINNED_HOST], ymd_from_now(-1), ymd_from_now(30)));
    let elsewhere = Arc::new(mint_ca_issued_cert(&["127.0.0.1"], ymd_from_now(-1), ymd_from_now(30)));
    let target = spawn_observed(Arc::clone(&elsewhere), media_body());
    let redirecting = spawn_redirecting(Arc::clone(&origin_cert), &format!("https://127.0.0.1:{}/video.mkv", target.port));
    pin_to_loopback(PINNED_HOST, redirecting.port);
    // The device store verifies the origin server and not the target; a bundle is on disk, so roots mode
    // is available to a server that is wrongly sent there.
    let _device = TestCaGuard::install(&origin_cert.pem, "roots-media-hop-fails");
    let _roots = roots::test_roots::Guard::install(&origin_cert.pem, "roots-media-hop-fails");
    let key = roots::key_of(PINNED_HOST, i32::from(redirecting.port));
    let _watch = roots::Scoped(key.clone());
    let _target_watch = roots::Scoped(roots::key_of("127.0.0.1", i32::from(target.port)));

    let err = crate::curlio::CurlSource::open(&format!("https://{PINNED_HOST}:{}/video.mkv", redirecting.port), 0)
        .err()
        .expect("the hop's issuer is in neither store");
    assert!(matches!(err, crate::curlio::OpenErr::Transport(60 | 35)), "the hop's own failure: {err:?}");
    assert_eq!(redirecting.accepted(), 1, "the server was verified and asked once; the hop's failure did not retry it");
    assert!(!roots::is_latched(&key), "the server never went through the bundle");
}

/// **A redirect hop is verified by the store ITS host selects.** A server that is in roots mode (its
/// name latched) answers with a redirect to a CDN: a different host, a different issuer, one the
/// firmware's own store holds and the bundle does not. The CDN is not latched, so the hop is a
/// strict request for its own host and the device store verifies it. The server's own mode does not follow the request
/// into the next host's handshake.
#[test]
fn a_redirect_from_a_roots_mode_host_is_verified_against_the_device_store() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let origin_cert = Arc::new(mint_ca_issued_cert(&[PINNED_HOST], ymd_from_now(-1), ymd_from_now(30)));
    let cdn = Arc::new(mint_ca_issued_cert(&["127.0.0.1"], ymd_from_now(-1), ymd_from_now(30)));
    let target = spawn_observed(Arc::clone(&cdn), media_body());
    let redirecting = spawn_redirecting(Arc::clone(&origin_cert), &format!("https://127.0.0.1:{}/video.mkv", target.port));
    pin_to_loopback(PINNED_HOST, redirecting.port);
    // The device store is the CDN's CA; the bundle is the origin server.s.
    let _device = TestCaGuard::install(&cdn.pem, "roots-media-redirect-store");
    let _roots = roots::test_roots::Guard::install(&origin_cert.pem, "roots-media-redirect-store");
    let key = roots::key_of(PINNED_HOST, i32::from(redirecting.port));
    let _watch = roots::Scoped(key.clone());
    let _target_watch = roots::Scoped(roots::key_of("127.0.0.1", i32::from(target.port)));
    roots::roots_established(&key, Some(20));

    let mut src = crate::curlio::CurlSource::open(&format!("https://{PINNED_HOST}:{}/video.mkv", redirecting.port), 0)
        .expect("the CDN's issuer is in the device store, which is what verifies its hop");
    assert_eq!(src.status(), 200);
    let mut head = [0u8; 64];
    assert_eq!(src.read(&mut head), 64);
    assert_eq!(head[..], media_body()[..64]);
}
