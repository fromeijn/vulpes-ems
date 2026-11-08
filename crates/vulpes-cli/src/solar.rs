//! Solar calibration and evaluation commands.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, NaiveDate};
use std::collections::HashMap;
use std::path::Path;
use vulpes_core::Store;
use vulpes_solar::calibrate::{FitOptions, GroupMember, HourInput, fit_group};
use vulpes_solar::model::{PlaneParams, Weather};
use vulpes_solar::shading::{ShadingMap, ShadingOptions, fit_shading, refit_pdc0, simulate_hours_shaded};
use vulpes_solar::{SunPosition, sun_position};

/// Split days by ISO week parity, for calibrating on one half and evaluating on the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Weeks {
    All,
    Even,
    Odd,
}

impl Weeks {
    pub fn includes(self, ts: i64) -> bool {
        let week = DateTime::from_timestamp(ts, 0).map_or(0, |d| d.iso_week().week());
        match self {
            Self::All => true,
            Self::Even => week % 2 == 0,
            Self::Odd => week % 2 == 1,
        }
    }
}

/// Hourly model inputs built from a weather archive, keyed by hour start.
pub(crate) struct HourlyInputs {
    pub hours: Vec<i64>,
    pub inputs: Vec<HourInput>,
    /// Forecast clearness: hourly GHI / clear-sky GHI.
    pub kt: Vec<f64>,
}

/// Haurwitz clear-sky GHI (W/m²).
fn clear_sky_ghi(zenith: f64) -> f64 {
    let cz = zenith.to_radians().cos();
    if cz <= 0.0 {
        0.0
    } else {
        1098.0 * cz * (-0.057 / cz).exp()
    }
}

pub(crate) fn load_inputs(
    store: &Store,
    source: &str,
    lat: f64,
    lon: f64,
    from: i64,
    to: i64,
) -> Result<HourlyInputs> {
    let get = |var: &str| -> Result<HashMap<i64, f64>> {
        Ok(store
            .forecasts_asof(source, var, from, to, i64::MAX)?
            .into_iter()
            .collect())
    };
    let (ghi, dni, dhi) = (
        get("shortwave_radiation")?,
        get("direct_normal_irradiance")?,
        get("diffuse_radiation")?,
    );
    let (temp, wind) = (get("temperature_2m")?, get("wind_speed_10m")?);
    if ghi.is_empty() {
        bail!("no weather archive for source '{source}' in range; run backfill-weather first");
    }
    let mut out = HourlyInputs {
        hours: Vec::new(),
        inputs: Vec::new(),
        kt: Vec::new(),
    };
    let mut h = from - from.rem_euclid(3600);
    while h < to {
        let mut ok = true;
        let mut sun = [SunPosition {
            zenith: 0.0,
            azimuth: 0.0,
        }; 4];
        let mut weather = [Weather {
            ghi: 0.0,
            dni: 0.0,
            dhi: 0.0,
            temp_air: 0.0,
            wind_speed: 0.0,
        }; 4];
        let (mut g_sum, mut cs_sum) = (0.0, 0.0);
        for k in 0..4 {
            let t = h + k as i64 * 900;
            match (ghi.get(&t), dni.get(&t), dhi.get(&t), temp.get(&t), wind.get(&t)) {
                (Some(&g), Some(&b), Some(&d), Some(&ta), Some(&ws)) => {
                    sun[k] = sun_position((t + 450) as f64, lat, lon);
                    weather[k] = Weather {
                        ghi: g,
                        dni: b,
                        dhi: d,
                        temp_air: ta,
                        wind_speed: ws,
                    };
                    g_sum += g;
                    cs_sum += clear_sky_ghi(sun[k].zenith);
                }
                _ => ok = false,
            }
        }
        if ok {
            let doy = DateTime::from_timestamp(h, 0).map(|d| d.ordinal()).unwrap_or(1);
            out.hours.push(h);
            out.inputs.push(HourInput {
                sun,
                weather,
                day_of_year: doy,
            });
            out.kt.push(if cs_sum > 0.0 { g_sum / cs_sum } else { 0.0 });
        }
        h += 3600;
    }
    Ok(out)
}

fn quantile(mut v: Vec<f64>, q: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(((v.len() as f64 - 1.0) * q).round() as usize)
        .copied()
        .unwrap_or(0.0)
}

/// One line of the panel list: `<serial> <name> [<group> <azimuth_offset>]`.
#[derive(Debug, Clone, PartialEq)]
struct PanelSpec {
    serial: String,
    name: String,
    /// Panels in a group share tilt and (offset) azimuth; defaults to the name.
    group: String,
    azimuth_offset: f64,
}

