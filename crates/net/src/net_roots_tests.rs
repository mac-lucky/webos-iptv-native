//! The bundled public roots fallback (`roots::Mode::Roots`): an https certificate that chains to a
//! root the television's trust store lacks (Let's Encrypt's newer roots on a 2020 firmware, verify
//! result 20) is verified once more against `pkg/le-roots.pem` — for ANY host — while a date
//! failure, a name mismatch and a self-signed chain never are.
//!
//! The "device store" of these tests is `test_ca_bundle` (what `Tls::CaBundle` hands libcurl), the
//! shipped bundle is `roots::test_roots`, and a host name reaches a loopback server through a
//! resolve pin, so the rule is exercised on the real request path. Every test holds
//! `testlock::serial()`: both overrides are process-global.

use super::*;
use std::sync::Arc;

/// A host name no resolver answers for (RFC 2606), reached through a resolve pin.
const HOST: &str = "addon.roots-test.invalid";

/// The first rung of `roots::after_failure`: a strict attempt failed with `rc` and `verify`.
fn after_strict_failure(rc: c_int, verify: Option<c_long>) -> Option<roots::Mode> {
    roots::after_failure(&roots::Mode::Strict, rc, verify, false)
}

struct Outcome {
    result: Result<Resp, RequestFailure>,
    /// Is the host latched in roots mode after the request?
    latched: bool,
}

/// A leaf for `names` issued by a fresh test CA (whose PEM is `.pem`), valid now.
fn leaf(names: &[&str]) -> Arc<TestCert> {
    Arc::new(mint_ca_issued_cert(names, ymd_from_now(-1), ymd_from_now(30)))
}

/// A trust store that does not hold whatever CA issued [`leaf`]: the device's, in these tests.
fn unrelated_store() -> String {
    mint_cert(&["unrelated.invalid"]).pem
}

/// One request to `https://host:port/manifest.json`, `host` pinned to loopback, against a TLS
/// double serving `cert`, with `device_pem` as the device store and `roots_pem` as the shipped
/// bundle (`None`: no bundle installed).
fn request_to(host: &str, cert: Arc<TestCert>, device_pem: &str, roots_pem: Option<&str>, tag: &str) -> Outcome {
    request_with(host, cert, device_pem, roots_pem, tag, false, |_| {})
}

/// [`request_to`], following redirects when `follow` is set, with `setup` called with the host's
/// [`roots`] key after the loopback port is known and before the request: where a test latches it.
fn request_with(
    host: &str,
    cert: Arc<TestCert>,
    device_pem: &str,
    roots_pem: Option<&str>,
    tag: &str,
    follow: bool,
    setup: impl FnOnce(&str),
) -> Outcome {
    let _device = TestCaGuard::install(device_pem, tag);
    let _roots = roots_pem.map(|p| roots::test_roots::Guard::install(p, tag));
    let served = spawn_observed(cert, b"{}".to_vec());
    let key = roots::key_of(host, i32::from(served.port));
    let _scoped = roots::Scoped(key.clone());
    setup(&key);
    let pin = origin::ResolvePin::for_test(host, i32::from(served.port), std::net::IpAddr::from([127, 0, 0, 1]));
    let result = request_result_evidence(
        &format!("https://{host}:{}/manifest.json", served.port),
        &[],
        "GET",
        None,
        API,
        follow,
        None,
        Some(&resolve::entry_of(&pin)),
    );
    Outcome { result, latched: roots::is_latched(&key) }
}

fn refused_with(out: &Outcome, rcs: &[i32]) {
    let Err(failure) = &out.result else { panic!("the request must be refused") };
    assert!(
        failure.curl_rc.is_some_and(|rc| rcs.contains(&rc)),
        "expected one of {rcs:?}, got {:?}",
        failure.curl_rc
    );
    assert!(!out.latched, "a refusal must not latch the bundle");
}

#[test]
fn a_host_whose_issuer_the_device_lacks_is_verified_through_the_bundle_and_latched() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let cert = leaf(&[HOST]);
    let out = request_to(HOST, Arc::clone(&cert), &unrelated_store(), Some(&cert.pem), "roots-ok");
    let resp = out.result.expect("the bundle holds the issuing root, so the request verifies");
    assert_eq!(resp.status, 200);
    assert!(out.latched, "a success through the bundle latches the host");
}

/// The fallback is not keyed on a name shape: an address literal's certificate is answered the same.
#[test]
fn an_address_literal_host_is_verified_through_the_bundle_too() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let cert = leaf(&["127.0.0.1"]);
    let out = request_to("127.0.0.1", Arc::clone(&cert), &unrelated_store(), Some(&cert.pem), "roots-literal");
    let resp = out.result.expect("the bundle holds the issuing root, so the request verifies");
    assert_eq!(resp.status, 200);
    assert!(out.latched, "a success through the bundle latches the host");
}

/// A request that follows redirects is re-requested against the bundle too, but its success does
/// not latch the first host: the hop that needed the bundle may have been another one.
#[test]
fn a_redirect_following_request_is_verified_through_the_bundle_without_latching() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let cert = leaf(&["127.0.0.1"]);
    let out = request_with("127.0.0.1", Arc::clone(&cert), &unrelated_store(), Some(&cert.pem), "roots-follow", true, |_| {});
    let resp = out.result.expect("the bundle holds the issuing root, so the request verifies");
    assert_eq!(resp.status, 200);
    assert!(!out.latched, "a redirect-following success latches nothing");
}

