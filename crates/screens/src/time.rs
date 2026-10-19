//! Time math and the time strings every screen shows. Pure functions of their arguments: every
//! "now" is passed in, so the tests pin it. Timestamps are UTC seconds; local time is UTC plus the
//! offset the app reads from the system ([`TvData::utc_offset_secs`]).
//!
//! [`TvData::utc_offset_secs`]: crate::data::TvData::utc_offset_secs

use iptv::model::{Programme, Timestamp};
use tv_base::time::civil_from_days;
use tv_platform::i18n::msg;

/// The dash TiviMate puts between a start and an end time.
pub const DASH: &str = " \u{2014} ";

pub const MINUTE: i64 = 60;
pub const HOUR: i64 = 60 * MINUTE;
pub const DAY: i64 = 24 * HOUR;
const WEEKDAYS: [fn() -> &'static str; 7] = [
    msg::weekday_mon,
    msg::weekday_tue,
    msg::weekday_wed,
    msg::weekday_thu,
    msg::weekday_fri,
    msg::weekday_sat,
    msg::weekday_sun,
];
const MONTHS: [fn() -> &'static str; 12] = [
    msg::month_jan,
    msg::month_feb,
    msg::month_mar,
    msg::month_apr,
    msg::month_may,
    msg::month_jun,
    msg::month_jul,
    msg::month_aug,
    msg::month_sep,
    msg::month_oct,
    msg::month_nov,
    msg::month_dec,
];

/// Local wall-clock seconds for a UTC timestamp.
fn local(ts: Timestamp, utc_offset_secs: i32) -> i64 {
    ts + i64::from(utc_offset_secs)
}

/// `15:12`.
pub fn hhmm(ts: Timestamp, utc_offset_secs: i32) -> String {
    let s = local(ts, utc_offset_secs).rem_euclid(DAY);
    format!("{:02}:{:02}", s / 3600, s % 3600 / 60)
}

/// `15:12 - 15:19` with TiviMate's dash.
pub fn span(start: Timestamp, stop: Timestamp, utc_offset_secs: i32) -> String {
    format!(
        "{}{DASH}{}",
        hhmm(start, utc_offset_secs),
        hhmm(stop, utc_offset_secs)
    )
}

/// The short local weekday: `Tue`.
pub fn weekday(ts: Timestamp, utc_offset_secs: i32) -> &'static str {
    // 1970-01-01 was a Thursday.
    WEEKDAYS[(local_day(ts, utc_offset_secs) + 3).rem_euclid(7) as usize]()
}

/// `Today`, `Yesterday`, `Tomorrow`, otherwise the date ([`day`]).
pub fn day_label(ts: Timestamp, now: Timestamp, utc_offset_secs: i32) -> String {
    match day_offset(ts, now, utc_offset_secs) {
        0 => msg::common_today().to_owned(),
        -1 => msg::common_yesterday().to_owned(),
        1 => msg::common_tomorrow().to_owned(),
        _ => day(ts, utc_offset_secs),
    }
}

/// `Tue, Aug 27`.
pub fn day(ts: Timestamp, utc_offset_secs: i32) -> String {
    let (_, m, d) = civil_from_days(local_day(ts, utc_offset_secs));
    msg::date_weekday_day(
        i64::from(d),
        MONTHS[m as usize - 1](),
        weekday(ts, utc_offset_secs),
    )
}

/// The top bar clock: `Tue, Aug 27 15:12`.
pub fn clock(ts: Timestamp, utc_offset_secs: i32) -> String {
    format!("{} {}", day(ts, utc_offset_secs), hhmm(ts, utc_offset_secs))
}

/// Programme times as the panels show them: `15:12 - 15:19` today, prefixed by the day when the
/// programme starts on another local day than `now`.
pub fn programme_span(p: &Programme, now: Timestamp, utc_offset_secs: i32) -> String {
    if day_offset(p.start, now, utc_offset_secs) == 0 {
        span(p.start, p.stop, utc_offset_secs)
    } else {
        format!(
            "{}, {}",
            day(p.start, utc_offset_secs),
            span(p.start, p.stop, utc_offset_secs)
        )
    }
}

/// The UTC instant of local midnight on `ts`'s local day.
pub fn day_start(ts: Timestamp, utc_offset_secs: i32) -> Timestamp {
    let l = local(ts, utc_offset_secs);
    l - l.rem_euclid(DAY) - i64::from(utc_offset_secs)
}

