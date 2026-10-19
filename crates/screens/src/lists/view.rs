//! Drawing the lists. Each lays itself out into a display list ([`Cached`]) when its generation
//! moves, measuring and fitting text then, never while painting. Only the rows on screen are laid
//! out, and a channel row laid out before is moved rather than measured again while it shows the
//! same thing.
//!
//! Geometry was measured from TiviMate at 1080p; colours, type and spacing come from
//! [`crate::theme`]. Where each column of the channel list stands depends on the focus
//! ([`layout`]): the groups panel pushes the channels right, and in preview mode the days column
//! takes the channels' place so the programme list and the days stay left of the video.

use std::os::raw::c_int;
use std::rc::Rc;

use iptv::model::{Channel, Programme, Timestamp};
use tv_gfx::gfx::tokens::with_a;
use tv_machine::machine::Measure;
use tv_platform::i18n::msg;
use tv_ui::{Painter, Rect};

use super::channels::{rows, ChannelList, Column, Mode, PREVIEW_RECT};
use super::history::{self, History, Line};
use crate::data::Logos;
use crate::draw::{self, channel_number, Cached, HAlign, Piece};
use crate::groups::Frame;
use crate::icons::Icon;
use crate::theme::{self, radius, size, space};
use crate::time::{self, When, DAY};

/// Geometry at 1080p.
mod geom {
    /// The channel column.
    pub const LIST_W: f32 = 600.0;
    pub const HEADER_H: f32 = 72.0;
    pub const ROW_H: f32 = 125.0;
    pub const LOGO_X: f32 = 28.0;
    pub const LOGO_W: f32 = 100.0;
    pub const LOGO_H: f32 = 64.0;
    pub const TEXT_X: f32 = 148.0;
    pub const TEXT_R: f32 = 572.0;
    pub const ICON: f32 = 28.0;
    pub const BAR_H: f32 = 4.0;
    /// The programme column, right of the channels.
    pub const PROG_X: f32 = 600.0;
    pub const PROG_W: f32 = 525.0;
    pub const PROG_Y: f32 = 135.0;
    pub const PROG_H: f32 = 75.0;
    pub const TIME_W: f32 = 100.0;
    /// The days column, right of the programmes.
    pub const DAYS_X: f32 = 1125.0;
    pub const DAYS_W: f32 = 270.0;
    /// In preview mode the days column ends where the programme column does, left of the video.
    pub const PREVIEW_DAYS_X: f32 = PROG_X + PROG_W - DAYS_W;
    /// Details: the card over full-screen video, and the panel under the preview.
    pub const CARD_X: f32 = 1152.0;
    pub const CARD_X_DAYS: f32 = 1410.0;
    pub const CARD_R: f32 = 1872.0;
    pub const CARD_Y: f32 = 48.0;
    pub const CARD_H: f32 = 264.0;
    pub const PREVIEW_DETAIL_Y: f32 = 525.0;
    pub const PREVIEW_DETAIL_H: f32 = 297.0;
    pub const PAD: f32 = 24.0;
    pub const DESC_LINE: f32 = 38.0;
    /// History.
    pub const H_X: f32 = 48.0;
    pub const H_W: f32 = 1095.0;
    pub const H_TOP: f32 = 40.0;
    pub const H_ROW: f32 = 120.0;
    pub const H_TEXT_X: f32 = 72.0;
    pub const H_TEXT_R: f32 = 680.0;
    pub const H_LOGO_X: f32 = 728.0;
    pub const H_NAME_X: f32 = 848.0;
    pub const H_TRASH_X: f32 = 1060.0;
    pub const H_TRASH_W: f32 = 64.0;
    pub const H_SIDE_X: f32 = 1192.0;
    pub const H_SIDE_W: f32 = 680.0;
    pub const H_CARD_Y: f32 = 160.0;
    pub const H_CARD_H: f32 = 300.0;
    pub const H_BAR_W: f32 = 80.0;
}

/// The groups panel beside the channel list: as narrow as its pills allow, so the channels right
/// of it still end left of the preview video.
pub(super) const GROUPS: Frame = Frame {
    w: 488.0,
    pill_x: 24.0,
};

/// Where the columns of the channel list stand, as `(x, width)` on the 1920x1080 plane; `None`
/// is a column not drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Layout {
    pub groups: Option<(f32, f32)>,
    pub channels: Option<(f32, f32)>,
    pub programmes: Option<(f32, f32)>,
    pub days: Option<(f32, f32)>,
}

/// The columns for `column` focused in `mode`.
pub(crate) fn layout(mode: Mode, column: Column) -> Layout {
    let programmes = Some((geom::PROG_X, geom::PROG_W));
    let list = |x| Some((x, geom::LIST_W));
    match (mode, column) {
        // The programme list gives way to the groups panel, which pushes the channels right.
        (_, Column::Groups) => Layout {
            groups: Some((0.0, GROUPS.w)),
            channels: list(GROUPS.w),
            programmes: None,
            days: None,
        },
        // No room for three columns left of the preview: the channels give way.
        (Mode::Preview, Column::Days) => Layout {
            groups: None,
            channels: None,
            programmes: Some((0.0, geom::PREVIEW_DAYS_X)),
            days: Some((geom::PREVIEW_DAYS_X, geom::DAYS_W)),
        },
        (Mode::Overlay, Column::Days) => Layout {
            groups: None,
            channels: list(0.0),
            programmes,
            days: Some((geom::DAYS_X, geom::DAYS_W)),
        },
        (_, Column::Channels | Column::Programmes) => Layout {
            groups: None,
            channels: list(0.0),
            programmes,
            days: None,
        },
    }
}

/// A display list being built, and the measure its text is fitted with.
struct Out<'a> {
    m: &'a dyn Measure,
    pieces: &'a mut Vec<Piece>,
}

