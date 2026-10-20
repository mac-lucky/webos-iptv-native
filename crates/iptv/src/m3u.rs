//! Streaming M3U playlist parser with Kodi pvr.iptvsimple and SIPTV catch-up rules.
//!
//! Parts are ported to Rust from webOS IPTV Player `src/parsers/m3u-parser.ts` (commit 7d8fd30,
//! Copyright 2026 lennylxx, Apache-2.0; see THIRD-PARTY-NOTICES.md) and changed: the attribute
//! scanner, the `#EXTINF` entry mapping, the directive switch, the guide URL keys and the name of
//! a bare URL. Streaming, the caps and the SIPTV, header and correction catch-up rules are this
//! project's. Input is untrusted: every buffer is capped ([`Limits`]), malformed entries are
//! skipped and counted in [`ParseReport`], and only http(s) stream URLs become channels. Nothing
//! here logs: playlist and stream URLs carry tokens.

use std::borrow::Cow;
use std::fmt;
use std::io::{self, BufRead, Read};

use crate::catchup::{self, leading_int};
use crate::model::{Catchup, CatchupMode, Channel, ChannelKey, Playlist};
use crate::url::{http_scheme, split_url};

/// Attributes kept per `#EXTINF` or header, and extras and headers per channel; more are ignored.
const MAX_ATTRS: usize = 64;

/// Leading text read to tell markup documents apart.
const SNIFF_BYTES: usize = 4 * 1024;

/// Archive days of a channel with a catch-up mode but no day count anywhere (Kodi's default).
pub const DEFAULT_CATCHUP_DAYS: u16 = 5;

/// Bounds on what one parse may read and keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Longest line, in input bytes.
    pub max_line_bytes: usize,
    /// Whole input, in bytes.
    pub max_total_bytes: u64,
    /// Channels kept; parsing stops at this many and sets [`ParseReport::truncated`].
    pub max_channels: usize,
}

impl Limits {
    /// What [`parse`] uses; a const so other caps can be derived from it. Sized for a TV app with
    /// 160 MB to live in: a parsed playlist sits in memory twice (the refresh storing it, the
    /// lists showing it), and a live-TV playlist runs to hundreds of channels, not tens of
    /// thousands.
    pub const DEFAULT: Limits = Limits {
        max_line_bytes: 64 * 1024,
        max_total_bytes: 32 * 1024 * 1024,
        max_channels: 50_000,
    };
}

impl Default for Limits {
    fn default() -> Self {
        Limits::DEFAULT
    }
}

/// What a parse skipped. Counts only: lines carry tokens.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParseReport {
    /// The input had an `#EXTM3U` header.
    pub has_header: bool,
    /// Stream locations dropped for not being http(s) URLs (other schemes, paths, stray text).
    pub dropped: usize,
    /// `#EXTINF` entries with no stream location.
    pub orphans: usize,
    /// `#EXTHTTP` payloads that are not a JSON object, and request headers with an invalid
    /// name or value; the entry is kept without them.
    pub bad_headers: usize,
    /// Parsing stopped at [`Limits::max_channels`].
    pub truncated: bool,
}

/// A document that is not an M3U channel list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotM3u {
    /// An HLS master or media playlist: a single stream, which the caller can wrap as a channel.
    Hls,
    /// A DASH manifest: a single stream, as for HLS.
    Dash,
    Xmltv,
    /// HTML or other markup, typically an error page.
    Markup,
    Json,
}

#[derive(Debug)]
pub enum M3uError {
    Io(io::Error),
    /// A line over [`Limits::max_line_bytes`] (1-based line number).
    LineTooLong {
        line: usize,
    },
    /// The input is over [`Limits::max_total_bytes`].
    TooLarge,
    NotM3u(NotM3u),
}

impl fmt::Display for M3uError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            M3uError::Io(e) => write!(f, "reading the playlist failed: {e}"),
            M3uError::LineTooLong { line } => write!(f, "playlist line {line} is too long"),
            M3uError::TooLarge => f.write_str("playlist is too large"),
            M3uError::NotM3u(kind) => {
                let what = match kind {
                    NotM3u::Hls => "an HLS stream",
                    NotM3u::Dash => "a DASH manifest",
                    NotM3u::Xmltv => "an XMLTV guide",
                    NotM3u::Markup => "an HTML or XML document",
                    NotM3u::Json => "JSON",
                };
                write!(f, "expected an M3U playlist, got {what}")
            }
        }
    }
}

