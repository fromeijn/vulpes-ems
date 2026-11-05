//! Solar calibration and evaluation commands.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, NaiveDate};
use std::collections::HashMap;
use std::path::Path;
use vulpes_core::Store;
use vulpes_solar::calibrate::{FitOptions, GroupMember, HourInput, fit_group};
use vulpes_solar::model::{PlaneParams, Weather};
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
) -> Result<()> {
    let ts = |d: NaiveDate| {
        d.and_hms_opt(0, 0, 0)
            .map(|x| x.and_utc().timestamp())
            .context("bad date")
    };
    let (from, to) = (ts(from)?, ts(to)? + 86_400);
    let source = format!("open-meteo-archive:{model}");
    let data = load_inputs(store, &source, lat, lon, from, to)?;
    // Panels: `<serial> <name> [<group> <azimuth_offset>]`.
    let text = std::fs::read_to_string(panels).with_context(|| format!("reading {}", panels.display()))?;
    struct Panel {
        serial: String,
        name: String,
        group: String,
        offset: f64,
        inputs: Vec<HourInput>,
        actual: Vec<f64>,
        clip_q: f64,
    }
    let mut list = Vec::new();
    for line in text
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
    {
        let f: Vec<&str> = line.split_whitespace().collect();
        let (serial, name) = (
            f[0].to_string(),
            f.get(1).context("panel line needs a name")?.to_string(),
        );
        let group = f.get(2).map_or(name.clone(), |g| g.to_string());
        let offset = f
            .get(3)
            .map_or(Ok(0.0), |o| o.parse())
            .context("bad azimuth offset")?;
        let actual: HashMap<i64, f64> = store
            .samples(&format!("pv.inv.{serial}"), 3600, from, to)?
            .into_iter()
            .collect();
        let clip_q = 1.01 * quantile(actual.values().copied().collect(), 0.995);
        // Clear-ish daytime hours present in both weather and actuals.
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
        list.push(Panel {
            serial,
            name,
            group,
            offset,
            inputs,
            actual: y,
            clip_q,
        });
    }
    // Panels that never reach their inverter limit cannot reveal it; they get
    // the highest limit seen (same inverter type assumed).
    let global_clip = list.iter().map(|p| p.clip_q).fold(0.0, f64::max);
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
    let mut groups: Vec<String> = list.iter().map(|p| p.group.clone()).collect();
    groups.dedup();
    let mut results = serde_json::Map::new();
    println!(
        "{:18} {:10} {:>7} {:>5} {:>6} {:>6} {:>8} {:>6}",
        "panel", "group", "azimuth", "tilt", "pdc0", "ac_max", "rmse(W)", "hours"
    );
    for g in groups {
        let idx: Vec<usize> = (0..list.len()).filter(|&i| list[i].group == g).collect();
        let members: Vec<GroupMember> = idx
            .iter()
            .map(|&i| {
                let p = &list[i];
                let ac_max = if p.clip_q < 0.95 * global_clip {
                    global_clip
                } else {
                    p.clip_q
                };
                GroupMember {
                    inputs: &p.inputs,
                    actual: &p.actual,
                    azimuth_offset: p.offset,
                    base: PlaneParams {
                        ac_max,
                        ..base.clone()
                    },
                }
            })
            .collect();
        for (&i, fit) in idx.iter().zip(fit_group(&members, FitOptions::default())) {
            let (p, q) = (&list[i], &fit.params);
            println!(
                "{:18} {:10} {:7.0} {:5.0} {:6.0} {:6.0} {:8.1} {:6}",
                p.name, p.group, q.azimuth, q.tilt, q.pdc0, q.ac_max, fit.rmse, fit.hours_used
            );
            results.insert(
                p.name.clone(),
                serde_json::json!({ "serial": p.serial, "group": p.group, "params": q, "rmse_w": fit.rmse, "hours": fit.hours_used }),
            );
        }
    }
    std::fs::write(out, serde_json::to_string_pretty(&results)?)?;
    println!("written to {}", out.display());
    Ok(())
}

