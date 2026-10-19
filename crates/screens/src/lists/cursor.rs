//! A focus index plus the first visible row of a fixed-height list, the one scrolling rule every
//! list shares: the focus keeps its rows of context ([`Rows`]) above and below while the list can
//! scroll.

/// The whole rows a list shows and the rows of context the focus keeps above and below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rows {
    pub shown: usize,
    pub above: usize,
    pub below: usize,
}

impl Rows {
    /// `shown` rows, keeping `margin` rows of context on both sides of the focus.
    pub const fn new(shown: usize, margin: usize) -> Self {
        Rows {
            shown,
            above: margin,
            below: margin,
        }
    }
}

impl From<usize> for Rows {
    /// The lists' rule: one row of context once three rows fit.
    fn from(shown: usize) -> Self {
        Rows::new(shown, usize::from(shown >= 3))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub index: usize,
    pub top: usize,
}

impl Cursor {
    /// Focus `index` (clamped) in a list of `len` rows showing `rows` at a time.
    pub fn set(&mut self, index: usize, len: usize, rows: impl Into<Rows>) {
        if len == 0 {
            *self = Cursor::default();
            return;
        }
        self.index = index.min(len - 1);
        self.scroll(len, rows);
    }

    /// Move by `delta`; `wrap` jumps from one end to the other, otherwise the ends hold.
    pub fn step(&mut self, delta: isize, len: usize, rows: impl Into<Rows>, wrap: bool) {
        if len == 0 {
            return;
        }
        let last = len as isize - 1;
        let to = self.index as isize + delta;
        let to = if wrap && delta.abs() == 1 {
            if to < 0 {
                last
            } else if to > last {
                0
            } else {
                to
            }
        } else {
            to.clamp(0, last)
        };
        self.set(to as usize, len, rows);
    }

    /// Re-establish the scroll after `index` or `len` changed.
    pub fn scroll(&mut self, len: usize, rows: impl Into<Rows>) {
        let rows = rows.into();
        let shown = rows.shown.max(1);
        // Context never pushes the focus itself off screen.
        let above = rows.above.min(shown - 1);
        let below = rows.below.min(shown - 1 - above);
        let max_top = len.saturating_sub(shown);
        if self.index < self.top + above {
            self.top = self.index.saturating_sub(above);
        } else if self.index + below >= self.top + shown {
            self.top = self.index + below + 1 - shown;
        }
        self.top = self.top.min(max_top);
    }

    /// The rows on screen: `top..top + shown`, cut at `len`.
    pub fn visible(&self, len: usize, rows: impl Into<Rows>) -> std::ops::Range<usize> {
        self.top.min(len)..(self.top + rows.into().shown).min(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_a_row_of_context_while_scrolling() {
        let mut c = Cursor::default();
        for _ in 0..6 {
            c.step(1, 20, 5, false);
        }
        assert_eq!(c, Cursor { index: 6, top: 3 });
        c.step(-1, 20, 5, false);
        c.step(-1, 20, 5, false);
        c.step(-1, 20, 5, false);
        assert_eq!(c, Cursor { index: 3, top: 2 });
        assert_eq!(c.visible(20, 5), 2..7);
    }

    #[test]
    fn ends_hold_or_wrap() {
        let mut c = Cursor::default();
        c.step(-1, 10, 4, false);
        assert_eq!(c.index, 0);
        c.step(-1, 10, 4, true);
        assert_eq!(c, Cursor { index: 9, top: 6 });
        c.step(1, 10, 4, true);
        assert_eq!(c, Cursor { index: 0, top: 0 });
        // Paging never wraps.
        c.step(-8, 10, 4, true);
        assert_eq!(c.index, 0);
        c.step(8, 10, 4, true);
        assert_eq!(c.index, 8);
    }

    #[test]
    fn short_and_empty_lists() {
        let mut c = Cursor::default();
        c.set(5, 3, 8);
        assert_eq!(c, Cursor { index: 2, top: 0 });
        c.set(5, 0, 8);
        assert_eq!(c, Cursor::default());
        assert_eq!(c.visible(0, 8), 0..0);
        c.step(1, 0, 8, true);
        assert_eq!(c, Cursor::default());
    }

    #[test]
    fn margins_are_the_callers() {
        // No context: the focus may sit on the first and last rows (the side panels).
        let mut c = Cursor::default();
        let edge = Rows::new(4, 0);
        c.set(3, 10, edge);
        assert_eq!(c, Cursor { index: 3, top: 0 });
        c.step(1, 10, edge, false);
        assert_eq!(c, Cursor { index: 4, top: 1 });
        c.step(-1, 10, edge, false);
        assert_eq!(c.top, 1);
        // The guide: seven whole rows and the top of an eighth, which is the context below.
        let guide = Rows {
            shown: 7,
            above: 1,
            below: 0,
        };
        for (top, focus, len, want) in [
            (0, 4, 5, 0),
            (0, 6, 100, 0),
            (0, 7, 100, 1),
            (1, 8, 100, 2),
            (10, 10, 100, 9),
            (10, 11, 100, 10),
            (93, 0, 100, 0),
            (0, 99, 100, 93),
        ] {
            let mut c = Cursor { index: focus, top };
            c.scroll(len, guide);
            assert_eq!(c.top, want, "top {top}, focus {focus}, {len} rows");
        }
        // Margins wider than the list allows still keep the focus on screen.
        let mut c = Cursor::default();
        c.set(5, 10, Rows::new(2, 3));
        assert!(c.visible(10, 2).contains(&5));
        assert_eq!(Rows::from(2), Rows::new(2, 0));
        assert_eq!(Rows::from(8), Rows::new(8, 1));
    }

    #[test]
    fn set_scrolls_a_far_index_into_view() {
        let mut c = Cursor::default();
        c.set(50, 100, 8);
        assert!(c.visible(100, 8).contains(&50));
        c.set(99, 100, 8);
        assert_eq!(c, Cursor { index: 99, top: 92 });
    }
}
