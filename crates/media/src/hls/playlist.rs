//! **The HLS playlist parser** (RFC 8216 §4): a master (multivariant) playlist or a media playlist,
//! from its text and the URL it was answered from.
//!
//! General, not tailored to one server: every tag that changes which bytes are fetched or how they
//! are assembled is read - `EXTINF`, `EXT-X-TARGETDURATION`, `EXT-X-MEDIA-SEQUENCE`,
//! `EXT-X-DISCONTINUITY(-SEQUENCE)`, `EXT-X-ENDLIST`, `EXT-X-PLAYLIST-TYPE`, `EXT-X-BYTERANGE`,
//! `EXT-X-KEY`, `EXT-X-MAP`, `EXT-X-GAP` and, in a master, `EXT-X-STREAM-INF` and `EXT-X-MEDIA`.
//! Everything else (`EXT-X-PROGRAM-DATE-TIME`, `EXT-X-INDEPENDENT-SEGMENTS`, `EXT-X-START`, LL-HLS
//! parts and preload hints, `EXT-X-I-FRAME-STREAM-INF`, comments) is skipped, which RFC 8216 §6.3.1
//! asks of a client for tags it does not recognise.
//!
//! **Every URI leaves this module absolute**, resolved against the playlist's own (effective) URL
//! by the same RFC 3986 resolver the media plane follows redirects with
//! (`tv_net::stream::redirect::resolve_location`), so a relative segment path, a `../` key path and
//! an absolute CDN URL all come out as something the transport can request. A URI that resolver
//! refuses (another scheme - `skd://`, `data:` - or a control byte) is a [`ParseError::BadUri`].
//!
//! Durations are integer **microseconds**: `EXTINF:5.005` sums exactly, and the window arithmetic
//! of [`super::live`] never accumulates float error.
//!
//! Errors carry a line number and a kind, never the line: a playlist URI may carry a credential.

use tv_net::net::origin::Origin;
use tv_net::stream::redirect::{resolve_location, Target};

/// Which playlist a text is.
#[derive(Clone, Debug, PartialEq)]
pub enum Playlist {
    Master(MasterPlaylist),
    Media(MediaPlaylist),
}

/// A master (multivariant) playlist: the variant streams and the renditions they refer to.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MasterPlaylist {
    pub variants: Vec<Variant>,
    pub renditions: Vec<Rendition>,
}

/// One `EXT-X-STREAM-INF` and the URI under it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Variant {
    pub uri: String,
    /// `BANDWIDTH`, bits per second (required by the RFC; 0 when a server left it out).
    pub bandwidth: u64,
    pub average_bandwidth: Option<u64>,
    /// `CODECS`, one RFC 6381 entry each (`avc1.64001f`, `mp4a.40.2`). Empty when not stated.
    pub codecs: Vec<String>,
    /// `RESOLUTION`, `(width, height)`.
    pub resolution: Option<(u32, u32)>,
    pub frame_rate: Option<f64>,
    /// `AUDIO`: the group of `EXT-X-MEDIA:TYPE=AUDIO` renditions this variant plays with.
    pub audio_group: Option<String>,
}

/// `EXT-X-MEDIA`'s `TYPE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaType {
    Audio,
    Video,
    Subtitles,
    ClosedCaptions,
}

/// One `EXT-X-MEDIA` rendition.
#[derive(Clone, Debug, PartialEq)]
pub struct Rendition {
    pub kind: MediaType,
    pub group_id: String,
    pub name: String,
    pub language: Option<String>,
    /// Absent: the rendition is carried in the variant's own segments (muxed).
    pub uri: Option<String>,
    pub default: bool,
}

/// `EXT-X-BYTERANGE`: `length` bytes of the resource from `offset`, both resolved (an omitted
/// offset is the byte after the previous sub-range of the same resource).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    pub length: u64,
    pub offset: u64,
}

/// How a segment (or an initialization section) is encrypted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Encryption {
    Clear,
    /// `METHOD=AES-128` with the identity key format: the key is the 16 bytes at `uri`.
    Aes128 {
        uri: String,
        /// The tag's `IV`. `None`: the media sequence number (RFC 8216 §5.2).
        iv: Option<[u8; 16]>,
    },
    /// Anything this client cannot decrypt: `SAMPLE-AES`, `SAMPLE-AES-CTR`, or a key format other
    /// than `identity` (a DRM system). The method / format text, for the error.
    Unsupported(String),
}

/// `EXT-X-MAP`: the media initialization section (an fMP4 `ftyp`+`moov`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitSection {
    pub uri: String,
    pub range: Option<ByteRange>,
    /// The key in force where the tag appears.
    pub key: Encryption,
}

