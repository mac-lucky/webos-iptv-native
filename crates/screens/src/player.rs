//! The full-screen player's overlays, as in TiviMate.
//!
//! - **Nothing up**: Up/Down and CH+/- zap through the current group, short Right goes back to
//!   the last channel, digits type a channel number, OK opens the info panel, long Right the
//!   programme card, Back the guide (long Back Settings), Left the channel list (long Left in
//!   preview mode), long Up History, long OK the channel menu. A zap shows the info banner (the
//!   panel without its tile row) for the panel timeout.
//! - **Info panel** (OK): logo, programme, times, progress, channel, media badges, the next
//!   programme and a tile row (TV guide, History, recent channels); Left/Right move along the
//!   row, OK opens or plays the tile, Up opens the playback panel, Down the player menu.
//! - **Playback panel** (Up from the info panel, catch-up or a live channel with an archive): a
//!   seek bar where Left/Right seek by the skip step (held: faster), sent as one seek once the
//!   presses stop, and below it prev/rewind/pause/forward/next, restart and Live. The tile row
//!   is hidden while it is up; Down from the buttons brings the info panel back.
//! - **Programme card** (long Right): the programme on screen, top right; Right steps on to the
//!   next ones, Left back.
//!
//! [`PlayerScreen`] is the [`Screen`] at the bottom of the shell's stack, a pure state machine:
//! keys and ticks in, [`Output`] out. It reads through [`TvData`], keeps what the panels show as
//! strings ([`model`]) rebuilt only when something changed, and draws from them ([`view`]). The
//! guide is read when the channel or the programme changes, not on every tick. Timeouts run on
//! the shell's millisecond clock and [`Screen::next_wake`] says when the next one is due, so an
//! idle player never redraws.

pub mod model;
pub mod transport;
pub mod view;

#[cfg(test)]
mod tests;

use std::rc::Rc;

use crate::data::{
    Command, Group, Logos, Navigate, Output, Playback, Screen, ScreenMessage, Stream, TvData,
};
use crate::draw::{self, Cached, Piece};
use crate::input::{Hold, Key, Press};
use crate::panels::settings::{keys, Settings};
use crate::time;
use iptv::model::{Channel, ChannelKey, Programme};
use model::{Card, Context, Header, Tile, TileKind};
use transport::{Button, Focus, Readout, Transport};
use tv_machine::machine::Measure;
use tv_ui::Painter;

/// The player's stored settings ([`crate::panels::settings`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerSettings {
    /// How long a panel stays after the last key.
    pub panel_timeout_ms: u64,
    /// A zap (or a channel started elsewhere) shows the info banner.
    pub info_on_zap: bool,
    /// What one tap of Left/Right on the seek bar moves back and forward.
    pub skip_back_secs: i64,
    pub skip_forward_secs: i64,
}

impl Default for PlayerSettings {
    fn default() -> Self {
        Self::from_settings(&Settings::new())
    }
}

impl PlayerSettings {
    pub fn from_settings(s: &Settings) -> Self {
        PlayerSettings {
            panel_timeout_ms: u64::from(s.number(keys::PLAYER_INFO_SECS)) * 1000,
            info_on_zap: s.flag(keys::PLAYER_INFO_ON_ZAP),
            skip_back_secs: s.number(keys::SKIP_BACK_SECS).into(),
            skip_forward_secs: s.number(keys::SKIP_FORWARD_SECS).into(),
        }
    }

    /// The seek step for direction `dir` (-1 back, +1 forward).
    pub fn skip_secs(&self, dir: i64) -> i64 {
        if dir < 0 {
            self.skip_back_secs
        } else {
            self.skip_forward_secs
        }
    }
}

/// Recent channels in the info panel's tile row (TiviMate's default).
pub const RECENT_TILES: usize = 10;
/// How long a typed channel number waits for the next digit.
pub const NUMBER_TIMEOUT_MS: u64 = 2_000;

/// Which panel is up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlay {
    Hidden,
    /// The info panel without its tile row, after a zap.
    Banner,
    /// The info panel; `tile` is the focused tile of the row.
    Info {
        tile: usize,
    },
    Playback,
    /// The programme card; `index` counts programmes on from the one on screen.
    Card {
        index: usize,
    },
}

