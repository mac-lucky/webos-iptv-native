//! VOD: the movies and series episodes an M3U playlist lists among its live channels,
//! kept apart from them for TiviMate's Movies and Shows sections, and how far into each the
//! viewer got.
//!
//! An M3U playlist has no VOD section of its own, so every entry is classified ([`classify`]):
//!
//! - `tvg-type` decides when it names a kind (`movie`, `series`, `live`, ...);
//! - a radio entry is live;
//! - Xtream's VOD path (`/movie/<user>/<pass>/<id>.mkv`, `/series/...`) is VOD, even with the
//!   header's archive;
//! - any other entry with an archive is live;
//! - a path segment `movie`, `movies` or `series`, or a file extension a live stream never has
//!   (`.mp4`, `.mkv`, `.avi`, ...) is VOD, and so is an entry in a group named VOD (`VOD`,
//!   `PL | VOD Filmy`); an `S01 E02` name in a group named for series (`Seriale`, `TV Shows`) is
//!   VOD too. A group named for films alone is not: live film channels sit in such groups.
//!
//! A VOD entry is an episode when anything says series (the type, the path, the group, an
//! `S01 E02` in its name), else a movie. Nothing here does I/O or logs: URLs carry tokens.

use std::fmt;

use crate::model::{Channel, ChannelKey, Timestamp};

/// What a VOD entry is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum VodKind {
    #[default]
    Movie,
    Episode,
}

impl VodKind {
    /// The stored name, which [`VodKind::parse`] reads back.
    pub fn as_str(self) -> &'static str {
        match self {
            VodKind::Movie => "movie",
            VodKind::Episode => "episode",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "movie" => Some(VodKind::Movie),
            "episode" => Some(VodKind::Episode),
            _ => None,
        }
    }
}

/// One movie or episode. `Debug` masks the URLs and header values.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct VodItem {
    /// [`ChannelKey::of_url`] of its URL: a rotated token keeps the saved position.
    pub key: ChannelKey,
    pub kind: VodKind,
    /// A movie's name, or an episode's own title (empty when its name had none).
    pub title: String,
    /// The series an episode belongs to.
    pub series: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    /// A movie's year, from a `(2019)` closing its name.
    pub year: Option<u16>,
    /// The poster (`tvg-logo`).
    pub poster: Option<String>,
    /// The playlist groups it was listed in, its categories, as [`Channel::groups`] holds them:
    /// a parental lock on any of them covers it.
    pub groups: Vec<String>,
    pub url: String,
    /// Request headers the stream needs (`#EXTVLCOPT` / `#EXTHTTP`).
    pub http_headers: Vec<(String, String)>,
}

impl VodItem {
    /// The name lists show: a movie's title, an episode's `S01 E02 Title`.
    pub fn label(&self) -> String {
        match (self.kind, self.season, self.episode) {
            (VodKind::Episode, Some(s), Some(e)) if self.title.is_empty() => {
                format!("S{s:02} E{e:02}")
            }
            (VodKind::Episode, Some(s), Some(e)) => format!("S{s:02} E{e:02} {}", self.title),
            _ => self.title.clone(),
        }
    }

    /// The name a player shows: a movie's title, an episode's series and then its
    /// [`label`](Self::label). An episode whose name named no episode is its series alone, as its
    /// label is that name too.
    pub fn full_title(&self) -> String {
        let label = self.label();
        match &self.series {
            Some(series) if label.is_empty() || label == *series => series.clone(),
            Some(series) => format!("{series} {label}"),
            None => label,
        }
    }
}

