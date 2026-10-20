//! What the guide draws, laid out at draw time and cached. Rows carry their fitted cell titles;
//! the details panel and the ruler are display lists. Text is measured again only when the state
//! behind it changed, so a frame walks the cache and allocates nothing.
//!
//! Only the rows the grid has read are laid out (the visible eight and two either side for the
//! scroll animation). A row whose channel and window are unchanged keeps its layout: a one-row
//! move lays out one row.

use std::collections::HashSet;
use std::rc::Rc;

use iptv::model::{ChannelKey, Timestamp};
use tv_machine::machine::Measure;
use tv_platform::i18n::msg;
use tv_ui::label::{HAlign, Label};
use tv_ui::{Painter, Rect, Spring};

use super::clock;
use super::geom::{self, CellGeom};
use super::grid::{Grid, Mode, Row, Shown};
use crate::data::Logos;
use crate::draw::{self, Piece, Run};
use crate::icons::{self, Icon};
use crate::point::{Spot, Spots};
use crate::theme::{self, radius, size};
use crate::time::{self, MINUTE};

/// Spring constant of the row scroll.
const SCROLL_K: f32 = tv_ui::consts::K_SCROLL;
/// A jump of more rows than this (a wrap, a page, a group change) snaps instead of scrolling.
const SCROLL_SNAP_ROWS: usize = 2;
/// The reminder mark at the end of a programme cell.
const BELL: f32 = 24.0;

pub struct CellSlot {
    /// `None` for the stand-in cell of a row with no guide data in the window.
    pub(super) start: Option<Timestamp>,
    pub(super) geom: CellGeom,
    text: Option<Run>,
    /// A reminder is set for the programme.
    bell: bool,
}

pub struct RowSlot {
    key: ChannelKey,
    window: Timestamp,
    number: Run,
    name: Run,
    /// In the row's own coordinates: y from the top of its band.
    logo: Piece,
    catchup: bool,
    pub(super) cells: Vec<CellSlot>,
}

/// The identity of what the details panel shows; a change lays it out again.
#[derive(Clone, PartialEq, Eq)]
struct DetailsKey {
    channel: Option<ChannelKey>,
    programme: Option<(Timestamp, Timestamp)>,
    minute: i64,
    favorite: bool,
}

#[derive(Default)]
pub struct Paint {
    /// The layout of [`Grid::rows`], in the same order.
    pub(super) rows: Vec<RowSlot>,
    pub(super) scroll: Spring,
    scroll_target: f32,
    /// The state changed since the last layout.
    stale: bool,
    pub(super) details: Vec<Piece>,
    details_key: Option<DetailsKey>,
    pub(super) ruler: Vec<Piece>,
    /// The window, the minute and the UTC offset the ruler shows.
    ruler_key: Option<(Timestamp, i64, i32)>,
    /// Picture in picture: no preview, the details from the left margin, the video in the
    /// [`geom::PIP`] window.
    pub(super) pip: bool,
}

impl Paint {
    /// Picture in picture on or off: the details are laid out again for it.
    pub fn set_pip(&mut self, on: bool) {
        if self.pip != on {
            self.pip = on;
            self.details_key = None;
            self.stale = true;
        }
    }

    /// The state changed: scroll towards row `top` and lay out again before the next frame.
    pub fn follow(&mut self, top: usize) {
        let target = top as f32 * geom::ROW_H;
        if (target - self.scroll_target).abs() > SCROLL_SNAP_ROWS as f32 * geom::ROW_H {
            self.scroll.jump(target);
        }
        self.scroll_target = target;
        self.stale = true;
    }

    /// The data behind the layout changed (a refresh): lay everything out again.
    pub fn forget(&mut self) {
        self.rows.clear();
        self.details_key = None;
        self.ruler_key = None;
        self.stale = true;
    }

