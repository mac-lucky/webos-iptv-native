//! One GET whose caller reads the response head: the status and every header field, then, for a
//! `2xx`, the body as it arrives. Redirects are never followed here; a `3xx` is a head like any
//! other, `Location` included, and the caller decides whether to request it.
//!
//! [`get`] picks the transport by scheme: `https` (and an `http` URL with userinfo, which only
//! libcurl turns into credentials) goes through [`crate::net::get_tls`], with the device trust
//! store and the bundled-roots fallback of every other request, and plain `http` through
//! [`crate::stream::http_get_plain`], the socket stream, which needs no libcurl at all. Both read the
//! head with one [`HeadReader`] under one cap, [`MAX_HEAD`].
//!
//! A head is untrusted input: it is kept as received (values trimmed, bytes that are not UTF-8
//! replaced) and never formatted into a log line here; [`Head`]'s `Debug` shows field names only,
//! since a `Location` or a `Set-Cookie` value can carry a credential.

use std::fmt;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::net::origin::Origin;
use crate::net::{RequestError, StreamEnd, Tls};

/// Most bytes of response head read for one request: every status line and field line, CRLFs
/// included, interim `1xx` heads counted too. A head past it ends the request with
/// [`StreamEnd::HeadTooLarge`] before it is handed over.
pub const MAX_HEAD: usize = 32 << 10;

/// The head of one response.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Head {
    pub status: u16,
    /// `(name, value)` in arrival order: the name as sent, the value with surrounding spaces and
    /// tabs trimmed, any byte that is not UTF-8 replaced.
    pub fields: Vec<(String, String)>,
}

impl Head {
    /// The first field named `name`, matched case-insensitively (RFC 9110 section 5.1).
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl fmt::Debug for Head {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.fields.iter().map(|(n, _)| n.as_str()).collect();
        f.debug_struct("Head")
            .field("status", &self.status)
            .field("fields", &names)
            .finish()
    }
}

/// What one line did to the head being read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    More,
    /// The final head is complete; later lines (chunked trailers) are ignored.
    Head(Head),
    TooLarge,
    /// A head ended with no usable status line.
    Malformed,
}

/// Reads a head one line at a time, the shape libcurl's header callback hands it over in.
#[derive(Default)]
pub(crate) struct HeadReader {
    head: Head,
    bytes: usize,
    done: bool,
    /// A line of the current head has been read: a status line is only its first line.
    started: bool,
}

impl HeadReader {
    /// One line, with or without its CRLF.
    pub(crate) fn line(&mut self, raw: &[u8]) -> Step {
        if self.done {
            return Step::More;
        }
        self.bytes = self.bytes.saturating_add(raw.len());
        if self.bytes > MAX_HEAD {
            self.done = true;
            return Step::TooLarge;
        }
        let line = trim_eol(raw);
        if line.is_empty() {
            return match self.head.status {
                0 => {
                    self.done = true;
                    Step::Malformed
                }
                // An interim response: the final one is still to come.
                100..=199 => {
                    self.head = Head::default();
                    self.started = false;
                    Step::More
                }
                _ => {
                    self.done = true;
                    Step::Head(std::mem::take(&mut self.head))
                }
            };
        }
        let first = !std::mem::replace(&mut self.started, true);
        if first {
            // The status line; a head that starts with anything else has no status and ends
            // `Malformed`. An `HTTP/` line later in a head is not one.
            self.head.status = if line.starts_with(b"HTTP/") {
                status_of(line)
            } else {
                0
            };
        } else if matches!(line[0], b' ' | b'\t') {
            // obs-fold (RFC 9112 section 5.2): a continuation of the previous field's value.
            if let Some((_, value)) = self.head.fields.last_mut() {
                value.push(' ');
                value.push_str(&String::from_utf8_lossy(trim_ows(line)));
            }
        } else if let Some(colon) = line.iter().position(|b| *b == b':') {
            let name = trim_ows(&line[..colon]);
            if !name.is_empty() {
                self.head.fields.push((
                    String::from_utf8_lossy(name).into_owned(),
                    String::from_utf8_lossy(trim_ows(&line[colon + 1..])).into_owned(),
                ));
            }
        }
        Step::More
    }
}

