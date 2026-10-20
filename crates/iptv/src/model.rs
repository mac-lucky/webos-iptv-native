//! The IPTV data model shared by the parsers, storage and screens. Times are Unix seconds (UTC).

use std::fmt;

/// Unix time in seconds, UTC.
pub type Timestamp = i64;

/// Now, as Unix seconds.
pub fn unix_now() -> Timestamp {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as Timestamp)
}

/// The form names are compared in when channels are matched to guide channels: trimmed and
/// lowercased. The XMLTV filter and the EPG matcher both use it, so what is stored and what is
/// shown cannot drift apart.
pub fn name_key(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Case- and accent-insensitive form for search: lower case, Latin diacritics off, `yo` as `ye`
/// in Cyrillic. Storage keeps every programme title in this form, so a query folded here finds
/// what search shows.
pub fn fold(s: &str) -> String {
    // U+00E0..=U+00FF and U+0100..=U+017F after lower-casing; `_` keeps the character.
    const LATIN1: &[u8; 32] = b"aaaaaa_ceeeeiiiidnooooo_ouuuuy_y";
    const LATIN_EXT_A: &[u8; 128] =
        b"aaaaaaccccccccddddeeeeeeeeeegggggggghhhhiiiiiiiiii__jjkkklllllll\
          lllnnnnnnnnnoooooo__rrrrrrssssssssttttttuuuuuuuuuuuuwwyyyzzzzzzs";
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        let u = c as u32;
        let mapped = match u {
            // combining marks
            0x300..=0x36F => continue,
            0xE0..=0xFF => LATIN1[(u - 0xE0) as usize],
            0x100..=0x17F => LATIN_EXT_A[(u - 0x100) as usize],
            0x451 => {
                out.push('\u{435}');
                continue;
            }
            _ => b'_',
        };
        out.push(if mapped == b'_' { c } else { mapped as char });
    }
    out
}

impl Channel {
    /// The names a guide may know this channel by: its name, then `tvg-name` when it differs.
    pub fn guide_names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_str()).chain(
            self.tvg_name
                .as_deref()
                .filter(|n| !n.eq_ignore_ascii_case(&self.name)),
        )
    }
}

/// Stable per-stream identity for everything stored per channel (favourites, history, recents).
///
/// FNV-1a (64-bit) of the stream URL with the fragment and volatile query parameters (tokens,
/// signatures, expiries) removed and the remaining parameters sorted, so a rotated token keeps the
/// same key. The stable URL, the volatile parameter list and the key are ported to Rust from
/// webOS IPTV Player `src/utils/channel.ts` (commit 7d8fd30, Copyright 2026 lennylxx, Apache-2.0;
/// see THIRD-PARTY-NOTICES.md) and changed: the hash is widened to 64 bits so a 100k-channel
/// playlist does not collide. Held as `i64` (the same bits) because SQLite integers are signed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChannelKey(pub i64);

impl ChannelKey {
    /// The hash of [`stable_stream_url`], fed piece by piece instead of building that string.
    pub fn of_url(url: &str) -> Self {
        let (base, kept) = stable_parts(url);
        let mut h = fnv1a(base.as_bytes());
        for (i, part) in kept.iter().enumerate() {
            h = fnv1a_more(h, if i == 0 { b"?" } else { b"&" });
            h = fnv1a_more(h, part.as_bytes());
        }
        ChannelKey(h as i64)
    }

    /// Sixteen lowercase hex digits, the form logged.
    pub fn hex(self) -> String {
        format!("{:016x}", self.0)
    }

    /// [`hex`](Self::hex) read back: exactly sixteen hex digits, as settings lines store keys.
    pub fn from_hex(hex: &str) -> Option<Self> {
        if hex.len() != 16 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let bits = u64::from_str_radix(hex, 16).ok()?;
        Some(ChannelKey(bits as i64))
    }