/// Load calibrated planes written by `calibrate-solar`.
fn load_planes(path: &Path) -> Result<Vec<(String, PlaneParams)>> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let obj = v.as_object().context("planes file must be a JSON object")?;
    obj.iter()
        .map(|(name, e)| Ok((name.clone(), serde_json::from_value(e["params"].clone())?)))
        .collect()
}

/// Daily plant energy (kWh) per UTC day from hourly inputs.
fn daily_forecast(planes: &[(String, PlaneParams)], data: &HourlyInputs) -> HashMap<i64, f64> {
    let mut by_day: HashMap<i64, f64> = HashMap::new();
    let per_plane: Vec<Vec<f64>> = planes
        .iter()
        .map(|(_, p)| vulpes_solar::calibrate::simulate_hours(p, &data.inputs))
        .collect();
    for (i, &h) in data.hours.iter().enumerate() {
        let w: f64 = per_plane.iter().map(|v| v[i]).sum();
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
) -> Result<()> {
    let ts = |d: NaiveDate| {
        d.and_hms_opt(0, 0, 0)
            .map(|x| x.and_utc().timestamp())
            .context("bad date")
    };
    let (from, to) = (ts(from)?, ts(to)? + 86_400);
    let planes = load_planes(planes_path)?;
    let prefix = if lead_days == 0 {
        "open-meteo-archive".to_string()
    } else {
        format!("open-meteo-day{lead_days}")
    };
    let models = ["knmi_harmonie_arome_netherlands", "icon_d2"];
    let mut ours = Vec::new();
    for m in models {
        let data = load_inputs(store, &format!("{prefix}:{m}"), lat, lon, from, to)?;
        ours.push(daily_forecast(&planes, &data));
    }
    // Actual daily energy; only complete days (24 hourly means; W·h → kWh).
    let mut actual: HashMap<i64, (f64, usize)> = HashMap::new();
    for (t, w) in store.samples("pv.total.power", 3600, from, to)? {
        let e = actual.entry(t - t.rem_euclid(86_400)).or_default();
        e.0 += w / 1000.0;
        e.1 += 1;
    }
    // Solar Forecast ML "tomorrow" as it stood at 19:00 UTC the day before.
    let sfml: HashMap<i64, f64> = store
        .samples("bench.sfml.tomorrow", 3600, from - 86_400, to)?
        .into_iter()
        .collect();

    let mut days: Vec<i64> = actual
        .iter()
        .filter(|(d, (_, n))| *n == 24 && weeks.includes(**d) && ours.iter().all(|o| o.contains_key(d)))
        .map(|(d, _)| *d)
        .collect();
    days.sort();
    let mean = |ds: &[i64]| ds.iter().map(|d| actual[d].0).sum::<f64>() / ds.len().max(1) as f64;

    let report = |title: &str, ds: &[i64], with_sfml: bool| {
        let m = mean(ds);
        println!("\n{title}: {} days, mean actual {m:.2} kWh/day", ds.len());
        let (mut knmi, mut icon, mut ens, mut sf) = (Score::new(), Score::new(), Score::new(), Score::new());
        for d in ds {
            let a = actual[d].0;
            knmi.add(ours[0][d], a);
            icon.add(ours[1][d], a);
            ens.add(0.5 * (ours[0][d] + ours[1][d]), a);
            if let Some(&s) = sfml.get(&(d - 86_400 + 19 * 3600)) {
                sf.add(s, a);
            }
        }
        println!("  {}", knmi.line("vulpes physics (KNMI)", m));
        println!("  {}", icon.line("vulpes physics (ICON-D2)", m));
        println!("  {}", ens.line("vulpes physics (mean of 2)", m));
        if with_sfml {
            println!("  {}", sf.line("Solar Forecast ML", m));
        }
    };
    report("All evaluated days", &days, false);
    let with_sf: Vec<i64> = days
        .iter()
        .copied()
        .filter(|d| sfml.contains_key(&(d - 86_400 + 19 * 3600)))
        .collect();
    report("Days with a Solar Forecast ML day-ahead value", &with_sf, true);
    Ok(())
}
