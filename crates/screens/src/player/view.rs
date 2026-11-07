//! Drawing the player panels: [`build`] lays them out into a display list when the screen's
//! [`generation`](super::PlayerScreen::generation) moves, measuring and fitting text then, never
//! while painting.
//!
//! Geometry was measured from TiviMate at 1080p; colours, type and spacing come from
//! [`crate::theme`]. [`spots`] says what the pointer can focus on the same layout.

use super::model::{Header, TileKind};
use super::transport::{Button, Focus, BUTTONS};
use super::{Overlay, PlayerScreen, READ_VIDEO};
use crate::draw::{self, HAlign, Piece, Run, VAlign};
use crate::icons::Icon;
use crate::lists::Rows;
use crate::point::{Spot, Spots};
use crate::theme::{self, radius, size, space};
use std::os::raw::c_int;
use tv_gfx::gfx::tokens::with_a;
use tv_machine::machine::Measure;
use tv_platform::i18n::msg;
use tv_ui::consts::{SCR_H, SCR_W};
use tv_ui::prose_scroll::{paging, scroll_at};
use tv_ui::Rect;

/// Panel geometry at 1080p.
pub(super) mod geom {
    /// Left edge of the panel content and its mirror on the right.
    pub const X: f32 = 81.0;
    pub const LOGO_W: f32 = crate::draw::LOGO_BOX.0 as f32;
    pub const LOGO_H: f32 = crate::draw::LOGO_BOX.1 as f32;
    /// Left edge of the programme text, right of the logo.
    pub const TEXT_X: f32 = 360.0;
    /// Header top for the banner, and for the info and playback panels.
    pub const BANNER_TOP: f32 = 818.0;
    pub const PANEL_TOP: f32 = 598.0;
    /// From the header top: the times row, the description, the next programme.
    pub const ROW_DY: f32 = 64.0;
    pub const DESC_DY: f32 = 112.0;
    pub const DESC_LINE: f32 = 36.0;
    pub const NEXT_DY: f32 = 112.0;
    /// The catch-up position line of the info panel.
    pub const LINE_Y: f32 = 770.0;
    pub const TILES_Y: f32 = 822.0;
    pub const TILE_W: f32 = 248.0;
    pub const TILE_H: f32 = 168.0;
    pub const TILE_PITCH: f32 = 264.0;
    pub const TILE_POP: f32 = 1.12;
    pub const SEEK_Y: f32 = 804.0;
    pub const SEEK_H: f32 = 6.0;
    pub const KNOB_R: f32 = 14.0;
    pub const KNOB_R_REST: f32 = 9.0;
    /// Centre line of the read-out and the transport buttons.
    pub const TRANSPORT_Y: f32 = 867.0;
    /// Button centres, in [`BUTTONS`](super::BUTTONS) order.
    pub const BUTTON_X: [f32; 7] = [751.0, 855.0, 960.0, 1063.0, 1167.0, 1590.0, 1700.0];
    pub const BUTTON_ICON: f32 = 44.0;
    pub const BUTTON_DISC: f32 = 76.0;
    /// The programme details: the text column right of the video, to the panels' right margin.
    const VIDEO_RIGHT: f32 = (super::READ_VIDEO.x + super::READ_VIDEO.w) as f32;
    pub const READ_X: f32 = VIDEO_RIGHT + crate::theme::space::XXL;
    /// The description's line pitch, the bottom its lines end by, and the page row under it.
    pub const DESC_PITCH: f32 = 40.0;
    pub const DESC_BOTTOM: f32 = 966.0;
    pub const PAGE_Y: f32 = 990.0;
    /// Clicks right of this (half the gutter past the video) do nothing.
    pub const DEAD_X: f32 = VIDEO_RIGHT + crate::theme::space::XXL * 0.5;
    pub const CHIP_H: f32 = 30.0;
    pub const BAR_H: f32 = 4.0;
}

/// `s` from `x` (its left end, centre or right end, as `h` says) on the line from `y` down.
fn text(s: Run, x: f32, y: f32, size: c_int, bold: bool, color: [f32; 4], h: HAlign) -> Piece {
    Piece::Text {
        s,
        x,
        y,
        size,
        bold,
        color,
        h,
        v: VAlign::Top,
    }
}