    /// The key of this stream as a channel of playlist `playlist` (a storage row id): the same
    /// stream in two playlists is two channels, each with its own favourite, recents and history
    /// entries. Playlist 1 keeps the stream's own key, the one everything stored before there
    /// were several playlists was kept under.
    pub fn in_playlist(self, playlist: i64) -> Self {
        if playlist == 1 {
            return self;
        }
        let h = fnv1a_more(self.0 as u64, b"\0playlist\0");
        ChannelKey(fnv1a_more(h, &playlist.to_le_bytes()) as i64)
    }
}

/// FNV-1a, 64-bit.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    fnv1a_more(0xcbf2_9ce4_8422_2325, bytes)
}

/// FNV-1a hash `h` continued over `bytes`.
fn fnv1a_more(h: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(h, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

const VOLATILE_QUERY_PARAMS: &[&str] = &[
    "access_token",
    "auth",
    "auth_token",
    "e",
    "exp",
    "expires",
    "expiry",
    "hdnea",
    "hdnts",
    "key",
    "mac",
    "signature",
    "sig",
    "token",
    "wmsauthsign",
];

/// `url` without its fragment and volatile query parameters, the rest sorted.
pub fn stable_stream_url(url: &str) -> String {
    let (base, kept) = stable_parts(url);
    if kept.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", kept.join("&"))
    }
}

/// [`stable_stream_url`] as the part before `?` and the kept parameters, sorted.
fn stable_parts(url: &str) -> (&str, Vec<&str>) {
    let url = url.split('#').next().unwrap_or_default();
    let Some((base, query)) = url.split_once('?') else {
        return (url, Vec::new());
    };
    let mut kept: Vec<&str> = query
        .split('&')
        .filter(|part| !part.is_empty() && !is_volatile(part))
        .collect();
    kept.sort_unstable();
    (base, kept)
}

fn is_volatile(part: &str) -> bool {
    let name = part.split('=').next().unwrap_or_default();
    VOLATILE_QUERY_PARAMS
        .iter()
        .any(|v| v.eq_ignore_ascii_case(name))
}

/// How a channel's archive URL is formed (Kodi pvr.iptvsimple `catchup` modes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatchupMode {
    /// `catchup-source` is the full URL template.
    Default,
    /// Stream URL + `catchup-source`.
    Append,
    /// Stream URL + `utc={utc}&lutc={lutc}` (SIPTV `timeshift`).
    Shift,
    /// Derived from a Flussonic stream URL (`fs` is an alias).
    Flussonic,
    /// Xtream Codes timeshift API.
    Xc,
    /// The source is itself a VOD item.
    Vod,
}

impl CatchupMode {
    /// Parses a `catchup` / `catchup-type` value, case-insensitively. Unknown values are `None`.
    pub fn parse(value: &str) -> Option<Self> {
        const NAMES: &[(&str, CatchupMode)] = &[
            ("default", CatchupMode::Default),
            ("append", CatchupMode::Append),
            ("shift", CatchupMode::Shift),
            ("timeshift", CatchupMode::Shift),
            ("flussonic", CatchupMode::Flussonic),
            ("flussonic-hls", CatchupMode::Flussonic),
            ("flussonic-ts", CatchupMode::Flussonic),
            ("fs", CatchupMode::Flussonic),
            ("xc", CatchupMode::Xc),
            ("vod", CatchupMode::Vod),
        ];
        let v = value.trim();
        NAMES
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(v))
            .map(|&(_, mode)| mode)
    }

    /// The canonical name, which [`CatchupMode::parse`] reads back.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Append => "append",
            Self::Shift => "shift",
            Self::Flussonic => "flussonic",
            Self::Xc => "xc",
            Self::Vod => "vod",
        }
    }
}