impl Out<'_> {
    fn push(&mut self, piece: Piece) {
        self.pieces.push(piece);
    }

    /// `s` fitted into `r`, on its middle against its left or right edge; the width it takes.
    fn text(&mut self, s: &str, r: Rect, h: HAlign, sz: c_int, bold: bool, color: [f32; 4]) -> f32 {
        if s.is_empty() || r.w <= 0.0 {
            return 0.0;
        }
        let (s, w) = draw::fit(self.m, s, r.w, sz, bold);
        self.push(Piece::text_in(s, r, h, sz, bold, color));
        w
    }

    /// `s` in at most `max` lines, the first in `first` and each next one its height lower; the
    /// height used.
    fn wrap(
        &mut self,
        s: &str,
        first: Rect,
        sz: c_int,
        bold: bool,
        color: [f32; 4],
        max: usize,
    ) -> f32 {
        let lines = draw::wrap(self.m, s, first.w, sz, bold, max);
        let used = lines.len() as f32 * first.h;
        for (n, line) in lines.into_iter().enumerate() {
            let r = Rect::new(first.x, first.y + n as f32 * first.h, first.w, first.h);
            self.push(Piece::text_in(line, r, HAlign::Left, sz, bold, color));
        }
        used
    }

    /// The details of `p`: title, times with progress or length, description.
    fn details(&mut self, card: Rect, p: &Programme, now: Timestamp, off: i32) {
        self.push(Piece::solid(card, radius::PILL, with_a(theme::PANEL, 0.94)));
        let x = card.x + geom::PAD;
        let w = card.w - 2.0 * geom::PAD;
        let mut y = card.y + geom::PAD - 4.0;
        let title = Rect::new(x, y, w, 52.0);
        self.text(
            &p.title,
            title,
            HAlign::Left,
            size::HEADING,
            true,
            theme::TEXT,
        );
        y += 60.0;
        let line = Rect::new(x, y, w, 40.0);
        let span = time::span(p.start, p.stop, off);
        let muted = theme::TEXT_MUTED;
        let mut cx = x + self.text(&span, line, HAlign::Left, size::LABEL, false, muted) + space::M;
        let mins = match time::when(p.start, p.stop, now) {
            When::OnAir => {
                let bar = Rect::new(cx, y + 18.0, geom::H_BAR_W, geom::BAR_H);
                let frac = time::progress(p, now);
                self.pieces
                    .extend(draw::bar(bar, frac, theme::PROGRESS, theme::PROGRESS_TRACK));
                cx += geom::H_BAR_W + space::M;
                time::minutes_left(p, now)
            }
            When::Past | When::Future => (p.duration_secs() + 59) / 60,
        };
        let r = Rect::new(cx, y, (x + w - cx).max(0.0), 40.0);
        self.text(
            &time::minutes_label(mins),
            r,
            HAlign::Left,
            size::LABEL,
            false,
            muted,
        );
        y += 52.0;
        if let Some(d) = &p.description {
            let lines = ((card.y + card.h - geom::PAD - y) / geom::DESC_LINE).floor() as usize;
            let first = Rect::new(x, y, w, geom::DESC_LINE);
            self.wrap(d, first, size::LABEL, false, muted, lines.min(3));
        }
    }
}

/// Inks of a row: (primary, secondary), on the focus pill or on the panel.
fn inks(focused: bool) -> ([f32; 4], [f32; 4]) {
    if focused {
        (theme::TEXT_ON_FOCUS, theme::TEXT_ON_FOCUS_MUTED)
    } else {
        (theme::TEXT, theme::TEXT_MUTED)
    }
}

/// The first row shown when `index` should sit in the middle of `rows` rows of `len`.
fn centred_top(index: usize, len: usize, rows: usize) -> usize {
    index.saturating_sub(rows / 2).min(len.saturating_sub(rows))
}

/// `p` moved right by `dx` and down by `dy`.
fn shifted(p: &Piece, dx: f32, dy: f32) -> Piece {
    let mut p = p.clone();
    match &mut p {
        Piece::Fill { r, .. } | Piece::Icon { r, .. } | Piece::Logo { r, .. } => {
            r.x += dx;
            r.y += dy;
        }
        Piece::Text { x, y, .. } => {
            *x += dx;
            *y += dy;
        }
    }
    p
}

/// Where the cursor row of the channel column stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Plain,
    /// The cursor row while another column has the focus.
    Parked,
    Focused,
}

/// Everything a channel row's text and marks depend on.
struct RowKey {
    channel: Rc<Channel>,
    number: u32,
    focus: Focus,
    playing: bool,
    on_air: Option<Rc<Programme>>,
}

impl PartialEq for RowKey {
    fn eq(&self, other: &Self) -> bool {
        let same_programme = match (&self.on_air, &other.on_air) {
            (Some(a), Some(b)) => Rc::ptr_eq(a, b),
            (a, b) => a.is_none() && b.is_none(),
        };
        Rc::ptr_eq(&self.channel, &other.channel)
            && (self.number, self.focus, self.playing) == (other.number, other.focus, other.playing)
            && same_programme
    }
}

/// The channel rows of the last layout, at y = 0 (their progress bars apart, which move with
/// the clock).
#[derive(Default)]
struct RowCache(Vec<(RowKey, Vec<Piece>)>);

impl RowCache {
    fn take(&mut self, key: &RowKey) -> Option<Vec<Piece>> {
        let i = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.swap_remove(i).1)
    }
}

/// The channel list's display list.
#[derive(Default)]
pub(crate) struct ChannelListView {
    cached: Cached,
    rows: RowCache,
}

impl ChannelListView {
    #[cfg(test)]
    pub fn pieces(&self) -> &[Piece] {
        self.cached.pieces()
    }

    /// Lay the list out again when it changed.
    pub fn prepare(&mut self, l: &ChannelList, m: &dyn Measure) {
        let rows = &mut self.rows;
        self.cached.prepare(l.generation(), |pieces| {
            let mut out = Out { m, pieces };
            let at = layout(l.mode(), l.column());
            if let Some((x, _)) = at.channels {
                channel_column(&mut out, l, rows, x);
            }
            if let Some(column) = at.programmes {
                programme_column(&mut out, l, column);
            }
            if let Some((x, _)) = at.days {
                days_column(&mut out, l, x);
            }
            details(&mut out, l);
        });
    }

    /// Paint what [`prepare`](Self::prepare) laid out.
    pub fn draw(&self, p: Painter, logos: &dyn Logos) {
        self.cached.paint(p, logos);
    }
}

