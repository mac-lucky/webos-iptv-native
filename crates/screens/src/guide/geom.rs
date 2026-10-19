//! Guide geometry on the 1920x1080 graphics plane: TiviMate's 1280x720 layout scaled by 1.5,
//! on the grid measures `theme::guide` declares.
//!
//! Everything here is pure arithmetic so the layout can be tested without a GPU. The grid's
//! time axis maps an hour to `theme::guide::HOUR_W`; its window starts at the left edge of the
//! programme cells and runs to the right edge of the screen.

use iptv::model::Timestamp;
use tv_ui::Rect;

use super::clock::HALF_HOUR;
use crate::lists::Rows;
use crate::theme::{guide, space};
use crate::time::MINUTE;

pub const SCREEN_W: f32 = 1920.0;
pub const SCREEN_H: f32 = 1080.0;

/// The preview video: top-left, a third of the screen wide, 16:9.
pub const PREVIEW: Rect = Rect::new(space::XL, space::L, guide::PREVIEW_W, guide::PREVIEW_H);

/// The details column right of the preview.
pub const DETAILS_X: f32 = PREVIEW.x + PREVIEW.w + space::XXL;
pub const DETAILS_RIGHT: f32 = SCREEN_W - space::XL - space::S;
/// Title band (vertical centre 66).
pub const TITLE: Rect = Rect::new(DETAILS_X, 44.0, 1080.0, 44.0);
/// The favourite star, top-right.
pub const STAR: Rect = Rect::new(DETAILS_RIGHT - 32.0, 50.0, 32.0, 32.0);
/// Time range, mini progress bar, minutes and group name share this band (centre 122).
pub const META_Y: f32 = 104.0;
pub const META_H: f32 = 36.0;
pub const PROGRESS_W: f32 = 78.0;
pub const PROGRESS_H: f32 = 4.0;
/// Description lines below the meta band.
pub const DESC_Y: f32 = 156.0;
pub const DESC_LINE_H: f32 = 34.0;
pub const DESC_LINES: usize = 3;

/// The ruler: date and time on the left, half-hour labels over the grid; its line is the top
/// edge of the grid.
pub const RULER_Y: f32 = 414.0;
pub const RULER_H: f32 = GRID_Y - RULER_Y;
pub const RULER_DATE_X: f32 = space::XL;
pub const TICK_H: f32 = space::S;
pub const NOW_DOT_R: f32 = 5.0;
pub const NOW_LINE_W: f32 = 2.0;

/// Left edge of the programme cells; the channel column is left of it.
pub const GRID_X: f32 = guide::GRID_X;
/// The ruler line; rows start just under it.
pub const GRID_Y: f32 = ROWS_Y - CELL_GAP_Y;
pub const ROWS_Y: f32 = guide::GRID_Y;
pub const ROW_H: f32 = guide::ROW_H;
/// Rows on screen: seven whole and the top of an eighth, as in TiviMate.
pub const VISIBLE_ROWS: usize = 8;
/// How the rows scroll: the focus keeps a row of context above it, and the partial eighth row is
/// the context below it.
pub const ROWS: Rows = Rows {
    shown: VISIBLE_ROWS - 1,
    above: 1,
    below: 0,
};
/// Air between rows and between the cells of a row.
pub const CELL_GAP_Y: f32 = space::XS;
pub const CELL_GAP_X: f32 = space::XS;
pub const CELL_PAD_X: f32 = space::M;
/// A cell narrower than this shows no text.
pub const CELL_TEXT_MIN_W: f32 = 2.0 * CELL_PAD_X + space::M;

