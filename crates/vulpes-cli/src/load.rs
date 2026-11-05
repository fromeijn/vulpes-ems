//! Load reconstruction and evaluation commands.

use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, TimeZone};
use chrono_tz::Tz;
use std::collections::HashMap;
use vulpes_core::{SeriesKind, Store};
use vulpes_load::reconstruct::Balance;
use vulpes_load::temperature::{Correction, DegreeBases, Sample};
use vulpes_load::{Country, ProfileForecaster, ProfileOptions};

/// Hourly mean of a 15-minute forecast variable, keyed by hour start.
fn hourly_temp(store: &Store, source: &str) -> Result<HashMap<i64, f64>> {
    let mut acc: HashMap<i64, (f64, u32)> = HashMap::new();
    for (t, v) in store.forecasts_asof(source, "temperature_2m", 0, i64::MAX / 2, i64::MAX)? {
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
    let mut hours: Vec<i64> = gi.keys().copied().collect();
    hours.sort();
    let (mut out, mut fallback) = (Vec::new(), 0);
    for t in hours {
        let (Some(&i), Some(&e), Some(&p)) = (gi.get(&t), ge.get(&t), pv.get(&t)) else {
            continue;
        };
        let battery = match (bi.get(&t), bo.get(&t), bp.get(&t)) {
            (Some(&a), Some(&b), _) => Some((a - b) * scale),
            (_, _, Some(&w)) => {
                fallback += 1;
                Some(w * src.battery_power_sign)
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
    // Day-ahead temperature for target hours; short-lead archive for reference days.
    let (t_fc, t_hist) = (
        hourly_temp(store, temp_forecast)?,
        hourly_temp(store, temp_history)?,
    );
    let bases = DegreeBases::default();
    let mut past: Vec<(i64, Sample)> = Vec::new();
    let local_midnight = |d: NaiveDate| -> Result<i64> {
        Ok(tz
            .from_local_datetime(&d.and_hms_opt(0, 0, 0).context("bad date")?)
            .earliest()
            .context("tz")?
            .timestamp())
    };
    let all: Vec<(i64, f64)> = store.samples(LOAD_KEY, 3600, 0, i64::MAX / 2)?;
    if all.is_empty() {
        bail!("no '{LOAD_KEY}' series; run build-load first");
    }
    let actual: HashMap<i64, f64> = all.iter().copied().collect();
    let methods: Vec<(&str, Option<ProfileOptions>)> = vec![
        ("last known day, same hour", None),
        ("same hour last week", None),
        ("profile, mean", Some(ProfileOptions::default())),
        (
            "profile, median",
            Some(ProfileOptions {
                median: true,
                ..Default::default()
            }),
        ),
        (
            "profile, mean + last year 30%",
            Some(ProfileOptions {
                last_year_weight: 0.3,
                ..Default::default()
            }),
        ),
        (
            "profile, median + last year 30%",
            Some(ProfileOptions {
                median: true,
                last_year_weight: 0.3,
                ..Default::default()
            }),
        ),
        ("profile, mean + temperature", Some(ProfileOptions::default())),
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
            .map(|(_, o)| o.map(|o| ProfileForecaster::new(&history, tz, country, o)))
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
    let last = Correction::fit(&past.iter().map(|(_, s)| *s).collect::<Vec<_>>(), 100.0);
    println!(
        "temperature correction (all data): +{:.0} W per °C above {} °C, +{:.0} W per °C below {} °C",
        last.beta_cool, bases.cool_base, last.beta_heat, bases.heat_base
    );
    let mean = sum_actual / n_actual.max(1) as f64;
    println!("{n_actual} hours, mean load {mean:.0} W; forecasts issued 12:00 UTC the day before");
    println!(
        "{:34} {:>14} {:>9} {:>8} {:>16}",
        "method", "hourly MAE", "RMSE", "bias", "daily MAE"
    );
    for ((name, _), s) in methods.iter().zip(&scores) {
        let n = s.n.max(1) as f64;
        println!(
            "{name:34} {:5.0} W ({:4.1}%) {:7.0} W {:+6.0} W {:6.2} kWh/day",
            s.abs / n,
            100.0 * s.abs / n / mean,
            (s.sq / n).sqrt(),
            s.bias / n,
            s.day_abs / s.days.max(1) as f64
        );
    }
    Ok(())
}
