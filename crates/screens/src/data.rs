//! The contract between the screens and the rest of the app.
//!
//! Every screen is a [`Screen`]: it reads through [`TvData`] and never touches storage, the
//! network or the player directly; everything it wants done comes back in an [`Output`] the app
//! shell carries out. That keeps every screen a pure state machine plus a view, testable on the
//! host with [`fake::FakeData`].

use std::rc::Rc;

use iptv::model::{fold, Channel, ChannelKey, Programme, Timestamp};
use tv_machine::machine::Measure;
use tv_platform::i18n::msg;
use tv_ui::Painter;

use crate::input::{Hold, Key, Press};
use crate::time::{self, When};

/// A row in the groups panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Group {
    Favorites,
    All,
    History,
    /// A group declared by the playlist.
    Playlist(String),
}

impl Group {
    /// The group's name as every screen shows it.
    pub fn name(&self) -> &str {
        match self {
            Group::Favorites => msg::channel_favorites(),
            Group::All => msg::channel_all(),
            Group::History => msg::channel_history(),
            Group::Playlist(name) => name,
        }
    }
}

/// One watched catch-up programme in History.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub channel: ChannelKey,
    pub programme: Programme,
    /// Seconds watched into the programme.
    pub position_secs: i64,
    pub watched_at: Timestamp,
}

/// What the decoder reports about the current stream (OSD badges).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MediaInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub audio_channels: u8,
}

/// The player as the screens see it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Playback {
    pub channel: Option<ChannelKey>,
    /// Set while a past programme plays from the archive.
    pub catchup: Option<Programme>,
    pub paused: bool,
    /// Position inside the catch-up programme, in seconds.
    pub position_secs: i64,
    pub media: MediaInfo,
    pub audio_tracks: Vec<String>,
    pub audio_track: usize,
}

/// A programme as the guide grid and the lists draw it: no description, no category.
/// [`TvData::programme_at`] reads the rest of the focused one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GridProgramme {
    pub start: Timestamp,
    pub stop: Timestamp,
    pub title: String,
}

impl GridProgramme {
    pub fn duration_secs(&self) -> i64 {
        self.stop - self.start
    }

    pub fn is_on_air(&self, now: Timestamp) -> bool {
        self.start <= now && now < self.stop
    }
}

impl From<GridProgramme> for Programme {
    fn from(p: GridProgramme) -> Self {
        Programme {
            start: p.start,
            stop: p.stop,
            title: p.title,
            ..Programme::default()
        }
    }
}

impl From<Programme> for GridProgramme {
    fn from(p: Programme) -> Self {
        GridProgramme {
            start: p.start,
            stop: p.stop,
            title: p.title,
        }
    }
}

/// Read access to everything a screen shows. Implementations must answer quickly (they back
/// per-frame drawing); screens still cache what they draw and re-query on change only.
pub trait TvData {
    fn now(&self) -> Timestamp;
    fn groups(&self) -> Vec<Group>;
    /// The channels of `group`, shared: an implementation keeps the list until the playlist,
    /// the favourites, History or the sort setting change, so screens hold it instead of copying.
    fn channels(&self, group: &Group) -> Rc<[Rc<Channel>]>;
    /// Programmes of `channel` overlapping `[from, to)`, ordered by start.
    fn programmes(&self, channel: ChannelKey, from: Timestamp, to: Timestamp) -> Vec<Programme>;
    fn is_favorite(&self, channel: ChannelKey) -> bool;
    /// Most recent first.
    fn recent_channels(&self) -> Vec<ChannelKey>;
    /// Most recent first.
    fn history(&self) -> Vec<HistoryEntry>;
    fn playback(&self) -> Playback;
    /// The TV's offset from UTC in seconds, for showing local times and day boundaries.
    fn utc_offset_secs(&self) -> i32 {
        0
    }

    /// How many days before today the guide reaches back (the `epg.past_days` setting, whose
    /// default this is).
    fn past_days(&self) -> u32 {
        7
    }

    /// The channel with `key`, wherever it is listed.
    fn channel(&self, key: ChannelKey) -> Option<Rc<Channel>> {
        self.channels(&Group::All)
            .iter()
            .find(|c| c.key == key)
            .cloned()
    }

