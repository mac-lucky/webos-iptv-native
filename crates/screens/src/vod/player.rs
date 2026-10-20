//! The player of a movie or an episode, over its section while it plays full screen: the title,
//! the seek bar and the play/pause state, up for the panel timeout after a key.
//!
//! The seek bar works as the catch-up playback panel's ([`crate::player::transport`]): taps of
//! Left/Right (or rewind/fast-forward) move one target by the skip step, a held key moves it
//! faster, and the burst is sent as one [`Command::SeekTo`] [`COMMIT_MS`] after its last press,
//! never closer to the end than [`END_GUARD_SECS`]. OK pauses or plays (or sends a burst at
//! once), Back hides the panel and then stops: [`Command::StopVod`], which keeps the position,
//! and the section comes back. An item that played to its end stops by itself.

use iptv::model::ChannelKey;
use tv_machine::machine::Measure;

use crate::data::{Command, Output, Playback, TvData};
use crate::draw::Piece;
use crate::input::{Hold, Key, Press};
use crate::player::transport::{
    Burst, Readout, COMMIT_MS, END_GUARD_SECS, FAST, FASTER, FAST_AFTER,
};
use crate::player::PlayerSettings;
use crate::time;

/// What a key or a tick did to the player.
#[derive(Debug, Default, PartialEq)]
pub struct Step {
    pub out: Output,
    /// The item stopped: the section is in front again.
    pub done: bool,
}

pub struct VodPlayer {
    settings: PlayerSettings,
    /// The item asked for: once playback is anything else, it was stopped or never started.
    item: ChannelKey,
    title: String,
    playback: Playback,
    /// The panel is up until this moment; `None` while it is hidden.
    hide_at_ms: Option<u64>,
    burst: Option<Burst>,
    readout: Readout,
    generation: u64,
}

impl VodPlayer {
    /// `item`, titled `title`, just asked for ([`Command::PlayVod`]): the panel shows while it
    /// starts.
    pub fn new(item: ChannelKey, title: String, settings: PlayerSettings, now_ms: u64) -> Self {
        let mut p = VodPlayer {
            settings,
            item,
            title,
            playback: Playback::default(),
            hide_at_ms: None,
            burst: None,
            readout: Readout::default(),
            generation: 0,
        };
        p.show(now_ms);
        p
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn shown(&self) -> bool {
        self.hide_at_ms.is_some()
    }

    pub fn readout(&self) -> &Readout {
        &self.readout
    }

    pub fn burst(&self) -> Option<Burst> {
        self.burst
    }

    /// Moves on every change worth a frame.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn duration(&self) -> i64 {
        self.playback.vod.as_ref().map_or(0, |v| v.duration_secs)
    }

    fn show(&mut self, now_ms: u64) {
        self.hide_at_ms = Some(now_ms + self.settings.panel_timeout_ms);
        self.relayout();
    }

    fn relayout(&mut self) {
        self.readout = self.build_readout();
        self.generation += 1;
    }

    fn build_readout(&self) -> Readout {
        let len = self.duration();
        let pos = self.shown_position();
        Readout {
            elapsed: time::clock_secs(pos),
            total: if len > 0 {
                time::clock_secs(len)
            } else {
                String::new()
            },
            offset: self
                .burst
                .map(|b| time::offset_label(b.delta))
                .unwrap_or_default(),
            knob: if len > 0 {
                (pos as f32 / len as f32).clamp(0.0, 1.0)
            } else {
                0.0
            },
            aired: 1.0,
            paused: self.playback.paused,
        }
    }

    /// The position the bar shows: the burst's target, else where playback is.
    pub fn shown_position(&self) -> i64 {
        match self.burst {
            Some(b) => b.base + b.delta,
            None => self.playback.position_secs,
        }
    }

    /// One seek press, as the playback panel's: `dir` -1 back, +1 forward. Nothing seeks
    /// before the player knows the length.
    fn seek(&mut self, dir: i64, held: bool, now_ms: u64) {
        let len = self.duration();
        if len <= 0 {
            return;
        }
        let step = self.settings.skip_secs(dir);
        let position = self.playback.position_secs;
        let b = self.burst.get_or_insert(Burst {
            programme: (),
            base: position,
            delta: 0,
            commit_at_ms: 0,
            hops: 0,
        });
        let hop = if held {
            b.hops += 1;
            step * if b.hops > FAST_AFTER { FASTER } else { FAST }
        } else {
            step
        };
        let hi = (len - END_GUARD_SECS).max(0);
        b.delta = (b.base + b.delta + dir * hop).clamp(0, hi) - b.base;
        b.commit_at_ms = now_ms + COMMIT_MS;
    }