impl fmt::Debug for VodItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VodItem")
            .field("key", &self.key)
            .field("kind", &self.kind)
            .field("title", &self.title)
            .field("series", &self.series)
            .field("season", &self.season)
            .field("episode", &self.episode)
            .field("year", &self.year)
            .field("poster", &self.poster.as_deref().map(crate::redact::url))
            .field("groups", &self.groups)
            .field("url", &crate::redact::url(&self.url))
            .field(
                "http_headers",
                &self
                    .http_headers
                    .iter()
                    .map(|(k, _)| (k.as_str(), crate::redact::MASK))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Under this far in, a stopped item starts over: no position is kept.
pub const RESUME_MIN_SECS: u32 = 30;
/// An item stopped within this much of its end (a tenth of it, for a short one) was watched.
pub const WATCHED_TAIL_SECS: u32 = 5 * 60;

/// Whether `position` seconds into an item `duration` seconds long counts as watched to the end.
pub fn watched(position: u32, duration: u32) -> bool {
    duration > 0 && position.saturating_add(WATCHED_TAIL_SECS.min(duration / 10)) >= duration
}

/// How far into an item the viewer got.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VodProgress {
    pub key: ChannelKey,
    pub position_secs: u32,
    /// 0 while the length is not known.
    pub duration_secs: u32,
    /// Played to within its last minutes ([`watched`]).
    pub watched: bool,
    pub updated_at: Timestamp,
}

impl VodProgress {
    /// `position_secs` into the item, with [`watched`] worked out.
    pub fn new(key: ChannelKey, position_secs: u32, duration_secs: u32, now: Timestamp) -> Self {
        VodProgress {
            key,
            position_secs,
            duration_secs,
            watched: watched(position_secs, duration_secs),
            updated_at: now,
        }
    }

    /// Where a reopened item offers to resume: the saved position, unless it was watched or
    /// barely started.
    pub fn resume_at(&self) -> Option<u32> {
        (!self.watched && self.position_secs >= RESUME_MIN_SECS).then_some(self.position_secs)
    }

    /// Storage keeps nothing for an item stopped in its first seconds and not watched.
    pub fn worth_keeping(&self) -> bool {
        self.watched || self.position_secs >= RESUME_MIN_SECS
    }
}

/// Extensions of files a live stream never ends in.
const FILE_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "m4v", "wmv", "mpg", "mpeg", "webm", "divx", "ogm", "3gp",
];

/// Group words that mean VOD whatever the URL.
const VOD_WORDS: &[&str] = &["vod"];
/// Group words that mean series.
const SERIES_WORDS: &[&str] = &[
    "series", "serie", "serial", "seriale", "seriali", "serials", "shows", "tvshows",
];

/// The kind of VOD `c` is, or `None` for a live channel (see the module doc).
pub fn classify(c: &Channel) -> Option<VodKind> {
    if let Some(kind) = declared(c) {
        return kind;
    }
    if c.radio {
        return None;
    }
    let path = url_path(&c.url).to_ascii_lowercase();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let named = |name: &str| segments.contains(&name);
    // Xtream's own VOD paths, `/movie/<user>/<pass>/<id>.<ext>` and `/series/...`: the only
    // path that outweighs an archive (the header's `catchup` reaches every entry). A live
    // stream merely named so (Flussonic's `/movies/index.m3u8`) keeps its archive.
    let xtream = segments.len() >= 4 && matches!(segments[0], "movie" | "series");
    let series_path = named("series");
    let movie_path = named("movie") || named("movies");
    let groups = Groups::of(c);
    let episode_name = episode_marker(&c.name).is_some();
    let series = series_path || groups.series || episode_name;
    let vod = xtream
        || (!c.catchup.available()
            && (series_path
                || movie_path
                || is_file(&path)
                || groups.vod
                || (groups.series && episode_name)));
    vod.then_some(if series {
        VodKind::Episode
    } else {
        VodKind::Movie
    })
}

