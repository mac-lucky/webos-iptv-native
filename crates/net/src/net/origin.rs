//! **Where a server is, as one value: scheme + host + port.** The type the transport passes around
//! instead of a bare `(host, port)` pair: [`crate::stream`]'s redirect follower walks `Origin`s hop
//! by hop, and [`crate::net`]'s request funnel asks [`url_host`] whether a name needs resolving and
//! applies a [`ResolvePin`].
//!
//! **An https `Origin` is PARSED from a URL, never assembled from an address.** The name a TLS
//! certificate is issued for exists only in the URL; an origin rebuilt from the address that name
//! resolved to connects and then fails hostname validation. [`Origin::parse`] is the front door,
//! and [`Origin::http`] is the deliberately-named plaintext constructor, so `Origin::http(` is the
//! grep that finds every place that assumes cleartext.
//!
//! ## The bracket rule, which is an invariant and not a detail
//!
//! A v6 address appears in two different spellings and they are not interchangeable:
//!
//! * **`getaddrinfo` takes the BARE literal** - `::1`. Hand it `[::1]` and resolution fails.
//! * **A URL takes the BRACKETED form** - `[::1]:8080`. Without the brackets the port colon is
//!   indistinguishable from an address group, and `http://::1:8080` names nothing.
//!
//! [`Origin::host`] is therefore **always unbracketed** (it is the resolver's node argument) and
//! [`Origin::authority`] is **always bracket-correct** (it is URL serialization). Anything that
//! dials calls `host()`; anything that builds a URL calls `authority()` or [`Origin::base`]. The
//! round trip `http://[::1]:8080` → `host() == "::1"` → `authority() == "[::1]:8080"` →
//! `base() == "http://[::1]:8080"` is pinned by a test, because the failure mode without it is
//! silent: URL construction keeps looking right while name resolution quietly stops working.
//!
//! ## Defaults and narrowings
//!
//! * **The default port is the scheme's**: 80 for `http`, 443 for `https` ([`Scheme::default_port`]).
//! * **Only `http` and `https` are recognised.** [`Origin::parse`] refuses anything else rather
//!   than inventing a meaning for it.
//! * **A written port must be dialable** ([`dial_port`]); an empty or out-of-range one is refused,
//!   never silently replaced by the default.

/// Which transport an origin names.
///
/// **Ordered best-first, and the derived `Ord` IS that preference**: https is the only transport a
/// certificate can authenticate, so a caller choosing between two origins for the same resource
/// may sort on this directly. `Default` is [`Scheme::Http`], what a URL with no scheme means;
/// `Default` and `Ord` are independent on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    Https,
    #[default]
    Http,
}

impl Scheme {
    /// The wire spelling, and the one used to serialize an origin.
    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
    /// Does this scheme require a TLS handshake - and therefore a certificate whose name must
    /// match [`Origin::host`]?
    pub fn is_tls(self) -> bool {
        self == Scheme::Https
    }
    /// The port an origin of this scheme means when its URL names none: 80 or 443.
    pub fn default_port(self) -> i32 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }
}

/// A port this client could actually dial, narrowed to the `i32` the transport takes - `None` for
/// anything outside `1..=65535`.
///
/// **The narrowing is the point.** A port read from JSON or a URL arrives as text or an `i64`, and
/// `port as i32` on that WRAPS: `4_294_967_376` becomes `80` and the app dials a port nobody wrote,
/// quietly and with a plausible-looking result. Every site that turns such a value into a
/// connection goes through here.
pub fn dial_port(p: i64) -> Option<i32> {
    (1..=65535).contains(&p).then_some(p as i32)
}

/// **One server's address, completely.** Scheme, host and port - no path, no query, no trailing
/// slash. See the module doc for the bracket invariant on [`Origin::host`] vs [`Origin::authority`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    scheme: Scheme,
    /// **Always UNBRACKETED**, even for a v6 literal - this is the `getaddrinfo` node argument.
    host: String,
    /// The port to dial. [`Origin::parse`] narrows through [`dial_port`], so anything read out of a
    /// URL is in `1..=65535`; [`Origin::new`] and [`Origin::http`] take the caller's `i32` as given.
    port: i32,
}