/// A channel number being typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumberEntry {
    pub digits: String,
    /// The channel the digits name so far, when there is one.
    pub preview: Option<String>,
    pub commit_at_ms: u64,
}

/// The most digits a channel number takes.
pub const NUMBER_DIGITS: usize = 4;
/// How far ahead the programme card can step.
const CARD_AHEAD_SECS: i64 = 24 * 3600;

pub struct PlayerScreen {
    pub settings: PlayerSettings,
    /// The list zapping walks.
    group: Group,
    overlay: Overlay,
    hide_at_ms: Option<u64>,
    transport: Transport,
    number: Option<NumberEntry>,
    /// The channel just asked for, until playback reports it (or the next tick finds it was not
    /// played).
    pending: Option<ChannelKey>,
    /// The stored data changed: the next sync reads the channel and its guide again.
    reread: bool,
    ctx: Context,
    header: Header,
    tiles: Vec<Tile>,
    readout: Readout,
    /// The programmes the card steps through, from the one on screen on.
    card_list: Vec<Programme>,
    card: Card,
    /// Moves on every change worth a frame; the view lays out again when it does.
    generation: u64,
    view: Cached,
}

impl PlayerScreen {
    pub fn new(settings: PlayerSettings) -> Self {
        PlayerScreen {
            settings,
            group: Group::All,
            overlay: Overlay::Hidden,
            hide_at_ms: None,
            transport: Transport::default(),
            number: None,
            pending: None,
            reread: false,
            ctx: Context::default(),
            header: Header::default(),
            tiles: Vec::new(),
            readout: Readout::default(),
            card_list: Vec::new(),
            card: Card::default(),
            generation: 0,
            view: Cached::default(),
        }
    }

    /// Changes whenever what the player draws may have changed.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The group zapping walks and the top bar names from now on: `group` (the list `channel`
    /// was picked from; `None` keeps the current one), or every channel when it does not list
    /// `channel`, the one playing. Every play goes through here, so a zap never jumps out of
    /// a group to its first channel.
    pub fn zap_through(
        &mut self,
        group: Option<Group>,
        channel: Option<ChannelKey>,
        data: &dyn TvData,
    ) {
        let group = group.unwrap_or_else(|| self.group.clone());
        let listed = channel.is_none_or(|k| data.channels(&group).iter().any(|c| c.key == k));
        let group = if listed { group } else { Group::All };
        if group != self.group {
            self.group = group;
            self.header = Header::build(&self.ctx, &self.group);
            self.generation += 1;
        }
    }

    pub fn group(&self) -> &Group {
        &self.group
    }

