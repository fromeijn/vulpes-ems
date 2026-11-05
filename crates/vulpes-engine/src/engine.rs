use crate::{Error, PlanRequest, PlanResponse, PlanSummary, Result, SlotPlan, WeatherSlot};
use chrono::{DateTime, Datelike};
use chrono_tz::Tz;
use std::collections::HashMap;
use vulpes_load::{Country, ProfileForecaster, ProfileOptions};
use vulpes_opt::{Problem, SlotInput, optimize};
use vulpes_solar::model::{Weather, plane_ac_power};
use vulpes_solar::sun_position;
use vulpes_sources::{Location as SrcLocation, open_meteo};

const WEATHER_VARS: &[&str] = &[
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
    // (start) -> per model: [ghi, dni, dhi, temp, wind]
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
    Ok(acc
        .into_iter()
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
        .collect())
}

/// Mean of a 15-minute weather series over `[start, start + step)`.
fn weather_for(weather: &HashMap<i64, WeatherSlot>, start: i64, step: i64) -> Option<Vec<WeatherSlot>> {
    let v: Vec<WeatherSlot> = (0..(step / 900).max(1))
        .filter_map(|k| weather.get(&(start + k * 900)).copied())
        .collect();
    (!v.is_empty()).then_some(v)
}

pub fn plan(req: &PlanRequest) -> Result<PlanResponse> {
    req.battery.validate()?;
    if req.modes.is_empty() {
        return Err(Error::Request("no modes defined".into()));
    }
    if req.step_s <= 0 || req.step_s % 900 != 0 {
        return Err(Error::Request("step_s must be a positive multiple of 900".into()));
    }
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
                total += req
                    .planes
                    .iter()
                    .map(|p| plane_ac_power(p, sun, wx, doy, 1.0))
                    .sum::<f64>();
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
    if req.load_history.is_empty() {
        return Err(Error::Request("load_history is empty".into()));
    }
    let load_fc = ProfileForecaster::new(&req.load_history, tz, country, ProfileOptions::default());
    let mean_load = req.load_history.iter().map(|x| x.1).sum::<f64>() / req.load_history.len() as f64;
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
            planes: vec![PlaneParams {
                azimuth: 180.0,
                tilt: 35.0,
                pdc0: 3000.0,
                ac_max: 2800.0,
                inverter_eff: 0.965,
                gamma: -0.004,
                iam_b0: 0.05,
                albedo: 0.2,
                u0: 25.0,
                u1: 6.84,
            }],
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
        // Cheap first hours, expensive later: it should charge early.
        assert!(
            resp.slots[..24].iter().any(|s| s.mode == "Charging"),
            "{:?}",
            &resp.slots[..24]
        );
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
}
