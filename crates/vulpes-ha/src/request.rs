//! From Home Assistant data to an engine call and back: the plan every 15
//! minutes, solar calibration, and when to refresh wholesale prices.

use crate::cfg::{Cfg, as_f64, num, obj, req_num, ts_pairs};
use crate::inputs::{
    attribute_pairs, energy_rows, hourly_series, load_groups, load_series, parse_price_attributes, soc_value,
    total_power,
};
use crate::modes::{engine_modes, split_mode};
use crate::settings::CONF_SOC_ENTITY;
use crate::solar_form::{CONF_PLANES, CONF_POWER_ENTITY, engine_planes};
use crate::time::zone;
use crate::{Error, Result};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Hours of load history the load forecast learns from.
pub const LOAD_HISTORY_DAYS: i64 = 60;
/// Days of wholesale price history for forecasting unpublished prices.
pub const WHOLESALE_HISTORY_DAYS: i64 = 35;
/// Planning horizon.
pub const HORIZON_HOURS: u32 = 84;
/// Days of PV history a calibration run uses.
pub const CALIBRATION_HISTORY_DAYS: i64 = 120;
/// Wholesale prices are fetched again after this long.
pub const WHOLESALE_REFRESH_S: f64 = 6.0 * 3600.0;

/// Start of the energy statistics the plan and the source check read:
/// [`LOAD_HISTORY_DAYS`] before `now`, on the whole UTC hour.
pub fn stats_start(now: f64) -> i64 {
    let t = now as i64 - LOAD_HISTORY_DAYS * 86_400;
    t - t.rem_euclid(3600)
}

/// Plan with the Home Assistant inputs. `req` holds `cfg` (the settings),
/// `now` (Unix seconds), `lat`, `lon`, `timezone`, `country`, the SOC state
/// `soc_state`, the price entity's attributes as ordered `price_attributes`
/// pairs with `price_unit`, hourly energy `stats` per statistic, and the
/// `wholesale` history. An optional `weather` list replaces the forecast
/// fetch (tests, offline use).
///
/// Returns the engine's plan with each slot's mode split into the Vulpes
/// mode and its `pv_off` / `no_export` flags, the same for `summary.mode_now`,
/// plus `inputs` (counts of what went in) and `planned_at`.
pub fn plan_ha(req: &Value) -> Result<Value> {
    let cfg = obj(req, "cfg")?;
    let now = req_num(req, "now")?;
    let tz_name = req.get("timezone").and_then(Value::as_str).unwrap_or("UTC");
    let tz = zone(tz_name);
    let soc_entity = cfg.get(CONF_SOC_ENTITY).and_then(Value::as_str).unwrap_or("");
    let soc_pct = soc_value(req.get("soc_state").and_then(Value::as_str))
        .ok_or_else(|| Error::Message(format!("battery SOC from {soc_entity} is unavailable")))?;
    let known = attribute_pairs(req, "price_attributes")
        .map(|a| parse_price_attributes(&a, req.get("price_unit").and_then(Value::as_str), tz))
        .unwrap_or_default();
    let groups = load_groups(cfg);
    if groups.plus.is_empty() && groups.minus.is_empty() {
        return Err(Error::Message("no energy statistics selected".into()));
    }
    let load = load_series(&groups, &energy_rows(req, "stats")?);
    let wholesale = ts_pairs(req, "wholesale");
    let mut request = engine_request(cfg, now, soc_pct, &load, &known, &wholesale, req)?;
    if let Some(w) = req.get("weather").filter(|w| !w.is_null()) {
        request["weather"] = w.clone();
    }
    let raw = vulpes_engine::plan_json(&request.to_string())
        .map_err(|e| Error::Message(format!("planning failed: {e}")))?;
    let mut plan: Value = serde_json::from_str(&raw)?;
    split_plan_modes(&mut plan);
    plan["inputs"] = json!({"load_hours": load.len(), "known_price_slots": known.len(), "wholesale_hours": wholesale.len()});
    plan["planned_at"] = json!(now as i64);
    Ok(plan)
}

