//! Wholesale day-ahead price forecast: a similar-day profile, corrected by a
//! ridge regression on wind and solar at reference points.
//!
//! The profile takes the same local hour on recent days of the same type. The
//! regression explains the profile's error by how the hour's wind-power and
//! solar indices differ from the profile's reference days: a windy day after
//! calm ones is cheaper than the profile says. `vulpes-cli eval-price` and the
//! engine both use these functions, so the evaluation measures what runs live.

use crate::ridge::{Ridge, recency_weight};
use chrono::TimeZone;
use chrono_tz::Tz;
use std::collections::HashMap;
use vulpes_load::{Country, ProfileForecaster, ProfileOptions};

/// A public weather reference point for the price drivers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RefPoint {
    pub label: &'static str,
    pub lat: f64,
    pub lon: f64,
}

/// Wind reference points for the NL zone: the North Sea off IJmuiden and
/// Schleswig-Holstein, where much of the neighbouring wind capacity is.
pub const WIND_POINTS: [RefPoint; 2] = [
    RefPoint {
        label: "nl-offshore",
        lat: 52.6,
        lon: 3.9,
    },
    RefPoint {
        label: "de-north",
        lat: 54.3,
        lon: 9.5,
    },
];

/// Solar reference points for the NL zone: central NL and central Germany.
pub const SOLAR_POINTS: [RefPoint; 2] = [
    RefPoint {
        label: "nl-central",
        lat: 52.1,
        lon: 5.2,
    },
    RefPoint {
        label: "de-central",
        lat: 51.0,
        lon: 10.5,
    },
];

/// Ridge penalty of the regression.
pub const LAMBDA: f64 = 1e-3;
/// Training rows lose half their weight every this many days.
pub const HALF_LIFE_DAYS: f64 = 30.0;
/// The regression needs this many hourly rows (14 days) before it is used.
pub const MIN_ROWS: usize = 24 * 14;
/// Days of history the profile looks back.
pub const LOOKBACK_DAYS: i64 = 28;
/// The live self-check compares regression and profile over this many recent days.
pub const CHECK_DAYS: i64 = 14;
/// Days of wholesale history the live forecaster gets: the profile's look-back
/// before each day the regression learns from, and about two months of those days.
pub const HISTORY_DAYS: i64 = 90;

/// Profile settings for wholesale prices: 28 days back, 7-day half-life.
pub fn profile_options() -> ProfileOptions {
    ProfileOptions {
        lookback_days: LOOKBACK_DAYS,
        half_life_days: 7.0,
        ..Default::default()
    }
}

/// The wholesale profile; prices have no national holidays.
pub fn profile(history: &[(i64, f64)], tz: Tz) -> ProfileForecaster {
    ProfileForecaster::new(history, tz, Country::None, profile_options())
}

/// Normalised wind turbine power curve: cubic from 3 m/s cut-in to 12 m/s rated.
pub fn wind_power(v: f64) -> f64 {
    ((v.powi(3) - 27.0) / (1728.0 - 27.0)).clamp(0.0, 1.0)
}

/// Mean of a per-point transform over all points, if every point has data.
pub fn index(points: &[HashMap<i64, f64>], h: i64, f: impl Fn(f64) -> f64) -> Option<f64> {
    if points.is_empty() {
        return None;
    }
    let vals: Option<Vec<f64>> = points.iter().map(|p| p.get(&h).map(|&v| f(v))).collect();
    vals.map(|v| v.iter().sum::<f64>() / v.len() as f64)
}

/// `(hour start, wind-power index, solar index)`; `None` where missing.
pub type DriverRow = (i64, Option<f64>, Option<f64>);

/// Hourly means of 15-minute points, keyed by hour start. Hours without all
/// four quarters are dropped, so a partial hour never looks complete.
pub fn hourly_means(points: impl IntoIterator<Item = (i64, f64)>) -> HashMap<i64, f64> {
    let mut acc: HashMap<i64, (f64, u32)> = HashMap::new();
    for (t, v) in points {
        let e = acc.entry(t - t.rem_euclid(3600)).or_default();
        e.0 += v;
        e.1 += 1;
    }
    acc.into_iter()
        .filter(|(_, (_, n))| *n == 4)
        .map(|(h, (s, n))| (h, s / n as f64))
        .collect()
}

