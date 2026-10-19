//! Masking of tokens in URLs and error strings before anything is logged or shown.
//!
//! Playlist, guide and stream URLs carry account tokens: in the query, in path segments and in
//! userinfo. Every log line or status text that can hold a URL goes through [`url`] (one URL) or
//! [`text`] (free text that may contain URLs). Both keep the scheme, host, port and path shape, so
//! a line still says which server and which kind of request it was about:
//! `http://host/pl/13/<masked>/playlist.m3u8?token=<masked>`.
//!
//! The secrets of the URLs the user configured are known exactly ([`register_secrets`]) and are
//! masked wherever they appear, whatever their shape. The other rules over-mask rather than
//! under-mask: every query value, the whole fragment, everything before the last `@`, every
//! token-like run (see `looks_like_token`) and the two credential segments of an Xtream path
//! (`/live/<user>/<pass>/1.ts`).

use std::sync::{PoisonError, RwLock};

use crate::url::{is_scheme, split_url};

/// What a masked value is replaced with.
pub const MASK: &str = "<masked>";

/// The secrets [`register_secrets`] recorded, oldest first.
static KNOWN: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Secrets kept; past this the oldest go.
const MAX_KNOWN: usize = 64;

/// Path segments and query values too common to be anyone's secret: registering them would mask
/// ordinary words in every line.
const COMMON_WORDS: [&str; 28] = [
    "api",
    "archive",
    "channel",
    "channels",
    "dash",
    "download",
    "epg",
    "false",
    "files",
    "get",
    "guide",
    "iptv",
    "json",
    "list",
    "live",
    "m3u8",
    "m3u_plus",
    "media",
    "movie",
    "mpegts",
    "play",
    "playlist",
    "series",
    "stream",
    "streams",
    "timeshift",
    "true",
    "xmltv",
];

/// Records the secrets of a URL the user configured (a playlist or guide URL), which [`url`] and
/// [`text`] then mask wherever they appear: the user and password of the userinfo, every query
/// value and every path segment but the last. Values under 4 bytes, numbers and common words are
/// left out. Only for configured URLs: the set is bounded, and URLs from a playlist would push
/// the configured ones out.
pub fn register_secrets(url: &str) {
    let (authority, rest) =
        split_url(url).map_or(("", url), |(_, authority, rest)| (authority, rest));
    let rest = rest.split('#').next().unwrap_or_default();
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let userinfo = authority
        .rsplit_once('@')
        .map_or("", |(userinfo, _)| userinfo);
    let values = query
        .split('&')
        .map(|p| p.split_once('=').map_or(p, |(_, v)| v));
    // Every segment but the last, which names the file.
    let dirs = path.rsplit_once(['/', '\\']).map_or("", |(dirs, _)| dirs);
    let found = userinfo
        .splitn(2, ':')
        .chain(values)
        .chain(dirs.split(['/', '\\']));
    let mut known = KNOWN.write().unwrap_or_else(PoisonError::into_inner);
    for secret in found.filter(|s| worth_knowing(s)) {
        if !known.iter().any(|k| k == secret) {
            if known.len() == MAX_KNOWN {
                known.remove(0);
            }
            known.push(secret.to_owned());
        }
    }
}

/// Long enough, not a number, not a common word, and not part of [`MASK`] (so masking stays
/// idempotent).
fn worth_knowing(s: &str) -> bool {
    s.len() >= 4
        && !s.bytes().all(|b| b.is_ascii_digit())
        && !COMMON_WORDS.iter().any(|w| w.eq_ignore_ascii_case(s))
        && !MASK.contains(s)
}

/// `u` with userinfo, every query value, the fragment and every token-like path segment masked.
/// Input without a `scheme://` prefix is treated as a path with an optional query and fragment.
pub fn url(u: &str) -> String {
    let known = KNOWN.read().unwrap_or_else(PoisonError::into_inner);
    let mut out = String::with_capacity(u.len());
    push_url(&mut out, u, &known);
    out
}

