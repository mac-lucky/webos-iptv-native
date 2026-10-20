//! The playback panel's state: its focus (the seek bar or one transport button) and the seek
//! burst. Taps of Left/Right on the seek bar move one target by the skip step; a held key moves
//! it faster; the burst is sent as ONE command [`COMMIT_MS`] after its last press, against the
//! programme it started in, whatever came on air meanwhile. On a time shift it also keeps how
//! far the archive itself put playback behind what the seeks asked for ([`Lag`]), which decides
//! when a forward seek is back at live.

use super::model::Context;
use super::PlayerSettings;
use crate::data::{Command, Stream};
use crate::time;
use iptv::model::{Programme, Timestamp};

/// A burst of seek presses is sent this long after its last press.
pub const COMMIT_MS: u64 = 700;
/// A seek never lands this close to the end of a finished programme.
pub const END_GUARD_SECS: i64 = 5;
/// A forward seek in the programme on air that lands this close to the live edge returns to
/// live, measured from where the edge would be without the time shift's [`Lag`]: the seeks
/// asked for no more than this behind live. Also the whole margin for a time shift the
/// transport did not see start (a pause on live).
pub const LIVE_SLACK_SECS: i64 = 10;
/// A held key hops this many skip steps; from [`FAST_AFTER`] hops on, [`FASTER`].
pub const FAST: i64 = 4;
pub const FASTER: i64 = 12;
pub const FAST_AFTER: u32 = 10;
/// A seek's [`Lag`] is read for at most this long after its press: by then its playback runs,
/// and a later reading would count the viewer's own time (the app in the background).
pub const LAG_READ_SECS: i64 = 30;

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

/// Seek presses not sent yet, in `P`: the programme on screen when the burst started (a
/// movie's burst has none).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Burst<P = ()> {
    /// Its positions count from its start, and the burst is sent against it even when the next
    /// one comes on air before it goes.
    pub programme: P,
    /// The position the burst started from.
    pub base: i64,
    pub delta: i64,
    pub commit_at_ms: u64,
    /// Held-key hops so far (the hop grows with them).
    pub hops: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transport {
    pub focus: Focus,
    pub burst: Option<Burst<Programme>>,
    pub lag: Lag,
}

impl Default for Transport {
    fn default() -> Self {
        Transport {
            focus: Focus::Seek,
            burst: None,
            lag: Lag::default(),
        }
    }
}

/// How far behind live the archive itself put a time shift, beyond what its seeks asked for.
/// Each seek opens the archive on a segment start at or before the moment it asks for (up to a
/// segment early) and shows its first frame a moment later (about 2 s on the C1). The next seek
/// counts from where playback really is, so a time shift of several seeks trails live by the
/// sum of theirs on top of what the viewer asked for. A pause is the viewer's: a seek's lag is
/// read until its playback runs (or pauses, or [`LAG_READ_SECS`] pass), and not after.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lag {
    /// The seeks of this time shift before the last one, seconds.
    before: i64,
    /// The last one, read again at every look until it settles.
    last: Option<Seeked>,
    /// When the burst being pressed started: the moment its position was read.
    pressed: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Seeked {
    /// The programme it played and the moment in it that it asked for.
    programme: Programme,
    asked: Timestamp,
    /// When the viewer pressed for it: ideally the asked moment plays then, and on from there.
    pressed: Timestamp,
    /// Its lag at the last reading.
    secs: i64,
    /// The position at the last reading, once playback was seen on the programme: from then on
    /// live means the time shift ended.
    position: Option<i64>,
    /// Playback ran (its position moved on between two readings), or a pause or the end of
    /// the reading time came first: the lag is what it is, and later readings would count a
    /// pause.
    settled: bool,
}

impl Lag {
    /// Seconds behind live that the viewer did not ask for.
    pub fn secs(&self) -> i64 {
        self.before + self.last.as_ref().map_or(0, |s| s.secs)
    }