/// Hourly driver indices: the wind-power index (0..1) and solar (kW/m²).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Drivers {
    pub wind: HashMap<i64, f64>,
    pub solar: HashMap<i64, f64>,
}

impl Drivers {
    /// Indices from hourly weather at the reference points: 100 m wind speed
    /// (m/s) per wind point and global horizontal irradiance (W/m²) per solar
    /// point. An hour needs every point.
    pub fn from_points(wind: &[HashMap<i64, f64>], solar: &[HashMap<i64, f64>]) -> Self {
        let hours = |points: &[HashMap<i64, f64>]| -> Vec<i64> {
            points
                .first()
                .map(|p| p.keys().copied().collect())
                .unwrap_or_default()
        };
        Self {
            wind: hours(wind)
                .into_iter()
                .filter_map(|h| index(wind, h, wind_power).map(|v| (h, v)))
                .collect(),
            solar: hours(solar)
                .into_iter()
                .filter_map(|h| index(solar, h, |g| g / 1000.0).map(|v| (h, v)))
                .collect(),
        }
    }

    /// From `(hour, wind index, solar index)` rows; missing and non-finite
    /// values are left out.
    pub fn from_rows(rows: &[DriverRow]) -> Self {
        let mut d = Self::default();
        for &(h, w, s) in rows {
            if let Some(w) = w.filter(|w| w.is_finite()) {
                d.wind.insert(h, w);
            }
            if let Some(s) = s.filter(|s| s.is_finite()) {
                d.solar.insert(h, s);
            }
        }
        d
    }

    /// `(hour, wind index, solar index)` rows in time order, for JSON.
    pub fn to_rows(&self) -> Vec<DriverRow> {
        let mut hours: Vec<i64> = self.wind.keys().chain(self.solar.keys()).copied().collect();
        hours.sort_unstable();
        hours.dedup();
        hours
            .into_iter()
            .map(|h| (h, self.wind.get(&h).copied(), self.solar.get(&h).copied()))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.wind.is_empty() && self.solar.is_empty()
    }

    /// These drivers, with the hours they lack taken from `other`.
    pub fn filled_from(mut self, other: &Drivers) -> Self {
        for (map, from) in [(&mut self.wind, &other.wind), (&mut self.solar, &other.solar)] {
            for (&h, &v) in from {
                map.entry(h).or_insert(v);
            }
        }
        self
    }
}

/// Profile forecast for the hour starting at `t`, and the regression features:
/// how the hour's drivers (`ahead`) differ from the same weighted mean over the
/// profile's reference days (`past`). `None` without a profile forecast; no
/// features when a driver is missing for the hour or a reference day.
pub fn features(
    profile: &ProfileForecaster,
    past: &Drivers,
    ahead: &Drivers,
    t: i64,
) -> Option<(f64, Option<Vec<f64>>)> {
    let (p, w_ref) = profile.forecast_with(t, &|h| past.wind.get(&h).copied())?;
    let (_, s_ref) = profile.forecast_with(t, &|h| past.solar.get(&h).copied())?;
    let x = match (ahead.wind.get(&t), ahead.solar.get(&t), w_ref, s_ref) {
        (Some(w), Some(s), Some(wr), Some(sr)) => Some(vec![w - wr, s - sr]),
        _ => None,
    };
    Some((p, x))
}

/// One training row: the profile's error in one hour and its features.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Start of the local day the hour belongs to.
    pub day: i64,
    pub x: Vec<f64>,
    /// Actual minus profile.
    pub residual: f64,
}

/// Hours of the local day starting at `start`, up to the next local midnight
/// (23 or 25 on DST days).
pub fn day_hours(start: i64, end: i64) -> Vec<i64> {
    (0..).map(|k| start + k * 3600).take_while(|&t| t < end).collect()
}

/// Start of the local day `date` in `tz`.
pub fn local_midnight(date: chrono::NaiveDate, tz: Tz) -> Option<i64> {
    tz.from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
        .earliest()
        .map(|t| t.timestamp())
}

