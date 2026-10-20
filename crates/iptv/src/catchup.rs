//! Catch-up (archive) URLs: the modes and format specifiers of Kodi's pvr.iptvsimple, which most
//! providers write playlists for.
//!
//! Ported from `src/utils/catchup-url.ts` of this project's catch-up patch for webOS IPTV Player,
//! written for this project and offered upstream as lennylxx/webos-iptv-player#87. Only the
//! `{channel-id}` substitution follows webOS IPTV Player's own `src/components/player.ts`
//! (Apache-2.0; see THIRD-PARTY-NOTICES.md). The M3U parser derives a channel's template once
//! ([`template`]); [`archive_url`] fills it for one programme. Nothing here logs: templates and
//! filled URLs carry stream tokens.

use std::fmt::Write as _;

use crate::model::{CatchupMode, Channel, Programme, Timestamp};
use crate::url::split_url;

/// SIPTV's shift query, which Flussonic answers with a fixed 3 h window from `utc`.
const SHIFT_QUERY: &str = "utc={utc}&lutc={lutc}";

/// Longest specifier name (`timestamp`); a longer name before `:` or `}` is not a specifier.
const MAX_NAME: usize = 9;

/// The times one archive request is for, Unix seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Times {
    pub start: Timestamp,
    pub end: Timestamp,
    /// Request time.
    pub now: Timestamp,
}

/// `url` plus `query`, joined with `?` or `&` as the URL needs; a fragment stays at the end.
pub fn append_query(url: &str, query: &str) -> String {
    let query = query.trim_start_matches(['?', '&']);
    if query.is_empty() {
        return url.to_owned();
    }
    let (base, fragment) = split_fragment(url);
    let joiner = if !base.contains('?') {
        "?"
    } else if base.ends_with(['?', '&']) {
        ""
    } else {
        "&"
    };
    format!("{base}{joiner}{query}{fragment}")
}

/// Flussonic's fixed-window archive URL for a live Flussonic URL, or `None` for another shape:
/// `.../<name>.m3u8?q` becomes `.../<name>-{utc}-{duration}.m3u8?q` and `.../mpegts?q` becomes
/// `.../archive-{utc}-{duration}.ts?q`. The window spans exactly the programme and ends with
/// `#EXT-X-ENDLIST`, unlike Kodi's sliding `timeshift_rel`, so the player seeks inside a finite
/// playlist.
pub fn flussonic_source(url: &str) -> Option<String> {
    let (rest, fragment) = split_fragment(url);
    let (path, query) = rest.find('?').map_or((rest, ""), |i| rest.split_at(i));
    let (scheme, authority, _) = split_url(path)?;
    let slash = path
        .rfind('/')
        .filter(|&i| i >= scheme.len() + 3 + authority.len())?;
    let (dir, file) = path.split_at(slash + 1);
    if let Some(name) = strip_suffix_ignore_case(file, ".m3u8").filter(|n| !n.is_empty()) {
        Some(format!(
            "{dir}{name}-{{utc}}-{{duration}}.m3u8{query}{fragment}"
        ))
    } else if file.eq_ignore_ascii_case("mpegts") {
        Some(format!(
            "{dir}archive-{{utc}}-{{duration}}.ts{query}{fragment}"
        ))
    } else {
        None
    }
}

/// The archive template for a channel's mode, or `None` when the mode cannot form one from what
/// the playlist declared. `default` (and `xc`, `vod`, no mode) use the declared source as the
/// full URL, `append` appends it to the stream URL, `shift` appends SIPTV's `utc`/`lutc` query
/// and `flussonic` derives the fixed window; a declared source wins over a derived one.
pub fn template(mode: Option<CatchupMode>, url: &str, declared: Option<&str>) -> Option<String> {
    let declared = declared.filter(|s| !s.is_empty());
    match mode {
        Some(CatchupMode::Append) => declared.map(|s| append_query(url, s)),
        Some(CatchupMode::Shift) => {
            Some(declared.map_or_else(|| append_query(url, SHIFT_QUERY), str::to_owned))
        }
        Some(CatchupMode::Flussonic) => declared
            .map(str::to_owned)
            .or_else(|| flussonic_source(url)),
        _ => declared.map(str::to_owned),
    }
}