/// The channel column from `x0`; its rows are laid out at x = 0 and moved there.
fn channel_column(out: &mut Out, l: &ChannelList, cache: &mut RowCache, x0: f32) {
    let panel = with_a(theme::PANEL, 0.96);
    out.push(Piece::solid(
        Rect::new(x0, 0.0, geom::LIST_W, 1080.0),
        0.0,
        panel,
    ));
    let header = Rect::new(x0, 0.0, geom::LIST_W, geom::HEADER_H);
    out.push(Piece::solid(header, 0.0, theme::PANEL_RAISED));
    let name = Rect::new(
        x0 + space::XL,
        0.0,
        geom::LIST_W - 2.0 * space::XL,
        geom::HEADER_H,
    );
    out.text(
        l.group().name(),
        name,
        HAlign::Left,
        size::BODY,
        false,
        theme::TEXT,
    );
    let cursor = l.cursor();
    let catchup = l.showing_catchup().map(|(c, _)| c);
    let mut laid = Vec::new();
    for i in cursor.visible(l.channels().len(), rows::CHANNELS) {
        let c = &l.channels()[i];
        let focus = match (i == cursor.index, l.column() == Column::Channels) {
            (false, _) => Focus::Plain,
            (true, false) => Focus::Parked,
            (true, true) => Focus::Focused,
        };
        let key = RowKey {
            channel: c.clone(),
            number: channel_number(c, i),
            focus,
            playing: l.is_showing_live(c.key) || catchup == Some(c.key),
            on_air: l.row(i).and_then(|r| r.on_air.clone()),
        };
        let pieces = cache.take(&key).unwrap_or_else(|| channel_row(out.m, &key));
        let y = geom::HEADER_H + (i - cursor.top) as f32 * geom::ROW_H;
        out.pieces.extend(pieces.iter().map(|p| shifted(p, x0, y)));
        if let Some(p) = &key.on_air {
            let bar = Rect::new(
                x0 + geom::TEXT_X,
                y + 104.0,
                geom::TEXT_R - geom::TEXT_X,
                geom::BAR_H,
            );
            let (fill, track) = if focus == Focus::Focused {
                (theme::TEXT_ON_FOCUS, with_a(theme::TEXT_ON_FOCUS, 0.2))
            } else {
                (theme::PROGRESS, theme::PROGRESS_TRACK)
            };
            out.pieces
                .extend(draw::bar(bar, time::progress(p, l.now()), fill, track));
        }
        laid.push((key, pieces));
    }
    cache.0 = laid;
}

/// One channel row at y = 0, without its progress bar.
fn channel_row(m: &dyn Measure, k: &RowKey) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut out = Out {
        m,
        pieces: &mut pieces,
    };
    let c = &*k.channel;
    let pill = Rect::new(12.0, 4.0, geom::LIST_W - 24.0, geom::ROW_H - 8.0);
    match k.focus {
        Focus::Focused => out.push(Piece::solid(pill, radius::PILL, theme::FOCUS)),
        Focus::Parked => out.push(Piece::solid(pill, radius::PILL, theme::PANEL_RAISED)),
        Focus::Plain => {}
    }
    let focused = k.focus == Focus::Focused;
    let (ink, muted) = inks(focused);
    let logo = Rect::new(
        geom::LOGO_X,
        (geom::ROW_H - geom::LOGO_H) * 0.5,
        geom::LOGO_W,
        geom::LOGO_H,
    );
    out.push(Piece::logo(
        Some(k.channel.clone()),
        logo,
        size::LABEL,
        theme::CELL_ROW,
    ));
    // Icons on the right of the name line: play mark, then the catch-up clock.
    let mut right = geom::TEXT_R;
    if c.catchup.available() {
        right -= geom::ICON;
        let r = Rect::new(right, 24.0, geom::ICON, geom::ICON);
        out.push(Piece::Icon {
            icon: Icon::CatchUp,
            r,
            tint: muted,
        });
        right -= space::S;
    }
    if k.playing {
        right -= geom::ICON;
        let tint = if focused { ink } else { theme::ACCENT };
        let r = Rect::new(right, 24.0, geom::ICON, geom::ICON);
        out.push(Piece::Icon {
            icon: Icon::Play,
            r,
            tint,
        });
        right -= space::S;
    }
    let name = format!("{} {}", k.number, c.name);
    let name_ink = if k.playing && !focused {
        theme::ACCENT_TEXT
    } else {
        ink
    };
    let r = Rect::new(geom::TEXT_X, 18.0, right - geom::TEXT_X, 40.0);
    out.text(&name, r, HAlign::Left, size::BODY, true, name_ink);
    let line = Rect::new(geom::TEXT_X, 60.0, geom::TEXT_R - geom::TEXT_X, 38.0);
    match &k.on_air {
        Some(p) => {
            let title_ink = if focused { ink } else { theme::ACCENT_TEXT };
            out.text(&p.title, line, HAlign::Left, size::LABEL, false, title_ink);
        }
        None => {
            let none = msg::channel_no_info();
            out.text(none, line, HAlign::Left, size::LABEL, false, muted);
        }
    }
    pieces
}

