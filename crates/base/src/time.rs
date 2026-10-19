//! Proleptic Gregorian dates as days since 1970-01-01, Howard Hinnant's algorithms
//! (<https://howardhinnant.github.io/date_algorithms.html>).

/// `(year, month 1-12, day 1-31)` of a day count since 1970-01-01.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Saturating so no day count panics; the dates only go wrong ~10^16 years out.
    let z = days.saturating_add(719_468);
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// Days since 1970-01-01 of a date; `month` is 1-12. Out-of-range days count on into the next
/// month, as the algorithm does.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = i64::from((month + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_days() {
        for (days, date) in [
            (0, (1970, 1, 1)),
            (-1, (1969, 12, 31)),
            (11_016, (2000, 2, 29)),
            (19_675, (2023, 11, 14)),
            (19_782, (2024, 2, 29)),
            (-719_468, (0, 3, 1)),
        ] {
            assert_eq!(civil_from_days(days), date, "{days}");
            assert_eq!(days_from_civil(date.0, date.1, date.2), days, "{date:?}");
        }
    }

    #[test]
    fn round_trips_across_four_centuries() {
        for days in (-146_097..2 * 146_097).step_by(7) {
            let (y, m, d) = civil_from_days(days);
            assert!((1..=12).contains(&m) && (1..=31).contains(&d), "{days}");
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }

    #[test]
    fn extreme_day_counts_do_not_panic() {
        let _ = civil_from_days(i64::MAX);
        let _ = civil_from_days(i64::MIN);
    }
}
