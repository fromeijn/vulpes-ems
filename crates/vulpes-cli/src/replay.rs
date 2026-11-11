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

/// Export pricing: `a · wholesale + b`. Unknown keys are an error, so a site
/// file with the former `net_metering` switch doesn't replay another scenario
/// unnoticed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Export {
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
        self.export.a * wholesale + self.export.b
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

#[derive(Debug, Default, Clone, PartialEq)]
struct Tally {
    cost: f64,
    import_kwh: f64,
    export_kwh: f64,
    throughput_kwh: f64,
    mode_hours: Vec<usize>,
}

impl Tally {
    /// Settle one hour with grid and battery power in W (grid positive = import).
    fn settle(&mut self, grid_w: f64, battery_w: f64, import: f64, export: f64, wear: f64) {
        let kwh = grid_w / 1000.0;
        let throughput = battery_w.abs() / 1000.0;
        self.cost += if kwh >= 0.0 { kwh * import } else { kwh * export } + wear * throughput;
        self.import_kwh += kwh.max(0.0);
        self.export_kwh += (-kwh).max(0.0);
        self.throughput_kwh += throughput;
    }
}

const STRATEGIES: [&str; 4] = [
    "self-consumption only",
    "threshold rules (current)",
    "vulpes optimizer",
    "perfect foresight",
];

/// Hourly history the replay runs on, keyed by hour start (Unix seconds).
struct ReplayData {
    /// Actual house load excluding the battery (W).
    load: HashMap<i64, f64>,
    /// Actual PV (W).
    pv: HashMap<i64, f64>,
    /// Actual wholesale price (currency/kWh).
    price: HashMap<i64, f64>,
    /// Day-ahead PV forecast (W).
    pv_fc: HashMap<i64, f64>,
}

struct Report {
    hours: usize,
    tallies: Vec<Tally>,
}

/// Local midnight starting date `d`.
fn midnight(tz: Tz, d: NaiveDate) -> Result<i64> {
    Ok(tz
        .from_local_datetime(&d.and_hms_opt(0, 0, 0).context("date")?)
        .earliest()
        .context("tz")?
        .timestamp())
}

/// When the day-ahead price for hour `h` becomes known: 12:00 UTC the day
/// before its local day.
fn published_at(tz: Tz, h: i64) -> i64 {
    let day = tz
        .timestamp_opt(h, 0)
        .single()
        .map(|t| t.date_naive())
        .unwrap_or_default();
    midnight(tz, day).unwrap_or(h) - 12 * 3600
}

/// Value of stored energy after the horizon: median import price × discharge efficiency.
fn terminal_value(slots: &[SlotInput], discharge_eff: f64) -> f64 {
    let mut v: Vec<f64> = slots.iter().map(|x| x.import_price).collect();
    v.sort_by(f64::total_cmp);
    v[v.len() / 2] * discharge_eff
}

/// The threshold rules: `[charge, grid, net_zero, discharge]` mode indices.
fn rule_choice(r: &Rules, idx: [usize; 4], import: f64, soc_frac: f64) -> usize {
    let [charge, grid, net_zero, discharge] = idx;
    if import < r.charge_below && soc_frac < r.charge_max_soc {
        charge
    } else if import < r.grid_below {
        grid
    } else if import < r.net_zero_below {
        net_zero
    } else if soc_frac > r.discharge_min_soc {
        discharge
    } else {
        net_zero
    }
}

fn sorted(m: &HashMap<i64, f64>) -> Vec<(i64, f64)> {
    let mut v: Vec<_> = m.iter().map(|(&t, &x)| (t, x)).collect();
    v.sort_by_key(|x| x.0);
    v
}

