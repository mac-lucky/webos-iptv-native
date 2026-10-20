//! The channel list: the overlay Left opens over full-screen video and the preview mode long Left
//! opens with the video moved into [`PREVIEW_RECT`].
//!
//! Three columns share the screen: the channels of one group, the programme list of the focused
//! channel (shown passively while the channels have focus, entered with Right) and the days
//! column (Right again). Moving through the days re-slices the programme list; OK on a past
//! programme inside the channel's archive window plays it as catch-up, OK on the one on air plays
//! the channel live. Left from the channels opens the groups panel ([`GroupsPanel`], the guide's)
//! beside them: moving its focus shows that group's channels, OK or Right goes back to them.
//!
//! What OK and Back do, by mode:
//! - overlay: OK plays ([`Command::Play`]) and closes; OK on what already plays only closes.
//! - preview: the video stays in [`PREVIEW_RECT`] while the list is open; OK plays the choice
//!   there; OK again on the same choice, or Back on the channels, closes, committing the live
//!   channel previewed when it is not the one the list opened on.
//! - either mode: with nothing playing, Back on the channels opens the guide ([`Navigate::Guide`])
//!   instead of closing onto an empty player.
//!
//! Only what is on screen is read: the programme on air of each channel row as it comes into
//! view or ends, and the focused channel's programme list once the focus rests on it.
//!
//! The pointer focuses the row it rests on in any column that does not move when it takes the
//! focus ([`Screen::point`]); a channel focused that way loads its programmes once the pointer
//! rests, as a held key does.

use std::ops::Range;
use std::rc::Rc;

use iptv::model::{Channel, ChannelKey, Programme, Timestamp};
use tv_machine::machine::Measure;
use tv_ui::Painter;

use super::cursor::Cursor;
use super::view::{self, ChannelListView};
use super::{Clock, Nav};
use crate::data::{
    Command, GridProgramme, Group, Logos, Navigate, Output, Rect, Screen, ScreenMessage, Stream,
    TvData,
};
use crate::groups::{Act, GroupsPanel};
use crate::input::{Hold, Key, Press};
use crate::point::Spot;
use crate::time::{self, When, DAY};

/// Where preview mode puts the video: top right, 16:9, on the 1920x1080 plane.
pub const PREVIEW_RECT: Rect = Rect {
    x: 1143,
    y: 72,
    w: 736,
    h: 414,
};

/// Rows on screen per column; the view lays out exactly these.
pub mod rows {
    pub const CHANNELS: usize = 8;
    pub const PROGRAMMES: usize = 12;
    pub const DAYS: usize = 12;
}

/// The days column spans this many days around today (the guide keeps a week each way).
pub const DAYS_BACK: i32 = 7;
pub const DAYS_AHEAD: i32 = 7;

/// A channel focused by a held key loads its programme list once the key has rested this long,
/// longer than the gap between key repeats.
pub const SETTLE_MS: u64 = 250;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Overlay,
    Preview,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Column {
    /// The groups panel left of the channels.
    Groups,
    Channels,
    Programmes,
    Days,
}

/// What the video shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Showing {
    Nothing,
    Live(ChannelKey),
    Catchup(ChannelKey, Timestamp),
}

impl Showing {
    fn of(data: &dyn TvData) -> Self {
        let p = data.playback();
        match (p.channel, p.catchup) {
            (Some(c), Some(prog)) => Showing::Catchup(c, prog.start),
            (Some(c), None) => Showing::Live(c),
            (None, _) => Showing::Nothing,
        }
    }

    fn channel(self) -> Option<ChannelKey> {
        match self {
            Showing::Nothing => None,
            Showing::Live(c) | Showing::Catchup(c, _) => Some(c),
        }
    }
}

/// A visible channel row: the programme on air, read when the row came into view.
pub(crate) struct Row {
    channel: ChannelKey,
    pub on_air: Option<Rc<Programme>>,
    /// The row is stale from this instant: its programme ends, or the next one starts.
    until: Timestamp,
}

impl Row {
    fn load(data: &dyn TvData, channel: ChannelKey, now: Timestamp) -> Self {
        let (on_air, next) = data.now_next(channel, now);
        let until = match (&on_air, &next) {
            (Some(p), _) => p.stop,
            (None, Some(n)) => n.start,
            (None, None) => Timestamp::MAX,
        };
        Row {
            channel,
            on_air: on_air.map(Rc::new),
            until,
        }
    }
}

/// The programmes of the focused channel: today only while it is browsed passively, the whole
/// days window once the days column is wanted.
pub struct Epg {
    pub key: ChannelKey,
    loaded: Vec<GridProgramme>,
    window: bool,
    /// Day offsets the days column lists, oldest first (always holds 0).
    pub days: Vec<i32>,
    /// The selected day offset.
    pub day: i32,
    range: Range<usize>,
    pub cursor: Cursor,
    pub day_cursor: Cursor,
    /// Every field of the focused programme, read once the focus is on it.
    detail: Option<Programme>,
}

impl Epg {
    fn today(key: ChannelKey, data: &dyn TvData, now: Timestamp, off: i32) -> Self {
        let from = time::day_start(now, off);
        let loaded = data.grid_programmes(key, from, from + DAY);
        let mut epg = Epg {
            key,
            range: 0..loaded.len(),
            loaded,
            window: false,
            days: vec![0],
            day: 0,
            cursor: Cursor::default(),
            day_cursor: Cursor::default(),
            detail: None,
        };
        epg.focus_default(now);
        epg
    }

    /// Load the whole days window, keeping the selected day and programme.
    fn widen(&mut self, data: &dyn TvData, now: Timestamp, off: i32) {
        if self.window {
            return;
        }
        let keep = self.focused().map(|p| p.start);
        let today = time::day_start(now, off);
        let from = today - i64::from(DAYS_BACK) * DAY;
        let to = today + i64::from(DAYS_AHEAD + 1) * DAY;
        self.loaded = data.grid_programmes(self.key, from, to);
        self.window = true;
        let mut days: Vec<i32> = Vec::new();
        for p in &self.loaded {
            // A programme crossing midnight lists under both days.
            let first = time::day_offset(p.start.max(from), now, off);
            let last = time::day_offset((p.stop - 1).clamp(from, to - 1), now, off);
            for d in first..=last {
                if !days.contains(&d) {
                    days.push(d);
                }
            }
        }
        if !days.contains(&0) {
            days.push(0);
        }
        days.sort_unstable();
        self.days = days;
        self.select_day(self.day, now, off);
        if let Some(start) = keep {
            if let Some(i) = self.programmes().iter().position(|p| p.start == start) {
                self.cursor.set(i, self.range.len(), rows::PROGRAMMES);
            }
        }
    }

    /// The selected day's programmes.
    pub fn programmes(&self) -> &[GridProgramme] {
        &self.loaded[self.range.clone()]
    }

    pub fn focused(&self) -> Option<&GridProgramme> {
        self.programmes().get(self.cursor.index)
    }

