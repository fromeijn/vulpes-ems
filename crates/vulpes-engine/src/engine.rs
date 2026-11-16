use crate::time::{day_start, local_date, local_midnight};
use crate::{Error, PlanRequest, PlanResponse, PlanSummary, Result, SlotPlan, WeatherSlot};
use chrono::{DateTime, Datelike};
use chrono_tz::Tz;
use std::collections::{HashMap, HashSet};
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

/// Longest planning horizon accepted (hours): weather forecasts do not reach further.
pub(crate) const MAX_HORIZON_H: u32 = 240;

/// Weather per slot start: the mean over all models that have the slot, and
/// the tail model where none of them does.
fn fetch_weather(req: &PlanRequest) -> Result<HashMap<i64, WeatherSlot>> {
    if let Some(w) = &req.weather {
        return Ok(w.iter().map(|s| (s.start, *s)).collect());
    }
    let mut models: Vec<&str> = req.weather_models.iter().map(String::as_str).collect();
    // A tail model that is also a regular model is simply averaged with the rest.
    let tail = req.weather_tail_model.as_deref().filter(|t| !models.contains(t));
    models.extend(tail);
    let days = (req.horizon_h / 24 + 2).min(16) as u8;
    let series = open_meteo::fetch(&open_meteo::Request {
        location: SrcLocation {
            lat: req.location.lat,
            lon: req.location.lon,
        },
        models: &models,
        vars: WEATHER_VARS,
        // One past day: the forecast starts at UTC midnight, after sunrise in the far east,
        // and "PV forecast today" needs the weather since local midnight.
        range: open_meteo::Range::Forecast { days, past_days: 1 },
    })
    .map_err(|e| Error::Weather(e.to_string()))?;
    Ok(with_tail(series, tail))
}

/// Average the regular models, then fill the slots none of them has from the
/// `tail` model. The regular models are finer and more accurate, but end
/// about two days ahead.
pub(crate) fn with_tail(series: Vec<WeatherSeries>, tail: Option<&str>) -> HashMap<i64, WeatherSlot> {
    let (tail_series, regular): (Vec<_>, Vec<_>) =
        series.into_iter().partition(|s| Some(s.model.as_str()) == tail);
    let mut weather = average_models(regular);
    for (t, slot) in average_models(tail_series) {
        weather.entry(t).or_insert(slot);
    }
    weather
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

/// AC power of the planes in the slot starting at `t` (mean W), from the
/// weather of its quarter hours; `None` without weather. Before curtailment.
fn slot_pv(req: &PlanRequest, weather: &HashMap<i64, WeatherSlot>, t: i64, step: i64) -> Option<f64> {
    let ws = weather_for(weather, t, step)?;
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
}

/// As [`slot_pv`], but a slot without weather counts as 0 when the sun is
/// down (the forecast may start at UTC midnight, after local midnight). With
/// the sun up it stays `None`.
fn slot_pv_or_dark(req: &PlanRequest, weather: &HashMap<i64, WeatherSlot>, t: i64, step: i64) -> Option<f64> {
    slot_pv(req, weather, t, step).or_else(|| {
        let sun = sun_position((t + step / 2) as f64, req.location.lat, req.location.lon);
        (sun.elevation() <= 0.0).then_some(0.0)
    })
}

/// PV energy per hour `(hour_start, Wh)`, before curtailment: the hours from
/// local midnight of the day of `start` that end at or before `end`,
/// including the ones that have passed. Each hour is the sum of its quarter
/// hours, whatever the plan's step. An hour with a daylight quarter hour
/// without weather is left out.
fn pv_hourly(
    req: &PlanRequest,
    weather: &HashMap<i64, WeatherSlot>,
    tz: Tz,
    start: i64,
    end: i64,
) -> Vec<(i64, f64)> {
    let Some(mut hour) = day_start(start, tz) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while hour + 3600 <= end {
        let wh: Option<f64> = (0..4)
            .map(|k| slot_pv_or_dark(req, weather, hour + k * 900, 900).map(|w| w * 0.25))
            .sum();
        if let Some(wh) = wh {
            out.push((hour, wh));
        }
        hour += 3600;
    }
    out
}

/// PV energy per local day (kWh): today, the rest of today from the current
/// slot, tomorrow and the day after. A slot without weather counts as 0 when
/// the sun is down (the forecast may start at UTC midnight, after local
/// midnight); with the sun up it makes that day's figure `None`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PvDays {
    pub today: Option<f64>,
    pub today_remaining: Option<f64>,
    pub tomorrow: Option<f64>,
    pub day_after: Option<f64>,
}