    pub fn overlay(&self) -> Overlay {
        self.overlay
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn tiles(&self) -> &[Tile] {
        &self.tiles
    }

    pub fn readout(&self) -> &Readout {
        &self.readout
    }

    pub fn transport_focus(&self) -> Focus {
        self.transport.focus
    }

    /// Whether a transport button does anything for what is playing.
    pub fn button_enabled(&self, b: Button) -> bool {
        transport::enabled(b, &self.ctx)
    }

    pub fn card(&self) -> &Card {
        &self.card
    }

    pub fn number(&self) -> Option<&NumberEntry> {
        self.number.as_ref()
    }

    /// The panels laid out for the current state ([`Screen::draw`] paints them); laid out again
    /// only when the generation moved.
    pub fn layout(&mut self, m: &dyn Measure) -> &[Piece] {
        let mut view = std::mem::take(&mut self.view);
        view.prepare(self.generation, |out| view::build(self, m, out));
        self.view = view;
        self.view.pieces()
    }

    /// Keys with no panel up (or only the passive zap banner).
    fn key_free(
        &mut self,
        key: Key,
        press: Press,
        data: &dyn TvData,
        now_ms: u64,
        out: &mut Output,
    ) {
        match (key, press) {
            (Key::Ok, Press::Short) => self.open(Overlay::Info { tile: 0 }, data, now_ms),
            (Key::Ok, Press::Long) => out.navigate.extend(self.channel_menu()),
            (Key::Up, Press::Long) => out.navigate.push(Navigate::History),
            (Key::Left, Press::Short) => {
                out.navigate.push(Navigate::ChannelList { preview: false })
            }
            (Key::Left, Press::Long) => out.navigate.push(Navigate::ChannelList { preview: true }),
            (Key::Right, Press::Short) => self.last_channel(data, now_ms, out),
            (Key::Right, Press::Long) => self.open_card(data),
            (Key::Back, Press::Short) => out.navigate.push(Navigate::Guide),
            (Key::Back, Press::Long) => out.navigate.push(Navigate::Settings),
            _ => self.key_common(key, press, data, now_ms, out),
        }
    }

    fn key_info(
        &mut self,
        key: Key,
        press: Press,
        tile: usize,
        data: &dyn TvData,
        now_ms: u64,
        out: &mut Output,
    ) {
        let last = self.tiles.len().saturating_sub(1);
        match (key, press) {
            (Key::Left, _) => self.show(
                Overlay::Info {
                    tile: tile.saturating_sub(1),
                },
                now_ms,
            ),
            (Key::Right, _) => self.show(
                Overlay::Info {
                    tile: (tile + 1).min(last),
                },
                now_ms,
            ),
            (Key::Ok, Press::Short) => match self.tiles.get(tile).map(|t| t.kind.clone()) {
                Some(TileKind::Guide) => {
                    self.hide(out);
                    out.navigate.push(Navigate::Guide);
                }
                Some(TileKind::History) => {
                    self.hide(out);
                    out.navigate.push(Navigate::History);
                }
                Some(TileKind::Channel(k)) => self.play(k, data, now_ms, out),
                None => {}
            },
            (Key::Ok, Press::Long) => out.navigate.extend(self.channel_menu()),
            (Key::Up, Press::Short) => {
                if transport::can_seek(&self.ctx) {
                    self.transport = Transport::default();
                    self.readout = Readout::build(&self.transport, &self.ctx);
                    self.show(Overlay::Playback, now_ms);
                }
            }
            (Key::Down, Press::Short) => {
                self.hide(out);
                out.navigate.push(Navigate::PlayerMenu);
            }
            (Key::Back, Press::Short) => self.hide(out),
            _ => self.key_common(key, press, data, now_ms, out),
        }
    }

    fn key_playback(
        &mut self,
        key: Key,
        press: Press,
        data: &dyn TvData,
        now_ms: u64,
        out: &mut Output,
    ) {
        let held = press != Press::Short;
        match (self.transport.focus, key) {
            (Focus::Seek, Key::Left | Key::Right) => {
                let dir = if key == Key::Left { -1 } else { 1 };
                self.transport
                    .seek(&self.ctx, dir, held, &self.settings, now_ms);
            }
            (Focus::Seek, Key::Down) => {
                self.transport.enter_buttons(&self.ctx);
            }
            // The seek bar is the panel's top row.
            (Focus::Seek, Key::Up) => {}
            (Focus::Seek, Key::Ok) => {
                let sent = self.transport.commit(&self.ctx, now_ms, true);
                if sent.is_empty() && self.ctx.in_catchup() {
                    out.commands.push(Command::TogglePause);
                }
                out.commands.extend(sent);
            }
            (Focus::Button(_), Key::Left | Key::Right) if !held => {
                let dir = if key == Key::Left { -1 } else { 1 };
                self.transport.step_button(dir, &self.ctx);
            }
            (Focus::Button(_), Key::Up) => self.transport.focus = Focus::Seek,
            (Focus::Button(_), Key::Down) => {
                let sent = self.transport.commit(&self.ctx, now_ms, true);
                out.commands.extend(sent);
                self.show(Overlay::Info { tile: 0 }, now_ms);
            }
            (Focus::Button(b), Key::Ok) if press == Press::Short => {
                out.commands
                    .extend(self.transport.press(b, &self.ctx, &self.settings, now_ms));
            }
            (_, Key::Back) => {
                // Back drops a burst not sent yet; with none, it closes the panel.
                if self.transport.burst.take().is_none() {
                    self.hide(out);
                    return;
                }
            }
            _ => return self.key_common(key, press, data, now_ms, out),
        }
        self.readout = Readout::build(&self.transport, &self.ctx);
        if self.overlay == Overlay::Playback {
            self.show(Overlay::Playback, now_ms);
        }
    }

    fn key_card(
        &mut self,
        key: Key,
        press: Press,
        index: usize,
        data: &dyn TvData,
        now_ms: u64,
        out: &mut Output,
    ) {
        match key {
            Key::Right => self.card_at(index + 1),
            Key::Left => self.card_at(index.saturating_sub(1)),
            Key::Back | Key::Ok => self.hide(out),
            _ => {
                self.overlay = Overlay::Hidden;
                self.key_common(key, press, data, now_ms, out);
            }
        }
    }

    /// Keys that work the same under every panel: zapping and the media keys.
    fn key_common(
        &mut self,
        key: Key,
        press: Press,
        data: &dyn TvData,
        now_ms: u64,
        out: &mut Output,
    ) {
        if press != Press::Short && !matches!(key, Key::FastForward | Key::Rewind) {
            return;
        }
        match key {
            Key::Up | Key::ChannelUp => self.zap(1, data, now_ms, out),
            Key::Down | Key::ChannelDown => self.zap(-1, data, now_ms, out),
            Key::PlayPause if self.ctx.in_catchup() => out.commands.push(Command::TogglePause),
            Key::Play if self.ctx.in_catchup() && self.ctx.playback.paused => {
                out.commands.push(Command::TogglePause)
            }
            Key::Pause if self.ctx.in_catchup() && !self.ctx.playback.paused => {
                out.commands.push(Command::TogglePause)
            }
            Key::FastForward | Key::Rewind if transport::can_seek(&self.ctx) => {
                if self.overlay != Overlay::Playback {
                    self.transport = Transport::default();
                }
                let dir = if key == Key::Rewind { -1 } else { 1 };
                let held = press != Press::Short;
                self.transport
                    .seek(&self.ctx, dir, held, &self.settings, now_ms);
                self.readout = Readout::build(&self.transport, &self.ctx);
                self.open(Overlay::Playback, data, now_ms);
            }
            _ => out.redraw = false,
        }
    }

    fn show(&mut self, overlay: Overlay, now_ms: u64) {
        self.overlay = match overlay {
            Overlay::Info { tile } => Overlay::Info {
                tile: tile.min(self.tiles.len().saturating_sub(1)),
            },
            o => o,
        };
        self.hide_at_ms = match overlay {
            Overlay::Hidden | Overlay::Card { .. } => None,
            _ => Some(now_ms + self.settings.panel_timeout_ms),
        };
    }

    /// Show the info or the playback panel, reading the tile row afresh when neither was up (the
    /// playback panel hides the row, but Down from it shows the info panel's).
    fn open(&mut self, overlay: Overlay, data: &dyn TvData, now_ms: u64) {
        let had_tiles = matches!(self.overlay, Overlay::Info { .. } | Overlay::Playback);
        self.overlay = overlay;
        if !had_tiles {
            self.sync(data, true);
        }
        self.show(self.overlay, now_ms);
    }

    fn hide(&mut self, out: &mut Output) {
        if self.overlay != Overlay::Hidden {
            out.redraw = true;
        }
        out.commands
            .extend(self.transport.commit(&self.ctx, 0, true));
        self.overlay = Overlay::Hidden;
        self.hide_at_ms = None;
    }

    /// Long OK: the context menu of the channel on screen.
    fn channel_menu(&self) -> Option<Navigate> {
        self.ctx.key().map(|channel| Navigate::ContextMenu {
            channel,
            programme: None,
        })
    }

    /// The channel on screen, or the one just asked for.
    fn current_key(&self) -> Option<ChannelKey> {
        self.ctx.playback.channel
    }

    /// Play `key` live. A channel the zap group does not list (a number, the last channel, a
    /// recent one) moves zapping to every channel ([`zap_through`](Self::zap_through)).
    fn play(&mut self, key: ChannelKey, data: &dyn TvData, now_ms: u64, out: &mut Output) {
        self.zap_through(None, Some(key), data);
        out.commands.push(Command::Play {
            stream: Stream::Live(key),
            group: None,
        });
        self.pending = Some(key);
        self.transport = Transport::default();
        if self.settings.info_on_zap {
            self.show(Overlay::Banner, now_ms);
        } else {
            self.show(Overlay::Hidden, now_ms);
        }
        self.sync(data, false);
    }

    /// Step `dir` channels through the current group, wrapping at its ends.
    fn zap(&mut self, dir: i32, data: &dyn TvData, now_ms: u64, out: &mut Output) {
        let list = data.channels(&self.group);
        if list.is_empty() {
            return;
        }
        let len = list.len() as i32;
        let current = self.current_key();
        let at = current.and_then(|k| list.iter().position(|c| c.key == k));
        let first = match at {
            Some(i) => i as i32 + dir,
            None => 0,
        };
        // Zapping steps over the channels the parental PIN locks.
        let Some(next) = (0..len)
            .map(|n| (first + n * dir.signum()).rem_euclid(len) as usize)
            .find(|&i| !data.locked(&list[i]))
        else {
            return;
        };
        if Some(list[next].key) != current {
            self.play(list[next].key, data, now_ms, out);
        }
    }

    /// Short Right: back to the channel watched before this one.
    fn last_channel(&mut self, data: &dyn TvData, now_ms: u64, out: &mut Output) {
        let current = self.current_key();
        if let Some(k) = data
            .recent_channels()
            .into_iter()
            .find(|k| Some(*k) != current)
        {
            self.play(k, data, now_ms, out);
        }
    }

    fn digit(&mut self, d: u8, data: &dyn TvData, now_ms: u64) {
        if d > 9 {
            return;
        }
        if matches!(
            self.overlay,
            Overlay::Card { .. } | Overlay::Info { .. } | Overlay::Playback
        ) {
            self.overlay = Overlay::Hidden;
            self.hide_at_ms = None;
        }
        let entry = self.number.get_or_insert(NumberEntry {
            digits: String::new(),
            preview: None,
            commit_at_ms: 0,
        });
        if entry.digits.len() < NUMBER_DIGITS {
            entry.digits.push(char::from(b'0' + d));
        }
        entry.commit_at_ms = now_ms + NUMBER_TIMEOUT_MS;
        let wanted = entry.digits.parse::<u32>().ok();
        entry.preview = wanted
            .and_then(|n| find_number(data, n))
            .map(|c| c.name.clone());
    }

    fn commit_number(&mut self, data: &dyn TvData, now_ms: u64, out: &mut Output) {
        out.redraw = true;
        let Some(entry) = self.number.take() else {
            return;
        };
        let Some(n) = entry.digits.parse::<u32>().ok() else {
            return;
        };
        if let Some(c) = find_number(data, n) {
            if Some(c.key) != self.current_key() {
                self.play(c.key, data, now_ms, out);
            }
        }
    }

    fn open_card(&mut self, data: &dyn TvData) {
        let Some(ch) = self.ctx.key() else {
            return;
        };
        let Some(current) = &self.ctx.current else {
            return;
        };
        let from = current.start;
        self.card_list = data
            .programmes(ch, from, from + CARD_AHEAD_SECS)
            .into_iter()
            .filter(|p| p.start >= from)
            .collect();
        if self.card_list.is_empty() {
            self.card_list.push(current.clone());
        }
        self.card_at(0);
    }

    fn card_at(&mut self, index: usize) {
        let index = index.min(self.card_list.len().saturating_sub(1));
        if let Some(p) = self.card_list.get(index) {
            self.card = Card::build(&self.ctx, p);
            self.overlay = Overlay::Card { index };
            self.hide_at_ms = None;
        }
    }

    /// What the panels describe: `playback`, or the channel just asked for (live) until
    /// playback reports it.
    fn effective(&self, playback: Playback) -> Playback {
        match self.pending {
            Some(k) if playback.channel != Some(k) => Playback {
                channel: Some(k),
                ..Playback::default()
            },
            _ => playback,
        }
    }

    /// [`sync_with`](Self::sync_with) what plays now.
    fn sync(&mut self, data: &dyn TvData, tiles: bool) {
        let playback = self.effective(data.playback());
        self.sync_with(data, playback, tiles);
    }

    /// Rebuild what the panels show for `playback` (already [`effective`](Self::effective)).
    /// The channel and its guide are read again only when the channel or the programme changed,
    /// the one on air ended or the stored data changed; `tiles` also reads the tile row again
    /// when it is up.
    fn sync_with(&mut self, data: &dyn TvData, playback: Playback, tiles: bool) {
        if std::mem::take(&mut self.reread) || self.ctx.stale(&playback, data.now()) {
            let channel = playback.channel.and_then(|k| numbered(data, k));
            self.ctx = Context::build(data, channel, playback);
        } else {
            self.ctx.advance(data, playback);
        }
        self.header = Header::build(&self.ctx, &self.group);
        self.readout = Readout::build(&self.transport, &self.ctx);
        if tiles && matches!(self.overlay, Overlay::Info { .. } | Overlay::Playback) {
            self.tiles = model::tiles(data, self.current_key());
            if let Overlay::Info { tile } = self.overlay {
                self.overlay = Overlay::Info {
                    tile: tile.min(self.tiles.len().saturating_sub(1)),
                };
            }
        }
        if let Overlay::Card { index } = self.overlay {
            if let Some(p) = self.card_list.get(index) {
                self.card = Card::build(&self.ctx, p);
            }
        }
    }
}

impl Screen for PlayerScreen {
    /// How `key` behaves while held, for the input layer ([`crate::input::Remote::down`]).
    fn hold(&self, key: Key) -> Hold {
        let seek_bar = self.overlay == Overlay::Playback && self.transport.focus == Focus::Seek;
        match (self.overlay, key) {
            (_, Key::FastForward | Key::Rewind) => Hold::Repeat,
            (
                Overlay::Hidden | Overlay::Banner,
                Key::Ok | Key::Up | Key::Left | Key::Right | Key::Back,
            ) => Hold::Long,
            (Overlay::Info { .. }, Key::Ok) => Hold::Long,
            (Overlay::Info { .. }, Key::Left | Key::Right) => Hold::Repeat,
            (Overlay::Playback, Key::Left | Key::Right) if seek_bar => Hold::Repeat,
            (Overlay::Card { .. }, Key::Left | Key::Right) => Hold::Repeat,
            _ => Hold::Once,
        }
    }