/// Channel column as (x, width): number, logo tile, name, play mark (or the Manage favorites
/// star), catch-up mark.
pub const NUMBER: (f32, f32) = (24.0, 72.0);
pub const LOGO: (f32, f32) = (102.0, 96.0);
pub const LOGO_H: f32 = 54.0;
/// The logo tile in its row, from the top of the row's band.
pub const LOGO_TILE: Rect = Rect::new(LOGO.0, (ROW_H - LOGO_H) * 0.5, LOGO.1, LOGO_H);
pub const NAME: (f32, f32) = (215.0, 230.0);
pub const MARK: (f32, f32) = (452.0, 28.0);
pub const CATCHUP: (f32, f32) = (493.0, 28.0);

/// Half an hour of guide, in pixels.
pub const HALF_HOUR_W: f32 = guide::HOUR_W * 0.5;
pub const PX_PER_SEC: f32 = HALF_HOUR_W / HALF_HOUR as f32;

/// The groups panel covers the left of the guide, which stays where it is.
pub const GROUPS_W: f32 = 615.0;
pub const GROUP_PILL_X: f32 = 144.0;
pub const GROUP_PILL_W: f32 = 440.0;
pub const GROUP_PILL_H: f32 = 72.0;
pub const GROUP_ROW_H: f32 = 77.0;
pub const GROUP_TEXT_PAD: f32 = space::L;
pub const GROUPS_Y: f32 = space::L;
/// Whole group rows on screen.
pub const GROUP_ROWS: usize = ((SCREEN_H - GROUPS_Y) / GROUP_ROW_H) as usize;
/// The group list scrolls only to keep the focus on screen.
pub const GROUP_LIST: Rows = Rows::new(GROUP_ROWS, 0);

/// Seconds of guide the window shows: from the grid's left edge to the screen's right edge.
pub const fn window_span() -> i64 {
    ((SCREEN_W - GRID_X) / PX_PER_SEC) as i64
}

/// A programme starting later than this into the window pages the window forward, so a focused
/// cell always shows at least this much of itself.
pub const WINDOW_TAIL: i64 = 30 * MINUTE;

/// Screen x of time `t` in the window starting at `window`.
pub fn x_of(t: Timestamp, window: Timestamp) -> f32 {
    GRID_X + (t - window) as f32 * PX_PER_SEC
}

/// One programme's cell in a row, cut to the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellGeom {
    /// Left and right edges of the drawn face (screen x).
    pub x0: f32,
    pub x1: f32,
    /// The window cuts the programme here: that side is square and flush with the edge.
    pub cut_left: bool,
    pub cut_right: bool,
}

impl CellGeom {
    pub fn w(&self) -> f32 {
        self.x1 - self.x0
    }
}

/// The cell of `[start, stop)` in the window starting at `window`, or `None` when it is not on
/// screen at all (or too thin to draw).
pub fn cell(start: Timestamp, stop: Timestamp, window: Timestamp) -> Option<CellGeom> {
    let end = window + window_span();
    if stop <= window || start >= end || stop <= start {
        return None;
    }
    let cut_left = start < window;
    let cut_right = stop > end;
    let half_gap = CELL_GAP_X * 0.5;
    let x0 = if cut_left {
        GRID_X
    } else {
        x_of(start, window) + half_gap
    };
    let x1 = if cut_right {
        SCREEN_W
    } else {
        x_of(stop, window) - half_gap
    };
    (x1 - x0 >= 1.0).then_some(CellGeom {
        x0,
        x1,
        cut_left,
        cut_right,
    })
}

/// The band of row `index` when the list is scrolled by `scroll` pixels.
pub fn row_rect(index: usize, scroll: f32) -> Rect {
    Rect::new(0.0, ROWS_Y + index as f32 * ROW_H - scroll, SCREEN_W, ROW_H)
}

/// The face of a cell in a row band.
pub fn cell_rect(row: Rect, c: &CellGeom) -> Rect {
    Rect::new(c.x0, row.y + CELL_GAP_Y * 0.5, c.w(), row.h - CELL_GAP_Y)
}

/// Where the rows are drawn (and clipped).
pub const fn rows_clip() -> Rect {
    Rect::new(0.0, GRID_Y, SCREEN_W, SCREEN_H - GRID_Y)
}

