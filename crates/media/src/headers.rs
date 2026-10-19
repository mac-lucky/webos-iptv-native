//! **The request headers a stream asks for**, checked once, carried with the playback and applied
//! by the media transport to every request it makes for that stream.
//!
//! Stremio addons attach them as `behaviorHints.proxyHeaders.request` — typically a `User-Agent`
//! and a `Referer` that a live-TV origin refuses to serve without. They are part of the stream's
//! address, not of the process: the same television plays the next stream with none.
//!
//! What a stream may set is what `tv_net::net::headers` lets any caller add, plus `User-Agent`,
//! which REPLACES the process's installed name for this stream's requests only (that is the whole
//! point of the hint). Refused, whole: a name that is not an RFC 9110 token, a CR, LF or NUL
//! anywhere (it would end the header or the C string), `Host`, `Content-Length` and
//! `Transfer-Encoding` (the transport writes those), and `Range` (the media source owns the byte
//! range: every seek is its own `Range`, and a fixed one from an addon would corrupt the demuxer's
//! view of the file).
//!
//! **Redirects.** Every hop of a stream's redirect chain gets the headers — a live-TV URL that
//! redirects to its CDN needs the same `User-Agent` there. The credential headers
//! ([`CREDENTIAL`]: `Authorization`, `Proxy-Authorization`, `Cookie`) are the exception: they go to
//! the stream's own origin only and are dropped from a hop to another origin, which is what
//! libcurl itself does for a redirect it follows (`CURLOPT_UNRESTRICTED_AUTH` off).
//!
//! A refusal names the KIND and the position, never the value: a header may carry a credential.
use std::fmt;

/// Headers a stream may not set, lowercase. The transport writes the first three itself; `range`
/// belongs to the media source's seek.
pub const REFUSED: [&str; 4] = ["host", "content-length", "transfer-encoding", "range"];

/// Headers that are sent to the stream's own origin and never to a redirect target on another
/// origin, lowercase.
pub const CREDENTIAL: [&str; 3] = ["authorization", "proxy-authorization", "cookie"];

/// Why a stream's header list was refused. Carries no part of any header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderRefusal {
    /// Header `index` (0-based, in the order given) has an empty name or one that is not a token.
    BadName { index: usize },
    /// Header `index` holds a CR, LF or NUL.
    Framing { index: usize },
    /// Header `index` names one of [`REFUSED`].
    Reserved { index: usize },
    /// More than one `User-Agent`.
    DuplicateUserAgent,
}

impl fmt::Display for HeaderRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeaderRefusal::BadName { index } => write!(f, "request header {index}: the name is not a token"),
            HeaderRefusal::Framing { index } => write!(f, "request header {index}: holds CR, LF or NUL"),
            HeaderRefusal::Reserved { index } => {
                write!(f, "request header {index}: Host, Content-Length, Transfer-Encoding and Range are the transport's")
            }
            HeaderRefusal::DuplicateUserAgent => f.write_str("more than one User-Agent request header"),
        }
    }
}

/// One `Name: value` line, already checked.
#[derive(Clone, PartialEq, Eq)]
struct Line {
    credential: bool,
    text: String,
}

/// A stream's checked request headers. `Debug` prints names only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RequestHeaders {
    user_agent: Option<String>,
    lines: Vec<Line>,
}

impl fmt::Debug for RequestHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.lines.iter().map(|l| l.text.split(':').next().unwrap_or("")).collect();
        f.debug_struct("RequestHeaders")
            .field("user_agent", &self.user_agent.is_some())
            .field("names", &names)
            .finish()
    }
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

impl RequestHeaders {
    /// No headers: every request carries the process's own `User-Agent` and nothing else.
    pub const fn none() -> RequestHeaders {
        RequestHeaders { user_agent: None, lines: Vec::new() }
    }

    /// Check `pairs` (name, value), in order. Surrounding whitespace of a value is not significant
    /// in HTTP and is trimmed; an empty value is kept (`Name:` is a header).
    pub fn from_pairs<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> Result<RequestHeaders, HeaderRefusal> {
        let mut out = RequestHeaders::none();
        for (index, (name, value)) in pairs.iter().enumerate() {
            let (name, value) = (name.as_ref().trim(), value.as_ref().trim_matches(|c| c == ' ' || c == '\t'));
            if name.is_empty() || !name.chars().all(is_token_char) {
                return Err(HeaderRefusal::BadName { index });
            }
            if value.chars().any(|c| matches!(c, '\r' | '\n' | '\0')) {
                return Err(HeaderRefusal::Framing { index });
            }
            let lower = name.to_ascii_lowercase();
            if REFUSED.contains(&lower.as_str()) {
                return Err(HeaderRefusal::Reserved { index });
            }
            if lower == "user-agent" {
                if out.user_agent.is_some() {
                    return Err(HeaderRefusal::DuplicateUserAgent);
                }
                out.user_agent = Some(value.to_owned());
                continue;
            }
            let text = format!("{name}: {value}");
            // The transport's own rule for what a caller may add, on top of the checks above.
            if tv_net::net::headers::check_line(&text).is_err() {
                return Err(HeaderRefusal::BadName { index });
            }
            out.lines.push(Line { credential: CREDENTIAL.contains(&lower.as_str()), text });
        }
        Ok(out)
    }

