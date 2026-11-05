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
}
