//! The app shell: the screen stack, key routing, navigation and the commands the screens ask for.
//!
//! The full-screen [`PlayerScreen`] is always at the bottom; the guide, the channel list,
//! History and the panels open over it, and only the topmost one still open gets keys and text.
//! Remote edges go through [`Remote`] with that screen's own [`Hold`](tv_screens::input::Hold)
//! for each key, so short, long and repeated presses are the screens' decision. What a screen
//! returns runs in order: its commands, its message for the screen under it, its navigation. A
//! closed screen that still animates out stays in the stack, drawn but deaf, until it is gone.
//! Every [`Command`] ends here: storage writes go to the [`Store`], playback to the [`Player`].
//!
//! After every change the rest follows from the screens: the video is where the topmost screen
//! that places it wants it (full screen when none does), and the TV keyboard is up while the
//! screen in front edits a text field.
//!
//! Nothing runs on a timer of its own: [`Shell::next_wake`] is the earliest moment anything is
//! due (a long press, a repeat, a screen's own deadline, the player, a save, a toast), and a frame
//! is drawn only when something changed or an animation is still moving.

use std::ffi::CString;

use iptv::refresh::{Refresher, Transport};
use tv_machine::machine::Measure;
use tv_screens::data::{
    Command, Group, Navigate, Output, Playback, Rect, Screen, ScreenMessage, Stream, TvData,
};
use tv_screens::guide::GuideScreen;
use tv_screens::input::{Action, Key, Remote};
use tv_screens::lists::{ChannelList, History, Mode};
use tv_screens::panels::{keys, ContextMenu, PanelCtx, Search, Settings, SettingsPanel};
use tv_screens::player::{PlayerScreen, PlayerSettings};
use tv_screens::theme::{self, size, space};
use tv_ui::consts::{SCR_H, SCR_W};
use tv_ui::widgets::LegacyMeasure;
use tv_ui::Painter;

use crate::input::{Edge, KeyEvent};
use crate::logos::{LogoCache, Pixels};
use crate::logs::log;
use crate::player::{self, Player};
use crate::store::{refresh_config, Store};

mod groups;
mod watch;

use groups::Groups;
use watch::Watch;

/// A toast stays this long.
const TOAST_MS: u64 = 3_000;
/// How often the refresher's status is read.
const REFRESH_POLL_MS: u64 = 1_000;

/// Makes a network transport (one per worker that fetches).
pub type NetMaker = Box<dyn Fn() -> Box<dyn Transport>>;

struct Toast {
    text: CString,
    until: u64,
    /// Measured on the first frame that draws it.
    width: Option<f32>,
}

/// A screen and when it is next due, as it said after the last change.
struct Layer<S: ?Sized = dyn Screen> {
    wake: Option<u64>,
    screen: Box<S>,
}

impl<S: Screen + ?Sized> Layer<S> {
    fn new(screen: Box<S>) -> Self {
        Layer { wake: None, screen }
    }

    fn due(&self, now: u64) -> bool {
        self.wake.is_some_and(|w| now >= w)
    }

    /// Closed, and still animating out.
    fn sliding_out(&self) -> bool {
        self.screen.closing() && !self.screen.gone()
    }
}

pub struct Shell {
    store: Box<dyn Store>,
    player: Box<dyn Player>,
    /// Makes the network transport the refresher fetches through; `None` without a network.
    net: Option<NetMaker>,
    /// Absent while no playlist URL is set.
    refresher: Option<Refresher>,
    logos: LogoCache,
    /// The refresher's status generation the store last loaded.
    refreshed: u64,
    settings: Settings,
    remote: Remote,
    /// The full-screen player, under every other screen.
    bottom: Layer<PlayerScreen>,
    /// Bottom up.
    stack: Vec<Layer>,
    /// The video rectangle; `None` is full screen.
    rect: Option<Rect>,
    toast: Option<Toast>,
    watch: Watch,
    /// The playback changed: every screen looks again.
    playback_moved: bool,
    /// The TV keyboard is up (raised by us, or by the viewer when text arrived without it).
    typing: bool,
    /// What the loop must do to the keyboard: raise (`true`) or dismiss it.
    text_input: Option<bool>,
    refresh_at: u64,
    dirty: bool,
    animating: bool,
    exit: bool,
}