    /// The burst as a seek, if its time has come (`force`: now).
    fn commit(&mut self, now_ms: u64, force: bool) -> Option<Command> {
        let b = self.burst?;
        if !force && now_ms < b.commit_at_ms {
            return None;
        }
        self.burst = None;
        Some(Command::SeekTo {
            position_secs: b.base + b.delta,
        })
    }

    pub fn hold(&self, key: Key) -> Hold {
        match key {
            Key::Left | Key::Right | Key::Rewind | Key::FastForward => Hold::Repeat,
            _ => Hold::Once,
        }
    }

    pub fn key(&mut self, key: Key, press: Press, now_ms: u64) -> Step {
        let held = press != Press::Short;
        if held && !matches!(key, Key::Left | Key::Right | Key::Rewind | Key::FastForward) {
            return Step::default();
        }
        let mut out = Output::redraw();
        match key {
            Key::Left | Key::Rewind => self.seek(-1, held, now_ms),
            Key::Right | Key::FastForward => self.seek(1, held, now_ms),
            Key::Ok if !self.shown() => {}
            Key::Ok => match self.commit(now_ms, true) {
                Some(seek) => out.commands.push(seek),
                None => out.commands.push(Command::TogglePause),
            },
            Key::PlayPause => out.commands.push(Command::TogglePause),
            Key::Play if self.playback.paused => out.commands.push(Command::TogglePause),
            Key::Pause if !self.playback.paused => out.commands.push(Command::TogglePause),
            Key::Back if self.shown() => {
                // Back drops a burst not sent yet; with none, it hides the panel.
                if self.burst.take().is_none() {
                    self.hide_at_ms = None;
                }
                self.relayout();
                return Step { out, done: false };
            }
            Key::Back => return self.stop(),
            Key::Up | Key::Down => {}
            _ => return Step::default(),
        }
        self.show(now_ms);
        Step { out, done: false }
    }

    /// Stop the item: the position is kept, the section comes back.
    fn stop(&mut self) -> Step {
        let mut out = Output::command(Command::StopVod);
        out.redraw = true;
        Step { out, done: true }
    }

    /// Send a finished burst, hide a timed-out panel, follow what playback reports; an item that
    /// ended, or that is not what plays (it was replaced, or never started), is done.
    pub fn tick(&mut self, data: &dyn TvData, now_ms: u64) -> Step {
        let playback = data.playback();
        let Some(vod) = playback.vod.as_ref().filter(|v| v.item == self.item) else {
            return Step {
                out: Output::redraw(),
                done: true,
            };
        };
        if vod.ended {
            return self.stop();
        }
        let mut out = Output::default();
        if let Some(seek) = self.commit(now_ms, false) {
            out.commands.push(seek);
            out.redraw = true;
        }
        if self.hide_at_ms.is_some_and(|t| now_ms >= t) {
            if let Some(seek) = self.commit(now_ms, true) {
                out.commands.push(seek);
            }
            self.hide_at_ms = None;
            out.redraw = true;
        }
        if playback != self.playback {
            let shown_moved = self.shown()
                && (playback.position_secs != self.playback.position_secs
                    || playback.paused != self.playback.paused
                    || playback.vod != self.playback.vod);
            self.playback = playback;
            out.redraw |= shown_moved;
        }
        if out.redraw {
            self.relayout();
        }
        Step { out, done: false }
    }

    /// The panel's timeout and a pending seek; playback changes tick the screens by themselves.
    pub fn next_wake(&self) -> Option<u64> {
        [self.hide_at_ms, self.burst.map(|b| b.commit_at_ms)]
            .into_iter()
            .flatten()
            .min()
    }

    /// Lay the panel out into `out` (nothing while it is hidden: the video alone).
    pub fn build(&self, m: &dyn Measure, out: &mut Vec<Piece>) {
        if self.shown() {
            super::view::player(self, m, out);
        }
    }
}
