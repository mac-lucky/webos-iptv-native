//! The playback panel's state: its focus (the seek bar or one transport button) and the seek
//! burst. Taps of Left/Right on the seek bar move one target by the skip step; a held key moves
//! it faster; the burst is sent as ONE command [`COMMIT_MS`] after its last press.

use super::model::Context;
use super::PlayerSettings;
use crate::data::{Command, Stream};
use crate::time;

/// A burst of seek presses is sent this long after its last press.
pub const COMMIT_MS: u64 = 700;
/// A seek never lands this close to the end of a finished programme.
pub const END_GUARD_SECS: i64 = 5;
/// A held key hops this many skip steps; from [`FAST_AFTER`] hops on, [`FASTER`].
pub const FAST: i64 = 4;
pub const FASTER: i64 = 12;
pub const FAST_AFTER: u32 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Prev,
    Rewind,
    PlayPause,
    FastForward,
    Next,
    Restart,
    Live,
}

/// The transport row, left to right.
pub const BUTTONS: [Button; 7] = [
    Button::Prev,
    Button::Rewind,
    Button::PlayPause,
    Button::FastForward,
    Button::Next,
    Button::Restart,
    Button::Live,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Seek,
    Button(Button),
}

/// Seek presses not sent yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Burst {
    /// The position the burst started from.
    pub base: i64,
    pub delta: i64,
    pub commit_at_ms: u64,
    /// Held-key hops so far (the hop grows with them).
    pub hops: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transport {
    pub focus: Focus,
    pub burst: Option<Burst>,
}

impl Default for Transport {
    fn default() -> Self {
        Transport {
            focus: Focus::Seek,
            burst: None,
        }
    }
}

/// Whether `b` does anything for what is playing.
pub fn enabled(b: Button, ctx: &Context) -> bool {
    let catchup = ctx.in_catchup();
    let current_in_archive = ctx.current.as_ref().is_some_and(|p| ctx.in_archive(p));
    match b {
        Button::Prev => ctx.previous.as_ref().is_some_and(|p| ctx.in_archive(p)),
        Button::Rewind | Button::Restart => current_in_archive,
        Button::PlayPause | Button::FastForward | Button::Live => catchup,
        Button::Next => catchup && ctx.next.as_ref().is_some_and(|p| ctx.in_archive(p)),
    }
}

/// The panel can seek at all: catch-up plays, or the live programme can be rewound.
pub fn can_seek(ctx: &Context) -> bool {
    enabled(Button::Rewind, ctx)
}

impl Transport {
    /// Move the focus along the button row by `dir` (-1, +1), skipping disabled buttons.
    pub fn step_button(&mut self, dir: i32, ctx: &Context) {
        let Focus::Button(b) = self.focus else {
            return;
        };
        let mut i = BUTTONS.iter().position(|x| *x == b).unwrap_or(0) as i32;
        loop {
            i += dir;
            let Some(next) = usize::try_from(i).ok().and_then(|i| BUTTONS.get(i)) else {
                return;
            };
            if enabled(*next, ctx) {
                self.focus = Focus::Button(*next);
                return;
            }
        }
    }

    /// Down from the seek bar: the play/pause button, else the first that works.
    pub fn enter_buttons(&mut self, ctx: &Context) -> bool {
        let pick = std::iter::once(Button::PlayPause)
            .chain(BUTTONS)
            .find(|b| enabled(*b, ctx));
        match pick {
            Some(b) => {
                self.focus = Focus::Button(b);
                true
            }
            None => false,
        }
    }

    /// One seek press: `dir` -1 back, +1 forward, by the skip step for that direction; `held`
    /// for a long press or its repeats.
    pub fn seek(
        &mut self,
        ctx: &Context,
        dir: i64,
        held: bool,
        steps: &PlayerSettings,
        now_ms: u64,
    ) {
        let step_secs = steps.skip_secs(dir);
        if !can_seek(ctx) || (dir > 0 && !ctx.in_catchup()) {
            return;
        }
        let b = self.burst.get_or_insert(Burst {
            base: ctx.position_secs(),
            delta: 0,
            commit_at_ms: 0,
            hops: 0,
        });
        let hop = if held {
            b.hops += 1;
            step_secs * if b.hops > FAST_AFTER { FASTER } else { FAST }
        } else {
            step_secs
        };
        let (lo, hi) = bounds(ctx);
        b.delta = (b.base + b.delta + dir * hop).clamp(lo, hi) - b.base;
        b.commit_at_ms = now_ms + COMMIT_MS;
    }