    /// [`programmes`](Self::programmes) without the descriptions: what the guide grid and the
    /// lists draw.
    fn grid_programmes(
        &self,
        channel: ChannelKey,
        from: Timestamp,
        to: Timestamp,
    ) -> Vec<GridProgramme> {
        self.programmes(channel, from, to)
            .into_iter()
            .map(GridProgramme::from)
            .collect()
    }

    /// The programme on air at `now` (the latest-starting one, when they overlap) and the next
    /// one to start.
    fn now_next(
        &self,
        channel: ChannelKey,
        now: Timestamp,
    ) -> (Option<Programme>, Option<Programme>) {
        let list = self.programmes(channel, now, now + time::DAY);
        let current = list
            .iter()
            .filter(|p| p.is_on_air(now))
            .max_by_key(|p| p.start)
            .cloned();
        let next = list.into_iter().find(|p| p.start > now);
        (current, next)
    }

    /// Every field of `channel`'s programme starting at `start` (the focused one of a list).
    fn programme_at(&self, channel: ChannelKey, start: Timestamp) -> Option<Programme> {
        self.programmes(channel, start, start + 1)
            .into_iter()
            .find(|p| p.start == start)
    }

    /// [`grid_programmes`](Self::grid_programmes) of each of `channels` (the visible guide
    /// rows), read together.
    fn guide_window(
        &self,
        channels: &[ChannelKey],
        from: Timestamp,
        to: Timestamp,
    ) -> Vec<Vec<GridProgramme>> {
        channels
            .iter()
            .map(|&c| self.grid_programmes(c, from, to))
            .collect()
    }

    /// Start a search for `query` (as typed; titles and query compared [`fold`]ed), or the
    /// finished answer to it; `None` while it runs (a search can outlast many frames on the TV),
    /// so the search panel asks again on its next tick. A new query supersedes the one before.
    /// The implementation owns what is listed: programmes on air, past ones the channel's
    /// archive and the past-days setting still reach, upcoming ones up to [`SEARCH_AHEAD_SECS`]
    /// ahead, in [`search_order`], at most [`SEARCH_HITS`]. This default answers at once,
    /// bounding the past by the archive alone.
    fn search(&self, query: &str) -> Option<SearchResult> {
        let query = fold(query.trim());
        let now = self.now();
        let mut hits = Vec::new();
        if !query.is_empty() {
            for c in self.channels(&Group::All).iter() {
                let days = if c.catchup.available() {
                    i64::from(c.catchup.days)
                } else {
                    0
                };
                let from = now - days * time::DAY;
                for p in self.grid_programmes(c.key, from, now + SEARCH_AHEAD_SECS) {
                    let listed = match time::when(p.start, p.stop, now) {
                        When::Past => p.start >= from && days > 0,
                        When::OnAir | When::Future => true,
                    };
                    if listed && fold(&p.title).contains(&query) {
                        hits.push((c.key, p));
                    }
                }
            }
        }
        hits.sort_by_key(|(_, p)| search_order(p, now));
        let capped = hits.len() > SEARCH_HITS;
        hits.truncate(SEARCH_HITS);
        Some(SearchResult {
            query,
            hits,
            capped,
        })
    }
}

/// The most programme hits a search lists.
pub const SEARCH_HITS: usize = 100;
/// How far ahead of now a search looks.
pub const SEARCH_AHEAD_SECS: i64 = 2 * time::DAY;

/// A programme search's answer ([`TvData::search`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchResult {
    /// The folded query this answers.
    pub query: String,
    /// In list order ([`search_order`]), at most [`SEARCH_HITS`].
    pub hits: Vec<(ChannelKey, GridProgramme)>,
    /// More matched than are listed.
    pub capped: bool,
}

/// The order search hits are listed in: on air first, then the past newest first, then the
/// future soonest first; a stable sort by it keeps playlist order within each.
pub fn search_order(p: &GridProgramme, now: Timestamp) -> (When, Timestamp) {
    match time::when(p.start, p.stop, now) {
        When::OnAir => (When::OnAir, 0),
        When::Past => (When::Past, -p.start),
        When::Future => (When::Future, p.start),
    }
}

/// Channel logos as resident textures. The app owns loading and caching; screens only draw.
pub trait Logos {
    /// The texture of `channel`'s logo and its size in pixels (so screens keep its aspect),
    /// or `None` while it loads or when the channel has none.
    fn logo(&self, channel: &Channel) -> Option<(u32, f32, f32)>;
}

