//! Tariff fitting and price forecast evaluation commands.

use crate::solar::Weeks;
use anyhow::{Result, bail};
use chrono::{DateTime, Datelike};
use std::collections::{BTreeMap, HashMap};
use vulpes_core::{ErrorStats, Store};
use vulpes_price::ridge::Ridge;
use vulpes_price::tariff::{TariffSample, fit};
use vulpes_price::wholesale::{
    self, ANCHOR_HOURS, Anchor, Drivers, Row, day_hours, day_rows, features, local_midnight,
};

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

#[derive(Default)]
struct PriceScore {
    err: ErrorStats,
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
    /// Forecast lead in days: 1 is the day-ahead forecast before the auction.
    pub lead_days: u8,
    /// Score only the days in these ISO weeks; the models learn from every past day.
    pub weeks: Weeks,
}

/// The source of the same forecast `lead_days` ahead: `open-meteo-day1:…` becomes
/// `open-meteo-day<N>:…`.
pub(crate) fn lead_source(source: &str, lead_days: u8) -> Result<String> {
    if lead_days == 1 {
        return Ok(source.to_string());
    }
    match source.split_once("-day1:") {
        Some((head, tail)) => Ok(format!("{head}-day{lead_days}:{tail}")),
        None => bail!("--lead-days needs day-ahead sources (open-meteo-day1:…), got '{source}'"),
    }
}

/// Evaluate wholesale forecasts issued 10:00 UTC `lead_days` before the day
/// (before that day's auction): prices up to the end of the issue day are known.
pub(crate) fn evaluate(
    store: &Store,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
    tz: chrono_tz::Tz,
    points: &PricePoints,
    wholesale: &str,
) -> Result<()> {
    let lead = points.lead_days;
    if !(1..=7).contains(&lead) {
        bail!("--lead-days must be between 1 and 7");
    }
    let all: Vec<(i64, f64)> = store.samples(wholesale, 3600, 0, i64::MAX / 2)?;
    if all.is_empty() {
        bail!("no wholesale series '{wholesale}'");
    }
    let load = |sources: &[String], var: &str, lead: u8| -> Result<Vec<HashMap<i64, f64>>> {
        sources
            .iter()
            .map(|s| crate::series::hourly_forecast(store, &lead_source(s, lead)?, var))
            .collect()
    };
    let (wind, solar) = ("wind_speed_100m", "shortwave_radiation");
    let past = Drivers::from_points(&load(&points.wind, wind, 1)?, &load(&points.solar, solar, 1)?);
    // A day ahead, the forecast of the day is the day-ahead history itself.
    let later = (lead > 1)
        .then(|| -> Result<Drivers> {
            Ok(Drivers::from_points(
                &load(&points.wind, wind, lead)?,
                &load(&points.solar, solar, lead)?,
            ))
        })
        .transpose()?;
    let ahead = later.as_ref().unwrap_or(&past);
    let with_points = !(points.wind.is_empty() && points.solar.is_empty());
    let e = evaluate_days(&all, &past, ahead, lead, with_points, points.weeks, from, to, tz);
    let scores = &e.scores;
    let d = scores[0].days.max(1) as f64;
    let issued = match lead {
        1 => "the day before".to_string(),
        n => format!("{n} days before"),
    };
    println!(
        "{} days evaluated (after a 14-day regression warm-up); forecasts issued 10:00 UTC {issued}",
        scores[0].days
    );
    println!(
        "{:40} {:>9} {:>9} {:>8} {:>10} {:>12} {:>12}",
        "method", "MAE ct", "RMSE ct", "bias", "rank corr", "cheapest-4", "priciest-4"
    );
    for (name, s) in PRICE_METHODS.iter().zip(scores) {
        let n = s.err.n.max(1) as f64;
        println!(
            "{name:40} {:9.2} {:9.2} {:+8.2} {:10.2} {:9.2} / 4 {:9.2} / 4",
            100.0 * s.err.abs / n,
            100.0 * (s.err.sq / n).sqrt(),
            100.0 * s.err.sum / n,
            s.rho / d,
            s.cheap_hits as f64 / d,
            s.dear_hits as f64 / d
        );
    }
    if let Some(m) = e.last_model {
        println!(
            "latest regression: {:+.1} ct per unit wind-power index, {:+.1} ct per kW/m² solar (vs reference days)",
            100.0 * m.coef[0],
            100.0 * m.coef[1]
        );
    }
    Ok(())
}

