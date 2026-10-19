//! **HTTP redirects on the plaintext MEDIA path** — where a `3xx` answer to a plaintext media
//! open is turned into the next request. `is_redirect`, `resolve_location` and `MAX_HOPS` are
//! public so a client that follows https hops itself applies the same rules.
//!
//! A media server commonly answers a stream URL with a `302` to a presigned CDN URL on another
//! origin — typically https, sometimes a relative `Location`. The socket client in [`super`] is one
//! request per open and cannot speak TLS, so following is a loop ABOVE it: [`open_following`]
//! re-opens the same [`HttpStream`] for each plaintext hop and stops at the first https hop, handing
//! that target back for the caller to open over TLS.
//!
//! Credentials: the ORIGINAL request's credential header block rides a hop only when the hop's
//! scheme+host+port equal the ORIGINAL origin's. A cross-origin hop is requested exactly as its
//! `Location` spells it — a presigned URL carries its own authority, and a credential must never
//! reach a third party. A `Range` is not a credential and rides every hop. Log lines carry an
//! origin and the path's extension ([`super::log_endpoint`]), never a path segment or a query.
//!
//! **Every plaintext hop that would carry a credential asks the credential authority first** — the
//! first request, every followed hop, and every reopen a seek makes. The authority is the caller's:
//! this transport names no layer above it, so each [`Request`] carries it as a [`CredentialGate`].
//! A plaintext media stream whose permission is withdrawn therefore stops at its next request:
//! [`FollowError::Refused`], with nothing dialled.
//!
//! A caller whose contract confines it to one origin (an HLS playlist whose children must stay on
//! its own origin) sets [`Request::same_origin_only`], and a hop elsewhere (a scheme change
//! included) is refused before anything is dialled there.
//!
//! The libcurl client (`crate::net`) does not come through here; its redirect policy is the
//! per-request `follow_redirects` option, off by default.

use std::ffi::CString;
use std::os::raw::c_int;
use std::time::Instant;

use super::{
    hs_redirect_location, http_open_with_timeouts, log_endpoint, HttpOpenError, HttpStream,
    CONNECT_TIMEOUT_MS, MEDIA_RECV_TIMEOUT_MS, MEDIA_SEND_TIMEOUT_MS,
};
use tv_base::checkpoint::Checkpoint;
use crate::net::origin::{Origin, Scheme};

/// Hops followed after the first request. The first request plus this many redirects is the most
/// a media open will send before failing with [`FollowError::TooManyHops`]. A TLS client that
/// follows the https hops itself counts them against this too; a target handed over from here
/// starts a fresh count there.
pub const MAX_HOPS: u32 = 5;

/// The statuses that carry a `Location` to re-request with the same method. 300 and 304 are not
/// redirects in that sense; 305/306 are deprecated.
pub fn is_redirect(status: c_int) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// One request target: an origin plus an absolute path with optional query (no fragment).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub origin: Origin,
    pub path: String,
}

impl Target {
    /// The full URL, for the curl source. Carries whatever query the target has — never log it.
    pub fn url(&self) -> String {
        format!("{}{}", self.origin.base(), self.path)
    }
}

/// Where a followed open ended.
#[derive(Debug)]
pub enum Opened {
    /// The stream holds a 2xx response for this target — the EFFECTIVE URL, which every later
    /// reopen (a Range seek) must use.
    Socket(Target),
    /// The chain reached an https target. Nothing was dialled for it: the socket client cannot
    /// speak TLS, so the caller opens [`Target::url`] through a TLS client.
    Tls(Target),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FollowError {
    /// A request of the chain failed; the stream's status field still holds its code.
    Open(HttpOpenError),
    /// More than [`MAX_HOPS`] redirects.
    TooManyHops,
    /// A hop off the original origin, for a [`Request::same_origin_only`] request.
    LeftOrigin,
    /// The hop would carry a credential to a plaintext origin the credential authority does not
    /// admit (never permitted, or permission withdrawn since the stream opened). Nothing was dialled.
    Refused,
    /// A redirect with no `Location`, or one this client cannot request (another scheme, a
    /// malformed authority, control bytes).
    BadLocation(c_int),
}

/// The credential authority, as the caller's own decision: may a request for `path` carrying
/// `headers` (the `Name: value` lines of its credential block) go to this origin? Asked before every
/// plaintext hop that would carry one, so a withdrawn permission stops the very next request. A plain
/// function pointer, not a lookup: the transport takes the decision as a value and never names the
/// layer that makes it.
pub type CredentialGate = fn(&Origin, &str, &[&str]) -> bool;

/// One media GET to be opened with redirects followed.
pub struct Request<'a> {
    pub origin: &'a Origin,
    /// Absolute path plus optional query.
    pub path: &'a str,
    /// A credential header block (`Name: value\r\n`…), sent to the original origin only.
    pub credentials: Option<&'a str>,
    /// `Range: bytes=N-` on every hop when set.
    pub range_from: Option<i64>,
    /// Absolute setup deadline for the whole chain (connect/send/headers of every hop).
    pub deadline: Option<Instant>,
    /// Refuse, undialled, any hop whose scheme+host+port differ from [`Self::origin`].
    pub same_origin_only: bool,
    /// Asked before every plaintext hop that would carry a credential; see [`CredentialGate`].
    pub credential_gate: CredentialGate,
}