    fn key(&mut self, key: Key, press: Press, data: &dyn TvData, now_ms: u64) -> Output {
        let mut out = Output::redraw();
        if let Key::Digit(d) = key {
            if press != Press::Short {
                return Output::default();
            }
            self.digit(d, data, now_ms);
            self.generation += 1;
            return out;
        }
        if self.number.is_some() {
            match key {
                Key::Ok => self.commit_number(data, now_ms, &mut out),
                _ => self.number = None,
            }
            if matches!(key, Key::Ok | Key::Back) {
                self.generation += 1;
                return out;
            }
        }
        if self.overlay == Overlay::Hidden || self.ctx.channel.is_none() {
            // Nothing keeps a hidden player up to date: read what plays before acting on it.
            self.sync(data, false);
        }
        match self.overlay {
            Overlay::Hidden | Overlay::Banner => self.key_free(key, press, data, now_ms, &mut out),
            Overlay::Info { tile } => self.key_info(key, press, tile, data, now_ms, &mut out),
            Overlay::Playback => self.key_playback(key, press, data, now_ms, &mut out),
            Overlay::Card { index } => self.key_card(key, press, index, data, now_ms, &mut out),
        }
        self.generation += u64::from(out.redraw);
        out
    }

    /// Send a finished seek burst or channel number, hide a timed-out panel, follow what
    /// playback reports. A tick inside the same programme reads no guide.
    fn tick(&mut self, data: &dyn TvData, now_ms: u64) -> Output {
        // The shell carries out a key's play before any tick: a channel asked for that playback
        // does not report by now was held back for the parental PIN (or is gone), and the panels
        // and the channel menu go back to what really plays.
        if self
            .pending
            .is_some_and(|k| data.playback().channel != Some(k))
        {
            self.pending = None;
        }
        let mut out = Output::default();
        if self
            .number
            .as_ref()
            .is_some_and(|n| now_ms >= n.commit_at_ms)
        {
            self.commit_number(data, now_ms, &mut out);
        }
        let seeks = self.transport.commit(&self.ctx, now_ms, false);
        if !seeks.is_empty() {
            out.commands.extend(seeks);
            out.redraw = true;
        }
        if self.hide_at_ms.is_some_and(|t| now_ms >= t) {
            self.hide(&mut out);
        }

        let playback = data.playback();
        if playback.channel.is_some() && playback.channel == self.pending {
            self.pending = None;
        }
        let playback = self.effective(playback);
        if playback.channel.is_some() && playback.channel != self.ctx.playback.channel {
            // A channel started from elsewhere (the guide, a list): announce it.
            if self.overlay == Overlay::Hidden && self.settings.info_on_zap {
                self.show(Overlay::Banner, now_ms);
            }
            self.sync_with(data, playback, true);
            out.redraw |= self.overlay != Overlay::Hidden;
        } else if self.overlay != Overlay::Hidden {
            let now = data.now();
            let minute = now.div_euclid(60) != self.ctx.now.div_euclid(60);
            let second = self.overlay == Overlay::Playback && now != self.ctx.now;
            if minute || second || playback != self.ctx.playback {
                self.sync_with(data, playback, minute);
                out.redraw = true;
            }
        }
        self.generation += u64::from(out.redraw);
        out
    }