/// The programme column at `(x0, w)`.
fn programme_column(out: &mut Out, l: &ChannelList, (x0, w): (f32, f32)) {
    let Some(c) = l.focused_channel() else {
        return;
    };
    let column = Rect::new(x0, 0.0, w, 1080.0);
    out.push(Piece::solid(column, 0.0, with_a(theme::BG, 0.88)));
    let x = x0 + 20.0;
    let logo = Rect::new(x, 30.0, 96.0, 60.0);
    out.push(Piece::logo(
        Some(c.clone()),
        logo,
        size::LABEL,
        theme::CELL_ROW,
    ));
    let tx = x + 96.0 + space::M;
    let tw = x0 + w - space::M - tx;
    let name = format!("{} {}", channel_number(c, l.cursor().index), c.name);
    let r = Rect::new(tx, 18.0, tw, 44.0);
    out.text(&name, r, HAlign::Left, size::BODY, true, theme::TEXT);
    let r = Rect::new(tx, 62.0, tw, 36.0);
    let group = l.group().name();
    out.text(
        group,
        r,
        HAlign::Left,
        size::LABEL,
        false,
        theme::TEXT_MUTED,
    );
    // Blank while the focus is still moving: the list loads once it rests.
    let Some(epg) = l.epg().filter(|e| e.key == c.key) else {
        return;
    };
    let list = epg.programmes();
    if list.is_empty() {
        let r = Rect::new(x, geom::PROG_Y, w - 40.0, geom::PROG_H);
        let none = msg::epg_no_data();
        out.text(none, r, HAlign::Left, size::LABEL, false, theme::TEXT_MUTED);
        return;
    }
    let focus = l.column() == Column::Programmes;
    let top = if l.column() == Column::Channels {
        centred_top(epg.cursor.index, list.len(), rows::PROGRAMMES)
    } else {
        epg.cursor.top
    };
    let end = (top + rows::PROGRAMMES).min(list.len());
    for (j, p) in list.iter().enumerate().take(end).skip(top) {
        let y = geom::PROG_Y + (j - top) as f32 * geom::PROG_H;
        let focused = focus && j == epg.cursor.index;
        if focused {
            let pill = Rect::new(x0 + 10.0, y + 4.0, w - 20.0, geom::PROG_H - 8.0);
            out.push(Piece::solid(pill, radius::PILL, theme::FOCUS));
        }
        let ink = if focused {
            theme::TEXT_ON_FOCUS
        } else if p.is_on_air(l.now()) {
            theme::ACCENT_TEXT
        } else {
            theme::TEXT
        };
        let start = time::hhmm(p.start, l.utc_offset());
        let r = Rect::new(x0 + 30.0, y, geom::TIME_W, geom::PROG_H);
        out.text(&start, r, HAlign::Left, size::LABEL, false, ink);
        let title_x = x0 + 30.0 + geom::TIME_W;
        let title_w = x0 + w - 20.0 - title_x;
        let r = Rect::new(title_x, y, title_w, geom::PROG_H);
        out.text(&p.title, r, HAlign::Left, size::LABEL, false, ink);
    }
}

/// The colour of a day in the days column: today in accent, past days muted.
pub fn day_ink(day: i32) -> [f32; 4] {
    match day {
        0 => theme::ACCENT_TEXT,
        d if d < 0 => theme::TEXT_MUTED,
        _ => theme::TEXT,
    }
}

/// The days column from `x0`.
fn days_column(out: &mut Out, l: &ChannelList, x0: f32) {
    let Some(epg) = l.epg() else {
        return;
    };
    let column = Rect::new(x0, 0.0, geom::DAYS_W, 1080.0);
    out.push(Piece::solid(column, 0.0, theme::PANEL));
    let (now, off) = (l.now(), l.utc_offset());
    let today = time::day_start(now, off);
    let c = epg.day_cursor;
    for k in c.visible(epg.days.len(), rows::DAYS) {
        let day = epg.days[k];
        let y = geom::PROG_Y + (k - c.top) as f32 * geom::PROG_H;
        let focused = k == c.index;
        if focused {
            let pill = Rect::new(x0 + 10.0, y + 4.0, geom::DAYS_W - 20.0, geom::PROG_H - 8.0);
            out.push(Piece::solid(pill, radius::PILL, theme::FOCUS));
        }
        let ink = if focused {
            theme::TEXT_ON_FOCUS
        } else {
            day_ink(day)
        };
        let label = time::day_label(today + i64::from(day) * DAY, now, off);
        let r = Rect::new(x0 + 30.0, y, geom::DAYS_W - 50.0, geom::PROG_H);
        out.text(&label, r, HAlign::Left, size::LABEL, false, ink);
    }
}

fn details(out: &mut Out, l: &ChannelList) {
    let (now, off) = (l.now(), l.utc_offset());
    match l.mode() {
        Mode::Overlay => {
            let x = if l.column() == Column::Days {
                geom::CARD_X_DAYS
            } else {
                geom::CARD_X
            };
            if let Some(p) = l.detail() {
                let card = Rect::new(x, geom::CARD_Y, geom::CARD_R - x, geom::CARD_H);
                out.details(card, p, now, off);
            }
        }
        Mode::Preview => {
            let r = PREVIEW_RECT;
            let (x, w) = (r.x as f32, r.w as f32);
            let above = Rect::new(x, 0.0, w, r.y as f32);
            let clock = time::clock(now, off);
            out.text(
                &clock,
                above,
                HAlign::Right,
                size::BODY,
                false,
                theme::ACCENT_TEXT,
            );
            if let Some(p) = l.detail() {
                let card = Rect::new(x, geom::PREVIEW_DETAIL_Y, w, geom::PREVIEW_DETAIL_H);
                out.details(card, p, now, off);
            }
        }
    }
}

/// History's display list.
#[derive(Default)]
pub(crate) struct HistoryView {
    cached: Cached,
}

impl HistoryView {
    #[cfg(test)]
    pub fn pieces(&self) -> &[Piece] {
        self.cached.pieces()
    }

    /// Lay History out again when it changed.
    pub fn prepare(&mut self, h: &History, m: &dyn Measure) {
        self.cached
            .prepare(h.generation(), |pieces| history(&mut Out { m, pieces }, h));
    }

    pub fn draw(&self, p: Painter, logos: &dyn Logos) {
        self.cached.paint(p, logos);
    }
}

