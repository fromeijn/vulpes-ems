//! Tariff fitting and price forecast evaluation commands.

use anyhow::{Result, bail};
use chrono::{DateTime, Datelike};
use std::collections::{BTreeMap, HashMap};
use vulpes_core::Store;
use vulpes_price::tariff::{TariffSample, fit};

fn series(store: &Store, key: &str) -> Result<HashMap<i64, f64>> {
    Ok(store.samples(key, 3600, 0, i64::MAX / 2)?.into_iter().collect())
}

/// Fit `all_in = a · wholesale + b` per calendar year, for each (cost, energy) pair.
pub(crate) fn fit_tariff(
    store: &Store,
    pairs: &[(String, String)],
    wholesale: &str,
    min_energy: f64,
) -> Result<()> {
    let w = series(store, wholesale)?;
    if w.is_empty() {
        bail!("no wholesale series '{wholesale}'");
    }
    println!(
        "{:42} {:>5} {:>6} {:>8} {:>8} {:>9}",
        "cost / energy", "year", "hours", "a", "b", "rms (ct)"
    );
    for (cost_key, energy_key) in pairs {
        let (cost, energy) = (series(store, cost_key)?, series(store, energy_key)?);
        for (year, samples) in samples_by_year(&cost, &energy, &w, min_energy) {
            match fit(&samples) {
                Some(f) => println!(
                    "{:42} {year:5} {:6} {:8.4} {:8.4} {:9.2}",
                    format!("{cost_key} / {energy_key}"),
                    f.samples,
                    f.tariff.a,
                    f.tariff.b,
                    100.0 * f.rms_error
                ),
                None => println!("{cost_key} / {energy_key} {year}: not enough data"),
            }
        }
    }
    Ok(())
}

/// Hourly tariff samples per UTC calendar year: hours with a cost, at least
/// `min_energy` kWh, and a wholesale price.
fn samples_by_year(
    cost: &HashMap<i64, f64>,
    energy: &HashMap<i64, f64>,
    wholesale: &HashMap<i64, f64>,
    min_energy: f64,
) -> BTreeMap<i32, Vec<TariffSample>> {
    let mut hours: Vec<i64> = cost.keys().copied().collect();
    hours.sort_unstable();
    let mut by_year: BTreeMap<i32, Vec<TariffSample>> = BTreeMap::new();
    for t in hours {
        if let (Some(&e), Some(&p)) = (energy.get(&t).filter(|&&e| e >= min_energy), wholesale.get(&t)) {
            let year = DateTime::from_timestamp(t, 0).map_or(0, |d| d.year());
            by_year.entry(year).or_default().push(TariffSample {
                energy_kwh: e,
                wholesale: p,
                cost: cost[&t],
            });
        }
    }
    by_year
}

/// Normalised wind turbine power curve: cubic from 3 m/s cut-in to 12 m/s rated.
fn wind_power(v: f64) -> f64 {
    ((v.powi(3) - 27.0) / (1728.0 - 27.0)).clamp(0.0, 1.0)
}

/// Mean of a per-point transform over all points, if every point has data.
fn index(points: &[HashMap<i64, f64>], h: i64, f: impl Fn(f64) -> f64) -> Option<f64> {
    if points.is_empty() {
        return None;
    }
    let vals: Option<Vec<f64>> = points.iter().map(|p| p.get(&h).map(|&v| f(v))).collect();
    vals.map(|v| v.iter().sum::<f64>() / v.len() as f64)
}

#[derive(Default)]
struct PriceScore {
    n: usize,
    abs: f64,
    sq: f64,
    bias: f64,
    rho: f64,
    days: usize,
    cheap_hits: usize,
    dear_hits: usize,
}

/// Spearman rank correlation (no tie correction; prices rarely tie exactly).
fn spearman(a: &[f64], b: &[f64]) -> f64 {
    let ranks = |v: &[f64]| {
        let mut idx: Vec<usize> = (0..v.len()).collect();
        idx.sort_by(|&i, &j| v[i].total_cmp(&v[j]));
        let mut r = vec![0.0; v.len()];
        for (rank, i) in idx.into_iter().enumerate() {
            r[i] = rank as f64;
        }
        r
    };
    let (ra, rb) = (ranks(a), ranks(b));
    let n = a.len() as f64;
    let d2: f64 = ra.iter().zip(&rb).map(|(x, y)| (x - y).powi(2)).sum();
    1.0 - 6.0 * d2 / (n * (n * n - 1.0))
}

