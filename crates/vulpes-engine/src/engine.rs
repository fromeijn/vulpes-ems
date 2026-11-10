use crate::{Error, PlanRequest, PlanResponse, PlanSummary, Result, SlotPlan, WeatherSlot};
use chrono::{DateTime, Datelike};
use chrono_tz::Tz;
use std::collections::HashMap;
use vulpes_load::{Country, ProfileForecaster, ProfileOptions};
use vulpes_opt::{Problem, SlotInput, optimize};
use vulpes_solar::model::{Weather, system_ac_power, validate_planes};
use vulpes_solar::sun_position;
use vulpes_sources::open_meteo::WeatherSeries;
use vulpes_sources::{Location as SrcLocation, open_meteo};

pub(crate) const WEATHER_VARS: &[&str] = &[
    "shortwave_radiation",
    "direct_normal_irradiance",
    "diffuse_radiation",
    "temperature_2m",
    "wind_speed_10m",
];

/// Weather per slot start: the mean over all models that have the slot.
fn fetch_weather(req: &PlanRequest) -> Result<HashMap<i64, WeatherSlot>> {
    if let Some(w) = &req.weather {
        return Ok(w.iter().map(|s| (s.start, *s)).collect());
    }
    let models: Vec<&str> = req.weather_models.iter().map(String::as_str).collect();
    let days = (req.horizon_h / 24 + 2).min(16) as u8;
    let series = open_meteo::fetch(&open_meteo::Request {
        location: SrcLocation {
            lat: req.location.lat,
            lon: req.location.lon,
        },
        models: &models,
        vars: WEATHER_VARS,
        range: open_meteo::Range::Forecast { days },
    })
    .map_err(|e| Error::Weather(e.to_string()))?;
    Ok(average_models(series))
}

/// Combine per-model series into one weather slot per start time: the mean
/// over the models that have all variables at that time.
pub(crate) fn average_models(series: Vec<WeatherSeries>) -> HashMap<i64, WeatherSlot> {
    // (start, model) -> [ghi, dni, dhi, temp, wind]
    let mut per: HashMap<(i64, String), [Option<f64>; 5]> = HashMap::new();
    for s in series {
        let Some(k) = WEATHER_VARS.iter().position(|v| *v == s.var) else {
            continue;
        };
        for (t, v) in s.points {
            per.entry((t, s.model.clone())).or_insert([None; 5])[k] = Some(v);
        }
    }
    let mut acc: HashMap<i64, ([f64; 5], u32)> = HashMap::new();
    for ((t, _), vals) in per {
        if let [Some(a), Some(b), Some(c), Some(d), Some(e)] = vals {
            let entry = acc.entry(t).or_default();
            for (x, v) in entry.0.iter_mut().zip([a, b, c, d, e]) {
                *x += v;
            }
            entry.1 += 1;
        }
    }
    acc.into_iter()
        .map(|(t, (s, n))| {
            let n = n as f64;
            (
                t,
                WeatherSlot {
                    start: t,
                    ghi: s[0] / n,
                    dni: s[1] / n,
                    dhi: s[2] / n,
                    temp_air: s[3] / n,
                    wind_speed: s[4] / n,
                },
            )
        })
        .collect()
}

/// Mean of a 15-minute weather series over `[start, start + step)`.
fn weather_for(weather: &HashMap<i64, WeatherSlot>, start: i64, step: i64) -> Option<Vec<WeatherSlot>> {
    let v: Vec<WeatherSlot> = (0..(step / 900).max(1))
        .filter_map(|k| weather.get(&(start + k * 900)).copied())
        .collect();
    (!v.is_empty()).then_some(v)
}

/// Load history without the hours a house cannot have consumed: more than the
/// grid connection, the battery and the solar panels can deliver together (or
/// not a finite number). Such hours come from meter glitches, like a counter
/// reset, and one of them distorts the profile's mean for weeks. Returns the
/// kept history and the number of hours left out.
fn plausible_load(req: &PlanRequest) -> (Vec<(i64, f64)>, usize) {
    let limit_w =
        req.grid.max_import_w + req.battery.max_discharge_w + req.planes.iter().map(|p| p.pdc0).sum::<f64>();
    let kept: Vec<(i64, f64)> = req
        .load_history
        .iter()
        .copied()
        .filter(|&(_, w)| w.is_finite() && w <= limit_w)
        .collect();
    let ignored = req.load_history.len() - kept.len();
    (kept, ignored)
}

