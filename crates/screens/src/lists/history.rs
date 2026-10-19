//! History: the catch-up programmes watched lately, grouped by the local day they were watched.
//!
//! Like TiviMate the list reads oldest first, so Today is the last group, and it opens on the
//! most recent entry. OK resumes the programme at the saved position (from the start once it was
//! watched to the end) in full screen; Right focuses the row's delete button, OK there removes the
//! entry. Which entries exist (watched >= 10 s, kept 7 days, both settings) is storage's business:
//! this screen shows what [`TvData::history`] returns.

use std::rc::Rc;

use iptv::model::{Channel, Programme, Timestamp};
use tv_machine::machine::Measure;
use tv_ui::Painter;

use super::cursor::Cursor;
use super::view::HistoryView;
use super::{Clock, Nav};
use crate::data::{Command, HistoryEntry, Logos, Navigate, Output, Screen, Stream, TvData};
use crate::input::{Hold, Key, Press};
use crate::time::{self, DAY};

/// Rows on screen (day headers take a row each).
pub const ROWS: usize = 8;

/// A programme watched to within this many seconds of its end restarts from the beginning.
pub const FINISHED_SLACK_SECS: i64 = 10;

/// One line of the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line {
    /// The local day of the entries below, as an offset from today (yesterday is -1).
    Day(i32),
    /// An index into [`History::entries`].
    Entry(usize),
}

/// The channel facts a row shows.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelFacts {
    /// `None` when the playlist no longer has the channel.
    pub channel: Option<Rc<Channel>>,
    /// Whether the entry can still be played from the archive.
    pub playable: bool,
}

impl ChannelFacts {
    /// The channel's name, empty when it is gone.
    pub fn name(&self) -> &str {
        self.channel.as_deref().map_or("", |c| c.name.as_str())
    }
}

pub struct History {
    entries: Vec<HistoryEntry>,
    facts: Vec<ChannelFacts>,
    lines: Vec<Line>,
    cursor: Cursor,
    on_delete: bool,
    utc_offset: i32,
    clock: Clock,
    generation: u64,
    view: HistoryView,
}

/// Where to resume `e`: its saved position, or the start once it was watched to the end.
pub fn resume_at(e: &HistoryEntry) -> i64 {
    let len = e.programme.duration_secs();
    if e.position_secs <= 0 || e.position_secs >= len - FINISHED_SLACK_SECS {
        0
    } else {
        e.position_secs
    }
}

impl History {
    pub fn open(data: &dyn TvData) -> Self {
        let mut h = History {
            entries: Vec::new(),
            facts: Vec::new(),
            lines: Vec::new(),
            cursor: Cursor::default(),
            on_delete: false,
            utc_offset: data.utc_offset_secs(),
            clock: Clock::new(data.now()),
            generation: 0,
            view: HistoryView::default(),
        };
        h.read(data);
        let last = h.lines.len().saturating_sub(1);
        h.cursor.set(last, h.lines.len(), ROWS);
        h
    }

    /// Read the entries and their channels.
    fn read(&mut self, data: &dyn TvData) {
        let now = self.clock.now;
        self.entries = data.history();
        // Storage answers most recent first; the list reads oldest first.
        self.entries.reverse();
        self.facts = self
            .entries
            .iter()
            .map(|e| {
                let channel = data.channel(e.channel);
                ChannelFacts {
                    playable: channel
                        .as_deref()
                        .is_some_and(|c| iptv::catchup::in_archive(c, &e.programme, now)),
                    channel,
                }
            })
            .collect();
        self.relayout();
    }

