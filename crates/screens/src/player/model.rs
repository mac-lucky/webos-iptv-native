//! What the player panels show, as plain strings and fractions built from [`TvData`] when
//! something changes (a key, a zap, the minute or second turning over). The view draws from this
//! and nothing else, so drawing allocates nothing and the content is host-testable.

use super::RECENT_TILES;
use crate::data::{Group, MediaInfo, Playback, TvData};
use crate::time;
use iptv::model::{Channel, ChannelKey, Programme, Timestamp};
use std::rc::Rc;
use tv_platform::i18n::msg;

/// How far back from the programme playing the one before it is looked for.
const WINDOW_SECS: i64 = 6 * 3600;

/// The facts every panel derives from: the channel on screen, its programmes around the playing
/// moment and the player state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Context {
    pub channel: Option<Rc<Channel>>,
    /// The number shown for the channel: its declared number, else its position in the playlist.
    pub number: Option<u32>,
    pub playback: Playback,
    pub now: Timestamp,
    /// [`TvData::utc_offset_secs`], for every local time shown.
    pub utc_offset_secs: i32,
    /// The programme playing: the catch-up programme, else the one on air.
    pub current: Option<Programme>,
    pub previous: Option<Programme>,
    pub next: Option<Programme>,
}

impl Context {
    /// `channel` playing `playback`, with its programmes around the playing moment read from
    /// `data`.
    pub fn build(
        data: &dyn TvData,
        channel: Option<(Rc<Channel>, u32)>,
        playback: Playback,
    ) -> Self {
        let now = data.now();
        let (channel, number) = channel.unzip();
        let mut ctx = Context {
            channel,
            number,
            playback,
            now,
            utc_offset_secs: data.utc_offset_secs(),
            ..Context::default()
        };
        let Some(key) = ctx.key() else {
            return ctx;
        };
        let (current, next) = match &ctx.playback.catchup {
            Some(p) => (Some(p.clone()), data.now_next(key, p.start).1),
            None => data.now_next(key, now),
        };
        ctx.previous = current.as_ref().and_then(|c| {
            let before = data.grid_programmes(key, c.start - WINDOW_SECS, c.start);
            let p = before.iter().rev().find(|p| p.start < c.start)?;
            data.programme_at(key, p.start)
        });
        ctx.current = current;
        ctx.next = next;
        ctx
    }

    /// The programmes were read for another channel or programme than `playback` shows, or the
    /// one on air has ended by `now`: [`build`](Self::build) again.
    pub fn stale(&self, playback: &Playback, now: Timestamp) -> bool {
        if playback.channel != self.playback.channel || playback.catchup != self.playback.catchup {
            return true;
        }
        playback.catchup.is_none()
            && match &self.current {
                Some(p) => now >= p.stop,
                None => self.next.as_ref().is_some_and(|n| now >= n.start),
            }
    }

    /// The clock and the player moved on inside the same programme.
    pub fn advance(&mut self, data: &dyn TvData, playback: Playback) {
        self.now = data.now();
        self.utc_offset_secs = data.utc_offset_secs();
        self.playback = playback;
    }

    pub fn key(&self) -> Option<ChannelKey> {
        self.channel.as_ref().map(|c| c.key)
    }

    pub fn in_catchup(&self) -> bool {
        self.playback.catchup.is_some()
    }

    /// The channel keeps an archive (catch-up).
    pub fn has_archive(&self) -> bool {
        self.channel.as_ref().is_some_and(|c| c.catchup.available())
    }

    /// `p` can be played from the channel's archive ([`iptv::catchup::in_archive`]).
    pub fn in_archive(&self, p: &Programme) -> bool {
        self.channel
            .as_ref()
            .is_some_and(|c| iptv::catchup::in_archive(c, p, self.now))
    }

    /// The moment on screen: the catch-up position, else now.
    pub fn playing_at(&self) -> Timestamp {
        match &self.playback.catchup {
            Some(p) => p.start + self.playback.position_secs,
            None => self.now,
        }
    }

    /// Where playback is inside the current programme, in seconds.
    pub fn position_secs(&self) -> i64 {
        match (&self.playback.catchup, &self.current) {
            (Some(_), _) => self.playback.position_secs,
            (None, Some(p)) => self.now - p.start,
            (None, None) => 0,
        }
    }

    /// The furthest position inside the current programme that has aired.
    pub fn live_edge_secs(&self) -> i64 {
        self.current
            .as_ref()
            .map_or(0, |p| (self.now - p.start).clamp(0, p.duration_secs()))
    }
}

/// The badges after the channel name. Resolutions and surround layouts are the same marks in
/// every language.
pub fn chips(m: &MediaInfo) -> Vec<String> {
    let mut out = Vec::new();
    let res = match m.height {
        h if h >= 2160 => "4K",
        h if h >= 1080 => "FULL HD",
        h if h >= 720 => "HD",
        h if h > 0 => "SD",
        _ => "",
    };
    if !res.is_empty() {
        out.push(res.to_owned());
    }
    if m.fps > 0 {
        out.push(msg::player_fps(i64::from(m.fps)));
    }
    match m.audio_channels {
        0 => {}
        1 => out.push(msg::player_mono().into()),
        2 => out.push(msg::player_stereo().into()),
        6 => out.push("5.1".into()),
        8 => out.push("7.1".into()),
        n => out.push(msg::player_audio_channels(i64::from(n))),
    }
    out
}