/// A channel's archive capability: no mode or zero days means no catch-up.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Catchup {
    pub mode: Option<CatchupMode>,
    pub days: u16,
    /// `catchup-source` template, when declared or derived.
    pub source: Option<String>,
    /// Kodi's `catchup-correction` in seconds: archive URLs ask for the programme's start and
    /// end minus this, for a provider whose archive runs off the guide's clock.
    pub correction_secs: i32,
}

impl Catchup {
    /// True when the guide may offer archive playback: a mode, a window and a URL template.
    pub fn available(&self) -> bool {
        self.mode.is_some() && self.days > 0 && self.source.is_some()
    }
}

/// One playlist entry.
#[derive(Clone, Default, PartialEq)]
pub struct Channel {
    pub key: ChannelKey,
    pub name: String,
    /// `tvg-chno` / `channel-number` / `tvg-num`, when declared.
    pub number: Option<u32>,
    pub logo: Option<String>,
    /// Groups in declaration order; the first is the primary group.
    pub groups: Vec<String>,
    pub url: String,
    pub tvg_id: Option<String>,
    pub tvg_name: Option<String>,
    /// `tvg-shift` converted to minutes.
    pub tvg_shift_minutes: i32,
    pub catchup: Catchup,
    pub radio: bool,
    /// Request headers the stream needs (`#EXTVLCOPT` / `#EXTHTTP`: user agent, referrer, ...).
    pub http_headers: Vec<(String, String)>,
    /// Attributes the parser does not map, kept as declared.
    pub extra: Vec<(String, String)>,
}

/// A parsed playlist.
#[derive(Clone, Default, PartialEq)]
pub struct Playlist {
    pub name: Option<String>,
    pub channels: Vec<Channel>,
    /// Guide URLs from the header (`url-tvg`, `x-tvg-url`, `tvg-url`), de-duplicated, in order.
    pub epg_urls: Vec<String>,
}

impl Playlist {
    /// Group names in order of first appearance.
    pub fn groups(&self) -> Vec<&str> {
        let mut seen = std::collections::HashSet::new();
        self.channels
            .iter()
            .flat_map(|c| c.groups.iter())
            .filter(|g| seen.insert(g.as_str()))
            .map(String::as_str)
            .collect()
    }
}

// Hand-written Debug: stream, logo, archive and guide URLs and header values can carry account
// tokens, so they are masked here and a `{:?}` in a log or test failure never leaks one.
fn masked(u: &Option<String>) -> Option<String> {
    u.as_deref().map(crate::redact::url)
}

fn masked_pairs(pairs: &[(String, String)]) -> Vec<(&str, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.as_str(), crate::redact::text(v)))
        .collect()
}

/// Header names with every value masked whole: any of them can be a credential (`Authorization`,
/// `Cookie`), whatever it looks like.
fn masked_headers(pairs: &[(String, String)]) -> Vec<(&str, &str)> {
    pairs
        .iter()
        .map(|(k, _)| (k.as_str(), crate::redact::MASK))
        .collect()
}

impl fmt::Debug for Catchup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Catchup")
            .field("mode", &self.mode)
            .field("days", &self.days)
            .field("source", &masked(&self.source))
            .field("correction_secs", &self.correction_secs)
            .finish()
    }
}

impl fmt::Debug for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Channel")
            .field("key", &self.key)
            .field("name", &self.name)
            .field("number", &self.number)
            .field("logo", &masked(&self.logo))
            .field("groups", &self.groups)
            .field("url", &crate::redact::url(&self.url))
            .field("tvg_id", &self.tvg_id)
            .field("tvg_name", &self.tvg_name)
            .field("tvg_shift_minutes", &self.tvg_shift_minutes)
            .field("catchup", &self.catchup)
            .field("radio", &self.radio)
            .field("http_headers", &masked_headers(&self.http_headers))
            .field("extra", &masked_pairs(&self.extra))
            .finish()
    }
}

