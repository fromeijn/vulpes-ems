//! Replay history hour by hour: each strategy decides with only the
//! information available at that moment, then the decision is executed
//! against actual load and PV and settled at actual prices.

use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, TimeZone};
use chrono_tz::Tz;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use vulpes_core::{BatteryParams, GridLimits, ModeDef, Store};
use vulpes_load::{Country, ProfileForecaster, ProfileOptions};
use vulpes_opt::{Problem, SlotInput, optimize, step};
use vulpes_price::LinearTariff;

#[derive(Deserialize)]
struct Export {
    net_metering: bool,
    a: f64,
    b: f64,
}

#[derive(Deserialize)]
struct Rules {
    charge_below: f64,
    grid_below: f64,
    net_zero_below: f64,
    charge_max_soc: f64,
    discharge_min_soc: f64,
    modes: HashMap<String, String>,
}

#[derive(Deserialize)]
pub(crate) struct Site {
    battery: BatteryParams,
    grid: GridLimits,
    modes: Vec<ModeDef>,
    tariff: LinearTariff,
    export: Export,
    rules: Option<Rules>,
}

impl Site {
    fn import_price(&self, wholesale: f64) -> f64 {
        self.tariff.price(wholesale)
    }
    fn export_price(&self, wholesale: f64) -> f64 {
        if self.export.net_metering {
            self.import_price(wholesale)
        } else {
            self.export.a * wholesale + self.export.b
        }
    }
    fn mode_index(&self, name: &str) -> Result<usize> {
        self.modes
            .iter()
            .position(|m| m.name == name)
            .with_context(|| format!("unknown mode '{name}'"))
    }
}

pub(crate) struct ReplayArgs<'a> {
    pub site: &'a Path,
    pub planes: &'a Path,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub tz: Tz,
    pub lat: f64,
    pub lon: f64,
    pub horizon_h: usize,
    pub wholesale: &'a str,
    pub export_override: Option<(f64, f64)>,
}

#[derive(Default, Clone)]
struct Tally {
    cost: f64,
    import_kwh: f64,
    export_kwh: f64,
    throughput_kwh: f64,
    mode_hours: Vec<usize>,
}

