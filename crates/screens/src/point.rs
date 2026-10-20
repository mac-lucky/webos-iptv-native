//! The Magic Remote's pointer: what each screen lets it focus.
//!
//! While its cursor shows, the remote reports where it points and sends its OK as a click (the
//! app turns the click into OK and the wheel into Up and Down). A screen records a [`Spot`] for
//! each row, tile, guide cell or menu item as it lays them out, in drawing order, and
//! [`Screen::point`](crate::data::Screen::point) focuses the one under the pointer the way the
//! arrow keys would. Pointing never scrolls a list
//! ([`Cursor::point`](crate::lists::Cursor::point)): only what is on screen can be pointed at,
//! and the wheel or the keys scroll. A row drawn but not pointable (cut by the screen's edge, or
//! in a column that would move if it took the focus) is a dead spot: a click there does nothing
//! ([`Screen::dead`](crate::data::Screen::dead)), where a click on nothing at all (the video, a
//! details card) is OK on the focus.

use tv_ui::Rect;

/// A place the pointer can focus, in the screen's own terms: which of its lists or columns
/// (`area`), the row in it (`index`) and, where a row holds several places (the guide's
/// programmes, by start), which one (`item`, 0 elsewhere).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spot {
    pub area: u8,
    pub index: usize,
    pub item: i64,
}

impl Spot {
    /// Row `index` of list `area`.
    pub const fn row(area: u8, index: usize) -> Self {
        Spot {
            area,
            index,
            item: 0,
        }
    }
}

/// What one rectangle of [`Spots`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    Spot(Spot),
    /// No spot, hiding the ones under it; a click on it is OK on the focus.
    Cover,
    /// No spot, hiding the ones under it; a click on it does nothing.
    Dead,
}

/// A screen's spots as last laid out, in drawing order: the last one over a point is on top.
#[derive(Clone, Debug, Default)]
pub struct Spots(Vec<(Rect, Mark)>);

impl Spots {
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// `spot`, drawn at `r` over the spots added before it.
    pub fn add(&mut self, r: Rect, spot: Spot) {
        self.0.push((r, Mark::Spot(spot)));
    }

    /// Something drawn at `r` that hides the spots under it and is no spot itself (a video
    /// window over the guide).
    pub fn cover(&mut self, r: Rect) {
        self.0.push((r, Mark::Cover));
    }

    /// Rows drawn at `r` that the pointer cannot focus: a click on them does nothing, rather
    /// than OK on a focus somewhere else.
    pub fn dead(&mut self, r: Rect) {
        self.0.push((r, Mark::Dead));
    }

    /// Every spot with where it is drawn, bottom first (tests aim at them).
    #[cfg(test)]
    pub fn rects(&self) -> impl Iterator<Item = (Rect, Spot)> + '_ {
        self.0.iter().filter_map(|&(r, m)| match m {
            Mark::Spot(s) => Some((r, s)),
            _ => None,
        })
    }

    /// The spot on top at `(x, y)`; edges belong to the spot right or below them.
    pub fn at(&self, x: f32, y: f32) -> Option<Spot> {
        match self.top(x, y)? {
            Mark::Spot(s) => Some(s),
            _ => None,
        }
    }

    /// A dead spot ([`dead`](Self::dead)) is on top at `(x, y)`.
    pub fn dead_at(&self, x: f32, y: f32) -> bool {
        self.top(x, y) == Some(Mark::Dead)
    }

    fn top(&self, x: f32, y: f32) -> Option<Mark> {
        let inside = |r: &Rect| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h;
        self.0
            .iter()
            .rev()
            .find(|(r, _)| inside(r))
            .map(|&(_, m)| m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spot_drawn_last_wins_and_a_cover_hides_what_is_under_it() {
        let mut s = Spots::default();
        s.add(Rect::new(0.0, 0.0, 100.0, 50.0), Spot::row(1, 0));
        s.add(Rect::new(0.0, 50.0, 100.0, 50.0), Spot::row(1, 1));
        s.add(Rect::new(80.0, 0.0, 40.0, 40.0), Spot::row(2, 7));
        assert_eq!(s.at(10.0, 10.0), Some(Spot::row(1, 0)));
        assert_eq!(s.at(10.0, 50.0), Some(Spot::row(1, 1)), "an edge is below");
        assert_eq!(s.at(90.0, 10.0), Some(Spot::row(2, 7)), "drawn over row 0");
        assert_eq!(s.at(100.0, 60.0), None, "right of the rows");
        assert_eq!(s.at(-1.0, 10.0), None);
        s.cover(Rect::new(0.0, 60.0, 20.0, 20.0));
        assert_eq!(s.at(10.0, 70.0), None, "under the cover");
        assert!(!s.dead_at(10.0, 70.0), "a cover is no dead spot");
        assert_eq!(s.at(30.0, 70.0), Some(Spot::row(1, 1)));
        s.dead(Rect::new(0.0, 100.0, 100.0, 20.0));
        assert_eq!(s.at(10.0, 110.0), None);
        assert!(s.dead_at(10.0, 110.0));
        assert!(!s.dead_at(10.0, 10.0) && !s.dead_at(10.0, 200.0));
        // Drawn over a dead spot, a spot wins.
        s.add(Rect::new(0.0, 100.0, 10.0, 20.0), Spot::row(3, 0));
        assert_eq!(s.at(5.0, 110.0), Some(Spot::row(3, 0)));
        assert!(!s.dead_at(5.0, 110.0));
        s.clear();
        assert_eq!(s.at(10.0, 10.0), None);
    }
}