/// Days since 1970-01-01 of the local day holding `ts`.
pub fn local_day(ts: Timestamp, utc_offset_secs: i32) -> i64 {
    local(ts, utc_offset_secs).div_euclid(DAY)
}

/// Local days from `now`'s day to `ts`'s day: yesterday is -1, tomorrow 1.
pub fn day_offset(ts: Timestamp, now: Timestamp, utc_offset_secs: i32) -> i32 {
    (local_day(ts, utc_offset_secs) - local_day(now, utc_offset_secs)) as i32
}

/// A programme relative to now, in the order search results list them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum When {
    OnAir,
    Past,
    Future,
}

/// Where `[start, stop)` is relative to `now`.
pub fn when(start: Timestamp, stop: Timestamp, now: Timestamp) -> When {
    if stop <= now {
        When::Past
    } else if start <= now {
        When::OnAir
    } else {
        When::Future
    }
}

/// The caller's millisecond clock reading for the moment `at`, given that it reads `now_ms` at
/// `now`; never sooner than a second away, so a deadline that just passed does not spin.
pub fn wake_ms(now_ms: u64, now: Timestamp, at: Timestamp) -> u64 {
    now_ms + (at - now).max(1) as u64 * 1000
}

/// How far `at` is into `p`, `0.0..=1.0`.
pub fn progress(p: &Programme, at: Timestamp) -> f32 {
    let len = p.duration_secs();
    if len <= 0 {
        return 0.0;
    }
    ((at - p.start) as f32 / len as f32).clamp(0.0, 1.0)
}

/// Whole minutes left of `p` at `at`, rounded up (a programme with 10 s left still says 1 min).
pub fn minutes_left(p: &Programme, at: Timestamp) -> i64 {
    ((p.stop - at).max(0) + 59) / 60
}

/// `7 min`.
pub fn minutes_label(mins: i64) -> String {
    msg::core_duration_long_minutes(mins)
}

/// A position or length: `23:13`, or `1:02:03` from an hour on. Negative counts as zero.
pub fn clock_secs(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// A signed offset in brackets: `[+02:00]`, `[-1:30:00]`.
pub fn offset_label(secs: i64) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    format!("[{sign}{}]", clock_secs(secs.abs()))
}

