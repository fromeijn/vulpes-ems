//! Similar-day profile forecaster. For a target local hour, it takes a
//! recency-weighted average of the load at the same local hour on recent days
//! of the same day type, optionally blended with the same season last year.

use crate::calendar::{Country, DayType, day_type};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Timelike};
use chrono_tz::Tz;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy)]
pub struct ProfileOptions {
    /// How far back to look for similar days.
    pub lookback_days: i64,
    /// Weight halves every `half_life_days`.
    pub half_life_days: f64,
    /// Minimum same-type days; below this, all day types are used.
    pub min_days: usize,
    /// Blend weight for the same calendar window last year (0 = off).
    pub last_year_weight: f64,
    /// Half-width of the last-year window in days.
    pub last_year_window: i64,
    /// Weighted median instead of weighted mean (robust to one-off events).
    pub median: bool,
}

impl Default for ProfileOptions {
    fn default() -> Self {
        Self {
            lookback_days: 56,
            half_life_days: 21.0,
            min_days: 2,
            last_year_weight: 0.0,
            last_year_window: 30,
            median: false,
        }
    }
}

/// History indexed by local date and hour.
pub struct ProfileForecaster {
    tz: Tz,
    country: Country,
    opt: ProfileOptions,
    by_day: HashMap<NaiveDate, [Option<f64>; 24]>,
}

fn weighted(mut v: Vec<(f64, f64)>, median: bool) -> Option<f64> {
    let total: f64 = v.iter().map(|x| x.1).sum();
    if v.is_empty() || total <= 0.0 {
        return None;
    }
    if !median {
        return Some(v.iter().map(|(x, w)| x * w).sum::<f64>() / total);
    }
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut acc = 0.0;
    for (x, w) in &v {
        acc += w;
        if acc >= total / 2.0 {
            return Some(*x);
        }
    }
    v.last().map(|x| x.0)
}

impl ProfileForecaster {
    /// `history` is `(hour_start_ts, value)`, hourly. Only pass data that was
    /// available at forecast time.
    pub fn new(history: &[(i64, f64)], tz: Tz, country: Country, opt: ProfileOptions) -> Self {
        let mut by_day: HashMap<NaiveDate, [Option<f64>; 24]> = HashMap::new();
        for &(ts, v) in history {
            if let Some(local) = DateTime::from_timestamp(ts, 0).map(|t| t.with_timezone(&tz)) {
                by_day.entry(local.date_naive()).or_insert([None; 24])[local.hour() as usize] = Some(v);
            }
        }
        Self {
            tz,
            country,
            opt,
            by_day,
        }
    }

    /// Forecast for the hour starting at `ts`.
    pub fn forecast(&self, ts: i64) -> Option<f64> {
        self.forecast_with(ts, &|_| None).map(|(v, _)| v)
    }