/// The preview rectangle on the 1920x1080 plane. The groups panel covers it rather than moving
/// it: moving the video window costs the TV a visible re-layout.
pub fn preview_rect() -> crate::data::Rect {
    crate::data::Rect {
        x: PREVIEW.x as i32,
        y: PREVIEW.y as i32,
        w: PREVIEW.w as i32,
        h: PREVIEW.h as i32,
    }
}

/// The now-line's pixel column in the window starting at `window`, or `None` while the window
/// does not show `now`.
pub fn now_column(now: Timestamp, window: Timestamp) -> Option<i32> {
    let x = x_of(now, window);
    (GRID_X..SCREEN_W).contains(&x).then(|| x.round() as i32)
}

/// When the visible now-line moves to its next pixel column (`None` while it is off screen).
pub fn now_line_step(now: Timestamp, window: Timestamp) -> Option<Timestamp> {
    let column = now_column(now, window)?;
    let secs =
        ((f64::from(column) + 0.5 - f64::from(GRID_X)) / f64::from(PX_PER_SEC)).ceil() as i64;
    Some((window + secs).max(now + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Timestamp = 1_792_418_400; // a half hour

    #[test]
    fn preview_is_a_third_of_the_width_and_sixteen_by_nine() {
        let r = preview_rect();
        assert_eq!(r.w * 3, 1920);
        assert_eq!(r.w * 9, r.h * 16);
        assert!(r.x + r.w < DETAILS_X as i32);
        assert!(r.y + r.h < RULER_Y as i32);
    }

    #[test]
    fn window_shows_a_bit_over_two_hours() {
        let span = window_span();
        assert!((2 * 3600..3 * 3600).contains(&span), "{span}");
        assert_eq!(x_of(W + HALF_HOUR, W) - x_of(W, W), HALF_HOUR_W);
    }

    #[test]
    fn cells_are_cut_square_by_the_window() {
        let inside = cell(W + 600, W + 1800, W).unwrap();
        assert!(!inside.cut_left && !inside.cut_right);
        assert!(inside.x0 > GRID_X && inside.x1 < SCREEN_W);
        assert!((inside.w() - (1200.0 * PX_PER_SEC - CELL_GAP_X)).abs() < 1e-3);

        let left = cell(W - 600, W + 600, W).unwrap();
        assert!(left.cut_left && !left.cut_right);
        assert_eq!(left.x0, GRID_X);

        let right = cell(W + window_span() - 60, W + window_span() + 600, W).unwrap();
        assert!(right.cut_right && !right.cut_left);
        assert_eq!(right.x1, SCREEN_W);

        let both = cell(W - 1, W + window_span() + 1, W).unwrap();
        assert!(both.cut_left && both.cut_right);

        assert_eq!(cell(W - 600, W, W), None);
        assert_eq!(cell(W + window_span(), W + window_span() + 60, W), None);
        assert_eq!(cell(W + 60, W + 60, W), None);
    }

    #[test]
    fn eight_rows_fit_under_the_ruler_the_last_one_cut() {
        let last = row_rect(VISIBLE_ROWS - 1, 0.0);
        assert!(last.y < SCREEN_H && last.y + last.h > SCREEN_H);
        assert!(row_rect(VISIBLE_ROWS - 2, 0.0).y + ROW_H <= SCREEN_H);
        assert!(row_rect(0, 0.0).y >= GRID_Y);
    }

    #[test]
    fn the_now_line_steps_a_pixel_at_a_time() {
        let column = now_column(W + 600, W).unwrap();
        let next = now_line_step(W + 600, W).unwrap();
        assert!(next > W + 600);
        assert_eq!(now_column(next, W), Some(column + 1));
        assert_eq!(now_column(next - 1, W), Some(column));
        assert_eq!(now_column(W - 1, W), None);
        assert_eq!(now_line_step(W + window_span() + 60, W), None);
    }
}