impl std::error::Error for M3uError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            M3uError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for M3uError {
    fn from(e: io::Error) -> Self {
        M3uError::Io(e)
    }
}

/// Parses a playlist with the default [`Limits`].
pub fn parse(reader: impl BufRead) -> Result<(Playlist, ParseReport), M3uError> {
    parse_with(reader, &Limits::default())
}

/// Parses a playlist read as UTF-8, or UTF-16 after a byte-order mark; lines end in LF, CRLF or
/// CR.
pub fn parse_with(
    reader: impl BufRead,
    limits: &Limits,
) -> Result<(Playlist, ParseReport), M3uError> {
    let mut lines = Lines::new(reader, limits)?;
    let mut parser = Parser::new(limits.max_channels);
    let mut seen_content = false;
    while let Some(line) = lines.next_line()? {
        if !seen_content && !line.trim().is_empty() {
            seen_content = true;
            match line.trim_start().as_bytes().first() {
                Some(b'{' | b'[') => return Err(M3uError::NotM3u(NotM3u::Json)),
                Some(b'<') => {
                    let head = line.to_ascii_lowercase();
                    return Err(M3uError::NotM3u(sniff_markup(head, &mut lines)?));
                }
                _ => {}
            }
        }
        if parser.line(&line)? {
            break;
        }
    }
    Ok(parser.finish())
}

/// Tells markup documents apart from their leading text.
fn sniff_markup(mut head: String, lines: &mut Lines<impl BufRead>) -> Result<NotM3u, M3uError> {
    while head.len() < SNIFF_BYTES {
        let Some(line) = lines.next_line()? else {
            break;
        };
        head.push(' ');
        head.push_str(&line.to_ascii_lowercase());
    }
    Ok(if head.contains("<mpd") {
        NotM3u::Dash
    } else if ["<tv>", "<tv ", "<!doctype tv"]
        .iter()
        .any(|t| head.contains(t))
    {
        NotM3u::Xmltv
    } else {
        NotM3u::Markup
    })
}

/// Splits input into lines under the line and total caps.
struct Lines<R> {
    reader: R,
    /// `Some(little_endian)` for UTF-16 input.
    utf16: Option<bool>,
    bytes: Vec<u8>,
    units: Vec<u16>,
    /// The last line ended in CR: a LF that follows belongs to it.
    skip_lf: bool,
    read: u64,
    line_no: usize,
    max_line: usize,
    max_total: u64,
}

impl<R: BufRead> Lines<io::Chain<io::Cursor<Vec<u8>>, R>> {
    /// Reads the first two bytes for a UTF-16 byte-order mark and puts back any others.
    fn new(mut reader: R, limits: &Limits) -> io::Result<Self> {
        let mut head = Vec::with_capacity(2);
        reader.by_ref().take(2).read_to_end(&mut head)?;
        let utf16 = match head[..] {
            [0xff, 0xfe] => Some(true),
            [0xfe, 0xff] => Some(false),
            _ => None,
        };
        if utf16.is_some() {
            head.clear();
        }
        Ok(Lines {
            reader: io::Cursor::new(head).chain(reader),
            utf16,
            bytes: Vec::new(),
            units: Vec::new(),
            skip_lf: false,
            read: 0,
            line_no: 0,
            max_line: limits.max_line_bytes,
            max_total: limits.max_total_bytes,
        })
    }
}