#[test]
fn without_the_issuing_root_in_the_bundle_the_request_fails_as_before() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let cert = leaf(&[HOST]);
    let other_roots = mint_cert(&["another-root.invalid"]).pem;
    refused_with(&request_to(HOST, Arc::clone(&cert), &unrelated_store(), Some(&other_roots), "roots-lacks"), &[60]);
}

#[test]
fn with_no_bundle_on_disk_the_fallback_does_not_engage() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let cert = leaf(&[HOST]);
    roots::test_roots::set(Some("/nonexistent/le-roots.pem"));
    let out = request_to(HOST, cert, &unrelated_store(), None, "roots-absent");
    roots::test_roots::set(None);
    refused_with(&out, &[60]);
}

#[test]
fn a_date_failure_never_uses_the_bundle() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    // The device store holds the CA, so the date is the ONLY defect; the bundle holds it too, and
    // must still not be consulted (it would fail the date again, so the decision is tested below).
    let cert = expired_leaf(&[HOST]);
    let out = request_to(HOST, Arc::clone(&cert), &cert.pem, Some(&cert.pem), "roots-date");
    refused_with(&out, &[60]);
    assert_eq!(out.result.err().unwrap().verify, Some(10), "the strict date failure decided it");
}

#[test]
fn a_certificate_for_another_name_never_succeeds_through_the_bundle() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let cert = leaf(&["some-other-name.example"]);
    // 51 is what libcurl 7.53.1 answers for a name mismatch; 60 is what 7.62+ answers.
    refused_with(&request_to(HOST, Arc::clone(&cert), &unrelated_store(), Some(&cert.pem), "roots-mismatch"), &[51, 60]);
}

/// A self-signed leaf is refused even when the bundle happens to hold it: a server presenting its
/// own root is not a missing issuer, and is never offered the bundle.
#[test]
fn a_self_signed_leaf_never_uses_the_bundle() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let cert = Arc::new(mint_cert(&[HOST]));
    let out = request_to(HOST, Arc::clone(&cert), &unrelated_store(), Some(&cert.pem), "roots-self-signed");
    refused_with(&out, &[60]);
    assert!(matches!(out.result.err().unwrap().verify, Some(18 | 19)), "a self-signed verify result");
}

/// **A wrong clock behind an old trust store.** The device store lacks the issuer (strict fails
/// with a missing-issuer verify result, so roots mode engages), and the certificate's dates are
/// also outside "now", so the bundle verifies the chain and then refuses the DATES. Nothing more is
/// tried: the request fails with the roots attempt's own date verify result, and nothing latches.
#[test]
fn a_roots_attempt_that_fails_on_the_date_fails_and_does_not_latch() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let cert = expired_leaf(&[HOST]);
    let out = request_to(HOST, Arc::clone(&cert), &unrelated_store(), Some(&cert.pem), "roots-date-roots");
    let failure = out.result.as_ref().err().expect("an expired certificate is refused");
    assert_eq!(failure.curl_rc, Some(60));
    assert_eq!(failure.verify, Some(10), "the roots attempt decided the request, and it failed on the date");
    assert!(!out.latched);
}

/// **A latched roots start the bundle then refuses.** The host was served through the bundle (the
/// latch), and since then its certificate moved to an issuer the device store trusts but the bundle
/// lacks. The first request after that starts in roots mode, which cannot succeed, and must not cost
/// the request: the latch ends AND the request goes strict once, in the same attempt budget.
#[test]
fn a_latched_roots_start_the_bundle_refuses_retries_once_in_strict() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let cert = leaf(&[HOST]);
    let bundle_without_the_issuer = mint_cert(&["another-root.invalid"]).pem;
    let out = request_with(
        HOST, Arc::clone(&cert), &cert.pem, Some(&bundle_without_the_issuer), "roots-latched-strict", false,
        |key| roots::roots_established_at(key, Some(20), std::time::Instant::now()),
    );
    let resp = out.result.expect("the device store holds the issuer, so the strict retry verifies");
    assert_eq!(resp.status, 200);
    assert!(!out.latched, "the refused latch is gone, and a strict success keeps it gone");
}

/// The retry is once: a latched start refused by the bundle, then refused by the device store too
/// (the issuer is in neither), fails — it does not loop, and does not go back to the bundle.
#[test]
fn a_latched_roots_start_refused_by_both_stores_fails_after_one_strict_retry() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let cert = leaf(&[HOST]);
    let bundle_without_the_issuer = mint_cert(&["another-root.invalid"]).pem;
    let out = request_with(
        HOST, Arc::clone(&cert), &unrelated_store(), Some(&bundle_without_the_issuer), "roots-latched-both", false,
        |key| roots::roots_established_at(key, Some(20), std::time::Instant::now()),
    );
    let failure = out.result.as_ref().err().expect("neither store holds the issuer");
    assert_eq!(failure.curl_rc, Some(60));
    assert!(!out.latched, "the latch is over");
}

