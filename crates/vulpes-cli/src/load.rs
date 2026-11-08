//! Load reconstruction and evaluation commands.

use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, TimeZone};
use chrono_tz::Tz;
use std::collections::HashMap;
use vulpes_core::{SeriesKind, Store};
use vulpes_load::reconstruct::Balance;
use vulpes_load::temperature::{Correction, DegreeBases, Sample};
use vulpes_load::{Country, ProfileForecaster, ProfileOptions};

pub(crate) const LOAD_KEY: &str = "load.house";

/// First series (in priority order) that has a value at each hour.
fn merged(store: &Store, keys: &[String], from: i64, to: i64) -> Result<HashMap<i64, f64>> {
    let mut out = HashMap::new();
    for k in keys.iter().rev() {
        out.extend(store.samples(k, 3600, from, to)?);
    }
    Ok(out)
}

pub(crate) struct LoadSources {
    pub grid_import: Vec<String>,
    pub grid_export: Vec<String>,
    /// Energy series in kWh per hour.
    pub energy_kwh: bool,
    pub pv: String,
    pub battery_in: Option<String>,
    pub battery_out: Option<String>,
    /// Battery power (W, hourly mean) used where the energy counters are missing.
    pub battery_power: Option<String>,
    /// +1 if `battery_power` is positive when charging, −1 if positive when discharging.
    pub battery_power_sign: f64,
}

/// Hourly inputs for load reconstruction, keyed by hour start.
struct Hourly {
    grid_import: HashMap<i64, f64>,
    grid_export: HashMap<i64, f64>,
    /// PV in W (hourly mean).
    pv: HashMap<i64, f64>,
    battery_in: HashMap<i64, f64>,
    battery_out: HashMap<i64, f64>,
    /// Battery power in W (hourly mean), the fallback where the counters are missing.
    battery_power: HashMap<i64, f64>,
}

/// House load per hour (W), excluding the battery. Grid and battery energy
/// are multiplied by `scale` (1000 for kWh per hour). Per hour, the battery
/// energy counters win; battery power (times `sign`) fills their gaps; hours
/// with neither are skipped when the site has a battery. Returns the load
/// and the number of hours that used the power fallback.
fn reconstruct(h: &Hourly, scale: f64, has_battery: bool, sign: f64) -> (Vec<(i64, f64)>, usize) {
    let mut hours: Vec<i64> = h.grid_import.keys().copied().collect();
    hours.sort_unstable();
    let (mut out, mut fallback) = (Vec::new(), 0);
    for t in hours {
        let (Some(&i), Some(&e), Some(&p)) = (h.grid_import.get(&t), h.grid_export.get(&t), h.pv.get(&t))
        else {
            continue;
        };
        let battery = match (
            h.battery_in.get(&t),
            h.battery_out.get(&t),
            h.battery_power.get(&t),
        ) {
            (Some(&a), Some(&b), _) => Some((a - b) * scale),
            (_, _, Some(&w)) => {
                fallback += 1;
                Some(w * sign)
            }
            _ if !has_battery => Some(0.0),
            _ => None,
        };
        let Some(battery_net_in) = battery else { continue };
        let bal = Balance {
            grid_import: i * scale,
            grid_export: e * scale,
            pv: p,
            battery_net_in,
        };
        out.push((t, bal.load()));
    }
    (out, fallback)
}

pub(crate) fn build(store: &mut Store, src: &LoadSources, from: NaiveDate) -> Result<()> {
    let from = from
        .and_hms_opt(0, 0, 0)
        .context("bad date")?
        .and_utc()
        .timestamp();
    let to = i64::MAX / 2;
    let scale = if src.energy_kwh { 1000.0 } else { 1.0 };
    let gi = merged(store, &src.grid_import, from, to)?;
    let ge = merged(store, &src.grid_export, from, to)?;
    let pv: HashMap<i64, f64> = store.samples(&src.pv, 3600, from, to)?.into_iter().collect();
    let get = |k: &Option<String>| -> Result<HashMap<i64, f64>> {
        Ok(match k {
            Some(k) => store.samples(k, 3600, from, to)?.into_iter().collect(),
            None => HashMap::new(),
        })
    };
    let (bi, bo, bp) = (
        get(&src.battery_in)?,
        get(&src.battery_out)?,
        get(&src.battery_power)?,
    );
    let has_battery = src.battery_in.is_some() || src.battery_power.is_some();
    let hourly = Hourly {
        grid_import: gi,
        grid_export: ge,
        pv,
        battery_in: bi,
        battery_out: bo,
        battery_power: bp,
    };
    let (out, fallback) = reconstruct(&hourly, scale, has_battery, src.battery_power_sign);
    if out.is_empty() {
        bail!("no complete hours found for the given series");
    }
    store.ensure_series(LOAD_KEY, "W", SeriesKind::Mean)?;
    let n = store.put_samples(LOAD_KEY, 3600, &out)?;
    println!("{LOAD_KEY}: {n} hours ({fallback} using battery power fallback)");
    Ok(())
}

