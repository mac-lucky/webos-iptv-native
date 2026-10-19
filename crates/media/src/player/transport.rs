//! The player's transport-pause contract: the one door every surface reaches the player's pause
//! through - the player's own transport keys and the detail page's trailer transport
//! ([`super::preview::transport`]) alike.

use std::sync::atomic::Ordering::Relaxed;

#[inline]
pub fn paused() -> bool {
    crate::player::TX.paused.load(Relaxed)
}

#[inline]
pub fn set_paused(v: bool) {
    crate::player::TX.commit_paused(v)
}

/// PURE: the paused value a transport request settles on. `Some(true)` is the remote's PLAY key,
/// `Some(false)` its PAUSE, and `None` the single PLAYPAUSE toggle - which is the only one of the
/// three that needs the current state at all. One function rather than a `match` per caller, so a
/// PLAY key cannot toggle on one surface and resume on another.
pub fn transport_target(play: Option<bool>, paused: bool) -> bool {
    match play {
        Some(true) => false,
        Some(false) => true,
        None => !paused,
    }
}

/// Ask the synchronized player clock to commit a user Pause/Resume. The player publishes the feed
/// gate at the same accepted native boundary.
pub fn set_transport_paused(pa: &mut crate::player::adapter::PlayerAdapter, value: bool) -> bool {
    if paused() == value {
        return true;
    }
    if value {
        crate::player::pause(pa)
    } else {
        crate::player::resume(pa)
    }
}

/// Resume if a seek landed while paused. Its stay-paused counterpart is the pump's
/// `settle_paused_seek`, which re-pauses a paused-seek preroll the pump itself opened.
pub fn resume_if_paused(pa: &mut crate::player::adapter::PlayerAdapter) {
    if paused() {
        set_transport_paused(pa, false);
    }
}

#[cfg(test)]
mod tests {
    use super::transport_target;

    #[test]
    fn the_three_keys_settle_where_they_say() {
        assert!(!transport_target(Some(true), true), "PLAY resumes");
        assert!(!transport_target(Some(true), false), "PLAY on a playing stream stays playing");
        assert!(transport_target(Some(false), false), "PAUSE pauses");
        assert!(transport_target(Some(false), true));
        assert!(transport_target(None, false), "the toggle pauses a playing stream");
        assert!(!transport_target(None, true), "and resumes a paused one");
    }
}