    /// Bring every layout in line with the state. Nothing to do when the state is unchanged.
    pub fn prepare(&mut self, m: &dyn Measure, grid: &Grid) {
        if !self.stale {
            return;
        }
        let mut old = std::mem::take(&mut self.rows);
        self.rows = grid
            .rows
            .iter()
            .map(|row| {
                let kept = old
                    .iter()
                    .position(|s| s.key == row.channel.key && s.window == row.window);
                match kept {
                    Some(i) => old.swap_remove(i),
                    None => row_slot(m, row, &grid.reminders),
                }
            })
            .collect();

        let key = DetailsKey {
            channel: grid.channel().map(|c| c.key),
            programme: grid.focus.as_ref().map(|p| (p.start, p.stop)),
            minute: grid.now.div_euclid(MINUTE),
            favorite: grid.row().is_some_and(|r| r.favorite),
        };
        if self.details_key.as_ref() != Some(&key) {
            self.details.clear();
            details(m, grid, key.favorite, self.pip, &mut self.details);
            self.details_key = Some(key);
        }

        let key = (grid.window, grid.now.div_euclid(MINUTE), grid.offset);
        if self.ruler_key != Some(key) {
            self.ruler.clear();
            ruler(grid, &mut self.ruler);
            self.ruler_key = Some(key);
        }
        self.stale = false;
    }

    pub fn update(&mut self, dt: f32) {
        self.scroll.step(self.scroll_target, SCROLL_K, dt);
        if !self.animating() {
            // Rest exactly on the row, not a fraction of a pixel short of it.
            self.scroll.jump(self.scroll_target);
        }
    }

    /// The row scroll has not come to rest.
    pub fn animating(&self) -> bool {
        !tv_machine::idle::settled(self.scroll.pos, self.scroll_target, self.scroll.vel)
    }

    /// The guide; `panel` while the groups panel covers its left (the screen draws the panel).
    pub fn draw(&self, p: Painter, grid: &Grid, panel: bool, logos: &dyn Logos) {
        // The panel hides the preview with the rest of the left side; the video plays on.
        // Picture in picture has no preview: its window is cut over the rows at the end.
        ground(p, !panel && !self.pip);
        draw::paint(&self.details, p, logos);
        draw::paint(&self.ruler, p, logos);
        let clip = geom::rows_clip();
        if grid.channels.is_empty() {
            Label::new(
                msg::channel_empty_c().as_ptr(),
                size::LABEL,
                theme::TEXT_MUTED,
            )
            .draw(p, Rect::new(geom::GRID_X, geom::ROWS_Y, 0.0, geom::ROW_H));
        }
        let shown = grid.shown.as_ref().map(Shown::channel);
        p.clip(clip);
        for (row, slot) in grid.rows.iter().zip(&self.rows) {
            let band = geom::row_rect(row.index, self.scroll.pos);
            if band.y >= clip.y + clip.h || band.y + band.h <= clip.y {
                continue;
            }
            draw_row(p, grid, row, slot, band, shown == Some(slot.key), logos);
        }
        p.clip_clear();
        draw_now_line(p, grid);
        if self.pip {
            pip_window(p);
        }
    }
}

/// A [`Spot`] on a row's band: the channel column, a gap between cells, the stand-in cell of a
/// row with no guide.
pub const ROW: u8 = 0;
/// A [`Spot`] on a programme cell; its `item` is the programme's start.
pub const CELL: u8 = 1;