/// Open `req` on `hs`, following plaintext redirects. See the module doc for the credential rule.
pub fn open_following(
    hs: *mut HttpStream,
    req: &Request,
    checkpoint: &mut dyn Checkpoint,
) -> Result<Opened, FollowError> {
    let mut cur = Target {
        origin: req.origin.clone(),
        path: req.path.to_owned(),
    };
    let mut hop: u32 = 0;
    loop {
        if cur.origin.is_tls() {
            return Ok(Opened::Tls(cur));
        }
        let same = same_origin(&cur.origin, req.origin);
        let mut extra = String::new();
        if same {
            if let Some(c) = req.credentials {
                extra.push_str(c);
            }
        }
        if !(req.credential_gate)(&cur.origin, &cur.path, &header_lines(&extra)) {
            tv_base::eventlog::log(&format!(
                "stream: {}{} REFUSED: a credential may not travel to this plaintext origin",
                cur.origin.log_form(),
                log_endpoint(&cur.path)
            ));
            return Err(FollowError::Refused);
        }
        if let Some(at) = req.range_from {
            extra.push_str(&format!("Range: bytes={at}-\r\n"));
        }
        let (Ok(host), Ok(path), Ok(extra)) = (
            CString::new(cur.origin.host()),
            CString::new(cur.path.as_str()),
            CString::new(extra),
        ) else {
            return Err(FollowError::Open(HttpOpenError::Transport));
        };
        let extra_ptr = if extra.as_bytes().is_empty() {
            std::ptr::null()
        } else {
            extra.as_ptr()
        };
        let opened = http_open_with_timeouts(
            hs,
            host.as_ptr(),
            cur.origin.port() as c_int,
            path.as_ptr(),
            extra_ptr,
            "GET",
            CONNECT_TIMEOUT_MS,
            MEDIA_RECV_TIMEOUT_MS,
            MEDIA_SEND_TIMEOUT_MS,
            req.deadline,
            req.deadline.is_some(),
            checkpoint,
        );
        let status = match opened {
            Ok(()) => return Ok(Opened::Socket(cur)),
            Err(HttpOpenError::Status(status)) if is_redirect(status) => status,
            Err(e) => return Err(FollowError::Open(e)),
        };
        let next = hs_redirect_location(hs).and_then(|loc| resolve_location(&cur, &loc));
        let Some(next) = next else {
            tv_base::eventlog::log(&format!(
                "stream: redirect {status} from {}{} has no usable Location",
                cur.origin.log_form(),
                log_endpoint(&cur.path)
            ));
            return Err(FollowError::BadLocation(status));
        };
        if hop >= MAX_HOPS {
            tv_base::eventlog::log(&format!(
                "stream: redirect {status} -> {}{} REFUSED: more than {MAX_HOPS} hops",
                next.origin.log_form(),
                log_endpoint(&next.path)
            ));
            return Err(FollowError::TooManyHops);
        }
        hop += 1;
        let same = same_origin(&next.origin, req.origin);
        if !same && req.same_origin_only {
            tv_base::eventlog::log(&format!(
                "stream: redirect {status} -> {}{} REFUSED: this request may not leave {}",
                next.origin.log_form(),
                log_endpoint(&next.path),
                req.origin.log_form()
            ));
            return Err(FollowError::LeftOrigin);
        }
        tv_base::eventlog::log(&format!(
            "stream: redirect {status} -> {}{} hop={hop} same_origin={same}",
            next.origin.log_form(),
            log_endpoint(&next.path)
        ));
        cur = next;
    }
}