/// Lays text out left to right on one line.
struct Row<'a> {
    m: &'a dyn Measure,
    out: &'a mut Vec<Piece>,
    x: f32,
    y: f32,
    right: f32,
}

impl Row<'_> {
    /// Add `s` unless it no longer fits before the right edge.
    fn text(&mut self, s: &str, sz: c_int, bold: bool, color: [f32; 4], gap: f32) {
        if s.is_empty() {
            return;
        }
        let w = self.m.width_str(s, sz, bold);
        if self.x + w > self.right {
            return;
        }
        self.out
            .push(Piece::text(draw::run(s), self.x, self.y, sz, bold, color));
        self.x += w + gap;
    }

    /// Add `s` ellipsised into what is left of the line.
    fn fitted(&mut self, s: &str, sz: c_int, bold: bool, color: [f32; 4], gap: f32) {
        let budget = self.right - self.x;
        if s.is_empty() || budget < sz as f32 {
            return;
        }
        let (run, w) = draw::fit(self.m, s, budget, sz, bold);
        self.out
            .push(Piece::text(run, self.x, self.y, sz, bold, color));
        self.x += w + gap;
    }

    fn icon(&mut self, icon: Icon, px: f32, tint: [f32; 4], gap: f32) {
        if self.x + px > self.right {
            return;
        }
        let line = self.m.line_h(size::LABEL);
        let r = Rect::new(self.x, self.y + (line - px) * 0.5, px, px);
        self.out.push(Piece::Icon { icon, r, tint });
        self.x += px + gap;
    }

    fn bar(&mut self, w: f32, frac: f32, gap: f32) {
        let line = self.m.line_h(size::LABEL);
        let r = Rect::new(self.x, self.y + (line - geom::BAR_H) * 0.5, w, geom::BAR_H);
        self.out
            .extend(draw::bar(r, frac, theme::PROGRESS, theme::PROGRESS_TRACK));
        self.x += w + gap;
    }

    fn chip(&mut self, s: &str) -> bool {
        let w = self.m.width_str(s, size::CAPTION, true) + 2.0 * space::S;
        if self.x + w > self.right {
            return false;
        }
        let line = self.m.line_h(size::LABEL);
        let top = self.y + (line - geom::CHIP_H) * 0.5;
        let text_y = top + (geom::CHIP_H - self.m.line_h(size::CAPTION)) * 0.5;
        self.out.push(Piece::solid(
            Rect::new(self.x, top, w, geom::CHIP_H),
            radius::CELL,
            theme::CHIP,
        ));
        self.out.push(text(
            draw::run(s),
            self.x + w * 0.5,
            text_y,
            size::CAPTION,
            true,
            theme::TEXT_ON_FOCUS,
            HAlign::Center,
        ));
        self.x += w + space::S + 2.0;
        true
    }
}

/// Lay the panels of `s` out into `out`; how many pages the programme details' description
/// makes (1 for every other panel).
pub fn build(s: &PlayerScreen, m: &dyn Measure, out: &mut Vec<Piece>) -> usize {
    let mut pages = 1;
    match s.overlay {
        Overlay::Hidden => {}
        Overlay::Banner => {
            top_bar(out, &s.header, m);
            scrim(out, geom::BANNER_TOP - 140.0);
            header(out, &s.header, geom::BANNER_TOP, true, m);
        }
        Overlay::Info { tile } => {
            top_bar(out, &s.header, m);
            scrim(out, geom::PANEL_TOP - 160.0);
            header(out, &s.header, geom::PANEL_TOP, false, m);
            if s.header.catchup {
                let r = Rect::new(geom::X, geom::LINE_Y, SCR_W - 2.0 * geom::X, geom::BAR_H);
                out.extend(draw::bar(
                    r,
                    s.readout.knob,
                    theme::ACCENT,
                    theme::SEEK_TRACK,
                ));
            }
            tiles(out, s, tile, m);
            let c = SCR_W * 0.5;
            out.push(Piece::Icon {
                icon: Icon::ChevronDown,
                r: Rect::new(c - 20.0, SCR_H - 64.0, 40.0, 40.0),
                tint: theme::TEXT_MUTED,
            });
        }
        // TiviMate hides the tile row while the seek bar and its buttons are up.
        Overlay::Playback => {
            top_bar(out, &s.header, m);
            scrim(out, geom::PANEL_TOP - 160.0);
            header(out, &s.header, geom::PANEL_TOP, false, m);
            transport(out, s, m);
        }
        Overlay::Card { .. } => pages = read(out, s, m),
    }
    if let Some(n) = &s.number {
        crate::number::paint(out, n, SCR_W - geom::X, geom::X, m);
    }
    pages
}