/// One media segment.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub uri: String,
    /// `EXTINF`, microseconds.
    pub duration_us: u64,
    /// The media sequence number: `EXT-X-MEDIA-SEQUENCE` plus the segment's position.
    pub sequence: u64,
    pub range: Option<ByteRange>,
    /// An `EXT-X-DISCONTINUITY` precedes this segment.
    pub discontinuity: bool,
    /// The discontinuity sequence number this segment belongs to.
    pub discontinuity_sequence: u64,
    pub key: Encryption,
    /// The initialization section in force, if any.
    pub map: Option<InitSection>,
    /// `EXT-X-GAP`: the segment is absent and must not be fetched.
    pub gap: bool,
}

/// `EXT-X-PLAYLIST-TYPE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaylistType {
    Vod,
    Event,
}

/// A media playlist.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaPlaylist {
    /// `EXT-X-TARGETDURATION`, microseconds. A playlist without the tag gets its longest segment's
    /// duration rounded up to a whole second (and at least one): what the tag would have said.
    pub target_duration_us: u64,
    pub media_sequence: u64,
    pub discontinuity_sequence: u64,
    /// `EXT-X-ENDLIST`: no segment will ever be added.
    pub endlist: bool,
    pub playlist_type: Option<PlaylistType>,
    pub segments: Vec<Segment>,
}

impl MediaPlaylist {
    /// Will this playlist grow? A VOD or ended playlist will not.
    pub fn is_live(&self) -> bool {
        !self.endlist && self.playlist_type != Some(PlaylistType::Vod)
    }

    /// The sum of every segment's duration, microseconds.
    pub fn duration_us(&self) -> u64 {
        self.segments.iter().map(|s| s.duration_us).sum()
    }

    /// The sequence number one past the last segment.
    pub fn end_sequence(&self) -> u64 {
        self.media_sequence + self.segments.len() as u64
    }

    /// The segment with media sequence number `seq`.
    pub fn by_sequence(&self, seq: u64) -> Option<&Segment> {
        let i = seq.checked_sub(self.media_sequence)?;
        self.segments.get(usize::try_from(i).ok()?)
    }
}

/// Why a playlist was refused. A line number (1-based) and a kind; never the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The text does not begin with `#EXTM3U`: not a playlist at all (an error page, a binary).
    NotAPlaylist,
    /// A tag's value could not be read.
    BadTag { line: usize },
    /// A URI the transport cannot request (another scheme, a control byte, an unusable base).
    BadUri { line: usize },
    /// `EXT-X-STREAM-INF` with no URI line after it, or an `EXT-X-MAP`/`EXT-X-KEY` without a
    /// required `URI`.
    MissingUri { line: usize },
    /// `EXT-X-BYTERANGE` without an offset, after a segment of another resource (or none).
    ByteRangeWithoutBase { line: usize },
    /// A segment URI with no `EXTINF` before it.
    MissingDuration { line: usize },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::NotAPlaylist => f.write_str("not an M3U8 playlist (no #EXTM3U)"),
            ParseError::BadTag { line } => write!(f, "playlist line {line}: unreadable tag"),
            ParseError::BadUri { line } => write!(f, "playlist line {line}: a URI this client cannot request"),
            ParseError::MissingUri { line } => write!(f, "playlist line {line}: a required URI is missing"),
            ParseError::ByteRangeWithoutBase { line } => {
                write!(f, "playlist line {line}: a byte range without an offset has no previous range to follow")
            }
            ParseError::MissingDuration { line } => write!(f, "playlist line {line}: a segment without EXTINF"),
        }
    }
}

/// An attribute list (RFC 8216 §4.2): `NAME=value` pairs, comma-separated, a quoted value
/// possibly holding commas. Names are kept as written (they are uppercase by the grammar).
fn attributes(s: &str) -> Option<Vec<(&str, AttrValue<'_>)>> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let eq = rest.find('=')?;
        let name = rest[..eq].trim();
        if name.is_empty() {
            return None;
        }
        rest = &rest[eq + 1..];
        let (value, after) = if let Some(q) = rest.strip_prefix('"') {
            let end = q.find('"')?;
            (AttrValue::Quoted(&q[..end]), &q[end + 1..])
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            (AttrValue::Plain(rest[..end].trim()), &rest[end..])
        };
        out.push((name, value));
        rest = after.trim_start();
        match rest.strip_prefix(',') {
            Some(r) => rest = r.trim_start(),
            None if rest.is_empty() => {}
            None => return None,
        }
    }
    Some(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttrValue<'a> {
    Quoted(&'a str),
    Plain(&'a str),
}

impl<'a> AttrValue<'a> {
    fn text(self) -> &'a str {
        match self {
            AttrValue::Quoted(s) | AttrValue::Plain(s) => s,
        }
    }
}

fn attr<'a>(list: &[(&str, AttrValue<'a>)], name: &str) -> Option<AttrValue<'a>> {
    list.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| *v)
}

