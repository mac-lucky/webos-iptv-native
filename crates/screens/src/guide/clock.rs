//! Half-hour arithmetic for the guide's time window. Guide times are Unix seconds (UTC); the
//! window snaps to local half hours through the fixed offset the app supplies (local minus UTC,
//! in seconds). The strings and the rest of the day arithmetic are [`crate::time`]'s.

use iptv::model::Timestamp;

use crate::time::MINUTE;

pub const HALF_HOUR: i64 = 30 * MINUTE;

/// `t` rounded down to a local half hour.
pub fn floor_half_hour(t: Timestamp, offset: i32) -> Timestamp {
    let local = t + i64::from(offset);
    local - local.rem_euclid(HALF_HOUR) - i64::from(offset)
}

/// `t` rounded up to a local half hour.
pub fn ceil_half_hour(t: Timestamp, offset: i32) -> Timestamp {
    let floor = floor_half_hour(t, offset);
    if floor == t {
        t
    } else {
        floor + HALF_HOUR
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::hhmm;

    // 2026-10-19 13:11:00 UTC.
    const T: Timestamp = 1_792_415_460;

    #[test]
    fn half_hours_follow_the_local_clock() {
        assert_eq!(hhmm(floor_half_hour(T, 7200), 7200), "15:00");
        assert_eq!(hhmm(ceil_half_hour(T, 7200), 7200), "15:30");
        assert_eq!(
            ceil_half_hour(floor_half_hour(T, 0), 0),
            floor_half_hour(T, 0)
        );
        // A half-hour zone keeps UTC half hours; a quarter-hour zone moves them.
        let india = 5 * 3600 + 1800;
        assert_eq!(hhmm(floor_half_hour(T, india), india), "18:30");
        assert_eq!(floor_half_hour(T, india) % HALF_HOUR, 0);
        let nepal = 5 * 3600 + 2700;
        assert_eq!(hhmm(floor_half_hour(T, nepal), nepal), "18:30");
        assert_eq!(floor_half_hour(T, nepal) % HALF_HOUR, 15 * MINUTE);
    }
}