impl Paint {
    /// What the pointer can focus in the frame drawn last: each whole row on screen and the
    /// programme cells on it; the row the bottom edge cuts is dead. The picture-in-picture
    /// window covers the cells under it. With the groups panel up (`panel`, its column) every
    /// row beside it is dead and the panel covers the rest.
    pub fn spots(&self, grid: &Grid, panel: Option<Rect>, out: &mut Spots) {
        out.clear();
        let clip = geom::rows_clip();
        let whole = grid.cursor.visible(grid.channels.len(), geom::ROWS);
        for (row, slot) in grid.rows.iter().zip(&self.rows) {
            let band = geom::row_rect(row.index, self.scroll.pos);
            if band.y >= clip.y + clip.h || band.y + band.h <= clip.y {
                continue;
            }
            if panel.is_some() || !whole.contains(&row.index) {
                out.dead(band.intersect(clip));
                continue;
            }
            out.add(band.intersect(clip), Spot::row(ROW, row.index));
            for cell in &slot.cells {
                let Some(start) = cell.start else { continue };
                let spot = Spot {
                    area: CELL,
                    index: row.index,
                    item: start,
                };
                out.add(geom::cell_rect(band, &cell.geom).intersect(clip), spot);
            }
        }
        if self.pip {
            let (v, f) = (geom::PIP, geom::PIP_FRAME);
            out.cover(Rect::new(v.x - f, v.y - f, v.w + 2.0 * f, v.h + 2.0 * f));
        }
        if let Some(column) = panel {
            out.cover(column);
        }
    }
}

/// The picture-in-picture window over the rows: a frame, and inside it a hole through to the
/// video plane: the transparent clear every frame starts with, scissored to the window.
fn pip_window(p: Painter) {
    let (v, f) = (geom::PIP, geom::PIP_FRAME);
    let frame = Rect::new(v.x - f, v.y - f, v.w + 2.0 * f, v.h + 2.0 * f);
    p.rrect(frame, radius::PILL, radius::PILL, theme::PANEL_RAISED);
    if p.is_recording() {
        return;
    }
    p.clip(v);
    tv_gfx::gfx::frame_clear_through();
    p.clip_clear();
}

fn row_slot(m: &dyn Measure, row: &Row, reminders: &HashSet<(ChannelKey, Timestamp)>) -> RowSlot {
    let channel = &row.channel;
    let mut cells: Vec<CellSlot> = row
        .programmes
        .iter()
        .filter_map(|p| {
            let geom = geom::cell(p.start, p.stop, row.window)?;
            let bell = reminders.contains(&(channel.key, p.start));
            Some(CellSlot {
                start: Some(p.start),
                text: cell_text(m, &p.title, &geom, bell),
                geom,
                bell,
            })
        })
        .collect();
    if cells.is_empty() {
        let geom = CellGeom {
            x0: geom::GRID_X,
            x1: geom::SCREEN_W,
            cut_left: true,
            cut_right: true,
        };
        cells.push(CellSlot {
            start: None,
            text: cell_text(m, &channel.name, &geom, false),
            geom,
            bell: false,
        });
    }
    let number = channel.number.map(|n| n.to_string()).unwrap_or_default();
    RowSlot {
        key: channel.key,
        window: row.window,
        number: draw::run(&number),
        name: m.fit_line(&channel.name, geom::NAME.1, size::LABEL, true),
        logo: Piece::logo(
            Some(Rc::clone(channel)),
            geom::LOGO_TILE,
            size::LABEL,
            theme::PANEL,
        ),
        catchup: channel.catchup.available(),
        cells,
    }
}