/// What to play.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stream {
    Live(ChannelKey),
    /// A past programme from the archive, starting `offset_secs` into it.
    Catchup {
        channel: ChannelKey,
        programme: Programme,
        offset_secs: i64,
    },
}

impl Stream {
    pub fn channel(&self) -> ChannelKey {
        match self {
            Stream::Live(key) | Stream::Catchup { channel: key, .. } => *key,
        }
    }
}

/// Everything a screen can ask the app to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Play `stream`; a live channel that already plays is left playing. The video shows where
    /// the screens say ([`Screen::video_rect`]): a screen that wants it full screen closes
    /// itself ([`Navigate::Close`]) or everything over the player ([`Navigate::Player`]).
    /// `group` is the list the channel was picked from: the player zaps through it from now on.
    Play {
        stream: Stream,
        group: Option<Group>,
    },
    /// Seek inside the current catch-up programme to an absolute position.
    SeekTo {
        position_secs: i64,
    },
    ReturnToLive,
    TogglePause,
    SelectAudioTrack(usize),
    ToggleFavorite(ChannelKey),
    RemoveHistory {
        channel: ChannelKey,
        start: Timestamp,
    },
    /// Refresh the playlist and its guides now.
    RefreshNow,
    SetSetting {
        key: String,
        value: String,
    },
    Exit,
}

/// Screen changes a screen asks the app shell for. Full-screen playback is always at the
/// bottom of the stack; everything else opens over it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Navigate {
    /// Close the requesting screen and return to the one below.
    Close,
    /// Close every screen: the full-screen player is in front.
    Player,
    Guide,
    /// The groups panel over the channel list, the list's current group focused.
    Groups(Group),
    /// TiviMate's main/options menu (Left from the groups panel).
    MainMenu,
    History,
    ChannelList {
        preview: bool,
    },
    /// Long OK on a channel, optionally on one of its programmes.
    ContextMenu {
        channel: ChannelKey,
        programme: Option<Programme>,
    },
    GroupMenu(Group),
    PlayerMenu,
    Settings,
    Search,
}

/// What a screen tells the screen right under it ([`Output::message`], [`Screen::message`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScreenMessage {
    /// Enter Manage favorites: the guide, from the context menu over it.
    ManageFavorites,
    /// Show this group: the channel list, from the groups panel over it.
    SetGroup(Group),
}

/// A rectangle on the 1920x1080 graphics plane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// What a key, a tick or text produced. The shell runs the commands in order, delivers the
/// message, then applies the navigation in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub commands: Vec<Command>,
    /// `Close` closes the screen that returned it.
    pub navigate: Vec<Navigate>,
    /// For the screen right under the one that returned it.
    pub message: Option<ScreenMessage>,
    /// A short confirmation at the bottom of the screen.
    pub toast: Option<String>,
    /// Something on screen changed: draw a frame.
    pub redraw: bool,
}

impl Output {
    pub fn redraw() -> Self {
        Output {
            redraw: true,
            ..Output::default()
        }
    }

    pub fn command(command: Command) -> Self {
        Output {
            commands: vec![command],
            ..Output::default()
        }
    }

    pub fn navigate(to: Navigate) -> Self {
        Output {
            navigate: vec![to],
            ..Output::default()
        }
    }

    pub fn close() -> Self {
        Self::navigate(Navigate::Close)
    }

    /// This, then `next`: its commands and navigation run after these, its message and toast
    /// win when both have one.
    pub fn and(mut self, next: Output) -> Self {
        self.commands.extend(next.commands);
        self.navigate.extend(next.navigate);
        self.message = next.message.or(self.message);
        self.toast = next.toast.or(self.toast);
        self.redraw |= next.redraw;
        self
    }
}

/// A screen the app shell stacks over the full-screen player (the player is one too, always at
/// the bottom). Only the top screen gets keys and text; every screen is ticked when its
/// [`next_wake`](Self::next_wake) is due and drawn bottom up. Nothing here runs on a timer of its
/// own: `now_ms` is the shell's millisecond clock, and [`crate::time::wake_ms`] converts a
/// [`Timestamp`] deadline onto it.
pub trait Screen {
    /// How the input layer treats `key` held down in the current state.
    fn hold(&self, key: Key) -> Hold;