/// What `tvg-type` says: `Some(None)` live, `Some(Some(kind))` VOD, `None` nothing.
fn declared(c: &Channel) -> Option<Option<VodKind>> {
    let (_, value) = c
        .extra
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("tvg-type"))?;
    match value.trim().to_ascii_lowercase().as_str() {
        "movie" | "movies" | "film" | "vod" => Some(Some(VodKind::Movie)),
        "series" | "serie" | "episode" | "show" | "tvshow" | "tv-show" | "tv_show" => {
            Some(Some(VodKind::Episode))
        }
        "live" | "channel" | "tv" | "radio" | "stream" => Some(None),
        _ => None,
    }
}

/// The path of `url`, without its scheme, authority, query or fragment.
fn url_path(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    rest.find('/').map_or("", |i| &rest[i..])
}

/// The path names a file with a VOD extension.
fn is_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or_default();
    name.rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && FILE_EXTENSIONS.contains(&ext))
}

/// What the entry's group names say.
#[derive(Default)]
struct Groups {
    vod: bool,
    series: bool,
}

impl Groups {
    fn of(c: &Channel) -> Self {
        let mut out = Groups::default();
        for word in c
            .groups
            .iter()
            .flat_map(|g| g.split(|ch: char| !ch.is_alphanumeric()))
            .filter(|w| !w.is_empty())
        {
            let word = word.to_lowercase();
            out.vod |= VOD_WORDS.contains(&word.as_str());
            out.series |= SERIES_WORDS.contains(&word.as_str());
        }
        out
    }
}

/// An episode's place, from its name: `Show S01 E02 Title`, `Show - s1e2`, `Show.S01.E02`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpisodeName {
    pub series: String,
    pub season: u32,
    pub episode: u32,
    pub title: String,
}

/// Separators trimmed off around an episode marker.
fn separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '-' | '|' | ':' | '.' | '_' | ',' | '/')
}

/// Reads `name` as an episode's (see [`EpisodeName`]); `None` without an `SxxEyy` marker.
pub fn parse_episode(name: &str) -> Option<EpisodeName> {
    let (start, end, season, episode) = episode_marker(name)?;
    Some(EpisodeName {
        series: name[..start].trim_matches(separator).to_owned(),
        season,
        episode,
        title: name[end..].trim_matches(separator).to_owned(),
    })
}

/// The first `S<1-3 digits>[ ._-]E<1-4 digits>` standing as a word in `name`: its byte range,
/// season and episode.
fn episode_marker(name: &str) -> Option<(usize, usize, u32, u32)> {
    let b = name.as_bytes();
    let word_edge = |i: usize| b.get(i).is_none_or(|c| !c.is_ascii_alphanumeric());
    for i in 0..b.len() {
        if !matches!(b[i], b'S' | b's') || (i > 0 && !word_edge(i - 1)) {
            continue;
        }
        let Some((season, mut j)) = number(b, i + 1, 3) else {
            continue;
        };
        if matches!(b.get(j), Some(b' ' | b'.' | b'_' | b'-')) {
            j += 1;
        }
        if !matches!(b.get(j), Some(b'E' | b'e')) {
            continue;
        }
        if let Some((episode, end)) = number(b, j + 1, 4).filter(|&(_, end)| word_edge(end)) {
            return Some((i, end, season, episode));
        }
    }
    None
}

/// 1 to `max` ASCII digits from `at`: their value and where they end.
fn number(b: &[u8], at: usize, max: usize) -> Option<(u32, usize)> {
    let len = b
        .get(at..)?
        .iter()
        .take(max + 1)
        .take_while(|c| c.is_ascii_digit())
        .count();
    if len == 0 || len > max {
        return None;
    }
    let digits = std::str::from_utf8(&b[at..at + len]).ok()?;
    Some((digits.parse().ok()?, at + len))
}