fn top_bar(out: &mut Vec<Piece>, h: &Header, m: &dyn Measure) {
    out.push(Piece::Fill {
        r: Rect::new(0.0, 0.0, SCR_W, 160.0),
        rad: 0.0,
        top: with_a(theme::BG, 0.75),
        bot: with_a(theme::BG, 0.0),
    });
    let clock_w = m.width_str(&h.clock, size::BODY, false);
    let group_w = SCR_W - 2.0 * space::XL - clock_w - space::XXL;
    let (group, _) = draw::fit(m, &h.group, group_w, size::BODY, false);
    out.push(Piece::text(
        group,
        space::XL,
        space::L,
        size::BODY,
        false,
        theme::TEXT,
    ));
    out.push(text(
        draw::run(&h.clock),
        SCR_W - space::XL,
        space::L,
        size::BODY,
        false,
        theme::TEXT,
        HAlign::Right,
    ));
}

/// The dark ramp under the panels, from `top` to the bottom edge.
fn scrim(out: &mut Vec<Piece>, top: f32) {
    out.push(Piece::Fill {
        r: Rect::new(0.0, top, SCR_W, SCR_H - top),
        rad: 0.0,
        top: with_a(theme::BG, 0.0),
        bot: theme::SCRIM,
    });
}

/// Logo, title, the times row, the description (banner) and the next programme.
fn header(out: &mut Vec<Piece>, h: &Header, top: f32, with_desc: bool, m: &dyn Measure) {
    let right = SCR_W - geom::X;
    let text_w = right - geom::TEXT_X;
    let desc: Vec<Run> = match (&h.description, with_desc) {
        (Some(d), true) => draw::wrap(m, d, text_w, size::LABEL, false, 2),
        _ => Vec::new(),
    };
    let next_y = if desc.is_empty() {
        top + geom::NEXT_DY
    } else {
        top + geom::DESC_DY + desc.len() as f32 * geom::DESC_LINE + space::S
    };
    let block_h = next_y + m.line_h(size::LABEL) - top;
    let logo = Rect::new(
        geom::X,
        top + (block_h - geom::LOGO_H) * 0.5,
        geom::LOGO_W,
        geom::LOGO_H,
    );
    out.push(Piece::logo(
        h.channel.clone(),
        logo,
        size::HEADING,
        theme::PANEL,
    ));
    let (title, _) = draw::fit(m, &h.title, text_w, size::TITLE, true);
    out.push(Piece::text(
        title,
        geom::TEXT_X,
        top,
        size::TITLE,
        true,
        theme::TEXT,
    ));

    let mut row = Row {
        m,
        out,
        x: geom::TEXT_X,
        y: top + geom::ROW_DY,
        right,
    };
    row.text(&h.times, size::LABEL, false, theme::TEXT_MUTED, space::L);
    if h.catchup {
        row.icon(Icon::CatchUp, 28.0, theme::TEXT_MUTED, space::S);
    } else if let Some(f) = h.progress {
        row.bar(80.0, f, space::M);
    }
    row.text(
        &h.remaining,
        size::LABEL,
        false,
        theme::TEXT_MUTED,
        space::XL,
    );
    row.text(&h.number, size::LABEL, true, theme::TEXT, space::M);
    row.fitted(&h.name, size::LABEL, true, theme::TEXT, space::S);
    if h.archive {
        row.icon(Icon::CatchUp, 22.0, theme::TEXT_MUTED, space::S);
    }
    row.x += space::M;
    for c in &h.chips {
        if !row.chip(c) {
            break;
        }
    }

    for (i, line) in desc.into_iter().enumerate() {
        out.push(Piece::text(
            line,
            geom::TEXT_X,
            top + geom::DESC_DY + i as f32 * geom::DESC_LINE,
            size::LABEL,
            false,
            theme::TEXT_MUTED,
        ));
    }
    let mut row = Row {
        m,
        out,
        x: geom::TEXT_X,
        y: next_y,
        right,
    };
    row.text(
        &h.next_times,
        size::LABEL,
        false,
        theme::TEXT_MUTED,
        space::L,
    );
    row.fitted(&h.next_title, size::LABEL, false, theme::TEXT, 0.0);
}