/// The info panel's header: the part shared by the zap banner, the info panel and the playback
/// panel.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Header {
    /// Top bar, left.
    pub group: String,
    /// Top bar, right.
    pub clock: String,
    /// The channel on screen, for its logo.
    pub channel: Option<Rc<Channel>>,
    pub title: String,
    pub times: String,
    /// The progress of a live programme (catch-up shows its seek bar instead).
    pub progress: Option<f32>,
    /// `7 min`: left of a live programme, or left from the catch-up position.
    pub remaining: String,
    /// Catch-up is playing: the remaining time carries the history icon.
    pub catchup: bool,
    pub number: String,
    pub name: String,
    /// The channel keeps an archive.
    pub archive: bool,
    pub chips: Vec<String>,
    pub description: Option<String>,
    /// `15:19 - 15:46` and the title of the programme after this one.
    pub next_times: String,
    pub next_title: String,
}

impl Header {
    pub fn build(ctx: &Context, group: &Group) -> Self {
        let off = ctx.utc_offset_secs;
        let mut h = Header {
            group: group.name().to_owned(),
            clock: time::clock(ctx.now, off),
            channel: ctx.channel.clone(),
            catchup: ctx.in_catchup(),
            archive: ctx.has_archive(),
            chips: chips(&ctx.playback.media),
            ..Header::default()
        };
        if let Some(c) = &ctx.channel {
            h.name = c.name.clone();
        }
        if let Some(n) = ctx.number {
            h.number = n.to_string();
        }
        match &ctx.current {
            Some(p) => {
                h.title = p.title.clone();
                h.times = time::programme_span(p, ctx.now, off);
                h.description = p.description.clone();
                let at = ctx.playing_at();
                h.remaining = time::minutes_label(time::minutes_left(p, at));
                if !ctx.in_catchup() {
                    h.progress = Some(time::progress(p, ctx.now));
                }
            }
            None => h.title = h.name.clone(),
        }
        if let Some(n) = &ctx.next {
            h.next_times = time::programme_span(n, ctx.now, off);
            h.next_title = n.title.clone();
        }
        h
    }
}

/// One tile of the info panel's row.
#[derive(Clone, Debug, PartialEq)]
pub enum TileKind {
    Guide,
    History,
    Channel(ChannelKey),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tile {
    pub kind: TileKind,
    /// The tile's caption: `TV guide`, `History`, or the recent channel's programme on air.
    pub label: String,
    /// The recent channel, for its logo.
    pub channel: Option<Rc<Channel>>,
    /// How far the recent channel's programme on air is.
    pub progress: Option<f32>,
}

/// The tile row: TV guide, History, then up to [`RECENT_TILES`] recent channels (not the one
/// playing).
pub fn tiles(data: &dyn TvData, playing: Option<ChannelKey>) -> Vec<Tile> {
    let now = data.now();
    let mut out = vec![
        Tile {
            kind: TileKind::Guide,
            label: msg::nav_epg().into(),
            channel: None,
            progress: None,
        },
        Tile {
            kind: TileKind::History,
            label: msg::channel_history().into(),
            channel: None,
            progress: None,
        },
    ];
    let recents = data
        .recent_channels()
        .into_iter()
        .filter(|k| Some(*k) != playing)
        .filter_map(|k| data.channel(k))
        .take(RECENT_TILES);
    for c in recents {
        let (on_air, _) = data.now_next(c.key, now);
        let progress = on_air.as_ref().map(|p| time::progress(p, now));
        out.push(Tile {
            kind: TileKind::Channel(c.key),
            label: on_air.map_or_else(|| c.name.clone(), |p| p.title),
            channel: Some(c),
            progress,
        });
    }
    out
}

/// The programme card (long Right): one programme of the channel playing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Card {
    pub channel: String,
    pub title: String,
    pub times: String,
    pub duration: String,
    /// Set while the programme is on air.
    pub progress: Option<f32>,
    pub category: Option<String>,
    pub description: Option<String>,
}

impl Card {
    pub fn build(ctx: &Context, p: &Programme) -> Self {
        let channel = match (&ctx.channel, ctx.number) {
            (Some(c), Some(n)) => format!("{n}  {}", c.name),
            (Some(c), None) => c.name.clone(),
            _ => String::new(),
        };
        Card {
            channel,
            title: p.title.clone(),
            times: time::programme_span(p, ctx.now, ctx.utc_offset_secs),
            duration: time::minutes_label((p.duration_secs() + 59) / 60),
            progress: p.is_on_air(ctx.now).then(|| time::progress(p, ctx.now)),
            category: p.category.clone(),
            description: p.description.clone(),
        }
    }
}
