//! The TV guide: a programme grid under a preview of the playing stream, with the groups panel
//! on its left.
//!
//! [`GuideScreen`] is a [`Screen`]: a state machine plus a view. The app feeds it remote keys and
//! clock ticks with read access to [`TvData`]; it answers with [`Command`]s for the app to run
//! and [`Navigate`]s for screens outside the guide (the player, the context menu, the main
//! menu). While it is up the video plays in its preview rectangle ([`Screen::video_rect`]), or
//! in picture in picture ([`GuideScreen::set_pip`]) in a corner window over the guide laid out
//! without the preview.
//!
//! Remote behaviour (TiviMate):
//!
//! | Key | Grid | Groups panel |
//! |---|---|---|
//! | Up / Down | channel, wrapping | group (the guide follows; History does not filter) |
//! | CH+ / CH- | a page of channels | a page of groups |
//! | Right | next programme (repeats) | back to the grid |
//! | Left | previous programme on every channel, back to [`TvData::past_days`] (repeats) | [`Navigate::MainMenu`] |
//! | long Left | - | top of the list |
//! | OK | preview, then play full screen; a past programme only from the archive | back to the grid; on History [`Navigate::History`] |
//! | long OK | [`Navigate::ContextMenu`] | [`Navigate::GroupMenu`] |
//! | Back | leave Manage favorites, else back to live, else leave picture in picture, else the groups panel | [`Navigate::Close`] with what played when the guide opened; with nothing playing, back to the grid |
//! | 0-9 | a channel number ([`crate::number`]): once the typing stops (or on OK) the focus moves to that channel at the same time, in every channel's list when the group shown lacks it; Back drops it | - |
//!
//! The pointer focuses the cell (or, on the channel column, the row) it rests on, without
//! scrolling the rows or moving the window; in the groups panel, the group.
//!
//! The groups panel ([`GroupsPanel`], shared with the channel list) covers the left of the guide
//! without moving it; the preview keeps its rectangle (and keeps playing) under the panel. A
//! group it shows is the channel list's too ([`Command::ShowGroup`]). In Manage favorites mode
//! ([`ScreenMessage::ManageFavorites`]) every row shows a star and OK toggles it.
//!
//! [`Command`]: crate::data::Command
//! [`Command::ShowGroup`]: crate::data::Command::ShowGroup

mod clock;
mod geom;
mod grid;
mod paint;

#[cfg(test)]
mod pip_tests;
#[cfg(test)]
mod tests;

use tv_machine::machine::Measure;
use tv_ui::Painter;

use tv_platform::i18n::msg;

use crate::data::{self, Command, Group, Logos, Navigate, Output, Screen, ScreenMessage, TvData};
use crate::draw;
use crate::groups::{Act, GroupsPanel};
use crate::input::{Hold, Key, Press};
use crate::number::{self, Interrupt, NumberEntry};
use crate::point::Spots;
use crate::time;
use grid::{Grid, Mode};
use paint::Paint;

/// One 60 Hz frame: how soon a moving scroll wants the next one.
const FRAME_MS: u64 = 16;

/// [`GuideScreen::focus`].
#[derive(Debug)]
pub struct GuideFocus<'a> {
    pub channel: Option<&'a iptv::model::Channel>,
    pub programme: Option<iptv::model::Programme>,
    /// The focus follows the programme on air.
    pub live: bool,
    /// The focused group while the groups panel has the focus.
    pub group: Option<&'a Group>,
    /// Manage favorites mode.
    pub managing: bool,
}

pub struct GuideScreen {
    grid: Grid,
    groups: GroupsPanel,
    group: Group,
    /// The group the guide opened on, which is the one the player zapped through then: leaving
    /// for the player gives it back.
    zap: Group,
    /// Focus is in the groups panel (drawn over the left of the guide).
    panel: bool,
    paint: Paint,
    /// The grid's rows and cells in the frame drawn last, for the pointer.
    spots: Spots,
    /// [`Grid::clock_face`] when the state last changed: a tick that moves neither the now-line
    /// nor the minute draws nothing.
    clock: (Option<i32>, i64),
    /// A channel number being typed.
    number: Option<NumberEntry>,
}

