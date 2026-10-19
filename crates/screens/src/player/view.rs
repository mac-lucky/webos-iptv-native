//! Drawing the player panels: [`build`] lays them out into a display list when the screen's
//! [`generation`](super::PlayerScreen::generation) moves, measuring and fitting text then, never
//! while painting.
//!
//! Geometry was measured from TiviMate at 1080p; colours, type and spacing come from
//! [`crate::theme`].

use super::model::{Header, TileKind};
use super::transport::{Button, Focus, BUTTONS};
use super::{Overlay, PlayerScreen};
use crate::draw::{self, HAlign, Piece, Run, VAlign};
use crate::icons::Icon;
use crate::theme::{self, radius, size, space};
use std::os::raw::c_int;
use tv_gfx::gfx::tokens::with_a;
use tv_machine::machine::Measure;
use tv_ui::consts::{SCR_H, SCR_W};
use tv_ui::Rect;

/// Panel geometry at 1080p.
mod geom {
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
    pub const CARD_W: f32 = 720.0;
    pub const CARD_PAD: f32 = 32.0;
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

/// Lay the panels of `s` out into `out`.
pub fn build(s: &PlayerScreen, m: &dyn Measure, out: &mut Vec<Piece>) {
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
        Overlay::Card { .. } => card(out, s, m),
    }
    if let Some(n) = &s.number {
        number(out, &n.digits, n.preview.as_deref(), m);
    }
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

/// The info panel's tile row; `focus` is the focused tile.
fn tiles(out: &mut Vec<Piece>, s: &PlayerScreen, focus: usize, m: &dyn Measure) {
    let tiles = &s.tiles;
    let fits = ((SCR_W - geom::X - geom::TILE_W) / geom::TILE_PITCH) as usize + 1;
    let first = (focus + 2)
        .saturating_sub(fits)
        .min(tiles.len().saturating_sub(fits));
    for (slot, (i, t)) in tiles.iter().enumerate().skip(first).take(fits).enumerate() {
        let focused = focus == i;
        let base = Rect::new(
            geom::X + slot as f32 * geom::TILE_PITCH,
            geom::TILES_Y,
            geom::TILE_W,
            geom::TILE_H,
        );
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
            let d = geom::BUTTON_DISC;
            let disc = Rect::new(cx - d * 0.5, geom::TRANSPORT_Y - d * 0.5, d, d);
            out.push(Piece::solid(disc, d * 0.5, theme::FOCUS));
        }
        let i = geom::BUTTON_ICON;
        out.push(Piece::Icon {
            icon,
            r: Rect::new(cx - i * 0.5, geom::TRANSPORT_Y - i * 0.5, i, i),
            tint,
        });
    }
}

/// The programme card, top right.
fn card(out: &mut Vec<Piece>, s: &PlayerScreen, m: &dyn Measure) {
    let c = &s.card;
    let x = SCR_W - geom::X - geom::CARD_W;
    let inner = geom::CARD_W - 2.0 * geom::CARD_PAD;
    let tx = x + geom::CARD_PAD;
    let top = geom::X;
    let at = out.len();
    let mut y = top + geom::CARD_PAD;
    let muted = |out: &mut Vec<Piece>, s: Run, y: f32| {
        out.push(Piece::text(s, tx, y, size::LABEL, false, theme::TEXT_MUTED));
    };

    muted(
        out,
        draw::fit(m, &c.channel, inner, size::LABEL, false).0,
        y,
    );
    y += m.line_h(size::LABEL) + space::S;
    for line in draw::wrap(m, &c.title, inner, size::HEADING, true, 2) {
        out.push(Piece::text(line, tx, y, size::HEADING, true, theme::TEXT));
        y += m.line_h(size::HEADING);
    }
    y += space::S;
    let times = format!("{}   {}", c.times, c.duration);
    muted(out, draw::fit(m, &times, inner, size::LABEL, false).0, y);
    y += m.line_h(size::LABEL) + space::S;
    if let Some(f) = c.progress {
        let r = Rect::new(tx, y, 160.0, geom::BAR_H);
        out.extend(draw::bar(r, f, theme::PROGRESS, theme::PROGRESS_TRACK));
        y += geom::BAR_H + space::M;
    }
    if let Some(cat) = &c.category {
        let mut row = Row {
            m,
            out,
            x: tx,
            y,
            right: tx + inner,
        };
        row.chip(cat);
        y += m.line_h(size::LABEL) + space::S;
    }
    if let Some(d) = &c.description {
        y += space::S;
        for line in draw::wrap(m, d, inner, size::LABEL, false, 6) {
            muted(out, line, y);
            y += geom::DESC_LINE;
        }
    }
    let h = y - top + geom::CARD_PAD;
    let panel = Rect::new(x, top, geom::CARD_W, h);
    out.insert(
        at,
        Piece::solid(panel, radius::PILL, with_a(theme::PANEL, 0.95)),
    );
}

/// The channel number being typed, top right.
fn number(out: &mut Vec<Piece>, digits: &str, preview: Option<&str>, m: &dyn Measure) {
    let right = SCR_W - geom::X;
    let top = geom::X;
    let digits_w = m.width_str(digits, size::HEADER, true);
    let preview = preview.map(|p| draw::fit(m, p, 600.0, size::BODY, false));
    let preview_w = preview.as_ref().map_or(0.0, |(_, w)| *w);
    let w = digits_w.max(preview_w) + 2.0 * space::L;
    let mut h = m.line_h(size::HEADER) + 2.0 * space::M;
    if preview.is_some() {
        h += m.line_h(size::BODY);
    }
    let panel = Rect::new(right - w, top, w, h);
    out.push(Piece::solid(
        panel,
        radius::PILL,
        with_a(theme::PANEL, 0.95),
    ));
    out.push(text(
        draw::run(digits),
        right - space::L,
        top + space::M,
        size::HEADER,
        true,
        theme::TEXT,
        HAlign::Right,
    ));
    if let Some((p, _)) = preview {
        out.push(text(
            p,
            right - space::L,
            top + space::M + m.line_h(size::HEADER),
            size::BODY,
            false,
            theme::TEXT_MUTED,
            HAlign::Right,
        ));
    }
}
