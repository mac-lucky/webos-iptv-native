//! The lists over the player: the channel list (overlay and preview mode, with the programme
//! list and the days column) and History.
//!
//! Each is a [`Screen`]: pure key handling that loads from [`TvData`] only what is on screen,
//! and a view laid out once per change.
//!
//! [`Screen`]: crate::data::Screen
//! [`TvData`]: crate::data::TvData

pub mod channels;
mod cursor;
pub mod history;
mod view;

pub use channels::{ChannelList, Column, Mode, PREVIEW_RECT};
pub use cursor::{Cursor, Rows};
pub use history::History;

use iptv::model::Timestamp;

use crate::input::{Hold, Key, Press};
use crate::time;

/// What one key does in a list; both lists read the remote the same way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Nav {
    Up,
    Down,
    /// CH+ / CH-: a page at a time.
    PageUp,
    PageDown,
    /// Long Left: to the top of the focused list.
    Top,
    Left,
    Right,
    Ok,
    Back,
}

impl Nav {
    pub(crate) fn of(key: Key, press: Press) -> Option<Nav> {
        Some(match (key, press) {
            (Key::Up, _) => Nav::Up,
            (Key::Down, _) => Nav::Down,
            (Key::ChannelUp, _) => Nav::PageUp,
            (Key::ChannelDown, _) => Nav::PageDown,
            (Key::Left, Press::Long) => Nav::Top,
            (Key::Left, Press::Short) => Nav::Left,
            (Key::Right, Press::Short) => Nav::Right,
            (Key::Ok, Press::Short) => Nav::Ok,
            (Key::Back, Press::Short) => Nav::Back,
            _ => return None,
        })
    }
}

/// How the lists want `key` held: the arrows and channel keys repeat, Left tells a long press
/// (to the top) from a short one.
fn hold(key: Key) -> Hold {
    match key {
        Key::Up | Key::Down | Key::ChannelUp | Key::ChannelDown => Hold::Repeat,
        Key::Left => Hold::Long,
        _ => Hold::Once,
    }
}

/// A list's clock: the time it shows, and the shell's millisecond clock when it read it, so its
/// deadlines land on the shell's clock without drifting however often they are asked for.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Clock {
    pub now: Timestamp,
    ms: Option<u64>,
}

impl Clock {
    pub fn new(now: Timestamp) -> Self {
        Clock { now, ms: None }
    }

    /// The time is `now` when the shell's clock reads `now_ms`.
    pub fn set(&mut self, now: Timestamp, now_ms: u64) {
        *self = Clock {
            now,
            ms: Some(now_ms),
        };
    }

    /// The moment `at` on the shell's clock. Until the list has read that clock once (a key or a
    /// tick), right away: the tick that follows reads it.
    pub fn wake(&self, now_ms: u64, at: Timestamp) -> u64 {
        self.ms.map_or(now_ms, |ms| time::wake_ms(ms, self.now, at))
    }
}

/// The key and press that make `nav`.
#[cfg(test)]
pub(crate) fn key_of(nav: Nav) -> (Key, Press) {
    match nav {
        Nav::Up => (Key::Up, Press::Short),
        Nav::Down => (Key::Down, Press::Short),
        Nav::PageUp => (Key::ChannelUp, Press::Short),
        Nav::PageDown => (Key::ChannelDown, Press::Short),
        Nav::Top => (Key::Left, Press::Long),
        Nav::Left => (Key::Left, Press::Short),
        Nav::Right => (Key::Right, Press::Short),
        Nav::Ok => (Key::Ok, Press::Short),
        Nav::Back => (Key::Back, Press::Short),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_keys_map_to_list_actions() {
        assert_eq!(Nav::of(Key::Up, Press::Repeat), Some(Nav::Up));
        assert_eq!(Nav::of(Key::ChannelDown, Press::Short), Some(Nav::PageDown));
        assert_eq!(Nav::of(Key::Left, Press::Long), Some(Nav::Top));
        assert_eq!(Nav::of(Key::Left, Press::Short), Some(Nav::Left));
        assert_eq!(Nav::of(Key::Ok, Press::Long), None);
        assert_eq!(Nav::of(Key::Red, Press::Short), None);
        assert_eq!(hold(Key::Down), Hold::Repeat);
        assert_eq!(hold(Key::Left), Hold::Long);
        assert_eq!(hold(Key::Ok), Hold::Once);
        for nav in [
            Nav::Up,
            Nav::Down,
            Nav::PageUp,
            Nav::PageDown,
            Nav::Top,
            Nav::Left,
            Nav::Right,
            Nav::Ok,
            Nav::Back,
        ] {
            let (key, press) = key_of(nav);
            assert_eq!(Nav::of(key, press), Some(nav));
        }
    }

    #[test]
    fn the_clock_wakes_on_the_shell_s_clock() {
        let mut c = Clock::new(1_000);
        assert_eq!(
            c.wake(70, 1_060),
            70,
            "a tick first, to read the shell's clock"
        );
        c.set(1_000, 5_000);
        // Asked again later, the deadline stays put.
        assert_eq!(c.wake(5_000, 1_060), 65_000);
        assert_eq!(c.wake(9_000, 1_060), 65_000);
        assert_eq!(c.wake(9_000, 990), 6_000);
    }
}