impl GuideScreen {
    /// The guide over `group`, the one the player zaps through, open: focus on the playing
    /// channel at live (or on the archived programme playing), the playing stream in the preview.
    pub fn new(data: &dyn TvData, group: Group) -> Self {
        let mut s = GuideScreen {
            grid: Grid::new(data.channels(&group)),
            groups: GroupsPanel::new(data.groups(), &group, geom::GROUPS),
            zap: group.clone(),
            group,
            panel: false,
            paint: Paint::default(),
            spots: Spots::default(),
            clock: (None, 0),
            number: None,
        };
        s.grid.open(data);
        s.sync(data);
        s
    }

    /// Where the focus is, for the app's remote-driven navigation tests.
    pub fn focus(&self) -> GuideFocus<'_> {
        GuideFocus {
            channel: self.grid.channel().map(|c| &**c),
            programme: self.grid.focused_programme(),
            live: self.grid.at_live(),
            group: self.panel.then(|| self.groups.focused()).flatten(),
            managing: self.grid.mode == Mode::ManageFavorites,
        }
    }

    fn grid_key(&mut self, key: Key, long: bool, data: &dyn TvData) -> Output {
        let page = (geom::VISIBLE_ROWS - 1) as isize;
        let managing = self.grid.mode == Mode::ManageFavorites;
        match (key, long) {
            (Key::Up, _) => self.grid.vertical(data, false),
            (Key::Down, _) => self.grid.vertical(data, true),
            (Key::ChannelUp, _) => self.grid.page(data, -page),
            (Key::ChannelDown, _) => self.grid.page(data, page),
            (Key::Back, _) => {
                if self.grid.back(data) {
                    if self.paint.pip {
                        // The video goes back into the preview.
                        self.set_pip(false);
                    } else {
                        self.open_panel();
                    }
                }
            }
            (Key::Ok, false) if !managing && self.grid.out_of_archive() => {
                return Output {
                    toast: Some(msg::epg_catchup_unavailable().to_owned()),
                    ..Output::default()
                };
            }
            (Key::Ok, false) => {
                let (command, close) = self.grid.ok(data, &self.group);
                let out = Output {
                    commands: command.into_iter().collect(),
                    ..Output::default()
                };
                return if close { out.and(Output::close()) } else { out };
            }
            (Key::Ok, true) if !managing => {
                if let Some(c) = self.grid.channel() {
                    return Output::navigate(Navigate::ContextMenu {
                        channel: c.key,
                        programme: self.grid.focused_programme(),
                    });
                }
            }
            (Key::Right, _) if !managing => self.grid.right(data),
            (Key::Left, _) if !managing => self.grid.left(data),
            _ => {}
        }
        Output::default()
    }

    fn groups_key(&mut self, key: Key, long: bool, data: &dyn TvData) -> Output {
        match self.groups.key(key, long) {
            Act::Moved => return self.show_group(data),
            // A locked group shows nothing until the PIN opens it.
            Act::Enter if data.group_locked(&self.group) => {
                return crate::parental::ask_to_open(&self.group)
            }
            // An empty group keeps the focus in the panel; Back always leaves it.
            Act::Enter if self.grid.channels.is_empty() => {}
            Act::Enter => self.panel = false,
            // Back goes back to the player while something plays; otherwise it only closes the
            // panel (no empty player).
            Act::Back if data.playback().channel.is_none() => self.panel = false,
            Act::Back => {
                self.panel = false;
                return self.close();
            }
            Act::Go(nav) => return Output::navigate(nav),
            Act::None => {}
        }
        Output::default()
    }

    /// The panel's focus moved: the guide follows the group it shows, and the app opens the
    /// channel list on it from now on.
    fn show_group(&mut self, data: &dyn TvData) -> Output {
        match self.groups.shown() {
            Some(g) if *g != self.group => {
                self.group = g.clone();
                self.grid.set_channels(data, data.channels(&self.group));
                Output::command(Command::ShowGroup(self.group.clone()))
            }
            _ => Output::default(),
        }
    }

    /// The typed number is taken: focus the channel it names, at the same time. One the group
    /// shown does not list brings every channel in, as the groups panel would.
    fn commit_number(&mut self, entry: &NumberEntry, data: &dyn TvData) -> Output {
        let Some(channel) = entry.channel(data) else {
            return Output::redraw();
        };
        let mut out = Output::redraw();
        if !self.grid.focus_channel(data, channel.key) {
            self.set_group(Group::All, data);
            self.grid.focus_channel(data, channel.key);
            out = out.and(Output::command(Command::ShowGroup(Group::All)));
        }
        self.sync(data);
        out
    }

    /// Show `group`'s channels, the groups panel on it.
    fn set_group(&mut self, group: Group, data: &dyn TvData) {
        self.groups.select(&group);
        self.grid.set_channels(data, data.channels(&group));
        self.group = group;
    }

    /// Picture in picture ([`Navigate::Pip`]): the guide without its preview, the video in the
    /// corner window ([`Screen::video_rect`]). OK on the channel in it plays it full screen, as
    /// the second OK on a preview does; Back at live leaves it.
    pub fn set_pip(&mut self, on: bool) {
        self.paint.set_pip(on);
    }

    fn open_panel(&mut self) {
        self.panel = true;
        self.groups.select(&self.group);
    }

    /// Back to the player, with what played when the guide opened.
    fn close(&self) -> Output {
        let restore = Output {
            commands: self.grid.restore(&self.zap).into_iter().collect(),
            ..Output::default()
        };
        restore.and(Output::close())
    }

    /// After every state change: read the focused programme's details, aim the row scroll and
    /// lay out again before the next frame.
    fn sync(&mut self, data: &dyn TvData) {
        self.grid.load_details(data);
        self.paint.follow(self.grid.cursor.top);
        self.clock = self.grid.clock_face();
    }
}