impl Origin {
    /// Build an origin from parts. `host` may arrive **either** spelling - bare or bracketed -
    /// because a caller holding a v6 address has usually just read it out of a URL authority; it
    /// is stored bare regardless, which is the invariant [`Origin::host`] promises. Unbalanced
    /// brackets are left alone rather than half-stripped.
    pub fn new(scheme: Scheme, host: &str, port: i32) -> Origin {
        Origin {
            scheme,
            host: unbracket(host).to_owned(),
            port,
        }
    }

    /// The **plaintext** constructor, named so that it is greppable: every call is a place that
    /// assumes cleartext. A caller that already has a URL must parse it instead; rebuilding that URL
    /// through this constructor silently downgrades it.
    pub fn http(host: &str, port: i32) -> Origin {
        Origin::new(Scheme::Http, host, port)
    }

    /// Parse an origin out of a URL - **the front door**, and the only way an https origin should
    /// come into existence, because the hostname TLS validates against exists nowhere but the URL.
    ///
    /// `None` when there is no host, when the scheme is one this transport does not speak, or when
    /// a port is written and is not dialable ([`dial_port`]). A path is ignored: an origin is the
    /// prefix, and [`split`] is the entry point for a caller that wants the rest of the URL too.
    /// A MISSING scheme is read as `http`; a missing port is the scheme's default.
    pub fn parse(url: &str) -> Option<Origin> {
        let p = Parts::of(url);
        (p.scheme_known && !p.host.is_empty() && !p.port_bad).then(|| {
            Origin::new(
                p.scheme,
                p.host,
                p.port.unwrap_or(p.scheme.default_port()),
            )
        })
    }

