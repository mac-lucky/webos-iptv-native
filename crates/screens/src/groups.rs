//! The groups panel the guide and the channel list share: Favorites, All channels and History
//! first, then the playlist's groups in playlist order, the focused one a white pill. Moving the
//! focus shows its group in the screen beside the panel (it filters as you go); History is a
//! screen of its own, so focusing it keeps the group shown, marked in the accent.
//!
//! The panel is a column on the left edge of the screen; each screen gives its [`Frame`]. What a
//! key means for the screen holding the panel comes back as an [`Act`]:
//!
//! | Key | Groups panel |
//! |---|---|
//! | Up / Down | group, stopping at the ends (repeats) |
//! | CH+ / CH- | a page of groups (repeats) |
//! | Left | [`Navigate::MainMenu`] |
//! | long Left | top of the list |
//! | OK / Right | [`Act::Enter`]; on History [`Navigate::History`] |
//! | long OK | [`Navigate::GroupMenu`] |
//! | Back | [`Act::Back`] |
//!
//! The pointer focuses the group it rests on, as Up and Down do ([`GroupsPanel::point`]).

use tv_machine::machine::Measure;
use tv_ui::{Painter, Rect};

use crate::data::{Group, Logos, Navigate};
use crate::draw::{self, HAlign, Piece, Run};
use crate::input::{Hold, Key};
use crate::lists::{Cursor, Rows};
use crate::point::{Spot, Spots};
use crate::theme::{self, radius, size, space};

const SCREEN_H: f32 = 1080.0;
/// Every panel's pills are this wide, so a group name fits the same in each.
pub const PILL_W: f32 = 440.0;
pub const PILL_H: f32 = 72.0;
pub const ROW_H: f32 = 77.0;
pub const TEXT_PAD: f32 = space::L;
/// The first row's top.
pub const TOP: f32 = space::L;
/// Whole rows on screen.
pub const ROWS: usize = ((SCREEN_H - TOP) / ROW_H) as usize;
/// The list scrolls only to keep the focus on screen.
const LIST: Rows = Rows::new(ROWS, 0);

/// Where a panel stands: a column of width `w` from the left edge of the screen, its pills
/// `pill_x` from that edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    pub w: f32,
    pub pill_x: f32,
}

impl Frame {
    /// The column the panel covers.
    pub fn column(&self) -> Rect {
        Rect::new(0.0, 0.0, self.w, SCREEN_H)
    }

    /// The pill of the `row`th row on screen.
    pub fn pill(&self, row: usize) -> Rect {
        Rect::new(self.pill_x, TOP + row as f32 * ROW_H, PILL_W, PILL_H)
    }
}

/// What a key in the panel asks of the screen holding it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Act {
    /// Nothing to do.
    None,
    /// The focus moved: show [`GroupsPanel::shown`] (which it did not change on History).
    Moved,
    /// OK or Right on a group: back to the list beside the panel, which shows it.
    Enter,
    /// A screen the panel opens the same from every host: History (OK or Right on it, a screen
    /// of its own), the group's menu (long OK) and the main menu (Left).
    Go(Navigate),
    /// Back: the screen holding the panel decides (close the panel, or the screen too).
    Back,
}

/// Display order: the three built-in groups first (Favorites always leads), playlist groups
/// after them in the order the data gives.
pub fn ordered(mut groups: Vec<Group>) -> Vec<Group> {
    groups.sort_by_key(|g| match g {
        Group::Favorites => 0,
        Group::All => 1,
        Group::History => 2,
        Group::Playlist(_) | Group::Custom(_) => 3,
    });
    groups
}

pub struct GroupsPanel {
    list: Vec<Group>,
    /// The focused group and the first visible row.
    cursor: Cursor,
    /// The group the screen beside the panel shows: the focus, or what it was before History.
    shown: Option<usize>,
    frame: Frame,
    /// The groups' names fitted to the pills, once the panel was first laid out.
    names: Vec<Run>,
    pieces: Vec<Piece>,
    /// The whole rows of the last layout, for the pointer, and the cut one under them (dead).
    spots: Spots,
    /// The state changed since the last layout.
    stale: bool,
}