    /// The position the panel shows: the burst's target, else where playback is.
    pub fn shown_position(&self, ctx: &Context) -> i64 {
        match self.burst {
            Some(b) => b.base + b.delta,
            None => ctx.position_secs(),
        }
    }

    /// Send the burst if its time has come (`force` sends it now).
    pub fn commit(&mut self, ctx: &Context, now_ms: u64, force: bool) -> Vec<Command> {
        match self.burst {
            Some(b) if force || now_ms >= b.commit_at_ms => {
                self.burst = None;
                seek_commands(ctx, b.base + b.delta)
            }
            _ => Vec::new(),
        }
    }

    pub fn press(
        &mut self,
        b: Button,
        ctx: &Context,
        steps: &PlayerSettings,
        now_ms: u64,
    ) -> Vec<Command> {
        if !enabled(b, ctx) {
            return Vec::new();
        }
        let play = |p: &iptv::model::Programme| play_archive(ctx, p, 0);
        match b {
            Button::Prev => ctx.previous.as_ref().map_or_else(Vec::new, play),
            Button::Next => ctx.next.as_ref().map_or_else(Vec::new, play),
            Button::Rewind | Button::FastForward => {
                let dir = if b == Button::Rewind { -1 } else { 1 };
                self.seek(ctx, dir, false, steps, now_ms);
                Vec::new()
            }
            Button::PlayPause => vec![Command::TogglePause],
            Button::Restart if ctx.in_catchup() => vec![Command::SeekTo { position_secs: 0 }],
            Button::Restart => ctx.current.as_ref().map_or_else(Vec::new, play),
            Button::Live => vec![Command::ReturnToLive],
        }
    }
}

/// Where a seek may land inside the current programme.
fn bounds(ctx: &Context) -> (i64, i64) {
    let Some(p) = &ctx.current else {
        return (0, 0);
    };
    if p.stop > ctx.now {
        // On air: up to the live edge, which is where the last forward hop returns to live.
        (0, ctx.live_edge_secs())
    } else {
        (0, (p.duration_secs() - END_GUARD_SECS).max(0))
    }
}

/// What a seek to `target` asks of the app.
fn seek_commands(ctx: &Context, target: i64) -> Vec<Command> {
    let Some(p) = &ctx.current else {
        return Vec::new();
    };
    let on_air = p.stop > ctx.now;
    if on_air && target >= ctx.live_edge_secs() {
        return if ctx.in_catchup() {
            vec![Command::ReturnToLive]
        } else {
            Vec::new()
        };
    }
    if ctx.in_catchup() {
        return vec![Command::SeekTo {
            position_secs: target,
        }];
    }
    play_archive(ctx, p, target)
}

/// Play `p` of the channel on screen from the archive, `offset_secs` in.
fn play_archive(ctx: &Context, p: &iptv::model::Programme, offset_secs: i64) -> Vec<Command> {
    ctx.key()
        .map(|channel| Command::Play {
            stream: Stream::Catchup {
                channel,
                programme: p.clone(),
                offset_secs,
            },
            group: None,
        })
        .into_iter()
        .collect()
}

/// The seek bar's read-out.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Readout {
    /// `23:13`
    pub elapsed: String,
    /// `40:00`
    pub total: String,
    /// The burst's offset (`[+02:00]`), else how far behind live catch-up plays (`[-2:30:47]`).
    pub offset: String,
    /// Where the knob sits, `0.0..=1.0` of the programme.
    pub knob: f32,
    /// How much of the programme has aired, `0.0..=1.0` (the bar beyond it is not seekable).
    pub aired: f32,
    pub paused: bool,
}

impl Readout {
    pub fn build(t: &Transport, ctx: &Context) -> Self {
        let Some(p) = &ctx.current else {
            return Readout::default();
        };
        let len = p.duration_secs().max(1);
        let pos = t.shown_position(ctx);
        let offset = match t.burst {
            Some(b) => time::offset_label(b.delta),
            None if ctx.in_catchup() => time::offset_label(ctx.playing_at() - ctx.now),
            None => String::new(),
        };
        Readout {
            elapsed: time::clock_secs(pos),
            total: time::clock_secs(p.duration_secs()),
            offset,
            knob: (pos as f32 / len as f32).clamp(0.0, 1.0),
            aired: (ctx.live_edge_secs() as f32 / len as f32).clamp(0.0, 1.0),
            paused: ctx.playback.paused,
        }
    }
}