/// Run every strategy hour by hour over `[start, end)`. Each decision only
/// uses data available at that hour; the decision is then executed against
/// the actual load and PV and settled at the actual price.
fn simulate(
    site: &Site,
    data: &ReplayData,
    tz: Tz,
    start: i64,
    end: i64,
    horizon_h: usize,
) -> Result<Report> {
    let (load, pv, price) = (&data.load, &data.pv, &data.price);
    let (load_hist, price_hist) = (sorted(load), sorted(price));
    let rules = match &site.rules {
        Some(r) => Some([
            site.mode_index(r.modes.get("charge").context("rules.modes.charge missing")?)?,
            site.mode_index(r.modes.get("grid").context("rules.modes.grid missing")?)?,
            site.mode_index(r.modes.get("net_zero").context("rules.modes.net_zero missing")?)?,
            site.mode_index(
                r.modes
                    .get("discharge")
                    .context("rules.modes.discharge missing")?,
            )?,
        ]),
        None => None,
    };
    let net_zero = match rules {
        Some(r) => r[2],
        None => site
            .modes
            .iter()
            .position(|m| m.behaviour == vulpes_core::Behaviour::SelfConsume)
            .context("no self_consume mode for the self-consumption baseline")?,
    };
    let b = &site.battery;
    let mut tallies = vec![
        Tally {
            mode_hours: vec![0; site.modes.len()],
            ..Default::default()
        };
        STRATEGIES.len()
    ];
    let mut soc = vec![0.5 * b.capacity_wh; STRATEGIES.len()];
    let plan_mode = |s: &[SlotInput], soc: f64| {
        optimize(&Problem {
            slots: s,
            dt_h: 1.0,
            modes: &site.modes,
            battery: b,
            grid: &site.grid,
            soc0_wh: soc,
            terminal_value_per_kwh: terminal_value(s, b.discharge_eff),
            soc_step_wh: 100.0,
        })
        .slots[0]
            .mode
    };

    let mut day_cache: Option<(NaiveDate, ProfileForecaster, ProfileForecaster)> = None;
    let mut hours = 0usize;
    let mut t = start;
    while t < end {
        let (Some(&l_act), Some(&pv_act), Some(&w_act)) = (load.get(&t), pv.get(&t), price.get(&t)) else {
            t += 3600;
            continue;
        };
        // Forecasters are refit once per local day on data before its midnight.
        let today = tz.timestamp_opt(t, 0).single().context("tz")?.date_naive();
        if day_cache.as_ref().is_none_or(|c| c.0 != today) {
            let cut = midnight(tz, today)?;
            let before = |h: &[(i64, f64)]| -> Vec<(i64, f64)> {
                h.iter().copied().filter(|&(x, _)| x < cut).collect()
            };
            let price_opt = ProfileOptions {
                lookback_days: 28,
                half_life_days: 7.0,
                ..Default::default()
            };
            day_cache = Some((
                today,
                ProfileForecaster::new(
                    &before(&load_hist),
                    tz,
                    Country::Netherlands,
                    ProfileOptions::default(),
                ),
                ProfileForecaster::new(&before(&price_hist), tz, Country::None, price_opt),
            ));
        }
        let (_, load_fc, price_fc) = day_cache.as_ref().expect("cache set");
        // Forecast inputs as known at t, and the actual values for the perfect-foresight bound.
        let mut fc = Vec::new();
        let mut perfect = Vec::new();
        for h in (0..horizon_h as i64).map(|k| t + k * 3600) {
            let w = if published_at(tz, h) <= t {
                price.get(&h).copied()
            } else {
                price_fc.forecast(h)
            };
            let (Some(w), Some(l), Some(&p)) = (w, load_fc.forecast(h), data.pv_fc.get(&h)) else {
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
        let (imp, exp) = (site.import_price(w_act), site.export_price(w_act));
        let choice = [
            net_zero,
            match (rules, &site.rules) {
                (Some(idx), Some(r)) => rule_choice(r, idx, imp, soc[1] / b.capacity_wh),
                _ => net_zero,
            },
            plan_mode(&fc, soc[2]),
            plan_mode(if perfect.len() == fc.len() { &perfect } else { &fc }, soc[3]),
        ];
        for (k, &m) in choice.iter().enumerate() {
            let o = step(&site.modes[m], l_act, pv_act, soc[k], b, &site.grid, 1.0);
            soc[k] = o.soc_wh;
            tallies[k].settle(o.grid_w, o.battery_w, imp, exp, b.wear_cost_per_kwh);
            tallies[k].mode_hours[m] += 1;
        }
        hours += 1;
        t += 3600;
    }
    if hours == 0 {
        bail!("no hours with complete data in range");
    }
    Ok(Report { hours, tallies })
}

pub(crate) fn run(store: &Store, args: &ReplayArgs<'_>) -> Result<()> {
    let mut site: Site = serde_json::from_str(&std::fs::read_to_string(args.site)?)
        .with_context(|| format!("parsing {}", args.site.display()))?;
    if let Some((a, b)) = args.export_override {
        site.export = Export { a, b };
    }
    site.battery.validate()?;
    let tz = args.tz;
    let all = |k: &str| -> Result<HashMap<i64, f64>> {
        Ok(store.samples(k, 3600, 0, i64::MAX / 2)?.into_iter().collect())
    };
    let (start, end) = (
        midnight(tz, args.from)?,
        midnight(tz, args.to + chrono::Duration::days(1))?,
    );
    // Day-ahead PV forecast: mean of the plant forecast from both weather models.
    let planes = crate::solar::load_planes(args.planes, false)?;
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
    let data = ReplayData {
        load: all(crate::load::LOAD_KEY)?,
        pv: all("pv.total.power")?,
        price: all(args.wholesale)?,
        pv_fc,
    };
    let Report { hours, tallies } = simulate(&site, &data, tz, start, end, args.horizon_h)?;
    let b = &site.battery;
    let export_desc = format!("export = {:.3} × wholesale {:+.3}", site.export.a, site.export.b);
    println!(
        "{hours} hours replayed; horizon {} h; {export_desc}; costs include battery wear",
        args.horizon_h
    );
    println!(
        "{:28} {:>10} {:>10} {:>10} {:>11} {:>9}  mode hours",
        "strategy", "cost €", "vs self", "import kWh", "export kWh", "cycles"
    );
    let base = tallies[0].cost;
    for (name, tl) in STRATEGIES.iter().zip(&tallies) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use vulpes_core::Behaviour;

    const TZ: Tz = chrono_tz::Europe::Amsterdam;

    fn site(rules: bool) -> Site {
        let m = |n: &str, b| ModeDef {
            name: n.into(),
            behaviour: b,
            pv_curtail: false,
            no_export: false,
        };
        Site {
            battery: BatteryParams {
                capacity_wh: 10_000.0,
                max_charge_w: 3000.0,
                max_discharge_w: 3000.0,
                charge_eff: 0.95,
                discharge_eff: 0.95,
                min_soc: 0.1,
                max_soc: 1.0,
                wear_cost_per_kwh: 0.01,
            },
            grid: GridLimits {
                max_import_w: 17_000.0,
                max_export_w: 17_000.0,
            },
            modes: vec![
                m("charge", Behaviour::GridSetpoint { w: 3000.0 }),
                m("hold", Behaviour::NoDischarge),
                m("self", Behaviour::SelfConsume),
                m("discharge", Behaviour::GridSetpoint { w: -3000.0 }),
            ],
            tariff: LinearTariff { a: 1.0, b: 0.0 },
            export: Export { a: 1.0, b: 0.0 },
            rules: rules.then(|| Rules {
                charge_below: 0.15,
                grid_below: 0.25,
                net_zero_below: 0.45,
                charge_max_soc: 0.6,
                discharge_min_soc: 0.3,
                modes: [
                    ("charge", "charge"),
                    ("grid", "hold"),
                    ("net_zero", "self"),
                    ("discharge", "discharge"),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            }),
        }
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    /// `days` days from 1 Oct: constant 500 W load, no PV, wholesale 0.10
    /// from 00–06 local time and 0.40 otherwise.
    fn data(days: u32) -> ReplayData {
        let (start, end) = (
            midnight(TZ, day(1)).unwrap(),
            midnight(TZ, day(1 + days)).unwrap(),
        );
        let hours: Vec<i64> = (start..end).step_by(3600).collect();
        let price = |h: i64| {
            let local = TZ.timestamp_opt(h, 0).unwrap();
            if chrono::Timelike::hour(&local) < 6 {
                0.10
            } else {
                0.40
            }
        };
        ReplayData {
            load: hours.iter().map(|&h| (h, 500.0)).collect(),
            pv: hours.iter().map(|&h| (h, 0.0)).collect(),
            price: hours.iter().map(|&h| (h, price(h))).collect(),
            pv_fc: hours.iter().map(|&h| (h, 0.0)).collect(),
        }
    }

    #[test]
    fn tally_settles_import_export_and_wear() {
        let mut t = Tally::default();
        t.settle(2000.0, 1000.0, 0.30, 0.10, 0.02); // import 2 kWh, 1 kWh through the battery
        assert!((t.cost - (0.60 + 0.02)).abs() < 1e-12);
        t.settle(-1000.0, -500.0, 0.30, 0.10, 0.02); // export 1 kWh
        assert!((t.cost - (0.62 - 0.10 + 0.01)).abs() < 1e-12);
        assert_eq!((t.import_kwh, t.export_kwh, t.throughput_kwh), (2.0, 1.0, 1.5));
    }

    #[test]
    fn day_ahead_prices_are_published_at_noon_utc_the_day_before() {
        // 2026-11-08 15:00 UTC is local 16:00 on 8 Nov (UTC+1); local midnight
        // is 7 Nov 23:00 UTC, so the price was known from 7 Nov 11:00 UTC.
        let h = 1_794_150_000; // 2026-11-08 15:00 UTC
        assert_eq!(published_at(TZ, h), 1_794_049_200); // 2026-11-07 11:00 UTC
        // 23:30 UTC on 8 Nov is already 9 Nov locally.
        assert_eq!(published_at(TZ, 1_794_180_600), 1_794_049_200 + 86_400);
    }

    #[test]
    fn terminal_value_is_the_median_import_price_after_losses() {
        let s = |p: f64| SlotInput {
            load_w: 0.0,
            pv_w: 0.0,
            import_price: p,
            export_price: p,
            min_soc_wh: None,
        };
        let v = [s(0.4), s(0.1), s(0.3)];
        assert!((terminal_value(&v, 0.9) - 0.27).abs() < 1e-12);
    }

    #[test]
    fn threshold_rules_cover_every_branch() {
        let site = site(true);
        let r = site.rules.as_ref().unwrap();
        let idx = [0, 1, 2, 3];
        assert_eq!(rule_choice(r, idx, 0.10, 0.5), 0, "cheap and room to charge");
        assert_eq!(rule_choice(r, idx, 0.10, 0.7), 1, "cheap but already charged");
        assert_eq!(rule_choice(r, idx, 0.20, 0.2), 1, "below the grid threshold");
        assert_eq!(rule_choice(r, idx, 0.30, 0.2), 2, "below the net-zero threshold");
        assert_eq!(rule_choice(r, idx, 0.50, 0.5), 3, "expensive and energy to spare");
        assert_eq!(
            rule_choice(r, idx, 0.50, 0.2),
            2,
            "expensive but at the discharge floor"
        );
    }

    #[test]
    fn site_prices() {
        let mut s = site(false);
        s.tariff = LinearTariff { a: 1.21, b: 0.13 };
        s.export = Export { a: 1.1, b: -0.02 };
        assert!((s.import_price(0.1) - 0.251).abs() < 1e-12);
        assert!((s.export_price(0.1) - 0.09).abs() < 1e-12);
        // Net metering: the export tariff equals the import tariff.
        s.export = Export { a: 1.21, b: 0.13 };
        assert_eq!(s.export_price(0.1), s.import_price(0.1));
        assert!(s.mode_index("hold").is_ok() && s.mode_index("nope").is_err());
    }

    #[test]
    fn export_rejects_the_former_net_metering_switch() {
        let ok: Export = serde_json::from_str(r#"{"a": 1.0, "b": -0.02}"#).unwrap();
        assert_eq!((ok.a, ok.b), (1.0, -0.02));
        let err = serde_json::from_str::<Export>(r#"{"net_metering": true, "a": 1.0, "b": 0.0}"#)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("net_metering"), "{err}");
    }

    #[test]
    fn optimizer_exploits_the_daily_spread() {
        let site = site(true);
        let (start, end) = (midnight(TZ, day(15)).unwrap(), midnight(TZ, day(22)).unwrap());
        let r = simulate(&site, &data(25), TZ, start, end, 36).unwrap();
        assert_eq!(r.hours, 7 * 24);
        let [own, rules, opt, perfect] = [0, 1, 2, 3].map(|k| r.tallies[k].cost);
        assert!(opt < own - 1.0, "optimizer {opt} vs self-consumption {own}");
        assert!(opt <= rules + 1e-9, "optimizer {opt} vs rules {rules}");
        // Load, PV and prices repeat exactly, so the forecasts are perfect too.
        assert!(
            (opt - perfect).abs() < 0.05,
            "optimizer {opt} vs perfect {perfect}"
        );
        assert!(r.tallies[2].mode_hours[0] > 0, "charges in the cheap hours");
        assert!(
            r.tallies
                .iter()
                .all(|t| t.mode_hours.iter().sum::<usize>() == r.hours)
        );
    }

    #[test]
    fn unpublished_prices_do_not_leak_into_earlier_decisions() {
        let site = site(true);
        let base = data(25);
        // A price spike on 20 Oct evening, published 19 Oct 12:00 UTC.
        let spike_hour = midnight(TZ, day(20)).unwrap() + 19 * 3600;
        let mut spiked = data(25);
        spiked.price.insert(spike_hour, 5.0);
        let published = published_at(TZ, spike_hour);
        let start = midnight(TZ, day(16)).unwrap();
        let before = |d: &ReplayData| simulate(&site, d, TZ, start, published, 36).unwrap();
        let (a, b) = (before(&base), before(&spiked));
        assert_eq!(
            a.tallies[2], b.tallies[2],
            "optimizer saw the spike before publication"
        );
        // Once published, the optimizer reacts to it (perfect foresight always could).
        let after = |d: &ReplayData| simulate(&site, d, TZ, start, spike_hour + 3600, 36).unwrap();
        assert_ne!(after(&base).tallies[2], after(&spiked).tallies[2]);
    }

    #[test]
    fn hours_with_missing_data_are_skipped() {
        let site = site(true);
        let mut d = data(25);
        let start = midnight(TZ, day(15)).unwrap();
        d.load.remove(&(start + 5 * 3600));
        d.pv.remove(&(start + 6 * 3600));
        d.price.remove(&(start + 7 * 3600));
        let r = simulate(&site, &d, TZ, start, start + 24 * 3600, 36).unwrap();
        assert_eq!(r.hours, 21);
        let empty = ReplayData {
            load: HashMap::new(),
            pv: HashMap::new(),
            price: HashMap::new(),
            pv_fc: HashMap::new(),
        };
        let err = simulate(&site, &empty, TZ, start, start + 86_400, 36)
            .err()
            .unwrap();
        assert!(err.to_string().contains("no hours with complete data"));
    }

    #[test]
    fn configuration_errors() {
        let start = midnight(TZ, day(15)).unwrap();
        let mut s = site(true);
        s.rules
            .as_mut()
            .unwrap()
            .modes
            .insert("grid".into(), "missing".into());
        assert!(simulate(&s, &data(16), TZ, start, start + 3600, 36).is_err());
        let mut s = site(true);
        s.rules.as_mut().unwrap().modes.remove("discharge");
        assert!(simulate(&s, &data(16), TZ, start, start + 3600, 36).is_err());
        // Without rules, the baseline needs a self-consume mode.
        let mut s = site(false);
        s.modes.retain(|m| m.behaviour != Behaviour::SelfConsume);
        assert!(simulate(&s, &data(16), TZ, start, start + 3600, 36).is_err());
    }
}