impl<R: BufRead> Lines<R> {
    /// The next line without its terminator, invalid sequences replaced; `None` at the end.
    fn next_line(&mut self) -> Result<Option<Cow<'_, str>>, M3uError> {
        let more = match self.utf16 {
            Some(little_endian) => self.fill_utf16(little_endian)?,
            None => self.fill_utf8()?,
        };
        if !more {
            return Ok(None);
        }
        // A UTF-8 BOM, taken off the first line so that no read boundary can hide it.
        if self.line_no == 0 && self.bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
            self.bytes.drain(..3);
        }
        self.line_no += 1;
        Ok(Some(match self.utf16 {
            Some(_) => Cow::Owned(String::from_utf16_lossy(&self.units)),
            None => String::from_utf8_lossy(&self.bytes),
        }))
    }

    fn fill_utf8(&mut self) -> Result<bool, M3uError> {
        self.bytes.clear();
        loop {
            let chunk = match self.reader.fill_buf() {
                Ok(chunk) => chunk,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            if chunk.is_empty() {
                return Ok(!self.bytes.is_empty());
            }
            let skip = usize::from(std::mem::take(&mut self.skip_lf) && chunk[0] == b'\n');
            let body = &chunk[skip..];
            let end = body.iter().position(|&b| b == b'\n' || b == b'\r');
            let take = end.unwrap_or(body.len());
            if self.bytes.len() + take > self.max_line {
                return Err(M3uError::LineTooLong {
                    line: self.line_no + 1,
                });
            }
            self.bytes.extend_from_slice(&body[..take]);
            if let Some(end) = end {
                self.skip_lf = body[end] == b'\r';
            }
            let used = skip + take + usize::from(end.is_some());
            self.reader.consume(used);
            self.count(used)?;
            if end.is_some() {
                return Ok(true);
            }
        }
    }

    fn fill_utf16(&mut self, little_endian: bool) -> Result<bool, M3uError> {
        self.units.clear();
        loop {
            let mut pair = [0u8; 2];
            match self.reader.read_exact(&mut pair) {
                Ok(()) => {}
                // A lone trailing byte is dropped.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(!self.units.is_empty())
                }
                Err(e) => return Err(e.into()),
            }
            self.count(2)?;
            let unit = if little_endian {
                u16::from_le_bytes(pair)
            } else {
                u16::from_be_bytes(pair)
            };
            if std::mem::take(&mut self.skip_lf) && unit == 0x0a {
                continue;
            }
            if unit == 0x0a || unit == 0x0d {
                self.skip_lf = unit == 0x0d;
                return Ok(true);
            }
            if (self.units.len() + 1) * 2 > self.max_line {
                return Err(M3uError::LineTooLong {
                    line: self.line_no + 1,
                });
            }
            self.units.push(unit);
        }
    }

    fn count(&mut self, bytes: usize) -> Result<(), M3uError> {
        self.read += bytes as u64;
        if self.read > self.max_total {
            return Err(M3uError::TooLarge);
        }
        Ok(())
    }
}

/// Defaults an `#EXTM3U` header declares for every entry.
#[derive(Default)]
struct HeaderDefaults {
    mode: Option<CatchupMode>,
    source: Option<String>,
    days: u16,
    tvg_shift_minutes: i32,
    correction_secs: i32,
}

type Attrs = Vec<(String, String)>;

struct Parser {
    playlist: Playlist,
    report: ParseReport,
    header: Attrs,
    defaults: HeaderDefaults,
    /// The `#EXTINF` waiting for its stream location, with an empty URL.
    entry: Option<Channel>,
    max_channels: usize,
}

impl Parser {
    fn new(max_channels: usize) -> Self {
        Parser {
            playlist: Playlist::default(),
            report: ParseReport::default(),
            header: Vec::new(),
            defaults: HeaderDefaults::default(),
            entry: None,
            max_channels,
        }
    }