fn parse_panels(text: &str) -> Result<Vec<PanelSpec>> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        let name = f
            .get(1)
            .with_context(|| format!("panel line {}: needs a name", n + 1))?
            .to_string();
        let azimuth_offset = match f.get(3) {
            Some(o) => o
                .parse()
                .with_context(|| format!("panel line {}: bad azimuth offset '{o}'", n + 1))?,
            None => 0.0,
        };
        out.push(PanelSpec {
            serial: f[0].to_string(),
            group: f.get(2).map_or(name.clone(), |g| g.to_string()),
            name,
            azimuth_offset,
        });
    }
    Ok(out)
}

/// Clear-ish daytime hours (forecast clearness ≥ 0.65, sun above 10°) in the
/// selected weeks that also have an actual value.
fn clear_hours(data: &HourlyInputs, actual: &HashMap<i64, f64>, weeks: Weeks) -> (Vec<HourInput>, Vec<f64>) {
    let (mut inputs, mut y) = (Vec::new(), Vec::new());
    for (i, h) in data.hours.iter().enumerate() {
        if let (true, Some(&a)) = (
            data.kt[i] >= 0.65 && data.inputs[i].sun[2].elevation() > 10.0 && weeks.includes(*h),
            actual.get(h),
        ) {
            inputs.push(data.inputs[i].clone());
            y.push(a);
        }
    }
    (inputs, y)
}

/// A fitted panel.
struct Calibrated<'a> {
    spec: &'a PanelSpec,
    params: PlaneParams,
    shading: Option<ShadingMap>,
    rmse: f64,
    hours: usize,
}