/// A movie title without a closing `(2019)`, and that year.
fn title_and_year(name: &str) -> (String, Option<u16>) {
    let trimmed = name.trim_end();
    let year = trimmed
        .strip_suffix(')')
        .and_then(|s| s.len().checked_sub(5).map(|at| (s, at)))
        .filter(|&(s, at)| s.is_char_boundary(at) && s[at..].starts_with('('))
        .and_then(|(s, at)| Some((at, s[at + 1..].parse::<u16>().ok()?)))
        .filter(|&(_, y)| (1880..=2100).contains(&y));
    match year {
        Some((at, y)) => {
            let title = trimmed[..at].trim_end_matches(separator);
            if title.is_empty() {
                (trimmed.to_owned(), None)
            } else {
                (title.to_owned(), Some(y))
            }
        }
        None => (trimmed.to_owned(), None),
    }
}

/// `c` as the VOD item of `kind`.
pub fn item(c: Channel, kind: VodKind) -> VodItem {
    let first = c.groups.first().cloned();
    let base = VodItem {
        key: c.key,
        kind,
        poster: c.logo,
        groups: c.groups,
        url: c.url,
        http_headers: c.http_headers,
        ..VodItem::default()
    };
    match kind {
        VodKind::Movie => {
            let (title, year) = title_and_year(&c.name);
            VodItem {
                title,
                year,
                ..base
            }
        }
        VodKind::Episode => match parse_episode(&c.name) {
            Some(e) => VodItem {
                // A name that is only the marker: the group stands for the series.
                series: Some(e.series).filter(|s| !s.is_empty()).or(first),
                season: Some(e.season),
                episode: Some(e.episode),
                title: e.title,
                ..base
            },
            None => VodItem {
                series: Some(c.name.trim().to_owned()),
                title: c.name.trim().to_owned(),
                ..base
            },
        },
    }
}

