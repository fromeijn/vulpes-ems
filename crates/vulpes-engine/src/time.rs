//! Local days in a time zone, as Unix seconds. One place for "midnight", so
//! "today" means the same span in the engine, the review and the integration.

use chrono::{DateTime, NaiveDate, TimeZone};
use chrono_tz::Tz;

/// The local date of `ts` in `tz`.
pub fn local_date(ts: i64, tz: Tz) -> Option<NaiveDate> {
    Some(DateTime::from_timestamp(ts, 0)?.with_timezone(&tz).date_naive())
}

/// `hour`:00 local time on `date`, the earlier one when the clock goes back;
/// `None` when that time does not exist (skipped when the clock goes forward).
pub fn local_time(tz: Tz, date: NaiveDate, hour: u32) -> Option<i64> {
    Some(
        tz.from_local_datetime(&date.and_hms_opt(hour, 0, 0)?)
            .earliest()?
            .timestamp(),
    )
}

/// Start of local day `date`.
pub fn local_midnight(tz: Tz, date: NaiveDate) -> Option<i64> {
    local_time(tz, date, 0)
}

/// Start of the local day that `ts` falls in.
pub fn day_start(ts: i64, tz: Tz) -> Option<i64> {
    local_midnight(tz, local_date(ts, tz)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMS: Tz = chrono_tz::Europe::Amsterdam;
    const KOLKATA: Tz = chrono_tz::Asia::Kolkata;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn midnight_and_day_start() {
        // 2026-11-08 00:00 CET = 2026-11-07 23:00 UTC.
        let m = local_midnight(AMS, date(2026, 11, 8)).unwrap();
        assert_eq!(m, 1_794_092_400);
        assert_eq!(day_start(m, AMS), Some(m));
        assert_eq!(day_start(m + 86_399, AMS), Some(m));
        assert_eq!(day_start(m - 1, AMS), Some(m - 86_400));
        assert_eq!(local_date(m, AMS), Some(date(2026, 11, 8)));
        assert_eq!(local_date(m, Tz::UTC), Some(date(2026, 11, 7)));
    }

    #[test]
    fn dst_days_are_23_and_25_hours() {
        let len = |d: NaiveDate| {
            local_midnight(AMS, d.succ_opt().unwrap()).unwrap() - local_midnight(AMS, d).unwrap()
        };
        assert_eq!(len(date(2026, 3, 29)), 23 * 3600);
        assert_eq!(len(date(2026, 10, 25)), 25 * 3600);
        assert_eq!(len(date(2026, 11, 8)), 24 * 3600);
    }

    #[test]
    fn times_that_dont_exist_or_repeat() {
        // 02:00 is skipped in spring; 02:00 happens twice in autumn (the first one counts).
        assert_eq!(local_time(AMS, date(2026, 3, 29), 2), None);
        let autumn = local_time(AMS, date(2026, 10, 25), 2).unwrap();
        assert_eq!(
            autumn,
            local_midnight(AMS, date(2026, 10, 25)).unwrap() + 2 * 3600
        );
        assert_eq!(local_time(AMS, date(2026, 11, 8), 24), None, "not an hour");
    }

    #[test]
    fn half_hour_offsets() {
        // Kolkata is UTC+5:30: midnight is at 18:30 UTC the day before.
        let m = local_midnight(KOLKATA, date(2026, 11, 8)).unwrap();
        assert_eq!(m.rem_euclid(3600), 1800);
        assert_eq!(day_start(m + 3600, KOLKATA), Some(m));
    }

    #[test]
    fn out_of_range() {
        assert_eq!(local_date(i64::MAX, AMS), None);
        assert_eq!(day_start(i64::MAX, AMS), None);
    }
}