fn history(out: &mut Out, h: &History) {
    let (now, off) = (h.now(), h.utc_offset());
    out.push(Piece::solid(Rect::FULL, 0.0, with_a(theme::BG, 0.85)));
    let title = Rect::new(geom::H_SIDE_X, 40.0, geom::H_SIDE_W, 80.0);
    let name = msg::channel_history();
    out.text(name, title, HAlign::Left, size::HEADER, false, theme::TEXT);
    if h.entries().is_empty() {
        let r = Rect::new(geom::H_X + geom::PAD, 480.0, geom::H_W, 60.0);
        let empty = msg::channel_history_empty();
        out.text(empty, r, HAlign::Left, size::BODY, false, theme::TEXT_MUTED);
        return;
    }
    let c = h.cursor();
    let today = time::day_start(now, off);
    for k in c.visible(h.lines().len(), history::ROWS) {
        let y = geom::H_TOP + (k - c.top) as f32 * geom::H_ROW;
        match h.lines()[k] {
            Line::Day(day) => {
                if k > c.top {
                    let rule = Rect::new(geom::H_X, y + 16.0, geom::H_W, 2.0);
                    out.push(Piece::solid(rule, 0.0, theme::PROGRESS_TRACK));
                }
                let label = time::day_label(today + i64::from(day) * DAY, now, off);
                let r = Rect::new(
                    geom::H_TEXT_X,
                    y + 36.0,
                    geom::H_TEXT_R - geom::H_TEXT_X,
                    60.0,
                );
                out.text(
                    &label,
                    r,
                    HAlign::Left,
                    size::LABEL,
                    true,
                    theme::TEXT_MUTED,
                );
            }
            Line::Entry(i) => history_row(out, h, i, k == c.index, y),
        }
    }
    if let Some(e) = h.focused_entry() {
        let card = Rect::new(
            geom::H_SIDE_X,
            geom::H_CARD_Y,
            geom::H_SIDE_W,
            geom::H_CARD_H,
        );
        out.push(Piece::solid(card, radius::PILL, with_a(theme::PANEL, 0.94)));
        let x = card.x + geom::PAD;
        let w = card.w - 2.0 * geom::PAD;
        let mut y = card.y + geom::PAD - 4.0;
        let first = Rect::new(x, y, w, 48.0);
        y += out.wrap(
            &e.programme.title,
            first,
            size::HEADING,
            true,
            theme::TEXT,
            2,
        );
        y += 8.0;
        let muted = theme::TEXT_MUTED;
        let span = h.when_line(&e.programme);
        let r = Rect::new(x, y, w - geom::ICON - space::M, 40.0);
        let sw = out.text(&span, r, HAlign::Left, size::LABEL, false, muted);
        out.push(Piece::Icon {
            icon: Icon::CatchUp,
            r: Rect::new(x + sw + space::M, y + 6.0, geom::ICON, geom::ICON),
            tint: muted,
        });
        y += 52.0;
        if let Some(d) = &e.programme.description {
            let lines = ((card.y + card.h - geom::PAD - y) / geom::DESC_LINE).floor() as usize;
            let first = Rect::new(x, y, w, geom::DESC_LINE);
            out.wrap(d, first, size::LABEL, false, muted, lines.min(3));
        }
    }
}