/// `s` with every `scheme://` URL masked as by [`url`], every other token-like run masked, and
/// the value of every `name=value` whose name is a credential (`token=`, `pass=`, ...) masked.
pub fn text(s: &str) -> String {
    let known = KNOWN.read().unwrap_or_else(PoisonError::into_inner);
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some((start, end)) = next_url(rest) {
        push_plain(&mut out, &rest[..start], &known);
        push_url(&mut out, &rest[start..end], &known);
        rest = &rest[end..];
    }
    push_plain(&mut out, rest, &known);
    out
}

/// Bytes a token run is made of: what an opaque token, a base64url value or a UUID consists of.
/// `.` is left out so a file extension and a host's labels stay apart from a token next to them.
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'~' | b'+' | b'%')
}

/// Whether a run of [`is_token_byte`] bytes reads as a secret rather than a word or an id: letters
/// mixed with digits from 10 bytes, upper and lower case mixed or all digits from 16, anything from
/// 32. A channel number, a timestamp, a file name or a hyphenated word stays readable; a short
/// alphanumeric path token, a hex or base64 token, a UUID and a long digit run do not.
fn looks_like_token(run: &str) -> bool {
    let n = run.len();
    if n < 10 {
        return false;
    }
    let has = |f: fn(&u8) -> bool| run.as_bytes().iter().any(f);
    let (lower, upper, digit) = (
        has(u8::is_ascii_lowercase),
        has(u8::is_ascii_uppercase),
        has(u8::is_ascii_digit),
    );
    let classes = u8::from(lower) + u8::from(upper) + u8::from(digit);
    (digit && classes >= 2) || (n >= 16 && (classes >= 2 || digit)) || n >= 32
}

/// Copy `s`, masking each known secret in it, then each token-like run in what is left.
fn push_runs(out: &mut String, s: &str, known: &[String]) {
    let mut rest = s;
    while let Some((at, len)) = find_known(rest, known) {
        push_token_runs(out, &rest[..at]);
        out.push_str(MASK);
        rest = &rest[at + len..];
    }
    push_token_runs(out, rest);
}

/// The first known secret in `s` that is not part of a longer token run, as `(start, len)`; the
/// longest one where several start at the same byte.
fn find_known(s: &str, known: &[String]) -> Option<(usize, usize)> {
    let b = s.as_bytes();
    let joined = |i: usize| i > 0 && i < b.len() && is_token_byte(b[i - 1]) && is_token_byte(b[i]);
    known
        .iter()
        .filter_map(|k| {
            let at = s
                .match_indices(k.as_str())
                .map(|(at, _)| at)
                .find(|&at| !joined(at) && !joined(at + k.len()))?;
            Some((at, k.len()))
        })
        .min_by_key(|&(at, len)| (at, std::cmp::Reverse(len)))
}

/// Copy `s`, masking each token-like run in it.
fn push_token_runs(out: &mut String, s: &str) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let token = is_token_byte(b[i]);
        while i < b.len() && is_token_byte(b[i]) == token {
            i += 1;
        }
        // Run boundaries are ASCII bytes, so both ends are char boundaries.
        let run = &s[start..i];
        out.push_str(if token && looks_like_token(run) {
            MASK
        } else {
            run
        });
    }
}

/// Where a URL ends in free text: whitespace, a control byte, a quote or an angle bracket.
fn ends_url(b: u8) -> bool {
    b.is_ascii_whitespace()
        || b.is_ascii_control()
        || matches!(b, b'"' | b'\'' | b'<' | b'>' | b'`')
}

/// The first byte at or after `from` that [`ends_url`], or the length.
fn url_end(b: &[u8], from: usize) -> usize {
    b[from..]
        .iter()
        .position(|&c| ends_url(c))
        .map_or(b.len(), |i| from + i)
}