/// The engine `PlanRequest` JSON from settings and inputs.
fn engine_request(
    cfg: &Cfg,
    now: f64,
    soc_pct: f64,
    load: &[(i64, f64)],
    known: &[crate::inputs::KnownPrice],
    wholesale: &[(i64, f64)],
    req: &Value,
) -> Result<Value> {
    let cap_wh = num(cfg, "capacity_kwh")? * 1000.0;
    let planes = cfg
        .get(CONF_PLANES)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(json!({
        "now": now as i64,
        "horizon_h": HORIZON_HOURS,
        // Rounded to ~1 km in the core before any external request.
        "location": {"lat": req_num(req, "lat")?, "lon": req_num(req, "lon")?},
        "timezone": req.get("timezone").and_then(Value::as_str).unwrap_or("UTC"),
        "country": req.get("country").and_then(Value::as_str).unwrap_or("").to_lowercase(),
        "battery": battery_json(cfg)?,
        "grid": grid_json(cfg)?,
        "modes": engine_modes(cfg)?,
        "soc_wh": soc_pct / 100.0 * cap_wh,
        "planes": engine_planes(&planes),
        "load_history": load,
        "known_prices": known,
        "tariff": {"a": num(cfg, "tariff_a")?, "b": num(cfg, "tariff_b")?},
        "export": {"a": num(cfg, "export_a")?, "b": num(cfg, "export_b")?},
        "wholesale_history": wholesale,
    }))
}

/// The engine's `BatteryParams` from the settings (kWh → Wh, % → fraction).
pub fn battery_json(cfg: &Cfg) -> Result<Value> {
    Ok(json!({
        "capacity_wh": num(cfg, "capacity_kwh")? * 1000.0,
        "max_charge_w": num(cfg, "max_charge_w")?,
        "max_discharge_w": num(cfg, "max_discharge_w")?,
        "charge_eff": num(cfg, "charge_eff")?,
        "discharge_eff": num(cfg, "discharge_eff")?,
        "min_soc": num(cfg, "min_soc")? / 100.0,
        "max_soc": num(cfg, "max_soc")? / 100.0,
        "wear_cost_per_kwh": num(cfg, "wear_cost")?,
    }))
}

/// The engine's `GridLimits` from the settings.
pub fn grid_json(cfg: &Cfg) -> Result<Value> {
    Ok(json!({"max_import_w": num(cfg, "max_import_w")?, "max_export_w": num(cfg, "max_export_w")?}))
}

/// Split each engine mode name into the Vulpes mode and its flags.
fn split_plan_modes(plan: &mut Value) {
    if let Some(slots) = plan.get_mut("slots").and_then(Value::as_array_mut) {
        for s in slots {
            let name = s["mode"].as_str().unwrap_or("").to_string();
            let (mode, pv_off, no_export) = split_mode(&name);
            s["mode"] = json!(mode);
            s["pv_off"] = json!(pv_off);
            s["no_export"] = json!(no_export);
        }
    }
    if let Some(summary) = plan.get_mut("summary") {
        let name = summary["mode_now"].as_str().unwrap_or("").to_string();
        let (mode, pv_off, no_export) = split_mode(&name);
        summary["mode_now"] = json!(mode);
        summary["pv_off_now"] = json!(pv_off);
        summary["no_export_now"] = json!(no_export);
    }
}

/// Calibrate the solar planes from recorder history. `req` holds `cfg`,
/// `lat`, `lon`, `strings` (hourly mean W rows per string power sensor) and
/// `total` (hourly kWh rows per solar energy statistic); optional `weather`
/// and `weather_day_ahead` replace the fetches (tests). Returns the result,
/// with a warning per string sensor without history, and `based_on`: the
/// planes it was computed for.
pub fn calibrate_ha(req: &Value) -> Result<Value> {
    let cfg = obj(req, "cfg")?;
    let planes = cfg
        .get(CONF_PLANES)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if planes.is_empty() {
        return Err(Error::Message("no solar arrays configured".into()));
    }
    let strings: BTreeMap<String, Vec<(i64, f64)>> = energy_rows(req, "strings")?
        .iter()
        .map(|(id, rows)| (id.clone(), hourly_series(rows, 1.0)))
        .collect();
    let energy: BTreeMap<String, Vec<(i64, f64)>> = energy_rows(req, "total")?
        .iter()
        .map(|(id, rows)| (id.clone(), hourly_series(rows, 1.0)))
        .collect();
    let per_plane: Vec<Vec<(i64, f64)>> = planes
        .iter()
        .map(|p| {
            let id = p.get(CONF_POWER_ENTITY).and_then(Value::as_str).unwrap_or("");
            strings.get(id).cloned().unwrap_or_default()
        })
        .collect();
    let any = per_plane.iter().any(|s| !s.is_empty());
    let mut request = json!({
        // Rounded to ~1 km in the core before any external request.
        "location": {"lat": req_num(req, "lat")?, "lon": req_num(req, "lon")?},
        "planes": engine_planes(&planes),
        "strings": if any { json!(per_plane) } else { json!([]) },
        "total": total_power(&energy),
    });
    for key in ["weather", "weather_day_ahead"] {
        if let Some(w) = req.get(key).filter(|w| !w.is_null()) {
            request[key] = w.clone();
        }
    }
    let raw = vulpes_engine::calibrate_json(&request.to_string())
        .map_err(|e| Error::Message(format!("solar calibration failed: {e}")))?;
    let mut result: Value = serde_json::from_str(&raw)?;
    let mut warnings: Vec<Value> = strings
        .iter()
        .filter(|(_, s)| s.is_empty())
        .map(|(id, _)| {
            json!(format!(
                "no hourly statistics for {id}; it needs state_class measurement and some history"
            ))
        })
        .collect();
    warnings.extend(
        result
            .get("warnings")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    );
    result["warnings"] = Value::Array(warnings);
    Ok(json!({"result": result, "based_on": engine_planes(&planes)}))
}