impl Shell {
    pub fn new(
        store: Box<dyn Store>,
        player: Box<dyn Player>,
        net: Option<NetMaker>,
        logos: LogoCache,
    ) -> Self {
        let settings = store.settings();
        let mut shell = Shell {
            bottom: Layer::new(Box::new(PlayerScreen::new(PlayerSettings::default()))),
            refresher: net.as_ref().and_then(|net| store.refresher(net(), true)),
            refreshed: 0,
            store,
            player,
            net,
            logos,
            settings,
            remote: Remote::default(),
            stack: Vec::new(),
            rect: None,
            toast: None,
            watch: Watch::default(),
            playback_moved: false,
            typing: false,
            text_input: None,
            refresh_at: 0,
            dirty: true,
            animating: false,
            exit: false,
        };
        shell.follow_settings();
        shell
    }

    /// The first screen: the last channel when the setting asks for it, otherwise the guide.
    pub fn start(&mut self, now: u64) {
        if self.settings.flag(keys::LAST_CHANNEL_ON_START) {
            if let Some(&key) = self.store.recent_channels().first() {
                self.play(Stream::Live(key), None, now);
            }
        }
        if self.store.playback_mut().channel.is_none() {
            self.navigate(Navigate::Guide, None);
        }
        self.settle(now);
    }

    /// One key edge from the remote (or the TV keyboard's delete and clear-all keys).
    pub fn key(&mut self, ev: KeyEvent) {
        // A printable key while the keyboard is up is typing: it arrives as text.
        if self.typing && (0x20..0x7f).contains(&ev.sym) {
            return;
        }
        let Some(key) = Key::from_sdl(ev.sym, ev.wcode) else {
            return;
        };
        let action = match ev.edge {
            Edge::Down => {
                let hold = self.screen(self.top()).0.hold(key);
                self.remote.down(key, ev.at_ms, hold)
            }
            Edge::Repeat => {
                self.remote.repeat(key, ev.at_ms);
                None
            }
            Edge::Up => self.remote.up(key, ev.at_ms),
        };
        if let Some(action) = action {
            self.act(action, u64::from(ev.at_ms));
        }
    }

    /// Text the TV keyboard committed, for the screen in front. Text while the keyboard is
    /// down means the viewer raised it (LG's SDL does not say so): it counts as up from now on.
    pub fn text(&mut self, text: &str, now: u64) {
        if !self.typing {
            log("keyboard: adopted (text arrived with it down)");
            self.typing = true;
        }
        let from = self.top();
        let (screen, data) = self.screen(from);
        let out = screen.text(text, data);
        self.dirty = true;
        self.apply(out, from, now);
        self.settle(now);
    }

    /// Run what is due at `now`: a long press or a repeat, what playback reported, the screens
    /// whose deadline came, saves, toasts, logos, refreshes.
    pub fn tick(&mut self, now: u64) {
        if let Some(action) = self.remote.tick(now as u32) {
            self.act(action, now);
        }
        if self.player.poll(now, self.store.playback_mut()) {
            self.playback_moved = true;
        }
        if let Some(text) = self.player.take_notice() {
            self.toast(text, now);
        }
        self.watch.tick(&mut *self.store, now);
        self.tick_screens(now);
        if self.toast.as_ref().is_some_and(|t| now >= t.until) {
            self.toast = None;
            self.dirty = true;
        }
        let mut upload =
            |px: &Pixels| tv_gfx::gfx::upload_rgba(0, px.w as i32, px.h as i32, px.rgba.as_ptr());
        self.dirty |= self.logos.pump(&mut upload, &mut tv_gfx::gfx::delete_tex);
        if self.refresher.is_some() && now >= self.refresh_at {
            self.refresh_at = now + REFRESH_POLL_MS;
            self.poll_refresh();
        }
        self.settle(now);
    }

    /// The earliest moment [`tick`](Self::tick) has something to do; `None` until an event.
    pub fn next_wake(&self, now: u64) -> Option<u64> {
        let screens = self.stack.iter().map(|l| l.wake);
        [
            self.remote
                .wait_ms(now as u32)
                .map(|ms| now + u64::from(ms)),
            self.playback_moved.then_some(now),
            self.bottom.wake,
            self.player.next_wake_ms(now),
            self.watch.next_wake(),
            self.toast.as_ref().map(|t| t.until),
            self.refresher.as_ref().map(|_| self.refresh_at),
        ]
        .into_iter()
        .chain(screens)
        .flatten()
        .min()
    }

    /// The TV keyboard is up: keys are typing, not remote presses.
    pub fn typing(&self) -> bool {
        self.typing
    }

    /// The keyboard change the loop must make, once.
    pub fn take_text_input(&mut self) -> Option<bool> {
        self.text_input.take()
    }

