//! The look of every screen in one place: palette, spacing, corner radii and type scale for the
//! 1920x1080 graphics plane.
//!
//! Values were measured from TiviMate 5 at 1080p. A screen names a role from here and never a
//! literal; colours are `[f32; 4]` RGBA and sizes are the `c_int` pixel sizes `Painter::text`
//! takes, with [`REGULAR`] or [`BOLD`] for its `bold` argument. The bundled face is Inter.

use std::cell::Cell;
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
///
/// The value is [`Accent::Blue`]'s. A screen names the role and [`ink`] paints it in the accent
/// picked in Settings, so a display list built before a change paints the new accent.
pub const ACCENT: [f32; 4] = rgb8(0x30, 0x99, 0xf0);
/// The accent as text on the dark ground: the clock, the programme on now in lists, the guide's
/// now dot. Painted through [`ink`] like [`ACCENT`].
pub const ACCENT_TEXT: [f32; 4] = rgb8(0xae, 0xd0, 0xf8);

/// The accent colours Settings offers, as TiviMate's tones; blue by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Accent {
    #[default]
    Blue,
    Green,
    Red,
    Orange,
    Purple,
    Teal,
}

impl Accent {
    pub const ALL: [Accent; 6] = [
        Accent::Blue,
        Accent::Green,
        Accent::Red,
        Accent::Orange,
        Accent::Purple,
        Accent::Teal,
    ];

    /// The value the setting stores.
    pub const fn key(self) -> &'static str {
        match self {
            Accent::Blue => "blue",
            Accent::Green => "green",
            Accent::Red => "red",
            Accent::Orange => "orange",
            Accent::Purple => "purple",
            Accent::Teal => "teal",
        }
    }

    /// The accent stored as `key`; `None` for anything else.
    pub fn from_key(key: &str) -> Option<Accent> {
        Self::ALL.into_iter().find(|a| a.key() == key)
    }

    /// The fill ([`ACCENT`]'s role) and the text tone on the dark ground ([`ACCENT_TEXT`]'s).
    pub const fn colors(self) -> ([f32; 4], [f32; 4]) {
        match self {
            Accent::Blue => (ACCENT, ACCENT_TEXT),
            Accent::Green => (rgb8(0x3f, 0xb7, 0x6a), rgb8(0xb2, 0xe3, 0xc2)),
            Accent::Red => (rgb8(0xef, 0x53, 0x50), rgb8(0xf8, 0xb9, 0xb7)),
            Accent::Orange => (rgb8(0xf2, 0x89, 0x2e), rgb8(0xf9, 0xcf, 0xa6)),
            Accent::Purple => (rgb8(0xa4, 0x77, 0xf2), rgb8(0xd6, 0xc5, 0xf9)),
            Accent::Teal => (rgb8(0x22, 0xaa, 0xa0), rgb8(0xa6, 0xde, 0xd9)),
        }
    }
}

thread_local! {
    /// The accent picked in Settings. Per thread: the shell and the painter share the main
    /// thread, and each host test picks its own.
    static PICKED: Cell<Accent> = const { Cell::new(Accent::Blue) };
}

/// Paint the accent roles in `accent` from the next frame on (the shell, from the setting).
pub fn set_accent(accent: Accent) {
    PICKED.with(|p| p.set(accent));
}

/// The accent the roles are painted in.
pub fn accent() -> Accent {
    PICKED.with(Cell::get)
}

/// `c` as painted: [`ACCENT`] and [`ACCENT_TEXT`] in the picked accent, keeping `c`'s alpha;
/// every other colour as it is. The display-list and icon painters call it, so screens name
/// the roles and never resolve them; a direct `Painter` call with an accent role wraps it.
pub fn ink(c: [f32; 4]) -> [f32; 4] {
    let picked = accent();
    if picked == Accent::Blue {
        return c;
    }
    let (fill, text) = picked.colors();
    let rgb = |x: [f32; 4]| [x[0], x[1], x[2]];
    if rgb(c) == rgb(ACCENT) {
        with_a(fill, c[3])
    } else if rgb(c) == rgb(ACCENT_TEXT) {
        with_a(text, c[3])
    } else {
        c
    }
}

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
    fn every_accent_reads_on_the_ground_and_the_panels() {
        for accent in Accent::ALL {
            let (fill, text) = accent.colors();
            for (fg, bg, floor) in [(fill, BG, 4.5), (text, BG, 7.0), (text, PANEL, 4.5)] {
                let c = contrast(fg, bg);
                assert!(c >= floor, "{accent:?} {fg:?} on {bg:?}: {c:.2} < {floor}");
            }
        }
    }

    #[test]
    fn accents_round_trip_through_their_stored_keys() {
        for accent in Accent::ALL {
            assert_eq!(Accent::from_key(accent.key()), Some(accent));
        }
        assert_eq!(Accent::from_key("pink"), None);
        assert_eq!(Accent::default(), Accent::Blue);
    }

    #[test]
    fn ink_paints_the_accent_roles_in_the_picked_accent() {
        let muted = with_a(ACCENT, 0.5);
        assert_eq!(accent(), Accent::Blue, "a thread starts on blue");
        assert_eq!(ink(ACCENT), ACCENT);
        set_accent(Accent::Green);
        let (fill, text) = Accent::Green.colors();
        assert_eq!(ink(ACCENT), fill);
        assert_eq!(ink(ACCENT_TEXT), text);
        assert_eq!(ink(muted), with_a(fill, 0.5), "alpha kept");
        for other in [TEXT, BG, FOCUS, PROGRESS, SEEK_TRACK] {
            assert_eq!(ink(other), other);
        }
        set_accent(Accent::Blue);
        assert_eq!(ink(ACCENT_TEXT), ACCENT_TEXT);
    }

    #[test]
    fn a_display_list_built_before_a_change_paints_the_new_accent() {
        use crate::data::fake::Mono;
        use crate::data::Group;
        use crate::draw::Piece;
        use crate::groups::{Frame, GroupsPanel};
        let frame = Frame {
            w: 615.0,
            pill_x: 144.0,
        };
        let groups = vec![Group::Favorites, Group::All, Group::History];
        let mut panel = GroupsPanel::new(groups, &Group::All, frame);
        panel.key(crate::input::Key::Down, false);
        panel.prepare(&Mono);
        let painted = || -> Vec<(String, [f32; 4])> {
            let texts = panel.pieces().iter().filter_map(|p| match p {
                Piece::Text { s, color, .. } => Some((s.to_string_lossy().into(), ink(*color))),
                _ => None,
            });
            texts.collect()
        };
        let shown = |t: &[(String, [f32; 4])]| t.iter().find(|t| t.0 == "All channels").unwrap().1;
        assert_eq!(shown(&painted()), ACCENT_TEXT, "built and painted in blue");
        set_accent(Accent::Purple);
        let after = painted();
        assert_eq!(shown(&after), Accent::Purple.colors().1, "the same list");
        assert!(after.iter().any(|t| t.1 == TEXT), "the rest as it was");
        set_accent(Accent::Blue);
    }

    #[test]
    fn surfaces_step_up_in_lightness() {
        let steps = [BG, CELL, PANEL, PANEL_RAISED, CELL_ROW, FOCUS];
        assert!(steps
            .windows(2)
            .all(|w| contrast(w[1], BG) > contrast(w[0], BG)));
    }
}