impl Screen for GuideScreen {
    fn hold(&self, key: Key) -> Hold {
        if self.panel {
            return GroupsPanel::hold(key);
        }
        match key {
            Key::Up | Key::Down | Key::ChannelUp | Key::ChannelDown | Key::Left | Key::Right => {
                Hold::Repeat
            }
            Key::Ok => Hold::Long,
            _ => Hold::Once,
        }
    }

    fn key(&mut self, key: Key, press: Press, data: &dyn TvData, now_ms: u64) -> Output {
        self.grid.now = data.now();
        if let Key::Digit(d) = key {
            // The grid takes a channel number; the groups panel takes none.
            if press != Press::Short || self.panel {
                return Output::default();
            }
            NumberEntry::type_digit(&mut self.number, d, data, now_ms);
            return Output::redraw();
        }
        match NumberEntry::interrupt(&mut self.number, key) {
            Interrupt::Commit(entry) => return self.commit_number(&entry, data),
            Interrupt::Dropped => return Output::redraw(),
            Interrupt::Pass => {}
        }
        let long = press == Press::Long;
        let mut out = if self.panel {
            self.groups_key(key, long, data)
        } else {
            self.grid_key(key, long, data)
        };
        self.sync(data);
        out.redraw = !out.navigate.contains(&Navigate::Close);
        out
    }

    fn point(&mut self, x: f32, y: f32, data: &dyn TvData, _now_ms: u64) -> Output {
        self.grid.now = data.now();
        let out = if self.panel {
            match self.groups.point(x, y) {
                Act::Moved => self.show_group(data),
                _ => return Output::default(),
            }
        } else {
            let Some(s) = self.spots.at(x, y) else {
                return Output::default();
            };
            let start = (s.area == paint::CELL).then_some(s.item);
            if !self.grid.point(data, s.index, start) {
                return Output::default();
            }
            Output::default()
        };
        self.sync(data);
        Output {
            redraw: true,
            ..out
        }
    }