    /// The window needs its picture again (exposed).
    pub fn redraw(&mut self) {
        self.dirty = true;
    }

    /// The app went to the background: keep the archive position, release the player.
    pub fn suspend(&mut self) {
        self.save_progress();
        self.player.suspend();
    }

    /// Back in the foreground: the player plays on, the frame is drawn again.
    pub fn resume(&mut self) {
        self.player.resume();
        self.dirty = true;
    }

    /// Save where the archive programme playing is, for History (and on the way out).
    pub fn save_progress(&mut self) {
        self.watch.save(&mut *self.store);
    }

    /// Something changed or still moves: draw a frame.
    pub fn needs_frame(&self) -> bool {
        self.dirty || self.animating || self.player.needs_frames()
    }

    pub fn exit(&self) -> bool {
        self.exit
    }

    /// Step the animations by `dt` seconds and drop the closed screens that have animated out.
    pub fn update(&mut self, dt: f32) {
        tv_machine::idle::frame_begin(dt);
        self.bottom.screen.update(dt);
        for layer in &mut self.stack {
            layer.screen.update(dt);
        }
        self.stack
            .retain(|l| !(l.screen.closing() && l.screen.gone()));
        self.animating = tv_machine::idle::present_moving();
    }

    /// Draw the frame: the player while it is in front, every stacked screen bottom up, the
    /// toast.
    pub fn paint(&mut self) {
        let m = LegacyMeasure;
        let p = Painter::root();
        self.logos.next_frame();
        tv_gfx::gfx::frame_clear_through();
        if self.top().is_none() {
            self.bottom.screen.draw(p, &m, &self.logos);
        }
        for layer in &mut self.stack {
            layer.screen.draw(p, &m, &self.logos);
        }
        if let Some(t) = &mut self.toast {
            let w = *t
                .width
                .get_or_insert_with(|| m.width(&t.text, size::BODY, false));
            let h = 72.0;
            let r = tv_ui::Rect::new(
                (SCR_W - w) * 0.5 - space::XL,
                SCR_H - 160.0,
                w + 2.0 * space::XL,
                h,
            );
            p.rect(r, h * 0.5, theme::PANEL_RAISED, theme::PANEL_RAISED, 0.0);
            let y = r.y + (h - size::BODY as f32 * 1.2) * 0.5;
            p.text(
                t.text.as_ptr(),
                SCR_W * 0.5,
                y,
                size::BODY,
                theme::TEXT,
                1,
                theme::REGULAR,
            );
        }
        self.dirty = false;
    }

    /// The topmost screen still open, which gets keys and text; `None` is the player.
    fn top(&self) -> Option<usize> {
        self.stack.iter().rposition(|l| !l.screen.closing())
    }

    /// The screen at `at` (`None`: the player), with the data to hand it.
    fn screen(&mut self, at: Option<usize>) -> (&mut dyn Screen, &dyn TvData) {
        let screen: &mut dyn Screen = match at {
            Some(i) => &mut *self.stack[i].screen,
            None => &mut *self.bottom.screen,
        };
        (screen, &*self.store)
    }

    /// One remote action, to the screen in front.
    fn act(&mut self, action: Action, now: u64) {
        self.dirty = true;
        self.player.on_key();
        let from = self.top();
        let (screen, data) = self.screen(from);
        let out = screen.key(action.key, action.press, data, now);
        self.apply(out, from, now);
        self.settle(now);
    }

    /// Tick the screens whose deadline came, and every screen when the playback changed. Top
    /// down, so a screen that closes itself moves none still to come.
    fn tick_screens(&mut self, now: u64) {
        let moved = std::mem::take(&mut self.playback_moved);
        for i in (0..self.stack.len()).rev() {
            let Some(layer) = self.stack.get_mut(i).filter(|l| moved || l.due(now)) else {
                continue;
            };
            let out = layer.screen.tick(&*self.store, now);
            self.apply(out, Some(i), now);
        }
        if moved || self.bottom.due(now) {
            let (screen, data) = self.screen(None);
            let out = screen.tick(data, now);
            self.apply(out, None, now);
        }
    }

