//! Local day types. Households behave by local clock and calendar, so day
//! types are derived from the local date, with public holidays as Sundays.

use chrono::{Datelike, Duration, NaiveDate, Weekday};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DayType {
    Workday,
    Saturday,
    /// Sundays and public holidays.
    Sunday,
}

/// Countries with a built-in public-holiday calendar. `None` uses weekdays only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Country {
    None,
    Netherlands,
}

/// Easter Sunday (Gregorian), anonymous computus.
pub fn easter(year: i32) -> NaiveDate {
    let a = year % 19;
    let b = year / 100;
    let c = year % 100;
    let d = b / 4;
    let e = b % 4;
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15) % 30;
    let i = c / 4;
    let k = c % 4;
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let m = (a + 11 * h + 22 * l) / 451;
    let month = (h + l - 7 * m + 114) / 31;
    let day = (h + l - 7 * m + 114) % 31 + 1;
    NaiveDate::from_ymd_opt(year, month as u32, day as u32).expect("valid Easter date")
}

pub fn is_holiday(date: NaiveDate, country: Country) -> bool {
    match country {
        Country::None => false,
        Country::Netherlands => {
            let y = date.year();
            let e = easter(y);
            let ymd = |m, d| NaiveDate::from_ymd_opt(y, m, d);
            // King's Day: 27 April, or 26 April when the 27th is a Sunday.
            let kings = match ymd(4, 27) {
                Some(k) if k.weekday() == Weekday::Sun => ymd(4, 26),
                k => k,
            };
            let fixed = [ymd(1, 1), kings, ymd(12, 25), ymd(12, 26)];
            let moving = [
                e + Duration::days(1),
                e + Duration::days(39),
                e + Duration::days(50),
            ];
            fixed.contains(&Some(date)) || moving.contains(&date)
                // Liberation Day is a national day off every fifth year.
                || (date == NaiveDate::from_ymd_opt(y, 5, 5).unwrap() && y % 5 == 0)
        }
    }
}

pub fn day_type(date: NaiveDate, country: Country) -> DayType {
    match date.weekday() {
        Weekday::Sun => DayType::Sunday,
        _ if is_holiday(date, country) => DayType::Sunday,
        Weekday::Sat => DayType::Saturday,
        _ => DayType::Workday,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn easter_dates() {
        assert_eq!(easter(2024), d(2024, 3, 31));
        assert_eq!(easter(2025), d(2025, 4, 20));
        assert_eq!(easter(2026), d(2026, 4, 5));
        assert_eq!(easter(2027), d(2027, 3, 28));
    }

    #[test]
    fn dutch_holidays() {
        let nl = Country::Netherlands;
        assert!(is_holiday(d(2026, 4, 6), nl)); // Easter Monday
        assert!(is_holiday(d(2026, 5, 14), nl)); // Ascension
        assert!(is_holiday(d(2026, 5, 25), nl)); // Whit Monday
        assert!(is_holiday(d(2025, 4, 26), nl)); // King's Day moved (27th is a Sunday)
        assert!(is_holiday(d(2025, 5, 5), nl) && !is_holiday(d(2026, 5, 5), nl));
        assert!(!is_holiday(d(2026, 4, 7), nl));
        assert_eq!(day_type(d(2026, 5, 14), nl), DayType::Sunday);
        assert_eq!(day_type(d(2026, 5, 14), Country::None), DayType::Workday);
        assert_eq!(day_type(d(2026, 11, 7), nl), DayType::Saturday);
    }

    #[test]
    fn day_types() {
        let nl = Country::Netherlands;
        assert_eq!(day_type(d(2026, 11, 7), nl), DayType::Saturday);
        assert_eq!(day_type(d(2026, 11, 8), nl), DayType::Sunday);
        assert_eq!(day_type(d(2026, 11, 9), nl), DayType::Workday);
        // Christmas 2025 is a Thursday: a holiday in NL, a workday without a calendar.
        assert_eq!(day_type(d(2025, 12, 25), nl), DayType::Sunday);
        assert_eq!(day_type(d(2025, 12, 25), Country::None), DayType::Workday);
        assert!(!is_holiday(d(2025, 12, 25), Country::None));
    }

    #[test]
    fn kings_day_moves_off_sunday() {
        // 27 April 2025 is a Sunday, so King's Day is Saturday the 26th.
        assert!(is_holiday(d(2025, 4, 26), Country::Netherlands));
        assert!(!is_holiday(d(2025, 4, 27), Country::Netherlands));
        assert_eq!(day_type(d(2025, 4, 26), Country::Netherlands), DayType::Sunday);
        assert!(is_holiday(d(2026, 4, 27), Country::Netherlands));
    }

    #[test]
    fn moving_holidays_and_liberation_day() {
        let nl = Country::Netherlands;
        // Easter 2026 is 5 April: Easter Monday, Ascension (+39) and Whit Monday (+50).
        for date in [d(2026, 4, 6), d(2026, 5, 14), d(2026, 5, 25)] {
            assert!(is_holiday(date, nl), "{date}");
        }
        assert!(!is_holiday(d(2026, 4, 3), nl), "Good Friday is not a day off");
        assert!(is_holiday(d(2025, 5, 5), nl) && !is_holiday(d(2026, 5, 5), nl));
        assert!(is_holiday(d(2030, 5, 5), nl));
    }
}
