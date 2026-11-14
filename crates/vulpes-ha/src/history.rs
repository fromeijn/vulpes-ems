//! The integration's history file: from Home Assistant data to the engine's
//! record request, agreement with the user's mode entity counted from the
//! stored slots (so it survives restarts), the diagnostics export, and the
//! redacted settings for the diagnostics download.

use crate::cfg::{Cfg, obj, req_num, str_list, truthy};
use crate::inputs::{EnergyRows, hourly_sums, load_groups, load_series, soc_value};
use crate::modes::{join_mode, mode_of_state, split_mode};
use crate::settings::{
    CONF_BATTERY_CHARGE, CONF_BATTERY_DISCHARGE, CONF_GRID_EXPORT, CONF_GRID_IMPORT, CONF_SOLAR_ENERGY,
};
use crate::solar_form::{CONF_PLANES, fits_form};
use crate::time::zone;
use crate::{Error, Result};
use chrono::{DateTime, TimeZone};
use serde_json::{Map, Value, json};
use vulpes_engine::history as engine;
use vulpes_engine::history::{EnergyHours, ExportRequest, RecordPlan, RecordRequest, RecordSlot};

/// Days of history the diagnostics download includes.
pub const EXPORT_DAYS: i64 = 35;

fn energy_rows(req: &Value) -> Result<EnergyRows> {
    match req.get("stats") {
        None | Some(Value::Null) => Ok(EnergyRows::new()),
        Some(v) => Ok(serde_json::from_value(v.clone())?),
    }
}

/// The plan of the coordinator (Vulpes modes with flags) as the engine records it.
fn record_plan(plan: &Value) -> Option<RecordPlan> {
    let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64);
    let slots = plan
        .get("slots")?
        .as_array()?
        .iter()
        .filter_map(|s| {
            let mode = s.get("mode")?.as_str()?;
            Some(RecordSlot {
                start: s.get("start")?.as_i64()?,
                mode: join_mode(mode, truthy(s.get("pv_off")), truthy(s.get("no_export"))),
                pv_w: n(s, "pv_w")?,
                load_w: n(s, "load_w")?,
                import_price: n(s, "import_price")?,
                export_price: n(s, "export_price")?,
                price_known: truthy(s.get("price_known")),
            })
        })
        .collect();
    Some(RecordPlan {
        planned_at: plan.get("planned_at")?.as_i64()?,
        terminal_value_per_kwh: plan
            .get("summary")
            .and_then(|s| n(s, "terminal_value_per_kwh"))
            .unwrap_or(0.0),
        slots,
    })
}

/// The user's mode entity as a Vulpes mode: the mapped mode, `""` for a
/// state that maps to none, `None` without an entity or while it is unknown.
fn user_mode(cfg: &Cfg, state: Option<&str>) -> Option<String> {
    let state = state.filter(|s| !matches!(*s, "unknown" | "unavailable"))?;
    Some(mode_of_state(cfg, state).unwrap_or_default())
}

fn local_midnight(now: i64, tz: chrono_tz::Tz) -> Option<i64> {
    let day = DateTime::from_timestamp(now, 0)?.with_timezone(&tz).date_naive();
    Some(
        tz.from_local_datetime(&day.and_hms_opt(0, 0, 0)?)
            .earliest()?
            .timestamp(),
    )
}

