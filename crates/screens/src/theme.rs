//! The look of every screen in one place: palette, spacing, corner radii and type scale for the
//! 1920x1080 graphics plane.
//!
//! Values were measured from TiviMate 5 at 1080p. A screen names a role from here and never a
//! literal; colours are `[f32; 4]` RGBA and sizes are the `c_int` pixel sizes `Painter::text`
//! takes, with [`REGULAR`] or [`BOLD`] for its `bold` argument. The bundled face is Inter.

use std::os::raw::c_int;
use tv_gfx::gfx::tokens::{rgb8, with_a};

// Surfaces, opaque ones darkest first.

/// The app ground: behind the guide, the groups list and everything that is not video.
pub const BG: [f32; 4] = rgb8(0x16, 0x15, 0x1b);
/// A programme cell in the guide.
pub const CELL: [f32; 4] = rgb8(0x1c, 0x1d, 0x22);
/// Side panels, the channel list overlay, info cards and the groups rail.
pub const PANEL: [f32; 4] = rgb8(0x24, 0x24, 0x2b);
/// A strip raised on a panel, such as the group name above the channel list.
pub const PANEL_RAISED: [f32; 4] = rgb8(0x36, 0x35, 0x3d);
/// The other cells in the focused channel's guide row.
pub const CELL_ROW: [f32; 4] = rgb8(0x45, 0x46, 0x4a);
/// The focus pill: a focused cell, list row or menu item. Text on it uses [`TEXT_ON_FOCUS`].
pub const FOCUS: [f32; 4] = rgb8(0xe0, 0xdf, 0xe4);
/// Behind the player's panels, over video.
pub const SCRIM: [f32; 4] = with_a(BG, 0.9);

// Text.

pub const TEXT: [f32; 4] = rgb8(0xff, 0xff, 0xff);
/// Times, descriptions, unfocused programme titles.
pub const TEXT_MUTED: [f32; 4] = rgb8(0xad, 0xad, 0xb3);
pub const TEXT_ON_FOCUS: [f32; 4] = rgb8(0x05, 0x05, 0x09);
pub const TEXT_ON_FOCUS_MUTED: [f32; 4] = rgb8(0x33, 0x32, 0x37);

// Accent.

/// The playing channel's number, name and play mark, and the catch-up seek bar.
pub const ACCENT: [f32; 4] = rgb8(0x30, 0x99, 0xf0);
/// The accent as text on the dark ground: the clock, the programme on now in lists, the guide's
/// now dot.
pub const ACCENT_TEXT: [f32; 4] = rgb8(0xae, 0xd0, 0xf8);

// Bars and badges.

/// Elapsed part of a programme's progress bar.
pub const PROGRESS: [f32; 4] = rgb8(0xbf, 0xc0, 0xc5);
pub const PROGRESS_TRACK: [f32; 4] = rgb8(0x47, 0x48, 0x4d);
/// Unplayed part of the catch-up seek bar, over video.
pub const SEEK_TRACK: [f32; 4] = with_a(TEXT, 0.65);
/// The guide's vertical line at the current time.
pub const NOW_LINE: [f32; 4] = rgb8(0x43, 0x43, 0x4b);
/// Hairlines: under the guide's time ruler, between list sections.
pub const DIVIDER: [f32; 4] = rgb8(0x30, 0x30, 0x36);
/// Stream fact chips (HD, 50 FPS, STEREO), with [`TEXT_ON_FOCUS`] and [`size::CAPTION`] bold.
pub const CHIP: [f32; 4] = rgb8(0x99, 0x99, 0x9e);

/// Spacing ladder in pixels.
pub mod space {
    /// Between guide cells and rows.
    pub const XS: f32 = 4.0;
    pub const S: f32 = 8.0;
    /// Inside rows and cards.
    pub const M: f32 = 16.0;
    /// Top screen margin.
    pub const L: f32 = 24.0;
    /// Side screen margin. TiviMate draws inside the 5% overscan frame; LG panels show apps 1:1.
    pub const XL: f32 = 32.0;
    /// Between the guide's preview and its details.
    pub const XXL: f32 = 48.0;
}

/// Corner radii in pixels. A progress bar is fully rounded: half its height.
pub mod radius {
    /// Programme cells and chips.
    pub const CELL: f32 = 4.0;
    /// Focus pills on list rows, cards, menu tiles.
    pub const PILL: f32 = 6.0;
}

/// Type scale in pixels for Inter.
pub mod size {
    use std::os::raw::c_int;
    /// Screen headers (History, Settings).
    pub const HEADER: c_int = 56;
    /// Programme title in the guide details and the player's info panel.
    pub const TITLE: c_int = 42;
    /// Card and tile titles.
    pub const HEADING: c_int = 36;
    /// List rows: channels, groups, programme lists, menus.
    pub const BODY: c_int = 30;
    /// Guide rows and cells, the time ruler, times and descriptions.
    pub const LABEL: c_int = 27;
    /// Chips and superscripts.
    pub const CAPTION: c_int = 20;
}

/// `bold` argument of `Painter::text`.
pub const REGULAR: c_int = 0;
pub const BOLD: c_int = 1;

/// Guide grid geometry.
pub mod guide {
    /// Row pitch; a cell is `ROW_H - space::XS` tall.
    pub const ROW_H: f32 = 78.0;
    /// Width of one hour on the time axis.
    pub const HOUR_W: f32 = 640.0;
    /// Left edge of the programme cells; the channel column (number, logo, name, catch-up mark)
    /// fills the space before it.
    pub const GRID_X: f32 = 543.0;
    /// Top of the first row, under the time ruler.
    pub const GRID_Y: f32 = 466.0;
    /// The 16:9 preview in the top-left corner.
    pub const PREVIEW_W: f32 = 640.0;
    pub const PREVIEW_H: f32 = 360.0;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG contrast ratio of two opaque colours.
    fn contrast(a: [f32; 4], b: [f32; 4]) -> f32 {
        let lum = |c: [f32; 4]| {
            let lin = |v: f32| {
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
        };
        let (hi, lo) = (lum(a).max(lum(b)), lum(a).min(lum(b)));
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn text_reads_on_every_surface_it_is_drawn_on() {
        for (fg, bg, floor) in [
            (TEXT, BG, 7.0),
            (TEXT, CELL_ROW, 7.0),
            (TEXT_ON_FOCUS, FOCUS, 7.0),
            (TEXT_ON_FOCUS, CHIP, 4.5),
            (TEXT_MUTED, BG, 4.5),
            (TEXT_MUTED, PANEL, 4.5),
            (TEXT_ON_FOCUS_MUTED, FOCUS, 4.5),
            (ACCENT_TEXT, BG, 4.5),
            (ACCENT, BG, 4.5),
        ] {
            let c = contrast(fg, bg);
            assert!(c >= floor, "{fg:?} on {bg:?}: {c:.2} < {floor}");
        }
    }

    #[test]
    fn surfaces_step_up_in_lightness() {
        let steps = [BG, CELL, PANEL, PANEL_RAISED, CELL_ROW, FOCUS];
        assert!(steps
            .windows(2)
            .all(|w| contrast(w[1], BG) > contrast(w[0], BG)));
    }
}