impl fmt::Debug for Playlist {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let epg: Vec<String> = self
            .epg_urls
            .iter()
            .map(|u| crate::redact::url(u))
            .collect();
        f.debug_struct("Playlist")
            .field("name", &self.name)
            .field("channels", &self.channels)
            .field("epg_urls", &epg)
            .finish()
    }
}

/// A guide channel from XMLTV.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EpgChannel {
    pub id: String,
    /// Display names; the first is the main one, the rest are aliases used for matching.
    pub names: Vec<String>,
    pub icon: Option<String>,
}

/// One guide entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Programme {
    pub start: Timestamp,
    pub stop: Timestamp,
    pub title: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub icon: Option<String>,
}

impl Programme {
    pub fn duration_secs(&self) -> i64 {
        self.stop - self.start
    }

    pub fn is_on_air(&self, now: Timestamp) -> bool {
        self.start <= now && now < self.stop
    }
}

#[cfg(test)]
mod tests {
    // Cases marked "ported" come from webOS IPTV Player `src/utils/channel.test.ts`.
    use super::*;

    fn key(url: &str) -> ChannelKey {
        ChannelKey::of_url(url)
    }

    #[test]
    fn folding_ignores_case_and_latin_accents() {
        assert_eq!(fold("Wiadomo\u{15b}ci"), "wiadomosci");
        assert_eq!(fold("\u{141}\u{d3}D\u{179}"), "lodz");
        assert_eq!(
            fold("Za\u{17c}\u{f3}\u{142}\u{107} g\u{119}\u{15b}l\u{105} ja\u{17a}\u{144}"),
            "zazolc gesla jazn"
        );
        assert_eq!(
            fold("\u{401}\u{43b}\u{43a}\u{430}"),
            "\u{435}\u{43b}\u{43a}\u{430}"
        );
        assert_eq!(fold("Cr\u{e8}me Br\u{fb}l\u{e9}e"), "creme brulee");
        assert_eq!(fold("I\u{307}stanbul"), "istanbul");
        assert_eq!(fold("Stra\u{df}e"), "stra\u{df}e");
    }

    #[test]
    fn same_url_same_key() {
        // ported
        assert_eq!(key("http://host/a"), key("http://host/a"));
    }

    #[test]
    fn rotating_tokens_keep_the_key() {
        // ported (two cases)
        assert_eq!(
            key("http://host/a?token=AAA&e=1"),
            key("http://host/a?token=BBB&e=2")
        );
        assert_eq!(
            key("http://host/a?stid=1&mac=A&key=A"),
            key("http://host/a?stid=1&mac=B&key=B")
        );
        assert_ne!(
            key("http://host/a?stid=1&mac=A&key=A"),
            key("http://host/a?stid=2&mac=A&key=A")
        );
    }

    #[test]
    fn stable_params_and_paths_distinguish_streams() {
        // ported (two cases)
        assert_ne!(key("http://host/a?id=1"), key("http://host/a?id=2"));
        assert_ne!(key("http://host/hd"), key("http://host/sd"));
    }

    #[test]
    fn fragment_is_ignored_and_params_are_sorted() {
        // ported (the fragment case; the sorting checks are new)
        assert_eq!(key("http://host/a#frag"), key("http://host/a"));
        assert_eq!(key("http://host/a?b=2&a=1"), key("http://host/a?a=1&b=2"));
        assert_eq!(
            stable_stream_url("http://host/a?TOKEN=x&id=1#f"),
            "http://host/a?id=1"
        );
    }