/// The ladder as a pure decision, [`roots::after_failure`]: what follows each failed attempt.
#[test]
fn the_ladder_after_each_failed_attempt_is_bounded_and_never_repeats_the_bundle() {
    let _serial = tv_base::testlock::serial();
    let _bundle = roots::test_roots::Guard::install(&mint_cert(&["ladder.invalid"]).pem, "roots-ladder");
    let started = |verify| roots::Mode::Roots { path: "p".into(), verify };
    let kind = |m: Option<roots::Mode>| match m {
        None => "none",
        Some(roots::Mode::Strict) => "strict",
        Some(roots::Mode::Roots { .. }) => "roots",
    };
    // After a strict failure: a missing issuer to the bundle, once.
    assert_eq!(kind(roots::after_failure(&roots::Mode::Strict, 60, Some(20), false)), "roots");
    assert_eq!(kind(roots::after_failure(&roots::Mode::Strict, 60, Some(20), true)), "none", "the bundle already refused it");
    assert_eq!(kind(roots::after_failure(&roots::Mode::Strict, 60, Some(10), false)), "none", "a date failure goes nowhere");
    // After a roots attempt reached from a strict failure: nowhere, whatever the failure.
    for (rc, verify) in [(60, Some(20)), (60, Some(10)), (60, Some(18)), (51, None), (28, None), (90, None)] {
        assert_eq!(kind(roots::after_failure(&started(Some(20)), rc, verify, true)), "none", "rc {rc} verify {verify:?}");
    }
    // After a LATCHED roots start (no strict failure led there): a refusal goes strict, once.
    assert_eq!(kind(roots::after_failure(&started(None), 60, Some(20), false)), "strict");
    assert_eq!(kind(roots::after_failure(&started(None), 51, None, false)), "strict");
    assert_eq!(kind(roots::after_failure(&started(None), 60, Some(10), false)), "strict", "the device store may still verify it");
    assert_eq!(kind(roots::after_failure(&started(None), 28, None, false)), "none", "a timeout is not the bundle's refusal");
}

/// **Userinfo cannot key the latch on a host the request never reaches.** In
/// `https://a.example:443@b.example/` libcurl dials `b.example` (everything before the `@` is a
/// user name and a password), but a reading that splits the authority at the first `:` sees the
/// host `a.example`. `key_of_url` is the one reading of a URL the fallback uses, so it refuses any
/// authority that is not a plain `host[:port]`, and such a request is never offered the bundle.
#[test]
fn an_authority_that_is_not_a_plain_host_and_port_has_no_key() {
    for url in [
        "https://a.example:443@b.example/",
        "https://a.example@b.example/",
        "https://user:pw@a.example/",
        "https://a.example:443@b.example",
        "https://a.example?@b.example/",
        "https://a.example#@b.example/",
        "https://a.example\\@b.example/",
        "https://a.example%2e@b.example/",
        "https://a.example /",
        "https://a.example:443 @b.example/",
        "https:///x",
    ] {
        assert_eq!(roots::key_of_url(url), None, "{url}");
    }
    // The plain shapes keep their key (lowercased, the default port filled in), including a
    // bracketed literal.
    for (url, want) in [
        ("https://127.0.0.1:8443/manifest.json", "127.0.0.1:8443"),
        ("HTTPS://Addon.Example.COM/x?y=z@w", "addon.example.com:443"),
        ("https://[::1]:8443/", "::1:8443"),
    ] {
        assert_eq!(roots::key_of_url(url).as_deref(), Some(want), "{url}");
    }
    assert_eq!(roots::key_of_url("http://addon.example.com/"), None, "plaintext has no key");
}

/// The same input through a real request: libcurl dials the loopback server whose certificate the
/// bundle (and only the bundle) would verify, and the request must fail strict.
#[test]
fn a_url_with_userinfo_never_uses_the_bundle() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    let cert = leaf(&["127.0.0.1"]);
    let _device = TestCaGuard::install(&unrelated_store(), "roots-userinfo");
    let _roots = roots::test_roots::Guard::install(&cert.pem, "roots-userinfo");
    let served = spawn_observed(Arc::clone(&cert), b"{}".to_vec());
    let port = served.port;
    let key = roots::key_of(HOST, 443);
    let _scoped = roots::Scoped(key.clone());
    let result = request_result_evidence(
        &format!("https://{HOST}:443@127.0.0.1:{port}/manifest.json"),
        &[],
        "GET",
        None,
        API,
        false,
        None,
        None,
    );
    let failure = result.err().expect("the device store does not hold the issuer and the bundle is not offered");
    assert_eq!(failure.curl_rc, Some(60));
    assert!(!roots::is_latched(&key), "nothing was served against the bundle");
    assert!(!roots::is_latched(&roots::key_of("127.0.0.1", i32::from(port))));
}