    pub fn scheme(&self) -> Scheme {
        self.scheme
    }
    /// Does reaching this origin need a TLS handshake? The one question a transport asks.
    pub fn is_tls(&self) -> bool {
        self.scheme.is_tls()
    }
    /// **The resolver's node argument - never bracketed**, even for a v6 literal. Also the name a
    /// TLS certificate must match. See the module doc.
    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn port(&self) -> i32 {
        self.port
    }
    /// **URL serialization - `host:port`, bracketing a v6 literal.** The other half of the
    /// bracket invariant: this is what goes in a URL, `host()` is what goes to the resolver.
    pub fn authority(&self) -> String {
        if is_v6_literal(&self.host) {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
    /// `"{scheme}://{authority}"` - no trailing slash, no path. Round-trips through
    /// [`Origin::parse`]. The port is always written, default or not.
    pub fn base(&self) -> String {
        format!("{}://{}", self.scheme.as_str(), self.authority())
    }

    /// **How an origin is written in the event log** - the authority alone for a plaintext origin,
    /// the whole base URL for anything else, so a TLS hop is never mistaken for a plaintext one in a
    /// log. It carries no path, no query and no credential.
    pub fn log_form(&self) -> String {
        if self.scheme == Scheme::Http {
            self.authority()
        } else {
            self.base()
        }
    }
}

/// Split a URL into its origin and the rest of it (path + query, `""` when there is none).
///
/// **Total**, unlike [`Origin::parse`], for a caller that has no failure path to take. Every
/// degenerate input therefore yields *something*: a missing scheme reads as `http`, an unknown one
/// is discarded rather than mistaken for a host, and a port that is absent or undialable becomes the
/// scheme's default rather than wrapping into a port nobody wrote down.
pub fn split(url: &str) -> (Origin, &str) {
    let p = Parts::of(url);
    (
        Origin::new(p.scheme, p.host, p.port.unwrap_or(p.scheme.default_port())),
        p.path,
    )
}

/// The HOST of a URL, **bare** - a borrowed view for a caller that only wants to look at the host
/// (is it a numeric literal, or a name that needs resolving?) and has no use for an owned
/// [`Origin`]. This lives here so that there is exactly one reading of a URL in the crate.
pub fn url_host(url: &str) -> &str {
    Parts::of(url).host
}

/// The raw reading of a URL, before any policy is applied to it. Two callers with two different
/// tolerances ([`Origin::parse`] refuses what [`split`] papers over), so the reading and the
/// judgement are separate.
struct Parts<'a> {
    scheme: Scheme,
    /// `false` when the URL named a scheme that is neither `http` nor `https`.
    scheme_known: bool,
    /// Unbracketed already.
    host: &'a str,
    /// `None` when the URL wrote no port at all.
    port: Option<i32>,
    /// `true` when a `:` was written and what follows it is not a dialable port - the case that
    /// must not silently become the default port for a caller that can refuse.
    ///
    /// **An EMPTY port counts.** `http://192.0.2.10:` is a truncated write or a half-finished hand
    /// edit, not a request for the default, and reading it as one is exactly the "dial a port
    /// nobody wrote down" outcome [`dial_port`] exists to prevent. RFC 3986 would let an empty port
    /// mean the default; a reader of possibly-corrupt input wants the opposite answer.
    port_bad: bool,
    path: &'a str,
}

impl<'a> Parts<'a> {
    fn of(url: &'a str) -> Parts<'a> {
        // A scheme is case-insensitive (RFC 3986 §3.1): `HTTPS://` is https, and must default to 443.
        let (scheme, scheme_known, rest) = match url.split_once("://") {
            Some((s, r)) if s.eq_ignore_ascii_case("http") => (Scheme::Http, true, r),
            Some((s, r)) if s.eq_ignore_ascii_case("https") => (Scheme::Https, true, r),
            // A scheme we do not speak. The authority is still readable, so `split` can go on;
            // `parse` refuses on `scheme_known`.
            Some((_, r)) => (Scheme::Http, false, r),
            None => (Scheme::Http, true, url),
        };
        // The authority ends at the first `/` - everything from there is the path.
        let (auth, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        // A bracketed v6 literal carries its own colons, so the port separator is the one AFTER
        // the closing bracket.
        let (host, port_txt) = match auth.strip_prefix('[').and_then(|r| r.split_once(']')) {
            Some((h, after)) => (h, after.strip_prefix(':')),
            None => match auth.split_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (auth, None),
            },
        };
        // `dial_port`, not a bare `parse::<i32>()`: the same narrowing every port goes through.
        let port = port_txt
            .and_then(|t| t.parse::<i64>().ok())
            .and_then(dial_port);
        Parts {
            scheme,
            scheme_known,
            host,
            port,
            port_bad: port_txt.is_some() && port.is_none(),
            path,
        }
    }
}

/// **A DNS shortcut for one origin: dial this address instead of resolving [`Origin::host`].** It
/// never validates anything - TLS still checks the certificate against the hostname, which stays
/// in the URL and in SNI. libcurl is merely told the answer DNS would have given
/// (`CURLOPT_RESOLVE`), so no lookup happens at all.
///
/// Useful whenever the caller already knows where a name lives and DNS is slow, absent or wrong:
/// a host whose address was learned out of band, a LAN service reached by name while the uplink is
/// down. A pin is built only for a TLS origin whose host is a NAME (a literal has nothing to
/// resolve, and the plaintext socket in [`crate::stream`] dials the literal it is given) and only
/// from an address that parses; anything else yields `None` and the request resolves through DNS
/// exactly as before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvePin {
    host: String,
    port: i32,
    addr: std::net::IpAddr,
}

/// `CURLOPT_RESOLVE` learned bracketed IPv6 addresses in 7.57.0 (`0x073900`). Below it, the entry
/// parser is `sscanf("%255[^:]:%d:%255s")` and takes the bare literal - verified in the 7.53.1
/// source the development television runs.
pub const CURL_RESOLVE_BRACKETS_SINCE: u32 = 0x07_39_00;

impl ResolvePin {
    /// The pin for `origin` at `address` (bare or bracketed) - `None` unless `origin` is TLS, its
    /// host is a name rather than a literal, and `address` is an IP address.
    pub fn for_origin(origin: &Origin, address: &str) -> Option<ResolvePin> {
        if !origin.is_tls() || origin.host().is_empty() {
            return None;
        }
        if origin.host().parse::<std::net::IpAddr>().is_ok() {
            return None;
        }
        let addr: std::net::IpAddr = unbracket(address.trim()).parse().ok()?;
        Some(ResolvePin {
            host: origin.host().to_owned(),
            port: origin.port(),
            addr,
        })
    }

