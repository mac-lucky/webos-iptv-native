//! The programme grid as a state machine: which channel and programme has focus, the time
//! window and the rows read for it, what the preview shows, and Manage favorites mode. No drawing
//! here; every method is input in, state change (and commands) out, so the whole remote
//! behaviour is host-tested.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use iptv::model::{Channel, ChannelKey, Programme, Timestamp};

use super::clock;
use super::geom;
use crate::data::{Command, GridProgramme, Group, Playback, Stream, TvData};
use crate::lists::Cursor;
use crate::time::{self, DAY, MINUTE};

/// Rows read beyond each edge of the visible ones, so a scroll animation has rows to show.
const SPARE_ROWS: usize = 2;

/// Where focus sits in time. Kept across Up/Down so the focused column does not drift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// On air now, following the clock.
    Live,
    /// The programme that holds this instant (the start of the last programme focused by a
    /// horizontal move).
    At(Timestamp),
}

/// What the preview rectangle plays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shown {
    Live(ChannelKey),
    Catchup {
        channel: ChannelKey,
        start: Timestamp,
    },
}

impl Shown {
    fn of(channel: ChannelKey, catchup: Option<&Programme>) -> Shown {
        match catchup {
            Some(p) => Shown::Catchup {
                channel,
                start: p.start,
            },
            None => Shown::Live(channel),
        }
    }

