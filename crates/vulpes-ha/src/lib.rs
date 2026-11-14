//! Home Assistant adapter for vulpes-ems: everything the integration decides
//! or computes, so the Python side only reads Home Assistant and publishes
//! entities.
//!
//! - settings: the current copy, migrations, and the config flow's checks;
//! - inputs: the SOC state, price attributes, the load balance from energy statistics;
//! - the plan and calibration requests built from those, and their results;
//! - the control script's variables;
//! - the solar array form and the calibration's status and messages.
//!
//! Python calls [`call`] with an op name and a JSON request (`vulpes_ems.ha`),
//! and [`plan_ha`](request::plan_ha) / [`calibrate_ha`](request::calibrate_ha)
//! for the calls that fetch weather. Every op is pure and fast.

pub mod cfg;
pub mod control;
pub mod health;
pub mod history;
pub mod inputs;
pub mod modes;
pub mod request;
pub mod settings;
pub mod solar_form;
pub mod time;

use cfg::{as_f64, obj, obj_or_empty, req_num, truthy};
use serde_json::{Value, json};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A message for the user or the log, shown as is.
    #[error("{0}")]
    Message(String),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

type Op = fn(&Value) -> Result<Value>;

fn planes(req: &Value, key: &str) -> Vec<Value> {
    req.get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn opt_str<'a>(req: &'a Value, key: &str) -> Option<&'a str> {
    req.get(key).and_then(Value::as_str)
}

fn constants(_: &Value) -> Result<Value> {
    Ok(json!({
        "config_version": settings::CONFIG_VERSION,
        "modes": modes::MODES,
        "power_controls": modes::POWER_CONTROLS,
        "directions": solar_form::DIRECTIONS.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        "max_arrays": solar_form::MAX_ARRAYS,
        "default_array": solar_form::default_array(),
        "default_inverter_w": solar_form::DEFAULT_INVERTER_W,
        "control_states": control::STATES,
        "stale_after_s": control::STALE_AFTER_S,
        "load_history_days": request::LOAD_HISTORY_DAYS,
        "wholesale_history_days": request::WHOLESALE_HISTORY_DAYS,
        "calibration_history_days": request::CALIBRATION_HISTORY_DAYS,
        "horizon_hours": request::HORIZON_HOURS,
    }))
}

fn check_sources(req: &Value) -> Result<Value> {
    let cfg = obj(req, "cfg")?;
    let attributes = req.get("price_attributes").and_then(Value::as_array).map(|a| {
        a.iter()
            .filter_map(|p| Some((p.get(0)?.as_str()?.to_string(), p.get(1)?.clone())))
            .collect::<Vec<_>>()
    });
    let stats: Option<inputs::EnergyRows> = match req.get("stats") {
        None | Some(Value::Null) => None,
        Some(v) => Some(serde_json::from_value(v.clone())?),
    };
    let check = settings::check_sources(
        cfg,
        &settings::SourceInputs {
            soc_state: opt_str(req, "soc_state"),
            price_attributes: attributes.as_deref(),
            price_unit: opt_str(req, "price_unit"),
            tz: time::zone(opt_str(req, "timezone").unwrap_or("UTC")),
            stats: stats.as_ref(),
        },
    );
    Ok(json!({"errors": check.errors, "placeholders": {"statistic": check.statistic}, "fetch": check.fetch}))
}