    /// Every field of the focused programme, once `load_detail` read it.
    pub fn detail(&self) -> Option<&Programme> {
        let start = self.focused()?.start;
        self.detail.as_ref().filter(|d| d.start == start)
    }

    /// Read the focused programme's details when the focus moved; whether they changed.
    fn load_detail(&mut self, data: &dyn TvData) -> bool {
        let Some(p) = self.focused() else {
            return self.detail.take().is_some();
        };
        if self.detail.as_ref().is_some_and(|d| d.start == p.start) {
            return false;
        }
        let full = data.programme_at(self.key, p.start);
        self.detail = Some(full.unwrap_or_else(|| p.clone().into()));
        true
    }

    fn select_day(&mut self, day: i32, now: Timestamp, off: i32) {
        self.day = day;
        let from = time::day_start(now, off) + i64::from(day) * DAY;
        let to = from + DAY;
        let lo = self
            .loaded
            .iter()
            .position(|p| p.stop > from)
            .unwrap_or(self.loaded.len());
        let hi = self.loaded[lo..]
            .iter()
            .position(|p| p.start >= to)
            .map_or(self.loaded.len(), |i| lo + i);
        self.range = lo..hi;
        let at = self.days.iter().position(|&d| d == day).unwrap_or(0);
        self.day_cursor.set(at, self.days.len(), rows::DAYS);
        self.focus_default(now);
    }

    /// Today focuses the programme on air (or the next one); other days their first programme.
    fn focus_default(&mut self, now: Timestamp) {
        let list = self.programmes();
        let len = list.len();
        let i = if self.day == 0 {
            list.iter()
                .position(|p| p.stop > now)
                .unwrap_or(len.saturating_sub(1))
        } else {
            0
        };
        self.cursor = Cursor::default();
        self.cursor.set(i, len, rows::PROGRAMMES);
    }
}

/// What a key can change on screen, to tell a real change from a key that did nothing.
type Look = (
    Column,
    Cursor,
    Showing,
    Option<(ChannelKey, i32, Cursor, Cursor)>,
);

pub struct ChannelList {
    mode: Mode,
    column: Column,
    group: Group,
    channels: Rc<[Rc<Channel>]>,
    utc_offset: i32,
    clock: Clock,
    origin: Showing,
    showing: Showing,
    cursor: Cursor,
    /// The rows `rows_first..` on screen.
    rows: Vec<Row>,
    rows_first: usize,
    epg: Option<Epg>,
    /// While a held key moves the focus: when the focused channel's programmes load.
    epg_due: Option<u64>,
    /// The groups panel, while it is open.
    groups: Option<GroupsPanel>,
    /// Bumped on every visible change; the view lays out again when it moves.
    generation: u64,
    view: ChannelListView,
}

impl ChannelList {
    /// Open over the player on `group`, focused on the channel that plays (or the first).
    pub fn new(mode: Mode, group: Group, data: &dyn TvData) -> Self {
        let origin = Showing::of(data);
        let mut list = ChannelList {
            mode,
            column: Column::Channels,
            group: Group::All,
            channels: Rc::from(Vec::new()),
            utc_offset: data.utc_offset_secs(),
            clock: Clock::new(data.now()),
            origin,
            showing: origin,
            cursor: Cursor::default(),
            rows: Vec::new(),
            rows_first: 0,
            epg: None,
            epg_due: None,
            groups: None,
            generation: 0,
            view: ChannelListView::default(),
        };
        list.set_group(group, data);
        list
    }

    /// Show `group` in the channel column, which takes the focus.
    fn set_group(&mut self, group: Group, data: &dyn TvData) {
        self.column = Column::Channels;
        self.groups = None;
        self.show(group, data);
    }

    /// Show `group`, focused on the channel the video shows when it is in the group.
    fn show(&mut self, group: Group, data: &dyn TvData) {
        self.channels = data.channels(&group);
        self.group = group;
        let at = self.showing.channel().and_then(|k| self.index_of(k));
        self.cursor = Cursor::default();
        self.cursor
            .set(at.unwrap_or(0), self.channels.len(), rows::CHANNELS);
        self.reset(data);
    }

    fn index_of(&self, key: ChannelKey) -> Option<usize> {
        self.channels.iter().position(|c| c.key == key)
    }