/// Fills every occurrence of Kodi's format specifiers in one pass; anything else stays as is.
///
/// - `{utc}` `${start}`: start; `{utcend}` `${end}`: end; `{lutc}` `${now}` `${timestamp}`:
///   request time, as Unix seconds. Each also takes a date format, `{utc:Y-m-d}`, whose letters
///   `YmdHMS` become that time's local date parts.
/// - `{duration}` `${duration}`, `{duration:N}`: programme length in seconds (divided by N).
/// - `${offset}`, `{offset:N}`: seconds since the start (divided by N).
/// - `{Y}` `{m}` `{d}` `{H}` `{M}` `{S}`: the start's local date parts.
///
/// Local time is UTC plus `offset_at(t)` seconds for each instant `t`: Kodi formats dates in the
/// device's time zone, and a daylight-saving change can fall between start and end.
pub fn fill(template: &str, times: Times, offset_at: impl Fn(Timestamp) -> i32) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut copied = 0;
    let mut close = 0;
    let mut at = 0;
    while let Some(found) = template[at..].find('{') {
        let open = at + found;
        // The first `}` after `open`; cached so a run of `{` stays linear.
        if close <= open {
            match template[open..].find('}') {
                Some(i) => close = open + i,
                None => break,
            }
        }
        let dollar = open > 0 && template.as_bytes()[open - 1] == b'$';
        match specifier(&template[open + 1..close], dollar, times, &offset_at) {
            Some((value, takes_dollar)) => {
                out.push_str(&template[copied..open - usize::from(takes_dollar)]);
                out.push_str(&value);
                copied = close + 1;
                at = close + 1;
            }
            None => at = open + 1,
        }
    }
    out.push_str(&template[copied..]);
    out
}

/// The value of the specifier whose text between the braces is `body`, and whether it includes
/// the `$` before it.
fn specifier(
    body: &str,
    dollar: bool,
    t: Times,
    offset_at: &dyn Fn(Timestamp) -> i32,
) -> Option<(String, bool)> {
    let (name, format) = match body.bytes().take(MAX_NAME + 1).position(|b| b == b':') {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None if body.len() <= MAX_NAME => (body, None),
        None => return None,
    };
    let duration = t.end.saturating_sub(t.start).max(0);
    let offset = t.now.saturating_sub(t.start).max(0);
    let epoch = match (dollar, name) {
        (false, "utc") | (true, "start") => Some(t.start),
        (false, "utcend") | (true, "end") => Some(t.end),
        (false, "lutc") | (true, "now" | "timestamp") => Some(t.now),
        _ => None,
    };
    if let Some(epoch) = epoch {
        let value = match format {
            None => epoch.to_string(),
            Some(format) => {
                let local = Local::new(epoch, offset_at(epoch));
                let mut s = String::new();
                for c in format.chars() {
                    if !local.push(c, &mut s) {
                        s.push(c);
                    }
                }
                s
            }
        };
        return Some((value, dollar));
    }
    match (name, format) {
        ("duration", None) => Some((duration.to_string(), dollar)),
        ("offset", None) if dollar => Some((offset.to_string(), true)),
        ("duration" | "offset", Some(divider)) => {
            let seconds = if name == "duration" { duration } else { offset };
            Some(((seconds / divisor(divider)?).to_string(), false))
        }
        (_, None) if name.len() == 1 => {
            let mut s = String::new();
            let c = name.chars().next()?;
            Local::new(t.start, offset_at(t.start))
                .push(c, &mut s)
                .then_some((s, false))
        }
        _ => None,
    }
}

