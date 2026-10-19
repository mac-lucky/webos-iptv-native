//! The TV guide: a programme grid under a preview of the playing stream, with the groups panel
//! on its left.
//!
//! [`GuideScreen`] is a [`Screen`]: a state machine plus a view. The app feeds it remote keys and
//! clock ticks with read access to [`TvData`]; it answers with [`Command`]s for the app to run
//! and [`Navigate`]s for screens outside the guide (the player, the context menu, the main
//! menu). While it is up the video plays in its preview rectangle ([`Screen::video_rect`]).
//!
//! Remote behaviour (TiviMate):
//!
//! | Key | Grid | Groups panel |
//! |---|---|---|
//! | Up / Down | channel, wrapping | group (the guide follows; History does not filter) |
//! | CH+ / CH- | a page of channels | a page of groups |
//! | Right | next programme | back to the grid |
//! | Left | previous programme; from live the groups panel; the past only on catch-up channels | [`Navigate::MainMenu`] |
//! | long Left | from the future to live, else one programme into the archive | top of the list |
//! | OK | preview, then play full screen | back to the grid; on History [`Navigate::History`] |
//! | long OK | [`Navigate::ContextMenu`] | [`Navigate::GroupMenu`] |
//! | Back | leave Manage favorites, else back to live, else the groups panel | back to the grid |
//! | long Back | [`Navigate::Close`] with what played when the guide opened | same |
//!
//! In Manage favorites mode ([`ScreenMessage::ManageFavorites`]) every row shows a star and OK
//! toggles it.
//!
//! [`Command`]: crate::data::Command

mod clock;
mod geom;
mod grid;
mod groups;
mod paint;

#[cfg(test)]
mod tests;

use tv_machine::machine::Measure;
use tv_ui::Painter;

use crate::data::{self, Group, Logos, Navigate, Output, Screen, ScreenMessage, TvData};
use crate::input::{Hold, Key, Press};
use crate::time;
use grid::{Grid, Mode, Step};
use groups::Groups;
use paint::Paint;

/// One 60 Hz frame: how soon a moving scroll wants the next one.
const FRAME_MS: u64 = 16;

pub struct GuideScreen {
    grid: Grid,
    groups: Groups,
    group: Group,
    /// The group the guide opened on, which is the one the player zapped through then: long
    /// Back gives it back.
    zap: Group,
    /// Focus is in the groups panel (and the guide is pushed right).
    panel: bool,
    paint: Paint,
    /// [`Grid::clock_face`] when the state last changed: a tick that moves neither the now-line
    /// nor the minute draws nothing.
    clock: (Option<i32>, i64),
}

impl GuideScreen {
    /// The guide over `group`, the one the player zaps through, open: focus on the playing
    /// channel at live (or on the archived programme playing), the playing stream in the preview.
    pub fn new(data: &dyn TvData, group: Group) -> Self {
        let mut s = GuideScreen {
            grid: Grid::new(data.channels(&group)),
            groups: Groups::new(data.groups(), &group),
            zap: group.clone(),
            group,
            panel: false,
            paint: Paint::default(),
            clock: (None, 0),
        };
        s.grid.open(data);
        s.sync(data);
        s
    }