    /// Drop what was read for the old channels and read what the view draws now.
    fn reset(&mut self, data: &dyn TvData) {
        self.rows.clear();
        self.epg = None;
        self.epg_due = None;
        self.generation += 1;
        self.load(data, 0);
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn column(&self) -> Column {
        self.column
    }

    pub fn group(&self) -> &Group {
        &self.group
    }

    pub fn channels(&self) -> &[Rc<Channel>] {
        &self.channels
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn focused_channel(&self) -> Option<&Rc<Channel>> {
        self.channels.get(self.cursor.index)
    }

    pub fn now(&self) -> Timestamp {
        self.clock.now
    }

    pub fn utc_offset(&self) -> i32 {
        self.utc_offset
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The programme column's state, once the focus rested on the channel.
    pub fn epg(&self) -> Option<&Epg> {
        self.epg.as_ref()
    }

    /// The groups panel, while it is open.
    pub fn groups(&self) -> Option<&GroupsPanel> {
        self.groups.as_ref()
    }

    /// The row of visible channel `index`.
    pub(crate) fn row(&self, index: usize) -> Option<&Row> {
        index
            .checked_sub(self.rows_first)
            .and_then(|i| self.rows.get(i))
            .filter(|r| self.channels.get(index).is_some_and(|c| c.key == r.channel))
    }

    /// The on-air programme of visible channel `index`.
    pub fn on_air(&self, index: usize) -> Option<&Programme> {
        self.row(index).and_then(|r| r.on_air.as_deref())
    }

    /// The channel the video shows live (the play mark).
    pub fn is_showing_live(&self, key: ChannelKey) -> bool {
        self.showing == Showing::Live(key)
    }

    /// The archived programme the video shows, as `(channel, start)`.
    pub fn showing_catchup(&self) -> Option<(ChannelKey, Timestamp)> {
        match self.showing {
            Showing::Catchup(c, s) => Some((c, s)),
            _ => None,
        }
    }

    /// The programme the details panel describes: the focused one in the programme or days
    /// column, otherwise the focused channel's programme on air; none while the groups panel
    /// has the focus.
    pub fn detail(&self) -> Option<&Programme> {
        match self.column {
            Column::Groups => None,
            Column::Channels => self.on_air(self.cursor.index),
            Column::Programmes | Column::Days => self.epg.as_ref().and_then(Epg::detail),
        }
    }

    /// Move the clock on, read on the shell's clock at `now_ms`: a new minute moves the progress
    /// bars and the preview clock.
    fn advance(&mut self, data: &dyn TvData, now_ms: u64) {
        let now = data.now();
        if now.div_euclid(60) != self.clock.now.div_euclid(60) {
            self.generation += 1;
        }
        self.clock.set(now, now_ms);
    }

    /// When what is drawn next changes: the next minute, or a row's programme ending before it.
    fn deadline(&self) -> Timestamp {
        let now = self.clock.now;
        let minute = (now.div_euclid(60) + 1) * 60;
        self.rows
            .iter()
            .map(|r| r.until)
            .fold(minute, Timestamp::min)
    }

    /// Load what the view draws: the on-air programme of each channel row that came into view or
    /// went stale, and the focused channel's programmes once the focus rests (`now_ms` on the
    /// shell's clock) and is not in the groups panel, which hides them.
    fn load(&mut self, data: &dyn TvData, now_ms: u64) {
        self.load_rows(data);
        if self.column == Column::Groups || self.epg_due.is_some_and(|due| now_ms < due) {
            return;
        }
        self.epg_due = None;
        self.ensure_epg(data, self.column == Column::Days);
        if self.column != Column::Channels {
            if let Some(e) = self.epg.as_mut() {
                if e.load_detail(data) {
                    self.generation += 1;
                }
            }
        }
    }

    /// Keep the rows still on screen and current; read the others.
    fn load_rows(&mut self, data: &dyn TvData) {
        let visible = self.cursor.visible(self.channels.len(), rows::CHANNELS);
        let now = self.clock.now;
        let first = std::mem::replace(&mut self.rows_first, visible.start);
        let mut old: Vec<Option<Row>> = std::mem::take(&mut self.rows)
            .into_iter()
            .map(Some)
            .collect();
        let mut read = false;
        self.rows = self.channels[visible]
            .iter()
            .enumerate()
            .map(|(k, c)| {
                let i = self.rows_first + k;
                let kept = i
                    .checked_sub(first)
                    .and_then(|j| old.get_mut(j))
                    .and_then(Option::take)
                    .filter(|r| r.channel == c.key && now < r.until);
                kept.unwrap_or_else(|| {
                    read = true;
                    Row::load(data, c.key, now)
                })
            })
            .collect();
        if read || first != self.rows_first {
            self.generation += 1;
        }
    }

    fn ensure_epg(&mut self, data: &dyn TvData, window: bool) {
        let Some(key) = self.focused_channel().map(|c| c.key) else {
            if self.epg.take().is_some() {
                self.generation += 1;
            }
            return;
        };
        let (now, off) = (self.clock.now, self.utc_offset);
        if self.epg.as_ref().is_none_or(|e| e.key != key) {
            self.epg = Some(Epg::today(key, data, now, off));
            self.generation += 1;
        }
        if window {
            if let Some(e) = self.epg.as_mut().filter(|e| !e.window) {
                e.widen(data, now, off);
                self.generation += 1;
            }
        }
    }

    fn look(&self) -> Look {
        let epg = self
            .epg
            .as_ref()
            .map(|e| (e.key, e.day, e.cursor, e.day_cursor));
        (self.column, self.cursor, self.showing, epg)
    }

    /// One key at `now_ms` on the shell's clock. A `held` key repeats: the channel it focuses
    /// loads its programmes once the key rests ([`SETTLE_MS`]).
    fn nav(&mut self, nav: Nav, held: bool, now_ms: u64, data: &dyn TvData) -> Output {
        let (before, generation) = (self.look(), self.generation);
        let settle = held.then(|| now_ms + SETTLE_MS);
        let out = match self.column {
            // Keys reach the groups panel whole, long presses too (`Screen::key`).
            Column::Groups => Output::default(),
            Column::Channels => self.key_channels(nav, settle, data),
            Column::Programmes => self.key_programmes(nav, data),
            Column::Days => self.key_days(nav, data),
        };
        self.load(data, now_ms);
        if self.look() != before {
            self.generation += 1;
        }
        Output {
            redraw: self.generation != generation,
            ..out
        }
    }

    /// Move the channel focus; its programmes load at once, or at `settle` on the shell's clock.
    fn move_channel(&mut self, delta: isize, wrap: bool, settle: Option<u64>) {
        let before = self.cursor.index;
        self.cursor
            .step(delta, self.channels.len(), rows::CHANNELS, wrap);
        if self.cursor.index != before {
            self.epg = None;
            self.epg_due = settle;
        }
    }

    fn key_channels(&mut self, key: Nav, settle: Option<u64>, data: &dyn TvData) -> Output {
        let page = rows::CHANNELS as isize;
        match key {
            Nav::Up => self.move_channel(-1, true, settle),
            Nav::Down => self.move_channel(1, true, settle),
            Nav::PageUp => self.move_channel(-page, false, settle),
            Nav::PageDown => self.move_channel(page, false, settle),
            Nav::Top => self.move_channel(-(self.cursor.index as isize), false, None),
            Nav::Left => {
                self.groups = Some(GroupsPanel::new(data.groups(), &self.group, view::GROUPS));
                self.epg_due = None;
                self.column = Column::Groups;
            }
            Nav::Right => {
                if !self.channels.is_empty() {
                    self.epg_due = None;
                    self.ensure_epg(data, false);
                    self.column = Column::Programmes;
                }
            }
            Nav::Ok => {
                if let Some(key) = self.focused_channel().map(|c| c.key) {
                    return self.choose(Showing::Live(key), None);
                }
            }
            // With nothing playing Back goes to the guide, as the guide's groups panel stays in
            // the guide (no empty player).
            Nav::Back if data.playback().channel.is_none() => {
                return Output::navigate(Navigate::Guide)
            }
            Nav::Back => return self.close(),
        }
        Output::default()
    }

    /// One key in the groups panel (`long` past the threshold): moving its focus shows that
    /// group here and, from now on, in the guide.
    fn key_groups(&mut self, key: Key, long: bool, data: &dyn TvData) -> Output {
        let Some(panel) = self.groups.as_mut() else {
            self.column = Column::Channels;
            return Output::default();
        };
        match panel.key(key, long) {
            Act::Moved => return self.panel_moved(data),
            // A locked group shows nothing until the PIN opens it.
            Act::Enter if data.group_locked(&self.group) => {
                return crate::parental::ask_to_open(&self.group)
            }
            // An empty group keeps the focus in the panel; Back always leaves it.
            Act::Enter if self.channels.is_empty() => {}
            Act::Enter | Act::Back => {
                self.groups = None;
                self.column = Column::Channels;
            }
            Act::Go(nav) => return Output::navigate(nav),
            Act::None => {}
        }
        Output::default()
    }

    /// The groups panel's focus moved: show its group here and, from now on, in the guide.
    fn panel_moved(&mut self, data: &dyn TvData) -> Output {
        self.generation += 1;
        let shown = self.groups.as_ref().and_then(GroupsPanel::shown);
        match shown.filter(|g| **g != self.group).cloned() {
            Some(group) => {
                self.show(group.clone(), data);
                Output::command(Command::ShowGroup(group))
            }
            None => Output::default(),
        }
    }

    /// The pointer on `spot` of the view: focus it as the keys would, without scrolling.
    fn point_at(&mut self, spot: Spot, now_ms: u64) {
        match spot.area {
            view::CHANNELS => {
                if self.column != Column::Channels {
                    self.leave_programmes();
                }
                if self
                    .cursor
                    .point(spot.index, self.channels.len(), rows::CHANNELS)
                {
                    self.epg = None;
                    self.epg_due = Some(now_ms + SETTLE_MS);
                }
            }
            view::PROGRAMMES => {
                let top = self.epg.as_ref().map(|e| view::programme_top(self, e));
                let Some((epg, top)) = self.epg.as_mut().zip(top) else {
                    return;
                };
                // The list stays where it showed while the channels had the focus.
                epg.cursor.top = top;
                epg.cursor
                    .point(spot.index, epg.programmes().len(), rows::PROGRAMMES);
                self.column = Column::Programmes;
            }
            view::DAYS => {
                let Some(epg) = self.epg.as_mut() else {
                    return;
                };
                let mut c = epg.day_cursor;
                if c.point(spot.index, epg.days.len(), rows::DAYS) {
                    epg.select_day(epg.days[c.index], self.clock.now, self.utc_offset);
                    epg.day_cursor.top = c.top;
                }
            }
            _ => {}
        }
    }

    fn key_programmes(&mut self, key: Nav, data: &dyn TvData) -> Output {
        self.ensure_epg(data, false);
        let Some(epg) = self.epg.as_mut() else {
            self.column = Column::Channels;
            return Output::default();
        };
        let len = epg.programmes().len();
        let page = rows::PROGRAMMES as isize;
        match key {
            Nav::Up => epg.cursor.step(-1, len, rows::PROGRAMMES, false),
            Nav::Down => epg.cursor.step(1, len, rows::PROGRAMMES, false),
            Nav::PageUp => epg.cursor.step(-page, len, rows::PROGRAMMES, false),
            Nav::PageDown => epg.cursor.step(page, len, rows::PROGRAMMES, false),
            Nav::Top => epg.cursor.set(0, len, rows::PROGRAMMES),
            Nav::Left | Nav::Back => self.leave_programmes(),
            Nav::Right => {
                self.ensure_epg(data, true);
                self.column = Column::Days;
            }
            Nav::Ok => return self.choose_programme(data),
        }
        Output::default()
    }

    /// Back to the channels; the passive list shows today again.
    fn leave_programmes(&mut self) {
        self.column = Column::Channels;
        let now = self.clock.now;
        if let Some(e) = self.epg.as_mut() {
            if e.day != 0 {
                e.select_day(0, now, self.utc_offset);
            } else {
                e.focus_default(now);
            }
        }
    }

    fn key_days(&mut self, key: Nav, data: &dyn TvData) -> Output {
        self.ensure_epg(data, true);
        let (now, off) = (self.clock.now, self.utc_offset);
        let Some(epg) = self.epg.as_mut() else {
            self.column = Column::Channels;
            return Output::default();
        };
        let step = |epg: &mut Epg, delta: isize| {
            let mut c = epg.day_cursor;
            c.step(delta, epg.days.len(), rows::DAYS, false);
            if let Some(&day) = epg.days.get(c.index) {
                if day != epg.day {
                    epg.select_day(day, now, off);
                }
            }
        };
        match key {
            Nav::Up => step(epg, -1),
            Nav::Down => step(epg, 1),
            Nav::PageUp => step(epg, -(rows::DAYS as isize)),
            Nav::PageDown => step(epg, rows::DAYS as isize),
            Nav::Top => {
                let at = epg.day_cursor.index as isize;
                step(epg, -at)
            }
            Nav::Left | Nav::Back | Nav::Ok | Nav::Right => self.column = Column::Programmes,
        }
        Output::default()
    }

    fn choose_programme(&mut self, data: &dyn TvData) -> Output {
        let Some(channel) = self.focused_channel().cloned() else {
            return Output::default();
        };
        let Some(epg) = self.epg.as_mut() else {
            return Output::default();
        };
        epg.load_detail(data);
        let Some(p) = epg.detail().cloned() else {
            return Output::default();
        };
        let now = self.clock.now;
        match time::when(p.start, p.stop, now) {
            When::OnAir => self.choose(Showing::Live(channel.key), None),
            When::Past if iptv::catchup::in_archive(&channel, &p, now) => {
                self.choose(Showing::Catchup(channel.key, p.start), Some(p))
            }
            When::Past | When::Future => Output::default(),
        }
    }

    /// OK on `target` (with its programme when it is catch-up).
    fn choose(&mut self, target: Showing, programme: Option<Programme>) -> Output {
        if self.showing == target {
            return self.close();
        }
        let stream = match (target, programme) {
            (Showing::Catchup(channel, _), Some(programme)) => Stream::Catchup {
                channel,
                programme,
                offset_secs: 0,
            },
            (Showing::Live(key), _) => Stream::Live(key),
            _ => return Output::default(),
        };
        self.showing = target;
        let play = self.play(stream);
        match self.mode {
            Mode::Overlay => play.and(Output::close()),
            Mode::Preview => play,
        }
    }

    fn play(&self, stream: Stream) -> Output {
        Output::command(Command::Play {
            stream,
            group: Some(self.group.clone()),
        })
    }

    /// Close the list; preview mode commits the live channel it previewed, which goes full screen
    /// with the list gone.
    fn close(&self) -> Output {
        match self.showing {
            Showing::Live(key) if self.mode == Mode::Preview && self.showing != self.origin => {
                self.play(Stream::Live(key)).and(Output::close())
            }
            _ => Output::close(),
        }
    }
}

impl Screen for ChannelList {
    fn hold(&self, key: Key) -> Hold {
        match self.column {
            Column::Groups => GroupsPanel::hold(key),
            _ => super::hold(key),
        }
    }

    fn key(&mut self, key: Key, press: Press, data: &dyn TvData, now_ms: u64) -> Output {
        let generation = self.generation;
        self.advance(data, now_ms);
        let out = if self.column == Column::Groups {
            let before = self.look();
            let out = self.key_groups(key, press == Press::Long, data);
            self.load(data, now_ms);
            if self.look() != before {
                self.generation += 1;
            }
            out
        } else if let Some(nav) = Nav::of(key, press) {
            self.nav(nav, press == Press::Repeat, now_ms, data)
        } else {
            self.load(data, now_ms);
            Output::default()
        };
        Output {
            redraw: self.generation != generation,
            ..out
        }
    }

    /// The channels beside the open groups panel, the programmes left of the days in preview
    /// mode and the groups panel's cut row.
    fn dead(&self, x: f32, y: f32) -> bool {
        self.view.dead(x, y)
            || (self.column == Column::Groups && self.groups.as_ref().is_some_and(|g| g.dead(x, y)))
    }

    fn point(&mut self, x: f32, y: f32, data: &dyn TvData, now_ms: u64) -> Output {
        let (before, generation) = (self.look(), self.generation);
        let out = if self.column == Column::Groups {
            match self.groups.as_mut().map(|g| g.point(x, y)) {
                Some(Act::Moved) => self.panel_moved(data),
                _ => return Output::default(),
            }
        } else {
            let Some(spot) = self.view.spot(x, y) else {
                return Output::default();
            };
            self.point_at(spot, now_ms);
            Output::default()
        };
        if self.look() == before && self.generation == generation {
            return out;
        }
        self.load(data, now_ms);
        self.generation += 1;
        Output {
            redraw: true,
            ..out
        }
    }

    fn tick(&mut self, data: &dyn TvData, now_ms: u64) -> Output {
        let generation = self.generation;
        // What really plays: a pick the shell held back for the parental PIN did not, and Back
        // must not commit it.
        let showing = Showing::of(data);
        if showing != self.showing {
            self.showing = showing;
            self.generation += 1;
        }
        self.advance(data, now_ms);
        self.load(data, now_ms);
        Output {
            redraw: self.generation != generation,
            ..Output::default()
        }
    }

    fn next_wake(&self, now_ms: u64) -> Option<u64> {
        let wake = self.clock.wake(now_ms, self.deadline());
        Some(self.epg_due.map_or(wake, |due| due.min(wake)))
    }

    fn video_rect(&self) -> Option<Rect> {
        (self.mode == Mode::Preview).then_some(PREVIEW_RECT)
    }

    fn refresh(&mut self, data: &dyn TvData) {
        let focused = self.focused_channel().map(|c| c.key);
        let epg = self
            .epg
            .as_ref()
            .map(|e| (e.window, e.day, e.focused().map(|p| p.start)));
        if let Some(panel) = self.groups.as_mut() {
            panel.set(data.groups(), &self.group);
            // A group the playlist lost: the one the panel falls back to.
            if let Some(g) = panel.shown().filter(|g| **g != self.group) {
                self.group = g.clone();
            }
        }
        self.channels = data.channels(&self.group);
        self.utc_offset = data.utc_offset_secs();
        let at = focused.and_then(|k| self.index_of(k));
        if at.is_none() && self.column != Column::Groups {
            self.column = Column::Channels;
        }
        self.cursor
            .set(at.unwrap_or(0), self.channels.len(), rows::CHANNELS);
        self.reset(data);
        // The same channel: its programme list keeps the day and the programme.
        let (now, off) = (self.clock.now, self.utc_offset);
        let kept = epg.filter(|_| at.is_some());
        if let (Some(e), Some((window, day, start))) = (self.epg.as_mut(), kept) {
            if window {
                e.widen(data, now, off);
            }
            if day != e.day && e.days.contains(&day) {
                e.select_day(day, now, off);
            }
            if let Some(i) = start.and_then(|s| e.programmes().iter().position(|p| p.start == s)) {
                e.cursor.set(i, e.programmes().len(), rows::PROGRAMMES);
            }
            self.load(data, 0);
        }
    }

    fn message(&mut self, message: ScreenMessage, data: &dyn TvData) {
        if let ScreenMessage::SetGroup(group) = message {
            self.set_group(group, data);
        }
    }

    fn draw(&mut self, p: Painter, m: &dyn Measure, logos: &dyn Logos) {
        let mut view = std::mem::take(&mut self.view);
        view.prepare(self, m);
        view.draw(p, logos);
        self.view = view;
        if let Some(panel) = self.groups.as_mut() {
            panel.prepare(m);
            panel.paint(p, logos);
        }
    }
}

#[cfg(test)]
impl ChannelList {
    /// What the last [`Screen::draw`] laid out.
    pub(crate) fn pieces(&self) -> &[crate::draw::Piece] {
        self.view.pieces()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fake::FakeData;
    use crate::data::Playback;
    use crate::lists::key_of;
    use std::cell::RefCell;

    // 2024-08-27 15:12:00 UTC.
    const NOW: Timestamp = 1_724_771_520;

    fn fake(count: usize) -> FakeData {
        FakeData::new(NOW, count)
    }

    fn playing(d: &mut FakeData, i: usize) {
        d.playback = Playback {
            channel: Some(d.channels[i].key),
            ..Playback::default()
        };
    }

    fn open(d: &FakeData, mode: Mode) -> ChannelList {
        ChannelList::new(mode, Group::All, d)
    }

    /// Short presses of `keys`, their outputs chained.
    fn press(l: &mut ChannelList, d: &FakeData, keys: &[Nav]) -> Output {
        keys.iter().fold(Output::default(), |out, &nav| {
            let (key, press) = key_of(nav);
            out.and(l.key(key, press, d, 0))
        })
    }

    fn live(d: &FakeData, i: usize) -> Command {
        Command::Play {
            stream: Stream::Live(d.channels[i].key),
            group: Some(Group::All),
        }
    }

    #[test]
    fn opens_on_the_playing_channel() {
        let mut d = fake(30);
        playing(&mut d, 20);
        let l = open(&d, Mode::Overlay);
        assert_eq!(
            l.video_rect(),
            None,
            "the overlay lies over full-screen video"
        );
        assert_eq!(l.cursor().index, 20);
        assert!(l.cursor().visible(30, rows::CHANNELS).contains(&20));
        assert!(l.is_showing_live(d.channels[20].key));
        assert_eq!(l.column(), Column::Channels);
    }

    #[test]
    fn rows_carry_the_programme_on_air() {
        let d = fake(30);
        let mut l = open(&d, Mode::Overlay);
        for i in l.cursor().visible(30, rows::CHANNELS) {
            let p = l.on_air(i).expect("the fake guide covers now");
            assert!(p.is_on_air(NOW));
        }
        assert!(
            l.on_air(rows::CHANNELS + 1).is_none(),
            "off-screen rows are not loaded"
        );
        // A tick reads the shell's clock; the next is due within the minute.
        assert_eq!(l.next_wake(70), Some(70));
        assert_eq!(l.tick(&d, 1_000), Output::default());
        let wake = l.next_wake(5_000).unwrap();
        assert!(wake > 1_000 && wake <= 61_000, "{wake}");
    }

    #[test]
    fn overlay_ok_plays_and_closes() {
        let mut d = fake(10);
        playing(&mut d, 0);
        let mut l = open(&d, Mode::Overlay);
        let out = press(&mut l, &d, &[Nav::Down, Nav::Down, Nav::Ok]);
        assert_eq!(out.commands, [live(&d, 2)]);
        assert_eq!(out.navigate, [Navigate::Close]);
    }

    #[test]
    fn overlay_ok_on_the_playing_channel_only_closes() {
        let mut d = fake(10);
        playing(&mut d, 3);
        let mut l = open(&d, Mode::Overlay);
        assert_eq!(press(&mut l, &d, &[Nav::Ok]), Output::close());
    }

    #[test]
    fn channels_wrap_and_page() {
        let d = fake(20);
        let mut l = open(&d, Mode::Overlay);
        press(&mut l, &d, &[Nav::Up]);
        assert_eq!(l.cursor().index, 19);
        press(&mut l, &d, &[Nav::Down]);
        assert_eq!(l.cursor().index, 0);
        press(&mut l, &d, &[Nav::PageDown, Nav::PageDown, Nav::PageDown]);
        assert_eq!(l.cursor().index, 19, "paging holds at the end");
        press(&mut l, &d, &[Nav::Top]);
        assert_eq!(l.cursor().index, 0);
    }

    #[test]
    fn preview_ok_previews_then_plays() {
        let mut d = fake(10);
        playing(&mut d, 0);
        let mut l = open(&d, Mode::Preview);
        assert_eq!(l.video_rect(), Some(PREVIEW_RECT));
        let out = press(&mut l, &d, &[Nav::Down, Nav::Ok]);
        assert_eq!((out.commands, out.navigate), (vec![live(&d, 1)], vec![]));
        assert!(l.is_showing_live(d.channels[1].key));
        let out = press(&mut l, &d, &[Nav::Ok]);
        assert_eq!(out.commands, [live(&d, 1)]);
        assert_eq!(out.navigate, [Navigate::Close]);
    }

    #[test]
    fn preview_back_keeps_what_the_rect_shows() {
        let mut d = fake(10);
        playing(&mut d, 0);
        let mut l = open(&d, Mode::Preview);
        // Nothing previewed: Back only closes, and the video goes full screen with the list.
        assert_eq!(press(&mut l, &d, &[Nav::Back]), Output::close());
        let mut l = open(&d, Mode::Preview);
        press(&mut l, &d, &[Nav::Down, Nav::Down, Nav::Ok]);
        let out = press(&mut l, &d, &[Nav::Up, Nav::Back]);
        assert_eq!(out.commands, [live(&d, 2)]);
        assert_eq!(out.navigate, [Navigate::Close]);
    }

    /// With nothing playing Back on the channels opens the guide instead of closing onto an
    /// empty player; once a preview plays, Back closes onto it.
    #[test]
    fn back_with_nothing_playing_opens_the_guide() {
        let mut d = fake(10);
        for mode in [Mode::Overlay, Mode::Preview] {
            let mut l = open(&d, mode);
            let out = press(&mut l, &d, &[Nav::Back]);
            assert_eq!(out, Output::navigate(Navigate::Guide), "{mode:?}");
        }
        let mut l = open(&d, Mode::Preview);
        press(&mut l, &d, &[Nav::Down, Nav::Ok]);
        playing(&mut d, 1);
        let out = press(&mut l, &d, &[Nav::Back]);
        assert_eq!(out.commands, [live(&d, 1)]);
        assert_eq!(out.navigate, [Navigate::Close]);
    }

    /// One key with a press, as the remote sends it.
    fn send(l: &mut ChannelList, d: &FakeData, key: Key, press: Press) -> Output {
        l.key(key, press, d, 0)
    }

    #[test]
    fn left_opens_the_groups_beside_the_list_which_shows_the_focused_group() {
        let mut d = fake(9);
        playing(&mut d, 4);
        let mut l = open(&d, Mode::Overlay);
        let out = press(&mut l, &d, &[Nav::Left]);
        assert!(out.navigate.is_empty() && out.commands.is_empty() && out.redraw);
        assert_eq!(l.column(), Column::Groups);
        let g = l.groups().expect("the panel is open");
        assert_eq!(g.focused(), Some(&Group::All), "on the list's group");
        assert_eq!(l.hold(Key::Ok), Hold::Long, "long OK is the group menu");
        assert_eq!(
            l.detail(),
            None,
            "no programme while the groups have the focus"
        );
        // History shows nothing: the list keeps All.
        let out = send(&mut l, &d, Key::Down, Press::Short);
        assert!(out.commands.is_empty() && out.redraw);
        assert_eq!((l.group(), l.channels().len()), (&Group::All, 9));
        // News: the list shows it at once and the app opens the guide on it too.
        let news = Group::Playlist("News".into());
        let out = send(&mut l, &d, Key::Down, Press::Repeat);
        assert_eq!(out.commands, [Command::ShowGroup(news.clone())]);
        assert_eq!((l.group(), l.channels().len()), (&news, 3));
        assert_eq!(l.column(), Column::Groups);
        assert!(l.epg().is_none(), "no programme list read beside the panel");
        // Sport holds channel 4, which plays: the cursor follows it into the group.
        send(&mut l, &d, Key::Down, Press::Short);
        assert_eq!(l.focused_channel().unwrap().key, d.channels[4].key);
        // Right goes back to the channels with the group applied.
        let out = send(&mut l, &d, Key::Right, Press::Short);
        assert!(out.commands.is_empty() && out.navigate.is_empty());
        assert_eq!(l.column(), Column::Channels);
        assert!(l.groups().is_none());
        assert_eq!(l.group().name(), "Sport");
        assert_eq!(l.epg().unwrap().key, d.channels[4].key);
        let out = press(&mut l, &d, &[Nav::Down, Nav::Ok]);
        let Command::Play { group, .. } = &out.commands[0] else {
            panic!("plays");
        };
        assert_eq!(group.as_ref(), Some(&Group::Playlist("Sport".into())));
    }

    #[test]
    fn the_groups_panel_keys_in_the_list() {
        let d = fake(9);
        let mut l = open(&d, Mode::Preview);
        press(&mut l, &d, &[Nav::Left]);
        // Favorites is empty: OK keeps the focus in the panel, Back always leaves it.
        let out = send(&mut l, &d, Key::Left, Press::Long);
        assert_eq!(out.commands, [Command::ShowGroup(Group::Favorites)]);
        assert!(l.channels().is_empty());
        send(&mut l, &d, Key::Ok, Press::Short);
        assert_eq!(l.column(), Column::Groups);
        let out = send(&mut l, &d, Key::Ok, Press::Long);
        assert_eq!(out.navigate, [Navigate::GroupMenu(Group::Favorites)]);
        let out = send(&mut l, &d, Key::Left, Press::Short);
        assert_eq!(out.navigate, [Navigate::MainMenu]);
        send(&mut l, &d, Key::Back, Press::Short);
        assert_eq!(l.column(), Column::Channels);
        // History (under All) is a screen of its own.
        press(&mut l, &d, &[Nav::Left, Nav::Down, Nav::Down]);
        assert_eq!(l.groups().unwrap().focused(), Some(&Group::History));
        let out = send(&mut l, &d, Key::Ok, Press::Short);
        assert_eq!(out.navigate, [Navigate::History]);
        assert_eq!(l.column(), Column::Groups);
        // Back to the channels, Back again leaves the list: for the guide, as nothing plays.
        send(&mut l, &d, Key::Back, Press::Short);
        assert_eq!(l.column(), Column::Channels);
        let out = send(&mut l, &d, Key::Back, Press::Short);
        assert_eq!(out.navigate, [Navigate::Guide]);
    }

    #[test]
    fn a_group_the_playlist_lost_falls_back_to_all_in_the_open_panel() {
        let mut d = fake(9);
        let mut l = open(&d, Mode::Overlay);
        press(&mut l, &d, &[Nav::Left]);
        // All channels, History, News, Sport.
        for _ in 0..3 {
            send(&mut l, &d, Key::Down, Press::Short);
        }
        assert_eq!(l.group().name(), "Sport");
        for c in &mut d.channels {
            c.groups = vec!["News".into()];
        }
        l.refresh(&d);
        assert_eq!(l.column(), Column::Groups);
        assert_eq!(l.group(), &Group::All);
        assert_eq!(l.channels().len(), 9);
        assert_eq!(l.groups().unwrap().focused(), Some(&Group::All));
    }

    #[test]
    fn a_group_chosen_elsewhere_sets_the_list() {
        let mut d = fake(9);
        playing(&mut d, 4);
        let mut l = open(&d, Mode::Overlay);
        // Channel 4 is in "Sport" (4 % 3 == 1): the cursor follows it into the group.
        l.message(ScreenMessage::SetGroup(Group::Playlist("Sport".into())), &d);
        assert_eq!(l.channels().len(), 3);
        assert_eq!(l.focused_channel().unwrap().key, d.channels[4].key);
        assert_eq!(l.group().name(), "Sport");
        assert_eq!(l.epg().unwrap().key, d.channels[4].key);
        l.message(ScreenMessage::SetGroup(Group::Favorites), &d);
        assert!(l.channels().is_empty());
        assert!(l.epg().is_none());
        let out = press(&mut l, &d, &[Nav::Down, Nav::Right, Nav::Ok]);
        assert!(out.commands.is_empty() && out.navigate.is_empty());
        assert_eq!(l.column(), Column::Channels);
        // Picking a channel plays it in the group it was picked from.
        l.message(ScreenMessage::SetGroup(Group::Playlist("News".into())), &d);
        let out = press(&mut l, &d, &[Nav::Ok]);
        let Command::Play { group, .. } = &out.commands[0] else {
            panic!("plays");
        };
        assert_eq!(group.as_ref(), Some(&Group::Playlist("News".into())));
    }

    #[test]
    fn programme_list_follows_the_focused_channel() {
        let d = fake(10);
        let mut l = open(&d, Mode::Overlay);
        let first = l.epg().unwrap().key;
        assert_eq!(first, d.channels[0].key);
        let p = l.epg().unwrap().focused().unwrap();
        assert!(
            p.stop > NOW && p.start <= NOW,
            "today focuses the programme on air"
        );
        assert_eq!(l.detail(), l.on_air(0));
        press(&mut l, &d, &[Nav::Down]);
        assert_eq!(l.epg().unwrap().key, d.channels[1].key);
    }

    #[test]
    fn a_held_key_loads_the_programmes_once_it_rests() {
        let d = fake(10);
        let mut l = open(&d, Mode::Overlay);
        let out = l.key(Key::Down, Press::Repeat, &d, 1_000);
        assert!(out.redraw);
        assert!(l.epg().is_none(), "not while the key repeats");
        assert_eq!(l.next_wake(1_000), Some(1_000 + SETTLE_MS));
        l.key(Key::Down, Press::Repeat, &d, 1_100);
        assert_eq!(l.next_wake(1_100), Some(1_100 + SETTLE_MS));
        assert_eq!(l.tick(&d, 1_200), Output::default());
        assert!(l.epg().is_none());
        assert!(l.tick(&d, 1_100 + SETTLE_MS).redraw);
        assert_eq!(l.epg().unwrap().key, d.channels[2].key);
        assert!(l.next_wake(1_400).unwrap() > 1_400 + SETTLE_MS);
        // Right does not wait.
        l.key(Key::Down, Press::Repeat, &d, 2_000);
        press(&mut l, &d, &[Nav::Right]);
        assert_eq!(l.epg().unwrap().key, d.channels[3].key);
        assert_eq!(l.column(), Column::Programmes);
    }

    /// Hour-long programmes on the hour.
    fn hourly(from: Timestamp, to: Timestamp) -> Vec<Programme> {
        let first = from - from.rem_euclid(3600);
        (0..)
            .map(|k| first + k * 3600)
            .take_while(|&s| s < to)
            .map(|s| Programme {
                start: s,
                stop: s + 3600,
                title: format!("Hour {s}"),
                ..Programme::default()
            })
            .collect()
    }

    #[test]
    fn scrolling_by_one_reads_only_the_row_coming_into_view() {
        let reads = Rc::new(RefCell::new(Vec::new()));
        let log = reads.clone();
        let d = fake(30).with_schedule(move |i, from, to| {
            log.borrow_mut().push(i);
            hourly(from, to)
        });
        let mut l = open(&d, Mode::Overlay);
        assert_eq!(
            reads.take(),
            [0, 1, 2, 3, 4, 5, 6, 7, 0],
            "the rows, then the list"
        );
        for _ in 0..6 {
            l.key(Key::Down, Press::Repeat, &d, 0);
        }
        assert!(reads.take().is_empty(), "no row came into view");
        l.key(Key::Down, Press::Repeat, &d, 0);
        assert_eq!(l.cursor(), Cursor { index: 7, top: 1 });
        assert_eq!(reads.take(), [8]);
        l.tick(&d, SETTLE_MS);
        assert_eq!(
            reads.take(),
            [7],
            "the focused channel's day, once the key rests"
        );
        // A row whose programme ended is read again, alone.
        let mut d = d;
        d.now = NOW - NOW.rem_euclid(3600) + 3600;
        l.tick(&d, 2 * SETTLE_MS);
        assert_eq!(reads.take().len(), 8);
        l.tick(&d, 3 * SETTLE_MS);
        assert!(reads.take().is_empty());
    }

    #[test]
    fn a_key_that_changes_nothing_draws_nothing() {
        let d = fake(10);
        let mut l = open(&d, Mode::Overlay);
        let generation = l.generation();
        let out = l.key(Key::Red, Press::Short, &d, 0);
        assert_eq!(out, Output::default());
        press(&mut l, &d, &[Nav::Right, Nav::Top]);
        let generation_in_list = l.generation();
        assert!(generation_in_list > generation);
        let out = press(&mut l, &d, &[Nav::Up]);
        assert!(!out.redraw, "the top holds");
        assert_eq!(l.generation(), generation_in_list);
    }

    #[test]
    fn right_enters_programmes_then_days() {
        let d = fake(10);
        let mut l = open(&d, Mode::Overlay);
        press(&mut l, &d, &[Nav::Right]);
        assert_eq!(l.column(), Column::Programmes);
        let on_air = l.epg().unwrap().focused().unwrap().clone();
        let full = d.programme_at(d.channels[0].key, on_air.start).unwrap();
        assert!(full.description.is_some());
        assert_eq!(l.detail(), Some(&full), "the details read every field");
        press(&mut l, &d, &[Nav::Right]);
        assert_eq!(l.column(), Column::Days);
        let e = l.epg().unwrap();
        assert_eq!(e.days.first(), Some(&-DAYS_BACK));
        assert_eq!(e.days.last(), Some(&DAYS_AHEAD));
        assert_eq!(e.day, 0);
        assert_eq!(e.days[e.day_cursor.index], 0);
        assert_eq!(e.focused(), Some(&on_air), "widening keeps the programme");
        // Left walks back column by column.
        press(&mut l, &d, &[Nav::Left]);
        assert_eq!(l.column(), Column::Programmes);
        let out = press(&mut l, &d, &[Nav::Back]);
        assert_eq!(l.column(), Column::Channels);
        assert!(out.navigate.is_empty());
    }

    #[test]
    fn moving_through_days_updates_the_programme_list() {
        let d = fake(10);
        let mut l = open(&d, Mode::Overlay);
        press(&mut l, &d, &[Nav::Right, Nav::Right, Nav::Up]);
        let e = l.epg().unwrap();
        assert_eq!(e.day, -1);
        let from = time::day_start(NOW, 0) - DAY;
        let list = e.programmes();
        assert!(!list.is_empty());
        assert!(list.iter().all(|p| p.stop > from && p.start < from + DAY));
        assert!(
            list.first().unwrap().start <= from,
            "the day starts with the programme crossing midnight"
        );
        assert_eq!(
            e.cursor.index, 0,
            "another day starts at its first programme"
        );
        press(&mut l, &d, &[Nav::Down, Nav::Down]);
        assert_eq!(l.epg().unwrap().day, 1);
        press(&mut l, &d, &[Nav::Top]);
        assert_eq!(l.epg().unwrap().day, -DAYS_BACK);
        // OK picks the day and returns to its programmes.
        press(&mut l, &d, &[Nav::Ok]);
        assert_eq!(l.column(), Column::Programmes);
        assert_eq!(l.epg().unwrap().day, -DAYS_BACK);
        // Leaving the programmes resets the passive list to today.
        press(&mut l, &d, &[Nav::Left]);
        let e = l.epg().unwrap();
        assert_eq!(e.day, 0);
        assert!(e.focused().unwrap().is_on_air(NOW));
    }

    #[test]
    fn ok_on_a_past_programme_plays_catchup() {
        let mut d = fake(10);
        playing(&mut d, 0);
        let mut l = open(&d, Mode::Overlay);
        press(&mut l, &d, &[Nav::Right, Nav::Up]);
        let p = l.epg().unwrap().focused().unwrap().clone();
        assert_eq!(time::when(p.start, p.stop, NOW), When::Past);
        let channel = d.channels[0].key;
        let out = press(&mut l, &d, &[Nav::Ok]);
        assert_eq!(
            out.commands,
            [Command::Play {
                stream: Stream::Catchup {
                    channel,
                    programme: d.programme_at(channel, p.start).unwrap(),
                    offset_secs: 0
                },
                group: Some(Group::All),
            }]
        );
        assert_eq!(out.navigate, [Navigate::Close]);
    }

    #[test]
    fn catchup_respects_the_channel_window() {
        let mut d = fake(10);
        d.channels[0].catchup.days = 1;
        let mut l = open(&d, Mode::Overlay);
        // Three days back is outside a one-day archive.
        press(
            &mut l,
            &d,
            &[Nav::Right, Nav::Right, Nav::Up, Nav::Up, Nav::Up, Nav::Ok],
        );
        assert_eq!(l.epg().unwrap().day, -3);
        assert!(press(&mut l, &d, &[Nav::Ok]).commands.is_empty());
        // Channel 2 has no archive at all.
        let mut l = open(&d, Mode::Overlay);
        press(&mut l, &d, &[Nav::Down, Nav::Down, Nav::Right, Nav::Up]);
        assert!(!d.channels[2].catchup.available());
        let out = press(&mut l, &d, &[Nav::Ok]);
        assert!(out.commands.is_empty() && out.navigate.is_empty());
    }

    #[test]
    fn ok_on_air_plays_live_and_future_does_nothing() {
        let mut d = fake(10);
        playing(&mut d, 0);
        let mut l = open(&d, Mode::Overlay);
        press(&mut l, &d, &[Nav::Down, Nav::Right, Nav::Down]);
        let p = l.epg().unwrap().focused().unwrap();
        assert_eq!(time::when(p.start, p.stop, NOW), When::Future);
        assert!(press(&mut l, &d, &[Nav::Ok]).commands.is_empty());
        let out = press(&mut l, &d, &[Nav::Up, Nav::Ok]);
        assert_eq!(out.commands, [live(&d, 1)]);
    }

    #[test]
    fn preview_catchup_twice_returns_to_full_screen() {
        let mut d = fake(10);
        playing(&mut d, 0);
        let mut l = open(&d, Mode::Preview);
        press(&mut l, &d, &[Nav::Right, Nav::Up]);
        let out = press(&mut l, &d, &[Nav::Ok]);
        assert!(matches!(
            out.commands.as_slice(),
            [Command::Play {
                stream: Stream::Catchup { .. },
                ..
            }]
        ));
        assert!(out.navigate.is_empty());
        assert!(l.showing_catchup().is_some());
        assert_eq!(press(&mut l, &d, &[Nav::Ok]), Output::close());
    }

    #[test]
    fn a_row_reloads_when_its_programme_ends() {
        let mut d = fake(10);
        let mut l = open(&d, Mode::Overlay);
        l.tick(&d, 0);
        let generation = l.generation();
        d.now = NOW + 1;
        assert!(
            !l.tick(&d, 1_000).redraw,
            "nothing drawn changes within the minute"
        );
        let end = l.row(0).unwrap().until;
        assert!(l.deadline() <= end);
        d.now = end;
        assert!(l.tick(&d, 2_000).redraw);
        assert!(l.generation() > generation);
        assert!(l.on_air(0).unwrap().is_on_air(end));
    }

    #[test]
    fn refresh_keeps_the_focus() {
        let mut d = fake(10);
        let mut l = open(&d, Mode::Overlay);
        press(
            &mut l,
            &d,
            &[Nav::Down, Nav::Down, Nav::Right, Nav::Right, Nav::Up],
        );
        press(&mut l, &d, &[Nav::Ok, Nav::Down]);
        let key = d.channels[2].key;
        let focus = l.epg().unwrap().focused().unwrap().clone();
        d.channels.remove(0);
        l.refresh(&d);
        assert_eq!(l.focused_channel().unwrap().key, key);
        assert_eq!(l.cursor().index, 1);
        assert_eq!(l.column(), Column::Programmes);
        let e = l.epg().unwrap();
        assert_eq!((e.key, e.day), (key, -1));
        assert_eq!(e.focused(), Some(&focus));
        assert_eq!(l.detail().map(|p| p.start), Some(focus.start));
        // A channel the playlist lost: back to the channels, on the first.
        d.channels.remove(1);
        l.refresh(&d);
        assert_eq!((l.cursor().index, l.column()), (0, Column::Channels));
    }
}