#[derive(Default)]
struct Score {
    n: usize,
    abs: f64,
    sq: f64,
    bias: f64,
    day_abs: f64,
    days: usize,
}

impl Score {
    fn add(&mut self, f: f64, a: f64) {
        let e = f - a;
        self.n += 1;
        self.abs += e.abs();
        self.sq += e * e;
        self.bias += e;
    }
}

pub(crate) fn evaluate(
    store: &Store,
    from: NaiveDate,
    to: NaiveDate,
    tz: Tz,
    country: Country,
    temp_forecast: &str,
    temp_history: &str,
) -> Result<()> {
    let all: Vec<(i64, f64)> = store.samples(LOAD_KEY, 3600, 0, i64::MAX / 2)?;
    if all.is_empty() {
        bail!("no '{LOAD_KEY}' series; run build-load first");
    }
    // Day-ahead temperature for target hours; short-lead archive for reference days.
    let temps = Temperatures {
        forecast: crate::series::hourly_forecast(store, temp_forecast, "temperature_2m")?,
        history: crate::series::hourly_forecast(store, temp_history, "temperature_2m")?,
    };
    let e = evaluate_days(&all, &temps, from, to, tz, country)?;
    let bases = DegreeBases::default();
    println!(
        "temperature correction (all data): +{:.0} W per °C above {} °C, +{:.0} W per °C below {} °C",
        e.correction.beta_cool, bases.cool_base, e.correction.beta_heat, bases.heat_base
    );
    println!(
        "{} hours, mean load {:.0} W; forecasts issued 12:00 UTC the day before",
        e.hours, e.mean_load
    );
    println!(
        "{:34} {:>14} {:>9} {:>8} {:>16}",
        "method", "hourly MAE", "RMSE", "bias", "daily MAE"
    );
    for (name, s) in LOAD_METHODS.iter().zip(&e.scores) {
        let n = s.n.max(1) as f64;
        println!(
            "{name:34} {:5.0} W ({:4.1}%) {:7.0} W {:+6.0} W {:6.2} kWh/day",
            s.abs / n,
            100.0 * s.abs / n / e.mean_load,
            (s.sq / n).sqrt(),
            s.bias / n,
            s.day_abs / s.days.max(1) as f64
        );
    }
    Ok(())
}

/// Hourly temperature (°C) keyed by hour start: day-ahead forecasts for the
/// target hours, and the short-lead archive for reference days.
pub(crate) struct Temperatures {
    pub forecast: HashMap<i64, f64>,
    pub history: HashMap<i64, f64>,
}

const LOAD_METHODS: [&str; 7] = [
    "last known day, same hour",
    "same hour last week",
    "profile, mean",
    "profile, median",
    "profile, mean + last year 30%",
    "profile, median + last year 30%",
    "profile, mean + temperature",
];

struct LoadEval {
    /// One score per entry of [`LOAD_METHODS`].
    scores: Vec<Score>,
    hours: usize,
    mean_load: f64,
    /// Temperature correction fitted on all evaluated days.
    correction: Correction,
}

