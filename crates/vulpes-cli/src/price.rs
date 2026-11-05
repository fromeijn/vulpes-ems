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
        let mut by_year: BTreeMap<i32, Vec<TariffSample>> = BTreeMap::new();
        for (t, &c) in &cost {
            if let (Some(&e), Some(&p)) = (energy.get(t).filter(|&&e| e >= min_energy), w.get(t)) {
                let year = DateTime::from_timestamp(*t, 0).map_or(0, |d| d.year());
                by_year.entry(year).or_default().push(TariffSample {
                    energy_kwh: e,
                    wholesale: p,
                    cost: c,
                });
            }
        }
        for (year, samples) in by_year {
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

/// Hourly mean of a 15-minute forecast variable at a labelled reference point.
fn hourly_var(store: &Store, source: &str, var: &str) -> Result<HashMap<i64, f64>> {
    let mut acc: HashMap<i64, (f64, u32)> = HashMap::new();
    for (t, v) in store.forecasts_asof(source, var, 0, i64::MAX / 2, i64::MAX)? {
        let e = acc.entry(t - t.rem_euclid(3600)).or_default();
        e.0 += v;
        e.1 += 1;
    }
    Ok(acc
        .into_iter()
        .filter(|(_, (_, n))| *n == 4)
        .map(|(h, (s, n))| (h, s / n as f64))
        .collect())
}

/// Normalised wind turbine power curve: cubic from 3 m/s cut-in to 12 m/s rated.
fn wind_power(v: f64) -> f64 {
    ((v.powi(3) - 27.0) / (1728.0 - 27.0)).clamp(0.0, 1.0)
}

/// Mean of a per-point transform over all points, if every point has data.
fn index(points: &[HashMap<i64, f64>], h: i64, f: impl Fn(f64) -> f64) -> Option<f64> {
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
        .map(|s| hourly_var(store, s, "wind_speed_100m"))
        .collect::<Result<_>>()?;
    let solar: Vec<HashMap<i64, f64>> = points
        .solar
        .iter()
        .map(|s| hourly_var(store, s, "shortwave_radiation"))
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