/// [`Spot`] areas of the player: the info panel's tiles, and the playback panel's seek bar and
/// buttons (by their place in [`BUTTONS`]).
pub const TILE: u8 = 0;
pub const SEEK: u8 = 1;
pub const BUTTON: u8 = 2;

/// What the pointer can focus on the panel that is up: the tiles on screen, the seek bar, the
/// transport buttons.
pub fn spots(s: &PlayerScreen, out: &mut Spots) {
    out.clear();
    match s.overlay {
        Overlay::Info { .. } => {
            for (i, r) in tile_slots(s) {
                out.add(r, Spot::row(TILE, i));
            }
        }
        Overlay::Playback => {
            let band = Rect::new(geom::X, geom::SEEK_Y - 24.0, SCR_W - 2.0 * geom::X, 48.0);
            out.add(band, Spot::row(SEEK, 0));
            for (i, cx) in geom::BUTTON_X.into_iter().enumerate() {
                out.add(button_disc(cx), Spot::row(BUTTON, i));
            }
        }
        // The text reads, it is not pointed at; a click on the picture is OK, which closes.
        Overlay::Card { .. } => out.dead(Rect::new(geom::DEAD_X, 0.0, SCR_W - geom::DEAD_X, SCR_H)),
        _ => {}
    }
}

/// The tiles the row shows at a time, the focus keeping one of context on either side while
/// the keys move it; the pointer never scrolls the row ([`crate::lists::Cursor::point`]).
pub const TILE_ROWS: Rows = Rows::new(
    ((SCR_W - geom::X - geom::TILE_W) / geom::TILE_PITCH) as usize + 1,
    1,
);

/// The tiles on screen, from the first one shown: their index and where each stands before the
/// focused one pops.
fn tile_slots(s: &PlayerScreen) -> impl Iterator<Item = (usize, Rect)> {
    let first = s.tiles_top;
    let end = s.tiles.len().min(first + TILE_ROWS.shown);
    (first..end).enumerate().map(|(slot, i)| {
        let x = geom::X + slot as f32 * geom::TILE_PITCH;
        (i, Rect::new(x, geom::TILES_Y, geom::TILE_W, geom::TILE_H))
    })
}

/// The info panel's tile row; `focus` is the focused tile.
fn tiles(out: &mut Vec<Piece>, s: &PlayerScreen, focus: usize, m: &dyn Measure) {
    for (i, base) in tile_slots(s) {
        let t = &s.tiles[i];
        let focused = focus == i;
        let r = if focused {
            base.scaled(geom::TILE_POP)
        } else {
            base
        };
        let fill = if focused {
            theme::PANEL_RAISED
        } else {
            theme::PANEL
        };
        out.push(Piece::solid(r, radius::PILL, fill));
        let cx = r.x + r.w * 0.5;
        let label_w = r.w - 2.0 * space::M;
        let icon = match t.kind {
            TileKind::Description => Some(Icon::Info),
            TileKind::Guide => Some(Icon::Guide),
            TileKind::History => Some(Icon::CatchUp),
            TileKind::Channel(_) => None,
        };
        if let Some(icon) = icon {
            out.push(Piece::Icon {
                icon,
                r: Rect::new(cx - 28.0, r.y + r.h * 0.2, 56.0, 56.0),
                tint: theme::TEXT,
            });
            let (label, _) = draw::fit(m, &t.label, label_w, size::BODY, false);
            out.push(text(
                label,
                cx,
                r.y + r.h * 0.62,
                size::BODY,
                false,
                theme::TEXT,
                HAlign::Center,
            ));
            continue;
        }
        let logo = Rect::new(cx - 80.0, r.y + space::M, 160.0, r.h * 0.5);
        out.push(Piece::logo(
            t.channel.clone(),
            logo,
            size::HEADING,
            theme::PANEL,
        ));
        let (label, _) = draw::fit(m, &t.label, label_w, size::LABEL, false);
        out.push(Piece::text(
            label,
            r.x + space::M,
            r.y + r.h * 0.66,
            size::LABEL,
            false,
            theme::ACCENT_TEXT,
        ));
        if let Some(f) = t.progress {
            let b = Rect::new(r.x, r.y + r.h - geom::BAR_H, r.w, geom::BAR_H);
            out.extend(draw::bar(b, f, theme::PROGRESS, theme::PROGRESS_TRACK));
        }
    }
}