    /// The row the bottom edge cuts, the grid beside the groups panel and the panel's own cut
    /// row.
    fn dead(&self, x: f32, y: f32) -> bool {
        self.spots.dead_at(x, y) || (self.panel && self.groups.dead(x, y))
    }

    fn tick(&mut self, data: &dyn TvData, now_ms: u64) -> Output {
        let typed = match NumberEntry::due(&mut self.number, now_ms) {
            Some(entry) => {
                self.grid.now = data.now();
                self.commit_number(&entry, data)
            }
            None => Output::default(),
        };
        let played = self.grid.read_playback(data);
        let moved = self.grid.tick(data);
        let redraw = played || moved || self.grid.clock_face() != self.clock;
        if redraw {
            self.sync(data);
        }
        typed.and(Output {
            redraw: redraw || self.paint.animating(),
            ..Output::default()
        })
    }

    /// When [`tick`](Self::tick) (or, while the rows scroll, the next frame) has something to
    /// do: the scroll animation, a typed number to take, the now-line's next pixel, the clock's
    /// minute, the live programme ending. `None` while nothing on screen follows the clock
    /// (browsing another day away from live) and no number is typed.
    fn next_wake(&self, now_ms: u64) -> Option<u64> {
        if self.paint.animating() {
            return Some(now_ms + FRAME_MS);
        }
        let typed = self.number.as_ref().map(|n| n.commit_at_ms);
        let grid = &self.grid;
        let now = grid.now;
        let minute = (now.div_euclid(time::MINUTE) + 1) * time::MINUTE;
        let on_air = grid.focus.as_ref().filter(|p| p.is_on_air(now));
        let clock = [
            // The ruler shows the clock while the window is on today.
            (time::local_day(grid.window, grid.offset) == time::local_day(now, grid.offset))
                .then_some(minute),
            // The details count down the minutes of an on-air programme.
            on_air.map(|_| minute),
            // At live the focus moves on when the programme ends.
            on_air.filter(|_| grid.at_live()).map(|p| p.stop),
            geom::now_line_step(now, grid.window),
        ]
        .into_iter()
        .flatten()
        .min()
        .map(|due| time::wake_ms(now_ms, now, due));
        clock.into_iter().chain(typed).min()
    }

    fn video_rect(&self) -> Option<data::Rect> {
        Some(if self.paint.pip {
            geom::pip_rect()
        } else {
            geom::preview_rect()
        })
    }

    fn refresh(&mut self, data: &dyn TvData) {
        self.groups.set(data.groups(), &self.group);
        if let Some(g) = self.groups.shown() {
            self.group = g.clone();
        }
        self.grid.refresh(data, data.channels(&self.group));
        self.paint.forget();
        self.sync(data);
    }

    fn message(&mut self, message: ScreenMessage, data: &dyn TvData) {
        match message {
            ScreenMessage::ManageFavorites => {
                self.panel = false;
                self.grid.mode = Mode::ManageFavorites;
            }
            ScreenMessage::SetGroup(group) => self.set_group(group, data),
        }
        self.sync(data);
    }

    fn update(&mut self, dt: f32) {
        self.paint.update(dt);
    }

    fn draw(&mut self, p: Painter, m: &dyn Measure, logos: &dyn Logos) {
        self.paint.prepare(m, &self.grid);
        self.paint.draw(p, &self.grid, self.panel, logos);
        let panel = self.panel.then(|| geom::GROUPS.column());
        self.paint.spots(&self.grid, panel, &mut self.spots);
        if self.panel {
            self.groups.prepare(m);
            self.groups.paint(p, logos);
        }
        // Laid out per frame, but only for the second or two while digits are typed.
        if let Some(n) = &self.number {
            let mut pieces = Vec::new();
            number::paint(&mut pieces, n, geom::DETAILS_RIGHT, geom::PREVIEW.y, m);
            draw::paint(&pieces, p, logos);
        }
    }
}
