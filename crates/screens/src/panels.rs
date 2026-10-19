//! Panels: the long-OK context menu and Settings, both sliding in from the right on the same
//! [`SidePanel`], and the Search screen with the TV keyboard.
//!
//! Each is a [`Screen`](crate::data::Screen) with pure key handling: a key goes in, an
//! [`Output`](crate::data::Output) comes out, and the shell carries it out (an app command, closing the panel or
//! opening Settings, a toast). A panel with a text field says so through `editing()`, and the
//! shell raises the TV keyboard while it does. Nothing here touches storage, the network or the
//! player.
//!
//! Opening a context menu from the shell:
//!
//! ```ignore
//! let cx = PanelCtx { data: &data, settings: &settings };
//! let menu: Box<dyn Screen> = Box::new(ContextMenu::open(&nav, &cx)?);
//! ```

mod field;
pub mod menu;
pub mod search;
pub mod settings;
mod side;

pub use field::TextField;
pub use menu::{ContextMenu, MenuItem};
pub use search::{Hit, Search};
pub use settings::{keys, Settings, SettingsPanel};
pub use side::{Edge, Nav, Page, Row, SidePanel};

use crate::data::TvData;
use crate::input::{Action, Hold, Key, Press};

/// What a panel reads when it opens.
pub struct PanelCtx<'a> {
    pub data: &'a dyn TvData,
    pub settings: &'a Settings,
}

/// The keys the panels react to: remote actions through [`PanelKey::from_action`], plus
/// `Backspace` and `Clear`, which the TV keyboard sends as SDL key events while it is up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelKey {
    Up,
    Down,
    Left,
    Right,
    Ok,
    Back,
    Backspace,
    Clear,
}

impl PanelKey {
    /// The panel key for a remote action. Panels act on short presses and list repeats; a long
    /// press means nothing on a panel.
    pub fn from_action(action: Action) -> Option<Self> {
        if action.press == Press::Long {
            return None;
        }
        Some(match action.key {
            Key::Up => PanelKey::Up,
            Key::Down => PanelKey::Down,
            Key::Left => PanelKey::Left,
            Key::Right => PanelKey::Right,
            Key::Ok => PanelKey::Ok,
            Key::Back => PanelKey::Back,
            Key::Backspace => PanelKey::Backspace,
            Key::Clear => PanelKey::Clear,
            _ => return None,
        })
    }

    /// How a key behaves while held on a panel: Up and Down repeat through lists, Backspace
    /// through the text.
    pub fn hold(key: Key) -> Hold {
        match key {
            Key::Up | Key::Down | Key::Backspace => Hold::Repeat,
            _ => Hold::Once,
        }
    }
}

/// Panel geometry on the 1920x1080 plane, and the theme roles the panels draw with.
pub(crate) mod style {
    use crate::theme::{self, radius, size, space};
    use std::os::raw::c_int;

    pub const SCREEN_W: f32 = 1920.0;
    pub const SCREEN_H: f32 = 1080.0;
    pub const PANEL_W: f32 = 640.0;
    pub const PAD_X: f32 = space::XL;
    pub const TITLE_Y: f32 = space::XL;
    pub const TITLE_H: f32 = 72.0;
    pub const TITLE_SZ: c_int = size::HEADING;
    pub const ROWS_TOP: f32 = 128.0;
    pub const ROW_H: f32 = 76.0;
    pub const ROW_SZ: c_int = size::BODY;
    pub const VALUE_SZ: c_int = size::LABEL;
    pub const BODY_SZ: c_int = size::LABEL;
    pub const ICON: f32 = 28.0;
    pub const GAP: f32 = space::M;
    pub const FIELD_PAD: f32 = space::L;
    pub const PLATE_INSET: f32 = space::M;
    pub const PLATE_RADIUS: f32 = radius::PILL;
    /// Rows that fit between the title and the bottom margin.
    pub const VISIBLE_ROWS: usize = 11;
    pub const SLIDE_K: f32 = 260.0;
    /// Below this slide position the panel is not drawn.
    pub const HIDDEN: f32 = 0.001;
    pub const SHADOW_BLUR: f32 = 48.0;
    pub const SHADOW: [f32; 4] = [0.0, 0.0, 0.0, 0.5];
    pub const PANEL_BG: [f32; 4] = theme::PANEL;
    pub const SCREEN_BG: [f32; 4] = theme::BG;
    pub const TEXT_PRIMARY: [f32; 4] = theme::TEXT;
    pub const TEXT_SECONDARY: [f32; 4] = theme::TEXT_MUTED;
    pub const FOCUS_FILL: [f32; 4] = theme::FOCUS;
    pub const FOCUS_INK: [f32; 4] = theme::TEXT_ON_FOCUS;
    pub const FOCUS_INK_SECONDARY: [f32; 4] = theme::TEXT_ON_FOCUS_MUTED;
    pub const FIELD_FILL: [f32; 4] = theme::PANEL_RAISED;
    pub const FIELD_RIM: [f32; 4] = theme::CELL_ROW;
    /// The rim of a field being typed into or focused.
    pub const FIELD_RIM_HOT: [f32; 4] = theme::ACCENT;
}

/// Driving a panel the way the shell does.
#[cfg(test)]
pub(crate) mod testing {
    use iptv::model::Channel;
    use tv_ui::Painter;

    use crate::data::fake::Mono;
    use crate::data::{Logos, Output, Screen, TvData};
    use crate::input::{Key, Press};

    /// Short presses of `keys` on `screen`, chained, without the redraw flag: what the shell is
    /// asked to do.
    pub fn press(screen: &mut dyn Screen, data: &dyn TvData, keys: &[Key]) -> Output {
        let out = keys.iter().fold(Output::default(), |out, &key| {
            out.and(screen.key(key, Press::Short, data, 0))
        });
        Output {
            redraw: false,
            ..out
        }
    }

    struct NoLogos;

    impl Logos for NoLogos {
        fn logo(&self, _: &Channel) -> Option<(u32, f32, f32)> {
            None
        }
    }

    /// One frame of `screen`, on a recording painter.
    pub fn draw(screen: &mut dyn Screen) {
        screen.draw(Painter::recording(), &Mono, &NoLogos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_actions_map_to_panel_keys() {
        let short = |key| Action {
            key,
            press: Press::Short,
        };
        assert_eq!(PanelKey::from_action(short(Key::Ok)), Some(PanelKey::Ok));
        let repeat = Action {
            key: Key::Down,
            press: Press::Repeat,
        };
        assert_eq!(PanelKey::from_action(repeat), Some(PanelKey::Down));
        let long = Action {
            key: Key::Back,
            press: Press::Long,
        };
        assert_eq!(PanelKey::from_action(long), None);
        assert_eq!(PanelKey::from_action(short(Key::Red)), None);
        assert_eq!(
            PanelKey::from_action(short(Key::Backspace)),
            Some(PanelKey::Backspace)
        );
        assert_eq!(PanelKey::hold(Key::Backspace), Hold::Repeat);
        assert_eq!(PanelKey::hold(Key::Up), Hold::Repeat);
        assert_eq!(PanelKey::hold(Key::Ok), Hold::Once);
    }
}