    /// Takes one line; true when parsing should stop.
    fn line(&mut self, line: &str) -> Result<bool, M3uError> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(false);
        }
        let Some(directive) = line.strip_prefix('#') else {
            return Ok(self.location(line));
        };
        let name_end = directive
            .bytes()
            .position(|b| !(b.is_ascii_alphanumeric() || b == b'-'))
            .unwrap_or(directive.len());
        let (name, rest) = directive.split_at(name_end);
        let body = rest.strip_prefix(':').unwrap_or(rest);
        let is = |tag: &str| name.eq_ignore_ascii_case(tag);
        if is("EXTM3U") {
            self.header(body);
        } else if is("EXTINF") {
            if self.entry.is_some() {
                self.report.orphans += 1;
            }
            self.entry = Some(entry(body, &self.defaults));
        } else if is("PLAYLIST") {
            self.playlist.name = Some(body.trim().to_owned()).filter(|s| !s.is_empty());
        } else if HLS_TAGS.iter().any(|tag| is(tag)) && self.playlist.channels.is_empty() {
            return Err(M3uError::NotM3u(NotM3u::Hls));
        } else if let Some(channel) = &mut self.entry {
            if is("EXTGRP") {
                let groups = split_groups(body);
                if !groups.is_empty() {
                    channel.groups = groups;
                }
            } else if is("EXTVLCOPT") {
                if !vlc_option(channel, body) {
                    self.report.bad_headers += 1;
                }
            } else if is("KODIPROP") {
                if let Some((key, value)) = key_value(body) {
                    set(&mut channel.extra, &format!("kodiprop:{key}"), value);
                }
            } else if is("EXTHTTP") {
                self.report.bad_headers += http_headers(channel, body);
            }
        }
        Ok(false)
    }

    fn header(&mut self, body: &str) {
        self.report.has_header = true;
        for &(key, value) in &scan_attributes(body).0 {
            set(&mut self.header, key, value);
        }
        let attr = |key| attr(&self.header, key);
        // Whole days, at least one: never offer a programme older than the archive.
        let seconds = attr("catchup-time").and_then(leading_int).unwrap_or(0);
        let archive_days = match seconds {
            0 => 0,
            s => days((s / 86_400).max(1)),
        };
        self.defaults = HeaderDefaults {
            mode: attr("catchup")
                .or(attr("catchup-type"))
                .and_then(CatchupMode::parse),
            source: attr("catchup-source").map(str::to_owned),
            days: positive_days(attr("catchup-days")).unwrap_or(archive_days),
            tvg_shift_minutes: attr("tvg-shift").map_or(0, shift_minutes),
            correction_secs: attr("catchup-correction").map_or(0, correction_secs),
        };
        let mut urls = Vec::new();
        for key in ["url-tvg", "x-tvg-url", "tvg-url"] {
            for url in attr(key).unwrap_or_default().split(',').map(str::trim) {
                if http_scheme(url).is_some() && !urls.iter().any(|u| u == url) {
                    urls.push(url.to_owned());
                }
            }
        }
        self.playlist.epg_urls = urls;
    }

    /// A stream location ends the pending entry; true when the channel cap stops parsing.
    fn location(&mut self, url: &str) -> bool {
        let entry = self.entry.take();
        if http_scheme(url).is_none() {
            self.report.dropped += 1;
            return false;
        }
        if self.playlist.channels.len() >= self.max_channels {
            self.report.truncated = true;
            return true;
        }
        let mut channel = entry.unwrap_or_else(|| Channel {
            name: name_from_url(url),
            ..Default::default()
        });
        channel.catchup.source =
            catchup::template(channel.catchup.mode, url, channel.catchup.source.as_deref())
                .filter(|template| http_scheme(template).is_some());
        channel.key = ChannelKey::of_url(url);
        channel.url = url.to_owned();
        self.playlist.channels.push(channel);
        false
    }

    fn finish(mut self) -> (Playlist, ParseReport) {
        if self.entry.is_some() {
            self.report.orphans += 1;
        }
        (self.playlist, self.report)
    }
}

/// Tags that only HLS playlists carry: before the first channel they mean a single stream.
const HLS_TAGS: [&str; 6] = [
    "EXT-X-STREAM-INF",
    "EXT-X-I-FRAME-STREAM-INF",
    "EXT-X-TARGETDURATION",
    "EXT-X-MEDIA-SEQUENCE",
    "EXT-X-PLAYLIST-TYPE",
    "EXT-X-ENDLIST",
];

/// Attributes the parser maps; the rest go to `Channel::extra`.
const KNOWN_ATTRS: [&str; 17] = [
    "tvg-id",
    "tvg-name",
    "tvg-logo",
    "logo",
    "group-title",
    "catchup",
    "catchup-type",
    "catchup-source",
    "catchup-days",
    "catchup-correction",
    "tvg-rec",
    "tvg-chno",
    "channel-number",
    "tvg-num",
    "tvg-shift",
    "timeshift",
    "radio",
];

