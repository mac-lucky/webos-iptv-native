//! **The Player machine** - the playback state the application owns.
//!
//! Everything about a playback that is a DECISION rather than a resource lives here, in one value
//! with one owner: the [`PlaybackSession`] (what is playing and the decisions frozen from it), the
//! sandbox repair attempt, and whether the hardware video plane is bound. A `&mut Player` is what a
//! function needs to change any of it, so "who may write this" is a signature, not a comment.
//!
//! It is a MACHINE, not an adapter: no OS handle, no FFI, no thread. The Starfish/ACB session
//! object and its `MainThread` token are the ADAPTER's
//! ([`crate::player::adapter::PlayerAdapter`]), and the two are deliberately separate so a screen
//! or a pump can be handed the decisions without being handed the pipeline.
//!
//! # The tick
//!
//! [`Player::now_ms`] is this frame's millisecond stamp, set ONCE per iteration by the loop from
//! the same frame time every other phase of the frame reads, so two stamps of "now" in one frame
//! cannot disagree.

use crate::session::PlaybackSession;

/// The playback state `App` owns (§2.2).
pub struct Player {
    /// One accepted sandbox repair for the entire app lifetime, including screen recreation.
    pub repair: RepairAttempt,
    /// What is playing and the decisions frozen from it (`crate::session`).
    pub session: PlaybackSession,
    /// This frame's millisecond stamp - see the module doc. Zero before the first frame.
    pub now_ms: u32,
    /// **Is the hardware video plane bound to our sink?** 
    ///
    /// A DECISION of this machine, with defined transitions, not an expression the frame
    /// re-derives: it is set when the ACB bind transaction has actually reached the sink and
    /// cleared only when the sink is confirmed unbound. Read it; never recompute it. Through
    /// phase 8 the same question was asked as `matches!(app.route, Route::Player)` at three
    /// separate places in the loop, which is a claim about which SCREEN is up - true a second
    /// before the plane binds and a second after it unbinds, when the compositor has an ordinary
    /// UI surface and the gate should be treating it like one.
    ///
    /// Its EDGES are the only source of [`tv_machine::present::PresentEvent::VideoPlane`]; see
    /// [`Player::set_video_plane_bound`].
    pub video_plane_bound: bool,
    /// Last frame's player-page `clock_fingerprint` (the application's) - the machine's
    /// memory of what the player page's clock-driven picture WAS, so a change can be a frame.
    clock_fp: u64,
}

impl Player {
    pub fn new() -> Self {
        Self {
            repair: RepairAttempt::new(),
            session: PlaybackSession::IDLE,
            now_ms: 0,
            video_plane_bound: false,
            clock_fp: 0,
        }
    }

    /// **The player page's clock-driven motion report** (spec §8.3). `true` = this frame is owed a
    /// present. See `PlayerScreen::clock_fingerprint` for what the value is and why the comparison
    /// lives here rather than as an `invalidate` inside each animator.
    pub fn note_clock(&mut self, fp: u64) -> bool {
        let changed = self.clock_fp != fp;
        self.clock_fp = fp;
        changed
    }

    /// **The one writer of [`Player::video_plane_bound`], and the one source of
    /// `PresentEvent::VideoPlane`** (spec §16 risk 10).
    ///
    /// Publishes on the EDGES only. That is not an optimisation: the present gate's video-plane
    /// input is a piece of STATE with one owner, and a level written every frame would be a second
    /// formula term feeding it - exactly the risk the spec names, where the gate and the machine
    /// come to disagree about which frame the plane went away on and the false edge lands on a
    /// frame that was never presented.
    /// `Some(bound)` when this was an EDGE - the loop forwards that, and only that, to the two
    /// `tv_machine::present::Present` machines it owns beside the live gate.
    pub fn set_video_plane_bound(&mut self, bound: bool) -> Option<bool> {
        if self.video_plane_bound == bound {
            return None;
        }
        self.video_plane_bound = bound;
        // ONE line per edge, in the event log. The bit decides the present gate, the opaque
        // region, the capture skip and whether a frame may sample the framebuffer at all - and
        // every one of those is invisible from a log that does not say when the plane arrived.
        tv_base::eventlog::log(if bound {
            "videoplane: BOUND — the gate presents unconditionally and no frame may snapshot"
        } else {
            "videoplane: unbound — ordinary idle rules from here"
        });
        tv_machine::idle::note(tv_machine::present::PresentEvent::VideoPlane(bound));
        // The FALSE edge has to reach the panel, and the frame it lands on is very often one the
        // gate would otherwise skip - the picture is gone and nothing is animating (spec §3.3
        // step 9). `invalidate` is what makes that frame present, so the opaque region is really
        // cleared and the UI surface really goes back to being blended.
        if !bound {
            tv_machine::idle::invalidate();
        }
        Some(bound)
    }

    /// The loop's one write of the frame tick, at the top of the iteration.
    pub fn set_now(&mut self, now_ms: u32) {
        self.now_ms = now_ms;
    }
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

/// Logical repair authority; resource handles live in PlayerAdapter. No reset-on-playback API.
pub struct RepairAttempt {
    state: tv_platform::tv::sandbox::State,
}
impl RepairAttempt {
    pub const fn new() -> Self { Self { state: tv_platform::tv::sandbox::State::Idle } }
    pub fn state(&self) -> tv_platform::tv::sandbox::State { self.state }
    pub fn begin(&mut self, supported: bool) -> Option<u64> {
        use tv_platform::tv::sandbox::{State, Failure};
        if self.state != State::Idle { return None; }
        if !supported { self.state = State::Failed(Failure::Unsupported); return None; }
        self.state = State::Running;
        Some(1)
    }
    /// The single issued token can land only once. Wrong/duplicate completions cannot rewrite it.
    pub fn complete(&mut self, token: u64, result: Result<(), tv_platform::tv::sandbox::Failure>) -> bool {
        use tv_platform::tv::sandbox::State;
        if token != 1 || self.state != State::Running { return false; }
        self.state = match result { Ok(()) => State::Repaired, Err(e) => State::Failed(e) };
        true
    }
}

#[cfg(test)]
mod repair_tests {
    use super::*;
    use tv_platform::tv::sandbox::Failure;
    // `repair_survives_screen_and_session_recreation_and_rejects_stale_completions` builds a
    // `PlayerScreen`, which `player` may not name: it lives in `screens::player`'s
    // `repair_confirmation_tests`, beside the screen it recreates.
    #[test]
    fn every_terminal_result_spends_the_process_attempt() {
        for result in [Ok(()), Err(Failure::StartFailed), Err(Failure::HbcUnavailable), Err(Failure::NotRoot), Err(Failure::CommandFailed), Err(Failure::Timeout), Err(Failure::Unreadable)] {
            let mut owner = RepairAttempt::new();
            let token = owner.begin(true).unwrap();
            assert!(owner.complete(token, result));
            assert_eq!(owner.begin(true), None);
        }
        let mut owner = RepairAttempt::new();
        assert_eq!(owner.begin(false), None);
        assert_eq!(owner.begin(true), None);
    }
}