/// Training rows of one day: for each hour, the profile's error and the
/// features, until the first hour without a profile forecast. Hours without
/// features give no row. The profile must only know the days before this one.
pub fn day_rows(
    profile: &ProfileForecaster,
    drivers: &Drivers,
    actual: &HashMap<i64, f64>,
    start: i64,
    hours: &[i64],
) -> Vec<Row> {
    let mut rows = Vec::new();
    for &t in hours {
        let Some(&a) = actual.get(&t) else { break };
        let Some((p, x)) = features(profile, drivers, drivers, t) else {
            break;
        };
        if let Some(x) = x {
            rows.push(Row {
                day: start,
                x,
                residual: a - p,
            });
        }
    }
    rows
}

/// Fit the regression on `rows`, weighted by age at `at`. `None` with fewer
/// than [`MIN_ROWS`] rows or a singular system.
pub fn fit(rows: &[Row], at: i64) -> Option<Ridge> {
    if rows.len() < MIN_ROWS {
        return None;
    }
    let weighted: Vec<(Vec<f64>, f64, f64)> = rows
        .iter()
        .map(|r| {
            let age = (at - r.day) as f64 / 86_400.0;
            (r.x.clone(), r.residual, recency_weight(age, HALF_LIFE_DAYS))
        })
        .collect();
    Ridge::fit(&weighted, LAMBDA)
}

/// Which forecast the live forecaster gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// The similar-day profile alone.
    Profile,
    /// The profile corrected by the wind/solar regression.
    Regression,
}

/// How regression and profile did over the most recent days, each day
/// forecast with a model fitted on the days before it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Check {
    pub days: usize,
    /// Mean absolute error per hour (currency/kWh).
    pub profile_mae: f64,
    pub regression_mae: f64,
}

/// Wholesale forecaster for the live planner.
pub struct WholesaleForecaster {
    profile: ProfileForecaster,
    past: Drivers,
    ahead: Drivers,
    model: Option<Ridge>,
    method: Method,
    check: Option<Check>,
}

impl WholesaleForecaster {
    /// - `history`: hourly wholesale prices known now;
    /// - `past`: the drivers as forecast a day ahead, for the history's hours;
    /// - `ahead`: the latest forecast of the drivers, for the hours to come.
    ///   Where it has no value, `past` is used. It also stands in for the
    ///   reference days `past` doesn't reach: today, and tomorrow once its
    ///   prices are published, are in the history but not yet in `past`.
    ///
    /// The regression trains on complete local days that have the profile's full
    /// look-back behind them. It is used only when it beat the profile over the
    /// last [`CHECK_DAYS`] days, each forecast with a model of the days before.
    pub fn new(history: &[(i64, f64)], past: Drivers, ahead: Drivers, tz: Tz, now: i64) -> Self {
        let profile = profile(history, tz);
        let rows = training_rows(&profile, &past, history, tz, now);
        let check = self_check(&rows);
        let model = fit(&rows, now);
        let past = past.filled_from(&ahead);
        let method = match (&model, check) {
            (Some(_), Some(c)) if c.regression_mae < c.profile_mae => Method::Regression,
            _ => Method::Profile,
        };
        Self {
            profile,
            past,
            ahead,
            model,
            method,
            check,
        }
    }

    pub fn method(&self) -> Method {
        self.method
    }

    pub fn check(&self) -> Option<Check> {
        self.check
    }

    pub fn model(&self) -> Option<&Ridge> {
        self.model.as_ref()
    }

    /// Forecast for the hour starting at `t`, and whether the regression
    /// corrected it, with the hour's drivers from `ahead`, else from `past`.
    pub fn forecast(&self, t: i64) -> Option<(f64, bool)> {
        let pick = |ahead: &HashMap<i64, f64>, past: &HashMap<i64, f64>| {
            ahead.get(&t).or_else(|| past.get(&t)).copied()
        };
        self.forecast_with(
            t,
            pick(&self.ahead.wind, &self.past.wind),
            pick(&self.ahead.solar, &self.past.solar),
        )
    }