fn pv_days(req: &PlanRequest, weather: &HashMap<i64, WeatherSlot>, tz: Tz, start: i64, step: i64) -> PvDays {
    let dt_h = step as f64 / 3600.0;
    let midnight = |d| local_midnight(tz, d);
    let today = local_date(start, tz);
    // Energy of the slots in [from, to); None if a daylight slot has no weather.
    let energy = |from: i64, to: i64| -> Option<f64> {
        let first = from + (step - from.rem_euclid(step)) % step;
        let mut wh = 0.0;
        let mut t = first;
        while t < to {
            wh += slot_pv_or_dark(req, weather, t, step)? * dt_h;
            t += step;
        }
        Some(wh / 1000.0)
    };
    let day = |offset: u64| -> Option<(i64, i64)> {
        let d = today?.checked_add_days(chrono::Days::new(offset))?;
        Some((midnight(d)?, midnight(d.succ_opt()?)?))
    };
    let sum = |offset: u64| day(offset).and_then(|(a, b)| energy(a, b));
    PvDays {
        today: sum(0),
        today_remaining: day(0).and_then(|(_, end)| energy(start, end)),
        tomorrow: sum(1),
        day_after: sum(2),
    }
}

/// The quarter hours of a 15-minute weather series in `[start, start + step)`;
/// missing ones are skipped, `None` when there are none.
fn weather_for(weather: &HashMap<i64, WeatherSlot>, start: i64, step: i64) -> Option<Vec<WeatherSlot>> {
    let v: Vec<WeatherSlot> = (0..(step / 900).max(1))
        .filter_map(|k| weather.get(&(start + k * 900)).copied())
        .collect();
    (!v.is_empty()).then_some(v)
}