    /// The last seek's lag is still being read.
    pub fn reading(&self) -> bool {
        self.last.as_ref().is_some_and(|s| !s.settled)
    }

    /// Read what plays: on live there is no time shift (unless a seek from live is still on its
    /// way); after a seek, how far playback trails the moment it asked for, counted on from the
    /// press.
    fn look(&mut self, ctx: &Context) {
        if !ctx.in_catchup() {
            if self.last.as_ref().is_none_or(|s| s.position.is_some()) {
                self.end();
            }
            return;
        }
        let Some(s) = self.last.as_mut().filter(|s| !s.settled) else {
            return;
        };
        if ctx.playback.paused || ctx.now - s.pressed > LAG_READ_SECS {
            s.settled = true;
        } else if ctx.playback.catchup.as_ref() == Some(&s.programme) {
            let position = ctx.playback.position_secs;
            s.secs = (s.asked + (ctx.now - s.pressed) - ctx.playing_at()).max(0);
            s.settled = s.position.is_some_and(|before| position > before);
            s.position = Some(position);
        }
    }

    /// The burst over `seeking` sent `commands`: a seek of the time shift starts a new reading,
    /// anything else ends the time shift.
    fn sent(&mut self, ctx: &Context, seeking: &Programme, commands: &[Command]) {
        let asked = match commands {
            [Command::SeekTo { position_secs }] => Some((seeking, *position_secs)),
            [Command::Play {
                stream:
                    Stream::Catchup {
                        programme,
                        offset_secs,
                        ..
                    },
                ..
            }] => Some((programme, *offset_secs)),
            _ => None,
        };
        match asked.filter(|(p, _)| live_follows(ctx, p)) {
            Some((p, offset)) => {
                self.before += self.last.take().map_or(0, |s| s.secs);
                self.last = Some(Seeked {
                    programme: p.clone(),
                    asked: p.start + offset,
                    pressed: self.pressed,
                    secs: 0,
                    position: None,
                    settled: false,
                });
            }
            None => self.end(),
        }
    }