    /// Nothing to apply.
    pub fn is_empty(&self) -> bool {
        self.user_agent.is_none() && self.lines.is_empty()
    }

    /// The stream's own `User-Agent`, replacing the process's for its requests.
    pub fn user_agent(&self) -> Option<&str> {
        self.user_agent.as_deref()
    }

    /// The header lines (without `User-Agent`) for a request to the stream's own origin
    /// (`same_origin`) or to a redirect target on another one, which never gets [`CREDENTIAL`].
    pub fn lines(&self, same_origin: bool) -> impl Iterator<Item = &str> {
        self.lines.iter().filter(move |l| same_origin || !l.credential).map(|l| l.text.as_str())
    }

    /// The same headers without [`CREDENTIAL`]: what a request to another origin carries (an HLS
    /// playlist naming a key or segment on a CDN, where every URI starts a chain of its own).
    pub fn without_credentials(&self) -> RequestHeaders {
        RequestHeaders { user_agent: self.user_agent.clone(), lines: self.lines.iter().filter(|l| !l.credential).cloned().collect() }
    }

    /// The headers as the plaintext socket path's request text: each line CRLF-terminated,
    /// `User-Agent` included when the stream sets one.
    pub fn wire_block(&self, same_origin: bool) -> String {
        let mut block = String::new();
        if let Some(ua) = &self.user_agent {
            block.push_str("User-Agent: ");
            block.push_str(ua);
            block.push_str("\r\n");
        }
        for line in self.lines(same_origin) {
            block.push_str(line);
            block.push_str("\r\n");
        }
        block
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_tv_pair_is_kept_with_the_user_agent_separate() {
        let h = RequestHeaders::from_pairs(&[("User-Agent", "LiveClient/2.6"), ("Referer", " https://live.example/ ")]).unwrap();
        assert_eq!(h.user_agent(), Some("LiveClient/2.6"));
        assert_eq!(h.lines(true).collect::<Vec<_>>(), ["Referer: https://live.example/"]);
        assert_eq!(h.wire_block(true), "User-Agent: LiveClient/2.6\r\nReferer: https://live.example/\r\n");
        assert!(!h.is_empty());
        assert!(RequestHeaders::none().is_empty());
    }

    #[test]
    fn framing_reserved_and_malformed_headers_are_refused_by_position() {
        assert_eq!(RequestHeaders::from_pairs(&[("Referer", "a"), ("X", "b\r\nHost: evil")]), Err(HeaderRefusal::Framing { index: 1 }));
        for name in ["Host", "content-length", "Transfer-Encoding", "Range"] {
            assert_eq!(RequestHeaders::from_pairs(&[(name, "1")]), Err(HeaderRefusal::Reserved { index: 0 }), "{name}");
        }
        assert_eq!(RequestHeaders::from_pairs(&[("Bad Name", "x")]), Err(HeaderRefusal::BadName { index: 0 }));
        assert_eq!(RequestHeaders::from_pairs(&[("", "x")]), Err(HeaderRefusal::BadName { index: 0 }));
        assert_eq!(
            RequestHeaders::from_pairs(&[("User-Agent", "a"), ("user-agent", "b")]),
            Err(HeaderRefusal::DuplicateUserAgent)
        );
    }

    #[test]
    fn credentials_never_follow_a_redirect_to_another_origin() {
        let h = RequestHeaders::from_pairs(&[("Referer", "r"), ("Authorization", "Bearer s"), ("Cookie", "c=1")]).unwrap();
        assert_eq!(h.lines(true).count(), 3);
        assert_eq!(h.lines(false).collect::<Vec<_>>(), ["Referer: r"]);
        assert!(!format!("{h:?}").contains("Bearer"), "Debug must not print a value");
        let ua = RequestHeaders::from_pairs(&[("User-Agent", "u"), ("Referer", "r"), ("Cookie", "c=1")]).unwrap();
        let cross = ua.without_credentials();
        assert_eq!((cross.user_agent(), cross.lines(true).collect::<Vec<_>>()), (Some("u"), vec!["Referer: r"]));
    }
}