    /// A pin with none of [`ResolvePin::for_origin`]'s rules - the loopback tests' way of proving
    /// that a name reached the wire through the pin and not through DNS, plaintext included.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(host: &str, port: i32, addr: std::net::IpAddr) -> ResolvePin {
        ResolvePin {
            host: host.to_owned(),
            port,
            addr,
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn port(&self) -> i32 {
        self.port
    }
    pub fn addr(&self) -> std::net::IpAddr {
        self.addr
    }

    /// The `CURLOPT_RESOLVE` list entry - `host:port:address`, the v6 address bracketed only on a
    /// libcurl that parses brackets (see [`CURL_RESOLVE_BRACKETS_SINCE`]).
    pub fn entry(&self, curl_version_num: u32) -> String {
        match self.addr {
            std::net::IpAddr::V4(a) => format!("{}:{}:{a}", self.host, self.port),
            std::net::IpAddr::V6(a) if curl_version_num >= CURL_RESOLVE_BRACKETS_SINCE => {
                format!("{}:{}:[{a}]", self.host, self.port)
            }
            std::net::IpAddr::V6(a) => format!("{}:{}:{a}", self.host, self.port),
        }
    }

    /// `"v4"` or `"v6"` - the only thing about a pin the event log says. Neither the host nor the
    /// address belongs in a file users paste into issues.
    pub fn family(&self) -> &'static str {
        if self.addr.is_ipv6() {
            "v6"
        } else {
            "v4"
        }
    }

