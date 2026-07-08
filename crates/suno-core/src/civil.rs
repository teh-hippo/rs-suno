//! Pure Gregorian civil-date conversion (Howard Hinnant's algorithm).
//!
//! The forward [`days_to_civil`] and its inverse [`civil_to_days`] are the one
//! home for the calendar math shared by the selection timestamp parser and the
//! CLI's RFC 3339 / ISO 8601 audit stamps, so the two directions cannot drift.
//! Integer-only, with no clock or IO.

/// Convert a Gregorian calendar date to days since the Unix epoch
/// (1970-01-01), or `None` when the date precedes the epoch.
pub(crate) fn civil_to_days(y: u32, m: u32, d: u32) -> Option<u64> {
    let (y, m, d) = (y as i64, m as i64, d as i64);
    let ya = if m <= 2 { y - 1 } else { y };
    let era = ya.div_euclid(400);
    let yoe = ya - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days).ok()
}

/// Convert days since the Unix epoch (1970-01-01) to a civil
/// `(year, month, day)`.
pub fn days_to_civil(days: u64) -> (i64, u32, u32) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_maps_both_ways() {
        assert_eq!(days_to_civil(0), (1970, 1, 1));
        assert_eq!(civil_to_days(1970, 1, 1), Some(0));
    }

    #[test]
    fn known_instant_round_trips() {
        // 2024-03-10 is 19792 days after the epoch.
        assert_eq!(civil_to_days(2024, 3, 10), Some(19_792));
        assert_eq!(days_to_civil(19_792), (2024, 3, 10));
    }

    #[test]
    fn pre_epoch_dates_have_no_unsigned_day_count() {
        assert_eq!(civil_to_days(1969, 12, 31), None);
        assert_eq!(civil_to_days(1900, 1, 1), None);
    }

    #[test]
    fn leap_day_is_valid_and_round_trips() {
        let days = civil_to_days(2024, 2, 29).unwrap();
        assert_eq!(days_to_civil(days), (2024, 2, 29));
        let days = civil_to_days(2000, 2, 29).unwrap();
        assert_eq!(days_to_civil(days), (2000, 2, 29));
    }

    #[test]
    fn month_and_year_boundaries() {
        assert_eq!(
            days_to_civil(civil_to_days(2024, 1, 1).unwrap()),
            (2024, 1, 1)
        );
        assert_eq!(
            days_to_civil(civil_to_days(2024, 12, 31).unwrap()),
            (2024, 12, 31)
        );
        assert_eq!(
            days_to_civil(civil_to_days(2023, 3, 1).unwrap()),
            (2023, 3, 1)
        );
    }

    #[test]
    fn forward_inverse_round_trip_sweep() {
        for days in 0..40_000u64 {
            let (y, m, d) = days_to_civil(days);
            assert_eq!(civil_to_days(y as u32, m, d), Some(days), "day {days}");
        }
    }
}