const PRICE_METHODS: [&str; 5] = [
    "copy last known day",
    "profile (28 d, half-life 7 d)",
    "profile + wind/solar regression",
    "profile + anchor",
    "profile + wind/solar regression + anchor",
];

struct PriceEval {
    /// One score per entry of [`PRICE_METHODS`].
    scores: Vec<PriceScore>,
    last_model: Option<Ridge>,
}

/// Score the wholesale forecasting methods on each complete local day in
/// `[from, to]`, forecast `lead` days ahead. `all` is the hourly price history.
/// `past` holds the drivers as forecast a day ahead (training and the
/// profile's reference days); `ahead` the drivers as forecast `lead` days
/// ahead (the day being forecast). With reference points (`with_points`), a
/// day is scored once the regression has 14 days of past residuals. Only days
/// in `weeks` are scored; the regression learns from every day.
///
/// The anchored methods add the [`Anchor`] of their forecast's error in the
/// last known hours, as the live forecaster does.
#[allow(clippy::too_many_arguments)]
fn evaluate_days(
    all: &[(i64, f64)],
    past: &Drivers,
    ahead: &Drivers,
    lead: u8,
    with_points: bool,
    weeks: Weeks,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
    tz: chrono_tz::Tz,
) -> PriceEval {
    let actual: HashMap<i64, f64> = all.iter().copied().collect();
    let lead = i64::from(lead.max(1));
    let mut scores: Vec<PriceScore> = PRICE_METHODS.iter().map(|_| PriceScore::default()).collect();
    // Past rows for the regression, from days whose prices are all known.
    let mut past_rows: Vec<Row> = Vec::new();
    let mut day = from;
    let mut last_model: Option<Ridge> = None;
    while day <= to {
        let next = day + chrono::Duration::days(1);
        let (Some(start), Some(end), Some(known_until)) = (
            local_midnight(day, tz),
            local_midnight(next, tz),
            local_midnight(day - chrono::Duration::days(lead - 1), tz),
        ) else {
            break;
        };
        let hours = day_hours(start, end);
        if !hours.iter().all(|t| actual.contains_key(t)) {
            day = next;
            continue;
        }
        // Known at issue time: all prices before the day after the issue day.
        let known = |cut: i64| -> Vec<(i64, f64)> { all.iter().copied().filter(|&(t, _)| t < cut).collect() };
        let profile = wholesale::profile(&known(known_until), tz);
        let usable: Vec<Row> = past_rows
            .iter()
            .filter(|r| r.day < known_until)
            .cloned()
            .collect();
        let model = wholesale::fit(&usable, start);
        // The error of both forecasts in the last known hours, with the
        // drivers as forecast a day ahead of them.
        let known_error = |k: i64| {
            let t = known_until - k * 3600;
            let (p, x) = features(&profile, past, past, t)?;
            let corrected = match (&model, &x) {
                (Some(m), Some(x)) => p + m.predict(x),
                _ => p,
            };
            let a = actual.get(&t)?;
            Some(((t, a - p), (t, a - corrected)))
        };
        // As live: no anchor when the latest known hour has no forecast.
        let (profile_errors, regression_errors): (Vec<_>, Vec<_>) = known_error(1)
            .into_iter()
            .chain((2..=ANCHOR_HOURS as i64).map_while(known_error))
            .unzip();
        let anchor = |errors: &[(i64, f64)], t: i64| Anchor::new(errors).map_or(0.0, |a| a.at(t));
        let mut f: Vec<Vec<f64>> = vec![Vec::new(); PRICE_METHODS.len()];
        let mut a = Vec::new();
        let mut ok = true;
        for &t in &hours {
            let Some((p, x)) = features(&profile, past, ahead, t) else {
                ok = false;
                break;
            };
            let corrected = match (&model, &x) {
                (Some(m), Some(x)) => p + m.predict(x),
                _ => p,
            };
            let last_known = actual.get(&(t - lead * 86_400)).copied();
            f[0].push(last_known.unwrap_or(p));
            f[1].push(p);
            f[2].push(corrected);
            f[3].push(p + anchor(&profile_errors, t));
            f[4].push(corrected + anchor(&regression_errors, t));
            a.push(actual[&t]);
        }
        // Training rows are day-ahead residuals, as live: the profile knows
        // every day before this one.
        let rows = if lead == 1 {
            day_rows(&profile, past, &actual, start, &hours)
        } else {
            day_rows(
                &wholesale::profile(&known(start), tz),
                past,
                &actual,
                start,
                &hours,
            )
        };
        past_rows.extend(rows);
        // With reference points, wait for the regression's warm-up so all
        // methods are scored on the same days; without, it never trains.
        if ok && (model.is_some() || !with_points) && weeks.includes(start + 12 * 3600) {
            for (m, s) in scores.iter_mut().enumerate() {
                for (fv, av) in f[m].iter().zip(&a) {
                    s.err.add(*fv, *av);
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
    PriceEval { scores, last_model }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vulpes_price::wholesale::wind_power;

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

    const TZ: chrono_tz::Tz = chrono_tz::Europe::Amsterdam;
    const START: i64 = 1_767_225_600; // 2026-01-01 00:00 UTC

    fn date(s: &str) -> chrono::NaiveDate {
        s.parse().unwrap()
    }

    fn series(days: i64, f: impl Fn(i64) -> f64) -> Vec<(i64, f64)> {
        (0..24 * days)
            .map(|k| START + k * 3600)
            .map(|t| (t, f(t)))
            .collect()
    }

    /// Local hour of a timestamp in Amsterdam.
    fn local_hour(t: i64) -> f64 {
        use chrono::{TimeZone, Timelike};
        TZ.timestamp_opt(t, 0).unwrap().hour() as f64
    }

    #[test]
    fn a_repeating_day_is_copied_perfectly() {
        let all = series(40, |t| 0.10 + 0.01 * local_hour(t));
        let none = Drivers::default();
        let e = evaluate_days(
            &all,
            &none,
            &none,
            1,
            false,
            Weeks::All,
            date("2026-01-20"),
            date("2026-01-29"),
            TZ,
        );
        let copy = &e.scores[0];
        assert_eq!(copy.days, 10, "without reference points no warm-up is needed");
        assert!(copy.err.abs < 1e-12);
        assert!((copy.rho / copy.days as f64 - 1.0).abs() < 1e-12);
        assert_eq!((copy.cheap_hits, copy.dear_hits), (4 * 10, 4 * 10));
        assert!(e.last_model.is_none());
        // Nothing to anchor on either: every method is exact.
        assert_eq!(e.scores.len(), PRICE_METHODS.len());
        for s in &e.scores {
            assert!(s.err.abs < 1e-12 && s.days == 10);
        }
    }

    /// Hour-of-day prices that rise by 5 ct on 2026-02-10 and stay there.
    fn risen_market() -> Vec<(i64, f64)> {
        let rise = midnight(date("2026-02-10"));
        series(70, |t| {
            0.10 + 0.01 * local_hour(t) + if t >= rise { 0.05 } else { 0.0 }
        })
    }

    #[test]
    fn anchor_follows_a_level_shift_a_day_ahead() {
        let all = risen_market();
        let none = Drivers::default();
        let run = |lead: u8| {
            evaluate_days(
                &all,
                &none,
                &none,
                lead,
                false,
                Weeks::All,
                date("2026-02-11"),
                date("2026-02-20"),
                TZ,
            )
        };
        let mae = |e: &PriceEval, k: usize| e.scores[k].err.mae().unwrap();
        // A day ahead the profile lags the new level; the anchor closes most of the gap
        // in the first hours. Without a model, the regression rows equal the profile rows.
        let e = run(1);
        assert!(mae(&e, 1) > 0.01, "the profile lags: {}", mae(&e, 1));
        assert!(mae(&e, 3) < 0.8 * mae(&e, 1), "{} vs {}", mae(&e, 3), mae(&e, 1));
        assert_eq!(mae(&e, 2), mae(&e, 1));
        assert_eq!(mae(&e, 4), mae(&e, 3));
        // The shape within the day is still right.
        assert!(e.scores[3].rho / e.scores[3].days as f64 > 0.99);
        // Three days ahead the latest known hour is two days before the day: next to nothing is left.
        let e = run(3);
        assert!((mae(&e, 3) - mae(&e, 1)).abs() < 0.03 * mae(&e, 1));
    }

    #[test]
    fn weeks_split_the_scored_days_only() {
        let wind_speed = |t: i64| 3.0 + ((t / 86_400 * 5) % 10) as f64;
        let all = series(90, |t| {
            0.20 + 0.005 * local_hour(t) - 0.10 * wind_power(wind_speed(t))
        });
        let wind: HashMap<i64, f64> = all.iter().map(|&(t, _)| (t, wind_speed(t))).collect();
        let solar: HashMap<i64, f64> = all.iter().map(|&(t, _)| (t, 0.0)).collect();
        let drivers = Drivers::from_points(&[wind], &[solar]);
        let run = |weeks: Weeks| {
            evaluate_days(
                &all,
                &drivers,
                &drivers,
                1,
                true,
                weeks,
                date("2026-01-30"),
                date("2026-03-25"),
                TZ,
            )
        };
        let (every, even, odd) = (run(Weeks::All), run(Weeks::Even), run(Weeks::Odd));
        for k in 0..PRICE_METHODS.len() {
            let (a, e, o) = (&every.scores[k], &even.scores[k], &odd.scores[k]);
            assert!(e.days > 0 && o.days > 0);
            assert_eq!(e.days + o.days, a.days);
            assert_eq!(e.err.n + o.err.n, a.err.n);
            assert!((e.err.abs + o.err.abs - a.err.abs).abs() < 1e-9);
        }
        // The regression learns from every day, whichever weeks are scored.
        assert!(even.last_model.is_some() && odd.last_model.is_some());
        let last_even_day = run(Weeks::Even).last_model;
        assert_eq!(even.last_model, last_even_day);
    }

    #[test]
    fn regression_learns_the_wind_effect() {
        // Windy days are cheaper: price = profile − 0.10 × wind-power index.
        let wind_speed = |t: i64| 3.0 + ((t / 86_400 * 5) % 10) as f64;
        let all = series(90, |t| {
            0.20 + 0.005 * local_hour(t) - 0.10 * wind_power(wind_speed(t))
        });
        let wind: HashMap<i64, f64> = all.iter().map(|&(t, _)| (t, wind_speed(t))).collect();
        let solar: HashMap<i64, f64> = all.iter().map(|&(t, _)| (t, 0.0)).collect();
        let drivers = Drivers::from_points(&[wind], &[solar]);
        let e = evaluate_days(
            &all,
            &drivers,
            &drivers,
            1,
            true,
            Weeks::All,
            date("2026-01-30"),
            date("2026-03-25"),
            TZ,
        );
        let [_, profile, regression] = [0, 1, 2].map(|k| e.scores[k].err.mae().unwrap_or(0.0));
        assert!(
            e.scores[0].days > 0 && e.scores[0].days < 55,
            "warm-up days are not scored"
        );
        assert!(
            regression < 0.5 * profile,
            "regression {regression} vs profile {profile}"
        );
        let m = e.last_model.expect("a fitted model");
        assert!((m.coef[0] + 0.10).abs() < 0.02, "{:?}", m.coef);
    }

    #[test]
    fn data_after_the_period_is_never_used() {
        let all = series(60, |t| 0.15 + 0.02 * ((t / 3600 * 7) % 13) as f64);
        let mut changed = all.clone();
        let end = midnight(date("2026-02-11"));
        for (t, v) in &mut changed {
            if *t >= end {
                *v = 9.99;
            }
        }
        let none = Drivers::default();
        let run = |a: &[(i64, f64)]| {
            let e = evaluate_days(
                a,
                &none,
                &none,
                1,
                false,
                Weeks::All,
                date("2026-01-25"),
                date("2026-02-10"),
                TZ,
            );
            e.scores
                .iter()
                .map(|s| (s.err.abs, s.err.sq, s.err.sum, s.rho))
                .collect::<Vec<_>>()
        };
        assert_eq!(run(&all), run(&changed));
    }

    fn midnight(d: chrono::NaiveDate) -> i64 {
        local_midnight(d, TZ).unwrap()
    }

    #[test]
    fn longer_leads_only_use_prices_known_at_issue_time() {
        // Forecasting 2026-02-10 three days ahead (issued 2026-02-07 10:00 UTC),
        // the prices of 2026-02-08 and 2026-02-09 are not known yet.
        let all = series(60, |t| 0.15 + 0.02 * ((t / 3600 * 7) % 13) as f64);
        let mut changed = all.clone();
        let (from, to) = (midnight(date("2026-02-08")), midnight(date("2026-02-10")));
        for (t, v) in &mut changed {
            if (from..to).contains(t) {
                *v = 9.99;
            }
        }
        let none = Drivers::default();
        let run = |a: &[(i64, f64)], lead: u8| {
            let e = evaluate_days(
                a,
                &none,
                &none,
                lead,
                false,
                Weeks::All,
                date("2026-02-10"),
                date("2026-02-10"),
                TZ,
            );
            let s = &e.scores[1];
            (s.days, s.err.abs, s.err.sq)
        };
        assert_eq!(run(&all, 3), run(&changed, 3));
        assert_ne!(run(&all, 1), run(&changed, 1), "a day ahead, they are known");
        assert_eq!(run(&all, 3).0, 1);
    }

    #[test]
    fn regression_at_a_longer_lead_uses_the_lead_forecast() {
        // Wind at lead 2 is the actual wind; the day-ahead "forecast" used for
        // training is the same, so the regression still learns the effect.
        let wind_speed = |t: i64| 3.0 + ((t / 86_400 * 5) % 10) as f64;
        let all = series(90, |t| {
            0.20 + 0.005 * local_hour(t) - 0.10 * wind_power(wind_speed(t))
        });
        let wind: HashMap<i64, f64> = all.iter().map(|&(t, _)| (t, wind_speed(t))).collect();
        let solar: HashMap<i64, f64> = all.iter().map(|&(t, _)| (t, 0.0)).collect();
        let past = Drivers::from_points(&[wind], std::slice::from_ref(&solar));
        // A calm lead-2 forecast everywhere: the correction comes from it, not from `past`.
        let calm: HashMap<i64, f64> = all.iter().map(|&(t, _)| (t, 0.0)).collect();
        let ahead = Drivers::from_points(&[calm], &[solar]);
        let (a, b) = (date("2026-02-15"), date("2026-03-25"));
        let good = evaluate_days(&all, &past, &past, 2, true, Weeks::All, a, b, TZ);
        let bad = evaluate_days(&all, &past, &ahead, 2, true, Weeks::All, a, b, TZ);
        let mae = |e: &PriceEval| e.scores[2].err.mae().unwrap();
        assert!(mae(&good) < 0.5 * mae(&bad), "{} vs {}", mae(&good), mae(&bad));
        assert_eq!(good.scores[2].days, bad.scores[2].days);
    }

    #[test]
    fn lead_sources() {
        assert_eq!(
            lead_source("open-meteo-day1:ecmwf_ifs025@de-north", 1).unwrap(),
            "open-meteo-day1:ecmwf_ifs025@de-north"
        );
        assert_eq!(
            lead_source("open-meteo-day1:ecmwf_ifs025@de-north", 3).unwrap(),
            "open-meteo-day3:ecmwf_ifs025@de-north"
        );
        assert!(lead_source("open-meteo-archive:icon_d2", 2).is_err());
        assert_eq!(
            lead_source("open-meteo-archive:icon_d2", 1).unwrap(),
            "open-meteo-archive:icon_d2"
        );
    }
}