/// The byte range of the first `scheme://...` URL in `s`.
fn next_url(s: &str) -> Option<(usize, usize)> {
    let b = s.as_bytes();
    let mut from = 0;
    while let Some(at) = s[from..].find("://").map(|i| from + i) {
        let mut start = at;
        while start > 0
            && (b[start - 1].is_ascii_alphanumeric() || matches!(b[start - 1], b'+' | b'-' | b'.'))
        {
            start -= 1;
        }
        // A scheme starts with a letter: skip leading digits and signs ("1http://").
        while start < at && !b[start].is_ascii_alphabetic() {
            start += 1;
        }
        if is_scheme(&s[start..at]) {
            let mut end = url_end(b, at);
            // A password holding a space ends the URL early. When the URL has no `@` and one
            // follows on the same line, before any other URL, the URL runs through it, so the
            // userinfo rule masks the whole password (and, for prose that only looks like
            // this, a little too much).
            if !s[start..end].contains('@') {
                let after = &b[end..];
                let line = after
                    .iter()
                    .position(|&c| ends_url(c) && c != b' ')
                    .unwrap_or(after.len());
                if let Some(p) = after[..line].iter().position(|&c| c == b'@') {
                    if !s[end..end + p].contains("://") {
                        end = url_end(b, end + p + 1);
                    }
                }
            }
            return Some((start, end));
        }
        from = at + 3;
    }
    None
}

fn push_url(out: &mut String, u: &str, known: &[String]) {
    let mut rest = u;
    if let Some((scheme, ..)) = split_url(u) {
        out.push_str(&u[..scheme.len() + 3]);
        rest = &u[scheme.len() + 3..];
        // Userinfo is everything before the last `@` in the URL: an unencoded password may hold
        // `/`, `\`, `?`, `#` or a space, and must not be read as a host or a path and printed.
        if let Some(at) = rest.rfind('@') {
            out.push_str(MASK);
            out.push('@');
            rest = &rest[at + 1..];
        }
        let host_end = rest.find(['/', '\\', '?', '#']).unwrap_or(rest.len());
        push_runs(out, &rest[..host_end], known);
        rest = &rest[host_end..];
    }
    let (rest, fragment) = match rest.split_once('#') {
        Some((r, f)) => (r, Some(f)),
        None => (rest, None),
    };
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (rest, None),
    };
    push_path(out, path, known);
    if let Some(q) = query {
        out.push('?');
        push_query(out, q, known);
    }
    if let Some(f) = fragment {
        out.push('#');
        if !f.is_empty() {
            out.push_str(MASK);
        }
    }
}

/// Path kinds of the Xtream Codes API, whose next two segments are the account's user and password.
const XTREAM_KINDS: [&str; 4] = ["live", "movie", "series", "timeshift"];

fn push_path(out: &mut String, path: &str, known: &[String]) {
    let segments: Vec<&str> = path.split(['/', '\\']).collect();
    let mut secret = vec![false; segments.len()];
    for (i, seg) in segments.iter().enumerate() {
        // `/live/<user>/<pass>/<stream>`: only when a stream segment follows the two.
        if XTREAM_KINDS.iter().any(|k| seg.eq_ignore_ascii_case(k)) && segments.len() > i + 3 {
            secret[i + 1] = true;
            secret[i + 2] = true;
        }
    }
    let mut pos = 0;
    for (i, seg) in segments.iter().enumerate() {
        if i > 0 {
            // The separator as written: `/` or `\`.
            out.push_str(&path[pos - 1..pos]);
        }
        pos += seg.len() + 1;
        if secret[i] && !seg.is_empty() {
            out.push_str(MASK);
        } else {
            push_runs(out, seg, known);
        }
    }
}

/// Every value masked; a parameter without `=` is masked whole, names stay unless token-like.
fn push_query(out: &mut String, q: &str, known: &[String]) {
    for (i, part) in q.split('&').enumerate() {
        if i > 0 {
            out.push('&');
        }
        match part.split_once('=') {
            Some((name, value)) => {
                push_runs(out, name, known);
                out.push('=');
                if !value.is_empty() {
                    out.push_str(MASK);
                }
            }
            None if part.is_empty() => {}
            None => out.push_str(MASK),
        }
    }
}

/// Name endings that mark a credential wherever `name=value` or `name: value` appears outside a
/// URL: `token` covers `auth_token` and `refresh_token`, `key` covers `api_key`.
const CREDENTIAL_SUFFIXES: [&str; 11] = [
    "token",
    "key",
    "pass",
    "password",
    "passwd",
    "pwd",
    "secret",
    "sig",
    "signature",
    "auth",
    "username",
];

