//! The one place URLs are taken apart: the parser, the fetcher, catch-up and redaction all agree
//! on what a valid http(s) URL is.

/// The schemes the player fetches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

/// RFC 3986 scheme: a letter, then letters, digits, `+`, `-` or `.`.
pub(crate) fn is_scheme(s: &str) -> bool {
    let mut b = s.bytes();
    b.next().is_some_and(|c| c.is_ascii_alphabetic())
        && b.all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
}

/// `url` as scheme, authority (userinfo included) and the rest; the authority ends at the first
/// `/`, `\`, `?` or `#`. `None` without a valid `scheme://` prefix.
pub(crate) fn split_url(url: &str) -> Option<(&str, &str, &str)> {
    let (scheme, after) = url.split_once("://")?;
    let end = after.find(['/', '\\', '?', '#']).unwrap_or(after.len());
    is_scheme(scheme).then(|| (scheme, &after[..end], &after[end..]))
}

/// The scheme of a fetchable URL: http or https, a non-empty host, no spaces or control bytes.
pub fn http_scheme(url: &str) -> Option<Scheme> {
    let (scheme, authority, _) = split_url(url)?;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if host.is_empty() || url.bytes().any(|b| b.is_ascii_control() || b == b' ') {
        return None;
    }
    if scheme.eq_ignore_ascii_case("http") {
        Some(Scheme::Http)
    } else if scheme.eq_ignore_ascii_case("https") {
        Some(Scheme::Https)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_scheme_rules() {
        assert_eq!(http_scheme("http://host/a"), Some(Scheme::Http));
        assert_eq!(
            http_scheme("HTTPS://u:p@host:8080/a?b#c"),
            Some(Scheme::Https)
        );
        assert_eq!(http_scheme("http://u@/a"), None);
        assert_eq!(http_scheme("http://host/a b"), None);
        assert_eq!(http_scheme("ftp://host/a"), None);
        assert_eq!(http_scheme("file:///etc/passwd"), None);
        assert_eq!(http_scheme("host/a"), None);
        assert_eq!(http_scheme("http:///a"), None);
        assert_eq!(http_scheme("javascript:x"), None);
        assert_eq!(http_scheme("http://host/a\n"), None);
        assert_eq!(http_scheme(""), None);
    }

    #[test]
    fn split_keeps_userinfo_in_authority() {
        assert_eq!(
            split_url("http://u:p@h/x?q"),
            Some(("http", "u:p@h", "/x?q"))
        );
        assert_eq!(split_url("1x://h"), None);
    }
}