/// Build the engine's record request from what the coordinator has: `cfg`,
/// `now`, `timezone`, the `plan` (null when the refresh failed), the SOC
/// state `soc_state`, the mode entity's `mode_state`, `active`, the
/// `last_command` and `last_error` of the control script, the energy
/// `stats` and the `wholesale` history.
pub fn record_request(req: &Value) -> Result<RecordRequest> {
    let cfg = obj(req, "cfg")?;
    let now = req_num(req, "now")? as i64;
    let tz = zone(req.get("timezone").and_then(Value::as_str).unwrap_or("UTC"));
    let stats = energy_rows(req)?;
    let group = |key: &str| hourly_sums(&str_list(cfg, key), &stats);
    let active = truthy(req.get("active"));
    let command_ok = req
        .get("last_command")
        .is_some_and(|c| c.is_object() && !truthy(c.get("fallback")));
    let error = req
        .get("last_error")
        .and_then(Value::as_str)
        .is_some_and(|e| !e.is_empty());
    let has_entity = cfg
        .get("mode_entity")
        .and_then(Value::as_str)
        .is_some_and(|e| !e.is_empty());
    let wholesale = req
        .get("wholesale")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| Some((p.get(0)?.as_f64()? as i64, p.get(1)?.as_f64()?)))
                .collect()
        })
        .unwrap_or_default();
    Ok(RecordRequest {
        now,
        plan: req.get("plan").filter(|p| !p.is_null()).and_then(record_plan),
        soc_pct: soc_value(req.get("soc_state").and_then(Value::as_str)),
        active,
        followed: active && command_ok && !error,
        user_mode: if has_entity {
            user_mode(cfg, req.get("mode_state").and_then(Value::as_str))
        } else {
            None
        },
        energy: EnergyHours {
            grid_import: group(CONF_GRID_IMPORT),
            grid_export: group(CONF_GRID_EXPORT),
            solar: group(CONF_SOLAR_ENERGY),
            battery_charge: group(CONF_BATTERY_CHARGE),
            battery_discharge: group(CONF_BATTERY_DISCHARGE),
        },
        load: load_series(&load_groups(cfg), &stats),
        wholesale,
        day_start: local_midnight(now, tz),
    })
}

/// Agreement of the user's mode entity with the plan over `slots`: slots
/// with both a planned mode and a known entity state count; they agree when
/// the entity showed the planned Vulpes mode (flags aside).
pub fn agreement(slots: &[vulpes_core::PlanSlotRecord]) -> (usize, usize) {
    let compared: Vec<bool> = slots
        .iter()
        .filter_map(|s| Some(split_mode(s.mode.as_deref()?).0 == s.user_mode.as_deref()?))
        .collect();
    (compared.len(), compared.iter().filter(|a| **a).count())
}

/// Record a tick in the history file at `path`; returns today's agreement,
/// whether the current slot agrees, the newest hour of energy data, and warnings.
pub fn record_ha(path: &str, req: &Value) -> Result<Value> {
    let request = record_request(req)?;
    let mut store = engine::open(path).map_err(|e| Error::Message(format!("history: {e}")))?;
    let resp = engine::record(&mut store, &request).map_err(|e| Error::Message(format!("history: {e}")))?;
    store
        .checkpoint()
        .map_err(|e| Error::Message(format!("history: {e}")))?;
    let (compared, agreed) = agreement(&resp.today);
    let slot = request.now - request.now.rem_euclid(900);
    let now = resp
        .today
        .iter()
        .find(|s| s.ts == slot)
        .and_then(|s| Some(split_mode(s.mode.as_deref()?).0 == s.user_mode.as_deref()?));
    Ok(json!({
        "agreement_today": {"compared": compared, "agreed": agreed},
        "agrees_now": now,
        "newest_energy_ts": resp.newest_energy_ts,
        "forecasts_written": resp.forecasts_written,
        "warnings": resp.warnings,
    }))
}

/// The last [`EXPORT_DAYS`] of history for the diagnostics download. `req` holds `now`.
pub fn export_ha(path: &str, req: &Value) -> Result<Value> {
    let now = req_num(req, "now")? as i64;
    let store = engine::open(path).map_err(|e| Error::Message(format!("history: {e}")))?;
    let e = engine::export(
        &store,
        &ExportRequest {
            from: now - EXPORT_DAYS * 86_400,
            to: now + 86_400,
        },
    )
    .map_err(|e| Error::Message(format!("history: {e}")))?;
    Ok(serde_json::to_value(e)?)
}