/// How many of the `k` lowest (or highest) actual hours the forecast also ranks there.
fn overlap(f: &[f64], a: &[f64], k: usize, lowest: bool) -> usize {
    let pick = |v: &[f64]| {
        let mut idx: Vec<usize> = (0..v.len()).collect();
        idx.sort_by(|&i, &j| {
            if lowest {
                v[i].total_cmp(&v[j])
            } else {
                v[j].total_cmp(&v[i])
            }
        });
        idx.truncate(k);
        idx
    };
    let (pf, pa) = (pick(f), pick(a));
    pf.iter().filter(|i| pa.contains(i)).count()
}

pub(crate) struct PricePoints {
    pub wind: Vec<String>,
    pub solar: Vec<String>,
}

/// Evaluate day-ahead wholesale forecasts issued 10:00 UTC the day before
/// (before the auction): prices up to the end of the issue day are known.
pub(crate) fn evaluate(
    store: &Store,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
    tz: chrono_tz::Tz,
    points: &PricePoints,
    wholesale: &str,
) -> Result<()> {
    use chrono::TimeZone;
    use vulpes_load::{Country, ProfileForecaster, ProfileOptions};
    use vulpes_ml::{recency_weight, ridge::Ridge};

    let all: Vec<(i64, f64)> = store.samples(wholesale, 3600, 0, i64::MAX / 2)?;
    let actual: HashMap<i64, f64> = all.iter().copied().collect();
    let wind: Vec<HashMap<i64, f64>> = points
        .wind
        .iter()
        .map(|s| crate::series::hourly_forecast(store, s, "wind_speed_100m"))
        .collect::<Result<_>>()?;
    let solar: Vec<HashMap<i64, f64>> = points
        .solar
        .iter()
        .map(|s| crate::series::hourly_forecast(store, s, "shortwave_radiation"))
        .collect::<Result<_>>()?;
    let wind_idx = |h: i64| index(&wind, h, wind_power);
    let solar_idx = |h: i64| index(&solar, h, |g| g / 1000.0);
    let opt = ProfileOptions {
        lookback_days: 28,
        half_life_days: 7.0,
        ..Default::default()
    };
    let names = [
        "copy previous day",
        "profile (28 d, half-life 7 d)",
        "profile + wind/solar regression",
    ];
    let mut scores: Vec<PriceScore> = names.iter().map(|_| PriceScore::default()).collect();
    // Past rows for the regression: (day start, features, residual).
    let mut past: Vec<(i64, Vec<f64>, f64)> = Vec::new();
    let midnight = |d: chrono::NaiveDate| {
        tz.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .map(|t| t.timestamp())
    };
    let mut day = from;
    let mut last_model: Option<Ridge> = None;
    while day <= to {
        let (Some(start), Some(end)) = (midnight(day), midnight(day + chrono::Duration::days(1))) else {
            break;
        };
        let hours: Vec<i64> = (0..).map(|k| start + k * 3600).take_while(|&t| t < end).collect();
        let next = day + chrono::Duration::days(1);
        if !hours.iter().all(|t| actual.contains_key(t)) {
            day = next;
            continue;
        }
        // Known at issue time: all prices before the target day.
        let history: Vec<(i64, f64)> = all.iter().copied().filter(|&(t, _)| t < start).collect();
        let profile = ProfileForecaster::new(&history, tz, Country::None, opt);
        let rows: Vec<(Vec<f64>, f64, f64)> = past
            .iter()
            .map(|(d, x, r)| (x.clone(), *r, recency_weight((start - d) as f64 / 86_400.0, 30.0)))
            .collect();
        let model = (rows.len() >= 24 * 14).then(|| Ridge::fit(&rows, 1e-3)).flatten();
        let mut f: Vec<Vec<f64>> = vec![Vec::new(); names.len()];
        let mut a = Vec::new();
        let mut ok = true;
        for &t in &hours {
            let prev = actual.get(&(t - 86_400)).copied();
            let (Some((p, w_ref)), Some((_, s_ref))) = (
                profile.forecast_with(t, &wind_idx),
                profile.forecast_with(t, &solar_idx),
            ) else {
                ok = false;
                break;
            };
            let x = match (wind_idx(t), solar_idx(t), w_ref, s_ref) {
                (Some(w), Some(s), Some(wr), Some(sr)) => Some(vec![w - wr, s - sr]),
                _ => None,
            };
            let corrected = match (&model, &x) {
                (Some(m), Some(x)) => p + m.predict(x),
                _ => p,
            };
            if let Some(x) = x {
                past.push((start, x, actual[&t] - p));
            }
            f[0].push(prev.unwrap_or(p));
            f[1].push(p);
            f[2].push(corrected);
            a.push(actual[&t]);
        }
        if ok && model.is_some() {
            for (m, s) in scores.iter_mut().enumerate() {
                for (fv, av) in f[m].iter().zip(&a) {
                    let e = fv - av;
                    s.n += 1;
                    s.abs += e.abs();
                    s.sq += e * e;
                    s.bias += e;
                }
                s.rho += spearman(&f[m], &a);
                s.cheap_hits += overlap(&f[m], &a, 4, true);
                s.dear_hits += overlap(&f[m], &a, 4, false);
                s.days += 1;
            }
            last_model = model;
        }
        day = next;
    }
    let d = scores[0].days.max(1) as f64;
    println!(
        "{} days evaluated (after a 14-day regression warm-up); forecasts issued 10:00 UTC the day before",
        scores[0].days
    );
    println!(
        "{:34} {:>9} {:>9} {:>8} {:>10} {:>12} {:>12}",
        "method", "MAE ct", "RMSE ct", "bias", "rank corr", "cheapest-4", "priciest-4"
    );
    for (name, s) in names.iter().zip(&scores) {
        let n = s.n.max(1) as f64;
        println!(
            "{name:34} {:9.2} {:9.2} {:+8.2} {:10.2} {:9.2} / 4 {:9.2} / 4",
            100.0 * s.abs / n,
            100.0 * (s.sq / n).sqrt(),
            100.0 * s.bias / n,
            s.rho / d,
            s.cheap_hits as f64 / d,
            s.dear_hits as f64 / d
        );
    }
    if let Some(m) = last_model {
        println!(
            "latest regression: {:+.1} ct per unit wind-power index, {:+.1} ct per kW/m² solar (vs reference days)",
            100.0 * m.coef[0],
            100.0 * m.coef[1]
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn spearman_rank_correlation() {
        let a = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert!((spearman(&a, &[10.0, 20.0, 30.0, 40.0, 50.0]) - 1.0).abs() < 1e-12);
        assert!((spearman(&a, &[5.0, 4.0, 3.0, 2.0, 1.0]) + 1.0).abs() < 1e-12);
        // Monotonic but non-linear is still a perfect rank match.
        assert!((spearman(&a, &[1.0, 8.0, 27.0, 64.0, 125.0]) - 1.0).abs() < 1e-12);
        let r = spearman(&a, &[2.0, 1.0, 4.0, 3.0, 5.0]);
        assert!((r - 0.8).abs() < 1e-12, "{r}");
    }

    #[test]
    fn overlap_of_cheapest_and_priciest_hours() {
        let actual = [0.1, 0.5, 0.2, 0.9, 0.3, 0.8];
        let forecast = [0.2, 0.95, 0.1, 0.7, 0.5, 0.6];
        assert_eq!(overlap(&forecast, &actual, 2, true), 2, "both pick hours 0 and 2");
        assert_eq!(
            overlap(&forecast, &actual, 2, false),
            1,
            "forecast 1+3, actual 3+5"
        );
        assert_eq!(
            overlap(&forecast, &actual, 4, true),
            3,
            "forecast adds hour 5, actual hour 1"
        );
        assert_eq!(overlap(&forecast, &actual, 0, true), 0);
        assert_eq!(overlap(&forecast, &actual, 10, true), 6, "k larger than the day");
    }

    #[test]
    fn tariff_samples_are_filtered_and_grouped_by_year() {
        // 2025-12-31 23:00 UTC and 2026-01-01 00:00/01:00 UTC.
        let (h1, h2, h3) = (1_767_222_000, 1_767_225_600, 1_767_229_200);
        let cost: HashMap<i64, f64> = [(h1, 0.5), (h2, 0.6), (h3, 0.7)].into();
        let energy: HashMap<i64, f64> = [(h1, 2.0), (h2, 0.05), (h3, 3.0)].into();
        let wholesale: HashMap<i64, f64> = [(h1, 0.1), (h3, 0.2)].into();
        let by_year = samples_by_year(&cost, &energy, &wholesale, 0.1);
        assert_eq!(by_year.keys().copied().collect::<Vec<_>>(), [2025, 2026]);
        assert_eq!(by_year[&2025].len(), 1);
        // h2 is below the minimum energy; h3 is kept.
        let s = &by_year[&2026];
        assert_eq!(
            (s.len(), s[0].energy_kwh, s[0].wholesale, s[0].cost),
            (1, 3.0, 0.2, 0.7)
        );
    }
}