/// The programme on air at `at` in `list` (ordered by start).
pub fn on_air(list: &[Programme], at: Timestamp) -> Option<usize> {
    list.iter().position(|p| p.is_on_air(at))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-08-27 15:12:00 UTC, a Thursday.
    const T: Timestamp = 1_787_843_520;

    fn prog(start: Timestamp, stop: Timestamp) -> Programme {
        Programme {
            start,
            stop,
            title: "P".into(),
            ..Programme::default()
        }
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(local_day(T, 0)), (2026, 8, 27));
        assert_eq!(day(-1, 0), "Wed, Dec 31");
        assert_eq!(day(19_782 * DAY, 0), "Thu, Feb 29");
    }

    #[test]
    fn clock_and_span_follow_the_offset() {
        assert_eq!(hhmm(T, 0), "15:12");
        assert_eq!(hhmm(T, 7200), "17:12");
        assert_eq!(hhmm(T, -16 * 3600), "23:12");
        assert_eq!(clock(T, 0), "Thu, Aug 27 15:12");
        // the offset moves the day too
        assert_eq!(day(T, 9 * 3600), "Fri, Aug 28");
        assert_eq!(span(T, T + 420, 0), "15:12 \u{2014} 15:19");
    }

    #[test]
    fn polish_dates_put_the_day_before_the_month() {
        let _pl =
            tv_platform::i18n::language_on_this_thread_for_test(tv_platform::i18n::Preference::Pl);
        assert_eq!(clock(T, 0), "Czw., 27 sie 15:12");
        assert_eq!(day(T + 3 * DAY, 0), "Niedz., 30 sie");
        assert_eq!(weekday(T - DAY, 0), "\u{15a}r.");
        assert_eq!(minutes_label(7), "7 min");
    }

    #[test]
    fn programme_span_names_other_days() {
        let today = prog(T, T + 600);
        assert_eq!(programme_span(&today, T, 0), "15:12 \u{2014} 15:22");
        let yesterday = prog(T - DAY, T - DAY + 600);
        assert_eq!(
            programme_span(&yesterday, T, 0),
            "Wed, Aug 26, 15:12 \u{2014} 15:22"
        );
    }

    #[test]
    fn progress_and_minutes_left() {
        let p = prog(T, T + 1200);
        assert_eq!(progress(&p, T - 5), 0.0);
        assert_eq!(progress(&p, T + 600), 0.5);
        assert_eq!(progress(&p, T + 5000), 1.0);
        assert_eq!(progress(&prog(T, T), T), 0.0);
        assert_eq!(minutes_left(&p, T), 20);
        assert_eq!(minutes_left(&p, T + 1190), 1);
        assert_eq!(minutes_left(&p, T + 1201), 0);
        assert_eq!(minutes_label(7), "7 min");
    }

    #[test]
    fn positions_and_offsets() {
        assert_eq!(clock_secs(1393), "23:13");
        assert_eq!(clock_secs(2400), "40:00");
        assert_eq!(clock_secs(3723), "1:02:03");
        assert_eq!(clock_secs(-4), "00:00");
        assert_eq!(offset_label(120), "[+02:00]");
        assert_eq!(offset_label(-5400), "[-1:30:00]");
        assert_eq!(offset_label(0), "[+00:00]");
    }

    #[test]
    fn local_days() {
        // 15:12 UTC: local midnight is 00:00 UTC, 22:00 UTC the day before at +2 h, and 10:00
        // UTC at -10 h.
        assert_eq!(day_start(T, 0), T - 15 * 3600 - 720);
        assert_eq!(day_start(T, 7200), T - 17 * 3600 - 720);
        assert_eq!(day_start(T, -10 * 3600), T - 5 * 3600 - 720);
        assert_eq!(day_start(day_start(T, 3600), 3600), day_start(T, 3600));
        assert_eq!(day_offset(T, T, 0), 0);
        assert_eq!(day_offset(T - DAY, T, 0), -1);
        assert_eq!(day_offset(T - 7 * DAY, T, 0), -7);
        assert_eq!(day_offset(T + DAY, T, 0), 1);
        // 23:30 UTC is still today in UTC but tomorrow at +1 h
        let late = day_start(T, 0) + DAY - 1800;
        assert_eq!(day_offset(late, T, 0), 0);
        assert_eq!(day_offset(late, T, 3600), 1);
    }

    #[test]
    fn day_labels_name_the_near_days() {
        assert_eq!(day_label(T, T, 0), "Today");
        assert_eq!(day_label(T - DAY, T, 0), "Yesterday");
        assert_eq!(day_label(T + DAY, T, 0), "Tomorrow");
        assert_eq!(day_label(T - 2 * DAY, T, 0), "Tue, Aug 25");
        // 23:30 UTC on the 27th is already the 28th at +2 h.
        let late = day_start(T, 0) + 23 * HOUR + 30 * MINUTE;
        assert_eq!(day_label(late, T, 2 * 3600), "Tomorrow");
        let _pl =
            tv_platform::i18n::language_on_this_thread_for_test(tv_platform::i18n::Preference::Pl);
        assert_eq!(day_label(T, T, 0), "Dzisiaj");
        assert_eq!(day_label(T - DAY, T, 0), "Wczoraj");
        assert_eq!(day_label(T + DAY, T, 0), "Jutro");
    }

    #[test]
    fn when_and_wake() {
        assert_eq!(when(T - 60, T, T), When::Past);
        assert_eq!(when(T, T + 60, T), When::OnAir);
        assert_eq!(when(T + 1, T + 60, T), When::Future);
        assert!(When::OnAir < When::Past && When::Past < When::Future);
        assert_eq!(wake_ms(5_000, T, T + 3), 8_000);
        // A deadline already passed is a second away, not now.
        assert_eq!(wake_ms(5_000, T, T - 10), 6_000);
        assert_eq!(local_day(T, 9 * 3600), local_day(T, 0) + 1);
    }

    #[test]
    fn on_air_finds_the_running_programme() {
        let list = [prog(T - 600, T), prog(T, T + 600), prog(T + 600, T + 1200)];
        assert_eq!(on_air(&list, T), Some(1));
        assert_eq!(on_air(&list, T + 1199), Some(2));
        assert_eq!(on_air(&list, T + 1200), None);
    }
}
