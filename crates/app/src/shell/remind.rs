//! Programme reminders on the loop's clock: when the next one starts is a wake like any other,
//! so nothing redraws while none is due.
//!
//! The time is the store's, which on the TV is the wall clock. The simulator's synthetic data
//! has a clock that stands still, so between two readings that differ the loop's clock carries
//! the last one forward: on the TV that changes nothing, in the simulator a reminder comes when
//! its start is reached in real time.

use iptv::model::Timestamp;
use tv_screens::reminders::{self, Reminder};

#[derive(Default)]
pub struct Reminders {
    /// In start order.
    list: Vec<Reminder>,
    /// The loop's clock (ms) when the store's clock last read a new second, and that second.
    anchor: Option<(u64, Timestamp)>,
}

impl Reminders {
    /// The reminders set (the setting was read again).
    pub fn set(&mut self, list: Vec<Reminder>) {
        self.list = list;
    }

    pub fn list(&self) -> &[Reminder] {
        &self.list
    }

    /// The store's clock read `store_now` at `now_ms`.
    pub fn observe(&mut self, now_ms: u64, store_now: Timestamp) {
        if self.anchor.is_none_or(|(_, t)| t != store_now) {
            self.anchor = Some((now_ms, store_now));
        }
    }

    /// The store's time at `now_ms`, in milliseconds.
    fn now(&self, now_ms: u64) -> Option<i64> {
        let (at, t) = self.anchor?;
        Some(t.saturating_mul(1_000) + now_ms.saturating_sub(at) as i64)
    }

    /// When the first reminder is due, on the loop's clock.
    pub fn next_wake(&self, now_ms: u64) -> Option<u64> {
        let first = self.list.first()?;
        let left = first
            .start
            .saturating_mul(1_000)
            .saturating_sub(self.now(now_ms)?);
        Some(now_ms.saturating_add(left.max(0) as u64))
    }

    /// The reminders whose start has come at `now_ms`, taken off the list (those long past
    /// are dropped unfired, [`reminders::due`]); `None` while none has.
    pub fn take_due(&mut self, now_ms: u64) -> Option<Vec<Reminder>> {
        let now = self.now(now_ms)?;
        if self
            .list
            .first()
            .is_none_or(|r| r.start.saturating_mul(1_000) > now)
        {
            return None;
        }
        let (fire, ahead) = reminders::due(&self.list, now.div_euclid(1_000));
        self.list = ahead;
        Some(fire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iptv::model::ChannelKey;

    const T: Timestamp = 1_700_000_000;

    fn at(start: Timestamp) -> Reminder {
        Reminder {
            channel: ChannelKey(1),
            start,
            title: "x".into(),
        }
    }

    #[test]
    fn a_still_clock_is_carried_forward_by_the_loop() {
        let mut r = Reminders::default();
        r.set(vec![at(T + 60), at(T + 120)]);
        assert_eq!(r.next_wake(0), None, "no clock read yet");
        r.observe(1_000, T);
        assert_eq!(r.next_wake(1_000), Some(61_000));
        r.observe(30_000, T);
        assert_eq!(
            r.next_wake(30_000),
            Some(61_000),
            "the store's clock stood still"
        );
        assert_eq!(r.take_due(60_999), None);
        assert_eq!(r.take_due(61_000), Some(vec![at(T + 60)]));
        assert_eq!(r.list(), [at(T + 120)]);
        assert_eq!(r.next_wake(61_000), Some(121_000));
    }

    #[test]
    fn a_moving_clock_is_read_as_it_is() {
        let mut r = Reminders::default();
        r.set(vec![at(T + 10)]);
        r.observe(0, T);
        // The wall clock jumped ahead (the TV set its time): the reminder is due at once.
        r.observe(500, T + 10);
        assert_eq!(r.next_wake(500), Some(500));
        assert_eq!(r.take_due(500), Some(vec![at(T + 10)]));
        assert_eq!(r.next_wake(500), None);
    }

    #[test]
    fn a_start_at_the_ends_of_the_clock_does_not_overflow() {
        let mut r = Reminders::default();
        r.set(vec![at(i64::MIN), at(i64::MAX)]);
        r.observe(0, T);
        assert_eq!(r.next_wake(0), Some(0), "the first is long past: due now");
        assert_eq!(r.take_due(0), Some(vec![]), "and dropped unfired");
        let never = (i64::MAX - T * 1_000) as u64;
        assert_eq!(r.next_wake(5), Some(never), "the last never comes");
        assert_eq!(r.take_due(5), None);
    }

    #[test]
    fn long_past_reminders_go_unfired() {
        let mut r = Reminders::default();
        r.set(vec![at(T - 3_600), at(T - 30), at(T + 5)]);
        r.observe(0, T);
        assert_eq!(r.take_due(0), Some(vec![at(T - 30)]));
        assert_eq!(r.list(), [at(T + 5)]);
    }
}