/// Every op `vulpes_ems.ha` routes, by name.
pub const OPS: &[(&str, Op)] = &[
    ("constants", constants),
    ("error_keys", |_| Ok(json!(settings::ERROR_KEYS))),
    ("settings", |r| {
        Ok(json!(settings::settings(
            &obj_or_empty(r, "data"),
            &obj_or_empty(r, "options")
        )))
    }),
    ("migrate", |r| {
        let version = req_num(r, "version")? as u32;
        Ok(json!(settings::migrate(obj(r, "cfg")?, version)?))
    }),
    ("enabled_modes", |r| {
        Ok(json!(modes::enabled_modes(obj(r, "cfg")?)))
    }),
    ("agrees", |r| {
        Ok(json!(modes::agrees(
            obj(r, "cfg")?,
            opt_str(r, "state"),
            opt_str(r, "mode_now").unwrap_or("")
        )))
    }),
    ("mode_map", |r| {
        Ok(json!(modes::mode_map(obj(r, "cfg")?, &obj_or_empty(r, "input"))))
    }),
    ("check_battery", |r| {
        Ok(json!(settings::check_battery(obj(r, "cfg")?)?))
    }),
    ("check_tariff", |r| {
        Ok(json!(settings::check_tariff(obj(r, "cfg")?)?))
    }),
    ("check_modes", |r| {
        let options = r.get("mode_options").and_then(Value::as_array).map(Vec::as_slice);
        Ok(json!(settings::check_modes(obj(r, "cfg")?, options)?))
    }),
    ("check_sources", check_sources),
    ("dashboard_defaults", |r| {
        Ok(json!(settings::dashboard_defaults(
            r.get("prefs").filter(|p| !p.is_null())
        )))
    }),
    ("energy_ids", |r| Ok(json!(settings::energy_ids(obj(r, "cfg")?)))),
    ("command", |r| {
        let plan = r.get("plan").filter(|p| !p.is_null());
        Ok(control::command(
            obj(r, "cfg")?,
            plan,
            req_num(r, "now")?,
            truthy(r.get("failed")),
        ))
    }),
    ("safe_command", |r| {
        let fallback = r.get("fallback").is_none_or(|f| truthy(Some(f)));
        Ok(control::safe_command(
            &obj_or_empty(r, "cfg"),
            req_num(r, "now")?,
            fallback,
        ))
    }),
    ("control_state", |r| {
        let last = r.get("last").filter(|l| !l.is_null());
        Ok(json!(control::state_of(
            truthy(r.get("active")),
            last,
            opt_str(r, "error")
        )))
    }),
    ("plane_from_array", |r| {
        let existing = r.get("existing").and_then(Value::as_object);
        let inverter_w = r
            .get("inverter_w")
            .and_then(as_f64)
            .unwrap_or(solar_form::DEFAULT_INVERTER_W);
        solar_form::plane_from_array(obj(r, "array")?, inverter_w, existing)
    }),
    ("form_defaults", |r| {
        let (count, inverter_w, arrays) = solar_form::form_defaults(&planes(r, "planes"))?;
        Ok(json!({"count": count, "inverter_w": inverter_w, "arrays": arrays}))
    }),
    ("fits_form", |r| {
        Ok(json!(solar_form::fits_form(&planes(r, "planes"))))
    }),
    ("engine_planes", |r| {
        Ok(json!(solar_form::engine_planes(&planes(r, "planes"))))
    }),
    ("parse_planes", |r| {
        Ok(json!(solar_form::parse_planes(opt_str(r, "text").unwrap_or(""))?))
    }),
    ("apply_proposal", |r| {
        let result = r.get("result").cloned().unwrap_or(Value::Null);
        Ok(json!(solar_form::apply_proposal(&planes(r, "planes"), &result)?))
    }),
    ("summarize", |r| {
        let result = r.get("result").cloned().unwrap_or(Value::Null);
        Ok(json!(solar_form::summarize(&planes(r, "planes"), &result)))
    }),
    ("calibration_view", |r| {
        Ok(solar_form::calibration_view(
            obj(r, "cfg")?,
            &obj_or_empty(r, "stored"),
            truthy(r.get("running")),
            truthy(r.get("error")),
        ))
    }),
    ("calibration_message", |r| {
        let result = r.get("result").cloned().unwrap_or(Value::Null);
        Ok(json!(solar_form::calibration_message(obj(r, "cfg")?, &result)))
    }),
    ("calibration_inputs", |r| {
        Ok(solar_form::calibration_inputs(obj(r, "cfg")?))
    }),
    ("wholesale_fetch", request::wholesale_fetch),
    ("redact_settings", |r| {
        Ok(history::redact_settings(obj(r, "cfg")?))
    }),
    ("health", health::health),
    ("issue_ids", |_| Ok(json!(health::ISSUES))),
    ("known_prices", |r| {
        let attributes = r.get("price_attributes").and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(|p| Some((p.get(0)?.as_str()?.to_string(), p.get(1)?.clone())))
                .collect::<Vec<_>>()
        });
        let tz = time::zone(opt_str(r, "timezone").unwrap_or("UTC"));
        let unit = opt_str(r, "price_unit");
        Ok(json!(
            attributes
                .map(|a| inputs::parse_price_attributes(&a, unit, tz))
                .unwrap_or_default()
        ))
    }),
    ("load_series", |r| {
        let stats: inputs::EnergyRows = match r.get("stats") {
            None | Some(Value::Null) => inputs::EnergyRows::new(),
            Some(v) => serde_json::from_value(v.clone())?,
        };
        Ok(json!(inputs::load_series(
            &inputs::load_groups(obj(r, "cfg")?),
            &stats
        )))
    }),
];