/// Moves the VOD entries out of `channels` (live channels keep their order) and returns them as
/// items, in playlist order.
pub fn split(channels: &mut Vec<Channel>) -> Vec<VodItem> {
    let mut vod = Vec::new();
    let mut live = Vec::with_capacity(channels.len());
    for c in channels.drain(..) {
        match classify(&c) {
            Some(kind) => vod.push(item(c, kind)),
            None => live.push(c),
        }
    }
    *channels = live;
    vod
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Catchup, CatchupMode};

    fn entry(name: &str, group: &str, url: &str) -> Channel {
        Channel {
            key: ChannelKey::of_url(url),
            name: name.into(),
            groups: (!group.is_empty())
                .then(|| group.to_owned())
                .into_iter()
                .collect(),
            url: url.into(),
            ..Channel::default()
        }
    }

    fn typed(mut c: Channel, t: &str) -> Channel {
        c.extra.push(("tvg-type".into(), t.into()));
        c
    }

    fn archived(mut c: Channel) -> Channel {
        c.catchup = Catchup {
            mode: Some(CatchupMode::Shift),
            days: 7,
            source: Some(format!("{}?utc={{utc}}", c.url)),
            ..Catchup::default()
        };
        c
    }

    #[test]
    fn live_channels_stay_live() {
        for c in [
            entry("News HD", "News", "http://h.example/news/index.m3u8"),
            entry("Sport", "", "http://h.example/live/u/p/12.ts"),
            entry("Kino Polska", "Filmy", "http://h.example/kino/mono.m3u8"),
            entry("HBO", "Movies", "http://h.example/hbo"),
            entry(
                "Friends S01E01 marathon",
                "Comedy",
                "http://h.example/f.m3u8",
            ),
            archived(entry("Film.mp4 channel", "", "http://h.example/a.mp4")),
            entry("Video", "", "http://h.example/movie.mp4.m3u8"),
            // Live streams with an archive whose names only look like VOD paths.
            archived(entry(
                "Movies HD",
                "Filmy",
                "https://cdn.example/movies/index.m3u8",
            )),
            archived(entry("Series", "", "https://cdn.example/series/mono.m3u8")),
            archived(entry("Kino", "", "https://cdn.example/movie/a/index.m3u8")),
        ] {
            assert_eq!(classify(&c), None, "{}", c.name);
        }
        let mut radio = entry("Radio", "VOD", "http://h.example/r.mp4");
        radio.radio = true;
        assert_eq!(classify(&radio), None);
    }

    #[test]
    fn each_vod_shape_is_found() {
        let movie = Some(VodKind::Movie);
        let episode = Some(VodKind::Episode);
        for (c, want) in [
            (entry("Film", "", "http://h.example/movie/u/p/1.mkv"), movie),
            (entry("Film", "", "http://h.example/MOVIE/u/p/1"), movie),
            (
                entry("Show", "", "http://h.example/series/u/p/2.mp4"),
                episode,
            ),
            (
                entry("Film", "", "http://h.example/movies/heat.m3u8"),
                movie,
            ),
            (entry("Film", "", "http://h.example/a/b.avi?token=x"), movie),
            (entry("Film", "", "https://h.example/f.MKV#t=1"), movie),
            (
                entry("Film", "PL | VOD Filmy", "http://h.example/v.m3u8"),
                movie,
            ),
            (
                entry("Ep", "VOD Seriale", "http://h.example/v.m3u8"),
                episode,
            ),
            (
                entry("Show S01 E02", "Seriale", "http://h.example/e.m3u8"),
                episode,
            ),
            (entry("Show S01 E02", "", "http://h.example/e.mp4"), episode),
            (typed(entry("X", "", "http://h.example/x"), "movie"), movie),
            (
                typed(entry("X", "", "http://h.example/x"), "Series"),
                episode,
            ),
        ] {
            assert_eq!(classify(&c), want, "{} in {:?}", c.name, c.groups);
        }
        // The header's archive does not make an Xtream VOD path live; an explicit type wins over
        // every guess.
        let header = archived(entry("Film", "", "http://h.example/movie/u/p/3.mkv"));
        assert_eq!(classify(&header), movie);
        let live = typed(entry("Film", "VOD", "http://h.example/a.mp4"), "live");
        assert_eq!(classify(&live), None);
        let unknown = typed(entry("Film", "", "http://h.example/a.mp4"), "other");
        assert_eq!(classify(&unknown), movie);
    }

    #[test]
    fn episode_names() {
        let e = |name: &str| parse_episode(name).map(|e| (e.series, e.season, e.episode, e.title));
        let own = |s: &str, a, b, t: &str| Some((s.to_owned(), a, b, t.to_owned()));
        assert_eq!(e("Ranczo S01 E02"), own("Ranczo", 1, 2, ""));
        assert_eq!(e("Ranczo S01E02 Wesele"), own("Ranczo", 1, 2, "Wesele"));
        assert_eq!(
            e("The Show - s2e10 - Finale"),
            own("The Show", 2, 10, "Finale")
        );
        assert_eq!(e("The.Show.S03.E04.Pilot"), own("The.Show", 3, 4, "Pilot"));
        assert_eq!(e("S01E01"), own("", 1, 1, ""));
        assert_eq!(e("Zo\u{142}za S1 E1"), own("Zo\u{142}za", 1, 1, ""));
        for none in [
            "Bus S1", "Seasons", "PS01E02", "S01E02x", "S1234E1", "S1E12345", "SE1",
        ] {
            assert_eq!(e(none), None, "{none}");
        }
    }

    #[test]
    fn items_carry_titles_years_and_series() {
        let m = item(
            entry("Heat (1995)", "Filmy", "http://h.example/movie/1.mkv"),
            VodKind::Movie,
        );
        assert_eq!((m.title.as_str(), m.year), ("Heat", Some(1995)));
        assert_eq!(m.groups, ["Filmy"]);
        assert_eq!(m.label(), "Heat");
        let mut both = entry("Heat", "Filmy", "http://h.example/movie/1.mkv");
        both.groups.push("Adult".into());
        assert_eq!(
            item(both, VodKind::Movie).groups,
            ["Filmy", "Adult"],
            "every group"
        );
        let odd = item(
            entry("(1995)", "", "http://h.example/2.mkv"),
            VodKind::Movie,
        );
        assert_eq!((odd.title.as_str(), odd.year), ("(1995)", None));
        let far = item(
            entry("Film (3000)", "", "http://h.example/3.mkv"),
            VodKind::Movie,
        );
        assert_eq!(far.year, None);

        let ep = item(
            entry(
                "Ranczo S02 E03 Wesele",
                "Seriale",
                "http://h.example/series/9.mp4",
            ),
            VodKind::Episode,
        );
        assert_eq!(ep.series.as_deref(), Some("Ranczo"));
        assert_eq!((ep.season, ep.episode), (Some(2), Some(3)));
        assert_eq!(ep.label(), "S02 E03 Wesele");
        assert_eq!(ep.full_title(), "Ranczo S02 E03 Wesele");
        let bare = item(
            entry("S01E05", "Ranczo", "http://h.example/5.mp4"),
            VodKind::Episode,
        );
        assert_eq!(bare.series.as_deref(), Some("Ranczo"));
        assert_eq!(bare.label(), "S01 E05");
        assert_eq!(bare.full_title(), "Ranczo S01 E05");
        let unnumbered = item(
            entry("Ranczo odc. 5", "Seriale", "http://h.example/6.mp4"),
            VodKind::Episode,
        );
        assert_eq!(unnumbered.series.as_deref(), Some("Ranczo odc. 5"));
        assert_eq!((unnumbered.season, unnumbered.episode), (None, None));
        assert_eq!(unnumbered.full_title(), "Ranczo odc. 5", "the name once");
    }

    #[test]
    fn split_keeps_live_order_and_moves_vod_out() {
        let mut list = vec![
            entry("A", "News", "http://h.example/a.m3u8"),
            entry("Heat (1995)", "VOD", "http://h.example/heat.mkv"),
            entry("B", "News", "http://h.example/b.m3u8"),
            entry("Show S01E01", "Seriale", "http://h.example/s.mp4"),
        ];
        let vod = split(&mut list);
        let names: Vec<_> = list.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["A", "B"]);
        let titles: Vec<_> = vod.iter().map(VodItem::label).collect();
        assert_eq!(titles, ["Heat", "S01 E01"]);
    }

    #[test]
    fn progress_rules() {
        let key = ChannelKey(1);
        assert!(watched(5_700, 6_000));
        assert!(!watched(5_600, 6_000));
        assert!(watched(590, 600) && !watched(530, 600));
        assert!(!watched(0, 0));
        let p = VodProgress::new(key, 1_200, 6_000, 5);
        assert_eq!((p.resume_at(), p.worth_keeping()), (Some(1_200), true));
        let done = VodProgress::new(key, 5_990, 6_000, 5);
        assert!(done.watched && done.resume_at().is_none() && done.worth_keeping());
        let barely = VodProgress::new(key, 10, 6_000, 5);
        assert!(barely.resume_at().is_none() && !barely.worth_keeping());
        // A length not known yet is never watched.
        assert_eq!(VodProgress::new(key, 400, 0, 5).resume_at(), Some(400));
    }

    #[test]
    fn debug_masks_urls_and_headers() {
        let mut c = entry(
            "Film",
            "",
            "http://h.example/movie/user/SECRETPASS99/1.mkv?token=TOKEN1234567",
        );
        c.logo = Some("http://h.example/p.jpg?token=TOKEN1234567".into());
        c.http_headers = vec![("Cookie".into(), "session=plain-word".into())];
        let shown = format!("{:?}", item(c, VodKind::Movie));
        assert!(!shown.contains("TOKEN1234567"), "{shown}");
        assert!(!shown.contains("plain-word"), "{shown}");
        assert!(shown.contains("Film") && shown.contains("Cookie"));
    }
}