/// The seek bar, its read-out and the transport buttons.
fn transport(out: &mut Vec<Piece>, s: &PlayerScreen, m: &dyn Measure) {
    let r = &s.readout;
    let focus = s.transport.focus;
    let track = Rect::new(
        geom::X,
        geom::SEEK_Y - geom::SEEK_H * 0.5,
        SCR_W - 2.0 * geom::X,
        geom::SEEK_H,
    );
    let rad = geom::SEEK_H * 0.5;
    // Not aired yet: the bar beyond the live edge, dimmer.
    out.push(Piece::solid(track, rad, with_a(theme::TEXT, 0.25)));
    let aired = Rect::new(track.x, track.y, track.w * r.aired, track.h);
    out.push(Piece::solid(aired, rad, theme::SEEK_TRACK));
    let played = Rect::new(track.x, track.y, (track.w * r.knob).max(track.h), track.h);
    out.push(Piece::solid(played, rad, theme::ACCENT));
    let k = if focus == Focus::Seek {
        geom::KNOB_R
    } else {
        geom::KNOB_R_REST
    };
    let kx = track.x + track.w * r.knob;
    let knob = Rect::new(kx - k, geom::SEEK_Y - k, 2.0 * k, 2.0 * k);
    out.push(Piece::solid(knob, k, theme::TEXT));

    let y = geom::TRANSPORT_Y - m.line_h(size::BODY) * 0.5;
    let mut row = Row {
        m,
        out,
        x: geom::X,
        y,
        right: geom::BUTTON_X[0] - geom::BUTTON_DISC,
    };
    row.text(&r.elapsed, size::BODY, false, theme::TEXT, 0.0);
    row.text(" / ", size::BODY, false, theme::TEXT_MUTED, 0.0);
    row.text(&r.total, size::BODY, false, theme::TEXT_MUTED, space::M);
    row.text(&r.offset, size::BODY, false, theme::TEXT, 0.0);

    for (b, cx) in BUTTONS.iter().zip(geom::BUTTON_X) {
        let icon = match b {
            Button::Prev => Icon::SkipPrevious,
            Button::Rewind => Icon::Rewind,
            Button::PlayPause if r.paused => Icon::Play,
            Button::PlayPause => Icon::Pause,
            Button::FastForward => Icon::FastForward,
            Button::Next => Icon::SkipNext,
            Button::Restart => Icon::Restart,
            Button::Live => Icon::Live,
        };
        let focused = focus == Focus::Button(*b);
        let tint = if focused {
            theme::TEXT_ON_FOCUS
        } else if s.button_enabled(*b) {
            theme::TEXT
        } else {
            with_a(theme::TEXT_MUTED, 0.4)
        };
        if focused {
            let disc = button_disc(cx);
            out.push(Piece::solid(disc, disc.w * 0.5, theme::FOCUS));
        }
        let i = geom::BUTTON_ICON;
        out.push(Piece::Icon {
            icon,
            r: Rect::new(cx - i * 0.5, geom::TRANSPORT_Y - i * 0.5, i, i),
            tint,
        });
    }
}

/// The disc behind the transport button centred at `cx`: drawn when focused, pointed at always.
fn button_disc(cx: f32) -> Rect {
    let d = geom::BUTTON_DISC;
    Rect::new(cx - d * 0.5, geom::TRANSPORT_Y - d * 0.5, d, d)
}

/// The video's place in the programme details, on the graphics plane.
fn read_video() -> Rect {
    let v = READ_VIDEO;
    Rect::new(v.x as f32, v.y as f32, v.w as f32, v.h as f32)
}