/// Review the history file at `path` with the current settings: savings,
/// forecast quality, control and the review's checks. `req` holds `cfg`,
/// `now` and `timezone`.
pub fn review_ha(path: &str, req: &Value) -> Result<Value> {
    let cfg = obj(req, "cfg")?;
    let nonempty = |k: &str| !str_list(cfg, k).is_empty();
    let planes = cfg
        .get(CONF_PLANES)
        .and_then(Value::as_array)
        .is_some_and(|p| !p.is_empty());
    let request = json!({
        "now": req_num(req, "now")? as i64,
        "timezone": req.get("timezone").and_then(Value::as_str).unwrap_or("UTC"),
        "battery": crate::request::battery_json(cfg)?,
        "grid": crate::request::grid_json(cfg)?,
        "modes": crate::modes::engine_modes(cfg)?,
        "tariff": {"a": crate::cfg::num(cfg, "tariff_a")?, "b": crate::cfg::num(cfg, "tariff_b")?},
        "export": {"a": crate::cfg::num(cfg, "export_a")?, "b": crate::cfg::num(cfg, "export_b")?},
        "has_export": nonempty(CONF_GRID_EXPORT),
        "has_solar": nonempty(CONF_SOLAR_ENERGY),
        "has_battery": nonempty(CONF_BATTERY_CHARGE) && nonempty(CONF_BATTERY_DISCHARGE),
        "has_pv": planes,
    });
    let raw = vulpes_engine::review::review_json(path, &request.to_string())
        .map_err(|e| Error::Message(format!("review: {e}")))?;
    Ok(serde_json::from_str(&raw)?)
}

/// Keys whose values name the user's entities, scripts or mode names.
const PRIVATE_KEYS: [&str; 10] = [
    "soc_entity",
    "price_entity",
    "mode_entity",
    "control_script",
    "mode_map",
    CONF_GRID_IMPORT,
    CONF_GRID_EXPORT,
    CONF_SOLAR_ENERGY,
    CONF_BATTERY_CHARGE,
    CONF_BATTERY_DISCHARGE,
];