    /// One remote action.
    fn key(&mut self, key: Key, press: Press, data: &dyn TvData, now_ms: u64) -> Output;

    /// Text the TV keyboard committed; only a screen with a text field takes it.
    fn text(&mut self, _text: &str, _data: &dyn TvData) -> Output {
        Output::default()
    }

    /// A text field is being typed into. The shell keeps the TV keyboard up while the top screen
    /// says so and dismisses it when that stops or the screen closes; text arriving while it is
    /// down means the user raised it, and the shell takes it as up from then on.
    fn editing(&self) -> bool {
        false
    }

    /// The clock moved on: called once [`next_wake`](Self::next_wake) is due and whenever
    /// playback changed. Cheap when nothing is due.
    fn tick(&mut self, _data: &dyn TvData, _now_ms: u64) -> Output {
        Output::default()
    }

    /// When [`tick`](Self::tick) next has something to do (a timeout, an animation frame, the
    /// clock on screen turning over); `None` while nothing is.
    fn next_wake(&self, _now_ms: u64) -> Option<u64> {
        None
    }

    /// Where this screen wants the video, `None` when it leaves that to the screens under it.
    /// The shell puts the video in the topmost screen's rectangle, full screen when none has one.
    fn video_rect(&self) -> Option<Rect> {
        None
    }

    /// The stored data changed (playlist, guide, favourites, settings): read it again, keeping
    /// the focus where it is.
    fn refresh(&mut self, _data: &dyn TvData) {}

    /// A message from the screen over this one.
    fn message(&mut self, _message: ScreenMessage, _data: &dyn TvData) {}

    /// The screen was closed and is still animating out: the shell keeps drawing it, over the
    /// screens that were under it, until it is [`gone`](Self::gone).
    fn closing(&self) -> bool {
        false
    }

    fn gone(&self) -> bool {
        true
    }

    /// Step the animations by `dt` seconds.
    fn update(&mut self, _dt: f32) {}

    /// Draw, laying out first whatever changed since the last frame: text is measured here, not
    /// while painting ([`crate::draw::Cached`]).
    fn draw(&mut self, p: Painter, m: &dyn Measure, logos: &dyn Logos);
}

/// In-memory [`TvData`] with synthetic channels and a generated guide, for tests and the
/// simulator, and a fixed-width [`Measure`] for layout tests.
pub mod fake {
    use super::*;
    use iptv::model::{Catchup, CatchupMode};
    use std::ffi::CStr;

    /// The guide of the channel at `index` in [`FakeData::channels`], overlapping `[from, to)`
    /// and ordered by start.
    pub type Schedule = Box<dyn Fn(usize, Timestamp, Timestamp) -> Vec<Programme>>;

    pub struct FakeData {
        pub now: Timestamp,
        pub channels: Vec<Channel>,
        pub favorites: Vec<ChannelKey>,
        pub recents: Vec<ChannelKey>,
        pub history: Vec<HistoryEntry>,
        pub playback: Playback,
        /// [`TvData::utc_offset_secs`].
        pub utc_offset: i32,
        /// [`TvData::past_days`].
        pub past_days: u32,
        /// Replaces the generated guide.
        pub schedule: Option<Schedule>,
    }

    impl FakeData {
        /// `count` channels over three groups; every third has no catch-up.
        pub fn new(now: Timestamp, count: usize) -> Self {
            let groups = ["News", "Sport", "Movies"];
            let channels = (0..count)
                .map(|i| {
                    let url = format!("http://host/ch{i}/mono.m3u8");
                    let source = format!("{url}?utc={{utc}}&lutc={{lutc}}");
                    Channel {
                        key: ChannelKey::of_url(&url),
                        name: format!("Channel {}", i + 1),
                        number: Some(i as u32 + 1),
                        logo: None,
                        groups: vec![groups[i % groups.len()].to_owned()],
                        url,
                        tvg_id: Some(format!("ch{i}")),
                        tvg_name: None,
                        tvg_shift_minutes: 0,
                        catchup: if i % 3 == 2 {
                            Catchup::default()
                        } else {
                            Catchup {
                                mode: Some(CatchupMode::Shift),
                                days: 7,
                                source: Some(source),
                            }
                        },
                        radio: false,
                        http_headers: vec![],
                        extra: vec![],
                    }
                })
                .collect();
            FakeData {
                now,
                channels,
                favorites: vec![],
                recents: vec![],
                history: vec![],
                playback: Playback::default(),
                utc_offset: 0,
                past_days: 7,
                schedule: None,
            }
        }