/// The details panel: the focused programme's title, times, progress and description, or the
/// channel's name on a row without guide data. Without the preview (`pip`) it starts at the left
/// margin and shows more of the description.
fn details(m: &dyn Measure, grid: &Grid, favorite: bool, pip: bool, out: &mut Vec<Piece>) {
    let Some(channel) = grid.channel() else {
        return;
    };
    let text =
        |s: Run, r: Rect, h: draw::HAlign, sz, color| Piece::text_in(s, r, h, sz, false, color);
    let left = draw::HAlign::Left;
    let (x0, desc_lines) = if pip {
        (geom::PIP_DETAILS_X, geom::PIP_DESC_LINES)
    } else {
        (geom::DETAILS_X, geom::DESC_LINES)
    };
    let title_w = geom::STAR.x - x0 - theme::space::L;
    let title = match grid.details() {
        Some(p) => &p.title,
        None => &channel.name,
    };
    let title = m.fit_line(title, title_w, size::TITLE, false);
    let title_band = Rect::new(x0, geom::TITLE.y, geom::TITLE.w, geom::TITLE.h);
    out.push(text(title, title_band, left, size::TITLE, theme::TEXT));
    let star = if favorite {
        Icon::StarFilled
    } else {
        Icon::Star
    };
    out.push(Piece::Icon {
        icon: star,
        r: geom::STAR,
        tint: theme::TEXT,
    });

    let band = |x: f32, w: f32| Rect::new(x, geom::META_Y, w, geom::META_H);
    if let Some(p) = grid.details() {
        let mut x = x0;
        let times = time::span(p.start, p.stop, grid.offset);
        let times_w = m.width_str(&times, size::LABEL, false);
        let r = band(x, times_w);
        out.push(text(
            draw::run(&times),
            r,
            left,
            size::LABEL,
            theme::TEXT_MUTED,
        ));
        x += times_w + theme::space::L;
        let bar = Rect::new(
            x,
            geom::META_Y + (geom::META_H - geom::PROGRESS_H) * 0.5,
            geom::PROGRESS_W,
            geom::PROGRESS_H,
        );
        let done = time::progress(p, grid.now);
        if done > 0.0 {
            out.extend(draw::bar(bar, done, theme::PROGRESS, theme::PROGRESS_TRACK));
        } else {
            out.push(Piece::solid(bar, bar.h * 0.5, theme::PROGRESS_TRACK));
        }
        x += geom::PROGRESS_W + theme::space::L;
        // On air: what is left; otherwise how long it runs.
        let minutes = if p.is_on_air(grid.now) {
            time::minutes_left(p, grid.now)
        } else {
            (p.duration_secs() + MINUTE - 1) / MINUTE
        };
        let minutes = draw::run(&time::minutes_label(minutes));
        out.push(text(
            minutes,
            band(x, 0.0),
            left,
            size::LABEL,
            theme::TEXT_MUTED,
        ));
    }
    if let Some(g) = channel.groups.first() {
        let w = geom::DETAILS_RIGHT - x0;
        let name = m.fit_line(g, w * 0.4, size::LABEL, false);
        let r = band(x0, w);
        out.push(text(name, r, draw::HAlign::Right, size::LABEL, theme::TEXT));
    }
    if let Some(description) = grid.details().and_then(|p| p.description.as_deref()) {
        let w = geom::DETAILS_RIGHT - x0;
        let lines = draw::wrap(m, description, w, size::LABEL, false, desc_lines);
        for (i, line) in lines.into_iter().enumerate() {
            let y = geom::DESC_Y + i as f32 * geom::DESC_LINE_H;
            let r = Rect::new(x0, y, 0.0, geom::DESC_LINE_H);
            out.push(text(line, r, left, size::LABEL, theme::TEXT_MUTED));
        }
    }
}

/// The ruler: the window's day (the clock while the window is on today), its line and the
/// half-hour marks.
fn ruler(grid: &Grid, out: &mut Vec<Piece>) {
    let today = time::local_day(grid.window, grid.offset) == time::local_day(grid.now, grid.offset);
    let date = if today {
        time::clock(grid.now, grid.offset)
    } else {
        time::day(grid.window, grid.offset)
    };
    let band = Rect::new(geom::RULER_DATE_X, geom::RULER_Y, 0.0, geom::RULER_H);
    let left = draw::HAlign::Left;
    out.push(Piece::text_in(
        draw::run(&date),
        band,
        left,
        size::LABEL,
        false,
        theme::ACCENT_TEXT,
    ));
    let line_y = geom::GRID_Y;
    let line = Rect::new(
        geom::GRID_X,
        line_y - 1.0,
        geom::SCREEN_W - geom::GRID_X,
        1.0,
    );
    out.push(Piece::solid(line, 0.0, theme::DIVIDER));
    let mut t = clock::ceil_half_hour(grid.window, grid.offset);
    while t <= grid.window_end() {
        let x = geom::x_of(t, grid.window);
        if x > geom::SCREEN_W {
            break;
        }
        let tick = Rect::new(x - 0.5, line_y - geom::TICK_H, 1.0, geom::TICK_H);
        out.push(Piece::solid(tick, 0.0, theme::DIVIDER));
        out.push(Piece::text_in(
            draw::run(&time::hhmm(t, grid.offset)),
            Rect::new(x, geom::RULER_Y, 0.0, geom::RULER_H - geom::TICK_H),
            draw::HAlign::Center,
            size::LABEL,
            false,
            theme::TEXT_MUTED,
        ));
        t += clock::HALF_HOUR;
    }
}