/// The channel an `#EXTINF` body describes, its URL still empty.
fn entry(body: &str, defaults: &HeaderDefaults) -> Channel {
    // Skip the duration: `-1`, `0`, `10.000`, also when no space follows (`0tvg-id=`).
    let body = body.trim_start_matches([' ', '\t']);
    let body = body.strip_prefix(['+', '-']).unwrap_or(body);
    let body = body.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
    let (attrs, title) = scan_attributes(body);
    let attr = |key| attr(&attrs, key);
    let title = title.map(str::trim).filter(|t| !t.is_empty());
    // The display title first, as Kodi and TiviMate show it (the TS port preferred tvg-name).
    let name = title
        .or(attr("tvg-name"))
        .or(attr("tvg-id"))
        .unwrap_or("Unknown");
    Channel {
        name: name.to_owned(),
        tvg_id: attr("tvg-id").map(str::to_owned),
        tvg_name: attr("tvg-name").map(str::to_owned),
        logo: attr("tvg-logo")
            .or(attr("logo"))
            .filter(|url| http_scheme(url).is_some())
            .map(str::to_owned),
        groups: split_groups(attr("group-title").unwrap_or_default()),
        number: attr("tvg-chno")
            .or(attr("channel-number"))
            .or(attr("tvg-num"))
            .and_then(leading_int)
            .and_then(|n| u32::try_from(n).ok()),
        tvg_shift_minutes: attr("tvg-shift").map_or(defaults.tvg_shift_minutes, shift_minutes),
        radio: attr("radio").is_some_and(|r| r.eq_ignore_ascii_case("true") || r == "1"),
        catchup: catchup_of(&attrs, defaults),
        extra: attrs
            .iter()
            .filter(|(key, _)| !KNOWN_ATTRS.iter().any(|k| k.eq_ignore_ascii_case(key)))
            .map(|&(key, value)| (key.to_ascii_lowercase(), value.to_owned()))
            .collect(),
        ..Default::default()
    }
}

/// Mode, source, days and correction per Kodi. The entry's `catchup` tag wins; the header's mode
/// applies only when the entry has none, so an unknown tag (`disabled`) means no mode. SIPTV's
/// `timeshift="<days>"` (or `tvg-rec`) predates the catch-up tags and implies shift when no mode
/// results; it is not an EPG shift (that is `tvg-shift`). A mode with no day count anywhere gets
/// [`DEFAULT_CATCHUP_DAYS`]. The source stays as declared until the stream URL turns it into a
/// template. The entry's `catchup-correction` wins over the header's whenever it is declared, a
/// value that is not a number included (0, as Kodi's `atof` reads it).
fn catchup_of(attrs: &[(&str, &str)], defaults: &HeaderDefaults) -> Catchup {
    let attr = |key| attr(attrs, key);
    let declared = match attr("catchup").or(attr("catchup-type")) {
        Some(tag) => CatchupMode::parse(tag),
        None => defaults.mode,
    };
    let siptv = positive_days(attr("timeshift")).or(positive_days(attr("tvg-rec")));
    let mode = declared.or(siptv.map(|_| CatchupMode::Shift));
    let source = match attr("catchup-source") {
        Some(source) => Some(source.to_owned()),
        None if mode == defaults.mode => defaults.source.clone(),
        None => None,
    };
    let days = positive_days(attr("catchup-days"))
        .or(siptv)
        .or(Some(defaults.days).filter(|&d| d > 0))
        .unwrap_or(if mode.is_some() {
            DEFAULT_CATCHUP_DAYS
        } else {
            0
        });
    Catchup {
        mode,
        days,
        source,
        correction_secs: attr("catchup-correction")
            .map_or(defaults.correction_secs, correction_secs),
    }
}

/// `tvg-shift` hours as minutes; 0 when it is not a number within a day.
fn shift_minutes(hours: &str) -> i32 {
    hours
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|h| h.abs() <= 24.0)
        .map_or(0, |h| (h * 60.0).round() as i32)
}

/// `catchup-correction` hours as seconds, read like Kodi's `atof`: the leading decimal (`-2.5`,
/// `1.5h`), else 0, truncated to whole seconds; 0 when it is not within a day.
fn correction_secs(hours: &str) -> i32 {
    let s = hours.trim_start();
    let sign = usize::from(s.starts_with(['+', '-']));
    let mut end = sign + s[sign..].bytes().take_while(u8::is_ascii_digit).count();
    if s[end..].starts_with('.') {
        end += 1 + s[end + 1..].bytes().take_while(u8::is_ascii_digit).count();
    }
    s[..end]
        .parse::<f64>()
        .ok()
        .filter(|h| h.abs() <= 24.0)
        .map_or(0, |h| (h * 3_600.0) as i32)
}