/// Whether to fetch wholesale prices now, and for which dates: the history
/// window up to tomorrow, at most every [`WHOLESALE_REFRESH_S`]. `req` holds
/// `cfg`, `now` and `fetched_at` (Unix seconds of the last successful fetch, or null).
pub fn wholesale_fetch(req: &Value) -> Result<Value> {
    let cfg = obj(req, "cfg")?;
    let now = req_num(req, "now")?;
    if cfg.get("wholesale_source").and_then(Value::as_str) != Some("energyzero") {
        return Ok(json!({"fetch": false}));
    }
    if let Some(at) = req.get("fetched_at").and_then(as_f64) {
        if now - at < WHOLESALE_REFRESH_S {
            return Ok(json!({"fetch": false}));
        }
    }
    let t: DateTime<Utc> = DateTime::from_timestamp(now as i64, 0)
        .ok_or_else(|| Error::Message("'now' is out of range".into()))?;
    let start = (t - Duration::days(WHOLESALE_HISTORY_DAYS)).date_naive();
    let end = (t + Duration::days(1)).date_naive();
    Ok(json!({"fetch": true, "start": start.to_string(), "end": end.to_string()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: f64 = 1_781_000_000.0; // mid-June 2026

    #[test]
    fn statistics_start_on_the_hour_load_history_days_back() {
        // 1_781_000_000 = 2026-06-09 10:13:20 UTC.
        let start = stats_start(T0);
        assert_eq!(start.rem_euclid(3600), 0);
        assert_eq!(start, 1_781_000_000 - 800 - LOAD_HISTORY_DAYS * 86_400);
        assert_eq!(
            stats_start(3600.0 * 1_000_000.0),
            3_600_000_000 - LOAD_HISTORY_DAYS * 86_400
        );
    }

    fn cfg() -> Value {
        json!({
            "soc_entity": "sensor.soc", "price_entity": "sensor.price",
            "grid_import": ["sensor.import"], "grid_export": ["sensor.export"],
            "capacity_kwh": 10.0, "max_charge_w": 3000, "max_discharge_w": 3000,
            "charge_eff": 0.95, "discharge_eff": 0.95, "min_soc": 10, "max_soc": 100, "wear_cost": 0.02,
            "max_import_w": 17250, "max_export_w": 17250,
            "tariff_a": 1.21, "tariff_b": 0.13, "export_a": 1.0, "export_b": 0.0,
            "charge_w": 3000, "pv_curtail": true,
            "planes": [{"azimuth": 180, "tilt": 35, "pdc0": 4000, "ac_max": 3600, "inverter": 0, "power_entity": "sensor.pv"}]
        })
    }

    fn weather(start: f64, hours: usize) -> Value {
        let slots: Vec<Value> = (0..hours * 4)
            .map(|i| {
                let t = start - start % 900.0 + 900.0 * i as f64;
                let h = ((t / 3600.0) % 24.0 + 24.0) % 24.0;
                let sun = (std::f64::consts::PI * (h - 4.0) / 16.0).sin().max(0.0);
                json!({"start": t as i64, "ghi": 800.0 * sun, "dni": 500.0 * sun, "dhi": 150.0 * sun, "temp_air": 20.0, "wind_speed": 2.0})
            })
            .collect();
        Value::Array(slots)
    }

    fn stats() -> Value {
        let start = T0 - T0 % 3600.0 - 14.0 * 86_400.0;
        let rows = |v: f64| -> Vec<Value> {
            (0..14 * 24)
                .map(|i| json!([start + 3600.0 * f64::from(i), v]))
                .collect()
        };
        json!({"sensor.import": rows(0.4), "sensor.export": rows(0.0)})
    }

    fn request() -> Value {
        let prices: Vec<Value> = (0..24).map(|h| json!({"start": T0 - T0 % 3600.0 + 3600.0 * f64::from(h), "price": 0.2 + 0.01 * f64::from(h)})).collect();
        json!({
            "cfg": cfg(), "now": T0, "lat": 52.0, "lon": 5.0, "timezone": "Europe/Amsterdam", "country": "NL",
            "soc_state": "55", "price_attributes": [["today", prices], ["friendly_name", "Price"]], "price_unit": "EUR/kWh",
            "stats": stats(), "wholesale": [], "weather": weather(T0, 96)
        })
    }

    #[test]
    fn plans_offline_and_splits_modes() {
        let plan = plan_ha(&request()).unwrap();
        assert_eq!(plan["planned_at"], json!(T0 as i64));
        assert_eq!(plan["inputs"]["load_hours"], json!(14 * 24));
        assert_eq!(plan["inputs"]["known_price_slots"], json!(24));
        let slots = plan["slots"].as_array().unwrap();
        assert!(!slots.is_empty());
        for s in slots {
            let mode = s["mode"].as_str().unwrap();
            assert!(!mode.contains(':'), "{mode}");
            assert!(s["pv_off"].is_boolean() && s["no_export"].is_boolean());
        }
        assert!(plan["summary"]["pv_off_now"].is_boolean());
        assert!(!plan["summary"]["mode_now"].as_str().unwrap().contains(':'));
    }

    #[test]
    fn errors_in_the_order_the_coordinator_reported_them() {
        let mut r = request();
        r["soc_state"] = json!("unavailable");
        r["cfg"]["grid_import"] = json!([]);
        r["cfg"]["grid_export"] = json!([]);
        assert_eq!(
            plan_ha(&r).unwrap_err().to_string(),
            "battery SOC from sensor.soc is unavailable"
        );
        r["soc_state"] = json!("50");
        assert_eq!(
            plan_ha(&r).unwrap_err().to_string(),
            "no energy statistics selected"
        );
        let mut r = request();
        r["stats"] = json!({});
        let err = plan_ha(&r).unwrap_err().to_string();
        assert!(
            err.starts_with("planning failed: invalid request: load_history is empty"),
            "{err}"
        );
    }

    #[test]
    fn engine_request_converts_units_and_strips_local_keys() {
        let c = cfg();
        let req = engine_request(c.as_object().unwrap(), T0, 50.0, &[], &[], &[], &request()).unwrap();
        assert_eq!(req["battery"]["capacity_wh"], json!(10_000.0));
        assert_eq!(req["battery"]["min_soc"], json!(0.1));
        assert_eq!(req["soc_wh"], json!(5000.0));
        assert_eq!(req["country"], "nl");
        assert!(req["planes"][0].get("power_entity").is_none());
        let names: Vec<&str> = req["modes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"charge:pv_off"));
        let mut missing = c.as_object().unwrap().clone();
        missing.remove("wear_cost");
        assert!(engine_request(&missing, T0, 50.0, &[], &[], &[], &request()).is_err());
    }

    #[test]
    fn wholesale_window_and_refresh() {
        let c = json!({"wholesale_source": "energyzero"});
        let now = 1_794_139_200.0; // 2026-11-08 12:00 UTC
        assert_eq!(
            wholesale_fetch(&json!({"cfg": c, "now": now, "fetched_at": null})).unwrap(),
            json!({"fetch": true, "start": "2026-10-04", "end": "2026-11-09"})
        );
        assert_eq!(
            wholesale_fetch(&json!({"cfg": c, "now": now, "fetched_at": now - 3600.0})).unwrap()["fetch"],
            false
        );
        assert_eq!(
            wholesale_fetch(&json!({"cfg": c, "now": now, "fetched_at": now - WHOLESALE_REFRESH_S})).unwrap()
                ["fetch"],
            true
        );
        let none = json!({"wholesale_source": "none"});
        assert_eq!(
            wholesale_fetch(&json!({"cfg": none, "now": now})).unwrap(),
            json!({"fetch": false})
        );
    }

    #[test]
    fn calibration_needs_planes() {
        let err = calibrate_ha(&json!({"cfg": {}, "lat": 52.0, "lon": 5.0})).unwrap_err();
        assert_eq!(err.to_string(), "no solar arrays configured");
    }
}