/// A `Name: value\r\n` header block as the lines the credential check reads.
fn header_lines(block: &str) -> Vec<&str> {
    block.split("\r\n").filter(|line| !line.is_empty()).collect()
}

/// Same scheme, host (ASCII case-insensitive) and port.
pub fn same_origin(a: &Origin, b: &Origin) -> bool {
    a.scheme() == b.scheme() && a.port() == b.port() && a.host().eq_ignore_ascii_case(b.host())
}

/// Resolve a `Location` against the target that answered it (RFC 3986 §5.2, restricted to what
/// an HTTP client needs). `None` for anything this client cannot request: a scheme other than
/// http/https, a malformed authority, or any whitespace/control byte (which would otherwise be
/// written into a request line).
pub fn resolve_location(base: &Target, location: &str) -> Option<Target> {
    let loc = location.trim_matches(|c| c == ' ' || c == '\t');
    if loc.is_empty() || loc.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return None;
    }
    let loc = loc.split('#').next().unwrap_or("");
    if loc.is_empty() {
        return None;
    }
    let lower = loc.get(..8).unwrap_or(loc).to_ascii_lowercase();
    let (scheme, rest) = if lower.starts_with("https://") {
        (Some(Scheme::Https), &loc[8..])
    } else if lower.starts_with("http://") {
        (Some(Scheme::Http), &loc[7..])
    } else if let Some(rest) = loc.strip_prefix("//") {
        (Some(base.origin.scheme()), rest)
    } else {
        (None, loc)
    };
    if let Some(scheme) = scheme {
        let end = rest.find(['/', '?']).unwrap_or(rest.len());
        let origin = parse_authority(scheme, &rest[..end])?;
        let tail = &rest[end..];
        let path = if tail.starts_with('/') {
            tail.to_owned()
        } else {
            format!("/{tail}")
        };
        return Some(Target {
            origin,
            path: normalize(&path),
        });
    }
    // Relative. A colon before the first '/' or '?' is another scheme (`ftp:`, `data:`).
    let first = loc.find(['/', '?']).unwrap_or(loc.len());
    if loc[..first].contains(':') {
        return None;
    }
    let base_path = base.path.split('?').next().unwrap_or("/");
    let path = if loc.starts_with('/') {
        loc.to_owned()
    } else if loc.starts_with('?') {
        format!("{base_path}{loc}")
    } else {
        let dir = match base_path.rfind('/') {
            Some(i) => &base_path[..=i],
            None => "/",
        };
        format!("{dir}{loc}")
    };
    Some(Target {
        origin: base.origin.clone(),
        path: normalize(&path),
    })
}

/// `host[:port]` or `[v6][:port]`, userinfo refused; the scheme's default port when none.
fn parse_authority(scheme: Scheme, auth: &str) -> Option<Origin> {
    if auth.contains('@') {
        return None;
    }
    let (host, port) = if let Some(rest) = auth.strip_prefix('[') {
        let close = rest.find(']')?;
        let after = &rest[close + 1..];
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p),
            None if after.is_empty() => None,
            None => return None,
        };
        (&rest[..close], port)
    } else {
        match auth.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (auth, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        None | Some("") => {
            if scheme.is_tls() {
                443
            } else {
                80
            }
        }
        Some(p) => match p.parse::<u16>() {
            Ok(n) if n > 0 => n as i32,
            _ => return None,
        },
    };
    Some(Origin::new(scheme, host, port))
}

