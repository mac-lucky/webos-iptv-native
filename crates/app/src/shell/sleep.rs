//! The sleep timer the player menu sets: a minute before it runs out the viewer is warned, then
//! playback stops and the decoder is released. One at a time, on the loop's clock, whatever plays
//! meanwhile; nothing of it is stored.

/// The warning comes this long before the stop.
pub const WARN_MS: u64 = 60_000;

/// What a timer has due.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Due {
    /// A minute left: tell the viewer.
    Warn,
    /// Time is up: stop playback.
    Stop,
}

#[derive(Debug)]
pub struct SleepTimer {
    /// When playback stops, on the loop's clock (ms).
    at: u64,
    warned: bool,
}

impl SleepTimer {
    /// A timer of `minutes` from `now`; `None` for 0, which is off.
    pub fn start(now: u64, minutes: u32) -> Option<Self> {
        (minutes > 0).then(|| SleepTimer {
            at: now + u64::from(minutes) * 60_000,
            warned: false,
        })
    }

    /// When [`tick`](Self::tick) next has something to do.
    pub fn next_wake(&self) -> u64 {
        if self.warned {
            self.at
        } else {
            self.at.saturating_sub(WARN_MS)
        }
    }

    /// The time left at `now`, whole seconds rounded up.
    pub fn left_secs(&self, now: u64) -> u32 {
        u32::try_from(self.at.saturating_sub(now).div_ceil(1_000)).unwrap_or(u32::MAX)
    }

    /// What is due at `now`: the warning once, then the stop.
    pub fn tick(&mut self, now: u64) -> Option<Due> {
        if now >= self.at {
            return Some(Due::Stop);
        }
        if !self.warned && now + WARN_MS >= self.at {
            self.warned = true;
            return Some(Due::Warn);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_is_no_timer() {
        assert!(SleepTimer::start(5, 0).is_none());
    }

    #[test]
    fn it_warns_a_minute_before_once_then_stops() {
        let mut t = SleepTimer::start(1_000, 15).unwrap();
        let at = 1_000 + 15 * 60_000;
        assert_eq!(t.next_wake(), at - WARN_MS);
        assert_eq!(t.left_secs(1_000), 15 * 60);
        assert_eq!(t.tick(at - WARN_MS - 1), None);
        assert_eq!(t.tick(at - WARN_MS), Some(Due::Warn));
        assert_eq!(t.tick(at - 1), None, "warned once");
        assert_eq!(t.next_wake(), at);
        assert_eq!(t.left_secs(at - 1), 1, "rounded up");
        assert_eq!(t.tick(at), Some(Due::Stop));
        assert_eq!(t.left_secs(at + 5), 0);
    }

    #[test]
    fn a_late_tick_stops_without_the_warning() {
        let mut t = SleepTimer::start(0, 30).unwrap();
        assert_eq!(t.tick(31 * 60_000), Some(Due::Stop));
    }
}