    /// Forecast plus the same weighted average of a covariate (e.g. temperature
    /// feature) over the reference days the forecast used, so a regression can
    /// correct for how the target hour differs from those days.
    pub fn forecast_with(
        &self,
        ts: i64,
        covariate: &dyn Fn(i64) -> Option<f64>,
    ) -> Option<(f64, Option<f64>)> {
        let local = self.tz.timestamp_opt(ts, 0).single()?;
        let (date, hour) = (local.date_naive(), local.hour() as usize);
        let target_type = day_type(date, self.country);
        let decay = |age: i64| 0.5f64.powf(age as f64 / self.opt.half_life_days);
        // (value, weight, reference-hour timestamp)
        let collect = |filter: &dyn Fn(DayType) -> bool| -> Vec<(f64, f64, i64)> {
            (1..=self.opt.lookback_days)
                .filter_map(|age| {
                    let d = date - Duration::days(age);
                    let v = self.by_day.get(&d)?[hour]?;
                    let t = ts - age * 86_400; // same local hour except across DST changes
                    filter(day_type(d, self.country)).then(|| (v, decay(age), t))
                })
                .collect()
        };
        let mut refs = collect(&|t| t == target_type);
        if refs.len() < self.opt.min_days {
            refs = collect(&|_| true);
        }
        let cov: Vec<(f64, f64)> = refs
            .iter()
            .filter_map(|&(_, w, t)| covariate(t).map(|c| (c, w)))
            .collect();
        let cov_mean = (cov.len() == refs.len()).then(|| weighted(cov, false)).flatten();
        let recent = weighted(refs.iter().map(|&(v, w, _)| (v, w)).collect(), self.opt.median)?;
        if self.opt.last_year_weight <= 0.0 {
            return Some((recent, cov_mean));
        }
        let ly_center = date - Duration::days(364); // same weekday last year
        let last_year: Vec<(f64, f64)> = (-self.opt.last_year_window..=self.opt.last_year_window)
            .filter_map(|o| {
                let d = ly_center + Duration::days(o);
                let v = self.by_day.get(&d)?[hour]?;
                (day_type(d, self.country) == target_type).then_some((v, 1.0))
            })
            .collect();
        let value = match weighted(last_year, self.opt.median) {
            Some(ly) => (1.0 - self.opt.last_year_weight) * recent + self.opt.last_year_weight * ly,
            None => recent,
        };
        Some((value, cov_mean))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Europe::Amsterdam;

    #[test]
    fn follows_local_time_and_day_type() {
        // Synthetic: workdays peak at 07:00 local (1000 W), weekends at 10:00 local; else 200 W.
        let start = Amsterdam
            .with_ymd_and_hms(2026, 3, 2, 0, 0, 0)
            .unwrap()
            .timestamp(); // a Monday
        let mut hist = Vec::new();
        for h in 0..(24 * 70) {
            let ts = start + h * 3600;
            let l = Amsterdam.timestamp_opt(ts, 0).unwrap();
            let weekend = matches!(
                day_type(l.date_naive(), Country::None),
                DayType::Saturday | DayType::Sunday
            );
            let peak = if weekend { 10 } else { 7 };
            hist.push((ts, if l.hour() == peak { 1000.0 } else { 200.0 }));
        }
        let f = ProfileForecaster::new(&hist, Amsterdam, Country::None, ProfileOptions::default());
        // Across the DST change (29 March 2026), the peak stays at 07:00 local.
        let mon = |h| {
            Amsterdam
                .with_ymd_and_hms(2026, 5, 11, h, 0, 0)
                .unwrap()
                .timestamp()
        };
        let sat = |h| {
            Amsterdam
                .with_ymd_and_hms(2026, 5, 9, h, 0, 0)
                .unwrap()
                .timestamp()
        };
        let close = |ts, want: f64| (f.forecast(ts).unwrap() - want).abs() < 1e-9;
        assert!(close(mon(7), 1000.0) && close(mon(10), 200.0) && close(sat(10), 1000.0));
    }

    #[test]
    fn weighted_median_ignores_outlier() {
        let v = vec![(100.0, 1.0), (110.0, 1.0), (5000.0, 1.0)];
        assert_eq!(weighted(v.clone(), true), Some(110.0));
        assert!(weighted(v, false).unwrap() > 1000.0);
    }

    fn at(y: i32, m: u32, d: u32, h: u32) -> i64 {
        Amsterdam.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap().timestamp()
    }

    /// One value per local hour on the given dates (Country::None, so day type is the weekday).
    fn days(dates: &[(i32, u32, u32, f64)]) -> Vec<(i64, f64)> {
        dates
            .iter()
            .flat_map(|&(y, m, d, v)| (0..24).map(move |h| (at(y, m, d, h), v)))
            .collect()
    }

    #[test]
    fn empty_history_gives_no_forecast() {
        let f = ProfileForecaster::new(&[], Amsterdam, Country::None, ProfileOptions::default());
        assert_eq!(f.forecast(at(2026, 5, 11, 7)), None);
    }

    #[test]
    fn every_hour_of_the_autumn_dst_day_is_forecast() {
        // 25 October 2026 has 25 hours in Amsterdam (02:00 happens twice).
        let start = at(2026, 9, 1, 0);
        let hist: Vec<(i64, f64)> = (0..24 * 54).map(|h| (start + h * 3600, 300.0)).collect();
        let f = ProfileForecaster::new(&hist, Amsterdam, Country::None, ProfileOptions::default());
        let day = at(2026, 10, 25, 0);
        let next = at(2026, 10, 26, 0);
        assert_eq!((next - day) / 3600, 25);
        for t in (day..next).step_by(3600) {
            assert!((f.forecast(t).unwrap() - 300.0).abs() < 1e-9, "{t}");
        }
    }

    #[test]
    fn too_few_same_type_days_falls_back_to_all_days() {
        // Only Mondays in the history; a Saturday forecast uses them anyway.
        let hist = days(&[(2026, 5, 4, 400.0), (2026, 4, 27, 400.0)]);
        let f = ProfileForecaster::new(&hist, Amsterdam, Country::None, ProfileOptions::default());
        assert!((f.forecast(at(2026, 5, 9, 12)).unwrap() - 400.0).abs() < 1e-9);
    }

    #[test]
    fn recent_days_weigh_more() {
        // Wednesdays 7 and 28 days back; half-life 21 days.
        let hist = days(&[(2026, 5, 6, 100.0), (2026, 4, 15, 0.0)]);
        let f = ProfileForecaster::new(&hist, Amsterdam, Country::None, ProfileOptions::default());
        let (w7, w28) = (0.5f64.powf(7.0 / 21.0), 0.5f64.powf(28.0 / 21.0));
        let want = 100.0 * w7 / (w7 + w28);
        assert!((f.forecast(at(2026, 5, 13, 9)).unwrap() - want).abs() < 1e-9);
    }

    #[test]
    fn lookback_limits_the_history() {
        let hist = days(&[(2026, 3, 4, 100.0)]); // 70 days before the target
        let opt = ProfileOptions {
            lookback_days: 56,
            ..Default::default()
        };
        let f = ProfileForecaster::new(&hist, Amsterdam, Country::None, opt);
        assert_eq!(f.forecast(at(2026, 5, 13, 9)), None);
    }

    #[test]
    fn last_year_window_is_blended_in() {
        // Recent Wednesdays at 200 W; the same weekday 364 days back at 100 W.
        let hist = days(&[(2026, 5, 6, 200.0), (2026, 4, 29, 200.0), (2025, 5, 14, 100.0)]);
        let opt = ProfileOptions {
            last_year_weight: 0.25,
            ..Default::default()
        };
        let f = ProfileForecaster::new(&hist, Amsterdam, Country::None, opt);
        assert!((f.forecast(at(2026, 5, 13, 9)).unwrap() - (0.75 * 200.0 + 0.25 * 100.0)).abs() < 1e-9);
        // Without last-year data the recent profile is used as is.
        let f = ProfileForecaster::new(&hist[..48], Amsterdam, Country::None, opt);
        assert!((f.forecast(at(2026, 5, 13, 9)).unwrap() - 200.0).abs() < 1e-9);
    }

    #[test]
    fn covariate_mean_over_the_reference_days() {
        let hist = days(&[(2026, 5, 6, 100.0), (2026, 4, 29, 100.0)]);
        let f = ProfileForecaster::new(&hist, Amsterdam, Country::None, ProfileOptions::default());
        let target = at(2026, 5, 13, 9);
        // Covariate is 10 on the 6th and 20 on the 29th (weights by age 7 and 14).
        let cov = |t: i64| Some(if t == target - 7 * 86_400 { 10.0 } else { 20.0 });
        let (w7, w14) = (0.5f64.powf(7.0 / 21.0), 0.5f64.powf(14.0 / 21.0));
        let (v, c) = f.forecast_with(target, &cov).unwrap();
        assert!((v - 100.0).abs() < 1e-9);
        assert!((c.unwrap() - (10.0 * w7 + 20.0 * w14) / (w7 + w14)).abs() < 1e-9);
        // Missing covariate on any reference day: no mean.
        let partial = |t: i64| (t == target - 7 * 86_400).then_some(10.0);
        assert_eq!(f.forecast_with(target, &partial).unwrap().1, None);
    }

    #[test]
    fn weighted_edge_cases() {
        assert_eq!(weighted(Vec::new(), false), None);
        assert_eq!(weighted(vec![(5.0, 0.0)], false), None, "zero total weight");
        assert_eq!(weighted(vec![(1.0, 1.0), (3.0, 3.0)], false), Some(2.5));
        assert_eq!(
            weighted(vec![(1.0, 1.0), (2.0, 1.0), (100.0, 1.0)], true),
            Some(2.0)
        );
    }
}