/// `key=value` pairs of an `#EXTINF` or `#EXTM3U` body (keys compared in any case, the last of a
/// repeated key kept) and the title after the first unquoted comma. Values may be double-,
/// single- or unquoted, with spaces around `=`.
fn scan_attributes(input: &str) -> (Vec<(&str, &str)>, Option<&str>) {
    let b = input.as_bytes();
    let blank = |i: usize| b.get(i).is_some_and(|&c| c == b' ' || c == b'\t');
    let mut attrs = Vec::new();
    let mut i = 0;
    while i < b.len() {
        while blank(i) {
            i += 1;
        }
        if b.get(i) == Some(&b',') {
            return (attrs, Some(&input[i + 1..]));
        }
        let key_start = i;
        while i < b.len() && !matches!(b[i], b' ' | b'\t' | b'=' | b',') {
            i += 1;
        }
        let key = &input[key_start..i];
        while blank(i) {
            i += 1;
        }
        if b.get(i) != Some(&b'=') {
            continue;
        }
        i += 1;
        while blank(i) {
            i += 1;
        }
        let value = match b.get(i) {
            Some(&quote @ (b'"' | b'\'')) => {
                let start = i + 1;
                let len = b[start..].iter().position(|&c| c == quote);
                i = len.map_or(b.len(), |len| start + len + 1);
                &input[start..len.map_or(b.len(), |len| start + len)]
            }
            _ => {
                let start = i;
                while i < b.len() && !matches!(b[i], b' ' | b'\t' | b',') {
                    i += 1;
                }
                &input[start..i]
            }
        };
        if key.is_empty() {
            continue;
        }
        if let Some(v) = slot(&mut attrs, key, || key) {
            *v = value;
        }
    }
    (attrs, None)
}

/// The value of the pair named `name` (any case); a new pair, named by `new_name` and with an
/// empty value, while there are fewer than [`MAX_ATTRS`], else `None`.
fn slot<'a, K: AsRef<str>, V: Default>(
    pairs: &'a mut Vec<(K, V)>,
    name: &str,
    new_name: impl FnOnce() -> K,
) -> Option<&'a mut V> {
    let i = match pairs
        .iter()
        .position(|(k, _)| k.as_ref().eq_ignore_ascii_case(name))
    {
        Some(i) => i,
        None if pairs.len() < MAX_ATTRS => {
            pairs.push((new_name(), V::default()));
            pairs.len() - 1
        }
        None => return None,
    };
    Some(&mut pairs[i].1)
}

/// Sets `key` (any case, stored lower-cased), replacing an earlier value; past [`MAX_ATTRS`]
/// keys new ones are ignored.
fn set(attrs: &mut Attrs, key: &str, value: &str) {
    if let Some(v) = slot(attrs, key, || key.to_ascii_lowercase()) {
        value.clone_into(v);
    }
}

/// A non-empty attribute value; keys compare in any case.
fn attr<'a, K: AsRef<str>, V: AsRef<str>>(attrs: &'a [(K, V)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k.as_ref().eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_ref())
        .filter(|v| !v.is_empty())
}

/// `;`-separated group names, trimmed, empty and repeated ones dropped.
fn split_groups(raw: &str) -> Vec<String> {
    let mut groups: Vec<String> = Vec::new();
    for group in raw.split(';').map(str::trim).filter(|g| !g.is_empty()) {
        if !groups.iter().any(|g| g == group) && groups.len() < MAX_ATTRS {
            groups.push(group.to_owned());
        }
    }
    groups
}

/// `key=value` of an `#EXTVLCOPT` or `#KODIPROP` body.
fn key_value(body: &str) -> Option<(&str, &str)> {
    let (key, value) = body.split_once('=')?;
    let key = key.trim();
    (!key.is_empty()).then(|| (key, value.trim()))
}