/// Remove `.` and `..` segments from the path part (RFC 3986 §5.2.4); the query is untouched.
fn normalize(path_and_query: &str) -> String {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };
    let segs: Vec<&str> = path.split('/').collect();
    let mut out: Vec<&str> = Vec::new();
    let last = segs.len().saturating_sub(1);
    for (i, s) in segs.iter().enumerate().skip(1) {
        match *s {
            "." => {
                if i == last {
                    out.push("");
                }
            }
            ".." => {
                out.pop();
                if i == last {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    let mut p = format!("/{}", out.join("/"));
    if let Some(q) = query {
        p.push('?');
        p.push_str(q);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(scheme: Scheme, host: &str, port: i32, path: &str) -> Target {
        Target {
            origin: Origin::new(scheme, host, port),
            path: path.into(),
        }
    }

    /// A gate that admits everything: these tests grade where credentials go, not who may send them.
    fn allow_all(_: &Origin, _: &str, _: &[&str]) -> bool {
        true
    }

    fn media() -> Target {
        base(
            Scheme::Http,
            "192.168.1.10",
            8080,
            "/streams/1/file.mp4?sig=abc",
        )
    }

    #[test]
    fn redirect_statuses() {
        for s in [301, 302, 303, 307, 308] {
            assert!(is_redirect(s), "{s}");
        }
        for s in [200, 206, 300, 304, 305, 400] {
            assert!(!is_redirect(s), "{s}");
        }
    }

    #[test]
    fn an_absolute_https_location_is_a_tls_target_on_443() {
        let t = resolve_location(&media(), "https://cdn.example.com/v/1.mp4?sig=a&e=2").unwrap();
        assert!(t.origin.is_tls());
        assert_eq!(t.origin.host(), "cdn.example.com");
        assert_eq!(t.origin.port(), 443);
        assert_eq!(t.path, "/v/1.mp4?sig=a&e=2");
        assert_eq!(t.url(), "https://cdn.example.com:443/v/1.mp4?sig=a&e=2");
        assert!(!same_origin(&t.origin, &media().origin));
    }

    #[test]
    fn an_absolute_http_location_defaults_to_80_and_keeps_explicit_ports() {
        let t = resolve_location(&media(), "HTTP://cdn.example.com?x=1").unwrap();
        assert_eq!((t.origin.port(), t.path.as_str()), (80, "/?x=1"));
        let t = resolve_location(&media(), "http://[::1]:8080/a#frag").unwrap();
        assert_eq!((t.origin.host(), t.origin.port()), ("::1", 8080));
        assert_eq!(t.path, "/a");
        let same = resolve_location(&media(), "http://192.168.1.10:8080/x").unwrap();
        assert!(same_origin(&same.origin, &media().origin));
    }

    #[test]
    fn relative_locations_resolve_against_the_answering_target() {
        let b = media();
        assert_eq!(resolve_location(&b, "/a/b?c=1").unwrap().path, "/a/b?c=1");
        assert_eq!(
            resolve_location(&b, "other.mp4").unwrap().path,
            "/streams/1/other.mp4"
        );
        assert_eq!(
            resolve_location(&b, "../2/./x.mp4").unwrap().path,
            "/streams/2/x.mp4"
        );
        assert_eq!(
            resolve_location(&b, "?download=1").unwrap().path,
            "/streams/1/file.mp4?download=1"
        );
        let sr = resolve_location(&b, "//cdn.example.com/z").unwrap();
        assert_eq!(sr.origin.scheme(), Scheme::Http);
        assert_eq!(
            (sr.origin.host(), sr.origin.port()),
            ("cdn.example.com", 80)
        );
        assert!(same_origin(
            &resolve_location(&b, "x").unwrap().origin,
            &b.origin
        ));
    }

    #[test]
    fn unusable_locations_are_refused() {
        let b = media();
        for bad in [
            "",
            "   ",
            "ftp://x/y",
            "data:text/plain,hi",
            "http://",
            "http://h:0/x",
            "http://h:99999/x",
            "http://user:pw@h/x",
            "http://[::1/x",
            "/a\r\nX-Injected: 1",
            "/a b",
        ] {
            assert_eq!(resolve_location(&b, bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_tls_origin_is_handed_back_without_dialling() {
        let origin = Origin::new(Scheme::Https, "127.0.0.1", 1);
        let mut hs = super::super::http_stream_boxed();
        let req = Request {
            origin: &origin,
            path: "/x",
            credentials: None,
            range_from: None,
            deadline: None,
            same_origin_only: false,
            credential_gate: allow_all,
        };
        match open_following(&mut *hs, &req, &mut tv_base::checkpoint::NoCheckpoint) {
            Ok(Opened::Tls(t)) => assert_eq!(t.url(), "https://127.0.0.1:1/x"),
            other => panic!("{other:?}"),
        }
    }

    /// The credential gate is asked before anything is dialled, and its refusal ends the chain.
    /// Port 1 has no listener, so a dial would have come back as `Open(Transport)` instead.
    #[test]
    fn a_refusing_credential_gate_stops_the_request_before_it_dials() {
        fn refuse(_: &Origin, _: &str, _: &[&str]) -> bool {
            false
        }
        let origin = Origin::http("127.0.0.1", 1);
        let mut hs = super::super::http_stream_boxed();
        let req = Request {
            origin: &origin,
            path: "/x",
            credentials: Some("Authorization: Bearer secret\r\n"),
            range_from: None,
            deadline: None,
            same_origin_only: false,
            credential_gate: refuse,
        };
        match open_following(&mut *hs, &req, &mut tv_base::checkpoint::NoCheckpoint) {
            Err(FollowError::Refused) => {}
            other => panic!("{other:?}"),
        }
    }

    /// A credential HEADER block stays with the original origin; a `Range` rides every hop.
    #[test]
    fn credential_headers_do_not_cross_origins_but_the_range_does() {
        use std::io::{Read, Write};
        let _serial = tv_base::testlock::serial();
        let serve = |reply: Vec<u8>| {
            let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = srv.local_addr().unwrap().port();
            let h = std::thread::spawn(move || {
                // A blocking listener: the accepted socket is blocking too.
                let (mut s, _) = srv.accept().unwrap();
                let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                let mut raw = Vec::new();
                let mut c = [0u8; 2048];
                while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut c) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => raw.extend_from_slice(&c[..n]),
                    }
                }
                let _ = s.write_all(&reply);
                String::from_utf8_lossy(&raw).into_owned()
            });
            (port, h)
        };
        let (cdn, cdn_h) = serve(
            b"HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                .to_vec(),
        );
        let (first, first_h) = serve(
            format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{cdn}/v\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .into_bytes(),
        );
        let origin = Origin::http("127.0.0.1", first as i32);
        let mut hs = super::super::http_stream_boxed();
        let req = Request {
            origin: &origin,
            path: "/p",
            credentials: Some("Authorization: Bearer secret\r\n"),
            range_from: Some(7),
            deadline: None,
            same_origin_only: false,
            credential_gate: allow_all,
        };
        let got = open_following(&mut *hs, &req, &mut tv_base::checkpoint::NoCheckpoint);
        super::super::http_close(&mut *hs);
        let (first_head, cdn_head) = (first_h.join().unwrap(), cdn_h.join().unwrap());
        match got {
            Ok(Opened::Socket(t)) => assert_eq!(t.origin.port(), cdn as i32),
            other => panic!("{other:?}"),
        }
        assert!(first_head.contains("Authorization: Bearer secret"), "{first_head}");
        assert!(!cdn_head.contains("secret"), "{cdn_head}");
        assert!(cdn_head.contains("Range: bytes=7-"), "{cdn_head}");
    }

    /// An Xtream stream URL carries the account in its path (`/live/<user>/<pass>/<id>.ts`): the
    /// lines a followed redirect writes (the hop's `status=`, the `redirect ... hop=` line) name
    /// the origin and the extension, never a path segment or the query.
    #[test]
    fn a_redirect_logs_no_path_segment_and_no_query() {
        use std::io::{Read, Write};
        let _serial = tv_base::testlock::serial();
        let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = srv.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let replies = [
                "HTTP/1.1 302 Found\r\nLocation: /live/SomeUser/S3cretPass/2.ts?token=Qz9Tok3n\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            ];
            for reply in replies {
                let (mut s, _) = srv.accept().unwrap();
                let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                let mut raw = Vec::new();
                let mut c = [0u8; 2048];
                while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut c) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => raw.extend_from_slice(&c[..n]),
                    }
                }
                let _ = s.write_all(reply.as_bytes());
            }
        });
        let origin = Origin::http("127.0.0.1", port as i32);
        let req = Request {
            origin: &origin,
            path: "/live/SomeUser/S3cretPass/1.ts",
            credentials: None,
            range_from: None,
            deadline: None,
            same_origin_only: false,
            credential_gate: allow_all,
        };
        let log = tv_base::eventlog::with_private_log(|| {
            let mut hs = super::super::http_stream_boxed();
            let got = open_following(&mut *hs, &req, &mut tv_base::checkpoint::NoCheckpoint);
            super::super::http_close(&mut *hs);
            assert!(matches!(got, Ok(Opened::Socket(_))), "{got:?}");
            std::fs::read_to_string(tv_base::eventlog::events_log()).unwrap_or_default()
        });
        server.join().unwrap();
        assert!(log.contains("hop=1"), "the redirect was logged: {log}");
        assert!(log.contains("/.../*.ts"), "{log}");
        for secret in ["SomeUser", "S3cretPass", "Qz9Tok3n", "live"] {
            assert!(!log.contains(secret), "{secret} reached the log: {log}");
        }
        assert_eq!(super::super::log_endpoint("/live/u/p/1234.TS?x=1"), "/.../*.ts");
        assert_eq!(super::super::log_endpoint("/play/AbCdEf0123456789"), "/...");
        assert_eq!(super::super::log_endpoint("/x/y.Tok3nTok3n"), "/...");
    }
}