    /// Forecast for the hour starting at `t` with the given wind and solar
    /// indices for that hour. The profile alone when the method is the profile
    /// or a driver is missing.
    pub fn forecast_with(&self, t: i64, wind: Option<f64>, solar: Option<f64>) -> Option<(f64, bool)> {
        let hour = Drivers {
            wind: wind.map(|v| (t, v)).into_iter().collect(),
            solar: solar.map(|v| (t, v)).into_iter().collect(),
        };
        let (p, x) = features(&self.profile, &self.past, &hour, t)?;
        match (self.method, &self.model, x) {
            (Method::Regression, Some(m), Some(x)) => Some((p + m.predict(&x), true)),
            _ => Some((p, false)),
        }
    }
}

/// Rows of every complete local day in `history` before `now` that has the
/// profile's full look-back of history before it.
fn training_rows(
    profile: &ProfileForecaster,
    drivers: &Drivers,
    history: &[(i64, f64)],
    tz: Tz,
    now: i64,
) -> Vec<Row> {
    let Some(first) = history.iter().map(|x| x.0).min() else {
        return Vec::new();
    };
    let actual: HashMap<i64, f64> = history.iter().copied().collect();
    let date = |t: i64| tz.timestamp_opt(t, 0).single().map(|d| d.date_naive());
    let (Some(first), Some(today)) = (date(first), date(now)) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    let mut day = first + chrono::Duration::days(LOOKBACK_DAYS);
    while day < today {
        let next = day + chrono::Duration::days(1);
        if let (Some(start), Some(end)) = (local_midnight(day, tz), local_midnight(next, tz)) {
            let hours = day_hours(start, end);
            if hours.iter().all(|t| actual.contains_key(t)) {
                rows.extend(day_rows(profile, drivers, &actual, start, &hours));
            }
        }
        day = next;
    }
    rows
}