    fn grid_key(&mut self, key: Key, long: bool, data: &dyn TvData) -> Output {
        let page = (geom::VISIBLE_ROWS - 1) as isize;
        let managing = self.grid.mode == Mode::ManageFavorites;
        match (key, long) {
            (Key::Up, _) => self.grid.vertical(data, false),
            (Key::Down, _) => self.grid.vertical(data, true),
            (Key::ChannelUp, _) => self.grid.page(data, -page),
            (Key::ChannelDown, _) => self.grid.page(data, page),
            (Key::Back, true) => {
                self.grid.mode = Mode::Browse;
                return self.close();
            }
            (Key::Back, false) => {
                if self.grid.back(data) {
                    self.open_panel();
                }
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
            (Key::Right, _) if !managing => {
                self.grid.right(data);
            }
            (Key::Left, false) if !managing => {
                if self.grid.left(data) == Step::Edge {
                    self.open_panel();
                }
            }
            (Key::Left, true) if !managing => {
                self.grid.long_left(data);
            }
            _ => {}
        }
        Output::default()
    }

    fn groups_key(&mut self, key: Key, long: bool, data: &dyn TvData) -> Output {
        let page = geom::GROUP_ROWS as isize - 1;
        let moved = match (key, long) {
            (Key::Up, _) => self.groups.step(-1),
            (Key::Down, _) => self.groups.step(1),
            (Key::ChannelUp, _) => self.groups.step(-page),
            (Key::ChannelDown, _) => self.groups.step(page),
            (Key::Left, true) => self.groups.first(),
            (Key::Left, false) => return Output::navigate(Navigate::MainMenu),
            (Key::Ok, true) => {
                return match self.groups.focused() {
                    Some(g) => Output::navigate(Navigate::GroupMenu(g.clone())),
                    None => Output::default(),
                }
            }
            // History is its own screen, not a filter.
            (Key::Ok | Key::Right, _) if self.groups.focused() == Some(&Group::History) => {
                return Output::navigate(Navigate::History)
            }
            // An empty group keeps the focus in the panel; Back always leaves it.
            (Key::Ok | Key::Right, _) if self.grid.channels.is_empty() => false,
            (Key::Ok | Key::Right, _) | (Key::Back, false) => {
                self.panel = false;
                return Output::default();
            }
            (Key::Back, true) => {
                self.panel = false;
                return self.close();
            }
            _ => false,
        };
        match self.groups.focused() {
            Some(g) if moved && *g != Group::History => {
                self.group = g.clone();
                self.grid.set_channels(data, data.channels(&self.group));
            }
            _ => {}
        }
        Output::default()
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
        match key {
            Key::Up | Key::Down | Key::ChannelUp | Key::ChannelDown => Hold::Repeat,
            Key::Right if !self.panel => Hold::Repeat,
            Key::Left | Key::Ok | Key::Back => Hold::Long,
            _ => Hold::Once,
        }
    }

    fn key(&mut self, key: Key, press: Press, data: &dyn TvData, _now_ms: u64) -> Output {
        self.grid.now = data.now();
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

    fn tick(&mut self, data: &dyn TvData, _now_ms: u64) -> Output {
        let played = self.grid.read_playback(data);
        let moved = self.grid.tick(data);
        let redraw = played || moved || self.grid.clock_face() != self.clock;
        if redraw {
            self.sync(data);
        }
        Output {
            redraw: redraw || self.paint.animating(),
            ..Output::default()
        }
    }

    /// When [`tick`](Self::tick) (or, while the rows scroll, the next frame) has something to
    /// do: the scroll animation, the now-line's next pixel, the clock's minute, the live
    /// programme ending. `None` while nothing on screen follows the clock (browsing another day
    /// away from live).
    fn next_wake(&self, now_ms: u64) -> Option<u64> {
        if self.paint.animating() {
            return Some(now_ms + FRAME_MS);
        }
        let grid = &self.grid;
        let now = grid.now;
        let minute = (now.div_euclid(time::MINUTE) + 1) * time::MINUTE;
        let on_air = grid.focus.as_ref().filter(|p| p.is_on_air(now));
        let due = [
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
        .min()?;
        Some(time::wake_ms(now_ms, now, due))
    }

    fn video_rect(&self) -> Option<data::Rect> {
        let shift = if self.panel { geom::GROUPS_W } else { 0.0 };
        Some(geom::preview_rect(shift))
    }

    fn refresh(&mut self, data: &dyn TvData) {
        self.groups.set(data.groups(), &self.group);
        if let Some(g) = self.groups.focused() {
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
            ScreenMessage::SetGroup(group) => {
                self.groups.select(&group);
                self.grid.set_channels(data, data.channels(&group));
                self.group = group;
            }
        }
        self.sync(data);
    }

    fn update(&mut self, dt: f32) {
        self.paint.update(dt);
    }

    fn draw(&mut self, p: Painter, m: &dyn Measure, logos: &dyn Logos) {
        self.paint.prepare(m, &self.grid, &self.groups);
        self.paint
            .draw(p, &self.grid, &self.groups, self.panel, logos);
    }
}