    #[test]
    fn hex_is_sixteen_digits_even_for_empty() {
        // ported (two cases; 16 digits here, 8 in the web app)
        for url in ["http://host/a", ""] {
            let h = key(url).hex();
            assert_eq!(h.len(), 16);
            assert!(h
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        }
    }

    #[test]
    fn keys_read_back_from_their_hex() {
        for k in [
            ChannelKey(0),
            ChannelKey(-1),
            ChannelKey(i64::MIN),
            ChannelKey(42),
        ] {
            assert_eq!(ChannelKey::from_hex(&k.hex()), Some(k));
        }
        for bad in ["xyz", "00000000000000zz", "+00000000000002a", "1"] {
            assert_eq!(ChannelKey::from_hex(bad), None, "{bad}");
        }
    }

    #[test]
    fn catchup_mode_round_trips() {
        for m in [
            CatchupMode::Default,
            CatchupMode::Append,
            CatchupMode::Shift,
            CatchupMode::Flussonic,
            CatchupMode::Xc,
            CatchupMode::Vod,
        ] {
            assert_eq!(CatchupMode::parse(m.as_str()), Some(m));
        }
    }

    #[test]
    fn guide_names_skip_a_duplicate_tvg_name() {
        let ch = |name: &str, tvg: Option<&str>| Channel {
            name: name.into(),
            tvg_name: tvg.map(Into::into),
            ..Default::default()
        };
        let names = |c: &Channel| c.guide_names().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(names(&ch("News", Some("NEWS"))), ["News"]);
        assert_eq!(names(&ch("News", Some("News HD"))), ["News", "News HD"]);
        assert_eq!(name_key("  News HD "), "news hd");
    }

    #[test]
    fn catchup_mode_aliases() {
        assert_eq!(CatchupMode::parse("FS"), Some(CatchupMode::Flussonic));
        assert_eq!(CatchupMode::parse(" shift "), Some(CatchupMode::Shift));
        assert_eq!(CatchupMode::parse("nope"), None);
    }

    #[test]
    fn debug_masks_tokens() {
        let url = "http://host/pl/13/tx7qx7qx7qx7q/playlist.m3u8?token=SECRETSECRET123";
        let ch = Channel {
            key: key(url),
            name: "News".into(),
            logo: Some("http://host/logo.png?token=SECRETSECRET123".into()),
            url: url.into(),
            catchup: Catchup {
                mode: Some(CatchupMode::Shift),
                days: 7,
                source: Some(format!("{url}&utc={{utc}}")),
                ..Default::default()
            },
            http_headers: vec![
                ("Authorization".into(), "Bearer SECRETSECRET123".into()),
                ("Cookie".into(), "session=plain-word".into()),
            ],
            ..Default::default()
        };
        let p = Playlist {
            channels: vec![ch],
            epg_urls: vec![url.into()],
            ..Default::default()
        };
        let shown = format!("{p:?}");
        assert!(!shown.contains("SECRETSECRET123"), "{shown}");
        assert!(!shown.contains("tx7qx7qx7qx7q"), "{shown}");
        assert!(!shown.contains("plain-word"), "{shown}");
        assert!(shown.contains("News") && shown.contains("Cookie"));
    }

    #[test]
    fn a_stream_is_one_channel_per_playlist_and_playlist_one_keeps_its_key() {
        let k = key("http://host/a?token=x");
        assert_eq!(k.in_playlist(1), k);
        assert_ne!(k.in_playlist(2), k);
        assert_ne!(k.in_playlist(2), k.in_playlist(3));
        assert_eq!(
            k.in_playlist(2),
            key("http://host/a?token=y").in_playlist(2)
        );
    }

    #[test]
    fn the_key_hashes_the_stable_url() {
        for url in [
            "http://host/a",
            "http://host/a?b=2&token=x&a=1#f",
            "http://host/a?token=x",
            "",
        ] {
            assert_eq!(
                key(url),
                ChannelKey(fnv1a(stable_stream_url(url).as_bytes()) as i64)
            );
        }
    }

    #[test]
    fn groups_in_first_appearance_order() {
        let ch = |g: &[&str]| Channel {
            key: key("http://h/x"),
            groups: g.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let p = Playlist {
            channels: vec![ch(&["B"]), ch(&["A", "B"]), ch(&["C"])],
            ..Default::default()
        };
        assert_eq!(p.groups(), ["B", "A", "C"]);
    }
}