        /// Use `schedule` for the guide instead of the generated one.
        pub fn with_schedule(
            mut self,
            schedule: impl Fn(usize, Timestamp, Timestamp) -> Vec<Programme> + 'static,
        ) -> Self {
            self.schedule = Some(Box::new(schedule));
            self
        }
    }

    /// Deterministic programme grid: lengths cycle through 15-90 minutes in a fixed 4-hour cycle
    /// anchored to the Unix epoch and shifted per channel, so a channel shows the same
    /// programmes whatever window is asked for.
    fn guide(channel: ChannelKey, from: Timestamp, to: Timestamp) -> Vec<Programme> {
        const LENGTHS: [i64; 5] = [30, 60, 15, 90, 45];
        const CYCLE: i64 = 240 * 60;
        let phase = channel.0.rem_euclid(16) * 15 * 60;
        let mut start = from - (from - phase).rem_euclid(CYCLE);
        let mut i = ((start - phase).div_euclid(CYCLE) as usize) * LENGTHS.len();
        let mut out = Vec::new();
        while start < to {
            let stop = start + LENGTHS[i % LENGTHS.len()] * 60;
            if stop > from {
                out.push(Programme {
                    start,
                    stop,
                    title: format!("Programme {}", (i + channel.0.rem_euclid(97) as usize) % 97),
                    description: Some("Synthetic programme for tests and the simulator.".into()),
                    category: None,
                    icon: None,
                });
            }
            start = stop;
            i += 1;
        }
        out
    }

    impl TvData for FakeData {
        fn now(&self) -> Timestamp {
            self.now
        }

        fn groups(&self) -> Vec<Group> {
            let mut out = vec![Group::Favorites, Group::All, Group::History];
            for c in &self.channels {
                let g = Group::Playlist(c.groups[0].clone());
                if !out.contains(&g) {
                    out.push(g);
                }
            }
            out
        }

        fn channels(&self, group: &Group) -> Rc<[Rc<Channel>]> {
            self.channels
                .iter()
                .filter(|c| match group {
                    Group::All => true,
                    Group::Favorites => self.favorites.contains(&c.key),
                    Group::History => self.history.iter().any(|h| h.channel == c.key),
                    Group::Playlist(name) => c.groups.contains(name),
                })
                .map(|c| Rc::new(c.clone()))
                .collect()
        }

        fn programmes(
            &self,
            channel: ChannelKey,
            from: Timestamp,
            to: Timestamp,
        ) -> Vec<Programme> {
            match &self.schedule {
                None => guide(channel, from, to),
                Some(schedule) => match self.channels.iter().position(|c| c.key == channel) {
                    Some(i) => schedule(i, from, to),
                    None => Vec::new(),
                },
            }
        }

        fn is_favorite(&self, channel: ChannelKey) -> bool {
            self.favorites.contains(&channel)
        }

        fn recent_channels(&self) -> Vec<ChannelKey> {
            self.recents.clone()
        }

        fn history(&self) -> Vec<HistoryEntry> {
            self.history.clone()
        }

        fn playback(&self) -> Playback {
            self.playback.clone()
        }

        fn utc_offset_secs(&self) -> i32 {
            self.utc_offset
        }

        fn past_days(&self) -> u32 {
            self.past_days
        }
    }

    /// Half the size per character, lines 1.2 sizes high: layout is predictable without a font.
    pub struct Mono;