/// A bundle that is on disk but cannot verify anything — empty, or unreadable — is not a bundle: with
/// it the fallback would hand libcurl a `CURLOPT_CAINFO` it cannot load (rc 77 on every request whose
/// strict attempt lacked an issuer), where without it the request fails as it always did (rc 60 with
/// the verify result that says why). So `roots_bundle` is only a non-empty readable file.
#[test]
fn an_empty_or_unreadable_bundle_is_no_bundle() {
    use std::os::unix::fs::PermissionsExt;
    let _serial = tv_base::testlock::serial();
    let decides = || after_strict_failure(60, Some(20));
    {
        let _empty = roots::test_roots::Guard::install("", "roots-empty");
        assert_eq!(decides(), None, "an empty file");
    }
    // Unreadable: root reads anything, so this half only means something for an ordinary user.
    if unsafe { libc::geteuid() } != 0 {
        let guard = roots::test_roots::Guard::install(&mint_cert(&["unreadable.invalid"]).pem, "roots-unreadable");
        let path = roots::test_roots::get().expect("installed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(decides(), None, "an unreadable file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(decides().is_some(), "and readable again");
        drop(guard);
    }
}

#[test]
fn the_roots_trigger_is_a_missing_issuer_and_never_a_date_a_name_or_a_self_signed_chain() {
    let _serial = tv_base::testlock::serial();
    let bundle = roots::test_roots::Guard::install(&mint_cert(&["decision.invalid"]).pem, "roots-decision");
    let is_roots = |rc, verify| matches!(after_strict_failure(rc, verify), Some(roots::Mode::Roots { .. }));
    for v in [2, 20, 21] {
        assert!(is_roots(60, Some(v)), "verify {v}");
    }
    for verify in [None, Some(0), Some(9), Some(10), Some(18), Some(19), Some(62), Some(7)] {
        assert_eq!(after_strict_failure(60, verify), None, "no mode at all for {verify:?}");
    }
    for rc in [0, 6, 7, 28, 35, 51, 58, 77, 90] {
        assert_eq!(after_strict_failure(rc, Some(20)), None, "rc {rc}");
    }
    // And with no bundle on disk, the issuer failure has no mode.
    drop(bundle);
    roots::test_roots::set(Some("/nonexistent/le-roots.pem"));
    assert_eq!(after_strict_failure(60, Some(20)), None);
    roots::test_roots::set(None);
}

#[test]
fn the_roots_latch_lapses_clears_on_a_strict_success_and_ends_with_the_bundle() {
    use std::time::{Duration, Instant};
    let _serial = tv_base::testlock::serial();
    let _bundle = roots::test_roots::Guard::install(&mint_cert(&["latch.invalid"]).pem, "roots-latch");
    let key = roots::key_of(HOST, 41003);
    let _scoped = roots::Scoped(key.clone());
    let second = Duration::from_secs(1);
    let is_roots = |mode: &roots::Mode| matches!(mode, roots::Mode::Roots { .. });
    let t0 = Instant::now();
    roots::roots_established_at(&key, Some(20), t0);
    assert!(roots::is_latched(&key));
    assert!(is_roots(&roots::begin_at(&key, t0 + roots::LATCH - second)), "live inside the interval");
    assert_eq!(roots::begin_at(&key, t0 + roots::LATCH), roots::Mode::Strict, "over at the interval");
    assert!(!roots::is_latched(&key), "a lapsed latch is dropped, not just ignored");
    // A later success inside the interval does not slide it.
    roots::roots_established_at(&key, Some(20), t0);
    roots::roots_established_at(&key, Some(20), t0 + roots::LATCH - second);
    assert_eq!(roots::begin_at(&key, t0 + roots::LATCH + second), roots::Mode::Strict);
    // A strict success on the host ends it.
    roots::roots_established_at(&key, Some(20), Instant::now());
    roots::strict_established(&key);
    assert!(!roots::is_latched(&key));
    // A refusal by the bundle ends it.
    roots::roots_established_at(&key, Some(20), Instant::now());
    roots::roots_failed(&key, 60, Some(20));
    assert!(!roots::is_latched(&key));
    // And a latch whose bundle has gone cannot start a request in a mode with nothing to verify by.
    roots::roots_established_at(&key, Some(20), Instant::now());
    roots::test_roots::set(Some("/nonexistent/le-roots.pem"));
    assert_eq!(roots::begin(&key), roots::Mode::Strict);
    assert!(!roots::is_latched(&key));
    roots::test_roots::set(None);
}

/// The bundle itself. These are the file the .ipk ships (`pkg/le-roots.pem`), not a copy of it.
mod shipped_bundle {
    use rustls::pki_types::{CertificateDer, UnixTime};

    const BUNDLE: &str = include_str!("../../../pkg/le-roots.pem");

    /// SHA-256 of each root's DER, lowercase hex: the values `openssl x509 -fingerprint -sha256`
    /// printed for the PEMs at https://letsencrypt.org/certificates/ (the self-signed links), the
    /// first two of which also match macOS's own system roots.
    const EXPECTED: [(&str, &str); 4] = [
        ("ISRG Root X1", "96bcec06264976f37460779acf28c5a7cfe8a3c0aae11a8ffcee05c0bddf08c6"),
        ("ISRG Root X2", "69729b8e15a86efc177a57afb7171dfc64add28c2fca8cf1507e34453ccb1470"),
        ("ISRG Root YR", "e57b7e6f150c419102e8d5c055729ff967b9d1a829bf00cec89ca604ebf4a86f"),
        ("ISRG Root YE", "e14ffcad5b0025731006caa43a121a22d8e9700f4fb9cf852f02a708aa5d5666"),
    ];

    // Public Let's Encrypt TEST sites (https://letsencrypt.org/certificates/ links them): the
    // leaf each serves and the intermediate that issued it. Leaves live a week, so they are
    // verified AT a fixed instant inside their validity, never "now".
    const YR_LEAF: &str = "-----BEGIN CERTIFICATE-----\n\
MIIE1jCCA76gAwIBAgISBmATjcWZybMlJMVR0CqLPIK+MA0GCSqGSIb3DQEBCwUA\n\
MDMxCzAJBgNVBAYTAlVTMRYwFAYDVQQKEw1MZXQncyBFbmNyeXB0MQwwCgYDVQQD\n\
EwNZUjEwHhcNMjYxMDAxMTY0MDI2WhcNMjYxMDA4MDg0MDI1WjAAMIIBIjANBgkq\n\
hkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAvZ5oo2nd9TM4D1XbeikDnrx1jHvjMSAt\n\
kTb8Cyql5iTXs8O9qQocHIMTtgZYkdesR2i4tmTqv2Kbfzgy7OJ1UYxOAm4YI0LD\n\
rvA8p/aBHwKr6AsmwWZmsqV2h7Ppka1TuFyIB/XQAAUtceCj9xGpc8v89a74RmPQ\n\
pUFQnM4NLEovsmGJ//l0OZAlYiBVUO7JhHDMDRVgPcG9GghLcHvWaqN8frbL9lMs\n\
JSCa9ACsOhrV2s4yX5enApz6HHvkCqKwsZ0yxPR1X7YmpDvMV3/BWmaCrdHlx9ia\n\
6lNzgSooXeZ/GJStnwjs7fHW2ByeCaqrjtY9WyFUDvhh5mwJdgyx6wIDAQABo4IC\n\
FTCCAhEwDgYDVR0PAQH/BAQDAgeAMBMGA1UdJQQMMAoGCCsGAQUFBwMBMAwGA1Ud\n\
EwEB/wQCMAAwHwYDVR0jBBgwFoAUHy81vkYUgs1Asa55LFV4+vfUaPswMwYIKwYB\n\
BQUHAQEEJzAlMCMGCCsGAQUFBzAChhdodHRwOi8veXIxLmkubGVuY3Iub3JnLzAx\n\
BgNVHREBAf8EJzAlgiN2YWxpZC55ci50ZXN0LWNlcnRzLmxldHNlbmNyeXB0Lm9y\n\
ZzATBgNVHSAEDDAKMAgGBmeBDAECATAuBgNVHR8EJzAlMCOgIaAfhh1odHRwOi8v\n\
eXIxLmMubGVuY3Iub3JnLzYzLmNybDCCAQwGCisGAQQB1nkCBAIEgf0EgfoA+AB/\n\
ABqLnWsP/r+BtHk5xtIxCobW0QLU8EbiGCyd419eJiXvAAABoPiMRaQACAAABQBH\n\
PahrBAMASDBGAiEAiKJx9aoQHMNHcl/6pusp6JN7TLRfnjZE2/cBnqTrOdoCIQDE\n\
D6KB/nSlHkYzOWiLN63qPfVUQEa8Cq+z7UJ+iBPANAB1ANdtfRDRp/V3wsfpX9cA\n\
v/mCyTNaZeHQswFzF8DIxWl3AAABoPiMSeYAAAQDAEYwRAIgOyHqwuJLhb1ELqav\n\
KCUm+hL18QVQu5++la24DKzt7pwCIDb87lF+rsWVOxAkEwqhDIAalVKNaDBdiJC/\n\
dHAzkjpIMA0GCSqGSIb3DQEBCwUAA4IBAQAnSX+aDNw+nCmTkY5AA4Mq975lIFGx\n\
t0J2PRy1n9HnYk61vL1lUSrjincEkzvVm+h9sMLg5YIQn2T0nOVZsrPXl9VXCfoH\n\
dB6YzH6te3CW9OVw0YPjhk6Sfdd9zyGcBEkJd7n15ss3mZtvXV9VQrRE5Sxa9fTw\n\
2GIRr3tAoWWeqZP//h/C+YaKDHUk3hvj0Y2lyAJsgTm+CDvpIw4gy3g6DT/QOi23\n\
Tje6ZIPx8b8FwgvrEBzlsYLWqCC+oxxphWThPBH+c34nSLrrM0XoFhjB1sS9gACI\n\
tcV7EDnJx/DKC0R7yc5hsTmUXvc0X+lr44XRXJ/dkmP5PSZ058zxfnWP\n\
-----END CERTIFICATE-----";
    const YR_INT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIE2zCCAsOgAwIBAgIRAKICU/FfJpHAXcHOE7m8yk4wDQYJKoZIhvcNAQELBQAw\n\
LjELMAkGA1UEBhMCVVMxDTALBgNVBAoTBElTUkcxEDAOBgNVBAMTB1Jvb3QgWVIw\n\
HhcNMjUwOTAzMDAwMDAwWhcNMjgwOTAyMjM1OTU5WjAzMQswCQYDVQQGEwJVUzEW\n\
MBQGA1UEChMNTGV0J3MgRW5jcnlwdDEMMAoGA1UEAxMDWVIxMIIBIjANBgkqhkiG\n\
9w0BAQEFAAOCAQ8AMIIBCgKCAQEAoVi8X2xCYgMXvJxNPKp/oF13UMgmPABB07VC\n\
LNDtoXmt9luEZNJSBV10VyT1Pz6LD8Zq1d2gc43WNl1AdRrj4sEnazbOiz0nPpmG\n\
Bp2hui49oZtDIY6wdKeZAi5BbNU20CH6RSBBMLSQ9cXrH8dxdv4PAJ45ssGML68U\n\
SE3BsjC2a6cAN9L5CgXVIQi5tfNiTPoFZZ3S0OlXqLmmtdV95udWAb5b6e/F49Di\n\
CsH0Y00Ag72BVIb1hzynmKe+X0mERBTtsb3BwmpV9ipeBjMLoR/D9cHxHQCWoi5l\n\
TmXwY015J5rGelz1nZjJuxc2kioaX29XJBnhMkP531rSdG5uMwIDAQABo4HuMIHr\n\
MA4GA1UdDwEB/wQEAwIBhjATBgNVHSUEDDAKBggrBgEFBQcDATASBgNVHRMBAf8E\n\
CDAGAQH/AgEAMB0GA1UdDgQWBBQfLzW+RhSCzUCxrnksVXj699Ro+zAfBgNVHSME\n\
GDAWgBTe51tg0CJtQCh9Pw0B/qS1UrRRlDAyBggrBgEFBQcBAQQmMCQwIgYIKwYB\n\
BQUHMAKGFmh0dHA6Ly95ci5pLmxlbmNyLm9yZy8wEwYDVR0gBAwwCjAIBgZngQwB\n\
AgEwJwYDVR0fBCAwHjAcoBqgGIYWaHR0cDovL3lyLmMubGVuY3Iub3JnLzANBgkq\n\
hkiG9w0BAQsFAAOCAgEA0+zvMq3kHig1ddTmmm+RibTr9/RpX7k4buanMMRqbV/y\n\
IvP82zAHN3mvaw+cASuVsdpd0ikjhr4hnhJQLQOzOp2ccKrsdGOAgo0vddeISFAq\n\
EWEV4lmUM3vFF796up+bSgmJ1u6RupDCMxDgF8M3eLvGuj6L0lu3zkQ0KuQLnKxL\n\
tB0oQqn1Idg5CuuGpMvQzk29Pa3D/qHurc0EIM9SxukQuJqq63lxsYyRQFU8yMBO\n\
hq1w5LbfaWNRrz1uklOfI/pYkAb2E2MTZrAMQkBIE2S8Jt1F8gRc96o/xOsrgvSk\n\
a84AisX6xq1lz1Z7jGvrnXc4TMcjxZTjiTaihcYI1JIXZiLtEMSCa5l3cu8YWd6z\n\
dLRQlqRdclVjuQfNHawRJ6GWlkK0QJosivTKwdBw3KxEtzGo8yMHERbsy57gP1UX\n\
HOMcmZYQC0gtyR3SxfenIM/MxC3Ia2Ypab/kQ/CTnlIn2KQ5JUC6NYrGCbhFN9bp\n\
5lKJStEwCUnLpntcrXk5XVDCNv/5RyWpRThkGOV7GetKkQ0qAY8hCzWK6oqnAhDZ\n\
cjlYVdWfqOw3DIOX6EDNBgAqHarRVxyF9QZdOaXSyPJ0ueD2BYJEBgaCGQ8rAaU/\n\
Qc123V5LTXDZW4CcsPBDyhy4v+c8hClAyw/IkJlfBqxB9D+/wvIMHgECZ4ptP6o=\n\
-----END CERTIFICATE-----";
    const YE_LEAF: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDaTCCAu+gAwIBAgISBr6hRE8R/J8dXAHJxpe3xQF0MAoGCCqGSM49BAMDMDMx\n\
CzAJBgNVBAYTAlVTMRYwFAYDVQQKEw1MZXQncyBFbmNyeXB0MQwwCgYDVQQDEwNZ\n\
RTIwHhcNMjYxMDAxMTY1MjEwWhcNMjYxMDA4MDg1MjA5WjAAMFkwEwYHKoZIzj0C\n\
AQYIKoZIzj0DAQcDQgAEl3NTn//iiF5kj/+RJgu0a1IGNua9/+AwuDNrxlvvwk/e\n\
ud4gYPv6KP5DuYIQo3TXn24qqHH9UewGxiu7yFaQFqOCAhQwggIQMA4GA1UdDwEB\n\
/wQEAwIHgDATBgNVHSUEDDAKBggrBgEFBQcDATAMBgNVHRMBAf8EAjAAMB8GA1Ud\n\
IwQYMBaAFLlZ8o7PIvCG0zdI/3YUGLqC2FWHMDMGCCsGAQUFBwEBBCcwJTAjBggr\n\
BgEFBQcwAoYXaHR0cDovL3llMi5pLmxlbmNyLm9yZy8wMQYDVR0RAQH/BCcwJYIj\n\
dmFsaWQueWUudGVzdC1jZXJ0cy5sZXRzZW5jcnlwdC5vcmcwEwYDVR0gBAwwCjAI\n\
BgZngQwBAgEwLwYDVR0fBCgwJjAkoCKgIIYeaHR0cDovL3llMi5jLmxlbmNyLm9y\n\
Zy8xMTcuY3JsMIIBCgYKKwYBBAHWeQIEAgSB+wSB+AD2AHUA2AlVO5RPev/IFhlv\n\
lE+Fq7D4/F6HVSYPFdEucrtFSxQAAAGg+JcAlgAABAMARjBEAiAczcgvMdqT3O2A\n\
MW8eP2mMNL4rNNsYVElEWjyII03qJwIgfn/goeDtbmxvw/yPGrwGN7k73VT0NsA3\n\
qLS1Pyqr/ugAfQAm42RuWGkhI7w0P0ckNZs3ks0kWojYFdOTM/2ZGKtHIwAAAaD4\n\
lvYTAAgAAAUASLsbngQDAEYwRAIgf1gJkGXCpbJvA+X27f8BxS13jZ8srPz57Kz9\n\
CBvU12sCIHdyD0AX1iRuNcdMhG+Y5ZKDYjuMFR8nHmabxkeC+Xl1MAoGCCqGSM49\n\
BAMDA2gAMGUCMQDTDuW9H521iZAqN6XCsBnaPx/sPCCar/2OLgIjaHk03ntssBd3\n\
DPgp9YdLmMLl6dcCMCMUE75QavgL2QHb2fwte70wc42VUpUQIBshV2yaLeQ0VkdC\n\
FfJo+B1QH3xt20a42w==\n\
-----END CERTIFICATE-----";
    const YE_INT: &str = "-----BEGIN CERTIFICATE-----\n\
MIICjDCCAhGgAwIBAgIQTfOxXdbAeExQfNN7WObxFTAKBggqhkjOPQQDAzAuMQsw\n\
CQYDVQQGEwJVUzENMAsGA1UEChMESVNSRzEQMA4GA1UEAxMHUm9vdCBZRTAeFw0y\n\
NTA5MDMwMDAwMDBaFw0yODA5MDIyMzU5NTlaMDMxCzAJBgNVBAYTAlVTMRYwFAYD\n\
VQQKEw1MZXQncyBFbmNyeXB0MQwwCgYDVQQDEwNZRTIwdjAQBgcqhkjOPQIBBgUr\n\
gQQAIgNiAARxmrQzkdbEEL3MqXt3dJQttYc47axkdDTHud5TPqM2z5uSD5cmk0Wr\n\
HlWXvnlvqBLqiB34kluxIbmMyAiq3/YD6e80/vV259K8XQIdjFXloYOa0mIU71f7\n\
HQ09PvYDlw+jge4wgeswDgYDVR0PAQH/BAQDAgGGMBMGA1UdJQQMMAoGCCsGAQUF\n\
BwMBMBIGA1UdEwEB/wQIMAYBAf8CAQAwHQYDVR0OBBYEFLlZ8o7PIvCG0zdI/3YU\n\
GLqC2FWHMB8GA1UdIwQYMBaAFKPIJlqOoUzQNWP8myPIOq5W809WMDIGCCsGAQUF\n\
BwEBBCYwJDAiBggrBgEFBQcwAoYWaHR0cDovL3llLmkubGVuY3Iub3JnLzATBgNV\n\
HSAEDDAKMAgGBmeBDAECATAnBgNVHR8EIDAeMBygGqAYhhZodHRwOi8veWUuYy5s\n\
ZW5jci5vcmcvMAoGCCqGSM49BAMDA2kAMGYCMQDIcnw5dcZLN9ffynXnnkLD/itS\n\
JEycJPb3sRkzeqBowup7vOsAwaqoCnNn/jh9wycCMQCJM6CPlaOC4pQYYbJtVPYb\n\
DKrIb2EKk5NpOpE6/XttQYZV/3gilB9l+Cc/DOVwmyg=\n\
-----END CERTIFICATE-----";
    /// 2026-10-02T12:00:00Z, inside both leaves' validity.
    const AT: u64 = 1790942400;

    fn der_of(pem: &str) -> Vec<u8> {
        let body: String = pem.lines().filter(|l| !l.starts_with("-----") && !l.starts_with('#')).collect();
        tv_base::b64::decode(&body).expect("PEM body is base64")
    }

    fn bundle_ders() -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut rest = BUNDLE;
        while let Some(start) = rest.find("-----BEGIN CERTIFICATE-----") {
            let end = rest[start..].find("-----END CERTIFICATE-----").expect("END line") + start;
            out.push(der_of(&rest[start..end]));
            rest = &rest[end + "-----END CERTIFICATE-----".len()..];
        }
        out
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn the_bundle_holds_exactly_the_four_expected_roots() {
        let got: Vec<String> = bundle_ders().iter().map(|d| hex(&tv_base::sha256::sha256(d))).collect();
        let want: Vec<&str> = EXPECTED.iter().map(|(_, fp)| *fp).collect();
        assert_eq!(got, want, "in this order: {:?}", EXPECTED.map(|(n, _)| n));
        assert_eq!(BUNDLE.matches("BEGIN CERTIFICATE").count(), 4);
    }

    fn store(ders: Vec<Vec<u8>>) -> rustls::RootCertStore {
        let mut roots = rustls::RootCertStore::empty();
        for d in ders {
            roots.add(CertificateDer::from(d)).expect("a usable trust anchor");
        }
        roots
    }

    fn verifies(roots: &rustls::RootCertStore, leaf: &str, intermediate: &str) -> Result<(), rustls::Error> {
        let algs = rustls::crypto::ring::default_provider().signature_verification_algorithms;
        let ee = CertificateDer::from(der_of(leaf));
        let parsed = rustls::server::ParsedCertificate::try_from(&ee)?;
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &parsed,
            roots,
            &[CertificateDer::from(der_of(intermediate))],
            UnixTime::since_unix_epoch(std::time::Duration::from_secs(AT)),
            algs.all,
        )
    }

    /// The point of the bundle: `leaf -> YR1 -> Root YR` (and the ECDSA twin) verifies against it,
    /// and verifies against NOTHING the old firmware stores hold (X1 and X2 alone) — the same
    /// failure, an unknown issuer, the television reports as OpenSSL result 20.
    #[test]
    fn the_new_roots_chain_verifies_against_the_bundle_and_not_against_the_older_ones() {
        let all = store(bundle_ders());
        let old = store(bundle_ders().into_iter().take(2).collect());
        for (name, leaf, int) in [("YR", YR_LEAF, YR_INT), ("YE", YE_LEAF, YE_INT)] {
            verifies(&all, leaf, int).unwrap_or_else(|e| panic!("{name} chain against the bundle: {e:?}"));
            assert!(verifies(&old, leaf, int).is_err(), "{name} chain must NOT verify against X1+X2 only");
        }
    }
}