fn history_row(out: &mut Out, h: &History, i: usize, focused: bool, y: f32) {
    let e = &h.entries()[i];
    let Some(facts) = h.facts(i) else {
        return;
    };
    let pill = focused && !h.on_delete();
    if pill {
        let r = Rect::new(geom::H_X, y + 6.0, geom::H_W, geom::H_ROW - 12.0);
        out.push(Piece::solid(r, radius::PILL, theme::FOCUS));
    }
    let (mut ink, muted) = inks(pill);
    if !facts.playable && !pill {
        ink = theme::TEXT_MUTED;
    }
    let text_w = geom::H_TEXT_R - geom::H_TEXT_X;
    let r = Rect::new(geom::H_TEXT_X, y + 14.0, text_w, 50.0);
    out.text(&e.programme.title, r, HAlign::Left, size::BODY, false, ink);
    let line_y = y + 64.0;
    let clock_w = out.m.width_str("00:00", size::LABEL, false);
    let watched = time::hhmm(e.watched_at, h.utc_offset());
    let r = Rect::new(geom::H_TEXT_X, line_y, text_w, 40.0);
    out.text(&watched, r, HAlign::Right, size::LABEL, false, muted);
    let span = h.when_line(&e.programme);
    let avail = text_w - clock_w - space::M;
    let r = Rect::new(geom::H_TEXT_X, line_y, avail, 40.0);
    let sw = out.text(&span, r, HAlign::Left, size::LABEL, false, muted);
    let mut x = geom::H_TEXT_X + sw + space::M;
    let end = geom::H_TEXT_R - clock_w - space::M;
    let len = e.programme.duration_secs();
    if history::resume_at(e) > 0 {
        let (fill, track) = if pill {
            (theme::TEXT_ON_FOCUS, with_a(theme::TEXT_ON_FOCUS, 0.2))
        } else {
            (theme::PROGRESS, theme::PROGRESS_TRACK)
        };
        let frac = e.position_secs as f32 / len.max(1) as f32;
        if x + geom::H_BAR_W <= end {
            let bar = Rect::new(x, line_y + 18.0, geom::H_BAR_W, geom::BAR_H);
            out.pieces.extend(draw::bar(bar, frac, fill, track));
            x += geom::H_BAR_W + space::S;
            let left = time::minutes_label((len - e.position_secs + 59) / 60);
            let r = Rect::new(x, line_y, end - x, 40.0);
            out.text(&left, r, HAlign::Left, size::LABEL, false, muted);
        }
    } else if x + geom::ICON <= end {
        out.push(Piece::Icon {
            icon: Icon::CatchUp,
            r: Rect::new(x, line_y + 6.0, geom::ICON, geom::ICON),
            tint: muted,
        });
    }
    let logo = Rect::new(geom::H_LOGO_X, y + 32.0, 96.0, 56.0);
    out.push(Piece::logo(
        facts.channel.clone(),
        logo,
        size::LABEL,
        theme::CELL_ROW,
    ));
    let name = match facts.name() {
        "" => msg::channel_unknown(),
        name => name,
    };
    let r = Rect::new(
        geom::H_NAME_X,
        y,
        geom::H_TRASH_X - space::M - geom::H_NAME_X,
        geom::H_ROW,
    );
    out.text(name, r, HAlign::Left, size::BODY, true, ink);
    if focused {
        let r = Rect::new(geom::H_TRASH_X, y + 28.0, geom::H_TRASH_W, geom::H_TRASH_W);
        let tint = if h.on_delete() {
            out.push(Piece::solid(r, geom::H_TRASH_W * 0.5, theme::FOCUS));
            theme::TEXT_ON_FOCUS
        } else {
            theme::TEXT_ON_FOCUS_MUTED
        };
        out.push(Piece::Icon {
            icon: Icon::Trash,
            r: r.inset(16.0),
            tint,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fake::{FakeData as Fixture, Mono};
    use crate::data::{Group, HistoryEntry, Playback, Screen};
    use crate::input::{Key, Press};
    use crate::lists::{key_of, Nav, PREVIEW_RECT};
    use std::cell::RefCell;
    use std::ffi::CStr;

    const NOW: Timestamp = 1_724_771_520;

    /// A text piece: its run, where it is anchored and its colour.
    type Text = (String, f32, f32, [f32; 4]);

    fn texts(pieces: &[Piece]) -> Vec<Text> {
        pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Text { s, x, y, color, .. } => {
                    Some((s.to_string_lossy().into_owned(), *x, *y, *color))
                }
                _ => None,
            })
            .collect()
    }

    fn find<'a>(t: &'a [Text], s: &str) -> Option<&'a Text> {
        t.iter().find(|(x, ..)| x == s)
    }

    fn icons(pieces: &[Piece], icon: Icon) -> usize {
        pieces
            .iter()
            .filter(|p| matches!(p, Piece::Icon { icon: i, .. } if *i == icon))
            .count()
    }

    /// Every channel has a 16:9 logo, or none at all.
    struct Wide(bool);
    impl Logos for Wide {
        fn logo(&self, _: &Channel) -> Option<(u32, f32, f32)> {
            self.0.then_some((7, 160.0, 90.0))
        }
    }

    fn draw(s: &mut dyn Screen, m: &dyn Measure) {
        s.draw(Painter::recording(), m, &Wide(false));
    }

    fn list(d: &Fixture, mode: Mode, keys: &[Nav]) -> ChannelList {
        let mut l = ChannelList::new(mode, Group::All, d);
        for &nav in keys {
            let (key, press) = key_of(nav);
            l.key(key, press, d, 0);
        }
        draw(&mut l, &Mono);
        l
    }

    #[test]
    fn rows_show_number_name_programme_progress_and_catchup() {
        let mut d = Fixture::new(NOW, 30);
        d.playback = Playback {
            channel: Some(d.channels[1].key),
            ..Playback::default()
        };
        let l = list(&d, Mode::Overlay, &[]);
        let t = texts(l.pieces());
        // The playing channel is focused: dark ink on the pill.
        let (.., ink) = find(&t, "2 Channel 2").unwrap();
        assert_eq!(*ink, theme::TEXT_ON_FOCUS);
        // Another row: white name, programme title in the accent.
        let (_, x, y, ink) = find(&t, "3 Channel 3").unwrap();
        assert_eq!(*ink, theme::TEXT);
        let on_air = l.on_air(2).unwrap();
        let (.., ty, tink) = t
            .iter()
            .find(|(s, x2, y2, _)| *s == on_air.title && y2 > y && x2 == x)
            .unwrap();
        assert_eq!(*tink, theme::ACCENT_TEXT);
        assert!(*ty < y + geom::ROW_H);
        // Rows 1..=8 are on screen (8 rows), and channels 3, 6 have no catch-up mark.
        assert!(find(&t, "8 Channel 8").is_some());
        assert!(
            find(&t, "9 Channel 9").is_none(),
            "off-screen rows are not laid out"
        );
        assert_eq!(icons(l.pieces(), Icon::CatchUp), 8 - 2);
        assert_eq!(icons(l.pieces(), Icon::Play), 1);
        assert_eq!(
            l.pieces()
                .iter()
                .filter(|p| matches!(p, Piece::Logo { .. }))
                .count(),
            8 + 1,
            "a logo per row and one over the programme list"
        );
    }

    #[test]
    fn the_programme_list_marks_the_one_on_air() {
        let d = Fixture::new(NOW, 10);
        let l = list(&d, Mode::Overlay, &[Nav::Right]);
        let t = texts(l.pieces());
        let p = l.epg().unwrap().focused().unwrap();
        let hm = time::hhmm(p.start, 0);
        let rows: Vec<_> = t.iter().filter(|(s, ..)| *s == hm).collect();
        assert!(!rows.is_empty());
        // Focused in the programme column: dark ink on the pill.
        assert!(rows.iter().any(|(.., c)| *c == theme::TEXT_ON_FOCUS));
        // Leaving to the channels shows it in the accent instead.
        let l = list(&d, Mode::Overlay, &[Nav::Right, Nav::Left]);
        let t = texts(l.pieces());
        assert!(t
            .iter()
            .any(|(s, .., c)| *s == hm && *c == theme::ACCENT_TEXT));
    }

    #[test]
    fn days_column_today_in_accent_past_days_muted() {
        let d = Fixture::new(NOW, 10);
        let l = list(&d, Mode::Overlay, &[Nav::Right, Nav::Right, Nav::Up]);
        let t = texts(l.pieces());
        let today = find(&t, "Today").unwrap();
        assert_eq!(today.3, day_ink(0));
        assert_eq!(today.3, theme::ACCENT_TEXT);
        // Yesterday has the focus now.
        assert_eq!(find(&t, "Yesterday").unwrap().3, theme::TEXT_ON_FOCUS);
        assert_eq!(find(&t, "Sun, Aug 25").unwrap().3, theme::TEXT_MUTED);
        assert_eq!(find(&t, "Tomorrow").unwrap().3, theme::TEXT);
        // The details card moves right of the days column.
        let cards = l
            .pieces()
            .iter()
            .filter(|p| matches!(p, Piece::Fill { r, .. } if r.x == geom::CARD_X_DAYS))
            .count();
        assert_eq!(cards, 1);
    }

    #[test]
    fn preview_mode_leaves_the_video_rect_clear() {
        let d = Fixture::new(NOW, 10);
        let l = list(&d, Mode::Preview, &[]);
        let rect = Rect::new(
            PREVIEW_RECT.x as f32,
            PREVIEW_RECT.y as f32,
            PREVIEW_RECT.w as f32,
            PREVIEW_RECT.h as f32,
        );
        let overlaps = |r: &Rect| {
            r.x < rect.x + rect.w
                && rect.x < r.x + r.w
                && r.y < rect.y + rect.h
                && rect.y < r.y + r.h
        };
        for p in l.pieces() {
            let covers = match p {
                Piece::Fill { r, .. } | Piece::Icon { r, .. } | Piece::Logo { r, .. } => {
                    overlaps(r)
                }
                Piece::Text { x, y, .. } => overlaps(&Rect::new(*x, *y, 0.0, 0.0)),
            };
            assert!(!covers, "{p:?} covers the video");
        }
        let t = texts(l.pieces());
        assert!(
            find(&t, "Tue, Aug 27 15:12").is_some(),
            "the clock above the preview"
        );
    }

    /// The x extent of a piece, its text measured with [`Mono`].
    fn x_extent(p: &Piece) -> (f32, f32) {
        match p {
            Piece::Fill { r, .. } | Piece::Icon { r, .. } | Piece::Logo { r, .. } => {
                (r.x, r.x + r.w)
            }
            Piece::Text {
                s,
                x,
                size,
                bold,
                h,
                ..
            } => {
                let w = Mono.width(s, *size, *bold);
                match h {
                    HAlign::Left => (*x, x + w),
                    HAlign::Center => (x - w * 0.5, x + w * 0.5),
                    HAlign::Right => (x - w, *x),
                }
            }
        }
    }

    const COLUMNS: [Column; 4] = [
        Column::Groups,
        Column::Channels,
        Column::Programmes,
        Column::Days,
    ];

    /// The keys from a freshly opened list to `column`.
    fn keys_to(column: Column) -> &'static [Nav] {
        match column {
            Column::Groups => &[Nav::Left],
            Column::Channels => &[],
            Column::Programmes => &[Nav::Right],
            Column::Days => &[Nav::Right, Nav::Right],
        }
    }

    #[test]
    fn every_column_has_its_own_area() {
        for mode in [Mode::Overlay, Mode::Preview] {
            for column in COLUMNS {
                let at = layout(mode, column);
                let areas: Vec<(f32, f32)> = [at.groups, at.channels, at.programmes, at.days]
                    .into_iter()
                    .flatten()
                    .collect();
                for (i, &(x, w)) in areas.iter().enumerate() {
                    assert!(
                        x >= 0.0 && w > 0.0 && x + w <= 1920.0,
                        "{mode:?} {column:?}"
                    );
                    for &(x2, w2) in &areas[i + 1..] {
                        assert!(x + w <= x2 || x2 + w2 <= x, "{mode:?} {column:?} overlap");
                    }
                    // Left of the video and the details card under it, or of the overlay's card.
                    let right = match (mode, column) {
                        (Mode::Preview, _) => PREVIEW_RECT.x as f32,
                        (Mode::Overlay, Column::Days) => geom::CARD_X_DAYS,
                        (Mode::Overlay, _) => geom::CARD_X,
                    };
                    assert!(x + w <= right, "{mode:?} {column:?}: {x} + {w} > {right}");
                }
            }
        }
        // The channels make room for the groups, and in preview mode for the days.
        assert_eq!(
            layout(Mode::Overlay, Column::Groups).channels.unwrap().0,
            GROUPS.w
        );
        assert_eq!(layout(Mode::Preview, Column::Days).channels, None);
        let days = layout(Mode::Preview, Column::Days).days.unwrap();
        assert_eq!(days.0 + days.1, geom::PROG_X + geom::PROG_W);
    }

    #[test]
    fn nothing_drawn_crosses_into_the_video_or_its_details() {
        let d = Fixture::new(NOW, 10);
        let video = PREVIEW_RECT.x as f32;
        for column in COLUMNS {
            let mut l = list(&d, Mode::Preview, keys_to(column));
            assert_eq!(l.column(), column);
            l.draw(Painter::recording(), &Mono, &Wide(false));
            let groups = l.groups().map_or(&[][..], |g| g.pieces());
            assert_eq!(groups.is_empty(), column != Column::Groups);
            // The left columns end before the video; the clock and the card start at it.
            for p in l.pieces().iter().chain(groups) {
                let (x0, x1) = x_extent(p);
                assert!(
                    x1 <= video || x0 >= video,
                    "{column:?}: {p:?} crosses x = {video}"
                );
            }
            let card = l.pieces().iter().any(|p| {
                matches!(p, Piece::Fill { r, .. } if r.x == video && r.y == geom::PREVIEW_DETAIL_Y)
            });
            assert_eq!(
                card,
                column != Column::Groups,
                "{column:?}: the details card"
            );
        }
    }

    #[test]
    fn preview_days_put_the_programmes_and_days_left_of_the_video() {
        let d = Fixture::new(NOW, 10);
        let l = list(&d, Mode::Preview, &[Nav::Right, Nav::Right, Nav::Up]);
        let t = texts(l.pieces());
        // The days column, the focused day on its pill.
        let (_, x, ..) = find(&t, "Yesterday").unwrap();
        assert_eq!(*x, geom::PREVIEW_DAYS_X + 30.0);
        // The programme list starts at the left edge, no channel rows beside it.
        let p = l.epg().unwrap().programmes()[0].clone();
        let hm = time::hhmm(p.start, 0);
        assert!(t.iter().any(|(s, x, ..)| *s == hm && *x == 30.0));
        assert!(
            find(&t, "1 Channel 1").is_some(),
            "the programme list's header"
        );
        assert!(
            !t.iter().any(|(s, ..)| s.starts_with("2 Channel")),
            "the channel rows gave way"
        );
    }

    #[test]
    fn the_groups_panel_pushes_the_channels_right() {
        let d = Fixture::new(NOW, 10);
        let mut l = list(&d, Mode::Overlay, &[Nav::Left]);
        l.draw(Painter::recording(), &Mono, &Wide(false));
        for p in l.pieces() {
            assert!(x_extent(p).0 >= GROUPS.w, "{p:?} under the groups panel");
        }
        let (_, x, ..) = *find(&texts(l.pieces()), "1 Channel 1").unwrap();
        assert_eq!(x, GROUPS.w + geom::TEXT_X);
        let g = texts(l.groups().unwrap().pieces());
        assert_eq!(g[1].0, "All channels");
        assert_eq!(
            g[1].3,
            theme::TEXT_ON_FOCUS,
            "the focused group on the white pill"
        );
        for p in l.groups().unwrap().pieces() {
            assert!(x_extent(p).1 <= GROUPS.w, "{p:?} out of the panel");
        }
        assert_eq!(l.detail(), None, "no details card");
        assert_eq!(icons(l.pieces(), Icon::Play), 0);
    }

    #[test]
    fn long_names_are_fitted() {
        let mut d = Fixture::new(NOW, 3);
        d.channels[0].name = "A very long channel name that cannot fit the row at all".into();
        let l = list(&d, Mode::Overlay, &[]);
        let fitted: Vec<_> = l
            .pieces()
            .iter()
            .filter_map(|p| match p {
                Piece::Text { s, size, bold, .. }
                    if s.to_string_lossy().starts_with("1 A very long") =>
                {
                    Some((
                        s.to_string_lossy().into_owned(),
                        Mono.width(s, *size, *bold),
                    ))
                }
                _ => None,
            })
            .collect();
        assert_eq!(fitted.len(), 2, "the row and the programme list header");
        assert!(fitted.iter().all(|(s, _)| s.ends_with('\u{2026}')));
        assert!(fitted[0].1 <= geom::TEXT_R - geom::TEXT_X);
    }

    /// Records the text it measures.
    #[derive(Default)]
    struct Recording(RefCell<Vec<String>>);

    impl Measure for Recording {
        fn width(&self, s: &CStr, sz: i32, bold: bool) -> f32 {
            self.0.borrow_mut().push(s.to_string_lossy().into_owned());
            Mono.width(s, sz, bold)
        }
        fn cap_h(&self, sz: i32) -> f32 {
            Mono.cap_h(sz)
        }
        fn line_h(&self, sz: i32) -> f32 {
            Mono.line_h(sz)
        }
    }

    #[test]
    fn rows_that_did_not_change_are_not_measured_again() {
        let d = Fixture::new(NOW, 30);
        let mut l = ChannelList::new(Mode::Overlay, Group::All, &d);
        let m = Recording::default();
        draw(&mut l, &m);
        let first = m.0.take();
        assert!(first.iter().any(|s| s == "3 Channel 3"));
        // Nothing changed: nothing is laid out.
        draw(&mut l, &m);
        assert!(m.0.take().is_empty());
        // The focus moves from the first row to the second: only those two are measured again.
        l.key(Key::Down, Press::Short, &d, 0);
        draw(&mut l, &m);
        let again = m.0.take();
        assert!(again.iter().any(|s| s == "1 Channel 1"));
        assert!(again.iter().any(|s| s == "2 Channel 2"));
        assert!(!again.iter().any(|s| s.starts_with("3 Channel")));
        // Scrolling by a row measures the row coming into view.
        for _ in 0..6 {
            l.key(Key::Down, Press::Short, &d, 0);
        }
        draw(&mut l, &m);
        m.0.take();
        l.key(Key::Down, Press::Short, &d, 0);
        draw(&mut l, &m);
        let scrolled = m.0.take();
        assert!(scrolled.iter().any(|s| s == "10 Channel 10"));
        assert!(!scrolled.iter().any(|s| s == "5 Channel 5"));
    }

    /// `l` laid out from scratch.
    fn laid_out_fresh(l: &ChannelList) -> Vec<Piece> {
        let mut v = ChannelListView::default();
        v.prepare(l, &Mono);
        v.pieces().to_vec()
    }

    #[test]
    fn reused_rows_land_where_fresh_ones_would() {
        let d = Fixture::new(NOW, 30);
        let mut l = list(&d, Mode::Overlay, &[]);
        for _ in 0..9 {
            l.key(Key::Down, Press::Short, &d, 0);
            draw(&mut l, &Mono);
            assert_eq!(l.pieces(), laid_out_fresh(&l));
        }
    }

    fn history_fixture() -> Fixture {
        let mut d = Fixture::new(NOW, 6);
        let e = |ch: usize, start: i64, pos: i64, watched: i64| HistoryEntry {
            channel: d.channels[ch].key,
            programme: Programme {
                start,
                stop: start + 3600,
                title: format!("Show {ch}"),
                description: Some("About the show.".into()),
                category: None,
                icon: None,
            },
            position_secs: pos,
            watched_at: watched,
        };
        let entries = vec![
            e(0, NOW - 3 * 3600, 600, NOW - 3600),
            e(1, NOW - 30 * 3600, 3600, NOW - 26 * 3600),
        ];
        d.history = entries;
        d
    }

    fn history_drawn(d: &Fixture, keys: &[Nav]) -> History {
        let mut h = History::open(d);
        for &nav in keys {
            h.nav(nav);
        }
        draw(&mut h, &Mono);
        h
    }

    #[test]
    fn history_rows_and_day_headers() {
        let d = history_fixture();
        let h = history_drawn(&d, &[]);
        let t = texts(h.pieces());
        assert!(find(&t, "History").is_some());
        assert!(find(&t, "Yesterday").is_some());
        assert!(find(&t, "Today").is_some());
        // The focused (most recent) row: in progress, so a bar and the minutes left.
        assert_eq!(find(&t, "Show 0").map(|x| x.3), Some(theme::TEXT_ON_FOCUS));
        assert!(find(&t, "50 min").is_some());
        // The finished one shows the catch-up mark instead; one more in the details card.
        assert_eq!(icons(h.pieces(), Icon::CatchUp), 2);
        assert_eq!(icons(h.pieces(), Icon::Trash), 1);
        // Yesterday's programme carries its date.
        let span = time::programme_span(&h.entries()[0].programme, NOW, 0);
        assert!(span.starts_with("Mon, Aug 26"), "{span}");
        assert!(find(&t, &span).is_some());
        assert!(find(&t, "Channel 2").is_some());
    }

    #[test]
    fn history_delete_button_takes_the_focus_pill() {
        let d = history_fixture();
        let h = history_drawn(&d, &[Nav::Right]);
        let pills: Vec<_> = h
            .pieces()
            .iter()
            .filter_map(|p| match p {
                Piece::Fill { r, top, .. } if *top == theme::FOCUS => Some(*r),
                _ => None,
            })
            .collect();
        assert_eq!(pills.len(), 1);
        assert_eq!(pills[0].x, geom::H_TRASH_X);
    }

    #[test]
    fn empty_history_says_so() {
        let d = Fixture::new(NOW, 3);
        let h = history_drawn(&d, &[]);
        assert!(find(&texts(h.pieces()), "No catch-up programmes watched yet").is_some());
    }

    #[test]
    fn drawing_with_logos_does_not_panic() {
        let d = history_fixture();
        let mut l = list(&d, Mode::Preview, &[Nav::Right, Nav::Right]);
        l.draw(Painter::recording(), &Mono, &Wide(true));
        let mut h = History::open(&d);
        h.draw(Painter::recording(), &Mono, &Wide(true));
    }

    #[test]
    fn logo_pieces_carry_the_channel() {
        let d = Fixture::new(NOW, 3);
        let l = list(&d, Mode::Overlay, &[]);
        let logos: Vec<_> = l
            .pieces()
            .iter()
            .filter_map(|p| match p {
                Piece::Logo {
                    channel,
                    fallback,
                    tile,
                    ..
                } => Some((
                    channel.as_ref().map(|c| c.key),
                    fallback.to_string_lossy(),
                    *tile,
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            logos[0],
            (Some(l.channels()[0].key), "C1".into(), theme::CELL_ROW)
        );
    }
}