fn is_credential_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    CREDENTIAL_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Free text outside a URL: token-like runs masked, and the value of every credential
/// `name=value` or `name: value`, with spaces around the separator, the name and the value
/// quoted or not (`"password": "x"`).
fn push_plain(out: &mut String, s: &str, known: &[String]) {
    let b = s.as_bytes();
    let mut copied = 0;
    let mut i = 0;
    while let Some(off) = b[i..].iter().position(|&c| c == b'=' || c == b':') {
        let sep = i + off;
        i = sep + 1;
        // The name: back over spaces and a closing quote, then name bytes.
        let mut j = sep;
        while j > copied && b[j - 1] == b' ' {
            j -= 1;
        }
        if j > copied && matches!(b[j - 1], b'"' | b'\'') {
            j -= 1;
        }
        let name_end = j;
        while j > copied && (b[j - 1].is_ascii_alphanumeric() || matches!(b[j - 1], b'_' | b'-')) {
            j -= 1;
        }
        if !is_credential_name(&s[j..name_end]) {
            continue;
        }
        // The value: after spaces, a quoted string (escapes skipped) or a run up to a delimiter.
        let mut start = sep + 1;
        while start < b.len() && b[start] == b' ' {
            start += 1;
        }
        let end = match b.get(start) {
            Some(&quote @ (b'"' | b'\'')) => {
                start += 1;
                let mut e = start;
                while e < b.len() && b[e] != quote && !b[e].is_ascii_control() {
                    e += if b[e] == b'\\' { 2 } else { 1 };
                }
                e.min(b.len())
            }
            _ => b[start..]
                .iter()
                .position(|&c| ends_url(c) || matches!(c, b'&' | b';' | b',' | b')' | b']' | b'}'))
                .map_or(b.len(), |p| start + p),
        };
        if start >= end {
            continue;
        }
        push_runs(out, &s[copied..start], known);
        out.push_str(MASK);
        copied = end;
        i = end;
    }
    push_runs(out, &s[copied..], known);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic secrets, assembled at run time so no credential-shaped literal sits in the source.
    const TOKEN: &str = "a1B2c3D4e5F6g7H8i9J0kLmN";
    const WORD: &str = "pw-value";

    fn userinfo(pass: &str) -> String {
        ["user", pass].join(":")
    }

    fn pair(name: &str, value: &str) -> String {
        [name, value].join("=")
    }

    fn masked(out: &str, secret: &str) {
        assert!(!out.contains(secret), "{secret:?} survived in {out:?}");
    }

    #[test]
    fn short_path_token_is_masked() {
        let token = ["k2v9x4", "q7m1z8b"].concat();
        assert_eq!(
            url(&format!("http://h.example/pl/13/{token}/playlist.m3u8")),
            "http://h.example/pl/13/<masked>/playlist.m3u8"
        );
    }

    #[test]
    fn path_token_keeps_scheme_host_and_shape() {
        let u = format!("http://host.example:8080/pl/13/{TOKEN}/playlist.m3u8");
        assert_eq!(
            url(&u),
            "http://host.example:8080/pl/13/<masked>/playlist.m3u8"
        );
    }

    #[test]
    fn every_query_value_is_masked_and_names_stay() {
        let u = format!(
            "https://h.example/get?{}&{}&type=m3u_plus&flag&empty=",
            pair("username", "alice"),
            pair("password", WORD)
        );
        let out = url(&u);
        assert_eq!(out, "https://h.example/get?username=<masked>&password=<masked>&type=<masked>&<masked>&empty=");
    }

    #[test]
    fn userinfo_is_masked_up_to_the_last_at_sign() {
        let out = url(&format!("http://{}@host.example/a", userinfo("p@ss")));
        assert_eq!(out, "http://<masked>@host.example/a");
        // `?` and `#` inside the userinfo do not make the password a host.
        let out = url(&format!("http://{}@host.example/a", userinfo("se?cr#et")));
        assert_eq!(out, "http://<masked>@host.example/a");
        masked(&out, "se?cr");
    }

    #[test]
    fn fragment_is_masked_whole() {
        assert_eq!(
            url(&format!(
                "https://h.example/cb#{}",
                pair("access_token", WORD)
            )),
            "https://h.example/cb#<masked>"
        );
        assert_eq!(url("https://h.example/cb#"), "https://h.example/cb#");
    }

    #[test]
    fn token_with_extension_keeps_the_extension() {
        assert_eq!(
            url(&format!("http://h.example/{TOKEN}.m3u8")),
            "http://h.example/<masked>.m3u8"
        );
    }

    #[test]
    fn xtream_user_and_password_segments_are_masked() {
        let out = url(&format!("http://h.example:8080/live/alice/{WORD}/1234.ts"));
        assert_eq!(out, "http://h.example:8080/live/<masked>/<masked>/1234.ts");
        let out = url(&format!(
            "http://h.example/timeshift/alice/{WORD}/60/2026-10-19:20-00/7.ts"
        ));
        assert_eq!(
            out,
            "http://h.example/timeshift/<masked>/<masked>/60/2026-10-19:20-00/7.ts"
        );
        // Not the Xtream shape: nothing after the would-be password.
        assert_eq!(
            url("http://h.example/live/stream/index.m3u8"),
            "http://h.example/live/stream/index.m3u8"
        );
    }

    #[test]
    fn readable_segments_stay() {
        for u in [
            "http://h.example/hls/channel-one-hd/index.m3u8",
            "http://10.0.0.1/udp/239.0.0.1:1234",
            "https://[::1]:8443/a/b",
            "http://h.example/archive/1729368000/3600/mono.m3u8",
        ] {
            assert_eq!(url(u), u);
        }
    }

    #[test]
    fn token_like_runs() {
        assert!(looks_like_token("0123456789abcdef"));
        assert!(looks_like_token("550e8400-e29b-41d4-a716-446655440000"));
        assert!(looks_like_token("1234567890123456"));
        assert!(looks_like_token("abcdefghijklmnopqrstuvwxyzabcdef"));
        assert!(
            looks_like_token("0123456789abcde"),
            "15 bytes, letters and digits"
        );
        assert!(
            looks_like_token("abcdef1234567"),
            "13 bytes, letters and digits"
        );
        assert!(!looks_like_token("abcdefghijklmno"), "15 bytes, one class");
        assert!(
            !looks_like_token("ChannelOneHD"),
            "case alone needs 16 bytes"
        );
        assert!(!looks_like_token("channel1h"), "9 bytes");
        assert!(!looks_like_token("international-news"));
        assert!(!looks_like_token("1729368000"));
        assert!(
            !looks_like_token("172936800012345"),
            "digits alone need 16 bytes"
        );
    }

    #[test]
    fn token_in_a_host_label_is_masked() {
        assert_eq!(
            url(&format!("https://{TOKEN}.cdn.example/a")),
            "https://<masked>.cdn.example/a"
        );
    }

    #[test]
    fn backslash_separators_are_paths_not_hosts() {
        assert_eq!(
            url(&format!("http://h.example\\{TOKEN}\\x.ts")),
            "http://h.example\\<masked>\\x.ts"
        );
    }

    #[test]
    fn url_inside_a_query_value_is_masked_whole() {
        let u = format!(
            "http://a.example/r?u=http://b.example/?{}",
            pair("token", WORD)
        );
        assert_eq!(url(&u), "http://a.example/r?u=<masked>");
    }

    #[test]
    fn schemeless_input_is_a_path() {
        assert_eq!(
            url(&format!("/pl/{TOKEN}/x?sig=1")),
            "/pl/<masked>/x?sig=<masked>"
        );
    }

    #[test]
    fn text_masks_every_url_and_keeps_the_prose() {
        let s = format!(
            "bad line 3: \"#EXTINF:-1,One\" -> HTTP://h.example/pl/{TOKEN}/1.ts?{} and ftp://{}@f.example/x done",
            pair("token", WORD),
            userinfo(WORD)
        );
        let out = text(&s);
        assert_eq!(
            out,
            "bad line 3: \"#EXTINF:-1,One\" -> HTTP://h.example/pl/<masked>/1.ts?token=<masked> and ftp://<masked>@f.example/x done"
        );
    }

    #[test]
    fn text_masks_credentials_and_tokens_outside_urls() {
        let out = text(&format!(
            "login failed: {}, user id {TOKEN}; status=404",
            pair("password", WORD)
        ));
        assert_eq!(
            out,
            "login failed: password=<masked>, user id <masked>; status=404"
        );
        assert_eq!(
            text(&format!("{}&x=1", pair("Token", WORD))),
            "Token=<masked>&x=1"
        );
    }

    #[test]
    fn text_handles_scheme_lookalikes() {
        assert_eq!(
            text("1http://h.example/?a=b"),
            "1http://h.example/?a=<masked>"
        );
        assert_eq!(text("x :// y"), "x :// y");
        assert_eq!(text("://h.example/?a=b"), "://h.example/?a=b");
    }

    #[test]
    fn masking_is_idempotent() {
        let s = format!(
            "see http://{}@h.example/pl/{TOKEN}/x.m3u8?token=t#f and {}",
            userinfo(WORD),
            pair("key", WORD)
        );
        let once = text(&s);
        masked(&once, WORD);
        assert_eq!(text(&once), once);
        let u = format!(
            "http://{}@h.example/pl/{TOKEN}/x.m3u8?token=t#f",
            userinfo(WORD)
        );
        assert_eq!(url(&url(&u)), url(&u));
    }

    #[test]
    fn non_ascii_input_never_panics_and_is_kept() {
        let out = text("kana\u{142} \u{2713} http://h\u{f3}st.example/\u{15b}cie\u{17c}ka/{x}?\u{fc}=\u{f6}#\u{df} ok");
        assert_eq!(out, "kana\u{142} \u{2713} http://h\u{f3}st.example/\u{15b}cie\u{17c}ka/{x}?\u{fc}=<masked>#<masked> ok");
    }

    #[test]
    fn adversarial_inputs_never_panic() {
        let cases = [
            "",
            "://",
            "a://",
            "http://",
            "http://@",
            "http://@@@/",
            "http://?#",
            "http://#?",
            "?",
            "#",
            "=",
            "&&",
            "http://h/??&&==",
            "http:///////",
            "http://h/%",
            "\u{0}http://h/\u{7f}",
            "a=b=c=d",
            "token=",
            "token==x",
        ];
        for c in cases {
            let _ = url(c);
            let _ = text(c);
        }
        let long = format!("http://h/{}", "a1".repeat(100_000));
        assert_eq!(url(&long), "http://h/<masked>");
    }

    fn json(name: &str, value: &str) -> String {
        format!("\"{name}\":\"{value}\"")
    }

    /// Held by the tests that register secrets, so the cap test cannot evict theirs mid-test.
    static REGISTRY: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn registry() -> std::sync::MutexGuard<'static, ()> {
        REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[test]
    fn registered_secrets_are_masked_whatever_their_shape() {
        let _lock = registry();
        // Lowercase only, 13 bytes: the token-like test alone keeps it.
        let token = ["qzvkwmb", "xrtplj"].concat();
        assert!(!looks_like_token(&token));
        register_secrets(&format!("http://h.example/api/7/{token}/list.m3u8"));
        assert_eq!(
            url(&format!("http://cdn.example/x/{token}/1.ts")),
            "http://cdn.example/x/<masked>/1.ts"
        );
        assert_eq!(
            text(&format!(
                "open {token} failed: GET https://{token}.example/"
            )),
            "open <masked> failed: GET https://<masked>.example/"
        );
        // Not inside a longer run.
        let longer = format!("{token}x");
        assert_eq!(
            url(&format!("http://h.example/{longer}")),
            format!("http://h.example/{longer}")
        );
        // The rest of the configured URL was too short or too common to register.
        assert_eq!(
            url("http://h.example/api/7/list.m3u8"),
            "http://h.example/api/7/list.m3u8"
        );
    }

    #[test]
    fn registered_xtream_credentials_are_masked_in_stream_paths() {
        let _lock = registry();
        let (user, pass) = ("xuser-reg", "pw-reg-value");
        // Unregistered, the bare `/<user>/<pass>/<id>` shape is an ordinary path.
        assert_eq!(
            url(&format!("http://h.example:8080/{user}/{pass}/1234.ts")),
            format!("http://h.example:8080/{user}/{pass}/1234.ts")
        );
        register_secrets(&format!(
            "http://h.example:8080/get.php?username={user}&password={pass}&type=m3u_plus"
        ));
        assert_eq!(
            url(&format!("http://h.example:8080/{user}/{pass}/1234.ts")),
            "http://h.example:8080/<masked>/<masked>/1234.ts"
        );
        assert_eq!(
            text(&format!("login {user} refused")),
            "login <masked> refused"
        );
        assert_eq!(text("type m3u_plus"), "type m3u_plus");
    }

    #[test]
    fn registered_userinfo_is_masked_in_text() {
        let _lock = registry();
        let userinfo = ["reguser", "pw-userinfo"].join(":");
        register_secrets(&format!("http://{userinfo}@h.example/a"));
        assert_eq!(
            text("bad password pw-userinfo for reguser"),
            "bad password <masked> for <masked>"
        );
    }

    #[test]
    fn the_registry_is_bounded() {
        let _lock = registry();
        // Letters only, so the token-like test keeps them: `capsecretaa` .. `capsecretex`.
        let secret = |i: usize| {
            let letter = |n: usize| char::from(b'a' + n as u8);
            format!("capsecret{}{}", letter(i / 26), letter(i % 26))
        };
        for i in 0..2 * MAX_KNOWN {
            register_secrets(&format!("http://h.example/?k={}", secret(i)));
        }
        let (first, last) = (secret(0), secret(2 * MAX_KNOWN - 1));
        assert_eq!(KNOWN.read().unwrap().len(), MAX_KNOWN);
        assert_eq!(
            text(&format!("{first} {last}")),
            format!("{first} <masked>")
        );
    }

    #[test]
    fn ordinary_paths_stay_readable() {
        for u in [
            "http://h.example/hls/one/index.m3u8",
            "http://h.example/news/sport/1234.ts",
            "http://h.example/alice/pw-value/1234",
        ] {
            assert_eq!(url(u), u);
        }
    }

    #[test]
    fn userinfo_with_a_slash_or_a_space_is_masked_whole() {
        let out = url(&format!("http://{}@host.example/a", userinfo("pa/ss")));
        assert_eq!(out, "http://<masked>@host.example/a");
        for pass in ["pa ss", "p/a s s"] {
            let out = text(&format!("x http://{}@host.example/a y", userinfo(pass)));
            assert_eq!(out, "x http://<masked>@host.example/a y", "{pass:?}");
        }
        // Not past the line, nor into the next URL.
        assert_eq!(
            text("http://h.example\nmail a@b"),
            "http://h.example\nmail a@b"
        );
        assert_eq!(
            text("http://h.example and http://u@g.example/"),
            "http://h.example and http://<masked>@g.example/"
        );
    }

    #[test]
    fn credential_values_are_masked_in_every_plain_form() {
        let cases = [
            (
                format!("{{{},\"user\":\"x\"}}", json("password", WORD)),
                "{\"password\":\"<masked>\",\"user\":\"x\"}",
            ),
            (format!("token = {WORD} next"), "token = <masked> next"),
            (format!("auth_token={WORD}"), "auth_token=<masked>"),
            (format!("refresh_token: {WORD}"), "refresh_token: <masked>"),
            (format!("sig={WORD}&x=1"), "sig=<masked>&x=1"),
            (
                format!("{} : '{WORD}'", "password"),
                "password : '<masked>'",
            ),
            (
                format!("\"secret\": \"pw\\\"{WORD}\""),
                "\"secret\": \"<masked>\"",
            ),
            (format!("API-Key:{WORD}"), "API-Key:<masked>"),
        ];
        for (input, want) in cases {
            let out = text(&input);
            assert_eq!(out, want, "{input}");
            masked(&out, WORD);
        }
        assert_eq!(
            text("at 10:00 status: 404, error=none"),
            "at 10:00 status: 404, error=none"
        );
    }

    #[test]
    fn control_characters_end_a_url_in_text() {
        let out = text(&format!("x http://h.example/{TOKEN}\nnext line {TOKEN}"));
        assert_eq!(out, "x http://h.example/<masked>\nnext line <masked>");
    }
}