/// Run the op `op` on a JSON request; returns its JSON result.
pub fn call(op: &str, request: &str) -> Result<String> {
    let f = OPS
        .iter()
        .find(|(name, _)| *name == op)
        .map(|(_, f)| f)
        .ok_or_else(|| Error::Message(format!("unknown op '{op}'")))?;
    let req: Value = serde_json::from_str(request)?;
    Ok(f(&req)?.to_string())
}

/// Record a tick in the history file at `path` (see [`history::record_ha`]).
pub fn record_ha_json(path: &str, request: &str) -> Result<String> {
    Ok(history::record_ha(path, &serde_json::from_str(request)?)?.to_string())
}

/// Review the history file at `path` (see [`history::review_ha`]).
pub fn review_ha_json(path: &str, request: &str) -> Result<String> {
    Ok(history::review_ha(path, &serde_json::from_str(request)?)?.to_string())
}

/// The recent history from the file at `path`, for diagnostics (see [`history::export_ha`]).
pub fn export_history_json(path: &str, request: &str) -> Result<String> {
    Ok(history::export_ha(path, &serde_json::from_str(request)?)?.to_string())
}

/// Plan with Home Assistant inputs (see [`request::plan_ha`]). May fetch weather.
pub fn plan_ha_json(request: &str) -> Result<String> {
    Ok(request::plan_ha(&serde_json::from_str(request)?)?.to_string())
}

/// Calibrate the solar planes from recorder history (see [`request::calibrate_ha`]). Fetches weather.
pub fn calibrate_ha_json(request: &str) -> Result<String> {
    Ok(request::calibrate_ha(&serde_json::from_str(request)?)?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_op_is_routed_and_answers_json() {
        let mut names: Vec<&str> = OPS.iter().map(|(n, _)| *n).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "op names are unique");
        let out: Value = serde_json::from_str(&call("constants", "{}").unwrap()).unwrap();
        assert_eq!(out["config_version"], json!(3));
        assert_eq!(out["modes"][0], json!(["charge", true]));
        assert_eq!(out["control_states"][1], "following_plan");
        assert_eq!(out["directions"].as_array().unwrap().len(), 8);
    }

    #[test]
    fn unknown_op_and_bad_json_are_errors() {
        assert_eq!(call("nope", "{}").unwrap_err().to_string(), "unknown op 'nope'");
        assert!(
            call("constants", "not json")
                .unwrap_err()
                .to_string()
                .starts_with("invalid JSON")
        );
        assert_eq!(
            call("enabled_modes", "{}").unwrap_err().to_string(),
            "'cfg' must be an object"
        );
    }

    #[test]
    fn ops_round_trip_through_json() {
        let r = |op: &str, req: Value| -> Value {
            serde_json::from_str(&call(op, &req.to_string()).unwrap()).unwrap()
        };
        assert_eq!(
            r("enabled_modes", json!({"cfg": {"idle": true}})),
            json!(["charge", "hold", "self_consume", "idle"])
        );
        assert_eq!(
            r(
                "agrees",
                json!({"cfg": {"mode_map": {"hold": "Grid"}}, "state": "Grid", "mode_now": "hold"})
            ),
            json!(true)
        );
        assert_eq!(
            r("agrees", json!({"cfg": {}, "state": null, "mode_now": "hold"})),
            Value::Null
        );
        assert_eq!(
            r("control_state", json!({"active": true, "last": null})),
            json!("fallback")
        );
        assert_eq!(r("safe_command", json!({"now": 0}))["fallback"], json!(true));
        assert_eq!(
            r(
                "check_sources",
                json!({"cfg": {"grid_import": ["sensor.i"]}, "soc_state": "50",
            "price_attributes": [["today", [{"start": 0, "price": 0.2}]]]})
            )["fetch"],
            json!(["sensor.i"])
        );
        assert_eq!(
            r(
                "check_modes",
                json!({"cfg": {"charge": false}, "mode_options": []})
            )["mode_entity"],
            "no_mode_options"
        );
        assert_eq!(r("form_defaults", json!({"planes": []}))["count"], json!(1));
        assert_eq!(
            r("migrate", json!({"cfg": {"net_metering": false}, "version": 2})),
            json!({})
        );
        assert!(
            r("error_keys", json!({}))
                .as_array()
                .unwrap()
                .contains(&json!("no_modes"))
        );
    }
}