fn draw_row(
    p: Painter,
    grid: &Grid,
    row: &Row,
    slot: &RowSlot,
    band: Rect,
    playing: bool,
    logos: &dyn Logos,
) {
    let focused_row = row.index == grid.cursor.index;
    let managing = grid.mode == Mode::ManageFavorites;
    let face = Rect::new(
        0.0,
        band.y + geom::CELL_GAP_Y * 0.5,
        0.0,
        band.h - geom::CELL_GAP_Y,
    );

    // Channel column.
    let head_focus = managing && focused_row;
    if head_focus {
        let pill = Rect::new(
            theme::space::S,
            face.y,
            geom::GRID_X - 2.0 * theme::space::S,
            face.h,
        );
        p.rrect(pill, radius::PILL, radius::PILL, theme::FOCUS);
    }
    let ink = if head_focus {
        theme::TEXT_ON_FOCUS
    } else if playing {
        theme::ink(theme::ACCENT_TEXT)
    } else {
        theme::TEXT
    };
    let col = |(x, w): (f32, f32)| Rect::new(x, face.y, w, face.h);
    Label::new(slot.number.as_ptr(), size::LABEL, ink)
        .h(HAlign::Center)
        .draw(p, col(geom::NUMBER));
    draw::paint(
        std::slice::from_ref(&slot.logo),
        p.translate(0.0, band.y),
        logos,
    );
    Label::new(slot.name.as_ptr(), size::LABEL, ink)
        .bold()
        .draw(p, col(geom::NAME));
    let mark = icon_box(col(geom::MARK));
    if managing {
        let star = if row.favorite {
            Icon::StarFilled
        } else {
            Icon::Star
        };
        icons::draw(p, star, mark, ink);
    } else if playing {
        icons::draw(p, Icon::Play, mark, theme::ACCENT);
    }
    if slot.catchup {
        let muted = if head_focus {
            theme::TEXT_ON_FOCUS_MUTED
        } else {
            theme::TEXT_MUTED
        };
        icons::draw(p, Icon::CatchUp, icon_box(col(geom::CATCHUP)), muted);
    }

    // Programme cells.
    for cell in &slot.cells {
        let r = geom::cell_rect(band, &cell.geom);
        let focused = !managing
            && focused_row
            && match (&grid.focus, cell.start) {
                (Some(f), Some(s)) => f.start == s,
                (None, None) => true,
                _ => false,
            };
        let (fill, text) = if focused {
            (theme::FOCUS, theme::TEXT_ON_FOCUS)
        } else if focused_row {
            (theme::CELL_ROW, theme::TEXT)
        } else {
            (theme::CELL, theme::TEXT_MUTED)
        };
        let rl = if cell.geom.cut_left {
            0.0
        } else {
            radius::CELL
        };
        let rr = if cell.geom.cut_right {
            0.0
        } else {
            radius::CELL
        };
        p.rrect(r, rl, rr, fill);
        if let Some(t) = &cell.text {
            let inner = Rect::new(
                r.x + geom::CELL_PAD_X,
                r.y,
                r.w - 2.0 * geom::CELL_PAD_X,
                r.h,
            );
            Label::new(t.as_ptr(), size::LABEL, text).draw(p, inner);
        }
        if cell.bell && r.w >= BELL + 2.0 * geom::CELL_PAD_X {
            let x = r.x + r.w - geom::CELL_PAD_X - BELL;
            let mark = Rect::new(x, r.y + (r.h - BELL) * 0.5, BELL, BELL);
            icons::draw(p, Icon::Bell, mark, text);
        }
    }
}