/// A decimal number of seconds as microseconds (`5.005` → 5 005 000). Negative, NaN, infinite or
/// absurd (more than a year) is unreadable.
fn seconds_us(s: &str) -> Option<u64> {
    let v: f64 = s.trim().parse().ok()?;
    (v.is_finite() && (0.0..=31_536_000.0).contains(&v)).then(|| (v * 1_000_000.0).round() as u64)
}

/// `0x`-prefixed hex, at most 128 bits, right-aligned.
fn hex_iv(s: &str) -> Option<[u8; 16]> {
    let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    if h.is_empty() || h.len() > 32 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let padded = format!("{h:0>32}");
    let mut iv = [0u8; 16];
    for (i, b) in iv.iter_mut().enumerate() {
        *b = u8::from_str_radix(&padded[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(iv)
}

/// `<n>[@<o>]`.
fn byte_range_parts(s: &str) -> Option<(u64, Option<u64>)> {
    let (n, o) = match s.trim().split_once('@') {
        Some((n, o)) => (n, Some(o.trim().parse().ok()?)),
        None => (s.trim(), None),
    };
    Some((n.trim().parse().ok()?, o))
}

fn resolution(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.trim().split_once(['x', 'X'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// The resolution base: the playlist's URL as a redirect target.
fn base_of(url: &str) -> Option<Target> {
    let origin = Origin::parse(url)?;
    let (_, path) = tv_net::net::origin::split(url);
    let path = if path.is_empty() { "/".to_owned() } else { path.to_owned() };
    Some(Target { origin, path })
}

/// Resolve `uri` against the playlist's URL.
pub fn resolve(base: &str, uri: &str) -> Option<String> {
    resolve_location(&base_of(base)?, uri).map(|t| t.url())
}

/// Parse `text`, answered from `url` (the EFFECTIVE URL, after redirects).
pub fn parse(text: &str, url: &str) -> Result<Playlist, ParseError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines().map(str::trim).enumerate().map(|(i, l)| (i + 1, l));
    match lines.find(|(_, l)| !l.is_empty()) {
        Some((_, first)) if first.starts_with("#EXTM3U") => {}
        _ => return Err(ParseError::NotAPlaylist),
    }
    let base = base_of(url);
    let resolve_at = |uri: &str, line: usize| -> Result<String, ParseError> {
        let b = base.as_ref().ok_or(ParseError::BadUri { line })?;
        resolve_location(b, uri).map(|t| t.url()).ok_or(ParseError::BadUri { line })
    };
    let lines: Vec<(usize, &str)> = lines.collect();
    if lines.iter().any(|(_, l)| l.starts_with("#EXT-X-STREAM-INF")) {
        parse_master(&lines, &resolve_at).map(Playlist::Master)
    } else {
        parse_media(&lines, &resolve_at).map(Playlist::Media)
    }
}

type Resolver<'a> = dyn Fn(&str, usize) -> Result<String, ParseError> + 'a;

fn tag_value<'a>(line: &'a str, tag: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(tag)?;
    if rest.is_empty() {
        return Some("");
    }
    rest.strip_prefix(':')
}

fn parse_master(lines: &[(usize, &str)], resolve: &Resolver<'_>) -> Result<MasterPlaylist, ParseError> {
    let mut m = MasterPlaylist::default();
    let mut pending: Option<(usize, Variant)> = None;
    for &(n, line) in lines {
        if line.is_empty() {
            continue;
        }
        if let Some(v) = tag_value(line, "#EXT-X-STREAM-INF") {
            if let Some((at, _)) = pending {
                return Err(ParseError::MissingUri { line: at });
            }
            let a = attributes(v).ok_or(ParseError::BadTag { line: n })?;
            let num = |name: &str| -> Result<Option<u64>, ParseError> {
                attr(&a, name).map(|v| v.text().parse().map_err(|_| ParseError::BadTag { line: n })).transpose()
            };
            let variant = Variant {
                uri: String::new(),
                bandwidth: num("BANDWIDTH")?.unwrap_or(0),
                average_bandwidth: num("AVERAGE-BANDWIDTH")?,
                codecs: attr(&a, "CODECS")
                    .map(|c| c.text().split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect())
                    .unwrap_or_default(),
                resolution: match attr(&a, "RESOLUTION") {
                    Some(r) => Some(resolution(r.text()).ok_or(ParseError::BadTag { line: n })?),
                    None => None,
                },
                frame_rate: match attr(&a, "FRAME-RATE") {
                    Some(r) => Some(r.text().parse::<f64>().ok().filter(|f| f.is_finite() && *f >= 0.0).ok_or(ParseError::BadTag { line: n })?),
                    None => None,
                },
                audio_group: attr(&a, "AUDIO").map(|g| g.text().to_owned()),
            };
            pending = Some((n, variant));
        } else if let Some(v) = tag_value(line, "#EXT-X-MEDIA") {
            let a = attributes(v).ok_or(ParseError::BadTag { line: n })?;
            let kind = match attr(&a, "TYPE").map(|t| t.text().to_ascii_uppercase()).as_deref() {
                Some("AUDIO") => MediaType::Audio,
                Some("VIDEO") => MediaType::Video,
                Some("SUBTITLES") => MediaType::Subtitles,
                Some("CLOSED-CAPTIONS") => MediaType::ClosedCaptions,
                _ => return Err(ParseError::BadTag { line: n }),
            };
            m.renditions.push(Rendition {
                kind,
                group_id: attr(&a, "GROUP-ID").map(|g| g.text().to_owned()).ok_or(ParseError::BadTag { line: n })?,
                name: attr(&a, "NAME").map(|g| g.text().to_owned()).unwrap_or_default(),
                language: attr(&a, "LANGUAGE").map(|g| g.text().to_owned()),
                uri: attr(&a, "URI").map(|u| resolve(u.text(), n)).transpose()?,
                default: attr(&a, "DEFAULT").is_some_and(|d| d.text().eq_ignore_ascii_case("YES")),
            });
        } else if line.starts_with('#') {
            // Other tags (I-frame variants, session data and keys, `EXT-X-INDEPENDENT-SEGMENTS`)
            // and comments.
        } else if let Some((_, mut variant)) = pending.take() {
            variant.uri = resolve(line, n)?;
            m.variants.push(variant);
        }
        // A URI line with no `EXT-X-STREAM-INF` before it belongs to nothing; skipped.
    }
    if let Some((at, _)) = pending {
        return Err(ParseError::MissingUri { line: at });
    }
    Ok(m)
}

/// The key a tag declares, and whether the tag is in the `identity` key format (a DRM system's
/// tag is not; `METHOD=NONE` is).
fn key_of(v: &str, n: usize, resolve: &Resolver<'_>) -> Result<(Encryption, bool), ParseError> {
    let a = attributes(v).ok_or(ParseError::BadTag { line: n })?;
    let method = attr(&a, "METHOD").map(|m| m.text().to_ascii_uppercase()).ok_or(ParseError::BadTag { line: n })?;
    if method == "NONE" {
        return Ok((Encryption::Clear, true));
    }
    let format = attr(&a, "KEYFORMAT").map(|f| f.text()).unwrap_or("identity");
    let identity = format.eq_ignore_ascii_case("identity");
    if method != "AES-128" {
        return Ok((Encryption::Unsupported(format!("METHOD={method}")), identity));
    }
    if !identity {
        return Ok((Encryption::Unsupported(format!("KEYFORMAT={format}")), false));
    }
    let uri = attr(&a, "URI").ok_or(ParseError::MissingUri { line: n })?;
    let iv = match attr(&a, "IV") {
        Some(iv) => Some(hex_iv(iv.text()).ok_or(ParseError::BadTag { line: n })?),
        None => None,
    };
    Ok((Encryption::Aes128 { uri: resolve(uri.text(), n)?, iv }, true))
}

fn parse_media(lines: &[(usize, &str)], resolve: &Resolver<'_>) -> Result<MediaPlaylist, ParseError> {
    let mut target: Option<u64> = None;
    let mut media_sequence = 0u64;
    let mut discontinuity_sequence = 0u64;
    let mut endlist = false;
    let mut playlist_type = None;
    let mut segments: Vec<Segment> = Vec::new();

    // State that applies to the next segment only.
    let mut duration: Option<u64> = None;
    let mut range: Option<(usize, u64, Option<u64>)> = None;
    let mut discontinuity = false;
    let mut gap = false;
    // State that applies until changed.
    let mut key = Encryption::Clear;
    // Several `EXT-X-KEY` tags in a row may describe the SAME segments for different key systems
    // (an identity key beside a DRM one); the identity one wins whatever the order. So a DRM
    // system's tag does not displace an identity tag of its own group (the key tags since the last
    // segment); every other key tag replaces the key in force.
    let mut key_group_open = false;
    let mut key_is_identity = true;
    let mut map: Option<InitSection> = None;
    // The resource and end of the last sub-range, for a range with no offset.
    let mut last_range: Option<(String, u64)> = None;
    let mut disc_seq_now: Option<u64> = None;

    for &(n, line) in lines {
        if line.is_empty() {
            continue;
        }
        if !line.starts_with('#') {
            let Some(d) = duration.take() else {
                return Err(ParseError::MissingDuration { line: n });
            };
            let uri = resolve(line, n)?;
            let byte_range = match range.take() {
                Some((_, len, Some(off))) => Some(ByteRange { length: len, offset: off }),
                Some((at, len, None)) => match &last_range {
                    Some((u, end)) if *u == uri => Some(ByteRange { length: len, offset: *end }),
                    _ => return Err(ParseError::ByteRangeWithoutBase { line: at }),
                },
                None => None,
            };
            last_range = byte_range.map(|r| (uri.clone(), r.offset + r.length));
            let dseq = disc_seq_now.unwrap_or(discontinuity_sequence) + u64::from(discontinuity);
            disc_seq_now = Some(dseq);
            segments.push(Segment {
                sequence: media_sequence + segments.len() as u64,
                uri,
                duration_us: d,
                range: byte_range,
                discontinuity,
                discontinuity_sequence: dseq,
                key: key.clone(),
                map: map.clone(),
                gap,
            });
            discontinuity = false;
            gap = false;
            key_group_open = false;
            continue;
        }
        if let Some(v) = tag_value(line, "#EXTINF") {
            let d = v.split(',').next().unwrap_or("");
            duration = Some(seconds_us(d).ok_or(ParseError::BadTag { line: n })?);
        } else if let Some(v) = tag_value(line, "#EXT-X-TARGETDURATION") {
            target = Some(seconds_us(v).ok_or(ParseError::BadTag { line: n })?);
        } else if let Some(v) = tag_value(line, "#EXT-X-MEDIA-SEQUENCE") {
            media_sequence = v.trim().parse().map_err(|_| ParseError::BadTag { line: n })?;
        } else if let Some(v) = tag_value(line, "#EXT-X-DISCONTINUITY-SEQUENCE") {
            discontinuity_sequence = v.trim().parse().map_err(|_| ParseError::BadTag { line: n })?;
        } else if tag_value(line, "#EXT-X-DISCONTINUITY").is_some() {
            discontinuity = true;
        } else if tag_value(line, "#EXT-X-ENDLIST").is_some() {
            endlist = true;
        } else if tag_value(line, "#EXT-X-GAP").is_some() {
            gap = true;
        } else if let Some(v) = tag_value(line, "#EXT-X-PLAYLIST-TYPE") {
            playlist_type = match v.trim().to_ascii_uppercase().as_str() {
                "VOD" => Some(PlaylistType::Vod),
                "EVENT" => Some(PlaylistType::Event),
                _ => return Err(ParseError::BadTag { line: n }),
            };
        } else if let Some(v) = tag_value(line, "#EXT-X-BYTERANGE") {
            let (len, off) = byte_range_parts(v).ok_or(ParseError::BadTag { line: n })?;
            range = Some((n, len, off));
        } else if let Some(v) = tag_value(line, "#EXT-X-KEY") {
            let (k, identity) = key_of(v, n, resolve)?;
            if identity || !key_group_open || !key_is_identity {
                key = k;
                key_is_identity = identity;
            }
            key_group_open = true;
        } else if let Some(v) = tag_value(line, "#EXT-X-MAP") {
            let a = attributes(v).ok_or(ParseError::BadTag { line: n })?;
            let uri = attr(&a, "URI").ok_or(ParseError::MissingUri { line: n })?;
            let range = match attr(&a, "BYTERANGE") {
                Some(r) => {
                    let (len, off) = byte_range_parts(r.text()).ok_or(ParseError::BadTag { line: n })?;
                    Some(ByteRange { length: len, offset: off.unwrap_or(0) })
                }
                None => None,
            };
            map = Some(InitSection { uri: resolve(uri.text(), n)?, range, key: key.clone() });
        }
        // Every other tag, and comments, are skipped (RFC 8216 §6.3.1).
    }
    let longest = segments.iter().map(|s| s.duration_us).max().unwrap_or(0);
    let target_duration_us = target.unwrap_or_else(|| longest.div_ceil(1_000_000).max(1) * 1_000_000);
    Ok(MediaPlaylist {
        target_duration_us: target_duration_us.max(1),
        media_sequence,
        discontinuity_sequence,
        endlist,
        playlist_type,
        segments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://cdn.example:443/live/chan/index.m3u8?token=abc";

    fn media(text: &str) -> MediaPlaylist {
        match parse(text, BASE).unwrap() {
            Playlist::Media(m) => m,
            other => panic!("not a media playlist: {other:?}"),
        }
    }

    #[test]
    fn a_master_with_several_variants_and_renditions() {
        let text = "#EXTM3U\n\
            #EXT-X-VERSION:4\n\
            #EXT-X-INDEPENDENT-SEGMENTS\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"audio/en.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=800000,AVERAGE-BANDWIDTH=700000,CODECS=\"avc1.4d401f,mp4a.40.2\",RESOLUTION=640x360,FRAME-RATE=25.000\n\
            low/index.m3u8\n\
            \n\
            # a comment between a tag and its URI is not a URI\n\
            #EXT-X-STREAM-INF:BANDWIDTH=5000000,CODECS=\"hvc1.2.4.L150.B0,ec-3\",RESOLUTION=3840x2160,AUDIO=\"aud\"\n\
            /abs/4k.m3u8\n\
            #EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=100000,URI=\"iframe.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720\n\
            https://other.example:443/hd.m3u8\n";
        let Playlist::Master(m) = parse(text, BASE).unwrap() else { panic!("master") };
        assert_eq!(m.variants.len(), 3, "the I-frame playlist is not a variant");
        let v = &m.variants[0];
        assert_eq!(v.uri, "https://cdn.example:443/live/chan/low/index.m3u8", "relative, no query inherited");
        assert_eq!((v.bandwidth, v.average_bandwidth), (800_000, Some(700_000)));
        assert_eq!(v.codecs, ["avc1.4d401f", "mp4a.40.2"], "a quoted list keeps its commas");
        assert_eq!(v.resolution, Some((640, 360)));
        assert_eq!(v.frame_rate, Some(25.0));
        assert_eq!(m.variants[1].uri, "https://cdn.example:443/abs/4k.m3u8");
        assert_eq!(m.variants[1].audio_group.as_deref(), Some("aud"));
        assert_eq!(m.variants[2].uri, "https://other.example:443/hd.m3u8");
        assert!(m.variants[2].codecs.is_empty());
        assert_eq!(m.renditions.len(), 1);
        let r = &m.renditions[0];
        assert_eq!((r.kind, r.group_id.as_str(), r.default), (MediaType::Audio, "aud", true));
        assert_eq!(r.uri.as_deref(), Some("https://cdn.example:443/live/chan/audio/en.m3u8"));
    }

    #[test]
    fn a_master_variant_without_its_uri_is_refused() {
        let text = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n";
        assert_eq!(parse(text, BASE), Err(ParseError::MissingUri { line: 2 }));
        let text = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n#EXT-X-STREAM-INF:BANDWIDTH=2\nb.m3u8\n";
        assert_eq!(parse(text, BASE), Err(ParseError::MissingUri { line: 2 }));
    }

    #[test]
    fn a_live_media_playlist() {
        let m = media(
            "#EXTM3U\r\n#EXT-X-VERSION:3\r\n#EXT-X-TARGETDURATION:6\r\n#EXT-X-MEDIA-SEQUENCE:1043\r\n\
             #EXT-X-PROGRAM-DATE-TIME:2026-10-16T10:00:00Z\r\n#EXTINF:6.006,\r\nseg1043.ts\r\n\
             #EXTINF:5.994,title text, with a comma\r\nseg1044.ts\r\n#EXTINF:6,\r\n../other/seg1045.ts?x=1\r\n",
        );
        assert!(m.is_live());
        assert_eq!(m.target_duration_us, 6_000_000);
        assert_eq!(m.media_sequence, 1043);
        assert_eq!(m.segments.iter().map(|s| s.sequence).collect::<Vec<_>>(), [1043, 1044, 1045]);
        assert_eq!(m.segments.iter().map(|s| s.duration_us).collect::<Vec<_>>(), [6_006_000, 5_994_000, 6_000_000]);
        assert_eq!(m.duration_us(), 18_000_000, "exact, in microseconds");
        assert_eq!(m.segments[0].uri, "https://cdn.example:443/live/chan/seg1043.ts");
        assert_eq!(m.segments[2].uri, "https://cdn.example:443/live/other/seg1045.ts?x=1");
        assert_eq!(m.end_sequence(), 1046);
        assert_eq!(m.by_sequence(1044).map(|s| s.duration_us), Some(5_994_000));
        assert_eq!(m.by_sequence(1042), None);
        assert_eq!(m.by_sequence(1046), None);
    }

    #[test]
    fn vod_endlist_and_playlist_type() {
        let m = media("#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-TARGETDURATION:10\n#EXTINF:10,\na.ts\n#EXTINF:4.5,\nb.ts\n#EXT-X-ENDLIST\n");
        assert!(!m.is_live());
        assert!(m.endlist);
        assert_eq!(m.playlist_type, Some(PlaylistType::Vod));
        assert_eq!(m.media_sequence, 0);
        let event = media("#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-TARGETDURATION:10\n#EXTINF:10,\na.ts\n");
        assert!(event.is_live(), "an EVENT playlist grows until it ends");
        let vod_without_endlist = media("#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-TARGETDURATION:10\n#EXTINF:10,\na.ts\n");
        assert!(!vod_without_endlist.is_live());
    }

    #[test]
    fn a_missing_target_duration_is_the_longest_segment_rounded_up() {
        let m = media("#EXTM3U\n#EXTINF:4.2,\na.ts\n#EXTINF:6.1,\nb.ts\n");
        assert_eq!(m.target_duration_us, 7_000_000);
        assert_eq!(media("#EXTM3U\n").target_duration_us, 1_000_000);
    }

    #[test]
    fn absolute_and_protocol_relative_uris() {
        let m = media("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\nhttp://plain.example:8080/s.ts\n#EXTINF:2,\n//edge.example/s2.ts\n#EXTINF:2,\n/root.ts\n#EXTINF:2,\n?q=only\n");
        assert_eq!(m.segments[0].uri, "http://plain.example:8080/s.ts");
        assert_eq!(m.segments[1].uri, "https://edge.example:443/s2.ts", "the playlist's scheme");
        assert_eq!(m.segments[2].uri, "https://cdn.example:443/root.ts");
        assert_eq!(m.segments[3].uri, "https://cdn.example:443/live/chan/index.m3u8?q=only");
        assert_eq!(
            parse("#EXTM3U\n#EXTINF:2,\nftp://x/s.ts\n", BASE),
            Err(ParseError::BadUri { line: 3 }),
            "another scheme is not requestable"
        );
        assert_eq!(resolve(BASE, "k/key.bin").as_deref(), Some("https://cdn.example:443/live/chan/k/key.bin"));
    }

    #[test]
    fn byte_ranges_follow_on_within_one_resource() {
        let m = media(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"main.mp4\",BYTERANGE=\"720@0\"\n\
             #EXTINF:4,\n#EXT-X-BYTERANGE:1000@720\nmain.mp4\n#EXTINF:4,\n#EXT-X-BYTERANGE:2000\nmain.mp4\n\
             #EXTINF:4,\n#EXT-X-BYTERANGE:500\nmain.mp4\n",
        );
        let ranges: Vec<_> = m.segments.iter().map(|s| s.range.unwrap()).collect();
        assert_eq!(
            ranges,
            [ByteRange { length: 1000, offset: 720 }, ByteRange { length: 2000, offset: 1720 }, ByteRange { length: 500, offset: 3720 }]
        );
        let map = m.segments[0].map.as_ref().unwrap();
        assert_eq!(map.uri, "https://cdn.example:443/live/chan/main.mp4");
        assert_eq!(map.range, Some(ByteRange { length: 720, offset: 0 }));
        assert_eq!(
            parse("#EXTM3U\n#EXTINF:4,\n#EXT-X-BYTERANGE:10\na.ts\n", BASE),
            Err(ParseError::ByteRangeWithoutBase { line: 3 })
        );
        assert_eq!(
            parse("#EXTM3U\n#EXTINF:4,\n#EXT-X-BYTERANGE:10@0\na.ts\n#EXTINF:4,\n#EXT-X-BYTERANGE:10\nb.ts\n", BASE),
            Err(ParseError::ByteRangeWithoutBase { line: 6 }),
            "no offset after another resource's range"
        );
    }

    #[test]
    fn discontinuities_are_counted() {
        let m = media(
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:10\n#EXT-X-DISCONTINUITY-SEQUENCE:3\n\
             #EXTINF:6,\na.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:6,\nad1.ts\n#EXTINF:6,\nad2.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:6,\nb.ts\n",
        );
        assert_eq!(m.segments.iter().map(|s| s.discontinuity).collect::<Vec<_>>(), [false, true, false, true]);
        assert_eq!(m.segments.iter().map(|s| s.discontinuity_sequence).collect::<Vec<_>>(), [3, 4, 4, 5]);
    }

    #[test]
    fn keys_apply_until_changed_and_identity_wins_a_group() {
        let m = media(
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:7\n\
             #EXTINF:6,\nclear.ts\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"keys/k1.bin\",IV=0x000102030405060708090A0B0C0D0E0F\n\
             #EXTINF:6,\ne1.ts\n#EXTINF:6,\ne2.ts\n\
             #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://drm\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.example/k2\"\n\
             #EXTINF:6,\ne3.ts\n\
             #EXT-X-KEY:METHOD=NONE\n#EXTINF:6,\nclear2.ts\n\
             #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"k3\"\n#EXTINF:6,\nsample.ts\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"k4\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\"\n#EXTINF:6,\nwv.ts\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"k5\",IV=0x1F\n#EXTINF:6,\nshort-iv.ts\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"k6\"\n\
             #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n\
             #EXTINF:6,\nidentity-first.ts\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"k7\"\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXT-X-KEY:METHOD=NONE\n\
             #EXTINF:6,\nnone-after-map.ts\n",
        );
        let keys: Vec<&Encryption> = m.segments.iter().map(|s| &s.key).collect();
        assert_eq!(keys[0], &Encryption::Clear);
        let mut iv = [0u8; 16];
        for (i, b) in iv.iter_mut().enumerate() {
            *b = i as u8;
        }
        let k1 = Encryption::Aes128 { uri: "https://cdn.example:443/live/chan/keys/k1.bin".into(), iv: Some(iv) };
        assert_eq!(keys[1], &k1);
        assert_eq!(keys[2], &k1, "a key applies until the next key tag");
        assert_eq!(
            keys[3],
            &Encryption::Aes128 { uri: "https://keys.example:443/k2".into(), iv: None },
            "the identity key of a group wins over the DRM one before it"
        );
        assert_eq!(keys[4], &Encryption::Clear);
        assert_eq!(keys[5], &Encryption::Unsupported("METHOD=SAMPLE-AES".into()));
        assert!(matches!(keys[6], Encryption::Unsupported(f) if f.starts_with("KEYFORMAT=")));
        let mut short = [0u8; 16];
        short[15] = 0x1f;
        assert!(matches!(keys[7], Encryption::Aes128 { iv: Some(v), .. } if *v == short), "a short IV is right-aligned");
        assert!(
            matches!(keys[8], Encryption::Aes128 { uri, .. } if uri.ends_with("/k6")),
            "a DRM tag after the identity one of its group does not displace it"
        );
        assert_eq!(keys[9], &Encryption::Clear, "METHOD=NONE replaces the key in force");
        assert_eq!(
            parse("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128\n#EXTINF:1,\na.ts\n", BASE),
            Err(ParseError::MissingUri { line: 2 })
        );
        assert_eq!(
            parse("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\",IV=0xZZ\n#EXTINF:1,\na.ts\n", BASE),
            Err(ParseError::BadTag { line: 2 })
        );
    }

    #[test]
    fn an_fmp4_map_takes_the_key_in_force_and_changes_with_a_new_tag() {
        let m = media(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\",IV=0x01\n#EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:4,\nseg1.m4s\n#EXT-X-DISCONTINUITY\n#EXT-X-KEY:METHOD=NONE\n#EXT-X-MAP:URI=\"init2.mp4\"\n#EXTINF:4,\nseg2.m4s\n",
        );
        let m0 = m.segments[0].map.as_ref().unwrap();
        assert_eq!(m0.uri, "https://cdn.example:443/live/chan/init.mp4");
        assert!(matches!(m0.key, Encryption::Aes128 { .. }));
        let m1 = m.segments[1].map.as_ref().unwrap();
        assert_eq!((m1.uri.as_str(), &m1.key), ("https://cdn.example:443/live/chan/init2.mp4", &Encryption::Clear));
    }

    #[test]
    fn gaps_and_malformed_input() {
        let m = media("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-GAP\n#EXTINF:2,\nmissing.ts\n#EXTINF:2,\nok.ts\n");
        assert_eq!(m.segments.iter().map(|s| s.gap).collect::<Vec<_>>(), [true, false]);
        assert_eq!(parse("<html>404</html>", BASE), Err(ParseError::NotAPlaylist));
        assert_eq!(parse("", BASE), Err(ParseError::NotAPlaylist));
        assert!(parse("\u{feff}#EXTM3U\n#EXT-X-TARGETDURATION:2\n", BASE).is_ok(), "a BOM is tolerated");
        assert_eq!(parse("#EXTM3U\nseg.ts\n", BASE), Err(ParseError::MissingDuration { line: 2 }));
        assert_eq!(parse("#EXTM3U\n#EXTINF:abc,\nseg.ts\n", BASE), Err(ParseError::BadTag { line: 2 }));
        assert_eq!(parse("#EXTM3U\n#EXT-X-TARGETDURATION:-1\n", BASE), Err(ParseError::BadTag { line: 2 }));
        assert_eq!(parse("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=x\na.m3u8\n", BASE), Err(ParseError::BadTag { line: 2 }));
        assert_eq!(parse("#EXTM3U\n#EXT-X-STREAM-INF:CODECS=\"unterminated\na.m3u8\n", BASE), Err(ParseError::BadTag { line: 2 }));
    }

    #[test]
    fn attribute_lists() {
        let a = attributes("A=1,B=\"x,y\",C=0x1F, D = plain ").unwrap();
        assert_eq!(attr(&a, "A"), Some(AttrValue::Plain("1")));
        assert_eq!(attr(&a, "B"), Some(AttrValue::Quoted("x,y")));
        assert_eq!(attr(&a, "c").map(AttrValue::text), Some("0x1F"));
        assert_eq!(attr(&a, "D").map(AttrValue::text), Some("plain"));
        assert!(attributes("A=\"x\"B=1").is_none(), "a quoted value must end its attribute");
        assert!(attributes("=1").is_none());
    }
}