/// How far the weather reaches over the PV forecast of the horizon:
/// `(covered, gaps)`. `covered` counts the slots up to the last one with
/// weather; the plan ends there, because planning further would mean guessing
/// the sun. `gaps` counts the slots without weather before that point.
///
/// An error when the weather is too thin to plan on: it must reach three
/// quarters of the first day (`day_slots`), or of the horizon when that is
/// shorter, with at most a quarter of the covered slots missing.
fn weather_cover(pv: &[Option<f64>], day_slots: usize) -> std::result::Result<(usize, usize), String> {
    let n = pv.len();
    let covered = pv.iter().rposition(|p| p.is_some()).map_or(0, |i| i + 1);
    let needed = (n.min(day_slots) * 3).div_ceil(4);
    if covered < needed {
        return Err(format!(
            "weather covers {covered} of {n} slots; at least {needed} are needed"
        ));
    }
    let gaps = pv[..covered].iter().filter(|p| p.is_none()).count();
    if gaps > covered / 4 {
        return Err(format!("no weather for {gaps} of the first {covered} slots"));
    }
    Ok((covered, gaps))
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
    req.grid.validate()?;
    if req.modes.is_empty() {
        return Err(Error::Request("no modes defined".into()));
    }
    let mut names = HashSet::new();
    if let Some(m) = req.modes.iter().find(|m| !names.insert(m.name.as_str())) {
        return Err(Error::Request(format!("duplicate mode name '{}'", m.name)));
    }
    if req.step_s <= 0 || req.step_s % 900 != 0 {
        return Err(Error::Request("step_s must be a positive multiple of 900".into()));
    }
    if req.horizon_h > MAX_HORIZON_H || i64::from(req.horizon_h) * 3600 < req.step_s {
        return Err(Error::Request(format!(
            "horizon_h must cover at least one slot and be at most {MAX_HORIZON_H}"
        )));
    }
    if !(req.tariff.a.is_finite() && req.tariff.b.is_finite()) {
        return Err(Error::Request("tariff: a and b must be numbers".into()));
    }
    if !(req.export.a.is_finite() && req.export.b.is_finite()) {
        return Err(Error::Request("export: a and b must be numbers".into()));
    }
    // A fixed import price (a = 0) says nothing about the wholesale price, so
    // it only goes with an export price that doesn't follow it either.
    if req.tariff.a == 0.0 && req.export.a != 0.0 {
        return Err(Error::Request(
            "tariff: a can only be 0 when export.a is 0 too: the export price follows the wholesale \
             price, which is derived from the import price"
                .into(),
        ));
    }
    if !req.soc_wh.is_finite() {
        return Err(Error::Request("soc_wh must be a number".into()));
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
    let horizon = (i64::from(req.horizon_h) * 3600 / step) as usize;
    let dt_h = step as f64 / 3600.0;

    // PV forecast per slot.
    let weather = fetch_weather(req)?;
    let mut pv: Vec<Option<f64>> = (0..horizon)
        .map(|i| slot_pv(req, &weather, start + i as i64 * step, step))
        .collect();
    let pv_days = pv_days(req, &weather, tz, start, step);
    // Plan as far as the weather reaches: a tail model that is missing must
    // not cost the plan for the days the other models do cover.
    let day_slots = (24 * 3600 / step) as usize;
    let (n, pv_gaps) = weather_cover(&pv, day_slots).map_err(Error::Weather)?;
    pv.truncate(n);
    if n < horizon {
        warnings.push(format!(
            "weather reaches {n} of {horizon} slots ahead; the plan ends there"
        ));
    }
    if pv_gaps > 0 {
        warnings.push(format!("no weather for {pv_gaps} slots; PV assumed 0 there"));
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
    // `validate` rejects `tariff.a == 0` with `export.a != 0`, so this always has a price.
    let export_from = |import: f64| {
        req.export
            .from_import(import, &req.tariff)
            .unwrap_or(req.export.b)
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
    let day_slots = day_slots.min(n);
    let kwh = |f: &dyn Fn(&SlotInput) -> f64| inputs[..day_slots].iter().map(f).sum::<f64>() * dt_h / 1000.0;
    Ok(PlanResponse {
        version: env!("CARGO_PKG_VERSION").into(),
        summary: PlanSummary {
            mode_now: slots.first().map(|s| s.mode.clone()).unwrap_or_default(),
            cost: plan.cost,
            pv_kwh_24h: kwh(&|s| s.pv_w),
            pv_kwh_today: pv_days.today,
            pv_kwh_today_remaining: pv_days.today_remaining,
            pv_kwh_tomorrow: pv_days.tomorrow,
            pv_kwh_day_after: pv_days.day_after,
            load_kwh_24h: kwh(&|s| s.load_w),
            terminal_value_per_kwh: terminal,
        },
        slots,
        pv_hourly: pv_hourly(req, &weather, tz, start, start + n as i64 * step),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExportTariff, KnownPrice, Location};
    use chrono::TimeZone;
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
            weather_tail_model: None,
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
            export: ExportTariff { a: 1.21, b: 0.0 },
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

    /// serde_json writes NaN and infinity as null: no nulls in a plan, except
    /// the day figures that are null by design when the weather doesn't cover a day.
    fn assert_no_nan(out: &str) {
        let mut v: serde_json::Value = serde_json::from_str(out).unwrap();
        for k in [
            "pv_kwh_today",
            "pv_kwh_today_remaining",
            "pv_kwh_tomorrow",
            "pv_kwh_day_after",
        ] {
            v["summary"].as_object_mut().unwrap().remove(k);
        }
        assert!(!v.to_string().contains("null"), "{out}");
    }

    /// Clear-sky-ish weather every quarter hour in `[from, to)`.
    fn clear_sky(from: i64, to: i64) -> HashMap<i64, WeatherSlot> {
        let mut out = HashMap::new();
        let mut t = from - from.rem_euclid(900);
        while t < to {
            let s = sun_position((t + 450) as f64, 52.0, 5.0);
            let cz = s.zenith.to_radians().cos().max(0.0);
            let dni = if cz > 0.0 {
                800.0 * (-0.15 / cz.max(0.05)).exp()
            } else {
                0.0
            };
            out.insert(
                t,
                WeatherSlot {
                    start: t,
                    ghi: dni * cz + 80.0 * cz,
                    dni,
                    dhi: 80.0 * cz,
                    temp_air: 12.0,
                    wind_speed: 3.0,
                },
            );
            t += 900;
        }
        out
    }

    const AMS: Tz = chrono_tz::Europe::Amsterdam;

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
        AMS.with_ymd_and_hms(y, m, d, h, min, 0)
            .earliest()
            .unwrap()
            .timestamp()
    }

    /// Energy of the slots in `[a, b)` summed by hand from `slot_pv`.
    fn by_hand(req: &PlanRequest, w: &HashMap<i64, WeatherSlot>, a: i64, b: i64) -> f64 {
        (0..)
            .map(|k| a + k * 900)
            .take_while(|t| *t < b)
            .map(|t| slot_pv(req, w, t, 900).unwrap_or(0.0) * 0.25)
            .sum::<f64>()
            / 1000.0
    }

    #[test]
    fn pv_days_sum_the_local_days() {
        let now = local(2026, 6, 10, 13, 7);
        let req = request(now);
        let start = now - now.rem_euclid(900);
        let w = clear_sky(local(2026, 6, 9, 0, 0), local(2026, 6, 14, 0, 0));
        let d = pv_days(&req, &w, AMS, start, 900);
        let close = |a: Option<f64>, b: f64| (a.unwrap() - b).abs() < 1e-9;
        assert!(close(
            d.today,
            by_hand(&req, &w, local(2026, 6, 10, 0, 0), local(2026, 6, 11, 0, 0))
        ));
        assert!(close(
            d.today_remaining,
            by_hand(&req, &w, start, local(2026, 6, 11, 0, 0))
        ));
        assert!(close(
            d.tomorrow,
            by_hand(&req, &w, local(2026, 6, 11, 0, 0), local(2026, 6, 12, 0, 0))
        ));
        assert!(close(
            d.day_after,
            by_hand(&req, &w, local(2026, 6, 12, 0, 0), local(2026, 6, 13, 0, 0))
        ));
        // Today is what has passed plus what remains, and a clear June day yields something.
        let past = by_hand(&req, &w, local(2026, 6, 10, 0, 0), start);
        assert!(close(d.today, past + d.today_remaining.unwrap()));
        assert!(d.today.unwrap() > 5.0 && d.today_remaining.unwrap() > 0.0);
    }

    #[test]
    fn pv_days_around_local_midnight() {
        let w = clear_sky(local(2026, 6, 9, 0, 0), local(2026, 6, 14, 0, 0));
        let before = local(2026, 6, 10, 23, 50);
        let b = pv_days(&request(before), &w, AMS, before - before.rem_euclid(900), 900);
        assert_eq!(b.today_remaining, Some(0.0), "the sun is down");
        let after = local(2026, 6, 11, 0, 5);
        let a = pv_days(&request(after), &w, AMS, after - after.rem_euclid(900), 900);
        assert_eq!(a.today, b.tomorrow, "after midnight, tomorrow became today");
        assert_eq!(
            a.today, a.today_remaining,
            "nothing of the new day has passed in the dark"
        );
    }

    #[test]
    fn pv_days_on_dst_days_have_23_and_25_hours() {
        for (y, m, d, slots) in [(2026, 3, 29, 92), (2026, 10, 25, 100)] {
            let (a, b) = (local(y, m, d, 0, 0), local(y, m, d + 1, 0, 0));
            assert_eq!((b - a) / 900, slots);
            let now = a + 600;
            let req = request(now);
            let w = clear_sky(a - 86_400, b + 3 * 86_400);
            let days = pv_days(&req, &w, AMS, now - now.rem_euclid(900), 900);
            assert!(
                (days.today.unwrap() - by_hand(&req, &w, a, b)).abs() < 1e-9,
                "{y}-{m}-{d}"
            );
        }
    }

    #[test]
    fn pv_days_east_of_utc_need_weather_from_before_utc_midnight() {
        // Tokyo, 13:00 local on 10 June = 04:00 UTC. The sun rose around 19:30 UTC the day before.
        let tz = chrono_tz::Asia::Tokyo;
        let now = tz.with_ymd_and_hms(2026, 6, 10, 13, 0, 0).unwrap().timestamp();
        let mut req = request(now);
        req.location = Location {
            lat: 35.7,
            lon: 139.7,
        };
        let utc_midnight = now - now.rem_euclid(86_400);
        // A forecast from UTC midnight misses the morning: today is unknown, the rest is not.
        let d = pv_days(
            &req,
            &clear_sky(utc_midnight, utc_midnight + 4 * 86_400),
            tz,
            now,
            900,
        );
        assert_eq!(d.today, None);
        assert!(d.today_remaining.is_some() && d.tomorrow.is_some());
        // With one past day, as the engine requests it, the whole local day is covered.
        let w = clear_sky(utc_midnight - 86_400, utc_midnight + 4 * 86_400);
        let d = pv_days(&req, &w, tz, now, 900);
        assert!(d.today.unwrap() >= d.today_remaining.unwrap());
        assert!(d.tomorrow.is_some() && d.day_after.is_some());
    }

    #[test]
    fn pv_days_without_weather() {
        let now = local(2026, 6, 10, 13, 7);
        let req = request(now);
        let start = now - now.rem_euclid(900);
        // Weather from UTC midnight: the local hour before it is dark, so today still counts.
        let utc_midnight = DateTime::from_timestamp(start, 0)
            .unwrap()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        let w = clear_sky(utc_midnight, local(2026, 6, 12, 15, 0));
        let d = pv_days(&req, &w, AMS, start, 900);
        assert!(d.today.is_some() && d.tomorrow.is_some());
        // The weather ends in the afternoon of the day after: that day is unknown.
        assert_eq!(d.day_after, None);
        // No weather at all: nothing is known, but nothing panics either.
        let none = pv_days(&req, &HashMap::new(), AMS, start, 900);
        assert_eq!(
            (none.today, none.today_remaining, none.tomorrow, none.day_after),
            (None, None, None, None)
        );
    }

    /// A request at `now` with clear-sky weather over `[from, to)`.
    fn request_with_weather(now: i64, from: i64, to: i64) -> PlanRequest {
        let mut r = request(now);
        r.weather = Some(clear_sky(from, to).into_values().collect());
        r
    }

    #[test]
    fn pv_hourly_covers_today_from_local_midnight_to_the_end_of_the_plan() {
        // 13:07: the current hour is under way and still one complete entry.
        let now = local(2026, 6, 10, 13, 7);
        let (from, to) = (local(2026, 6, 9, 0, 0), local(2026, 6, 14, 0, 0));
        let r = request_with_weather(now, from, to);
        let w = clear_sky(from, to);
        let resp = plan(&r).unwrap();
        let h = &resp.pv_hourly;
        let midnight = local(2026, 6, 10, 0, 0);
        assert_eq!(h[0].0, midnight);
        assert!(h.windows(2).all(|p| p[1].0 - p[0].0 == 3600), "contiguous hours");
        assert!(h.iter().all(|&(_, wh)| wh.is_finite() && wh >= 0.0));
        // The plan ends at 13:00 the next day, and so do the hours.
        let last_slot_end = resp.slots.last().unwrap().start + 900;
        assert_eq!(last_slot_end, local(2026, 6, 11, 13, 0));
        assert_eq!(h.last().unwrap().0 + 3600, last_slot_end);
        // Today's hours add up to the day figure.
        let today: f64 = h.iter().take(24).map(|x| x.1).sum();
        assert!((today - resp.summary.pv_kwh_today.unwrap() * 1000.0).abs() < 1e-6);
        // Each hour is the sum of its quarter hours: past, current and planned.
        for &(t, wh) in h {
            assert!((wh - by_hand(&r, &w, t, t + 3600) * 1000.0).abs() < 1e-9);
        }
        // A planned hour is the mean power of its four slots.
        let at = |t: i64| h.iter().find(|x| x.0 == t).unwrap().1;
        let slots = &resp.slots[4..8];
        assert_eq!(slots[0].start, local(2026, 6, 10, 14, 0));
        let mean = slots.iter().map(|s| s.pv_w).sum::<f64>() / 4.0;
        assert!((at(slots[0].start) - mean).abs() < 1e-9 && mean > 100.0);
        assert_eq!(at(midnight), 0.0, "dark");
    }

    #[test]
    fn pv_hourly_on_dst_days_has_23_and_25_hours() {
        for (y, m, d, hours) in [(2026, 3, 29, 23), (2026, 10, 25, 25)] {
            let (a, b) = (local(y, m, d, 0, 0), local(y, m, d + 1, 0, 0));
            let now = a + 600;
            let req = request(now);
            let w = clear_sky(a - 86_400, b + 86_400);
            let h = pv_hourly(&req, &w, AMS, now - now.rem_euclid(900), b);
            assert_eq!(h.len(), hours, "{y}-{m}-{d}");
            assert_eq!((h[0].0, h.last().unwrap().0 + 3600), (a, b));
            let sum: f64 = h.iter().map(|x| x.1).sum();
            assert!((sum - by_hand(&req, &w, a, b) * 1000.0).abs() < 1e-6);
        }
    }

    #[test]
    fn pv_hourly_follows_local_hours_in_a_half_hour_time_zone() {
        let tz = chrono_tz::Asia::Kolkata;
        let midnight = tz.with_ymd_and_hms(2026, 6, 10, 0, 0, 0).unwrap().timestamp();
        let now = midnight + 13 * 3600 + 420;
        let req = request(now);
        let w = clear_sky(midnight - 86_400, midnight + 3 * 86_400);
        let h = pv_hourly(&req, &w, tz, now - now.rem_euclid(900), midnight + 86_400);
        assert_eq!(h.len(), 24);
        assert_eq!(h[0].0, midnight);
        assert!(h.iter().all(|x| x.0.rem_euclid(3600) == 1800), "UTC half hours");
    }

    #[test]
    fn pv_hourly_leaves_out_daylight_hours_without_weather() {
        let now = local(2026, 6, 10, 13, 7);
        let req = request(now);
        let start = now - now.rem_euclid(900);
        let (midnight, end) = (local(2026, 6, 10, 0, 0), local(2026, 6, 11, 0, 0));
        // Weather from UTC midnight (02:00 local): the dark hours before it count as 0.
        let mut w = clear_sky(midnight + 2 * 3600, end);
        // One quarter hour at noon is missing: that hour has no value.
        w.remove(&local(2026, 6, 10, 12, 15));
        let h = pv_hourly(&req, &w, AMS, start, end);
        assert_eq!(h.len(), 23);
        assert_eq!(h[..2], [(midnight, 0.0), (midnight + 3600, 0.0)]);
        assert!(h.iter().all(|x| x.0 != local(2026, 6, 10, 12, 0)));
        assert!(h.iter().any(|x| x.0 == local(2026, 6, 10, 11, 0) && x.1 > 100.0));
        // No weather at all: only the dark hours are known, as 0.
        let none = pv_hourly(&req, &HashMap::new(), AMS, start, end);
        assert!(!none.is_empty() && none.len() < 12);
        assert!(none.iter().all(|x| x.1 == 0.0));
        // Nothing before the end of the first hour, and no panic on an odd range.
        assert!(pv_hourly(&req, &w, AMS, start, midnight + 3599).is_empty());
        assert!(pv_hourly(&req, &w, AMS, start, midnight - 86_400).is_empty());
    }

    #[test]
    fn pv_hourly_with_an_hourly_step_still_sums_quarter_hours() {
        let now = local(2026, 6, 10, 13, 7);
        let (from, to) = (local(2026, 6, 9, 0, 0), local(2026, 6, 14, 0, 0));
        let mut r = request_with_weather(now, from, to);
        let quarter = plan(&r).unwrap().pv_hourly;
        r.step_s = 3600;
        let hourly = plan(&r).unwrap().pv_hourly;
        assert_eq!(hourly, quarter);
    }

    #[test]
    fn plan_summary_has_the_day_figures() {
        let now = 1_781_000_000;
        let resp = plan(&request(now)).unwrap();
        // The test weather starts now (a June morning) and covers two days: the rest
        // of today and tomorrow are known; the whole of today and the day after are not.
        let s = &resp.summary;
        assert!(s.pv_kwh_today_remaining.unwrap() > 0.0 && s.pv_kwh_tomorrow.unwrap() > 0.0);
        assert_eq!((s.pv_kwh_today, s.pv_kwh_day_after), (None, None));
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
    fn tail_model_only_fills_slots_the_regular_models_lack() {
        let model = |name: &str, pts: Vec<(i64, f64)>| -> Vec<WeatherSeries> {
            WEATHER_VARS
                .iter()
                .map(|var| WeatherSeries {
                    model: name.into(),
                    var: (*var).into(),
                    points: pts.clone(),
                })
                .collect()
        };
        let series = || {
            let mut s = model("near", vec![(0, 10.0), (900, 20.0)]);
            s.extend(model("far", vec![(0, 100.0), (900, 200.0), (1800, 300.0)]));
            s
        };
        // The tail is not averaged in where a regular model has the slot.
        let w = with_tail(series(), Some("far"));
        assert_eq!((w[&0].ghi, w[&900].ghi, w[&1800].ghi), (10.0, 20.0, 300.0));
        assert_eq!(w[&1800].start, 1800);
        // Without a tail, every model is a regular one.
        let w = with_tail(series(), None);
        assert_eq!((w[&0].ghi, w[&900].ghi, w[&1800].ghi), (55.0, 110.0, 300.0));
        // A tail that returned nothing leaves the regular models as they are.
        let w = with_tail(model("near", vec![(0, 10.0)]), Some("far"));
        assert_eq!((w.len(), w[&0].ghi), (1, 10.0));
        // Only the tail answered: it covers everything.
        let w = with_tail(model("far", vec![(0, 100.0)]), Some("far"));
        assert_eq!((w.len(), w[&0].ghi), (1, 100.0));
        assert!(with_tail(Vec::new(), Some("far")).is_empty());
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
        check(Box::new(|r| r.grid.max_export_w = -1.0), "invalid grid limits");
        check(
            Box::new(|r| r.grid.max_import_w = f64::NAN),
            "invalid grid limits",
        );
        check(
            Box::new(|r| r.modes[1].name = r.modes[0].name.clone()),
            "duplicate mode name 'Charging'",
        );
        // No slot at all used to panic on the terminal value.
        check(Box::new(|r| r.horizon_h = 0), "horizon_h");
        check(
            Box::new(|r| (r.step_s, r.horizon_h) = (7200, 1)),
            "horizon_h must cover at least one slot",
        );
        check(Box::new(|r| r.horizon_h = MAX_HORIZON_H + 1), "at most 240");
        // a = 0 with an export price that follows wholesale used to divide by zero.
        check(Box::new(|r| r.tariff.a = 0.0), "tariff: a can only be 0");
        check(Box::new(|r| r.tariff.a = f64::NAN), "tariff: a and b");
        check(Box::new(|r| r.tariff.b = f64::NAN), "tariff: a and b");
        check(Box::new(|r| r.export.a = f64::INFINITY), "export: a and b");
        check(Box::new(|r| r.soc_wh = f64::NAN), "soc_wh");
        check(
            Box::new(|r| r.timezone = "Nowhere/City".into()),
            "unknown time zone",
        );
        check(Box::new(|r| r.load_history.clear()), "load_history is empty");
        check(
            Box::new(|r| r.weather = Some(Vec::new())),
            "weather covers 0 of 96 slots",
        );
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
    fn unknown_request_keys_are_rejected() {
        let mut json = serde_json::to_value(request(1_781_000_000)).unwrap();
        // A misspelled `horizon_h` would otherwise silently plan the default horizon.
        json["horizon"] = 24.into();
        let err = crate::plan_json(&json.to_string()).unwrap_err().to_string();
        assert!(err.contains("unknown field `horizon`"), "{err}");
        // Also inside the engine's own nested objects. The former net metering
        // switch would otherwise be dropped and export priced differently.
        let nested = |path: &[&str], key: &str| {
            let mut json = serde_json::to_value(request(1_781_000_000)).unwrap();
            let mut at = &mut json;
            for p in path {
                at = if let Ok(i) = p.parse::<usize>() {
                    &mut at[i]
                } else {
                    &mut at[*p]
                };
            }
            at[key] = true.into();
            let err = crate::plan_json(&json.to_string()).unwrap_err();
            assert!(matches!(err, Error::Request(_)), "{err}");
            assert!(
                err.to_string().contains(&format!("unknown field `{key}`")),
                "{err}"
            );
        };
        nested(&["export"], "net_metering");
        nested(&["location"], "altitude");
        nested(&["known_prices", "0"], "export");
        nested(&["weather", "0"], "cloud_cover");
    }

    #[test]
    fn the_longest_horizon_and_a_single_slot_are_accepted() {
        let mut r = request(1_781_000_000);
        r.horizon_h = MAX_HORIZON_H;
        // The fixture has two days of weather: repeat its first day over the whole horizon.
        let day: Vec<WeatherSlot> = r.weather.as_ref().unwrap()[..96].to_vec();
        r.weather = Some(
            (0..i64::from(MAX_HORIZON_H) / 24)
                .flat_map(|d| {
                    day.iter().map(move |w| WeatherSlot {
                        start: w.start + d * 86_400,
                        ..*w
                    })
                })
                .collect(),
        );
        let resp = plan(&r).unwrap();
        assert_eq!(resp.slots.len(), 4 * MAX_HORIZON_H as usize);
        (r.step_s, r.horizon_h) = (3600, 1);
        assert_eq!(plan(&r).unwrap().slots.len(), 1);
    }

    #[test]
    fn negative_prices_and_soc_at_the_limits_give_finite_plans() {
        let now = 1_781_000_000;
        for soc_wh in [0.0, 1000.0, 10_000.0, 20_000.0] {
            let mut r = request(now);
            r.soc_wh = soc_wh;
            r.grid.max_export_w = 0.0;
            for (h, p) in r.known_prices.iter_mut().enumerate() {
                p.import = if h % 2 == 0 { -0.20 } else { 0.0 };
            }
            let out = crate::plan_json(&serde_json::to_string(&r).unwrap()).unwrap();
            assert_no_nan(&out);
            let resp: PlanResponse = serde_json::from_str(&out).unwrap();
            assert_eq!(resp.slots.len(), 96);
            assert!(resp.slots.iter().all(|s| s.grid_w >= -1e-9), "export limit 0");
            assert!(resp.slots.iter().all(|s| (10.0..=100.0).contains(&s.soc_pct)));
        }
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
    fn weather_cover_ends_at_the_last_slot_with_weather() {
        let pv = |have: &[bool]| -> Vec<Option<f64>> { have.iter().map(|&h| h.then_some(1.0)).collect() };
        let run = |with: usize, without: usize| pv(&[vec![true; with], vec![false; without]].concat());
        // Fully covered, and a tail without weather: three and a half days, two covered.
        assert_eq!(weather_cover(&run(336, 0), 96), Ok((336, 0)));
        assert_eq!(weather_cover(&run(192, 144), 96), Ok((192, 0)));
        // Three quarters of the first day is the least to plan on.
        assert_eq!(weather_cover(&run(72, 264), 96), Ok((72, 0)));
        let err = weather_cover(&run(71, 265), 96).unwrap_err();
        assert_eq!(err, "weather covers 71 of 336 slots; at least 72 are needed");
        // A horizon shorter than a day needs three quarters of itself.
        assert_eq!(weather_cover(&run(3, 1), 96), Ok((3, 0)));
        assert!(weather_cover(&run(2, 2), 96).is_err());
        assert_eq!(weather_cover(&run(1, 0), 96), Ok((1, 0)));
        // Gaps before the end are counted, up to a quarter of the covered slots.
        let mut gaps = run(96, 0);
        gaps[..24].fill(None);
        assert_eq!(weather_cover(&gaps, 96), Ok((96, 24)));
        gaps[24] = None;
        let err = weather_cover(&gaps, 96).unwrap_err();
        assert_eq!(err, "no weather for 25 of the first 96 slots");
        // No weather at all, and no slots at all.
        assert!(weather_cover(&run(0, 96), 96).is_err());
        assert_eq!(weather_cover(&[], 96), Ok((0, 0)));
    }

    #[test]
    fn the_plan_ends_where_the_weather_ends() {
        let mut r = request(1_781_000_000);
        let start = r.now - r.now.rem_euclid(900);
        let cut = start + 90 * 900; // drop the last 6 of 96 slots
        r.weather.as_mut().unwrap().retain(|w| w.start < cut);
        let resp = plan(&r).unwrap();
        assert_eq!(resp.slots.len(), 90);
        assert!(
            resp.warnings
                .iter()
                .any(|w| w.contains("weather reaches 90 of 96 slots")),
            "{:?}",
            resp.warnings
        );
    }

    #[test]
    fn a_missing_tail_model_shortens_the_plan_instead_of_failing_it() {
        // The default horizon with two days of weather, as when the tail model
        // returns nothing: this used to fail on "no weather for 144 of 336 slots".
        let mut r = request(1_781_000_000);
        r.horizon_h = 84;
        let resp = plan(&r).unwrap();
        assert_eq!(resp.slots.len(), 192);
        assert!(
            resp.warnings
                .iter()
                .any(|w| w.contains("weather reaches 192 of 336 slots")),
            "{:?}",
            resp.warnings
        );
        assert!(resp.summary.cost.is_finite() && resp.summary.pv_kwh_24h > 5.0);
        // With the full horizon covered there is no such warning.
        r.horizon_h = 48;
        let full = plan(&r).unwrap();
        assert_eq!(full.slots.len(), 192);
        assert!(
            full.warnings.iter().all(|w| !w.contains("weather")),
            "{:?}",
            full.warnings
        );
    }

    #[test]
    fn gaps_in_the_weather_warn_and_assume_no_pv() {
        let mut r = request(1_781_000_000);
        let start = r.now - r.now.rem_euclid(900);
        let gap = start + 40 * 900..start + 46 * 900;
        r.weather.as_mut().unwrap().retain(|w| !gap.contains(&w.start));
        let resp = plan(&r).unwrap();
        assert_eq!(resp.slots.len(), 96);
        assert!(
            resp.warnings.iter().any(|w| w.contains("no weather for 6 slots")),
            "{:?}",
            resp.warnings
        );
        assert!(resp.slots[40..46].iter().all(|s| s.pv_w == 0.0));
    }

    #[test]
    fn a_fixed_import_price_plans_with_a_fixed_export_price() {
        // Import a = 0: the price is b whatever the market does. Export a = 0
        // too, as after migrating a fixed-price setup that had net metering.
        let mut r = request(1_781_000_000);
        r.known_prices.truncate(6);
        r.wholesale_history = (0..(24 * 30))
            .map(|h| (r.now - 30 * 86_400 + h * 3600, 0.10))
            .collect();
        r.tariff = LinearTariff { a: 0.0, b: 0.30 };
        r.export = ExportTariff { a: 0.0, b: 0.30 };
        let out = crate::plan_json(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_no_nan(&out);
        let resp: PlanResponse = serde_json::from_str(&out).unwrap();
        assert!(resp.slots.iter().any(|s| s.price_known) && resp.slots.iter().any(|s| !s.price_known));
        for s in &resp.slots {
            assert_eq!(s.export_price, 0.30, "{s:?}");
            if !s.price_known {
                assert_eq!(s.import_price, 0.30, "{s:?}");
            }
        }
        // An export price that follows the market cannot be derived from it.
        r.export.a = 1.0;
        let err = plan(&r).unwrap_err().to_string();
        assert!(err.contains("tariff: a can only be 0"), "{err}");
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
            // Export = export_a × wholesale + export_b.
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
    fn export_tariff_equal_to_import_pays_the_import_price() {
        // Net metering: export a/b are the import a/b. That holds for published
        // prices (first 6 hours) and for forecast ones alike.
        let mut r = request(1_781_000_000);
        r.known_prices.truncate(6);
        r.wholesale_history = (0..(24 * 30))
            .map(|h| (r.now - 30 * 86_400 + h * 3600, 0.10))
            .collect();
        r.export = ExportTariff {
            a: r.tariff.a,
            b: r.tariff.b,
        };
        let resp = plan(&r).unwrap();
        assert!(resp.slots.iter().any(|s| s.price_known) && resp.slots.iter().any(|s| !s.price_known));
        for s in &resp.slots {
            assert!((s.export_price - s.import_price).abs() < 1e-12, "{s:?}");
        }
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
            "export": {"a": 1.0, "b": -0.02},
            "wholesale_history": [[1_780_996_400, 0.1]]
        });
        let r: PlanRequest = serde_json::from_value(json).unwrap();
        assert_eq!((r.step_s, r.horizon_h), (900, 36));
        assert_eq!(
            r.weather_models,
            vec!["knmi_harmonie_arome_netherlands", "icon_d2"]
        );
        assert_eq!(r.weather_tail_model.as_deref(), Some("ecmwf_ifs025"));
        assert!(r.weather.is_none());
        assert_eq!(r.load_history, vec![(1_780_996_400, 400.0)]);
        assert!(r.modes[1].pv_curtail);
        assert!(!r.modes[0].no_export && r.modes[2].no_export);
        assert_eq!((r.export.a, r.export.b), (1.0, -0.02));
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
        assert_eq!(keys(&v), ["pv_hourly", "slots", "summary", "version", "warnings"]);
        // Hours as `[hour_start, Wh]` pairs; the round trip keeps them, and a
        // response from before the field parses with none.
        let first = &v["pv_hourly"][0];
        assert!(first[0].is_i64() && first[1].is_f64(), "{first}");
        let back: PlanResponse = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(back, resp);
        let mut old = v.clone();
        old.as_object_mut().unwrap().remove("pv_hourly");
        let old: PlanResponse = serde_json::from_value(old).unwrap();
        assert!(old.pv_hourly.is_empty());
        assert_eq!(
            keys(&v["summary"]),
            [
                "cost",
                "load_kwh_24h",
                "mode_now",
                "pv_kwh_24h",
                "pv_kwh_day_after",
                "pv_kwh_today",
                "pv_kwh_today_remaining",
                "pv_kwh_tomorrow",
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