/// Settings for the diagnostics download: entity ids, the script and mode
/// names redacted (lists keep their length), the solar planes reduced to a
/// count and total capacity.
pub fn redact_settings(cfg: &Cfg) -> Value {
    let mut out = Map::new();
    for (k, v) in cfg {
        let value = if k == CONF_PLANES {
            let planes = v.as_array().cloned().unwrap_or_default();
            let wp: f64 = planes
                .iter()
                .filter_map(|p| p.get("pdc0").and_then(Value::as_f64))
                .sum();
            json!({"count": planes.len(), "total_wp": wp, "form": fits_form(&planes),
                   "string_sensors": planes.iter().filter(|p| truthy(p.get("power_entity"))).count()})
        } else if PRIVATE_KEYS.contains(&k.as_str()) {
            match v {
                Value::Array(a) => Value::Array(vec![json!("**REDACTED**"); a.len()]),
                Value::Object(o) => o
                    .keys()
                    .map(|m| (m.clone(), json!("**REDACTED**")))
                    .collect::<Map<_, _>>()
                    .into(),
                Value::Null => Value::Null,
                Value::String(s) if s.is_empty() => json!(""),
                _ => json!("**REDACTED**"),
            }
        } else {
            v.clone()
        };
        out.insert(k.clone(), value);
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vulpes_core::PlanSlotRecord;

    const T0: i64 = 1_794_139_200; // 2026-11-08 12:00 UTC

    fn cfg() -> Value {
        json!({
            "grid_import": ["sensor.i"], "grid_export": ["sensor.e"], "solar_energy": ["sensor.s"],
            "mode_entity": "input_select.m", "mode_map": {"hold": "Grid", "charge": "Charging"},
            "soc_entity": "sensor.soc", "control_script": "script.x", "capacity_kwh": 10,
            "planes": [{"azimuth": 180, "tilt": 30, "pdc0": 3000, "ac_max": 3000, "power_entity": "sensor.pv"},
                       {"azimuth": 90, "tilt": 30, "pdc0": 1500, "ac_max": 3000}]
        })
    }

    fn plan() -> Value {
        let slots: Vec<Value> = (0..8)
            .map(|k| {
                json!({"start": T0 + 900 * k, "mode": "hold", "pv_off": false, "no_export": k % 2 == 1,
                            "pv_w": 100.0, "load_w": 300.0, "import_price": 0.2, "export_price": 0.05,
                            "price_known": true, "soc_pct": 50.0, "battery_w": 0.0, "grid_w": 200.0})
            })
            .collect();
        json!({"planned_at": T0, "slots": slots, "summary": {"terminal_value_per_kwh": 0.19, "mode_now": "hold"}})
    }

    fn request(now: i64) -> Value {
        let rows = |v: f64| -> Vec<Value> { (0..3).map(|h| json!([T0 - 3600 * (3 - h), v])).collect() };
        json!({
            "cfg": cfg(), "now": now as f64 + 0.4, "timezone": "Europe/Amsterdam", "plan": plan(),
            "soc_state": "61", "mode_state": "Grid", "active": true,
            "last_command": {"mode": "hold", "fallback": false}, "last_error": null,
            "stats": {"sensor.i": rows(0.5), "sensor.e": rows(0.1), "sensor.s": rows(1.0)},
            "wholesale": [[T0 - 3600, 0.1]]
        })
    }

    #[test]
    fn request_from_home_assistant_data() {
        let r = record_request(&request(T0 + 1000)).unwrap();
        assert_eq!(r.now, T0 + 1000);
        assert_eq!(r.soc_pct, Some(61.0));
        assert!(r.active && r.followed);
        assert_eq!(r.user_mode.as_deref(), Some("hold"));
        let p = r.plan.unwrap();
        assert_eq!(
            (p.planned_at, p.terminal_value_per_kwh, p.slots.len()),
            (T0, 0.19, 8)
        );
        assert_eq!(
            p.slots[1].mode, "hold:no_export",
            "flags join back into the engine name"
        );
        assert_eq!(r.energy.grid_import.len(), 3);
        assert_eq!(r.energy.solar[0].1, 1.0);
        assert!(r.energy.battery_charge.is_empty());
        // Load = import + solar − export = 1.4 kWh per hour.
        assert!(r.load.iter().all(|(_, w)| (w - 1400.0).abs() < 1e-9));
        assert_eq!(r.wholesale, [(T0 - 3600, 0.1)]);
        // Local midnight in Amsterdam (UTC+1 in November).
        assert_eq!(r.day_start, Some(T0 - 13 * 3600));
    }

    #[test]
    fn followed_and_user_mode_edge_cases() {
        let mut v = request(T0);
        v["last_command"]["fallback"] = json!(true);
        assert!(!record_request(&v).unwrap().followed);
        let mut v = request(T0);
        v["last_error"] = json!("boom");
        assert!(!record_request(&v).unwrap().followed);
        let mut v = request(T0);
        v["active"] = json!(false);
        assert!(!record_request(&v).unwrap().followed);
        let mut v = request(T0);
        v["mode_state"] = json!("Weird");
        assert_eq!(
            record_request(&v).unwrap().user_mode.as_deref(),
            Some(""),
            "unmapped disagrees"
        );
        v["mode_state"] = json!("unavailable");
        assert_eq!(record_request(&v).unwrap().user_mode, None);
        v["cfg"]["mode_entity"] = Value::Null;
        v["mode_state"] = json!("Grid");
        assert_eq!(
            record_request(&v).unwrap().user_mode,
            None,
            "no entity, no comparison"
        );
        v["plan"] = Value::Null;
        assert!(record_request(&v).unwrap().plan.is_none());
    }

    #[test]
    fn agreement_counts_slots_with_a_plan_and_a_known_state() {
        let rec = |mode: Option<&str>, user: Option<&str>| PlanSlotRecord {
            mode: mode.map(String::from),
            user_mode: user.map(String::from),
            ..Default::default()
        };
        let slots = [
            rec(Some("hold:no_export"), Some("hold")),
            rec(Some("charge"), Some("hold")),
            rec(Some("charge"), Some("")),
            rec(None, Some("hold")),
            rec(Some("hold"), None),
        ];
        assert_eq!(agreement(&slots), (3, 1));
        assert_eq!(agreement(&[]), (0, 0));
    }

    #[test]
    fn agreement_survives_reopening_the_file() {
        let dir = std::env::temp_dir().join(format!("vulpes-ha-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("e").join("vulpes.db");
        let path = path.to_str().unwrap();
        let first = record_ha(path, &request(T0 + 20)).unwrap();
        assert_eq!(first["agreement_today"], json!({"compared": 1, "agreed": 1}));
        assert_eq!(first["agrees_now"], json!(true));
        let mut v = request(T0 + 920);
        v["mode_state"] = json!("Charging");
        let second = record_ha(path, &v).unwrap();
        assert_eq!(second["agreement_today"], json!({"compared": 2, "agreed": 1}));
        assert_eq!(second["agrees_now"], json!(false));
        assert_eq!(second["newest_energy_ts"], json!(T0 - 3600));
        let export = export_ha(path, &json!({"now": T0 + 1000})).unwrap();
        assert_eq!(export["plan_slots"].as_array().unwrap().len(), 2);
        assert!(
            record_ha("", &request(T0))
                .unwrap_err()
                .to_string()
                .starts_with("history:")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn review_from_settings() {
        let dir = std::env::temp_dir().join(format!("vulpes-ha-review-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("vulpes.db");
        let path = path.to_str().unwrap();
        let mut c = cfg();
        for (k, v) in [
            ("max_charge_w", 3000.0),
            ("max_discharge_w", 3000.0),
            ("min_soc", 10.0),
            ("max_soc", 100.0),
            ("max_import_w", 17250.0),
            ("max_export_w", 17250.0),
            ("charge_w", 3000.0),
            ("charge_eff", 0.95),
            ("discharge_eff", 0.95),
            ("wear_cost", 0.02),
            ("tariff_a", 1.21),
            ("tariff_b", 0.13),
            ("export_a", 1.0),
            ("export_b", 0.0),
        ] {
            c[k] = json!(v);
        }
        record_ha(
            path,
            &json!({"cfg": c, "now": T0, "timezone": "Europe/Amsterdam", "plan": plan()}),
        )
        .unwrap();
        let out = review_ha(
            path,
            &json!({"cfg": c, "now": T0 + 3600, "timezone": "Europe/Amsterdam"}),
        )
        .unwrap();
        assert_eq!(out["windows"]["days_7"]["days"], json!(0));
        let coverage = out["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["check"] == "coverage")
            .unwrap();
        assert!(
            coverage["detail"].as_str().unwrap().contains("battery"),
            "{coverage}"
        );
        c.as_object_mut().unwrap().remove("wear_cost");
        assert!(review_ha(path, &json!({"cfg": c, "now": T0, "timezone": "UTC"})).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redacted_settings_keep_numbers_and_shapes() {
        let r = redact_settings(cfg().as_object().unwrap());
        assert_eq!(r["grid_import"], json!(["**REDACTED**"]));
        assert_eq!(r["soc_entity"], "**REDACTED**");
        assert_eq!(
            r["mode_map"],
            json!({"hold": "**REDACTED**", "charge": "**REDACTED**"})
        );
        assert_eq!(r["capacity_kwh"], json!(10));
        assert_eq!(
            r["planes"],
            json!({"count": 2, "total_wp": 4500.0, "form": false, "string_sensors": 1})
        );
        assert!(!r.to_string().contains("sensor."), "{r}");
    }
}