impl GroupsPanel {
    /// The panel over `groups`, focused on `current` (see [`set`](Self::set)).
    pub fn new(groups: Vec<Group>, current: &Group, frame: Frame) -> Self {
        let mut g = GroupsPanel {
            list: Vec::new(),
            cursor: Cursor::default(),
            shown: None,
            frame,
            names: Vec::new(),
            pieces: Vec::new(),
            spots: Spots::default(),
            stale: true,
        };
        g.set(groups, current);
        g
    }

    /// Replace the list, keeping focus on `current` when it is still there, else on All
    /// channels, else on the first group that is not History.
    pub fn set(&mut self, groups: Vec<Group>, current: &Group) {
        let list = ordered(groups);
        // The names stay fitted while the list is the same (a refresh that changed no group).
        if list != self.list {
            self.names.clear();
            self.list = list;
        }
        let at = |group: &Group| self.list.iter().position(|g| g == group);
        let focus = at(current)
            .or_else(|| at(&Group::All))
            .or_else(|| self.list.iter().position(|g| *g != Group::History))
            .unwrap_or(0);
        self.shown = None;
        self.cursor = Cursor::default();
        self.focus(focus);
    }

    /// Focus (and show) `group` when it is listed.
    pub fn select(&mut self, group: &Group) {
        if let Some(i) = self.list.iter().position(|g| g == group) {
            self.focus(i);
        }
    }