/// `N` of `{duration:N}`: one or more digits, zero read as one.
fn divisor(digits: &str) -> Option<i64> {
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = leading_int(digits)?;
    Some(i64::try_from(n).unwrap_or(i64::MAX).max(1))
}

/// The leading decimal integer of `s` as JavaScript's `parseInt` reads it, saturating; `None`
/// without digits or when negative.
pub(crate) fn leading_int(s: &str) -> Option<u64> {
    let s = s.trim_start();
    let s = s.strip_prefix('+').unwrap_or(s);
    let len = s.bytes().take_while(u8::is_ascii_digit).count();
    (len > 0).then(|| {
        s[..len].bytes().fold(0u64, |n, d| {
            n.saturating_mul(10).saturating_add(u64::from(d - b'0'))
        })
    })
}

/// The archive URL for a programme on `channel`, or `None` without a catch-up template.
///
/// A shift template the parser derived (no declared `catchup-source`) on a Flussonic live
/// playlist (`.../mono.m3u8`, `.../index.m3u8`) asks for the fixed window instead once the
/// programme has ended: Flussonic answers shift with a 3 h window from the start, the window
/// with exactly the programme. `offset_at` is the device's UTC offset at an instant, as for
/// [`fill`].
///
/// `times` are the programme's; the URL asks for its start and end minus the channel's
/// `catchup-correction`, with the request time as is (Kodi's `FormatDateTime`). Whether the
/// programme has ended is decided on the programme's times, as Kodi does.
pub fn archive_url(
    channel: &Channel,
    times: Times,
    offset_at: impl Fn(Timestamp) -> i32,
) -> Option<String> {
    let source = channel.catchup.source.as_deref()?;
    let window = window_template(channel, times);
    let correction = i64::from(channel.catchup.correction_secs);
    let requested = Times {
        start: times.start.saturating_sub(correction),
        end: times.end.saturating_sub(correction),
        ..times
    };
    let url = fill(window.as_deref().unwrap_or(source), requested, offset_at);
    if !url.contains("{channel-id}") {
        return Some(url);
    }
    let id = channel.tvg_id.as_deref().unwrap_or(&channel.name);
    Some(url.replace("{channel-id}", &encode_component(id)))
}

/// Whether `programme` can be played from the channel's archive at `now`, the one test the
/// screens use: the channel offers catch-up and the programme started within the last `days`
/// days. It counts from the start, so the programme on air is included: the archive already
/// holds its beginning, and playing it from there is start-over. The `catchup-correction` does
/// not move this test (Kodi checks the archive days on the guide's times too).
pub fn in_archive(channel: &Channel, programme: &Programme, now: Timestamp) -> bool {
    let catchup = &channel.catchup;
    catchup.available()
        && programme.start < now
        && now.saturating_sub(programme.start) <= i64::from(catchup.days) * 86_400
}

fn window_template(channel: &Channel, times: Times) -> Option<String> {
    let catchup = &channel.catchup;
    let ended = times.start < times.end && times.end <= times.now;
    if catchup.mode != Some(CatchupMode::Shift) || !ended || !is_flussonic_live(&channel.url) {
        return None;
    }
    // A declared catchup-source is the provider's choice; only the derived shift is replaced.
    let derived = append_query(&channel.url, SHIFT_QUERY);
    if catchup.source.as_deref() != Some(derived.as_str()) {
        return None;
    }
    flussonic_source(&channel.url)
}

/// A live playlist name Flussonic serves (`mono.m3u8`, `index.m3u8`).
fn is_flussonic_live(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let file = path.rsplit('/').next().unwrap_or_default();
    file.eq_ignore_ascii_case("mono.m3u8") || file.eq_ignore_ascii_case("index.m3u8")
}

fn split_fragment(url: &str) -> (&str, &str) {
    url.find('#').map_or((url, ""), |i| url.split_at(i))
}