    fn relayout(&mut self) {
        self.lines.clear();
        let mut day = None;
        for (i, e) in self.entries.iter().enumerate() {
            let d = time::day_offset(e.watched_at, self.clock.now, self.utc_offset);
            if day != Some(d) {
                self.lines.push(Line::Day(d));
                day = Some(d);
            }
            self.lines.push(Line::Entry(i));
        }
        self.generation += 1;
    }

    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    pub fn facts(&self, entry: usize) -> Option<&ChannelFacts> {
        self.facts.get(entry)
    }

    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Whether the focus is on the focused row's delete button.
    pub fn on_delete(&self) -> bool {
        self.on_delete
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

    /// The focused entry's index.
    pub fn focused(&self) -> Option<usize> {
        match self.lines.get(self.cursor.index) {
            Some(&Line::Entry(i)) => Some(i),
            _ => None,
        }
    }

    pub fn focused_entry(&self) -> Option<&HistoryEntry> {
        self.focused().and_then(|i| self.entries.get(i))
    }

    /// The clock moved to `now`: past midnight the day headers move. Returns whether they did.
    fn set_now(&mut self, now: Timestamp) -> bool {
        let day_changed = time::day_offset(now, self.clock.now, self.utc_offset) != 0;
        self.clock.now = now;
        if day_changed {
            let focused = self.focused();
            self.relayout();
            self.focus_entry(focused);
        }
        day_changed
    }

    pub(crate) fn nav(&mut self, nav: Nav) -> Output {
        let before = (self.cursor, self.on_delete, self.generation);
        let mut out = Output::default();
        match nav {
            Nav::Up => self.step(-1),
            Nav::Down => self.step(1),
            Nav::PageUp => self.page(-1),
            Nav::PageDown => self.page(1),
            Nav::Top => self.focus_entry((!self.entries.is_empty()).then_some(0)),
            Nav::Right => self.on_delete = self.focused().is_some(),
            Nav::Left => self.on_delete = false,
            Nav::Back => return Output::close(),
            Nav::Ok if self.on_delete => out = self.delete(),
            Nav::Ok => return self.play(),
        }
        if (self.cursor, self.on_delete) != (before.0, before.1) {
            self.generation += 1;
        }
        Output {
            redraw: self.generation != before.2,
            ..out
        }
    }

    /// Move to the next entry line in `dir`, skipping day headers; the ends hold.
    fn step(&mut self, dir: isize) {
        let mut i = self.cursor.index as isize + dir;
        while i >= 0 && (i as usize) < self.lines.len() {
            if matches!(self.lines[i as usize], Line::Entry(_)) {
                self.cursor.set(i as usize, self.lines.len(), ROWS);
                return;
            }
            i += dir;
        }
    }

    fn page(&mut self, dir: isize) {
        for _ in 0..ROWS - 1 {
            self.step(dir);
        }
    }

    fn focus_entry(&mut self, entry: Option<usize>) {
        let line = entry
            .and_then(|e| self.lines.iter().position(|l| *l == Line::Entry(e)))
            .unwrap_or(0);
        self.cursor.set(line, self.lines.len(), ROWS);
    }

    /// Resume the focused entry in full screen.
    fn play(&self) -> Output {
        let Some(i) = self.focused().filter(|&i| self.facts[i].playable) else {
            return Output::default();
        };
        let e = &self.entries[i];
        let stream = Stream::Catchup {
            channel: e.channel,
            programme: e.programme.clone(),
            offset_secs: resume_at(e),
        };
        Output::command(Command::Play {
            stream,
            group: None,
        })
        .and(Output::navigate(Navigate::Player))
    }

    fn delete(&mut self) -> Output {
        let Some(i) = self.focused() else {
            return Output::default();
        };
        let e = self.entries.remove(i);
        self.facts.remove(i);
        self.relayout();
        if self.entries.is_empty() {
            self.on_delete = false;
            self.cursor = Cursor::default();
        } else {
            // The entry that took its place, or the new last one.
            self.focus_entry(Some(i.min(self.entries.len() - 1)));
        }
        Output::command(Command::RemoveHistory {
            channel: e.channel,
            start: e.programme.start,
        })
    }

    /// The programme times a row shows: `HH:MM - HH:MM`, with the date when it did not air today.
    pub fn when_line(&self, p: &Programme) -> String {
        time::programme_span(p, self.clock.now, self.utc_offset)
    }
}

impl Screen for History {
    fn hold(&self, key: Key) -> Hold {
        super::hold(key)
    }

    fn key(&mut self, key: Key, press: Press, data: &dyn TvData, now_ms: u64) -> Output {
        let moved = self.set_now(data.now());
        self.clock.set(self.clock.now, now_ms);
        let out = Nav::of(key, press).map_or_else(Output::default, |nav| self.nav(nav));
        Output {
            redraw: out.redraw || moved,
            ..out
        }
    }

    fn tick(&mut self, data: &dyn TvData, now_ms: u64) -> Output {
        let moved = self.set_now(data.now());
        self.clock.set(self.clock.now, now_ms);
        Output {
            redraw: moved,
            ..Output::default()
        }
    }

    /// Midnight, when the day headers move.
    fn next_wake(&self, now_ms: u64) -> Option<u64> {
        let midnight = time::day_start(self.clock.now, self.utc_offset) + DAY;
        Some(self.clock.wake(now_ms, midnight))
    }

    fn refresh(&mut self, data: &dyn TvData) {
        let focused = self.focused_entry().map(|e| (e.channel, e.programme.start));
        self.utc_offset = data.utc_offset_secs();
        self.read(data);
        let at = focused.and_then(|(channel, start)| {
            self.entries
                .iter()
                .position(|e| e.channel == channel && e.programme.start == start)
        });
        let last = self.entries.len().checked_sub(1);
        self.focus_entry(at.or(last));
        self.on_delete &= self.focused().is_some();
    }