/// Fit every panel: one shared geometry per group (groups in order of first
/// appearance), a capacity per panel, and optionally a shading map.
/// `actual` holds each panel's hourly AC power (W), in the order of `panels`.
fn calibrate_panels<'a>(
    data: &HourlyInputs,
    panels: &'a [PanelSpec],
    actual: &[HashMap<i64, f64>],
    weeks: Weeks,
    shading_prior: Option<f64>,
) -> Vec<Calibrated<'a>> {
    struct Prepared {
        inputs: Vec<HourInput>,
        actual: Vec<f64>,
        clip_q: f64,
    }
    let prepared: Vec<Prepared> = actual
        .iter()
        .map(|a| {
            let (inputs, y) = clear_hours(data, a, weeks);
            Prepared {
                inputs,
                actual: y,
                clip_q: 1.01 * quantile(a.values().copied().collect(), 0.995),
            }
        })
        .collect();
    // Panels that never reach their inverter limit cannot reveal it; they get
    // the highest limit seen (same inverter type assumed).
    let global_clip = prepared.iter().map(|p| p.clip_q).fold(0.0, f64::max);
    let base = PlaneParams {
        azimuth: 180.0,
        tilt: 30.0,
        pdc0: 295.0,
        ac_max: global_clip,
        inverter_eff: 0.965,
        gamma: -0.004,
        iam_b0: 0.05,
        albedo: 0.2,
        u0: 25.0,
        u1: 6.84,
    };
    let mut groups: Vec<&str> = Vec::new();
    for p in panels {
        if !groups.contains(&p.group.as_str()) {
            groups.push(&p.group);
        }
    }
    let mut out = Vec::new();
    for g in groups {
        let idx: Vec<usize> = (0..panels.len()).filter(|&i| panels[i].group == g).collect();
        let members: Vec<GroupMember> = idx
            .iter()
            .map(|&i| {
                let p = &prepared[i];
                let ac_max = if p.clip_q < 0.95 * global_clip {
                    global_clip
                } else {
                    p.clip_q
                };
                GroupMember {
                    inputs: &p.inputs,
                    actual: &p.actual,
                    azimuth_offset: panels[i].azimuth_offset,
                    base: PlaneParams {
                        ac_max,
                        ..base.clone()
                    },
                }
            })
            .collect();
        for (&i, fit) in idx.iter().zip(fit_group(&members, FitOptions::default())) {
            let p = &prepared[i];
            // Optionally learn shading on top of the fitted geometry, then refit capacity with it.
            let (params, shading) = match shading_prior {
                Some(prior) => {
                    let opt = ShadingOptions {
                        prior_hours: prior,
                        ..Default::default()
                    };
                    let map = fit_shading(&fit.params, &p.inputs, &p.actual, opt);
                    let pdc0 = refit_pdc0(
                        &fit.params,
                        &map,
                        &p.inputs,
                        &p.actual,
                        FitOptions::default().trim,
                    );
                    (
                        PlaneParams {
                            pdc0,
                            ..fit.params.clone()
                        },
                        Some(map),
                    )
                }
                None => (fit.params.clone(), None),
            };
            out.push(Calibrated {
                spec: &panels[i],
                params,
                shading,
                rmse: fit.rmse,
                hours: fit.hours_used,
            });
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn calibrate(
    store: &Store,
    lat: f64,
    lon: f64,
    from: NaiveDate,
    to: NaiveDate,
    panels: &Path,
    model: &str,
    out: &Path,
    weeks: Weeks,
    shading_prior: Option<f64>,
) -> Result<()> {
    let (from, to) = utc_range(from, to)?;
    let data = load_inputs(store, &format!("open-meteo-archive:{model}"), lat, lon, from, to)?;
    let text = std::fs::read_to_string(panels).with_context(|| format!("reading {}", panels.display()))?;
    let specs = parse_panels(&text)?;
    let actual: Vec<HashMap<i64, f64>> = specs
        .iter()
        .map(|p| {
            Ok(store
                .samples(&format!("pv.inv.{}", p.serial), 3600, from, to)?
                .into_iter()
                .collect())
        })
        .collect::<Result<_>>()?;
    let mut results = serde_json::Map::new();
    println!(
        "{:18} {:10} {:>7} {:>5} {:>6} {:>6} {:>8} {:>6} {:>9}",
        "panel", "group", "azimuth", "tilt", "pdc0", "ac_max", "rmse(W)", "hours", "shaded<0.7"
    );
    for c in calibrate_panels(&data, &specs, &actual, weeks, shading_prior) {
        let (p, q) = (c.spec, &c.params);
        let shaded_bins = c
            .shading
            .as_ref()
            .map_or(0, |m| m.factors.iter().flatten().filter(|&&f| f < 0.7).count());
        println!(
            "{:18} {:10} {:7.0} {:5.0} {:6.0} {:6.0} {:8.1} {:6} {:>9}",
            p.name, p.group, q.azimuth, q.tilt, q.pdc0, q.ac_max, c.rmse, c.hours, shaded_bins
        );
        results.insert(
            p.name.clone(),
            serde_json::json!({
                "serial": p.serial, "group": p.group, "params": q, "shading": c.shading,
                "rmse_w": c.rmse, "hours": c.hours,
            }),
        );
    }
    std::fs::write(out, serde_json::to_string_pretty(&results)?)?;
    println!("written to {}", out.display());
    Ok(())
}

/// `[from 00:00 UTC, to + 1 day 00:00 UTC)` as Unix seconds.
fn utc_range(from: NaiveDate, to: NaiveDate) -> Result<(i64, i64)> {
    let ts = |d: NaiveDate| {
        d.and_hms_opt(0, 0, 0)
            .map(|x| x.and_utc().timestamp())
            .context("bad date")
    };
    Ok((ts(from)?, ts(to)? + 86_400))
}

pub(crate) type Plane = (String, PlaneParams, Option<ShadingMap>);

/// Load calibrated planes (and shading maps, unless disabled) written by `calibrate-solar`.
pub(crate) fn load_planes(path: &Path, shading: bool) -> Result<Vec<Plane>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_planes(&text, shading)
}

/// Parse `calibrate-solar` output: `{name: {"params": {...}, "shading": {...} | null}}`.
fn parse_planes(text: &str, shading: bool) -> Result<Vec<Plane>> {
    let v: serde_json::Value = serde_json::from_str(text)?;
    let obj = v.as_object().context("planes file must be a JSON object")?;
    obj.iter()
        .map(|(name, e)| {
            let map = match (&e["shading"], shading) {
                (serde_json::Value::Null, _) | (_, false) => None,
                (m, true) => Some(serde_json::from_value(m.clone())?),
            };
            Ok((name.clone(), serde_json::from_value(e["params"].clone())?, map))
        })
        .collect()
}

/// Plant AC power (W, hourly mean) per hour start.
pub(crate) fn plant_hourly(planes: &[Plane], data: &HourlyInputs) -> HashMap<i64, f64> {
    let per_plane: Vec<Vec<f64>> = planes
        .iter()
        .map(|(_, p, map)| simulate_hours_shaded(p, map.as_ref(), &data.inputs))
        .collect();
    data.hours
        .iter()
        .enumerate()
        .map(|(i, &h)| (h, per_plane.iter().map(|v| v[i]).sum()))
        .collect()
}

/// Daily energy (kWh) per UTC day from hourly mean power (W).
fn daily(hourly: &HashMap<i64, f64>) -> HashMap<i64, f64> {
    let mut by_day: HashMap<i64, f64> = HashMap::new();
    for (&h, &w) in hourly {
        *by_day.entry(h - h.rem_euclid(86_400)).or_default() += w / 1000.0;
    }
    by_day
}

struct Score {
    n: usize,
    abs: f64,
    sq: f64,
    bias: f64,
}

impl Score {
    fn new() -> Self {
        Self {
            n: 0,
            abs: 0.0,
            sq: 0.0,
            bias: 0.0,
        }
    }
    fn add(&mut self, forecast: f64, actual: f64) {
        let e = forecast - actual;
        self.n += 1;
        self.abs += e.abs();
        self.sq += e * e;
        self.bias += e;
    }
    fn line(&self, name: &str, mean_actual: f64) -> String {
        let n = self.n.max(1) as f64;
        format!(
            "{name:28} MAE {:5.2} kWh/day ({:4.1}%)  RMSE {:5.2}  bias {:+5.2}",
            self.abs / n,
            100.0 * self.abs / n / mean_actual,
            (self.sq / n).sqrt(),
            self.bias / n
        )
    }
}

/// Scores on a set of days: our KNMI, ICON-D2 and two-model mean forecasts,
/// and (where present) Solar Forecast ML.
struct DayScores {
    days: usize,
    mean_actual: f64,
    knmi: Score,
    icon: Score,
    mean: Score,
    sfml: Score,
}

struct SolarEval {
    all_days: DayScores,
    sfml_days: DayScores,
    /// Hourly daylight scores of the two-model mean and of persistence.
    hourly: Score,
    persistence: Score,
    hourly_mean_actual: f64,
}

/// Score daily and hourly forecasts. `forecast` holds each model's hourly
/// plant power (W) keyed by hour start, `actual` the measured hourly power,
/// and `sfml` Solar Forecast ML's next-day kWh keyed by the hour it was read
/// (19:00 UTC the day before). Only complete UTC days in `weeks` count.
fn evaluate_days(
    forecast: &[HashMap<i64, f64>; 2],
    actual: &HashMap<i64, f64>,
    sfml: &HashMap<i64, f64>,
    weeks: Weeks,
) -> SolarEval {
    let ours: Vec<HashMap<i64, f64>> = forecast.iter().map(daily).collect();
    // Actual daily energy; only complete days (24 hourly means; W·h → kWh).
    let mut daily_actual: HashMap<i64, (f64, usize)> = HashMap::new();
    for (&t, &w) in actual {
        let e = daily_actual.entry(t - t.rem_euclid(86_400)).or_default();
        e.0 += w / 1000.0;
        e.1 += 1;
    }
    let mut days: Vec<i64> = daily_actual
        .iter()
        .filter(|(d, (_, n))| *n == 24 && weeks.includes(**d) && ours.iter().all(|o| o.contains_key(d)))
        .map(|(d, _)| *d)
        .collect();
    days.sort();
    let sfml_at = |d: i64| sfml.get(&(d - 86_400 + 19 * 3600)).copied();
    let score = |ds: &[i64]| {
        let mut r = DayScores {
            days: ds.len(),
            mean_actual: ds.iter().map(|d| daily_actual[d].0).sum::<f64>() / ds.len().max(1) as f64,
            knmi: Score::new(),
            icon: Score::new(),
            mean: Score::new(),
            sfml: Score::new(),
        };
        for d in ds {
            let a = daily_actual[d].0;
            r.knmi.add(ours[0][d], a);
            r.icon.add(ours[1][d], a);
            r.mean.add(0.5 * (ours[0][d] + ours[1][d]), a);
            if let Some(s) = sfml_at(*d) {
                r.sfml.add(s, a);
            }
        }
        r
    };
    let with_sf: Vec<i64> = days.iter().copied().filter(|&d| sfml_at(d).is_some()).collect();
    // Hourly skill on the evaluated days, daylight hours only (actual or forecast > 0).
    let (mut hourly, mut persistence, mut sum_a, mut n) = (Score::new(), Score::new(), 0.0, 0usize);
    for d in &days {
        for k in 0..24 {
            let h = d + k * 3600;
            let (Some(&a), Some(&f0), Some(&f1), Some(&y)) = (
                actual.get(&h),
                forecast[0].get(&h),
                forecast[1].get(&h),
                actual.get(&(h - 86_400)),
            ) else {
                continue;
            };
            let f = 0.5 * (f0 + f1);
            if a <= 0.0 && f <= 0.0 {
                continue;
            }
            hourly.add(f, a);
            persistence.add(y, a);
            sum_a += a;
            n += 1;
        }
    }
    SolarEval {
        all_days: score(&days),
        sfml_days: score(&with_sf),
        hourly,
        persistence,
        hourly_mean_actual: sum_a / n.max(1) as f64,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate(
    store: &Store,
    lat: f64,
    lon: f64,
    from: NaiveDate,
    to: NaiveDate,
    planes_path: &Path,
    lead_days: u8,
    weeks: Weeks,
    shading: bool,
) -> Result<()> {
    let (from, to) = utc_range(from, to)?;
    let planes = load_planes(planes_path, shading)?;
    let prefix = if lead_days == 0 {
        "open-meteo-archive".to_string()
    } else {
        format!("open-meteo-day{lead_days}")
    };
    let model = |m: &str| -> Result<HashMap<i64, f64>> {
        Ok(plant_hourly(
            &planes,
            &load_inputs(store, &format!("{prefix}:{m}"), lat, lon, from, to)?,
        ))
    };
    let forecast = [model("knmi_harmonie_arome_netherlands")?, model("icon_d2")?];
    let actual: HashMap<i64, f64> = store
        .samples("pv.total.power", 3600, from, to)?
        .into_iter()
        .collect();
    // Solar Forecast ML "tomorrow" as it stood at 19:00 UTC the day before.
    let sfml: HashMap<i64, f64> = store
        .samples("bench.sfml.tomorrow", 3600, from - 86_400, to)?
        .into_iter()
        .collect();
    let e = evaluate_days(&forecast, &actual, &sfml, weeks);

    let report = |title: &str, r: &DayScores, with_sfml: bool| {
        let m = r.mean_actual;
        println!("\n{title}: {} days, mean actual {m:.2} kWh/day", r.days);
        println!("  {}", r.knmi.line("vulpes physics (KNMI)", m));
        println!("  {}", r.icon.line("vulpes physics (ICON-D2)", m));
        println!("  {}", r.mean.line("vulpes physics (mean of 2)", m));
        if with_sfml {
            println!("  {}", r.sfml.line("Solar Forecast ML", m));
        }
    };
    report("All evaluated days", &e.all_days, false);
    report(
        "Days with a Solar Forecast ML day-ahead value",
        &e.sfml_days,
        true,
    );
    let (m, n) = (e.hourly_mean_actual, e.hourly.n);
    let w = |s: &Score| {
        let k = s.n.max(1) as f64;
        format!(
            "MAE {:4.0} W ({:4.1}%)  RMSE {:4.0} W  bias {:+4.0} W",
            s.abs / k,
            100.0 * s.abs / k / m,
            (s.sq / k).sqrt(),
            s.bias / k
        )
    };
    println!("\nHourly, daylight ({n} hours, mean actual {m:.0} W):");
    println!("  {:28} {}", "vulpes physics (mean of 2)", w(&e.hourly));
    println!("  {:28} {}", "persistence (same hour -1d)", w(&e.persistence));
    println!(
        "  skill vs persistence (RMSE): {:.0}%",
        100.0 * (1.0 - (e.hourly.sq / e.persistence.sq).sqrt())
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vulpes_core::ForecastPoint;

    // 2026-06-21 00:00 UTC (a Sunday, ISO week 25).
    const DAY: i64 = 1_782_000_000;

    #[test]
    fn iso_week_split() {
        assert_eq!(DateTime::from_timestamp(DAY, 0).unwrap().iso_week().week(), 25);
        assert!(Weeks::Odd.includes(DAY) && !Weeks::Even.includes(DAY) && Weeks::All.includes(DAY));
        let next_week = DAY + 86_400; // Monday, week 26
        assert!(Weeks::Even.includes(next_week) && !Weeks::Odd.includes(next_week));
    }

    #[test]
    fn haurwitz_clear_sky() {
        assert!((clear_sky_ghi(0.0) - 1098.0 * (-0.057f64).exp()).abs() < 1e-9);
        assert!(clear_sky_ghi(60.0) < clear_sky_ghi(30.0));
        assert_eq!(clear_sky_ghi(90.0), 0.0);
        assert_eq!(clear_sky_ghi(120.0), 0.0);
    }

    #[test]
    fn quantiles() {
        let v = vec![5.0, 1.0, 3.0, 2.0, 4.0];
        assert_eq!(quantile(v.clone(), 0.0), 1.0);
        assert_eq!(quantile(v.clone(), 0.5), 3.0);
        assert_eq!(quantile(v, 1.0), 5.0);
        assert_eq!(quantile(Vec::new(), 0.5), 0.0);
    }

    #[test]
    fn daily_energy_per_utc_day() {
        let hourly: HashMap<i64, f64> = [(DAY, 1000.0), (DAY + 3600, 500.0), (DAY + 86_400, 2000.0)].into();
        let d = daily(&hourly);
        assert_eq!((d[&DAY], d[&(DAY + 86_400)]), (1.5, 2.0));
    }

    #[test]
    fn score_line() {
        let mut s = Score::new();
        s.add(11.0, 10.0);
        s.add(7.0, 10.0);
        assert_eq!((s.n, s.abs, s.sq, s.bias), (2, 4.0, 10.0, -2.0));
        let line = s.line("test", 10.0);
        assert!(
            line.contains("MAE  2.00") && line.contains("(20.0%)") && line.contains("bias -1.00"),
            "{line}"
        );
    }

    fn planes_json() -> &'static str {
        r#"{
            "east": {"params": {"azimuth": 90, "tilt": 30, "pdc0": 400, "ac_max": 350},
                     "shading": {"az_step": 10, "el_step": 10, "factors": [[0.5]]}},
            "west": {"params": {"azimuth": 270, "tilt": 30, "pdc0": 400, "ac_max": 350}, "shading": null}
        }"#
    }

    #[test]
    fn planes_file_parsing() {
        let with = parse_planes(planes_json(), true).unwrap();
        assert_eq!(with.len(), 2);
        let east = with.iter().find(|p| p.0 == "east").unwrap();
        assert!(east.2.is_some() && east.1.azimuth == 90.0);
        assert!(with.iter().find(|p| p.0 == "west").unwrap().2.is_none());
        assert!(
            parse_planes(planes_json(), false)
                .unwrap()
                .iter()
                .all(|p| p.2.is_none())
        );
        assert!(parse_planes("[1, 2]", true).is_err(), "must be an object");
        assert!(parse_planes(r#"{"x": {"params": {"tilt": 3}}}"#, true).is_err());
        assert!(parse_planes("not json", true).is_err());
    }

    fn store_with_weather(hours: i64, skip: Option<i64>) -> Store {
        let mut store = Store::open_in_memory().unwrap();
        let vals = [
            ("shortwave_radiation", 400.0),
            ("direct_normal_irradiance", 500.0),
            ("diffuse_radiation", 100.0),
            ("temperature_2m", 20.0),
            ("wind_speed_10m", 2.0),
        ];
        for (var, v) in vals {
            let pts: Vec<ForecastPoint> = (0..hours * 4)
                .map(|k| DAY + 10 * 3600 + k * 900)
                .filter(|&t| Some(t) != skip || var != "temperature_2m")
                .map(|t| ForecastPoint {
                    valid_ts: t,
                    issued_ts: t - 86_400,
                    value: v,
                })
                .collect();
            store.put_forecasts("src", var, &pts).unwrap();
        }
        store
    }

    #[test]
    fn load_inputs_builds_complete_hours() {
        let skip = DAY + 11 * 3600 + 900; // one missing quarter in the second hour
        let store = store_with_weather(3, Some(skip));
        let d = load_inputs(&store, "src", 52.0, 5.0, DAY, DAY + 86_400).unwrap();
        assert_eq!(d.hours, vec![DAY + 10 * 3600, DAY + 12 * 3600]);
        assert_eq!((d.inputs.len(), d.kt.len()), (2, 2));
        assert_eq!(d.inputs[0].day_of_year, 172);
        assert_eq!(d.inputs[0].weather[3].ghi, 400.0);
        // Midday in June at 52° N: 400 W/m² is well below clear sky.
        assert!(d.kt[0] > 0.3 && d.kt[0] < 0.6, "{}", d.kt[0]);
        assert!(load_inputs(&store, "other", 52.0, 5.0, DAY, DAY + 86_400).is_err());
    }

    #[test]
    fn plant_is_the_sum_of_its_planes() {
        let store = store_with_weather(2, None);
        let d = load_inputs(&store, "src", 52.0, 5.0, DAY, DAY + 86_400).unwrap();
        let planes = parse_planes(planes_json(), false).unwrap();
        let total = plant_hourly(&planes, &d);
        let single = |i: usize| plant_hourly(&planes[i..=i], &d);
        for h in &d.hours {
            assert!((total[h] - single(0)[h] - single(1)[h]).abs() < 1e-9);
            assert!(total[h] > 0.0);
        }
        // Shading on one plane lowers the total.
        let shaded = plant_hourly(&parse_planes(planes_json(), true).unwrap(), &d);
        assert!(d.hours.iter().all(|h| shaded[h] <= total[h]));
    }

    /// Clear-sky-ish hourly inputs at 52° N for `days` days from DAY (kt = 1).
    fn clear_data(days: i64) -> HourlyInputs {
        let mut d = HourlyInputs {
            hours: Vec::new(),
            inputs: Vec::new(),
            kt: Vec::new(),
        };
        for h in (0..24 * days).map(|k| DAY + k * 3600) {
            let mut sun = [SunPosition {
                zenith: 0.0,
                azimuth: 0.0,
            }; 4];
            let mut weather = [Weather {
                ghi: 0.0,
                dni: 0.0,
                dhi: 0.0,
                temp_air: 20.0,
                wind_speed: 2.0,
            }; 4];
            for k in 0..4 {
                let s = sun_position((h + k as i64 * 900 + 450) as f64, 52.0, 5.0);
                let cz = s.zenith.to_radians().cos().max(0.0);
                let dni = if cz > 0.0 {
                    850.0 * (-0.15 / cz.max(0.05)).exp()
                } else {
                    0.0
                };
                sun[k] = s;
                weather[k] = Weather {
                    ghi: dni * cz + 90.0 * cz,
                    dni,
                    dhi: 90.0 * cz,
                    temp_air: 20.0,
                    wind_speed: 2.0,
                };
            }
            d.hours.push(h);
            d.inputs.push(HourInput {
                sun,
                weather,
                day_of_year: 172,
            });
            d.kt.push(1.0);
        }
        d
    }

    fn simulated(data: &HourlyInputs, p: &PlaneParams) -> HashMap<i64, f64> {
        let w = vulpes_solar::calibrate::simulate_hours(p, &data.inputs);
        data.hours.iter().copied().zip(w).collect()
    }

    fn params(azimuth: f64, tilt: f64, pdc0: f64) -> PlaneParams {
        serde_json::from_value(
            serde_json::json!({"azimuth": azimuth, "tilt": tilt, "pdc0": pdc0, "ac_max": 1000}),
        )
        .unwrap()
    }

    #[test]
    fn panel_list_parsing() {
        let text =
            "# serial name group offset\n111 front\n222 back_l back 0  # comment\n333 back_r back 180\n";
        let p = parse_panels(text).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(
            (p[0].group.as_str(), p[0].azimuth_offset),
            ("front", 0.0),
            "group defaults to the name"
        );
        assert_eq!(
            (p[2].serial.as_str(), p[2].group.as_str(), p[2].azimuth_offset),
            ("333", "back", 180.0)
        );
        assert!(
            parse_panels("111\n")
                .unwrap_err()
                .to_string()
                .contains("line 1: needs a name")
        );
        assert!(
            parse_panels("111 a g east\n")
                .unwrap_err()
                .to_string()
                .contains("bad azimuth offset 'east'")
        );
        assert!(parse_panels("\n# only comments\n").unwrap().is_empty());
    }

    #[test]
    fn clear_hours_need_clear_sky_high_sun_week_and_actual() {
        let mut data = clear_data(8);
        let actual: HashMap<i64, f64> = data.hours.iter().map(|&h| (h, 100.0)).collect();
        let (all, y) = clear_hours(&data, &actual, Weeks::All);
        assert_eq!(all.len(), y.len());
        assert!(all.iter().all(|h| h.sun[2].elevation() > 10.0));
        let noon = data.hours.iter().position(|&h| h == DAY + 12 * 3600).unwrap();
        data.kt[noon] = 0.5; // cloudy
        let (fewer, _) = clear_hours(&data, &actual, Weeks::All);
        assert_eq!(fewer.len(), all.len() - 1);
        // DAY is a Sunday in week 25; the next six days are week 26.
        let (odd, _) = clear_hours(&data, &actual, Weeks::Odd);
        let (even, _) = clear_hours(&data, &actual, Weeks::Even);
        assert_eq!(odd.len() + even.len(), fewer.len());
        assert!(odd.len() < even.len());
        let none = HashMap::new();
        assert!(clear_hours(&data, &none, Weeks::All).0.is_empty());
    }

    #[test]
    fn calibration_recovers_groups_and_capacities() {
        let data = clear_data(10);
        let specs = parse_panels("1 east tent 0\n2 south solo\n3 west tent 180\n").unwrap();
        let actual = vec![
            simulated(&data, &params(100.0, 25.0, 300.0)),
            simulated(&data, &params(180.0, 40.0, 400.0)),
            simulated(&data, &params(280.0, 25.0, 250.0)),
        ];
        let r = calibrate_panels(&data, &specs, &actual, Weeks::All, None);
        // Each panel exactly once, even though the tent's panels are not adjacent; groups in order of first appearance.
        let names: Vec<&str> = r.iter().map(|c| c.spec.name.as_str()).collect();
        assert_eq!(names, ["east", "west", "south"]);
        let get = |n: &str| r.iter().find(|c| c.spec.name == n).unwrap();
        let (e, w, s) = (get("east"), get("west"), get("south"));
        assert!(
            (e.params.azimuth - 100.0).abs() <= 1.0 && (w.params.azimuth - 280.0).abs() <= 1.0,
            "{} {}",
            e.params.azimuth,
            w.params.azimuth
        );
        assert_eq!(e.params.tilt, w.params.tilt, "a group shares its tilt");
        assert!((s.params.tilt - 40.0).abs() <= 1.0 && (s.params.azimuth - 180.0).abs() <= 1.0);
        for (c, pdc0) in [(e, 300.0), (w, 250.0), (s, 400.0)] {
            assert!(
                (c.params.pdc0 - pdc0).abs() < 0.03 * pdc0,
                "{}: {}",
                c.spec.name,
                c.params.pdc0
            );
            assert!(c.shading.is_none() && c.hours > 0);
        }
        // With a shading prior every panel also gets a map.
        let with_maps = calibrate_panels(&data, &specs[1..2], &actual[1..2], Weeks::All, Some(10.0));
        assert!(with_maps[0].shading.is_some());
    }

    #[test]
    fn panels_that_never_clip_get_the_highest_limit_seen() {
        let data = clear_data(10);
        let specs = parse_panels("1 big\n2 small\n").unwrap();
        let mut clipped = params(180.0, 35.0, 400.0);
        clipped.ac_max = 250.0;
        let actual = vec![
            simulated(&data, &clipped),
            simulated(&data, &params(180.0, 35.0, 100.0)),
        ];
        let r = calibrate_panels(&data, &specs, &actual, Weeks::All, None);
        let limit = 1.01 * 250.0;
        assert!(
            r.iter().all(|c| (c.params.ac_max - limit).abs() < 1e-6),
            "{:?}",
            r.iter().map(|c| c.params.ac_max).collect::<Vec<_>>()
        );
    }

    #[test]
    fn scoring_days_hours_and_the_benchmark() {
        // Four days of measured power; day 3 is incomplete.
        let hours: Vec<i64> = (0..24 * 4).map(|k| DAY + k * 3600).collect();
        let shape = |h: i64| (((h % 86_400) / 3600) as f64 - 12.0).abs().min(6.0);
        let mut actual: HashMap<i64, f64> = hours.iter().map(|&h| (h, 600.0 - 100.0 * shape(h))).collect();
        actual.remove(&(DAY + 2 * 86_400 + 5 * 3600));
        // KNMI is exact, ICON 10% high.
        let knmi = actual.clone();
        let icon: HashMap<i64, f64> = hours
            .iter()
            .map(|&h| (h, 1.1 * (600.0 - 100.0 * shape(h))))
            .collect();
        // Solar Forecast ML said 10 kWh for the second day, read at 19:00 UTC on the first.
        let sfml: HashMap<i64, f64> = [(DAY + 19 * 3600, 10.0)].into();
        let e = evaluate_days(&[knmi, icon], &actual, &sfml, Weeks::All);
        assert_eq!(e.all_days.days, 3, "the incomplete day is skipped");
        assert_eq!(e.all_days.knmi.abs, 0.0);
        let day_kwh = e.all_days.mean_actual;
        assert!((e.all_days.icon.bias / 3.0 - 0.1 * day_kwh).abs() < 1e-9);
        assert!((e.all_days.mean.bias / 3.0 - 0.05 * day_kwh).abs() < 1e-9);
        assert_eq!(e.sfml_days.days, 1);
        assert!((e.sfml_days.sfml.bias - (10.0 - day_kwh)).abs() < 1e-9);
        // Every day has the same shape, so persistence is perfect after the first day.
        assert_eq!(e.persistence.abs, 0.0);
        assert!(e.hourly.n > 0 && e.hourly.bias > 0.0);
        // The week filter: DAY is in an odd week, the following days in an even one.
        let only_odd = evaluate_days(
            &[actual.clone(), actual.clone()],
            &actual,
            &HashMap::new(),
            Weeks::Odd,
        );
        assert_eq!(only_odd.all_days.days, 1);
    }
}