    /// No time shift (any more): nothing behind live is the archive's.
    fn end(&mut self) {
        self.before = 0;
        self.last = None;
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
    /// for a long press or its repeats. Live has nothing ahead of it, except a step back not
    /// sent yet.
    pub fn seek(
        &mut self,
        ctx: &Context,
        dir: i64,
        held: bool,
        steps: &PlayerSettings,
        now_ms: u64,
    ) {
        let step_secs = steps.skip_secs(dir);
        if !can_seek(ctx) || (dir > 0 && !ctx.in_catchup() && self.burst.is_none()) {
            return;
        }
        let Some(current) = &ctx.current else {
            return;
        };
        self.lag.look(ctx);
        if self.burst.is_none() {
            self.lag.pressed = ctx.now;
        }
        let b = self.burst.get_or_insert_with(|| Burst {
            programme: current.clone(),
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
        let (lo, hi) = bounds(ctx, &b.programme);
        b.delta = (b.base + b.delta + dir * hop).clamp(lo, hi) - b.base;
        b.commit_at_ms = now_ms + COMMIT_MS;
    }

    /// The position the panel shows: the burst's target, else where playback is.
    pub fn shown_position(&self, ctx: &Context) -> i64 {
        match &self.burst {
            Some(b) => b.base + b.delta,
            None => ctx.position_secs(),
        }
    }

    /// Send the burst if its time has come (`force` sends it now). Called on every tick, it also
    /// reads the time shift's [`Lag`].
    pub fn commit(&mut self, ctx: &Context, now_ms: u64, force: bool) -> Vec<Command> {
        self.lag.look(ctx);
        let Some(b) = self.burst.take_if(|b| force || now_ms >= b.commit_at_ms) else {
            return Vec::new();
        };
        let slack = LIVE_SLACK_SECS + self.lag.secs();
        let commands = seek_commands(ctx, &b, slack);
        self.lag.sent(ctx, &b.programme, &commands);
        commands
    }

    /// A new panel: the focus on the seek bar and no burst. The time shift's lag stays.
    pub fn reset(&mut self) {
        self.focus = Focus::Seek;
        self.burst = None;
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
        let play = |p: &Programme| play_archive(ctx, p, 0);
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

/// Where a seek in `p` may land: up to the live edge while live follows it (where the last
/// forward hop returns to live), else inside it.
fn bounds(ctx: &Context, p: &Programme) -> (i64, i64) {
    if live_follows(ctx, p) {
        // Never below the start, should the clock be set back before it.
        (0, (ctx.now - p.start).max(0))
    } else {
        (0, (p.duration_secs() - END_GUARD_SECS).max(0))
    }
}

/// `p` is on air, or the programme after it is: what plays from `p`'s archive leads up to live,
/// as a time shift (Left on live, or a pause) does once its programme ended while it plays
/// behind (the app plays on into the next one at its end).
fn live_follows(ctx: &Context, p: &Programme) -> bool {
    p.stop > ctx.now || after(ctx, p).is_some()
}

/// The programme on air after `p` (the context holds it as the one on air, or as the one after
/// the catch-up programme).
fn after<'a>(ctx: &'a Context, p: &Programme) -> Option<&'a Programme> {
    [&ctx.current, &ctx.next]
        .into_iter()
        .flatten()
        .find(|n| n.start >= p.stop && n.is_on_air(ctx.now))
}

/// What the burst `b` asks of the app. Live stays live unless the burst went back. Where live
/// follows the programme, a burst brought forward to the live edge, or within `slack` of it,
/// goes live, and one past the end of a programme that has ended plays on in the next one from
/// the archive.
fn seek_commands(ctx: &Context, b: &Burst<Programme>, slack: i64) -> Vec<Command> {
    let p = &b.programme;
    let target = b.base + b.delta;
    if !ctx.in_catchup() && b.delta >= 0 {
        return Vec::new();
    }
    if live_follows(ctx, p) {
        let edge = ctx.now - p.start;
        if target >= edge || (b.delta > 0 && target >= edge - slack) {
            return vec![Command::ReturnToLive];
        }
        if let Some(next) = after(ctx, p).filter(|_| target >= p.duration_secs()) {
            return play_archive(ctx, next, (p.start + target - next.start).max(0));
        }
    }
    if ctx.playback.catchup.as_ref() == Some(p) {
        return vec![Command::SeekTo {
            position_secs: target,
        }];
    }
    play_archive(ctx, p, target)
}

/// Play `p` of the channel on screen from the archive, `offset_secs` in.
fn play_archive(ctx: &Context, p: &Programme, offset_secs: i64) -> Vec<Command> {
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
    /// The bar of the programme a burst seeks in, else of the one playing. A burst past the
    /// end of a programme that has ended (into the next one) shows the knob at the end.
    pub fn build(t: &Transport, ctx: &Context) -> Self {
        let seeking = t.burst.as_ref().map(|b| &b.programme);
        let Some(p) = seeking.or(ctx.current.as_ref()) else {
            return Readout::default();
        };
        let len = p.duration_secs().max(1);
        let pos = t.shown_position(ctx).min(p.duration_secs());
        let offset = match &t.burst {
            Some(b) => time::offset_label(b.delta),
            None if ctx.in_catchup() => time::offset_label(ctx.playing_at() - ctx.now),
            None => String::new(),
        };
        Readout {
            elapsed: time::clock_secs(pos),
            total: time::clock_secs(p.duration_secs()),
            offset,
            knob: (pos as f32 / len as f32).clamp(0.0, 1.0),
            aired: ((ctx.now - p.start) as f32 / len as f32).clamp(0.0, 1.0),
            paused: ctx.playback.paused,
        }
    }
}