// ---- the trust verdict a failed request reports (`RequestFailure::untrusted_chain`) ---------------

#[test]
fn only_a_missing_issuer_or_a_self_signed_chain_under_rc_60_is_an_untrusted_chain() {
    for verify in [2, 18, 19, 20, 21] {
        assert_eq!(roots::untrusted_chain_verify(60, Some(verify)), Some(verify as u8), "verify {verify}");
    }
    // 9 and 10 are the wrong-clock case; the rest are a name mismatch, a bad signature or "not reported" — none of them says the store lacks a root.
    for verify in [None, Some(0), Some(7), Some(9), Some(10), Some(17), Some(22), Some(62), Some(-1), Some(274)] {
        assert_eq!(roots::untrusted_chain_verify(60, verify), None, "verify {verify:?}");
    }
    for rc in [0, 6, 7, 28, 35, 51, 58, 77, 90] {
        assert_eq!(roots::untrusted_chain_verify(rc, Some(20)), None, "rc {rc}");
    }
    let failure = |curl_rc, verify| RequestFailure { cause: RequestError::Transport, status: None, body_limit: None, curl_rc, verify };
    assert_eq!(failure(Some(60), Some(20)).untrusted_chain(), Some(20));
    assert_eq!(failure(Some(60), Some(10)).untrusted_chain(), None);
    assert_eq!(failure(None, Some(20)).untrusted_chain(), None);
}

