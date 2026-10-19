//! The groups panel's list: Favorites, All channels and History first, then the playlist's
//! groups in playlist order. Moving the focus selects the group (the guide filters as you go).

use super::geom::GROUP_LIST;
use crate::data::Group;
use crate::lists::Cursor;

pub struct Groups {
    pub list: Vec<Group>,
    /// The focused group (which is also the one the guide shows) and the first visible row.
    pub cursor: Cursor,
}

/// Display order: the three built-in groups first (Favorites always leads), playlist groups
/// after them in the order the data gives.
pub fn ordered(mut groups: Vec<Group>) -> Vec<Group> {
    groups.sort_by_key(|g| match g {
        Group::Favorites => 0,
        Group::All => 1,
        Group::History => 2,
        Group::Playlist(_) => 3,
    });
    groups
}

impl Groups {
    pub fn new(groups: Vec<Group>, current: &Group) -> Self {
        let mut g = Groups {
            list: Vec::new(),
            cursor: Cursor::default(),
        };
        g.set(groups, current);
        g
    }

    /// Replace the list, keeping focus on `current` when it is still there, else on All
    /// channels, else on the first group.
    pub fn set(&mut self, groups: Vec<Group>, current: &Group) {
        self.list = ordered(groups);
        let at = |group: &Group| self.list.iter().position(|g| g == group);
        let focus = at(current).or_else(|| at(&Group::All)).unwrap_or(0);
        self.cursor.set(focus, self.list.len(), GROUP_LIST);
    }

    /// Focus `group` when it is listed.
    pub fn select(&mut self, group: &Group) {
        if let Some(i) = self.list.iter().position(|g| g == group) {
            self.go(i);
        }
    }

    pub fn focused(&self) -> Option<&Group> {
        self.list.get(self.cursor.index)
    }

    /// Move the focus by `by` rows, stopping at the ends. Returns whether it moved.
    pub fn step(&mut self, by: isize) -> bool {
        let Some(last) = self.list.len().checked_sub(1) else {
            return false;
        };
        self.go(self.cursor.index.saturating_add_signed(by).min(last))
    }

    /// Long Left: the top of the list.
    pub fn first(&mut self) -> bool {
        self.go(0)
    }

    fn go(&mut self, focus: usize) -> bool {
        if focus == self.cursor.index || focus >= self.list.len() {
            return false;
        }
        self.cursor.set(focus, self.list.len(), GROUP_LIST);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guide::geom;

    fn playlist(name: &str) -> Group {
        Group::Playlist(name.into())
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
        let mut g = Groups::new(
            vec![Group::Favorites, Group::All, playlist("A")],
            &playlist("A"),
        );
        assert_eq!(g.cursor.index, 2);
        assert!(!g.step(1), "no wrap at the bottom");
        assert!(g.step(-1));
        assert_eq!(g.focused(), Some(&Group::All));
        assert!(g.first());
        assert_eq!(g.cursor.index, 0);
        assert!(!g.first());
        assert!(!g.step(-1), "no wrap at the top");
        assert!(g.step(10));
        assert_eq!(g.cursor.index, 2);
    }

    #[test]
    fn a_vanished_group_falls_back_to_all_channels() {
        let mut g = Groups::new(vec![Group::All, playlist("A")], &playlist("A"));
        g.set(
            vec![Group::Favorites, Group::All, playlist("B")],
            &playlist("A"),
        );
        assert_eq!(g.focused(), Some(&Group::All));
        g.set(vec![playlist("C"), playlist("B")], &playlist("A"));
        assert_eq!(g.cursor.index, 0);
        let empty = Groups::new(vec![], &Group::All);
        assert_eq!(empty.focused(), None);
    }

    #[test]
    fn long_lists_scroll() {
        let list: Vec<Group> = (0..40).map(|i| playlist(&format!("G{i}"))).collect();
        let mut g = Groups::new(list, &Group::All);
        for _ in 0..geom::GROUP_ROWS {
            g.step(1);
        }
        assert_eq!(g.cursor.index, geom::GROUP_ROWS);
        assert_eq!(g.cursor.top, 1);
        g.first();
        assert_eq!(g.cursor.top, 0);
    }
}