    pub fn list(&self) -> &[Group] {
        &self.list
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn focused(&self) -> Option<&Group> {
        self.list.get(self.cursor.index)
    }

    /// The group the screen beside the panel shows; `None` while there is none to show (an
    /// empty list, or only History).
    pub fn shown(&self) -> Option<&Group> {
        self.shown.and_then(|i| self.list.get(i))
    }

    /// How the panel wants `key` held.
    pub fn hold(key: Key) -> Hold {
        match key {
            Key::Up | Key::Down | Key::ChannelUp | Key::ChannelDown => Hold::Repeat,
            Key::Left | Key::Ok => Hold::Long,
            _ => Hold::Once,
        }
    }

    /// One key, `long` when held past the threshold.
    pub fn key(&mut self, key: Key, long: bool) -> Act {
        let page = ROWS as isize - 1;
        let moved = match (key, long) {
            (Key::Up, _) => self.step(-1),
            (Key::Down, _) => self.step(1),
            (Key::ChannelUp, _) => self.step(-page),
            (Key::ChannelDown, _) => self.step(page),
            (Key::Left, true) => self.go(0),
            (Key::Left, false) => return Act::Go(Navigate::MainMenu),
            (Key::Ok, true) => {
                let menu = |g: &Group| Act::Go(Navigate::GroupMenu(g.clone()));
                return self.focused().map_or(Act::None, menu);
            }
            (Key::Ok | Key::Right, _) if self.focused() == Some(&Group::History) => {
                return Act::Go(Navigate::History)
            }
            (Key::Ok | Key::Right, _) => return Act::Enter,
            (Key::Back, _) => return Act::Back,
            _ => false,
        };
        if moved {
            Act::Moved
        } else {
            Act::None
        }
    }

    /// Move the focus by `by` rows, stopping at the ends. Returns whether it moved.
    fn step(&mut self, by: isize) -> bool {
        let Some(last) = self.list.len().checked_sub(1) else {
            return false;
        };
        self.go(self.cursor.index.saturating_add_signed(by).min(last))
    }

    fn go(&mut self, focus: usize) -> bool {
        if focus == self.cursor.index || focus >= self.list.len() {
            return false;
        }
        self.focus(focus);
        true
    }

    /// The row under the last whole one, which shows only its top, is at `(x, y)`: the pointer
    /// cannot focus it ([`crate::point`]).
    pub fn dead(&self, x: f32, y: f32) -> bool {
        self.spots.dead_at(x, y)
    }

    /// The pointer at `(x, y)`: focus the group it rests on, as Up and Down do.
    pub fn point(&mut self, x: f32, y: f32) -> Act {
        match self.spots.at(x, y) {
            Some(s) if self.cursor.point(s.index, self.list.len(), LIST) => {
                self.focus(s.index);
                Act::Moved
            }
            _ => Act::None,
        }
    }

    fn focus(&mut self, focus: usize) {
        self.cursor.set(focus, self.list.len(), LIST);
        if self.list.get(focus).is_some_and(|g| *g != Group::History) {
            self.shown = Some(focus);
        }
        self.stale = true;
    }

    /// Lay the panel out when it changed; the names are fitted once per list.
    pub fn prepare(&mut self, m: &dyn Measure) {
        if !self.stale {
            return;
        }
        if self.names.len() != self.list.len() {
            let w = PILL_W - 2.0 * TEXT_PAD;
            self.names = self
                .list
                .iter()
                .map(|g| m.fit_line(g.name(), w, size::BODY, false))
                .collect();
        }
        self.pieces.clear();
        self.pieces
            .push(Piece::solid(self.frame.column(), 0.0, theme::PANEL));
        self.spots.clear();
        let top = self.cursor.top;
        // The row under the last whole one shows its top, as the list goes on.
        let end = (top + ROWS + 1).min(self.list.len());
        let whole = self.cursor.visible(self.list.len(), LIST);
        for i in top..end {
            let pill = self.frame.pill(i - top);
            let band = Rect::new(0.0, pill.y, self.frame.w, ROW_H);
            if whole.contains(&i) {
                self.spots.add(band, Spot::row(0, i));
            } else {
                self.spots.dead(band);
            }
            let focused = i == self.cursor.index;
            let ink = if focused {
                self.pieces
                    .push(Piece::solid(pill, radius::PILL, theme::FOCUS));
                theme::TEXT_ON_FOCUS
            } else if self.shown == Some(i) {
                theme::ACCENT_TEXT
            } else {
                theme::TEXT
            };
            let text = Rect::new(pill.x + TEXT_PAD, pill.y, pill.w - 2.0 * TEXT_PAD, pill.h);
            self.pieces.push(Piece::text_in(
                self.names[i].clone(),
                text,
                HAlign::Left,
                size::BODY,
                false,
                ink,
            ));
        }
        self.stale = false;
    }

    /// What the last [`prepare`](Self::prepare) laid out.
    pub fn pieces(&self) -> &[Piece] {
        &self.pieces
    }

    pub fn paint(&self, p: Painter, logos: &dyn Logos) {
        draw::paint(&self.pieces, p, logos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fake::Mono;

    const FRAME: Frame = Frame {
        w: 488.0,
        pill_x: 24.0,
    };

    fn playlist(name: &str) -> Group {
        Group::Playlist(name.into())
    }

    fn panel(groups: Vec<Group>, current: &Group) -> GroupsPanel {
        GroupsPanel::new(groups, current, FRAME)
    }

    /// The text pieces laid out: their run and colour.
    fn texts(g: &mut GroupsPanel) -> Vec<(String, [f32; 4])> {
        g.prepare(&Mono);
        g.pieces()
            .iter()
            .filter_map(|p| match p {
                Piece::Text { s, color, .. } => Some((s.to_string_lossy().into_owned(), *color)),
                _ => None,
            })
            .collect()
    }

    fn pills(g: &GroupsPanel) -> Vec<Rect> {
        g.pieces()
            .iter()
            .filter_map(|p| match p {
                Piece::Fill { r, top, .. } if *top == theme::FOCUS => Some(*r),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn favorites_lead_and_playlist_order_is_kept() {
        let g = ordered(vec![
            playlist("News"),
            Group::History,
            playlist("Sport"),
            Group::All,
            Group::Favorites,
            playlist("Movies"),
        ]);
        assert_eq!(
            g,
            vec![
                Group::Favorites,
                Group::All,
                Group::History,
                playlist("News"),
                playlist("Sport"),
                playlist("Movies"),
            ]
        );
    }

    #[test]
    fn focus_stops_at_the_ends_and_long_left_goes_to_the_top() {
        let mut g = panel(
            vec![Group::Favorites, Group::All, playlist("A")],
            &playlist("A"),
        );
        assert_eq!(g.cursor().index, 2);
        assert_eq!(g.key(Key::Down, false), Act::None, "no wrap at the bottom");
        assert_eq!(g.key(Key::Up, false), Act::Moved);
        assert_eq!(g.focused(), Some(&Group::All));
        assert_eq!(g.shown(), Some(&Group::All));
        assert_eq!(g.key(Key::Left, true), Act::Moved);
        assert_eq!(g.cursor().index, 0);
        assert_eq!(g.key(Key::Left, true), Act::None);
        assert_eq!(g.key(Key::Up, false), Act::None, "no wrap at the top");
        assert_eq!(g.key(Key::ChannelDown, false), Act::Moved);
        assert_eq!(g.cursor().index, 2);
    }

    #[test]
    fn the_keys_ask_the_screen_beside_it() {
        let mut g = panel(
            vec![Group::Favorites, Group::All, Group::History, playlist("A")],
            &Group::All,
        );
        assert_eq!(g.key(Key::Ok, false), Act::Enter);
        assert_eq!(g.key(Key::Right, false), Act::Enter);
        assert_eq!(
            g.key(Key::Ok, true),
            Act::Go(Navigate::GroupMenu(Group::All))
        );
        assert_eq!(g.key(Key::Left, false), Act::Go(Navigate::MainMenu));
        assert_eq!(g.key(Key::Back, false), Act::Back);
        assert_eq!(g.key(Key::Red, false), Act::None);
        // History opens its own screen and shows nothing: the guide keeps All.
        assert_eq!(g.key(Key::Down, false), Act::Moved);
        assert_eq!(g.focused(), Some(&Group::History));
        assert_eq!(g.shown(), Some(&Group::All));
        assert_eq!(g.key(Key::Ok, false), Act::Go(Navigate::History));
        assert_eq!(g.key(Key::Right, false), Act::Go(Navigate::History));
        assert_eq!(g.key(Key::Down, false), Act::Moved);
        assert_eq!(g.shown(), Some(&playlist("A")));
        assert_eq!(GroupsPanel::hold(Key::Down), Hold::Repeat);
        assert_eq!(GroupsPanel::hold(Key::Left), Hold::Long);
        assert_eq!(GroupsPanel::hold(Key::Right), Hold::Once);
        assert_eq!(GroupsPanel::hold(Key::Back), Hold::Once, "never held");
    }

    #[test]
    fn a_vanished_group_falls_back_to_all_channels() {
        let mut g = panel(vec![Group::All, playlist("A")], &playlist("A"));
        g.set(
            vec![Group::Favorites, Group::All, playlist("B")],
            &playlist("A"),
        );
        assert_eq!(g.focused(), Some(&Group::All));
        g.set(vec![playlist("C"), playlist("B")], &playlist("A"));
        assert_eq!(g.cursor().index, 0);
        // Never onto History, which shows nothing.
        g.set(vec![Group::History, playlist("B")], &Group::All);
        assert_eq!(g.shown(), Some(&playlist("B")));
        let mut empty = panel(vec![], &Group::All);
        assert_eq!((empty.focused(), empty.shown()), (None, None));
        assert_eq!(empty.key(Key::Down, false), Act::None);
        assert_eq!(empty.key(Key::Ok, true), Act::None);
    }

    #[test]
    fn long_lists_scroll() {
        let list: Vec<Group> = (0..40).map(|i| playlist(&format!("G{i}"))).collect();
        let mut g = panel(list, &Group::All);
        for _ in 0..ROWS {
            g.key(Key::Down, false);
        }
        assert_eq!(g.cursor().index, ROWS);
        assert_eq!(g.cursor().top, 1);
        let t = texts(&mut g);
        assert_eq!(t.len(), ROWS + 1, "the whole rows and the top of the next");
        assert_eq!(t[0].0, "G1");
        g.key(Key::Left, true);
        assert_eq!(g.cursor().top, 0);
    }

    #[test]
    fn the_focus_is_the_white_pill_and_the_group_shown_is_marked() {
        let mut g = panel(
            vec![Group::Favorites, Group::All, Group::History, playlist("A")],
            &Group::All,
        );
        let t = texts(&mut g);
        assert_eq!(t[1], ("All channels".into(), theme::TEXT_ON_FOCUS));
        assert_eq!(t[0].1, theme::TEXT);
        assert_eq!(pills(&g), [FRAME.pill(1)]);
        assert!(theme::FOCUS[..3].iter().all(|&c| c > 0.85), "white");
        // On History the pill moves and All, still shown, is in the accent.
        g.key(Key::Down, false);
        let t = texts(&mut g);
        assert_eq!(t[1].1, theme::ACCENT_TEXT);
        assert_eq!(t[2], ("History".into(), theme::TEXT_ON_FOCUS));
        assert_eq!(pills(&g), [FRAME.pill(2)]);
        // Every piece stays inside the panel's column.
        let column = FRAME.column();
        for p in g.pieces() {
            if let Piece::Fill { r, .. } = p {
                assert!(r.x >= column.x && r.x + r.w <= column.x + column.w, "{r:?}");
            }
        }
    }

    #[test]
    fn the_pointer_focuses_the_group_it_rests_on() {
        let list: Vec<Group> = (0..40).map(|i| playlist(&format!("G{i}"))).collect();
        let mut g = panel(list, &playlist("G0"));
        g.prepare(&Mono);
        let at = |row: usize| {
            let pill = FRAME.pill(row);
            (pill.x + 10.0, pill.y + 10.0)
        };
        let (x, y) = at(3);
        assert_eq!(g.point(x, y), Act::Moved);
        assert_eq!(
            g.shown(),
            Some(&playlist("G3")),
            "shown at once, as Down does"
        );
        assert_eq!(g.point(x, y + 20.0), Act::None, "the same row");
        assert_eq!(g.point(FRAME.w + 1.0, y), Act::None, "beside the panel");
        // The partial row under the last whole one is not pointed at: it would scroll. A click
        // on it does nothing; one beside the panel is no business of the panel's.
        let (x, y) = at(ROWS);
        assert_eq!(g.point(x, y), Act::None);
        assert_eq!(g.cursor(), Cursor { index: 3, top: 0 });
        assert!(g.dead(x, y));
        assert!(!g.dead(at(3).0, at(3).1) && !g.dead(FRAME.w + 1.0, y));
        // Down carries on from the pointed row.
        assert_eq!(g.key(Key::Down, false), Act::Moved);
        assert_eq!(g.focused(), Some(&playlist("G4")));
    }

    #[test]
    fn an_unchanged_panel_is_not_laid_out_again() {
        let mut g = panel(vec![Group::All, playlist("A")], &Group::All);
        g.prepare(&Mono);
        let first = g.pieces().to_vec();
        g.key(Key::Down, false);
        g.key(Key::Up, false);
        assert!(g.stale);
        g.prepare(&Mono);
        assert!(!g.stale);
        assert_eq!(g.pieces(), first);
        g.prepare(&Mono);
        assert_eq!(g.pieces(), first);
    }
}
