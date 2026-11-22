//! The home navigation (Settings > Appearance > Navigation: `home`). The app opens on the home
//! page ([`HomeScreen`]), a page of section tiles; each section opens over it, and Back leaves a
//! section for it once nothing is left to unwind inside the section (a series, a column, the
//! guide's live position).
//!
//! Live TV plays only where it shows: in the guide's and the channel list's preview, and full
//! screen. Back to the home page stops it. A channel that goes full screen from a section takes
//! the home page away with the section, and Back on the player opens that section again, the
//! channel still playing in its preview.

use std::any::Any;

use tv_screens::data::{Navigate, Output, Screen};
use tv_screens::home::HomeScreen;
use tv_screens::panels::keys;

use super::{Layer, Shell};

/// The section Back on the full-screen player opens when no section put the channel there
/// (Search, History, a reminder).
const PLAYER_BACK: Navigate = Navigate::ChannelList { preview: true };

/// What the home navigation remembers between screens.
#[derive(Default)]
pub(super) struct Home {
    /// The section the channel playing full screen came from.
    back_to: Option<Navigate>,
}

/// `screen` is the home page.
pub(super) fn is_home(screen: &dyn Screen) -> bool {
    let any: &dyn Any = screen;
    any.is::<HomeScreen>()
}

/// A section the full-screen player can go back to: one with a preview of the channel.
fn shows_live(nav: &Navigate) -> bool {
    matches!(nav, Navigate::Guide | Navigate::ChannelList { .. })
}

/// What a section answered Back with: when it leaves the section (closes it, or hands over to
/// the player, the guide or the main menu), the section closes and nothing else of the answer
/// is carried out. The sections build such an answer without changing their state.
pub(super) fn leave_section(out: Output) -> Output {
    let leaves = out.navigate.iter().any(|nav| {
        matches!(
            nav,
            Navigate::Close | Navigate::Player | Navigate::Guide | Navigate::MainMenu
        )
    });
    if leaves {
        Output::close()
    } else {
        out
    }
}

impl Shell {
    pub(super) fn home_mode(&self) -> bool {
        self.settings.get(keys::NAVIGATION) == "home"
    }

    /// The home page in front.
    fn home_in_front(&self) -> bool {
        self.top().is_some_and(|i| is_home(&*self.stack[i].screen))
    }

    /// A live channel, an archive programme or a movie plays.
    fn playing(&self) -> bool {
        let pb = self.store.playback();
        pb.channel.is_some() || pb.vod.is_some()
    }

    /// The home page alone over the player, nothing playing.
    pub(super) fn go_home(&mut self) {
        if self.playing() {
            self.stop_playing();
        }
        self.clear();
        self.stack
            .push(Layer::new(Box::new(HomeScreen::new(&*self.store))));
    }

    /// `screen`, a section, over the home page alone.
    pub(super) fn open_section(&mut self, screen: Box<dyn Screen>) {
        self.clear();
        // Back from the section finds the home page on its tile.
        let mut home = HomeScreen::new(&*self.store);
        if let Some(section) = screen.navigation_target() {
            home.focus_on(&section);
        }
        self.stack.push(Layer::new(Box::new(home)));
        self.stack.push(Layer::new(screen));
    }

    /// The section at `at` has nothing left to unwind: Back leaves it at once.
    pub(super) fn section_root(&self, at: Option<usize>) -> bool {
        at.is_some_and(|i| self.stack[i].screen.back_opens_navigation())
    }

    /// The section the page at `at` is, if it is one.
    pub(super) fn section(&self, at: Option<usize>) -> Option<Navigate> {
        at.and_then(|i| self.stack[i].screen.navigation_target())
    }

    /// Back on the full-screen player: the guide it asks for becomes the section the channel
    /// came from.
    pub(super) fn player_back(&self, out: &mut Output) {
        let back_to = self.home.back_to.clone().unwrap_or(PLAYER_BACK);
        for nav in &mut out.navigate {
            if *nav == Navigate::Guide {
                *nav = back_to.clone();
            }
        }
    }

    /// After an action. A Back that brought the home page to the front stops what plays. Any
    /// other way there while something plays (OK on a channel closed the guide, the channel
    /// list or Search over it) puts it full screen instead, and Back on the player will open
    /// `section` again. And the player never stands alone with nothing playing: the home page
    /// comes back.
    pub(super) fn after_home_action(&mut self, back: bool, section: Option<Navigate>) {
        if self.home_in_front() && self.playing() {
            if back {
                self.stop_playing();
            } else {
                self.home.back_to = section.filter(shows_live);
                self.clear();
            }
        } else if self.top().is_none() && !self.playing() {
            self.go_home();
        }
    }

    /// The navigation setting changed: the home page goes under every screen open, or every home
    /// page goes away (the classic navigation opens the guide where nothing would show).
    pub(super) fn follow_navigation(&mut self) {
        let has_home = self.stack.iter().any(|l| is_home(&*l.screen));
        if self.home_mode() && !has_home {
            self.stack
                .insert(0, Layer::new(Box::new(HomeScreen::new(&*self.store))));
        } else if !self.home_mode() && has_home {
            self.stack.retain(|l| !is_home(&*l.screen));
            if self.top().is_none() && !self.playing() {
                self.navigate(Navigate::Guide, None);
            }
        }
        self.home.back_to = None;
    }
}