    pub fn channel(&self) -> ChannelKey {
        match self {
            Shown::Live(channel) | Shown::Catchup { channel, .. } => *channel,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Browse,
    /// OK stars and unstars channels; Back leaves the mode.
    ManageFavorites,
}

/// A channel's programmes in the window: one of the rows around the scroll position.
pub struct Row {
    /// Where the channel is in [`Grid::channels`].
    pub index: usize,
    pub channel: Rc<Channel>,
    pub window: Timestamp,
    pub programmes: Vec<GridProgramme>,
    pub favorite: bool,
}

pub struct Grid {
    pub channels: Rc<[Rc<Channel>]>,
    /// The focused row and the first visible one (the scroll target).
    pub cursor: Cursor,
    pub anchor: Anchor,
    /// The focused programme; `None` on a row with no guide data at the anchor.
    pub focus: Option<GridProgramme>,
    /// Every field of the focused programme, read once it has focus.
    details: Option<(ChannelKey, Programme)>,
    /// Start of the visible time window, on a local half hour.
    pub window: Timestamp,
    pub mode: Mode,
    /// What the preview plays: what the guide asked for last, until playback reports otherwise.
    pub shown: Option<Shown>,
    /// What played when the guide opened; leaving for the player goes back to it.
    pub original: Playback,
    /// Favourite toggles sent but not yet seen through [`TvData`].
    flips: HashMap<ChannelKey, bool>,
    /// The rows around the scroll position, in list order, read for the window.
    pub rows: Vec<Row>,
    pub now: Timestamp,
    /// Local time minus UTC, seconds.
    pub offset: i32,
    /// The programmes with a reminder set (channel, start): their cells carry a bell.
    pub reminders: HashSet<(ChannelKey, Timestamp)>,
}

impl Grid {
    pub fn new(channels: Rc<[Rc<Channel>]>) -> Self {
        Grid {
            channels,
            cursor: Cursor::default(),
            anchor: Anchor::Live,
            focus: None,
            details: None,
            window: 0,
            mode: Mode::Browse,
            shown: None,
            original: Playback::default(),
            flips: HashMap::new(),
            rows: Vec::new(),
            now: 0,
            offset: 0,
            reminders: HashSet::new(),
        }
    }

    pub fn channel(&self) -> Option<&Rc<Channel>> {
        self.channels.get(self.cursor.index)
    }

    /// The focused row.
    pub fn row(&self) -> Option<&Row> {
        self.rows.iter().find(|r| r.index == self.cursor.index)
    }

    pub fn window_end(&self) -> Timestamp {
        self.window + geom::window_span()
    }

    fn live_window(&self) -> Timestamp {
        clock::floor_half_hour(self.now, self.offset)
    }

    fn anchor_time(&self) -> Timestamp {
        match self.anchor {
            Anchor::Live => self.now,
            Anchor::At(t) => t,
        }
    }

    pub fn at_live(&self) -> bool {
        self.anchor == Anchor::Live
    }

    /// Whether `channel` is a favourite, counting toggles not yet stored.
    fn is_favorite(&self, data: &dyn TvData, channel: ChannelKey) -> bool {
        favorite(&self.flips, data, channel)
    }

    /// The focused programme with every field, once read; `None` while nothing has focus.
    pub fn details(&self) -> Option<&Programme> {
        let (key, p) = self.details.as_ref()?;
        let focus = self.focus.as_ref()?;
        (Some(*key) == self.channel().map(|c| c.key) && p.start == focus.start).then_some(p)
    }

    /// [`details`](Self::details), or what the grid row knows of the focus before they are read.
    pub fn focused_programme(&self) -> Option<Programme> {
        self.details()
            .cloned()
            .or_else(|| self.focus.clone().map(Programme::from))
    }

    /// Read every field of the focused programme (its description) once it has focus.
    pub fn load_details(&mut self, data: &dyn TvData) {
        if self.details().is_some() {
            return;
        }
        self.details = match (self.channel(), &self.focus) {
            (Some(c), Some(p)) => {
                let full = data.programme_at(c.key, p.start);
                Some((c.key, full.unwrap_or_else(|| p.clone().into())))
            }
            _ => None,
        };
    }

    /// The now-line's pixel column and the clock's minute: what a tick redraws when it moves.
    pub fn clock_face(&self) -> (Option<i32>, i64) {
        (
            geom::now_column(self.now, self.window),
            self.now.div_euclid(MINUTE),
        )
    }

    /// The guide opens on the playing channel: at live, or on the archived programme playing
    /// (Back then returns to live).
    pub fn open(&mut self, data: &dyn TvData) {
        self.read_clock(data);
        self.read_reminders(data);
        self.mode = Mode::Browse;
        self.flips.clear();
        self.read_playback(data);
        let playback = data.playback();
        let playing = playback
            .channel
            .and_then(|k| self.channels.iter().position(|c| c.key == k));
        self.focus_row(playing.unwrap_or(self.cursor.index));
        self.anchor = Anchor::Live;
        self.window = self.live_window();
        match (playing, &playback.catchup) {
            (Some(_), Some(p)) => self.focus_on(data, p.clone().into()),
            _ => self.resolve(data),
        }
        self.original = playback;
    }

    /// Replace the rows (a group change), keeping focus on the focused channel when it is still
    /// listed and on the first row otherwise.
    pub fn set_channels(&mut self, data: &dyn TvData, channels: Rc<[Rc<Channel>]>) {
        self.replace(channels);
        self.resolve(data);
    }

    /// The stored data changed (playlist, favourites, guide, playback): drop optimistic state and
    /// everything read, and read again.
    pub fn refresh(&mut self, data: &dyn TvData, channels: Rc<[Rc<Channel>]>) {
        self.flips.clear();
        self.read_reminders(data);
        self.rows.clear();
        self.details = None;
        self.read_clock(data);
        if self.at_live() {
            self.window = self.live_window();
        }
        self.replace(channels);
        self.resolve(data);
    }

    fn read_reminders(&mut self, data: &dyn TvData) {
        let list = data.reminders().into_iter();
        self.reminders = list.map(|r| (r.channel, r.start)).collect();
    }

    fn replace(&mut self, channels: Rc<[Rc<Channel>]>) {
        let keep = self.channel().map(|c| c.key);
        self.channels = channels;
        let row = keep
            .and_then(|k| self.channels.iter().position(|c| c.key == k))
            .unwrap_or(0);
        self.focus_row(row);
    }

    /// Focus `row` with a row of context above it where the list allows.
    fn focus_row(&mut self, row: usize) {
        self.cursor.top = row.saturating_sub(1);
        self.cursor.set(row, self.channels.len(), geom::ROWS);
    }

    /// Time passed. Returns whether anything the guide shows changed beyond the clock itself
    /// (the window moved, or the live programme ended).
    pub fn tick(&mut self, data: &dyn TvData) -> bool {
        self.read_clock(data);
        if !self.at_live() {
            return false;
        }
        let window = self.live_window();
        let ended = self.focus.as_ref().is_some_and(|p| p.stop <= self.now);
        if window == self.window && !ended {
            return false;
        }
        self.window = window;
        self.resolve(data);
        true
    }

    /// What plays, as playback reports it: a screen over the guide may have started something.
    /// Returns whether the preview changed.
    pub fn read_playback(&mut self, data: &dyn TvData) -> bool {
        let playback = data.playback();
        let shown = playback
            .channel
            .map(|channel| Shown::of(channel, playback.catchup.as_ref()));
        let changed = shown != self.shown;
        self.shown = shown;
        changed
    }

    /// The time and the UTC offset (which a daylight-saving change moves).
    fn read_clock(&mut self, data: &dyn TvData) {
        self.now = data.now();
        self.offset = data.utc_offset_secs();
    }

    fn go_live(&mut self, data: &dyn TvData) {
        self.anchor = Anchor::Live;
        self.window = self.live_window();
        self.resolve(data);
    }

    /// Read the rows, then focus the programme at the anchor on the focused row: the one holding
    /// the anchor, else the next in the window, else the last before it.
    fn resolve(&mut self, data: &dyn TvData) {
        self.load_rows(data);
        let t = self.anchor_time();
        self.focus = self.row().and_then(|r| {
            let list = &r.programmes;
            list.iter()
                .find(|p| p.start <= t && t < p.stop)
                .or_else(|| list.iter().find(|p| p.start > t))
                .or_else(|| list.iter().rev().find(|p| p.stop <= t))
                .cloned()
        });
    }

    /// Bring [`rows`](Self::rows) in line with the scroll position and the window: rows already
    /// read stay, the missing ones are read together, and every row's star is looked up again.
    fn load_rows(&mut self, data: &dyn TvData) {
        let top = self.cursor.top;
        let wanted = top.saturating_sub(SPARE_ROWS)
            ..(top + geom::VISIBLE_ROWS + SPARE_ROWS).min(self.channels.len());
        let mut old = std::mem::take(&mut self.rows);
        let mut missing = Vec::new();
        for index in wanted {
            let channel = &self.channels[index];
            let kept = old
                .iter()
                .position(|r| r.channel.key == channel.key && r.window == self.window);
            let row = match kept {
                Some(i) => old.swap_remove(i),
                None => {
                    missing.push(self.rows.len());
                    Row {
                        index,
                        channel: Rc::clone(channel),
                        window: self.window,
                        programmes: Vec::new(),
                        favorite: false,
                    }
                }
            };
            self.rows.push(Row { index, ..row });
        }
        if !missing.is_empty() {
            let keys: Vec<ChannelKey> = missing.iter().map(|&i| self.rows[i].channel.key).collect();
            let read = data.guide_window(&keys, self.window, self.window_end());
            for (i, programmes) in missing.into_iter().zip(read) {
                self.rows[i].programmes = programmes;
            }
        }
        for row in &mut self.rows {
            row.favorite = favorite(&self.flips, data, row.channel.key);
        }
    }

    /// Up/Down: the next row with wrap-around, the anchor kept.
    pub fn vertical(&mut self, data: &dyn TvData, down: bool) {
        let by = if down { 1 } else { -1 };
        self.cursor.step(by, self.channels.len(), geom::ROWS, true);
        self.resolve(data);
    }

    /// Focus `channel`'s row, keeping the time as Up and Down keep it. Returns whether the rows
    /// list it.
    pub fn focus_channel(&mut self, data: &dyn TvData, channel: ChannelKey) -> bool {
        let Some(row) = self.channels.iter().position(|c| c.key == channel) else {
            return false;
        };
        self.focus_row(row);
        self.resolve(data);
        true
    }

    /// Move focus by `rows` (a page) without wrapping.
    pub fn page(&mut self, data: &dyn TvData, rows: isize) {
        self.cursor
            .step(rows, self.channels.len(), geom::ROWS, false);
        self.resolve(data);
    }

    /// The pointer on row `index` (one whole on screen), on the programme starting at `start`
    /// when it rests on a cell: focus there as the arrows would, without scrolling the rows or
    /// moving the window. On the channel column the time is kept, as Up and Down keep it.
    /// Returns whether the focus moved.
    pub fn point(&mut self, data: &dyn TvData, index: usize, start: Option<Timestamp>) -> bool {
        let before = (self.cursor.index, self.focus.as_ref().map(|p| p.start));
        if index != before.0 && !self.cursor.point(index, self.channels.len(), geom::ROWS) {
            return false;
        }
        let cell = start.and_then(|s| {
            let row = self.rows.iter().find(|r| r.index == index)?;
            row.programmes.iter().find(|p| p.start == s).cloned()
        });
        match cell {
            Some(p) => {
                // Live only where Back would not move the window back to live.
                let live = p.is_on_air(self.now) && self.window == self.live_window();
                self.anchor = if live {
                    Anchor::Live
                } else {
                    Anchor::At(p.start)
                };
                self.focus = Some(p);
            }
            None if index != before.0 => self.resolve(data),
            None => {}
        }
        (self.cursor.index, self.focus.as_ref().map(|p| p.start)) != before
    }

    /// Whether `p` on the focused row has ended and can be played from the archive. A programme
    /// still on air plays live here; watching it from the start is the context menu's.
    fn in_archive(&self, p: &GridProgramme) -> bool {
        let times = Programme {
            start: p.start,
            stop: p.stop,
            ..Programme::default()
        };
        p.stop <= self.now
            && self
                .channel()
                .is_some_and(|c| iptv::catchup::in_archive(c, &times, self.now))
    }

    fn focus_on(&mut self, data: &dyn TvData, p: GridProgramme) {
        if p.is_on_air(self.now) {
            self.anchor = Anchor::Live;
            self.window = self.live_window();
        } else {
            self.anchor = Anchor::At(p.start);
            self.show_start(p.start);
        }
        self.focus = Some(p);
        self.load_rows(data);
    }

    /// Page the window by half hours so a programme starting at `start` shows its start and at
    /// least [`geom::WINDOW_TAIL`] of itself.
    fn show_start(&mut self, start: Timestamp) {
        let last = geom::window_span() - geom::WINDOW_TAIL;
        if start < self.window {
            self.window = clock::floor_half_hour(start, self.offset);
        } else if start > self.window + last {
            self.window = clock::ceil_half_hour(start - last, self.offset);
        }
    }

    /// The first programme of the focused row starting at or after `t`: from the row as read
    /// when the window holds it, else read ahead, a window's span before a whole day.
    fn next_after(&self, data: &dyn TvData, t: Timestamp) -> Option<GridProgramme> {
        let row = self.row()?;
        let first = |list: &[GridProgramme]| list.iter().find(|p| p.start >= t).cloned();
        let read = (t >= self.window).then(|| first(&row.programmes)).flatten();
        read.or_else(|| {
            [geom::window_span(), DAY]
                .into_iter()
                .find_map(|ahead| first(&data.grid_programmes(row.channel.key, t, t + ahead)))
        })
    }

    /// The last programme of the focused row ending at or before `t`: from the row as read when
    /// it starts inside the window, else read back, a window's span before a whole day.
    fn prev_before(&self, data: &dyn TvData, t: Timestamp) -> Option<GridProgramme> {
        let row = self.row()?;
        let last = |list: &[GridProgramme]| list.iter().rev().find(|p| p.stop <= t).cloned();
        let read = last(&row.programmes).filter(|p| p.start >= self.window);
        read.or_else(|| {
            [geom::window_span(), DAY]
                .into_iter()
                .find_map(|back| last(&data.grid_programmes(row.channel.key, t - back, t)))
        })
    }

    /// Right: the next programme.
    pub fn right(&mut self, data: &dyn TvData) {
        let from = match &self.focus {
            Some(p) => p.stop,
            None => self.anchor_time(),
        };
        if let Some(p) = self.next_after(data, from) {
            self.focus_on(data, p);
        }
    }

    /// Left: the previous programme on every channel, catch-up or not, back to the start of the
    /// day [`TvData::past_days`] before today; from the future with nothing before it, live.
    pub fn left(&mut self, data: &dyn TvData) {
        let reference = match &self.focus {
            Some(p) => p.start,
            None => self.anchor_time(),
        };
        let limit = time::day_start(self.now, self.offset) - i64::from(data.past_days()) * DAY;
        match self.prev_before(data, reference) {
            Some(prev) if prev.stop > limit => self.focus_on(data, prev),
            _ if reference > self.now => self.go_live(data),
            _ => {}
        }
    }

    /// The focused programme has ended and the channel's archive does not reach it: it shows
    /// its details but does not play. The archive's edge moves with the clock, so one the
    /// preview already plays from it still counts as playable.
    pub fn out_of_archive(&self) -> bool {
        let (Some(p), Some(c)) = (&self.focus, self.channel()) else {
            return false;
        };
        let previewed = self.shown
            == Some(Shown::Catchup {
                channel: c.key,
                start: p.start,
            });
        p.stop <= self.now && !previewed && !self.in_archive(p)
    }

    /// OK: the first press previews the focused channel (or archived programme) in the
    /// preview rectangle, a press on what the preview already plays (live, or the focused
    /// programme from the archive, also one on air watched from its start) plays it full screen.
    /// Returns the command and whether the guide should close. `group` is the one the guide
    /// shows: the player zaps through it from then on.
    pub fn ok(&mut self, data: &dyn TvData, group: &Group) -> (Option<Command>, bool) {
        let Some(key) = self.channel().map(|c| c.key) else {
            return (None, false);
        };
        if self.mode == Mode::ManageFavorites {
            let on = !self.is_favorite(data, key);
            self.flips.insert(key, on);
            self.load_rows(data);
            return (Some(Command::ToggleFavorite(key)), false);
        }
        let archived = self.focus.as_ref().is_some_and(|p| self.in_archive(p));
        let focused = self.focus.as_ref().map(|p| p.start);
        let play = |stream| Command::Play {
            stream,
            group: Some(group.clone()),
        };
        match &self.shown {
            // Live is asked for again (it keeps playing) so the player zaps through this group.
            Some(Shown::Live(c)) if *c == key && !archived => {
                return (Some(play(Stream::Live(key))), true)
            }
            // The archive plays on where it is.
            Some(Shown::Catchup { channel, start })
                if *channel == key && focused == Some(*start) =>
            {
                return (None, true)
            }
            _ => {}
        }
        let catchup = self.focused_programme().filter(|_| archived);
        self.shown = Some(Shown::of(key, catchup.as_ref()));
        let stream = match catchup {
            Some(programme) => Stream::Catchup {
                channel: key,
                programme,
                offset_secs: 0,
            },
            None => Stream::Live(key),
        };
        (Some(play(stream)), false)
    }

    /// Back: leaves Manage favorites, else returns to live, else (already live) hands over to
    /// the groups panel (`true`).
    pub fn back(&mut self, data: &dyn TvData) -> bool {
        if self.mode == Mode::ManageFavorites {
            self.mode = Mode::Browse;
            return false;
        }
        if !self.at_live() || self.window != self.live_window() {
            self.go_live(data);
            return false;
        }
        true
    }

    /// Leaving for the player: whatever played when the guide opened, the player zapping
    /// through `group` (the one it had then) again. Live is asked for even while it still
    /// plays, which only gives the group back; a programme still playing from the archive
    /// plays on.
    pub fn restore(&self, group: &Group) -> Option<Command> {
        let original = &self.original;
        let channel = original.channel?;
        let stream = match &original.catchup {
            Some(p) if self.shown == Some(Shown::of(channel, Some(p))) => return None,
            Some(programme) => Stream::Catchup {
                channel,
                programme: programme.clone(),
                offset_secs: original.position_secs,
            },
            None => Stream::Live(channel),
        };
        Some(Command::Play {
            stream,
            group: Some(group.clone()),
        })
    }
}

/// Whether `channel` is a favourite, counting the toggles in `flips` not yet stored.
fn favorite(flips: &HashMap<ChannelKey, bool>, data: &dyn TvData, channel: ChannelKey) -> bool {
    flips
        .get(&channel)
        .copied()
        .unwrap_or_else(|| data.is_favorite(channel))
}