pub fn plan(req: &PlanRequest) -> Result<PlanResponse> {
    req.battery.validate()?;
    if req.modes.is_empty() {
        return Err(Error::Request("no modes defined".into()));
    }
    if req.step_s <= 0 || req.step_s % 900 != 0 {
        return Err(Error::Request("step_s must be a positive multiple of 900".into()));
    }
    validate_planes(&req.planes).map_err(Error::Request)?;
    let tz: Tz = req
        .timezone
        .parse()
        .map_err(|_| Error::Request(format!("unknown time zone '{}'", req.timezone)))?;
    let country = match req.country.as_str() {
        "nl" => Country::Netherlands,
        _ => Country::None,
    };
    let mut warnings = Vec::new();
    let step = req.step_s;
    let start = req.now - req.now.rem_euclid(step);
    let n = (req.horizon_h as i64 * 3600 / step) as usize;
    let dt_h = step as f64 / 3600.0;

    // PV forecast per slot.
    let weather = fetch_weather(req)?;
    let pv: Vec<Option<f64>> = (0..n)
        .map(|i| {
            let t = start + i as i64 * step;
            let ws = weather_for(&weather, t, step)?;
            let mut total = 0.0;
            for w in &ws {
                let sun = sun_position((w.start + 450) as f64, req.location.lat, req.location.lon);
                let doy = DateTime::from_timestamp(w.start, 0).map_or(1, |d| d.ordinal());
                let wx = Weather {
                    ghi: w.ghi,
                    dni: w.dni,
                    dhi: w.dhi,
                    temp_air: w.temp_air,
                    wind_speed: w.wind_speed,
                };
                total += system_ac_power(&req.planes, sun, wx, doy);
            }
            Some(total / ws.len() as f64)
        })
        .collect();
    let pv_missing = pv.iter().filter(|p| p.is_none()).count();
    if pv_missing > n / 4 {
        return Err(Error::Weather(format!(
            "no weather for {pv_missing} of {n} slots"
        )));
    }
    if pv_missing > 0 {
        warnings.push(format!(
            "no weather for {pv_missing} slots at the end of the horizon; PV assumed 0 there"
        ));
    }

    // Load forecast (hourly profile, applied to each slot of the hour).
    let (load_history, load_ignored) = plausible_load(req);
    if load_ignored > 0 {
        warnings.push(format!(
            "ignored {load_ignored} load hours above the physical limit"
        ));
    }
    if load_history.is_empty() {
        return Err(Error::Request("load_history is empty".into()));
    }
    let load_fc = ProfileForecaster::new(&load_history, tz, country, ProfileOptions::default());
    let mean_load = load_history.iter().map(|x| x.1).sum::<f64>() / load_history.len() as f64;
    let mut load_fallback = 0;

    // Prices: published where known, else forecast wholesale through the tariff.
    let price_fc = (!req.wholesale_history.is_empty()).then(|| {
        let opt = ProfileOptions {
            lookback_days: 28,
            half_life_days: 7.0,
            ..Default::default()
        };
        ProfileForecaster::new(&req.wholesale_history, tz, Country::None, opt)
    });
    let export_from = |import: f64| {
        if req.export.net_metering {
            import
        } else {
            let wholesale = (import - req.tariff.b) / req.tariff.a;
            req.export.a * wholesale + req.export.b
        }
    };
    let mean_known = (!req.known_prices.is_empty())
        .then(|| req.known_prices.iter().map(|p| p.import).sum::<f64>() / req.known_prices.len() as f64);
    let mut price_fallback = 0;

    let mut inputs = Vec::with_capacity(n);
    let mut known_flags = Vec::with_capacity(n);
    for (i, pv_w) in pv.iter().enumerate() {
        let t = start + i as i64 * step;
        let hour = t - t.rem_euclid(3600);
        let load = load_fc.forecast(hour).unwrap_or_else(|| {
            load_fallback += 1;
            mean_load
        });
        let known = req.known_prices.iter().find(|p| p.start <= t && t < p.end);
        let import = match known {
            Some(p) => p.import,
            None => match price_fc.as_ref().and_then(|f| f.forecast(hour)) {
                Some(w) => req.tariff.price(w),
                None => {
                    price_fallback += 1;
                    mean_known
                        .ok_or_else(|| Error::Request("no known prices and no wholesale history".into()))?
                }
            },
        };
        known_flags.push(known.is_some());
        inputs.push(SlotInput {
            load_w: load,
            pv_w: pv_w.unwrap_or(0.0),
            import_price: import,
            export_price: export_from(import),
            min_soc_wh: None,
        });
    }
    if load_fallback > 0 {
        warnings.push(format!(
            "load profile missing for {load_fallback} slots; used the historical mean"
        ));
    }
    if price_fallback > 0 {
        warnings.push(format!(
            "no price forecast for {price_fallback} slots; used the mean known price"
        ));
    }

    let terminal = {
        let mut v: Vec<f64> = inputs.iter().map(|s| s.import_price).collect();
        v.sort_by(f64::total_cmp);
        v[v.len() / 2] * req.battery.discharge_eff
    };
    let plan = optimize(&Problem {
        slots: &inputs,
        dt_h,
        modes: &req.modes,
        battery: &req.battery,
        grid: &req.grid,
        soc0_wh: req.soc_wh,
        terminal_value_per_kwh: terminal,
        soc_step_wh: 100.0,
    });
    let slots: Vec<SlotPlan> = plan
        .slots
        .iter()
        .zip(&inputs)
        .zip(&known_flags)
        .enumerate()
        .map(|(i, ((p, inp), &known))| SlotPlan {
            start: start + i as i64 * step,
            mode: req.modes[p.mode].name.clone(),
            soc_pct: 100.0 * p.soc_wh / req.battery.capacity_wh,
            battery_w: p.battery_w,
            grid_w: p.grid_w,
            pv_w: inp.pv_w,
            load_w: inp.load_w,
            import_price: inp.import_price,
            export_price: inp.export_price,
            price_known: known,
        })
        .collect();
    let day_slots = ((24 * 3600 / step) as usize).min(n);
    let kwh = |f: &dyn Fn(&SlotInput) -> f64| inputs[..day_slots].iter().map(f).sum::<f64>() * dt_h / 1000.0;
    Ok(PlanResponse {
        version: env!("CARGO_PKG_VERSION").into(),
        summary: PlanSummary {
            mode_now: slots.first().map(|s| s.mode.clone()).unwrap_or_default(),
            cost: plan.cost,
            pv_kwh_24h: kwh(&|s| s.pv_w),
            load_kwh_24h: kwh(&|s| s.load_w),
            terminal_value_per_kwh: terminal,
        },
        slots,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExportTariff, KnownPrice, Location};
    use vulpes_core::{BatteryParams, Behaviour, GridLimits, ModeDef};
    use vulpes_price::LinearTariff;
    use vulpes_solar::PlaneParams;

    fn request(now: i64) -> PlanRequest {
        let m = |n: &str, b| ModeDef {
            name: n.into(),
            behaviour: b,
            pv_curtail: false,
            no_export: false,
        };
        // Clear-sky-ish synthetic weather every 15 minutes for two days.
        let weather = (0..(2 * 96))
            .map(|k| {
                let t = now - now.rem_euclid(900) + k * 900;
                let s = sun_position((t + 450) as f64, 52.0, 5.0);
                let cz = s.zenith.to_radians().cos().max(0.0);
                let dni = if cz > 0.0 {
                    800.0 * (-0.15 / cz.max(0.05)).exp()
                } else {
                    0.0
                };
                WeatherSlot {
                    start: t,
                    ghi: dni * cz + 80.0 * cz,
                    dni,
                    dhi: 80.0 * cz,
                    temp_air: 12.0,
                    wind_speed: 3.0,
                }
            })
            .collect();
        PlanRequest {
            now,
            step_s: 900,
            horizon_h: 24,
            location: Location { lat: 52.0, lon: 5.0 },
            timezone: "Europe/Amsterdam".into(),
            country: "nl".into(),
            battery: BatteryParams {
                capacity_wh: 10_000.0,
                max_charge_w: 3000.0,
                max_discharge_w: 3000.0,
                charge_eff: 0.95,
                discharge_eff: 0.95,
                min_soc: 0.1,
                max_soc: 1.0,
                wear_cost_per_kwh: 0.02,
            },
            grid: GridLimits {
                max_import_w: 17_000.0,
                max_export_w: 17_000.0,
            },
            modes: vec![
                m("Charging", Behaviour::GridSetpoint { w: 3000.0 }),
                m("Grid", Behaviour::NoDischarge),
                m("Net Zero", Behaviour::SelfConsume),
                m("Discharging", Behaviour::GridSetpoint { w: -3000.0 }),
            ],
            soc_wh: 5000.0,
            planes: vec![PlaneParams::new(180.0, 35.0, 3000.0, 2800.0)],
            weather_models: vec![],
            load_history: (0..(24 * 30))
                .map(|h| (now - 30 * 86_400 + h * 3600, 400.0))
                .collect(),
            known_prices: (0..24)
                .map(|h| {
                    let s = now - now.rem_euclid(3600) + h * 3600;
                    KnownPrice {
                        start: s,
                        end: s + 3600,
                        import: if h < 6 { 0.10 } else { 0.40 },
                    }
                })
                .collect(),
            tariff: LinearTariff { a: 1.21, b: 0.13 },
            export: ExportTariff {
                net_metering: false,
                a: 1.21,
                b: 0.0,
            },
            wholesale_history: vec![],
            weather: Some(weather),
        }
    }

    #[test]
    fn produces_a_full_plan_offline() {
        let now = 1_781_000_000; // mid-June 2026
        let resp = plan(&request(now)).unwrap();
        assert_eq!(resp.slots.len(), 96);
        assert!(resp.summary.pv_kwh_24h > 5.0, "{:?}", resp.summary);
        assert!((resp.summary.load_kwh_24h - 9.6).abs() < 0.5);
        assert!(resp.slots.iter().all(|s| s.price_known));
        assert!(resp.slots.iter().all(|s| (10.0..=100.0).contains(&s.soc_pct)));
        // Cheap first hours, expensive later, and a sunny morning: the battery
        // fills from solar alone, so "Charging" (a tie once it is full) is not planned.
        let cheap = &resp.slots[..24];
        assert!(cheap.iter().all(|s| s.mode != "Charging"), "{cheap:?}");
        assert!(cheap.iter().all(|s| s.grid_w <= 1e-6), "{cheap:?}");
        assert!(cheap.last().unwrap().soc_pct > 99.9, "{cheap:?}");
        // Without sun it charges from the grid in the cheap hours.
        let mut dark = request(now);
        for w in dark.weather.as_mut().unwrap() {
            (w.ghi, w.dni, w.dhi) = (0.0, 0.0, 0.0);
        }
        let resp = plan(&dark).unwrap();
        assert!(
            resp.slots[..24]
                .iter()
                .any(|s| s.mode == "Charging" && s.battery_w > 0.0),
            "{:?}",
            &resp.slots[..24]
        );
    }

    #[test]
    fn planes_on_a_shared_inverter_clip_together() {
        let mut r = request(1_781_000_000);
        // East and west, each 3 kWp; separate 2 kW inverters vs one shared 2 kW inverter.
        r.planes = vec![
            PlaneParams::new(90.0, 35.0, 3000.0, 2000.0),
            PlaneParams::new(270.0, 35.0, 3000.0, 2000.0),
        ];
        let separate = plan(&r).unwrap();
        for p in &mut r.planes {
            p.inverter = Some(0);
        }
        let shared = plan(&r).unwrap();
        let peak = |resp: &PlanResponse| resp.slots.iter().map(|s| s.pv_w).fold(0.0, f64::max);
        assert!(peak(&separate) > 2000.0, "{}", peak(&separate));
        assert!(peak(&shared) <= 2000.0 + 1e-9, "{}", peak(&shared));
        assert!(shared.summary.pv_kwh_24h < separate.summary.pv_kwh_24h);
        r.planes[1].ac_max = 2500.0;
        let err = plan(&r).unwrap_err().to_string();
        assert!(err.contains("same ac_max"), "{err}");
    }

    #[test]
    fn json_roundtrip_and_errors() {
        let req = request(1_781_000_000);
        let out = crate::plan_json(&serde_json::to_string(&req).unwrap()).unwrap();
        let resp: PlanResponse = serde_json::from_str(&out).unwrap();
        assert_eq!(resp.slots.len(), 96);
        let mut bad = req.clone();
        bad.load_history.clear();
        assert!(plan(&bad).is_err());
        let mut bad = req;
        bad.timezone = "Mars/Olympus".into();
        assert!(plan(&bad).is_err());
    }

    fn ws(start: i64, ghi: f64) -> WeatherSlot {
        WeatherSlot {
            start,
            ghi,
            dni: 2.0 * ghi,
            dhi: 0.5 * ghi,
            temp_air: 10.0,
            wind_speed: 2.0,
        }
    }

    #[test]
    fn average_models_uses_complete_models_only() {
        let s = |model: &str, var: &str, pts: Vec<(i64, f64)>| WeatherSeries {
            model: model.into(),
            var: var.into(),
            points: pts,
        };
        let mut series = Vec::new();
        for (k, var) in WEATHER_VARS.iter().enumerate() {
            series.push(s("a", var, vec![(0, 10.0 * (k + 1) as f64), (900, 1.0)]));
            // Model b lacks wind at t=900, so only model a counts there.
            let pts = if *var == "wind_speed_10m" {
                vec![(0, 30.0 * (k + 1) as f64)]
            } else {
                vec![(0, 30.0 * (k + 1) as f64), (900, 5.0)]
            };
            series.push(s("b", var, pts));
        }
        series.push(s("a", "cloud_cover", vec![(0, 99.0)])); // ignored
        let w = average_models(series);
        assert_eq!(w.len(), 2);
        let t0 = w[&0];
        assert_eq!(
            (t0.ghi, t0.dni, t0.dhi, t0.temp_air, t0.wind_speed),
            (20.0, 40.0, 60.0, 80.0, 100.0)
        );
        assert_eq!(w[&900].ghi, 1.0);
        assert!(average_models(Vec::new()).is_empty());
    }

    #[test]
    fn weather_for_averages_quarter_hours_within_the_step() {
        let w: HashMap<i64, WeatherSlot> = [ws(0, 100.0), ws(900, 300.0)]
            .into_iter()
            .map(|x| (x.start, x))
            .collect();
        assert_eq!(weather_for(&w, 0, 900).unwrap().len(), 1);
        let hour = weather_for(&w, 0, 3600).unwrap();
        assert_eq!(hour.len(), 2, "missing quarters are skipped");
        assert!(weather_for(&w, 7200, 3600).is_none());
    }

    #[test]
    fn invalid_requests_are_rejected() {
        let base = request(1_781_000_000);
        let check = |mut f: Box<dyn FnMut(&mut PlanRequest)>, want: &str| {
            let mut r = base.clone();
            f(&mut r);
            let err = plan(&r).expect_err(want).to_string();
            assert!(err.contains(want), "{want}: got '{err}'");
        };
        check(Box::new(|r| r.modes.clear()), "no modes");
        check(Box::new(|r| r.step_s = 0), "multiple of 900");
        check(Box::new(|r| r.step_s = 600), "multiple of 900");
        check(Box::new(|r| r.battery.capacity_wh = 0.0), "invalid battery");
        check(
            Box::new(|r| r.timezone = "Nowhere/City".into()),
            "unknown time zone",
        );
        check(Box::new(|r| r.load_history.clear()), "load_history is empty");
        check(Box::new(|r| r.weather = Some(Vec::new())), "no weather");
        check(
            Box::new(|r| {
                r.known_prices.clear();
                r.wholesale_history.clear();
            }),
            "no known prices",
        );
    }

    #[test]
    fn invalid_json_is_a_request_error() {
        assert!(matches!(crate::plan_json("{"), Err(Error::Request(_))));
        assert!(matches!(
            crate::plan_json(r#"{"now": 1}"#),
            Err(Error::Request(_))
        ));
    }

    #[test]
    fn slots_start_on_the_step_boundary() {
        let now = 1_781_000_000 + 7 * 60; // 7 minutes into a quarter hour
        let resp = plan(&request(now)).unwrap();
        assert_eq!(resp.slots[0].start, now - now.rem_euclid(900));
        assert!(resp.slots.windows(2).all(|w| w[1].start - w[0].start == 900));
        assert_eq!(resp.summary.mode_now, resp.slots[0].mode);
        assert_eq!(resp.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn hourly_steps() {
        let mut r = request(1_781_000_000);
        r.step_s = 3600;
        let resp = plan(&r).unwrap();
        assert_eq!(resp.slots.len(), 24);
        assert!(resp.slots.windows(2).all(|w| w[1].start - w[0].start == 3600));
    }

    #[test]
    fn missing_weather_at_the_end_warns_and_assumes_no_pv() {
        let mut r = request(1_781_000_000);
        let start = r.now - r.now.rem_euclid(900);
        let cut = start + 90 * 900; // drop the last 6 of 96 slots
        r.weather.as_mut().unwrap().retain(|w| w.start < cut);
        let resp = plan(&r).unwrap();
        assert!(
            resp.warnings.iter().any(|w| w.contains("no weather for 6 slots")),
            "{:?}",
            resp.warnings
        );
        assert!(resp.slots[90..].iter().all(|s| s.pv_w == 0.0));
    }

    #[test]
    fn unpublished_prices_come_from_wholesale_through_the_tariff() {
        let mut r = request(1_781_000_000);
        r.known_prices.truncate(6); // the first 6 hours are published
        r.wholesale_history = (0..(24 * 30))
            .map(|h| (r.now - 30 * 86_400 + h * 3600, 0.10))
            .collect();
        let resp = plan(&r).unwrap();
        let later = &resp.slots[6 * 4..];
        assert!(later.iter().all(|s| !s.price_known));
        for s in later {
            assert!((s.import_price - (1.21 * 0.10 + 0.13)).abs() < 1e-9, "{s:?}");
            // Without net metering: export = export_a × wholesale + export_b.
            assert!((s.export_price - 1.21 * 0.10).abs() < 1e-9, "{s:?}");
        }
        assert!(resp.slots[..6 * 4].iter().all(|s| s.price_known));
        assert!(resp.warnings.is_empty(), "{:?}", resp.warnings);
    }

    #[test]
    fn without_wholesale_history_unknown_prices_use_the_mean_known_price() {
        let mut r = request(1_781_000_000);
        r.known_prices.truncate(6); // all at 0.10
        let resp = plan(&r).unwrap();
        assert!(
            resp.slots[6 * 4..]
                .iter()
                .all(|s| (s.import_price - 0.10).abs() < 1e-12)
        );
        assert!(
            resp.warnings
                .iter()
                .any(|w| w.contains("used the mean known price")),
            "{:?}",
            resp.warnings
        );
    }

    #[test]
    fn net_metering_pays_export_at_the_import_price() {
        let mut r = request(1_781_000_000);
        r.export.net_metering = true;
        let resp = plan(&r).unwrap();
        assert!(resp.slots.iter().all(|s| s.export_price == s.import_price));
    }

    #[test]
    fn terminal_value_is_the_median_price_after_discharge_losses() {
        let resp = plan(&request(1_781_000_000)).unwrap();
        // 6 hours at 0.10 and 18 at 0.40: the median is 0.40.
        assert!((resp.summary.terminal_value_per_kwh - 0.40 * 0.95).abs() < 1e-12);
    }

    #[test]
    fn short_load_history_falls_back_to_the_mean_with_a_warning() {
        let mut r = request(1_781_000_000);
        r.load_history = vec![(r.now - 3 * 3600, 600.0), (r.now - 2 * 3600, 200.0)];
        let resp = plan(&r).unwrap();
        assert!(
            resp.warnings.iter().any(|w| w.contains("load profile missing")),
            "{:?}",
            resp.warnings
        );
        // Hours with history on an earlier day use it; all other hours use the mean.
        let at = |h: i64| {
            let t = r.now - r.now.rem_euclid(3600) + h * 3600;
            resp.slots.iter().find(|s| s.start == t).unwrap().load_w
        };
        assert_eq!((at(21), at(22)), (600.0, 200.0));
        assert!(resp.slots.iter().filter(|s| s.load_w == 400.0).count() >= 22 * 4);
    }

    #[test]
    fn impossible_load_hours_are_ignored_with_a_warning() {
        let clean = request(1_781_000_000);
        let base = plan(&clean).unwrap();
        assert!(!base.warnings.iter().any(|w| w.contains("physical limit")));
        // A meter glitch: 500 kW for one hour, a week ago. The limit is 17 + 3 + 3 = 23 kW.
        let mut r = clean.clone();
        let glitch = r.load_history.len() - 7 * 24;
        r.load_history[glitch].1 = 500_000.0;
        r.load_history[glitch + 1].1 = f64::NAN;
        let resp = plan(&r).unwrap();
        assert!(
            resp.warnings
                .iter()
                .any(|w| w == "ignored 2 load hours above the physical limit"),
            "{:?}",
            resp.warnings
        );
        assert!(resp.slots.iter().all(|s| s.load_w == 400.0), "{:?}", resp.slots);
        // A heavy but possible hour stays in.
        let mut r = clean.clone();
        r.load_history[glitch].1 = 23_000.0;
        let resp = plan(&r).unwrap();
        assert!(!resp.warnings.iter().any(|w| w.contains("physical limit")));
        assert!(resp.slots.iter().any(|s| s.load_w > 400.0));
        // Nothing plausible left: the same error as an empty history.
        let mut r = clean;
        r.load_history = vec![(r.now - 3600, 1e9)];
        assert!(
            plan(&r)
                .unwrap_err()
                .to_string()
                .contains("load_history is empty")
        );
    }

    /// The JSON the Home Assistant integration sends (`coordinator._request`):
    /// optional fields left out, tuples as arrays, integer numbers.
    #[test]
    fn integration_request_shape_parses_with_defaults() {
        let json = serde_json::json!({
            "now": 1_781_000_000,
            "horizon_h": 36,
            "location": {"lat": 52.0, "lon": 5.0},
            "timezone": "Europe/Amsterdam",
            "country": "nl",
            "battery": {"capacity_wh": 10000, "max_charge_w": 3000, "max_discharge_w": 3000,
                        "charge_eff": 0.95, "discharge_eff": 0.95, "min_soc": 0.1, "max_soc": 1.0,
                        "wear_cost_per_kwh": 0.02},
            "grid": {"max_import_w": 17250, "max_export_w": 17250},
            "modes": [{"name": "charge", "behaviour": {"type": "grid_setpoint", "w": 3000}, "pv_curtail": false},
                      {"name": "charge:pv_off", "behaviour": {"type": "grid_setpoint", "w": 3000}, "pv_curtail": true},
                      {"name": "hold:no_export", "behaviour": {"type": "no_discharge"}, "pv_curtail": false,
                       "no_export": true}],
            "soc_wh": 5000,
            "planes": [{"azimuth": 180, "tilt": 35, "pdc0": 3000, "ac_max": 2800, "label": "extra fields are ignored"}],
            "load_history": [[1_780_996_400, 400.0]],
            "known_prices": [{"start": 1_780_999_200, "end": 1_781_002_800, "import": 0.25}],
            "tariff": {"a": 1.21, "b": 0.13},
            "export": {"net_metering": true, "a": 1.21, "b": 0.0},
            "wholesale_history": [[1_780_996_400, 0.1]]
        });
        let r: PlanRequest = serde_json::from_value(json).unwrap();
        assert_eq!((r.step_s, r.horizon_h), (900, 36));
        assert_eq!(
            r.weather_models,
            vec!["knmi_harmonie_arome_netherlands", "icon_d2"]
        );
        assert!(r.weather.is_none());
        assert_eq!(r.load_history, vec![(1_780_996_400, 400.0)]);
        assert!(r.modes[1].pv_curtail);
        assert!(!r.modes[0].no_export && r.modes[2].no_export);
        assert_eq!(r.planes[0].inverter_eff, 0.965);
    }

    /// The fields the integration reads from the response.
    #[test]
    fn response_json_fields() {
        let resp = plan(&request(1_781_000_000)).unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        let keys = |v: &serde_json::Value| {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(keys(&v), ["slots", "summary", "version", "warnings"]);
        assert_eq!(
            keys(&v["summary"]),
            [
                "cost",
                "load_kwh_24h",
                "mode_now",
                "pv_kwh_24h",
                "terminal_value_per_kwh"
            ]
        );
        assert_eq!(
            keys(&v["slots"][0]),
            [
                "battery_w",
                "export_price",
                "grid_w",
                "import_price",
                "load_w",
                "mode",
                "price_known",
                "pv_w",
                "soc_pct",
                "start"
            ]
        );
        assert!(resp.summary.cost.is_finite());
    }
}