fn draw_now_line(p: Painter, grid: &Grid) {
    let x = geom::x_of(grid.now, grid.window);
    if x < geom::GRID_X || x >= geom::SCREEN_W {
        return;
    }
    let w = geom::NOW_LINE_W;
    let top = geom::GRID_Y;
    p.rrect(
        Rect::new(x - w * 0.5, top, w, geom::SCREEN_H - top),
        0.0,
        0.0,
        theme::NOW_LINE,
    );
    let r = geom::NOW_DOT_R;
    p.rrect(
        Rect::new(x - r, top - r, 2.0 * r, 2.0 * r),
        r,
        r,
        theme::ink(theme::ACCENT_TEXT),
    );
}

/// The app ground, with the preview left as a hole for the video plane when `hole`.
fn ground(p: Painter, hole: bool) {
    let (w, h) = (geom::SCREEN_W, geom::SCREEN_H);
    if !hole {
        p.rrect(Rect::new(0.0, 0.0, w, h), 0.0, 0.0, theme::BG);
        return;
    }
    let v = geom::PREVIEW;
    for r in [
        Rect::new(0.0, 0.0, w, v.y),
        Rect::new(0.0, v.y + v.h, w, h - v.y - v.h),
        Rect::new(0.0, v.y, v.x, v.h),
        Rect::new(v.x + v.w, v.y, w - v.x - v.w, v.h),
    ] {
        if r.w > 0.0 && r.h > 0.0 {
            p.rrect(r, 0.0, 0.0, theme::BG);
        }
    }
}

/// A square icon centred in a column cell.
fn icon_box(column: Rect) -> Rect {
    let s = column.w;
    Rect::new(column.x, column.y + (column.h - s) * 0.5, s, s)
}

/// A cell's title fitted to its visible width (less the bell's, when it has one), or nothing
/// when the cell is too thin.
fn cell_text(m: &dyn Measure, title: &str, geom: &CellGeom, bell: bool) -> Option<Run> {
    let bell_w = if bell { BELL + geom::CELL_PAD_X } else { 0.0 };
    let w = geom.w() - 2.0 * geom::CELL_PAD_X - bell_w;
    (geom.w() >= geom::CELL_TEXT_MIN_W).then(|| m.fit_line(title, w, size::LABEL, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fake::{FakeData, Mono};
    use crate::data::TvData;
    use crate::reminders::Reminder;

    #[test]
    fn a_reminded_programme_carries_the_bell() {
        let mut data = FakeData::new(1_700_000_000, 3);
        let key = data.channels[0].key;
        let next = data
            .now_next(key, data.now)
            .1
            .expect("a programme after this one");
        data.reminders = vec![Reminder {
            channel: key,
            start: next.start,
            title: next.title.clone(),
        }];
        let mut grid = Grid::new(data.channels(&crate::data::Group::All));
        grid.open(&data);
        let mut paint = Paint::default();
        paint.follow(grid.cursor.top);
        paint.prepare(&Mono, &grid);
        let bells: Vec<(ChannelKey, Option<Timestamp>)> = paint
            .rows
            .iter()
            .flat_map(|r| r.cells.iter().filter(|c| c.bell).map(|c| (r.key, c.start)))
            .collect();
        assert_eq!(bells, [(key, Some(next.start))]);
        paint.draw(Painter::recording(), &grid, false, &NoLogos);
    }

    struct NoLogos;

    impl Logos for NoLogos {
        fn logo(&self, _: &iptv::model::Channel) -> Option<(u32, f32, f32)> {
            None
        }
    }
}