    impl Measure for Mono {
        fn width(&self, s: &CStr, sz: i32, _bold: bool) -> f32 {
            let chars = s.to_str().map_or(s.to_bytes().len(), |t| t.chars().count());
            chars as f32 * sz as f32 * 0.5
        }
        fn cap_h(&self, sz: i32) -> f32 {
            sz as f32 * 0.7
        }
        fn line_h(&self, sz: i32) -> f32 {
            sz as f32 * 1.2
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const NOW: Timestamp = 1_700_000_000;

        fn at(start: Timestamp, minutes: i64, title: &str) -> Programme {
            Programme {
                start,
                stop: start + minutes * 60,
                title: title.into(),
                description: Some(format!("About {title}")),
                ..Programme::default()
            }
        }

        /// Channel 0: a show on air, the news before it and a film after; channel 1: the news
        /// yesterday and tomorrow; channel 2: nothing.
        fn listings() -> FakeData {
            FakeData::new(NOW, 3).with_schedule(|i, from, to| {
                let all = match i {
                    0 => vec![
                        at(NOW - 3600, 30, "Evening News"),
                        at(NOW - 1800, 60, "Quiz Show"),
                        at(NOW + 1800, 90, "Film"),
                    ],
                    1 => vec![
                        at(NOW - 86_400, 30, "Morning NEWS"),
                        at(NOW + 80_000, 30, "Late news"),
                    ],
                    _ => vec![],
                };
                all.into_iter()
                    .filter(|p| p.stop > from && p.start < to)
                    .collect()
            })
        }

        #[test]
        fn guide_is_contiguous_and_covers_the_window() {
            let d = FakeData::new(NOW, 6);
            let key = d.channels[0].key;
            let p = d.programmes(key, d.now - 3600, d.now + 4 * 3600);
            assert!(p.first().unwrap().start <= d.now - 3600);
            assert!(p.last().unwrap().stop >= d.now + 4 * 3600);
            assert!(p.windows(2).all(|w| w[0].stop == w[1].start));
        }

        #[test]
        fn guide_is_stable_across_query_windows() {
            let d = FakeData::new(NOW, 6);
            let key = d.channels[1].key;
            let wide = d.programmes(key, d.now - 6 * 3600, d.now + 6 * 3600);
            let narrow = d.programmes(key, d.now, d.now + 3600);
            assert!(narrow.iter().all(|p| wide.contains(p)));
        }

        #[test]
        fn groups_and_filters() {
            let mut d = FakeData::new(NOW, 6);
            d.favorites.push(d.channels[4].key);
            assert_eq!(d.groups().len(), 6);
            assert_eq!(d.channels(&Group::All).len(), 6);
            assert_eq!(d.channels(&Group::Favorites).len(), 1);
            assert_eq!(d.channels(&Group::Playlist("Sport".into())).len(), 2);
            assert!(!d.channels[2].catchup.available());
            let list = d.channels(&Group::Playlist("Sport".into()));
            assert_eq!(list.len(), 2);
            assert_eq!(list[0].key, d.channels[1].key);
            let key = d.channels[3].key;
            assert_eq!(
                d.channel(key).map(|c| c.name.clone()).as_deref(),
                Some("Channel 4")
            );
            assert!(d.channel(ChannelKey::of_url("http://host/gone")).is_none());
        }

        #[test]
        fn a_schedule_and_an_offset_replace_the_defaults() {
            let mut d = listings();
            assert_eq!(d.utc_offset_secs(), 0);
            d.utc_offset = 7200;
            assert_eq!(d.utc_offset_secs(), 7200);
            let titles = |i: usize| -> Vec<String> {
                d.programmes(d.channels[i].key, NOW - 7200, NOW + 7200)
                    .into_iter()
                    .map(|p| p.title)
                    .collect()
            };
            assert_eq!(titles(0), ["Evening News", "Quiz Show", "Film"]);
            assert!(titles(1).is_empty());
            assert!(titles(2).is_empty());
            let gone = ChannelKey::of_url("http://host/gone.m3u8");
            assert!(d.programmes(gone, 0, Timestamp::MAX).is_empty());
        }

        #[test]
        fn the_cheap_queries_answer_from_programmes() {
            let d = listings();
            let key = d.channels[0].key;
            let (now, next) = d.now_next(key, NOW);
            assert_eq!(now.map(|p| p.title).as_deref(), Some("Quiz Show"));
            assert_eq!(next.map(|p| p.title).as_deref(), Some("Film"));
            assert_eq!(d.now_next(d.channels[2].key, NOW), (None, None));
            let (none, next) = d.now_next(d.channels[1].key, NOW);
            assert!(none.is_none());
            assert_eq!(next.map(|p| p.title).as_deref(), Some("Late news"));

            let full = d.programme_at(key, NOW + 1800).unwrap();
            assert_eq!(full.description.as_deref(), Some("About Film"));
            assert_eq!(d.programme_at(key, NOW + 1801), None);

            let grid = d.grid_programmes(key, NOW, NOW + 1);
            assert_eq!(
                grid,
                [GridProgramme {
                    start: NOW - 1800,
                    stop: NOW + 1800,
                    title: "Quiz Show".into(),
                }]
            );
            assert!(grid[0].is_on_air(NOW) && grid[0].duration_secs() == 3600);
        }

        #[test]
        fn search_lists_on_air_then_the_past_newest_first_then_the_future() {
            let mut d = listings();
            let titles = |d: &FakeData, query: &str| -> Vec<String> {
                let found = d.search(query).expect("answers at once");
                found.hits.into_iter().map(|(_, p)| p.title).collect()
            };
            assert_eq!(
                titles(&d, "NEWS "),
                ["Evening News", "Morning NEWS", "Late news"]
            );
            assert_eq!(titles(&d, "show"), ["Quiz Show"]);
            assert!(titles(&d, "weather").is_empty());
            assert!(titles(&d, "  ").is_empty());
            assert_eq!(d.search("news").unwrap().query, "news");
            // Past programmes only while the channel keeps an archive.
            d.channels[1].catchup = Default::default();
            assert_eq!(titles(&d, "news"), ["Evening News", "Late news"]);
            let many = FakeData::new(NOW, 40);
            let found = many.search("programme").unwrap();
            assert!(found.capped && found.hits.len() == SEARCH_HITS);
            assert!(found
                .hits
                .windows(2)
                .all(|w| search_order(&w[0].1, NOW) <= search_order(&w[1].1, NOW)));
        }

        #[test]
        fn guide_window_reads_every_row() {
            let d = listings();
            let keys: Vec<_> = d.channels.iter().map(|c| c.key).collect();
            let rows = d.guide_window(&keys, NOW, NOW + 3600);
            let counts: Vec<usize> = rows.iter().map(Vec::len).collect();
            assert_eq!(counts, [2, 0, 0]);
            let back: Programme = rows[0][0].clone().into();
            assert_eq!((back.title.as_str(), back.description), ("Quiz Show", None));
        }

        #[test]
        fn group_names() {
            assert_eq!(Group::Favorites.name(), "Favorites");
            assert_eq!(Group::All.name(), "All channels");
            assert_eq!(Group::Playlist("Sport".into()).name(), "Sport");
            let _pl = tv_platform::i18n::language_on_this_thread_for_test(
                tv_platform::i18n::Preference::Pl,
            );
            assert_eq!(Group::Favorites.name(), "Ulubione");
            assert_eq!(Group::History.name(), "Historia");
            assert_eq!(Group::All.name(), "Wszystkie kana\u{142}y");
        }

        #[test]
        fn outputs_chain_in_order() {
            let a = Output::command(Command::RefreshNow);
            let b = Output {
                toast: Some("done".into()),
                ..Output::close()
            };
            let both = a.and(b).and(Output::redraw());
            assert_eq!(both.commands, [Command::RefreshNow]);
            assert_eq!(both.navigate, [Navigate::Close]);
            assert_eq!(both.toast.as_deref(), Some("done"));
            assert!(both.redraw);
        }

        /// The least a screen implements.
        struct Blank;

        impl Screen for Blank {
            fn hold(&self, _: Key) -> Hold {
                Hold::Once
            }
            fn key(&mut self, _: Key, _: Press, _: &dyn TvData, _: u64) -> Output {
                Output::close()
            }
            fn draw(&mut self, _: Painter, _: &dyn Measure, _: &dyn Logos) {}
        }

        #[test]
        fn a_screen_needs_only_keys_and_drawing() {
            let d = FakeData::new(NOW, 1);
            let mut s: Box<dyn Screen> = Box::new(Blank);
            assert_eq!(s.key(Key::Ok, Press::Short, &d, 0), Output::close());
            assert_eq!(s.text("x", &d), Output::default());
            assert_eq!(s.tick(&d, 0), Output::default());
            assert!(!s.editing() && !s.closing() && s.gone());
            assert_eq!((s.next_wake(0), s.video_rect()), (None, None));
        }

        #[test]
        fn mono_measures_characters() {
            assert_eq!(Mono.width_str("abc", 20, false), 30.0);
            assert_eq!(Mono.width_str("\u{2026}", 20, true), 10.0);
            assert_eq!(Mono.line_h(20), 24.0);
        }
    }
}