/// The programme details: opaque ground around the video (nothing over it), the clock, the
/// channel under the video, and on the right the programme's title, times and description, one
/// page of it. Pages overlap by a line and the last one is shown full. Returns the page count.
fn read(out: &mut Vec<Piece>, s: &PlayerScreen, m: &dyn Measure) -> usize {
    let v = read_video();
    for r in [
        Rect::new(0.0, 0.0, SCR_W, v.y),
        Rect::new(0.0, v.y + v.h, SCR_W, SCR_H - v.y - v.h),
        Rect::new(0.0, v.y, v.x, v.h),
        Rect::new(v.x + v.w, v.y, SCR_W - v.x - v.w, v.h),
    ] {
        out.push(Piece::solid(r, 0.0, theme::BG));
    }
    let h = &s.header;
    out.push(text(
        draw::run(&h.clock),
        SCR_W - space::XL,
        space::L,
        size::BODY,
        false,
        theme::TEXT,
        HAlign::Right,
    ));

    // Under the video: the channel on the left, the minutes left on the right.
    let strip_y = v.y + v.h + space::L;
    let icon = 28.0;
    let tail = m.width_str(&h.remaining, size::LABEL, false)
        + if h.catchup { icon + space::S } else { 0.0 };
    let video_right = v.x + v.w;
    let mut row = Row {
        m,
        out,
        x: v.x,
        y: strip_y,
        right: video_right - tail - space::L,
    };
    row.text(&h.number, size::LABEL, true, theme::TEXT, space::M);
    row.fitted(&h.name, size::LABEL, true, theme::TEXT, 0.0);
    let mut row = Row {
        m,
        out,
        x: video_right - tail,
        y: strip_y,
        right: video_right,
    };
    if h.catchup {
        row.icon(Icon::CatchUp, icon, theme::TEXT_MUTED, space::S);
    }
    row.text(&h.remaining, size::LABEL, false, theme::TEXT_MUTED, 0.0);
    if let Some(f) = h.progress {
        let y = strip_y + m.line_h(size::LABEL) + space::M;
        let r = Rect::new(v.x, y, v.w, geom::BAR_H);
        out.extend(draw::bar(r, f, theme::PROGRESS, theme::PROGRESS_TRACK));
    }

    let c = &s.card;
    let x = geom::READ_X;
    let w = SCR_W - geom::X - x;
    let mut y = v.y;
    for line in draw::wrap(m, &c.title, w, size::TITLE, true, 2) {
        out.push(Piece::text(line, x, y, size::TITLE, true, theme::TEXT));
        y += m.line_h(size::TITLE);
    }
    y += space::S;
    let mut row = Row {
        m,
        out,
        x,
        y,
        right: x + w,
    };
    row.text(&c.times, size::LABEL, false, theme::TEXT_MUTED, space::L);
    row.text(&c.duration, size::LABEL, false, theme::TEXT_MUTED, space::L);
    if let Some(f) = c.progress {
        row.bar(80.0, f, space::M);
    }
    if let Some(cat) = &c.category {
        row.chip(cat);
    }

    let top = y + m.line_h(size::LABEL) + space::L;
    let mut lines = c.description.as_deref().map_or_else(Vec::new, |d| {
        draw::wrap(m, d, w, size::BODY, false, usize::MAX)
    });
    if lines.is_empty() {
        lines.push(draw::run(msg::epg_no_description()));
    }
    let per = (((geom::DESC_BOTTOM - top) / geom::DESC_PITCH) as usize).max(2);
    // One line kept from the page before.
    let step = (per - 1) as f32;
    let (max, pages) = paging(lines.len() as f32, per as f32, step);
    let page = s.card_page.min(pages - 1);
    let start = scroll_at(page + 1, max, step) as usize;
    for (i, line) in lines.into_iter().skip(start).take(per).enumerate() {
        out.push(Piece::text(
            line,
            x,
            top + i as f32 * geom::DESC_PITCH,
            size::BODY,
            false,
            theme::TEXT_MUTED,
        ));
    }

    if pages > 1 {
        let ink = |more: bool| {
            if more {
                theme::TEXT
            } else {
                with_a(theme::TEXT_MUTED, 0.4)
            }
        };
        let mut row = Row {
            m,
            out,
            x,
            y: geom::PAGE_Y,
            right: x + w,
        };
        row.icon(Icon::ChevronUp, icon, ink(page > 0), 0.0);
        row.icon(Icon::ChevronDown, icon, ink(page + 1 < pages), space::M);
        let at = format!("{} / {pages}", page + 1);
        row.text(&at, size::LABEL, false, theme::TEXT_MUTED, 0.0);
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fake::{FakeData, Mono};
    use crate::data::{Navigate, Output, Playback, Screen};
    use crate::input::{Key, Press};

    const NOW: iptv::model::Timestamp = 1_787_843_520;

    /// The middle of a spot's rectangle once laid out, as the pointer would rest on it.
    fn centre(s: &mut PlayerScreen, area: u8, index: usize) -> (f32, f32) {
        s.layout(&Mono);
        let r = s
            .spots
            .rects()
            .find(|(_, spot)| *spot == Spot::row(area, index));
        let r = r.expect("a spot").0;
        (r.x + r.w * 0.5, r.y + r.h * 0.5)
    }

    #[test]
    fn the_pointer_focuses_tiles_and_working_transport_buttons() {
        let mut d = FakeData::new(NOW, 6);
        d.playback = Playback {
            channel: Some(d.channels[0].key),
            ..Playback::default()
        };
        d.recents = vec![d.channels[3].key, d.channels[4].key];
        let mut s = PlayerScreen::new(Default::default());
        assert!(!s.wheel(), "Up and Down zap");
        let (x, y) = (960.0, 540.0);
        assert_eq!(s.point(x, y, &d, 0), Output::default(), "nothing up");
        s.key(Key::Ok, Press::Short, &d, 0);
        assert_eq!(s.overlay(), Overlay::Info { tile: 0 });
        let (x, y) = centre(&mut s, TILE, 2);
        assert_eq!(s.point(x, y, &d, 10), Output::redraw());
        assert_eq!(s.overlay(), Overlay::Info { tile: 2 });
        assert_eq!(s.point(x + 5.0, y, &d, 20), Output::default());
        let out = s.key(Key::Ok, Press::Short, &d, 30);
        assert_eq!(out.navigate, [Navigate::History]);
        // The playback panel: the seek bar and the buttons that work for live with an archive.
        s.key(Key::Ok, Press::Short, &d, 40);
        s.key(Key::Up, Press::Short, &d, 50);
        assert_eq!(s.overlay(), Overlay::Playback);
        let rewind = BUTTONS.iter().position(|b| *b == Button::Rewind).unwrap();
        let (x, y) = centre(&mut s, BUTTON, rewind);
        assert_eq!(s.point(x, y, &d, 60), Output::redraw());
        assert_eq!(s.transport_focus(), Focus::Button(Button::Rewind));
        let pause = BUTTONS
            .iter()
            .position(|b| *b == Button::PlayPause)
            .unwrap();
        let (x, y) = centre(&mut s, BUTTON, pause);
        assert_eq!(s.point(x, y, &d, 70), Output::default(), "not on live");
        let (x, y) = centre(&mut s, SEEK, 0);
        assert_eq!(s.point(x, y, &d, 80), Output::redraw());
        assert_eq!(s.transport_focus(), Focus::Seek);
    }

    /// The programme details over channel 0 playing live, laid out.
    fn reading() -> (FakeData, PlayerScreen) {
        let mut d = FakeData::new(NOW, 6);
        d.playback = Playback {
            channel: Some(d.channels[0].key),
            ..Playback::default()
        };
        let mut s = PlayerScreen::new(Default::default());
        s.key(Key::Ok, Press::Short, &d, 0);
        s.key(Key::Ok, Press::Short, &d, 10);
        assert_eq!(s.overlay(), Overlay::Card { index: 0 });
        s.layout(&Mono);
        (d, s)
    }

    #[test]
    fn the_programme_details_leave_the_video_rect_clear() {
        let (_, mut s) = reading();
        let rect = read_video();
        let overlaps = |r: &Rect| {
            r.x < rect.x + rect.w
                && rect.x < r.x + r.w
                && r.y < rect.y + rect.h
                && rect.y < r.y + r.h
        };
        let pieces = s.layout(&Mono);
        for p in pieces {
            let covers = match p {
                Piece::Fill { r, .. } | Piece::Icon { r, .. } | Piece::Logo { r, .. } => {
                    overlaps(r)
                }
                Piece::Text { x, y, .. } => overlaps(&Rect::new(*x, *y, 0.0, 0.0)),
            };
            assert!(!covers, "{p:?} covers the video");
        }
        // The ground is opaque all around it.
        let ground: f32 = pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Fill { r, top, .. } if *top == theme::BG => Some(r.w * r.h),
                _ => None,
            })
            .sum();
        assert_eq!(ground, SCR_W * SCR_H - rect.w * rect.h);
    }

    #[test]
    fn a_click_on_the_text_does_nothing_and_on_the_picture_closes() {
        let (d, mut s) = reading();
        assert!(s.dead(1500.0, 500.0), "the text column");
        assert!(s.dead(1500.0, 1000.0), "the page row");
        assert!(!s.dead(500.0, 400.0), "the picture");
        assert!(!s.dead(500.0, 720.0), "the strip under it");
        assert_eq!(s.point(1500.0, 500.0, &d, 20), Output::default());
        assert_eq!(s.point(500.0, 400.0, &d, 20), Output::default());
        // A click on the picture is OK on the focus: it closes them.
        s.key(Key::Ok, Press::Short, &d, 30);
        assert_eq!(s.overlay(), Overlay::Hidden);
        s.layout(&Mono);
        assert!(!s.dead(1500.0, 500.0), "nothing is dead on full screen");
    }

    /// Where slot `slot` of the tile row stands on screen, whatever tile it shows.
    fn slot(s: &mut PlayerScreen, slot: usize) -> (f32, f32) {
        s.layout(&Mono);
        let r = s.spots.rects().nth(slot).expect("a tile").0;
        (r.x + r.w * 0.5, r.y + r.h * 0.5)
    }

    #[test]
    fn pointing_at_a_tile_never_scrolls_the_row() {
        let mut d = FakeData::new(NOW, 13);
        d.playback = Playback {
            channel: Some(d.channels[0].key),
            ..Playback::default()
        };
        d.recents = d.channels[1..].iter().map(|c| c.key).collect();
        let mut s = PlayerScreen::new(Default::default());
        s.key(Key::Ok, Press::Short, &d, 0);
        assert_eq!(s.tiles().len(), 13);
        let shown = TILE_ROWS.shown;
        assert!(shown < 13, "the row scrolls");
        // A still pointer on the last tile shown focuses it, and the row stays put.
        let last = slot(&mut s, shown - 1);
        for t in 0..4 {
            s.point(last.0, last.1, &d, 10 + t);
            s.layout(&Mono);
        }
        assert_eq!(s.overlay(), Overlay::Info { tile: shown - 1 });
        assert_eq!(s.tiles_top, 0);
        // The keys to the end of the row, then the pointer on the first tile shown: that tile,
        // not the TV guide scrolled under the pointer.
        for t in 0..13 {
            s.key(Key::Right, Press::Short, &d, 100 + t);
        }
        assert_eq!(s.overlay(), Overlay::Info { tile: 12 });
        let top = s.tiles_top;
        assert_eq!(top, 13 - shown);
        let first = slot(&mut s, 0);
        s.point(first.0, first.1, &d, 200);
        assert_eq!(
            (s.overlay(), s.tiles_top),
            (Overlay::Info { tile: top }, top)
        );
        s.layout(&Mono);
        s.point(first.0, first.1, &d, 210);
        assert_eq!(s.overlay(), Overlay::Info { tile: top });
        // From the pointed tile the keys scroll as they do, keeping a tile of context.
        s.key(Key::Left, Press::Short, &d, 220);
        assert_eq!(
            (s.overlay(), s.tiles_top),
            (Overlay::Info { tile: top - 1 }, top - 2)
        );
        let out = s.key(Key::Ok, Press::Short, &d, 230);
        assert!(out.navigate.is_empty(), "not the TV guide: {out:?}");
        assert_eq!(out.commands.len(), 1, "the channel on that tile plays");
    }
}