    /// A timeout, a pending seek or channel number, the shown second (playback panel) or minute
    /// (the clock and the minutes left) turning over; `None` while nothing is up.
    fn next_wake(&self, now_ms: u64) -> Option<u64> {
        let refresh = match self.overlay {
            Overlay::Hidden => None,
            Overlay::Playback => Some(now_ms + 1000),
            _ => {
                let minute = self.ctx.now - self.ctx.now.rem_euclid(60) + 60;
                Some(time::wake_ms(now_ms, self.ctx.now, minute))
            }
        };
        [
            self.hide_at_ms,
            self.number.as_ref().map(|n| n.commit_at_ms),
            self.transport.burst.map(|b| b.commit_at_ms),
            refresh,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// A group picked over the player (with no list in between) is the one to zap through.
    fn message(&mut self, message: ScreenMessage, data: &dyn TvData) {
        if let ScreenMessage::SetGroup(group) = message {
            self.zap_through(Some(group), data.playback().channel, data);
        }
    }

    /// Channel names and numbers and the guide are read again; an open panel shows them now.
    /// A zap group the data no longer has (a custom group renamed or deleted, a playlist group
    /// gone) gives way to every channel.
    fn refresh(&mut self, data: &dyn TvData) {
        if !data.all_groups().contains(&self.group) {
            self.zap_through(Some(Group::All), None, data);
        }
        self.reread = true;
        if self.overlay != Overlay::Hidden {
            self.sync(data, true);
            self.generation += 1;
        }
    }

    fn draw(&mut self, p: Painter, m: &dyn Measure, logos: &dyn Logos) {
        self.layout(m);
        self.view.paint(p, logos);
    }
}

/// The channel numbered `n`: its declared number, else its position in the playlist.
fn find_number(data: &dyn TvData, n: u32) -> Option<Rc<Channel>> {
    data.channels(&Group::All)
        .iter()
        .enumerate()
        .find(|(i, c)| draw::channel_number(c, *i) == n)
        .map(|(_, c)| Rc::clone(c))
}

/// The channel with `key` and the number it shows.
fn numbered(data: &dyn TvData, key: ChannelKey) -> Option<(Rc<Channel>, u32)> {
    data.channels(&Group::All)
        .iter()
        .enumerate()
        .find(|(_, c)| c.key == key)
        .map(|(i, c)| (Rc::clone(c), draw::channel_number(c, i)))
}