    /// Carry out what the screen at `from` (`None`: the player) returned. Its message goes to
    /// the open screen under it, the player when there is none; Manage favorites over the
    /// player opens the guide for it once the sender's own navigation is done.
    fn apply(&mut self, out: Output, from: Option<usize>, now: u64) {
        self.dirty |= out.redraw;
        for command in out.commands {
            self.run(command, now);
        }
        let mut manage_favorites = false;
        if let (Some(message), Some(i)) = (out.message, from) {
            let under = self.stack[..i].iter().rposition(|l| !l.screen.closing());
            if under.is_none() && message == ScreenMessage::ManageFavorites {
                manage_favorites = true;
            } else {
                let (screen, data) = self.screen(under);
                screen.message(message, data);
            }
            self.dirty = true;
        }
        if let Some(text) = out.toast {
            self.toast(text, now);
        }
        for nav in out.navigate {
            self.navigate(nav, from);
        }
        if manage_favorites {
            self.navigate(Navigate::Guide, None);
            let (guide, data) = self.screen(self.top());
            guide.message(ScreenMessage::ManageFavorites, data);
        }
    }

    fn toast(&mut self, text: String, now: u64) {
        self.toast = Some(Toast {
            text: tv_screens::draw::cstr(&text),
            until: now + TOAST_MS,
            width: None,
        });
        self.dirty = true;
    }

    /// After a change, follow the screens: when each is next due, where the video goes, and
    /// whether the TV keyboard is up.
    fn settle(&mut self, now: u64) {
        self.bottom.wake = self.bottom.screen.next_wake(now);
        for layer in &mut self.stack {
            layer.wake = layer.screen.next_wake(now);
        }
        let mut open = self.stack.iter().rev().filter(|l| !l.screen.closing());
        let rect = open
            .find_map(|l| l.screen.video_rect())
            .or_else(|| self.bottom.screen.video_rect());
        if rect != self.rect {
            self.rect = rect;
            self.player.set_rect(rect);
        }
        let editing = self.screen(self.top()).0.editing();
        if editing != self.typing {
            self.typing = editing;
            self.text_input = Some(editing);
        }
    }

    fn navigate(&mut self, nav: Navigate, from: Option<usize>) {
        let screen: Box<dyn Screen> = match nav {
            Navigate::Close => {
                if let Some(i) = from.filter(|&i| i < self.stack.len()) {
                    if !self.stack[i].sliding_out() {
                        self.stack.remove(i);
                    }
                }
                return;
            }
            Navigate::Player => {
                self.clear();
                return;
            }
            Navigate::Guide => {
                let group = self.bottom.screen.group().clone();
                let guide = GuideScreen::new(&*self.store, group);
                return self.replace(Box::new(guide));
            }
            Navigate::ChannelList { preview } => {
                let mode = if preview {
                    Mode::Preview
                } else {
                    Mode::Overlay
                };
                let group = self.bottom.screen.group().clone();
                let list = ChannelList::new(mode, group, &*self.store);
                return self.replace(Box::new(list));
            }
            Navigate::Groups(group) => Box::new(Groups::open(&*self.store, &group)),
            Navigate::History => Box::new(History::open(&*self.store)),
            Navigate::Settings => Box::new(SettingsPanel::new(
                self.settings.clone(),
                env!("CARGO_PKG_VERSION"),
            )),
            Navigate::Search => Box::new(Search::new(&*self.store)),
            Navigate::MainMenu
            | Navigate::ContextMenu { .. }
            | Navigate::GroupMenu(_)
            | Navigate::PlayerMenu => {
                let cx = PanelCtx {
                    data: &*self.store,
                    settings: &self.settings,
                };
                match ContextMenu::open(&nav, &cx) {
                    Some(menu) => Box::new(menu),
                    None => return,
                }
            }
        };
        self.stack.push(Layer::new(screen));
    }

    /// Close every screen: the player is in front. Screens already animating out go on doing so.
    fn clear(&mut self) {
        self.stack.retain(Layer::sliding_out);
    }

    /// Close every screen and open `screen` over the player, under the ones animating out.
    fn replace(&mut self, screen: Box<dyn Screen>) {
        self.clear();
        self.stack.insert(0, Layer::new(screen));
    }