    /// Host and address, for a test's eyes. Never logged - see [`ResolvePin::family`].
    #[cfg(test)]
    pub fn log_form(&self) -> String {
        format!("{} -> {}", self.host, self.addr)
    }
}

/// Strip the brackets a URL authority puts around a v6 literal, if they are there.
fn unbracket(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
        .unwrap_or(host)
}

/// Is this bare host a v6 literal - i.e. does it need bracketing before it can carry a port?
/// A colon cannot appear in a hostname or in a dotted quad, so one colon is the whole test.
fn is_v6_literal(host: &str) -> bool {
    host.contains(':')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The bracket invariant, as a round trip.** `host()` is the resolver's node and is bare;
    /// `authority()` is URL serialization and is bracketed; `base()` reproduces the input exactly.
    ///
    /// Without this the failure is silent in the worst direction: every URL the app *builds* keeps
    /// looking correct while `getaddrinfo` is handed `[::1]` and quietly resolves nothing.
    #[test]
    fn a_v6_origin_is_bare_for_the_resolver_and_bracketed_for_a_url() {
        let o = Origin::parse("http://[::1]:8080").expect("a bracketed v6 origin parses");
        assert_eq!(o.host(), "::1", "the getaddrinfo node is NEVER bracketed");
        assert_eq!(o.authority(), "[::1]:8080", "…and a URL authority always is");
        assert_eq!(
            o.base(),
            "http://[::1]:8080",
            "so the round trip is byte-identical"
        );
        assert_eq!(o.port(), 8080);

        // and the same holds for an origin built from parts, whichever spelling the caller has
        assert_eq!(
            Origin::http("[2001:db8::1]", 8080),
            Origin::http("2001:db8::1", 8080)
        );
        assert_eq!(
            Origin::http("2001:db8::1", 8080).authority(),
            "[2001:db8::1]:8080"
        );
        assert_eq!(
            Origin::http("2001:db8::1", 8080).base(),
            "http://[2001:db8::1]:8080"
        );
    }

    /// A plaintext v4 origin serializes to exactly the bytes it was read from.
    #[test]
    fn a_v4_origin_round_trips_unchanged() {
        let o = Origin::parse("http://192.0.2.10:8080").expect("parses");
        assert_eq!(
            (o.scheme(), o.host(), o.port()),
            (Scheme::Http, "192.0.2.10", 8080)
        );
        assert_eq!(o.authority(), "192.0.2.10:8080");
        assert_eq!(o.base(), "http://192.0.2.10:8080");
        assert!(!o.is_tls());
    }

    /// An https origin keeps the NAME, which is what the certificate is issued for.
    #[test]
    fn an_https_origin_keeps_the_name_tls_validates_against() {
        let o = Origin::parse("https://addon.example.com:8443").expect("parses");
        assert_eq!(o.scheme(), Scheme::Https);
        assert!(o.is_tls());
        assert_eq!(o.host(), "addon.example.com");
        assert_eq!(o.base(), "https://addon.example.com:8443");
    }

    /// The defaults: no scheme is `http`, no port is the scheme's own.
    #[test]
    fn a_missing_scheme_is_http_and_a_missing_port_is_the_schemes_default() {
        let o = Origin::parse("192.0.2.10").expect("a bare host is an origin");
        assert_eq!(
            (o.scheme(), o.host(), o.port()),
            (Scheme::Http, "192.0.2.10", 80)
        );
        let o = Origin::parse("https://addon.example.com").unwrap();
        assert_eq!(o.port(), 443);
        assert_eq!(o.base(), "https://addon.example.com:443");
        assert_eq!(Origin::parse("http://addon.example.com/x").unwrap().port(), 80);
        assert_eq!(Origin::parse("https://[2001:db8::1]/").unwrap().port(), 443);
        assert_eq!((Scheme::Http.default_port(), Scheme::Https.default_port()), (80, 443));
        // the scheme is read case-insensitively, so its default port is too
        assert_eq!(
            Origin::parse("HTTPS://Addon.Example.com").map(|o| (o.scheme(), o.port())),
            Some((Scheme::Https, 443))
        );
        assert_eq!(split("Http://192.0.2.10/x").0.base(), "http://192.0.2.10:80");
    }

    /// A path is not part of an origin - `base()` never grows one, whatever came in.
    #[test]
    fn a_path_is_not_part_of_the_origin() {
        assert_eq!(
            Origin::parse("https://addon.example.com/manifest.json?x=1")
                .unwrap()
                .base(),
            "https://addon.example.com:443"
        );
        assert_eq!(
            Origin::parse("http://192.0.2.10:8080/").unwrap().base(),
            "http://192.0.2.10:8080"
        );
    }

    /// What [`Origin::parse`] refuses. Each of these would otherwise dial something nobody named.
    #[test]
    fn parse_refuses_what_cannot_be_dialled() {
        assert_eq!(Origin::parse(""), None, "no host at all");
        assert_eq!(Origin::parse("http://"), None);
        assert_eq!(
            Origin::parse("ftp://h:21"),
            None,
            "a scheme this transport does not speak"
        );
        // The port narrowing that `dial_port` exists for: `4_294_967_376 as i32` is 80.
        assert_eq!(Origin::parse("http://192.0.2.10:4294967376"), None);
        assert_eq!(Origin::parse("http://192.0.2.10:0"), None);
        assert_eq!(Origin::parse("http://192.0.2.10:70000"), None);
        assert_eq!(Origin::parse("http://192.0.2.10:abc"), None);
        // an EMPTY port is a port that WAS written - a truncated write, not a request for the
        // default.
        assert_eq!(Origin::parse("http://192.0.2.10:"), None);
        assert_eq!(Origin::parse("https://addon.example.com:"), None);
        // ...while a URL that writes no `:` at all really does mean the default
        assert_eq!(Origin::parse("http://192.0.2.10").unwrap().port(), 80);
    }

    /// [`split`] is the TOTAL sibling, so the same inputs that [`Origin::parse`] refuses still come
    /// back as something, with the port defaulted rather than wrapped.
    #[test]
    fn split_is_total_and_hands_back_the_path() {
        let (o, p) = split("http://192.0.2.10:8080/streams/movie/tt1.json?a=1");
        assert_eq!(o.base(), "http://192.0.2.10:8080");
        assert_eq!(p, "/streams/movie/tt1.json?a=1");

        assert_eq!(split("192.0.2.10").0.base(), "http://192.0.2.10:80");
        assert_eq!(
            split("192.0.2.10").1,
            "",
            "no path is an empty path, not a slash"
        );
        assert_eq!(
            split("http://192.0.2.10:70000/x").0.port(),
            80,
            "undialable → the default"
        );
        assert_eq!(
            split("https://addon.example.com:/x").0.port(),
            443,
            "…and so does an empty one, here, by scheme"
        );
        assert_eq!(split("http://192.0.2.10:/x").1, "/x");
        assert_eq!(
            split("https://[2001:db8::1]:8443/y").0.base(),
            "https://[2001:db8::1]:8443"
        );
        assert_eq!(split("https://[2001:db8::1]:8443/y").1, "/y");
    }

    /// **The event log's spelling**: the bare authority for plaintext, the whole URL for TLS.
    #[test]
    fn the_log_form_is_the_bare_authority_for_http_and_the_whole_url_for_anything_else() {
        assert_eq!(
            Origin::http("192.0.2.10", 8080).log_form(),
            "192.0.2.10:8080"
        );
        assert_eq!(
            Origin::parse("https://addon.example.com").unwrap().log_form(),
            "https://addon.example.com:443"
        );
        // a v6 literal is bracketed either way - it is a URL authority, not a resolver node
        assert_eq!(
            Origin::http("2001:db8::1", 8080).log_form(),
            "[2001:db8::1]:8080"
        );
    }

    #[test]
    fn url_host_reads_the_urls_own_host_without_scheme_or_port() {
        assert_eq!(url_host("http://203.0.113.9:31234"), "203.0.113.9");
        assert_eq!(
            url_host("https://addon.example.com/manifest.json"),
            "addon.example.com"
        );
        assert_eq!(
            url_host("http://[2001:db8::1]:8080"),
            "2001:db8::1",
            "the port is not a v6 group"
        );
    }

    /// The scheme's wire spelling is what `base()` is built from, so it is pinned rather than left
    /// to a `Debug` derive.
    #[test]
    fn a_scheme_knows_its_wire_spelling_and_whether_it_needs_tls() {
        assert_eq!(
            (Scheme::Http.as_str(), Scheme::Https.as_str()),
            ("http", "https")
        );
        assert!(!Scheme::Http.is_tls() && Scheme::Https.is_tls());
        assert_eq!(Scheme::default(), Scheme::Http, "a URL with no scheme");
        // The ranking and the default are independent: a URL that names no scheme is plaintext,
        // while an origin CHOSEN between is TLS first.
        assert!(Scheme::Https < Scheme::Http, "TLS is the better connection");
    }

    // ---- ResolvePin ----

    #[test]
    fn a_tls_name_with_an_address_yields_a_v4_pin() {
        let o = Origin::parse("https://addon.example.com").unwrap();
        let p = ResolvePin::for_origin(&o, "192.0.2.10").expect("pinned");
        assert_eq!(p.host(), "addon.example.com");
        assert_eq!(p.port(), 443);
        assert_eq!(p.addr(), "192.0.2.10".parse::<std::net::IpAddr>().unwrap());
        assert_eq!(p.entry(0x07_35_01), "addon.example.com:443:192.0.2.10");
        assert_eq!(p.log_form(), "addon.example.com -> 192.0.2.10");
        assert_eq!(p.family(), "v4");
    }

    #[test]
    fn a_tls_name_with_a_v6_address_yields_a_v6_pin() {
        let o = Origin::parse("https://addon.example.com:8443").unwrap();
        let p = ResolvePin::for_origin(&o, "2001:db8::1").expect("pinned");
        assert_eq!(p.addr(), "2001:db8::1".parse::<std::net::IpAddr>().unwrap());
        assert_eq!(p.family(), "v6");
        // ...and a bracketed spelling of the address is the same address
        assert_eq!(ResolvePin::for_origin(&o, "[2001:db8::1]"), Some(p));
    }

    #[test]
    fn the_entry_is_bare_v6_below_7_57_and_bracketed_from_7_57() {
        let o = Origin::parse("https://addon.example.com:8443").unwrap();
        let p = ResolvePin::for_origin(&o, "2001:db8::1").unwrap();
        // the development television's 7.53.1 parses `%255[^:]:%d:%255s`: bare
        assert_eq!(p.entry(0x07_35_01), "addon.example.com:8443:2001:db8::1");
        assert_eq!(
            p.entry(CURL_RESOLVE_BRACKETS_SINCE),
            "addon.example.com:8443:[2001:db8::1]"
        );
        assert_eq!(p.entry(0x08_00_00), "addon.example.com:8443:[2001:db8::1]");
    }

    #[test]
    fn no_pin_without_a_tls_name_and_a_parseable_address() {
        let name = Origin::parse("https://addon.example.com").unwrap();
        // an address that is not one: no pin, DNS as before
        assert_eq!(ResolvePin::for_origin(&name, ""), None);
        assert_eq!(ResolvePin::for_origin(&name, "nas.local"), None);
        assert_eq!(ResolvePin::for_origin(&name, "192.0.2.256"), None);
        // plaintext never pins: the raw socket dials the literal it is given
        let http = Origin::parse("http://addon.example.com").unwrap();
        assert_eq!(ResolvePin::for_origin(&http, "192.0.2.10"), None);
        // a literal host has nothing to resolve
        let lit = Origin::parse("https://192.0.2.10").unwrap();
        assert_eq!(ResolvePin::for_origin(&lit, "192.0.2.10"), None);
        let lit6 = Origin::parse("https://[2001:db8::1]").unwrap();
        assert_eq!(ResolvePin::for_origin(&lit6, "2001:db8::1"), None);
    }
}