fn strip_suffix_ignore_case<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let split = s.len().checked_sub(suffix.len())?;
    let (head, tail) = (s.get(..split)?, s.get(split..)?);
    tail.eq_ignore_ascii_case(suffix).then_some(head)
}

/// JavaScript's `encodeURIComponent`: everything but `A-Z a-z 0-9 - _ . ! ~ * ' ( )` as `%XX`.
fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Local date parts `[Y, m, d, H, M, S]` of a Unix time.
struct Local([i64; 6]);

impl Local {
    fn new(epoch: Timestamp, utc_offset_secs: i32) -> Self {
        let secs = epoch.saturating_add(i64::from(utc_offset_secs));
        let (year, month, day) = tv_base::time::civil_from_days(secs.div_euclid(86_400));
        let t = secs.rem_euclid(86_400);
        Local([
            year,
            month.into(),
            day.into(),
            t / 3_600,
            t / 60 % 60,
            t % 60,
        ])
    }

    /// Appends the part `letter` names (`YmdHMS`); false for any other character.
    fn push(&self, letter: char, out: &mut String) -> bool {
        let index = match letter {
            'Y' => 0,
            'm' => 1,
            'd' => 2,
            'H' => 3,
            'M' => 4,
            'S' => 5,
            _ => return false,
        };
        let _ = if index == 0 {
            write!(out, "{}", self.0[0])
        } else {
            write!(out, "{:02}", self.0[index])
        };
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_parts_of_known_times() {
        assert_eq!(Local::new(0, 0).0, [1970, 1, 1, 0, 0, 0]);
        assert_eq!(Local::new(1_700_000_000, 0).0, [2023, 11, 14, 22, 13, 20]);
        assert_eq!(Local::new(951_782_400, 3_600).0, [2000, 2, 29, 1, 0, 0]);
        assert_eq!(Local::new(-1, 0).0, [1969, 12, 31, 23, 59, 59]);
    }

    #[test]
    fn extreme_times_do_not_panic() {
        for (start, end) in [(i64::MIN, i64::MAX), (i64::MAX, i64::MIN), (0, 0)] {
            let times = Times {
                start,
                end,
                now: end,
            };
            for offset in [i32::MIN, 0, i32::MAX] {
                let _ = fill(
                    "{utc:YmdHMS}{Y}{duration:1}${offset}{utcend:S}",
                    times,
                    |_| offset,
                );
            }
        }
    }

    #[test]
    fn unclosed_and_nested_braces_stay_literal() {
        let t = Times {
            start: 0,
            end: 60,
            now: 120,
        };
        let utc = |_| 0;
        assert_eq!(fill("{utc", t, utc), "{utc");
        assert_eq!(fill("{{utc}", t, utc), "{0");
        assert_eq!(fill("{utc:{Y}}", t, utc), "{1970}");
        assert_eq!(fill("{timestampx}{utc:}", t, utc), "{timestampx}");
        let long = "{".repeat(100_000) + "}";
        assert_eq!(fill(&long, t, utc), long);
    }

    #[test]
    fn leading_int_reads_like_parse_int() {
        assert_eq!(leading_int(" 7 days"), Some(7));
        assert_eq!(leading_int("+3"), Some(3));
        assert_eq!(leading_int("-3"), None);
        assert_eq!(leading_int("x"), None);
        assert_eq!(leading_int("99999999999999999999999"), Some(u64::MAX));
        assert_eq!(divisor("99999999999999999999999"), Some(i64::MAX));
        assert_eq!(
            (divisor(""), divisor("1x"), divisor("0")),
            (None, None, Some(1))
        );
    }

    #[test]
    fn encodes_like_encode_uri_component() {
        assert_eq!(encode_component("a b/c?d&\u{e9}"), "a%20b%2Fc%3Fd%26%C3%A9");
        assert_eq!(encode_component("A-z_0.9!~*'()"), "A-z_0.9!~*'()");
    }
}