    fn run(&mut self, command: Command, now: u64) {
        match command {
            Command::Play { stream, group } => self.play(stream, group, now),
            Command::SeekTo { position_secs } => {
                let pb = self.store.playback_mut();
                if pb.catchup.is_some() {
                    pb.position_secs = position_secs;
                    self.player.seek(position_secs);
                    self.playback_moved = true;
                }
            }
            Command::ReturnToLive => {
                let pb = self.store.playback_mut();
                if let Some(key) = pb.channel.filter(|_| pb.catchup.is_some()) {
                    self.watch.begin(&Stream::Live(key), &mut *self.store, now);
                    self.player.go_live();
                    *self.store.playback_mut() = Playback {
                        channel: Some(key),
                        ..Playback::default()
                    };
                    self.playback_moved = true;
                }
            }
            Command::TogglePause => {
                let pb = self.store.playback_mut();
                if pb.catchup.is_some() {
                    pb.paused = !pb.paused;
                    let paused = pb.paused;
                    self.player.set_paused(paused);
                    self.playback_moved = true;
                }
            }
            Command::SelectAudioTrack(i) => {
                let pb = self.store.playback_mut();
                if i < pb.audio_tracks.len() {
                    pb.audio_track = i;
                    self.player.select_audio_track(i);
                    self.playback_moved = true;
                }
            }
            Command::ToggleFavorite(key) => {
                self.store.toggle_favorite(key);
                self.refresh_screens();
            }
            Command::RemoveHistory { channel, start } => {
                self.store.remove_history(channel, start);
                self.refresh_screens();
            }
            Command::RefreshNow => match &self.refresher {
                Some(r) => r.refresh_now(),
                None => log("refresh: this build has no network transport"),
            },
            Command::SetSetting { key, value } => self.set_setting(&key, value),
            Command::Exit => self.exit = true,
        }
    }

    /// Play `stream`, picked from `group`, in the current rectangle (the screens move it
    /// after); the live channel already playing is left playing. The player zaps through
    /// `group` from now on ([`PlayerScreen::zap_through`]).
    fn play(&mut self, stream: Stream, group: Option<Group>, now: u64) {
        let key = stream.channel();
        self.bottom
            .screen
            .zap_through(group, Some(key), &*self.store);
        let pb = self.store.playback_mut();
        if stream == Stream::Live(key) && pb.channel == Some(key) && pb.catchup.is_none() {
            return;
        }
        let Some(channel) = self.store.channel(key) else {
            log(&format!("play: channel {} is gone", key.hex()));
            return;
        };
        let open = match &stream {
            Stream::Live(_) => player::Stream::Live(&channel),
            Stream::Catchup {
                programme,
                offset_secs,
                ..
            } if channel.catchup.available() => player::Stream::Catchup {
                channel: &channel,
                programme,
                offset_secs: *offset_secs,
            },
            Stream::Catchup { .. } => {
                log(&format!("play: channel {} has no archive", key.hex()));
                return;
            }
        };
        self.watch.begin(&stream, &mut *self.store, now);
        self.player.open(&open, self.rect);
        let (catchup, position_secs) = match stream {
            Stream::Live(_) => (None, 0),
            Stream::Catchup {
                programme,
                offset_secs,
                ..
            } => (Some(programme), offset_secs),
        };
        *self.store.playback_mut() = Playback {
            channel: Some(key),
            catchup,
            position_secs,
            ..Playback::default()
        };
        self.playback_moved = true;
    }

    fn set_setting(&mut self, key: &str, value: String) {
        let config = refresh_config(&self.settings);
        if !self.settings.set(key, value) {
            return;
        }
        self.store.save_setting(key, &self.settings);
        self.follow_settings();
        if refresh_config(&self.settings) != config {
            self.reconfigure();
        }
        self.refresh_screens();
    }

    /// Everything that follows the settings, from them.
    fn follow_settings(&mut self) {
        self.bottom.screen.settings = PlayerSettings::from_settings(&self.settings);
        self.remote.long_ms = self.settings.number(keys::LONG_PRESS_MS);
    }

    /// The refresh settings changed: refresh with them, start refreshing once a playlist URL is
    /// set, stop when it is cleared.
    fn reconfigure(&mut self) {
        let config = refresh_config(&self.settings);
        if config.playlist_url.is_empty() {
            self.refresher = None;
        } else if let Some(r) = &self.refresher {
            r.set_config(config);
        } else if let Some(net) = &self.net {
            self.refresher = self.store.refresher(net(), false);
            self.refreshed = 0;
        }
    }

    /// The refresher stored new data: read it again everywhere.
    fn poll_refresh(&mut self) {
        let Some(generation) = self.refresher.as_ref().map(Refresher::generation) else {
            return;
        };
        if generation != self.refreshed {
            self.refreshed = generation;
            self.store.reload();
            self.refresh_screens();
        }
    }

    /// The stored data changed: every screen reads it again.
    fn refresh_screens(&mut self) {
        let data: &dyn TvData = &*self.store;
        self.bottom.screen.refresh(data);
        for layer in &mut self.stack {
            layer.screen.refresh(data);
        }
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests;