/// The line without its trailing CR and LF.
fn trim_eol(line: &[u8]) -> &[u8] {
    let mut line = line;
    while let [rest @ .., b'\r' | b'\n'] = line {
        line = rest;
    }
    line
}

/// Without the spaces and tabs around it.
fn trim_ows(v: &[u8]) -> &[u8] {
    let start = v
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t'))
        .unwrap_or(v.len());
    let end = v
        .iter()
        .rposition(|b| !matches!(b, b' ' | b'\t'))
        .map_or(start, |i| i + 1);
    &v[start..end]
}

/// `HTTP/1.1 304 Not Modified` (or `HTTP/2 304`) to 304: exactly three digits after the version,
/// in 100..=599, else 0.
fn status_of(line: &[u8]) -> u16 {
    let Some(sp) = line.iter().position(|b| *b == b' ') else {
        return 0;
    };
    let rest = &line[sp + 1..];
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits != 3 {
        return 0;
    }
    let code = rest[..3]
        .iter()
        .fold(0u16, |acc, b| acc * 10 + u16::from(b - b'0'));
    if (100..=599).contains(&code) {
        code
    } else {
        0
    }
}

/// GET `url` without following redirects. `on_head` gets the final head once (a `false` return
/// ends the request, [`StreamEnd::Consumer`]); for a `2xx` the body follows into `on_bytes` until
/// it is complete ([`StreamEnd::Closed`]), and any other status ends the request right after the
/// head ([`StreamEnd::Status`]).
///
/// * `budget` bounds the whole request, connect and body included: libcurl's
///   `CURLOPT_TIMEOUT_MS`, the socket stream's deadline. Running out is a
///   [`StreamEnd::Transport`] whose cause is [`RequestError::TimedOut`].
/// * `idle` bounds silence from the server (no head or body byte): [`StreamEnd::Idle`]. Time the
///   caller spends inside `on_bytes` is not silence.
/// * `cancel` is looked at about once a second on libcurl (its progress callback) and every
///   250 ms on the socket stream: [`StreamEnd::Cancelled`].
/// * `headers` are full `Name: value` lines, checked like every other request's.
///
/// Blocking: call it off the frame thread. A URL with a control byte or a space, or a scheme that
/// is not http or https, is refused before anything is dialled.
pub fn get(
    url: &str,
    headers: &[String],
    budget: Duration,
    idle: Duration,
    cancel: &AtomicBool,
    on_head: &mut dyn FnMut(Head) -> bool,
    on_bytes: &mut dyn FnMut(&[u8]) -> bool,
) -> StreamEnd {
    let refused = || StreamEnd::Transport(RequestError::Transport.into());
    if url.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return refused();
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return refused();
    };
    let tls = scheme.eq_ignore_ascii_case("https");
    if !tls && !scheme.eq_ignore_ascii_case("http") {
        return refused();
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if uses_libcurl(url) {
        // The one test seam `request_result` has too: a loopback TLS server's certificate.
        #[cfg(any(test, feature = "test-support"))]
        let bundle = crate::net::test_ca_bundle::get();
        #[cfg(not(any(test, feature = "test-support")))]
        let bundle: Option<String> = None;
        let tls = bundle.as_deref().map_or(Tls::Ca, Tls::CaBundle);
        return crate::net::get_tls(url, headers, tls, budget, idle, cancel, on_head, on_bytes);
    }
    let Some(origin) = Origin::parse(&format!("http://{authority}")) else {
        return refused();
    };
    let tail = &tail[..tail.find('#').unwrap_or(tail.len())];
    let path = if tail.starts_with('/') {
        tail.to_owned()
    } else {
        format!("/{tail}")
    };
    crate::stream::http_get_plain(
        origin.host(),
        origin.port(),
        &path,
        headers,
        budget,
        idle,
        cancel,
        on_head,
        on_bytes,
    )
}

/// Whether [`get`] sends `url` through libcurl: https, and http with userinfo. Under the
/// serialised-TLS fallback ([`crate::net::threaded_tls_ready`] false) such a request holds the
/// process's one libcurl lock for as long as it runs, its callbacks included.
pub fn uses_libcurl(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    scheme.eq_ignore_ascii_case("https")
        || (scheme.eq_ignore_ascii_case("http") && authority.contains('@'))
}

#[cfg(test)]
#[path = "get_tests.rs"]
mod tests;