pub(crate) fn run(store: &Store, args: &ReplayArgs<'_>) -> Result<()> {
    let mut site: Site = serde_json::from_str(&std::fs::read_to_string(args.site)?)
        .with_context(|| format!("parsing {}", args.site.display()))?;
    if let Some((a, b)) = args.export_override {
        site.export = Export {
            net_metering: false,
            a,
            b,
        };
    }
    site.battery.validate()?;
    let tz = args.tz;
    let all = |k: &str| -> Result<HashMap<i64, f64>> {
        Ok(store.samples(k, 3600, 0, i64::MAX / 2)?.into_iter().collect())
    };
    let (load, pv, price) = (
        all(crate::load::LOAD_KEY)?,
        all("pv.total.power")?,
        all(args.wholesale)?,
    );
    let load_hist: Vec<(i64, f64)> = {
        let mut v: Vec<_> = load.iter().map(|(&t, &x)| (t, x)).collect();
        v.sort_by_key(|x| x.0);
        v
    };
    let price_hist: Vec<(i64, f64)> = {
        let mut v: Vec<_> = price.iter().map(|(&t, &x)| (t, x)).collect();
        v.sort_by_key(|x| x.0);
        v
    };
    // Day-ahead PV forecast: mean of the plant forecast from both weather models.
    let planes = crate::solar::load_planes(args.planes, false)?;
    let midnight = |d: NaiveDate| -> Result<i64> {
        Ok(tz
            .from_local_datetime(&d.and_hms_opt(0, 0, 0).context("date")?)
            .earliest()
            .context("tz")?
            .timestamp())
    };
    let (start, end) = (
        midnight(args.from)?,
        midnight(args.to + chrono::Duration::days(1))?,
    );
    let mut pv_fc: HashMap<i64, f64> = HashMap::new();
    let models = ["knmi_harmonie_arome_netherlands", "icon_d2"];
    let span = (start - 86_400, end + (args.horizon_h as i64 + 24) * 3600);
    for m in models {
        let data = crate::solar::load_inputs(
            store,
            &format!("open-meteo-day1:{m}"),
            args.lat,
            args.lon,
            span.0,
            span.1,
        )?;
        for (h, w) in crate::solar::plant_hourly(&planes, &data) {
            *pv_fc.entry(h).or_default() += w / models.len() as f64;
        }
    }
    // A price for local day D is published at 12:00 UTC on D-1.
    let published = |h: i64| -> i64 {
        let d = tz
            .timestamp_opt(h, 0)
            .single()
            .map(|t| t.date_naive())
            .unwrap_or_default();
        midnight(d).unwrap_or(h) - 12 * 3600
    };

    let rules = site.rules.as_ref().map(|r| -> Result<[usize; 4]> {
        Ok([
            site.mode_index(&r.modes["charge"])?,
            site.mode_index(&r.modes["grid"])?,
            site.mode_index(&r.modes["net_zero"])?,
            site.mode_index(&r.modes["discharge"])?,
        ])
    });
    let rules = rules.transpose()?;
    let net_zero = rules.map(|r| r[2]).unwrap_or(0);
    let names = [
        "self-consumption only",
        "threshold rules (current)",
        "vulpes optimizer",
        "perfect foresight",
    ];
    let mut tallies = vec![
        Tally {
            mode_hours: vec![0; site.modes.len()],
            ..Default::default()
        };
        names.len()
    ];
    let soc0 = 0.5 * site.battery.capacity_wh;
    let mut soc = vec![soc0; names.len()];
    let b = &site.battery;

    let mut day_cache: Option<(NaiveDate, ProfileForecaster, ProfileForecaster)> = None;
    let mut t = start;
    let mut hours = 0usize;
    while t < end {
        let (Some(&l_act), Some(&pv_act), Some(&w_act)) = (load.get(&t), pv.get(&t), price.get(&t)) else {
            t += 3600;
            continue;
        };
        let today = tz.timestamp_opt(t, 0).single().context("tz")?.date_naive();
        if day_cache.as_ref().is_none_or(|c| c.0 != today) {
            let cut = midnight(today)?;
            let lh: Vec<_> = load_hist.iter().copied().filter(|&(x, _)| x < cut).collect();
            let ph: Vec<_> = price_hist.iter().copied().filter(|&(x, _)| x < cut).collect();
            let price_opt = ProfileOptions {
                lookback_days: 28,
                half_life_days: 7.0,
                ..Default::default()
            };
            day_cache = Some((
                today,
                ProfileForecaster::new(&lh, tz, Country::Netherlands, ProfileOptions::default()),
                ProfileForecaster::new(&ph, tz, Country::None, price_opt),
            ));
        }
        let (_, load_fc, price_fc) = day_cache.as_ref().expect("cache set");
        // Forecast inputs for the horizon, as known at time t (perfect ones for the bound).
        let horizon: Vec<i64> = (0..args.horizon_h as i64).map(|k| t + k * 3600).collect();
        let mut fc = Vec::new();
        let mut perfect = Vec::new();
        for &h in &horizon {
            let w = if published(h) <= t {
                price.get(&h).copied()
            } else {
                price_fc.forecast(h)
            };
            let l = load_fc.forecast(h);
            let (Some(w), Some(l), Some(&p)) = (w, l, pv_fc.get(&h)) else {
                break;
            };
            fc.push(SlotInput {
                load_w: l,
                pv_w: p,
                import_price: site.import_price(w),
                export_price: site.export_price(w),
                min_soc_wh: None,
            });
            if let (Some(&la), Some(&pa), Some(&wa)) = (load.get(&h), pv.get(&h), price.get(&h)) {
                perfect.push(SlotInput {
                    load_w: la,
                    pv_w: pa,
                    import_price: site.import_price(wa),
                    export_price: site.export_price(wa),
                    min_soc_wh: None,
                });
            }
        }
        if fc.is_empty() {
            t += 3600;
            continue;
        }
        let terminal = |s: &[SlotInput]| {
            let mut v: Vec<f64> = s.iter().map(|x| x.import_price).collect();
            v.sort_by(f64::total_cmp);
            v[v.len() / 2] * b.discharge_eff
        };
        let plan_mode = |s: &[SlotInput], soc: f64| {
            optimize(&Problem {
                slots: s,
                dt_h: 1.0,
                modes: &site.modes,
                battery: b,
                grid: &site.grid,
                soc0_wh: soc,
                terminal_value_per_kwh: terminal(s),
                soc_step_wh: 100.0,
            })
            .slots[0]
                .mode
        };
        let imp = site.import_price(w_act);
        let choice = [
            net_zero,
            match rules {
                Some([charge, grid, nz, dis]) => {
                    let r = site.rules.as_ref().expect("rules");
                    let frac = soc[1] / b.capacity_wh;
                    if imp < r.charge_below && frac < r.charge_max_soc {
                        charge
                    } else if imp < r.grid_below {
                        grid
                    } else if imp < r.net_zero_below {
                        nz
                    } else if frac > r.discharge_min_soc {
                        dis
                    } else {
                        nz
                    }
                }
                None => net_zero,
            },
            plan_mode(&fc, soc[2]),
            if perfect.len() == fc.len() {
                plan_mode(&perfect, soc[3])
            } else {
                plan_mode(&fc, soc[3])
            },
        ];
        for (k, &m) in choice.iter().enumerate() {
            let o = step(&site.modes[m], l_act, pv_act, soc[k], b, &site.grid, 1.0);
            soc[k] = o.soc_wh;
            let kwh = o.grid_w / 1000.0;
            let tl = &mut tallies[k];
            tl.cost += if kwh >= 0.0 {
                kwh * imp
            } else {
                kwh * site.export_price(w_act)
            } + b.wear_cost_per_kwh * o.battery_w.abs() / 1000.0;
            tl.import_kwh += kwh.max(0.0);
            tl.export_kwh += (-kwh).max(0.0);
            tl.throughput_kwh += o.battery_w.abs() / 1000.0;
            tl.mode_hours[m] += 1;
        }
        hours += 1;
        t += 3600;
    }
    if hours == 0 {
        bail!("no hours with complete data in range");
    }
    let export_desc = if site.export.net_metering {
        "net metering (export = import price)".to_string()
    } else {
        format!("export = {:.3} × wholesale {:+.3}", site.export.a, site.export.b)
    };
    println!(
        "{hours} hours replayed; horizon {} h; {export_desc}; costs include battery wear",
        args.horizon_h
    );
    println!(
        "{:28} {:>10} {:>10} {:>10} {:>11} {:>9}  mode hours",
        "strategy", "cost €", "vs self", "import kWh", "export kWh", "cycles"
    );
    let base = tallies[0].cost;
    for (name, tl) in names.iter().zip(&tallies) {
        let modes: Vec<String> = site
            .modes
            .iter()
            .zip(&tl.mode_hours)
            .map(|(m, h)| format!("{}:{h}", m.name))
            .collect();
        println!(
            "{name:28} {:10.2} {:+10.2} {:10.0} {:11.0} {:9.1}  {}",
            tl.cost,
            tl.cost - base,
            tl.import_kwh,
            tl.export_kwh,
            tl.throughput_kwh / 2.0 / (b.capacity_wh / 1000.0),
            modes.join(" ")
        );
    }
    Ok(())
}