/// Rolling check over the last [`CHECK_DAYS`] days in `rows`: each day is
/// forecast with a model fitted on the days before it. `None` when no day had
/// a model.
fn self_check(rows: &[Row]) -> Option<Check> {
    let last = rows.iter().map(|r| r.day).max()?;
    let from = last - (CHECK_DAYS - 1) * 86_400 - 3 * 3600; // DST-safe: a day start within the window
    let mut days: Vec<i64> = rows.iter().map(|r| r.day).filter(|&d| d >= from).collect();
    days.dedup();
    let (mut n, mut checked, mut prof, mut reg) = (0usize, 0usize, 0.0, 0.0);
    for d in days {
        // Rows are in day order: the days before `d` are a prefix.
        let before = &rows[..rows.partition_point(|r| r.day < d)];
        let Some(m) = fit(before, d) else { continue };
        checked += 1;
        for r in rows.iter().filter(|r| r.day == d) {
            prof += r.residual.abs();
            reg += (r.residual - m.predict(&r.x)).abs();
            n += 1;
        }
    }
    (n > 0).then(|| Check {
        days: checked,
        profile_mae: prof / n as f64,
        regression_mae: reg / n as f64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    const TZ: Tz = chrono_tz::Europe::Amsterdam;
    const START: i64 = 1_767_225_600; // 2026-01-01 00:00 UTC

    fn local_hour(t: i64) -> f64 {
        TZ.timestamp_opt(t, 0).unwrap().hour() as f64
    }

    /// Wind index per day, varying from day to day: 0 to 1.
    fn wind_of(t: i64) -> f64 {
        wind_power(3.0 + ((t / 86_400 * 5) % 10) as f64)
    }

    fn hours(days: i64) -> Vec<i64> {
        (0..24 * days).map(|k| START + k * 3600).collect()
    }

    /// Windy hours are cheaper: price = shape − 0.10 × wind index.
    fn windy_market(days: i64) -> (Vec<(i64, f64)>, Drivers) {
        let h = hours(days);
        let prices = h
            .iter()
            .map(|&t| (t, 0.20 + 0.005 * local_hour(t) - 0.10 * wind_of(t)))
            .collect();
        let rows: Vec<DriverRow> = h.iter().map(|&t| (t, Some(wind_of(t)), Some(0.0))).collect();
        (prices, Drivers::from_rows(&rows))
    }

    #[test]
    fn wind_power_curve() {
        assert_eq!(wind_power(0.0), 0.0);
        assert_eq!(wind_power(3.0), 0.0, "cut-in");
        assert_eq!(wind_power(12.0), 1.0, "rated");
        assert_eq!(wind_power(25.0), 1.0, "capped");
        assert_eq!(wind_power(-4.0), 0.0);
        assert!(wind_power(6.0) < wind_power(9.0));
    }

    #[test]
    fn index_needs_every_point() {
        let a: HashMap<i64, f64> = [(0, 2.0), (3600, 4.0)].into();
        let b: HashMap<i64, f64> = [(0, 6.0)].into();
        assert_eq!(index(&[a.clone(), b.clone()], 0, |v| v / 2.0), Some(2.0));
        assert_eq!(index(&[a, b], 3600, |v| v), None);
        assert_eq!(index(&[], 0, |v| v), None, "no points is no index, not NaN");
    }

    #[test]
    fn drivers_from_points_and_rows() {
        let w1: HashMap<i64, f64> = [(0, 12.0), (3600, 3.0)].into();
        let w2: HashMap<i64, f64> = [(0, 3.0)].into();
        let s1: HashMap<i64, f64> = [(0, 500.0)].into();
        let d = Drivers::from_points(&[w1, w2], &[s1]);
        assert_eq!(d.wind, [(0, 0.5)].into(), "an hour needs every wind point");
        assert_eq!(d.solar, [(0, 0.5)].into());
        assert!(Drivers::from_points(&[], &[]).is_empty());
        let d = Drivers::from_rows(&[
            (0, Some(0.3), Some(f64::NAN)),
            (3600, Some(f64::INFINITY), Some(0.2)),
        ]);
        assert_eq!(d.wind, [(0, 0.3)].into(), "non-finite values are dropped");
        assert_eq!(d.solar, [(3600, 0.2)].into());
        assert_eq!(d.to_rows(), vec![(0, Some(0.3), None), (3600, None, Some(0.2))]);
        let d = Drivers::from_rows(&[(7200, None, Some(0.1)), (0, Some(0.5), Some(0.0))]);
        assert_eq!(
            d.to_rows(),
            vec![(0, Some(0.5), Some(0.0)), (7200, None, Some(0.1))],
            "time order"
        );
        assert_eq!(Drivers::from_rows(&d.to_rows()), d, "round trip");
    }

    #[test]
    fn hourly_means_of_complete_hours_only() {
        let pts = [
            (3600, 1.0),
            (4500, 2.0),
            (5400, 3.0),
            (6300, 6.0),
            (7200, 5.0),
            (8100, 5.0),
        ];
        let m = hourly_means(pts);
        assert_eq!(m.len(), 1, "the second hour has only two quarters");
        assert_eq!(m[&3600], 3.0);
        let neg = [(-3600, 4.0), (-2700, 4.0), (-1800, 4.0), (-900, 8.0)];
        assert_eq!(hourly_means(neg)[&-3600], 5.0);
        assert!(hourly_means(Vec::new()).is_empty());
    }

    #[test]
    fn day_hours_on_dst_days() {
        let day = |s: &str| {
            let d: chrono::NaiveDate = s.parse().unwrap();
            let (a, b) = (
                local_midnight(d, TZ).unwrap(),
                local_midnight(d + chrono::Duration::days(1), TZ).unwrap(),
            );
            day_hours(a, b).len()
        };
        assert_eq!(day("2026-03-29"), 23);
        assert_eq!(day("2026-10-25"), 25);
        assert_eq!(day("2026-06-01"), 24);
    }

    #[test]
    fn fit_needs_enough_rows() {
        let row = |day: i64, w: f64| Row {
            day,
            x: vec![w, 0.0],
            residual: -0.1 * w,
        };
        let few: Vec<Row> = (0..MIN_ROWS - 1).map(|i| row(0, (i % 7) as f64)).collect();
        assert!(fit(&few, 0).is_none());
        let enough: Vec<Row> = (0..MIN_ROWS).map(|i| row(0, (i % 7) as f64)).collect();
        let m = fit(&enough, 0).unwrap();
        assert!((m.coef[0] + 0.1).abs() < 1e-3, "{m:?}");
        assert!(fit(&[], 0).is_none());
    }

    #[test]
    fn live_forecaster_learns_the_wind_effect() {
        let (prices, drivers) = windy_market(90);
        let now = START + 80 * 86_400 + 10 * 3600;
        let known: Vec<(i64, f64)> = prices.iter().copied().filter(|&(t, _)| t < now).collect();
        let f = WholesaleForecaster::new(&known, drivers.clone(), drivers, TZ, now);
        assert_eq!(f.method(), Method::Regression);
        let c = f.check().unwrap();
        assert_eq!(c.days, CHECK_DAYS as usize);
        assert!(c.regression_mae < 0.5 * c.profile_mae, "{c:?}");
        let m = f.model().unwrap();
        assert!((m.coef[0] + 0.10).abs() < 0.02, "{m:?}");
        // The next days, with the actual wind as the forecast.
        let actual: HashMap<i64, f64> = prices.iter().copied().collect();
        let (mut err_reg, mut err_prof, mut n) = (0.0, 0.0, 0);
        for t in (now..START + 90 * 86_400).step_by(3600) {
            let (v, corrected) = f.forecast(t).unwrap();
            assert!(corrected && v.is_finite());
            err_reg += (v - actual[&t]).abs();
            err_prof += (f.profile.forecast(t).unwrap() - actual[&t]).abs();
            n += 1;
        }
        assert!(n > 0 && err_reg < 0.5 * err_prof, "{err_reg} vs {err_prof}");
    }

    #[test]
    fn falls_back_to_the_profile_when_the_regression_is_worse() {
        // Prices follow the hour only; the "wind" is noise that changes sign
        // between the training and the check, so the regression hurts.
        let h = hours(90);
        let prices: Vec<(i64, f64)> = h.iter().map(|&t| (t, 0.10 + 0.01 * local_hour(t))).collect();
        let check_from = START + 66 * 86_400;
        let noise = |t: i64| {
            let w = ((t / 86_400 * 7) % 5) as f64 / 4.0;
            if t < check_from { w } else { 1.0 - w }
        };
        // Prices react to the noise during training only.
        let prices: Vec<(i64, f64)> = prices
            .into_iter()
            .map(|(t, p)| (t, if t < check_from { p - 0.2 * noise(t) } else { p }))
            .collect();
        let rows: Vec<DriverRow> = h.iter().map(|&t| (t, Some(noise(t)), Some(0.0))).collect();
        let drivers = Drivers::from_rows(&rows);
        let now = START + 80 * 86_400 + 10 * 3600;
        let known: Vec<(i64, f64)> = prices.iter().copied().filter(|&(t, _)| t < now).collect();
        let f = WholesaleForecaster::new(&known, drivers.clone(), drivers, TZ, now);
        let c = f.check().unwrap();
        assert!(c.regression_mae > c.profile_mae, "{c:?}");
        assert_eq!(f.method(), Method::Profile);
        let (v, corrected) = f.forecast(now + 86_400).unwrap();
        assert!(!corrected);
        assert_eq!(v, f.profile.forecast(now + 86_400).unwrap());
    }

    #[test]
    fn without_drivers_it_is_the_profile() {
        let (prices, _) = windy_market(60);
        let now = START + 50 * 86_400;
        let known: Vec<(i64, f64)> = prices.iter().copied().filter(|&(t, _)| t < now).collect();
        let f = WholesaleForecaster::new(&known, Drivers::default(), Drivers::default(), TZ, now);
        assert_eq!(f.method(), Method::Profile);
        assert!(f.check().is_none() && f.model().is_none());
        for t in (now..now + 3 * 86_400).step_by(3600) {
            assert_eq!(f.forecast(t), f.profile.forecast(t).map(|v| (v, false)));
        }
    }

    #[test]
    fn short_or_empty_history_gives_the_profile_or_nothing() {
        let f = WholesaleForecaster::new(&[], Drivers::default(), Drivers::default(), TZ, START);
        assert_eq!(f.forecast(START + 3600), None);
        assert_eq!(f.method(), Method::Profile);
        // One day: a profile, no training rows (no full look-back).
        let (prices, drivers) = windy_market(1);
        let now = START + 86_400;
        let f = WholesaleForecaster::new(&prices, drivers.clone(), drivers, TZ, now);
        assert_eq!(f.method(), Method::Profile);
        let (v, corrected) = f.forecast(now + 3600).unwrap();
        assert!(v.is_finite() && !corrected);
    }

    #[test]
    fn ahead_drivers_take_precedence_and_past_fills_gaps() {
        let (prices, drivers) = windy_market(90);
        let now = START + 80 * 86_400;
        let known: Vec<(i64, f64)> = prices.iter().copied().filter(|&(t, _)| t < now).collect();
        let t = now + 30 * 3600;
        let past_only = WholesaleForecaster::new(&known, drivers.clone(), Drivers::default(), TZ, now);
        let calm = Drivers::from_rows(&[(t, Some(0.0), Some(0.0))]);
        let with_ahead = WholesaleForecaster::new(&known, drivers.clone(), calm, TZ, now);
        let (a, _) = past_only.forecast(t).unwrap();
        let (b, _) = with_ahead.forecast(t).unwrap();
        if drivers.wind[&t] > 0.0 {
            assert!(b > a, "a calm forecast is dearer: {b} vs {a}");
        }
        // Another hour: the ahead map has nothing, so past is used.
        assert_eq!(with_ahead.forecast(t + 3600), past_only.forecast(t + 3600));
    }

    #[test]
    fn published_days_without_past_drivers_use_the_forecast() {
        // Live: the history holds today and tomorrow (published), the past
        // drivers end yesterday, and the forecast covers today onwards. Every
        // later hour has today or tomorrow as a reference day.
        let (prices, drivers) = windy_market(90);
        let today = START + 80 * 86_400 - 3600; // local midnight (CET)
        let now = today + 14 * 3600;
        let known: Vec<(i64, f64)> = prices
            .iter()
            .copied()
            .filter(|&(t, _)| t < today + 2 * 86_400)
            .collect();
        let split = |keep: &dyn Fn(i64) -> bool| Drivers {
            wind: drivers
                .wind
                .iter()
                .filter(|e| keep(*e.0))
                .map(|(&t, &v)| (t, v))
                .collect(),
            solar: drivers
                .solar
                .iter()
                .filter(|e| keep(*e.0))
                .map(|(&t, &v)| (t, v))
                .collect(),
        };
        let past = split(&|t| t < today);
        let ahead = split(&|t| t >= today);
        let f = WholesaleForecaster::new(&known, past, ahead, TZ, now);
        assert_eq!(f.method(), Method::Regression);
        for t in (today + 2 * 86_400..today + 4 * 86_400).step_by(3600) {
            let (v, corrected) = f.forecast(t).unwrap();
            assert!(corrected && v.is_finite(), "{t}");
        }
    }

    #[test]
    fn drivers_fill_gaps_from_another_set() {
        let a = Drivers::from_rows(&[(0, Some(0.5), None), (3600, Some(0.6), Some(0.1))]);
        let b = Drivers::from_rows(&[(0, Some(0.9), Some(0.2)), (7200, Some(0.7), None)]);
        let c = a.filled_from(&b);
        assert_eq!(
            c.to_rows(),
            vec![
                (0, Some(0.5), Some(0.2)),
                (3600, Some(0.6), Some(0.1)),
                (7200, Some(0.7), None)
            ]
        );
        assert_eq!(
            Drivers::default().filled_from(&Drivers::default()),
            Drivers::default()
        );
    }

    #[test]
    fn training_uses_no_data_after_now() {
        let (prices, drivers) = windy_market(90);
        let now = START + 70 * 86_400 + 10 * 3600;
        let known: Vec<(i64, f64)> = prices.iter().copied().filter(|&(t, _)| t < now).collect();
        let mut later = known.clone();
        later.extend(
            prices
                .iter()
                .copied()
                .filter(|&(t, _)| t >= now)
                .map(|(t, _)| (t, 9.99)),
        );
        let a = WholesaleForecaster::new(&known, drivers.clone(), drivers.clone(), TZ, now);
        let b = WholesaleForecaster::new(&later, drivers.clone(), drivers, TZ, now);
        assert_eq!(a.model(), b.model());
        assert_eq!(a.check(), b.check());
    }
}