/// Score every load forecasting method on each complete local day in
/// `[from, to]`, with forecasts issued 12:00 UTC the day before. Only data
/// before that moment is used, also for fitting the temperature correction.
fn evaluate_days(
    all: &[(i64, f64)],
    temps: &Temperatures,
    from: NaiveDate,
    to: NaiveDate,
    tz: Tz,
    country: Country,
) -> Result<LoadEval> {
    let (t_fc, t_hist) = (&temps.forecast, &temps.history);
    let bases = DegreeBases::default();
    let mut past: Vec<(i64, Sample)> = Vec::new();
    let local_midnight = |d: NaiveDate| -> Result<i64> {
        Ok(tz
            .from_local_datetime(&d.and_hms_opt(0, 0, 0).context("bad date")?)
            .earliest()
            .context("tz")?
            .timestamp())
    };
    let actual: HashMap<i64, f64> = all.iter().copied().collect();
    let methods: [Option<ProfileOptions>; LOAD_METHODS.len()] = [
        None,
        None,
        Some(ProfileOptions::default()),
        Some(ProfileOptions {
            median: true,
            ..Default::default()
        }),
        Some(ProfileOptions {
            last_year_weight: 0.3,
            ..Default::default()
        }),
        Some(ProfileOptions {
            median: true,
            last_year_weight: 0.3,
            ..Default::default()
        }),
        Some(ProfileOptions::default()),
    ];
    let temp_method = methods.len() - 1;
    let mut scores: Vec<Score> = methods.iter().map(|_| Score::default()).collect();
    let mut day = from;
    let (mut sum_actual, mut n_actual) = (0.0, 0usize);
    while day <= to {
        let (start, end) = (
            local_midnight(day)?,
            local_midnight(day + chrono::Duration::days(1))?,
        );
        // Issued at 12:00 UTC the day before; only data before that is used.
        let cutoff = start - start.rem_euclid(86_400) - 12 * 3600;
        let history: Vec<(i64, f64)> = all.iter().copied().filter(|&(t, _)| t < cutoff).collect();
        let hours: Vec<i64> = (0..).map(|k| start + k * 3600).take_while(|&t| t < end).collect();
        if !hours.iter().all(|t| actual.contains_key(t)) {
            day += chrono::Duration::days(1);
            continue;
        }
        let profiles: Vec<Option<ProfileForecaster>> = methods
            .iter()
            .map(|o| o.map(|o| ProfileForecaster::new(&history, tz, country, o)))
            .collect();
        let mut day_f = vec![0.0; methods.len()];
        let mut day_a = 0.0;
        // Temperature correction fitted on days whose data was complete before the cutoff.
        let train: Vec<Sample> = past
            .iter()
            .filter(|(end, _)| *end <= cutoff)
            .map(|(_, s)| *s)
            .collect();
        let correction = Correction::fit(&train, 100.0);
        for &t in &hours {
            let a = actual[&t];
            // Last known day at the same hour: step back a day until before the cutoff.
            let mut back = t - 86_400;
            while back >= cutoff {
                back -= 86_400;
            }
            let fs = [actual.get(&back).copied(), actual.get(&(t - 7 * 86_400)).copied()];
            for (m, p) in profiles.iter().enumerate() {
                let f = match p {
                    Some(p) if m == temp_method => {
                        let cdh = |h: i64| t_hist.get(&h).map(|&x| bases.cdh(x));
                        let hdh = |h: i64| t_hist.get(&h).map(|&x| bases.hdh(x));
                        match (p.forecast_with(t, &cdh), p.forecast_with(t, &hdh), t_fc.get(&t)) {
                            (Some((v, Some(c_ref))), Some((_, Some(h_ref))), Some(&temp)) => {
                                let (dc, dh) = (bases.cdh(temp) - c_ref, bases.hdh(temp) - h_ref);
                                past.push((
                                    end,
                                    Sample {
                                        error: a - v,
                                        d_cdh: dc,
                                        d_hdh: dh,
                                    },
                                ));
                                Some((v + correction.apply(dc, dh)).max(0.0))
                            }
                            _ => p.forecast(t),
                        }
                    }
                    Some(p) => p.forecast(t),
                    None => fs[m],
                };
                if let Some(f) = f {
                    scores[m].add(f, a);
                    day_f[m] += f;
                }
            }
            day_a += a;
            sum_actual += a;
            n_actual += 1;
        }
        for (m, s) in scores.iter_mut().enumerate() {
            s.day_abs += (day_f[m] - day_a).abs() / 1000.0;
            s.days += 1;
        }
        day += chrono::Duration::days(1);
    }
    if n_actual == 0 {
        bail!("no complete days with load data between {from} and {to}");
    }
    Ok(LoadEval {
        scores,
        hours: n_actual,
        mean_load: sum_actual / n_actual as f64,
        correction: Correction::fit(&past.iter().map(|(_, s)| *s).collect::<Vec<_>>(), 100.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(v: &[(i64, f64)]) -> HashMap<i64, f64> {
        v.iter().copied().collect()
    }

    fn hourly() -> Hourly {
        // Hours 0..4 h; kWh per hour for grid and battery counters, W for PV and power.
        Hourly {
            grid_import: map(&[(0, 1.0), (3600, 0.2), (7200, 0.5), (10_800, 0.4), (14_400, 0.3)]),
            grid_export: map(&[(0, 0.0), (3600, 0.5), (7200, 0.0), (10_800, 0.0), (14_400, 0.0)]),
            pv: map(&[
                (0, 0.0),
                (3600, 2000.0),
                (7200, 0.0),
                (10_800, 0.0),
                (14_400, 0.0),
            ]),
            battery_in: map(&[(0, 0.5), (3600, 1.0)]),
            battery_out: map(&[(0, 0.0), (3600, 0.0), (10_800, 0.2)]),
            // Hour 2 has only power (+ = discharging here); hour 3 has a half pair; hour 4 nothing.
            battery_power: map(&[(7200, 300.0), (3600, 999.0)]),
        }
    }

    #[test]
    fn counters_win_power_fills_gaps_and_incomplete_hours_are_skipped() {
        let (load, fallback) = reconstruct(&hourly(), 1000.0, true, -1.0);
        // h0: 1000 import − 500 into battery = 500 W.
        // h1: 200 + 2000 − 500 − 1000 = 700 W (counters win over the power value).
        // h2: 500 import + 300 W from the battery (power × sign −1 = −300 net in) = 800 W.
        // h3 has only battery_out and no power: skipped. h4 has nothing: skipped.
        assert_eq!(load, vec![(0, 500.0), (3600, 700.0), (7200, 800.0)]);
        assert_eq!(fallback, 1);
    }

    #[test]
    fn without_a_battery_every_complete_hour_counts() {
        let mut h = hourly();
        h.battery_in.clear();
        h.battery_out.clear();
        h.battery_power.clear();
        let (load, fallback) = reconstruct(&h, 1000.0, false, 1.0);
        assert_eq!(load.len(), 5);
        assert_eq!(load[1], (3600, 1700.0));
        assert_eq!(fallback, 0);
    }

    #[test]
    fn missing_grid_or_pv_skips_the_hour_and_load_is_never_negative() {
        let mut h = hourly();
        h.pv.remove(&0);
        h.grid_export.insert(7200, 5.0); // impossible: exports more than it has
        let (load, _) = reconstruct(&h, 1000.0, true, -1.0);
        assert!(load.iter().all(|&(t, _)| t != 0));
        assert!(load.iter().all(|&(_, w)| w >= 0.0));
    }

    #[test]
    fn energy_in_watt_hours_needs_no_scaling() {
        let h = Hourly {
            grid_import: map(&[(0, 800.0)]),
            grid_export: map(&[(0, 0.0)]),
            pv: map(&[(0, 100.0)]),
            battery_in: HashMap::new(),
            battery_out: HashMap::new(),
            battery_power: HashMap::new(),
        };
        assert_eq!(reconstruct(&h, 1.0, false, 1.0).0, vec![(0, 900.0)]);
    }

    #[test]
    fn merged_prefers_earlier_series_per_hour() {
        let mut store = Store::open_in_memory().unwrap();
        for k in ["p1", "backup"] {
            store.ensure_series(k, "kWh", SeriesKind::Sum).unwrap();
        }
        store.put_samples("p1", 3600, &[(0, 1.0), (3600, 2.0)]).unwrap();
        store
            .put_samples("backup", 3600, &[(3600, 9.0), (7200, 3.0)])
            .unwrap();
        let m = merged(&store, &["p1".into(), "backup".into()], 0, 10_000).unwrap();
        assert_eq!(m, map(&[(0, 1.0), (3600, 2.0), (7200, 3.0)]));
        assert!(merged(&store, &[], 0, 10_000).unwrap().is_empty());
    }

    #[test]
    fn score_accumulates_errors() {
        let mut s = Score::default();
        s.add(110.0, 100.0);
        s.add(80.0, 100.0);
        assert_eq!((s.n, s.abs, s.sq, s.bias), (2, 30.0, 500.0, -10.0));
    }

    const TZ: Tz = chrono_tz::Europe::Amsterdam;

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    /// Hourly series from 1 Jan 2026 for `days` days.
    fn series(days: i64, f: impl Fn(i64) -> f64) -> Vec<(i64, f64)> {
        let start = 1_767_225_600; // 2026-01-01 00:00 UTC
        (0..24 * days)
            .map(|k| start + k * 3600)
            .map(|t| (t, f(t)))
            .collect()
    }

    fn no_temps() -> Temperatures {
        Temperatures {
            forecast: HashMap::new(),
            history: HashMap::new(),
        }
    }

    #[test]
    fn constant_load_is_forecast_perfectly() {
        let all = series(60, |_| 500.0);
        let e = evaluate_days(
            &all,
            &no_temps(),
            date("2026-02-01"),
            date("2026-02-07"),
            TZ,
            Country::None,
        )
        .unwrap();
        assert_eq!(e.hours, 7 * 24);
        assert_eq!(e.mean_load, 500.0);
        for (name, s) in LOAD_METHODS.iter().zip(&e.scores) {
            assert_eq!(s.n, 7 * 24, "{name}");
            assert!(s.abs < 1e-6, "{name}: {}", s.abs);
        }
        assert_eq!(e.correction, Correction::NONE);
    }

    #[test]
    fn data_after_the_issue_time_is_never_used() {
        // Forecasts for 10 Feb are issued 9 Feb 12:00 UTC. Changing the load
        // between then and the start of 10 Feb must not change its scores.
        let base = series(60, |t| 300.0 + ((t / 3600) % 24) as f64 * 10.0);
        let target = date("2026-02-10");
        let start = TZ
            .from_local_datetime(&target.and_hms_opt(0, 0, 0).unwrap())
            .unwrap()
            .timestamp();
        let cutoff = start - start.rem_euclid(86_400) - 12 * 3600;
        let mut changed = base.clone();
        for (t, v) in &mut changed {
            if (cutoff..start).contains(t) {
                *v += 5000.0;
            }
        }
        let run = |all: &[(i64, f64)]| {
            let e = evaluate_days(all, &no_temps(), target, target, TZ, Country::None).unwrap();
            e.scores.iter().map(|s| (s.abs, s.sq, s.bias)).collect::<Vec<_>>()
        };
        assert_eq!(run(&base), run(&changed));
        // Data after the evaluated period doesn't matter either.
        let mut later = base.clone();
        for (t, v) in &mut later {
            if *t >= start + 86_400 {
                *v = 0.0;
            }
        }
        assert_eq!(run(&base), run(&later));
    }

    #[test]
    fn incomplete_days_are_skipped_and_no_days_is_an_error() {
        let mut all = series(40, |_| 400.0);
        let hole = TZ
            .from_local_datetime(&date("2026-02-03").and_hms_opt(13, 0, 0).unwrap())
            .unwrap()
            .timestamp();
        all.retain(|&(t, _)| t != hole);
        let e = evaluate_days(
            &all,
            &no_temps(),
            date("2026-02-01"),
            date("2026-02-05"),
            TZ,
            Country::None,
        )
        .unwrap();
        assert_eq!(e.hours, 4 * 24);
        let err = evaluate_days(
            &all,
            &no_temps(),
            date("2027-01-01"),
            date("2027-01-02"),
            TZ,
            Country::None,
        );
        assert!(err.is_err());
    }

    #[test]
    fn temperature_correction_learns_cooling_load() {
        // A heatwave pattern: daily temperature cycles between days; each °C
        // above 18 adds 50 W. The plain profile lags; the corrected one doesn't.
        let temp = |t: i64| {
            let day = t / 86_400;
            let hour = ((t / 3600) % 24) as f64;
            16.0 + ((day * 7) % 11) as f64 + 4.0 * (std::f64::consts::PI * (hour - 6.0) / 12.0).sin().max(0.0)
        };
        let all = series(100, |t| 400.0 + 50.0 * (temp(t) - 18.0).max(0.0));
        let temps: HashMap<i64, f64> = all.iter().map(|&(t, _)| (t, temp(t))).collect();
        let temps = Temperatures {
            forecast: temps.clone(),
            history: temps,
        };
        let e = evaluate_days(
            &all,
            &temps,
            date("2026-02-15"),
            date("2026-04-05"),
            TZ,
            Country::None,
        )
        .unwrap();
        let mae = |name: &str| {
            let s = &e.scores[LOAD_METHODS.iter().position(|m| *m == name).unwrap()];
            s.abs / s.n as f64
        };
        assert!(
            mae("profile, mean + temperature") < 0.5 * mae("profile, mean"),
            "{} vs {}",
            mae("profile, mean + temperature"),
            mae("profile, mean")
        );
        assert!((e.correction.beta_cool - 50.0).abs() < 10.0, "{:?}", e.correction);
    }
}