/// `#EXTVLCOPT`: the user agent and referrer become request headers, other options extras.
/// False when a header is invalid.
fn vlc_option(channel: &mut Channel, body: &str) -> bool {
    let Some((key, value)) = key_value(body) else {
        return true;
    };
    let is = |name: &str| key.eq_ignore_ascii_case(name);
    if is("http-user-agent") {
        set_header(&mut channel.http_headers, "User-Agent", value)
    } else if is("http-referrer") || is("http-referer") {
        set_header(&mut channel.http_headers, "Referer", value)
    } else {
        set(&mut channel.extra, &format!("vlcopt:{key}"), value);
        true
    }
}

/// `#EXTHTTP:{"User-Agent":"..."}`: string members become request headers. Returns how many
/// problems it met.
fn http_headers(channel: &mut Channel, body: &str) -> usize {
    let Ok(serde_json::Value::Object(members)) = serde_json::from_str(body) else {
        return 1;
    };
    let mut bad = 0;
    for (name, value) in &members {
        if let Some(value) = value.as_str() {
            bad += usize::from(!set_header(&mut channel.http_headers, name, value));
        }
    }
    bad
}

/// Sets a request header, replacing one of the same name (any case); an empty value sets
/// nothing. False, and nothing set, for a name that is not an HTTP token or a value with control
/// characters (header injection). Only the framing is checked here: which headers a stream may
/// send is the media layer's call at playback.
fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) -> bool {
    let token = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
    let valid = !name.is_empty()
        && name.bytes().all(token)
        && value
            .bytes()
            .all(|b| b == b'\t' || (b >= 0x20 && b != 0x7f));
    if !valid {
        return false;
    }
    if !value.is_empty() {
        if let Some(v) = slot(headers, name, || name.to_owned()) {
            value.clone_into(v);
        }
    }
    true
}

/// A day count over zero.
fn positive_days(value: Option<&str>) -> Option<u16> {
    value.and_then(leading_int).map(days).filter(|&d| d > 0)
}

fn days(n: u64) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// A name for a bare stream URL: its last path segment without the extension, percent-decoded,
/// else its host.
fn name_from_url(url: &str) -> String {
    let (authority, rest) =
        split_url(url).map_or(("", url), |(_, authority, rest)| (authority, rest));
    let path = rest.split(['?', '#']).next().unwrap_or_default();
    let file = path
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or_default();
    let stem = match file.rfind('.') {
        Some(dot) if dot + 1 < file.len() => &file[..dot],
        _ => file,
    };
    let name: String = percent_decode(stem)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    if !name.is_empty() {
        return name;
    }
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = match host.find(']') {
        Some(end) => &host[..=end],
        None => host.split(':').next().unwrap_or_default(),
    };
    match host {
        "" => "Stream".to_owned(),
        host => host.to_owned(),
    }
}

/// `%XX` escapes decoded, invalid ones kept as written.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |i: usize| b.get(i).and_then(|&c| char::from(c).to_digit(16));
        match (b[i], hex(i + 1), hex(i + 2)) {
            (b'%', Some(hi), Some(lo)) => {
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_bare_urls_from_path_or_host() {
        assert_eq!(name_from_url("http://host/a/alpha.ts?x=1"), "alpha");
        assert_eq!(name_from_url("http://host/a%20b.m3u8"), "a b");
        assert_eq!(name_from_url("http://host/bad%zz%0a.ts"), "bad%zz");
        assert_eq!(name_from_url("http://user@host:8080/"), "host");
        assert_eq!(name_from_url("http://[::1]:80/"), "[::1]");
    }

    #[test]
    fn attribute_scanner_always_advances() {
        for input in [
            "=", "==,", "a", "a=", "a='", "a=\"x", " , ", "\t=\t=", "k=v,",
        ] {
            let _ = scan_attributes(input);
        }
        assert_eq!(scan_attributes("a=\"x").0, [("a", "x")]);
        assert_eq!(scan_attributes("A=1 b=2 a=3").0, [("A", "3"), ("b", "2")]);
    }

    #[test]
    fn header_values_with_control_characters_are_refused() {
        let mut headers = Vec::new();
        assert!(!set_header(&mut headers, "X", "a\r\nInjected: 1"));
        assert!(!set_header(&mut headers, "Bad Name", "v"));
        assert!(set_header(&mut headers, "user-agent", "a"));
        assert!(set_header(&mut headers, "User-Agent", "b"));
        assert_eq!(headers, [("user-agent".to_owned(), "b".to_owned())]);
    }
}