/// The verify result has to survive the trip from libcurl's handle to the failure the layers above
/// read: a store that lacks the issuer is untrusted whether or not the bundle was tried, and a date
/// failure under a store that DOES hold the issuer is not.
#[test]
fn a_real_failed_handshake_reports_its_x509_verify_result_on_the_failure() {
    let _serial = tv_base::testlock::serial();
    if !curl_ready() { return; }
    // No bundle on disk: the strict failure is the final one, so this is what a build without
    // `le-roots.pem` reports.
    let cert = leaf(&[HOST]);
    roots::test_roots::set(Some("/nonexistent/le-roots.pem"));
    let out = request_to(HOST, Arc::clone(&cert), &unrelated_store(), None, "roots-verify-issuer");
    roots::test_roots::set(None);
    let failure = out.result.err().expect("an unknown issuer is refused");
    assert_eq!(failure.curl_rc, Some(60));
    assert!(matches!(failure.verify, Some(2 | 20 | 21)), "an unknown issuer: {:?}", failure.verify);
    assert_eq!(failure.untrusted_chain(), failure.verify.map(|v| v as u8));
    assert!(failure.untrusted_chain().is_some());
    // The bundle was tried and did not hold the root either: still the same verdict.
    let other_roots = mint_cert(&["another-root.invalid"]).pem;
    let out = request_to(HOST, Arc::clone(&cert), &unrelated_store(), Some(&other_roots), "roots-verify-lacks");
    assert!(out.result.err().expect("refused").untrusted_chain().is_some(), "a failed roots retry is still untrusted");
    // A date failure with the issuer trusted is the clock's, never the store's.
    let expired = expired_leaf(&[HOST]);
    let out = request_to(HOST, Arc::clone(&expired), &expired.pem, None, "roots-verify-date");
    let failure = out.result.err().expect("an expired leaf is refused");
    assert_eq!(failure.curl_rc, Some(60));
    assert_eq!(failure.verify, Some(10));
    assert_eq!(failure.untrusted_chain(), None);
}