    fn draw(&mut self, p: Painter, m: &dyn Measure, logos: &dyn Logos) {
        let mut view = std::mem::take(&mut self.view);
        view.prepare(self, m);
        view.draw(p, logos);
        self.view = view;
    }
}

#[cfg(test)]
impl History {
    /// What the last [`Screen::draw`] laid out.
    pub(crate) fn pieces(&self) -> &[crate::draw::Piece] {
        self.view.pieces()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fake::FakeData as Fixture;
    use iptv::model::ChannelKey;

    // 2024-08-27 15:12:00 UTC.
    const NOW: Timestamp = 1_724_771_520;
    const H: i64 = 3600;

    fn entry(
        d: &Fixture,
        ch: usize,
        start: Timestamp,
        len: i64,
        pos: i64,
        watched: Timestamp,
    ) -> HistoryEntry {
        HistoryEntry {
            channel: d.channels[ch].key,
            programme: Programme {
                start,
                stop: start + len,
                title: format!("Show {start}"),
                description: None,
                category: None,
                icon: None,
            },
            position_secs: pos,
            watched_at: watched,
        }
    }

    /// Two entries watched today, two yesterday, one three days ago; most recent first as
    /// storage answers.
    fn fixture() -> Fixture {
        let mut d = Fixture::new(NOW, 6);
        d.history = vec![
            entry(&d, 0, NOW - 3 * H, H, 600, NOW - H),
            entry(&d, 1, NOW - 5 * H, H, 3590, NOW - 2 * H),
            entry(&d, 3, NOW - 30 * H, H, 1200, NOW - 26 * H),
            entry(&d, 4, NOW - 31 * H, 2 * H, 60, NOW - 27 * H),
            entry(&d, 0, NOW - 75 * H, H, 900, NOW - 74 * H),
        ];
        d
    }

    #[test]
    fn groups_by_watch_day_oldest_first() {
        let d = fixture();
        let h = History::open(&d);
        assert_eq!(
            h.lines(),
            &[
                Line::Day(-3),
                Line::Entry(0),
                Line::Day(-1),
                Line::Entry(1),
                Line::Entry(2),
                Line::Day(0),
                Line::Entry(3),
                Line::Entry(4),
            ]
        );
        // Opens on the most recent entry.
        assert_eq!(h.focused(), Some(4));
        assert_eq!(h.focused_entry().unwrap().watched_at, NOW - H);
        assert_eq!(h.facts(0).unwrap().name(), "Channel 1");
        assert_eq!(h.video_rect(), None, "the video stays where it was");
    }

    #[test]
    fn day_grouping_follows_the_local_offset() {
        let mut d = fixture();
        // At UTC+10 it is 01:12 tomorrow: only the entry watched an hour ago is today, the one
        // watched two hours ago is yesterday, and the rest move a day back.
        d.utc_offset = 10 * 3600;
        let h = History::open(&d);
        assert_eq!(
            h.lines(),
            &[
                Line::Day(-4),
                Line::Entry(0),
                Line::Day(-2),
                Line::Entry(1),
                Line::Entry(2),
                Line::Day(-1),
                Line::Entry(3),
                Line::Day(0),
                Line::Entry(4),
            ]
        );
    }

    #[test]
    fn navigation_skips_headers() {
        let d = fixture();
        let mut h = History::open(&d);
        h.nav(Nav::Up);
        assert_eq!(h.focused(), Some(3));
        h.nav(Nav::Up);
        assert_eq!(h.focused(), Some(2), "skips the Today header");
        h.nav(Nav::Up);
        h.nav(Nav::Up);
        assert_eq!(h.focused(), Some(0));
        assert!(!h.nav(Nav::Up).redraw);
        assert_eq!(h.focused(), Some(0), "the top holds");
        assert_eq!(h.cursor().top, 0, "and shows its day header");
        assert!(h.nav(Nav::PageDown).redraw);
        assert_eq!(h.focused(), Some(4));
        h.nav(Nav::Top);
        assert_eq!(h.focused(), Some(0));
        assert_eq!(h.cursor().top, 0);
    }

    #[test]
    fn ok_resumes_at_the_saved_position_in_full_screen() {
        let d = fixture();
        let mut h = History::open(&d);
        let e = h.focused_entry().unwrap().clone();
        let out = h.key(Key::Ok, Press::Short, &d, 0);
        assert_eq!(
            out.commands,
            [Command::Play {
                stream: Stream::Catchup {
                    channel: e.channel,
                    programme: e.programme,
                    offset_secs: 600
                },
                group: None,
            }]
        );
        assert_eq!(out.navigate, [Navigate::Player]);
    }

    #[test]
    fn a_finished_programme_restarts() {
        let d = fixture();
        let mut h = History::open(&d);
        h.nav(Nav::Up);
        let out = h.nav(Nav::Ok);
        assert!(matches!(
            out.commands.as_slice(),
            [Command::Play {
                stream: Stream::Catchup { offset_secs: 0, .. },
                ..
            }]
        ));
        assert_eq!(resume_at(&d.history[1]), 0);
        assert_eq!(resume_at(&d.history[0]), 600);
    }

    #[test]
    fn entries_outside_the_archive_do_not_play() {
        let mut d = fixture();
        d.channels[0].catchup.days = 2;
        // Channel 2 never had catch-up; a key the playlist lost has no channel at all.
        let no_catchup = entry(&d, 2, NOW - 4 * H, H, 100, NOW - 76 * H);
        d.history.push(no_catchup);
        let mut gone = entry(&d, 5, NOW - 4 * H, H, 100, NOW - 77 * H);
        gone.channel = ChannelKey(1);
        d.history.push(gone);
        let mut h = History::open(&d);
        assert!(!h.facts(0).unwrap().playable, "no channel");
        assert!(h.facts(0).unwrap().name().is_empty());
        assert!(!h.facts(1).unwrap().playable, "no catch-up");
        assert!(!h.facts(2).unwrap().playable, "older than 2 days");
        assert!(h.facts(6).unwrap().playable);
        h.nav(Nav::Top);
        let out = h.nav(Nav::Ok);
        assert!(out.commands.is_empty() && out.navigate.is_empty());
    }

    #[test]
    fn delete_removes_the_row_and_keeps_the_place() {
        let d = fixture();
        let mut h = History::open(&d);
        h.nav(Nav::Up);
        h.nav(Nav::Up);
        assert_eq!(h.focused(), Some(2));
        h.nav(Nav::Right);
        assert!(h.on_delete());
        let e = h.focused_entry().unwrap().clone();
        let out = h.nav(Nav::Ok);
        assert_eq!(
            out.commands,
            [Command::RemoveHistory {
                channel: e.channel,
                start: e.programme.start
            }]
        );
        assert!(out.navigate.is_empty() && out.redraw);
        assert_eq!(h.entries().len(), 4);
        assert!(
            h.on_delete(),
            "the delete button keeps the focus for the next row"
        );
        // The Today entry that followed moved up into index 2.
        assert_eq!(h.focused(), Some(2));
        assert_eq!(h.focused_entry().unwrap().watched_at, NOW - 2 * H);
        h.nav(Nav::Left);
        assert!(!h.on_delete());
    }

    #[test]
    fn deleting_a_group_s_only_entry_drops_its_header() {
        let d = fixture();
        let mut h = History::open(&d);
        h.nav(Nav::Top);
        h.nav(Nav::Right);
        h.nav(Nav::Ok);
        assert_eq!(h.lines().first(), Some(&Line::Day(-1)));
        assert_eq!(h.focused(), Some(0));
        for _ in 0..4 {
            h.nav(Nav::Ok);
        }
        assert!(h.entries().is_empty());
        assert!(h.lines().is_empty());
        assert!(!h.on_delete());
        assert_eq!(h.nav(Nav::Ok), Output::default());
        assert_eq!(h.nav(Nav::Back), Output::close());
    }

    #[test]
    fn empty_history() {
        let d = Fixture::new(NOW, 3);
        let mut h = History::open(&d);
        assert!(h.lines().is_empty());
        assert_eq!(h.focused(), None);
        for k in [
            Nav::Up,
            Nav::Down,
            Nav::Right,
            Nav::Ok,
            Nav::PageDown,
            Nav::Top,
        ] {
            assert_eq!(h.nav(k), Output::default());
        }
        assert!(!h.on_delete());
    }

    #[test]
    fn midnight_moves_the_headers() {
        let mut d = fixture();
        let mut h = History::open(&d);
        let focused = h.focused();
        d.now = NOW + 60;
        assert!(!h.tick(&d, 5_000).redraw);
        let midnight = time::day_start(NOW, 0) + DAY;
        assert_eq!(
            h.next_wake(9_000),
            Some(5_000 + (midnight - NOW - 60) as u64 * 1000),
            "the only wake is midnight"
        );
        d.now = midnight;
        assert!(h.tick(&d, 6_000).redraw);
        assert_eq!(h.lines()[0], Line::Day(-4));
        assert!(h.lines().contains(&Line::Day(-1)));
        assert!(!h.lines().contains(&Line::Day(0)));
        assert_eq!(h.focused(), focused);
    }

    #[test]
    fn refresh_keeps_the_focused_entry() {
        let mut d = fixture();
        let mut h = History::open(&d);
        h.nav(Nav::Up);
        let e = h.focused_entry().unwrap().clone();
        // Storage dropped the oldest entry.
        d.history.pop();
        h.refresh(&d);
        assert_eq!(h.entries().len(), 4);
        assert_eq!(h.focused_entry(), Some(&e));
        d.history.clear();
        h.refresh(&d);
        assert_eq!(h.focused(), None);
    }
}
