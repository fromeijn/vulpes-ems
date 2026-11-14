//! Solar arrays: the simple setup form, and solar calibration requests and results.
//!
//! A user describes each array (direction, tilt, kWp, optionally the power
//! sensor of the string that feeds it) and the inverter's AC limit. That
//! becomes a list of engine planes on one shared inverter. After setup,
//! Vulpes calibrates the planes from recorder history and proposes
//! corrections; the user applies them.

use crate::cfg::{Cfg, as_f64, int_value, py_eq, round_to, str_list, text, truthy};
use crate::settings::CONF_SOLAR_ENERGY;
use crate::{Error, Result};
use serde::Deserialize;
use serde_json::{Map, Value, json};

pub const CONF_PLANES: &str = "planes";
/// `form` or `json`; absent for setups before 0.3.0.
pub const CONF_SOLAR_SETUP: &str = "solar_setup";
pub const CONF_DIRECTION: &str = "direction";
pub const CONF_TILT: &str = "tilt";
pub const CONF_KWP: &str = "kwp";
pub const CONF_POWER_ENTITY: &str = "power_entity";

pub const MAX_ARRAYS: usize = 4;
/// The inverter id the form gives every array: one shared inverter.
pub const FORM_INVERTER: i64 = 0;
pub const DEFAULT_INVERTER_W: f64 = 4000.0;

pub const DIRECTIONS: [(&str, f64); 8] = [
    ("n", 0.0),
    ("ne", 45.0),
    ("e", 90.0),
    ("se", 135.0),
    ("s", 180.0),
    ("sw", 225.0),
    ("w", 270.0),
    ("nw", 315.0),
];

/// Plane keys only the integration uses; stripped before any engine call.
const LOCAL_KEYS: [&str; 1] = [CONF_POWER_ENTITY];

pub fn default_array() -> Value {
    json!({CONF_DIRECTION: "s", CONF_TILT: 35.0, CONF_KWP: 4.0})
}

/// `str(value)` in Python for a direction from the form.
fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) if n.is_f64() => {
            let x = n.as_f64().unwrap_or(0.0);
            if x.fract() == 0.0 {
                format!("{x:.1}")
            } else {
                x.to_string()
            }
        }
        other => other.to_string(),
    }
}

/// Compass key (`"se"`) or degrees clockwise from north (`"200"`, `200`) → azimuth.
pub fn azimuth_of(direction: &Value) -> Result<f64> {
    let key = py_str(direction).trim().to_lowercase();
    if let Some((_, az)) = DIRECTIONS.iter().find(|(k, _)| *k == key) {
        return Ok(*az);
    }
    let az: f64 = key
        .trim_end_matches('°')
        .parse()
        .map_err(|_| Error::Message(format!("invalid direction '{key}'")))?;
    if !(0.0..=360.0).contains(&az) {
        return Err(Error::Message(format!("azimuth {az} out of range 0..360")));
    }
    Ok(az % 360.0)
}

/// Python's `f"{x:g}"`: six significant digits, no trailing zeros.
fn py_g(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    let exp = x.abs().log10().floor() as i32;
    let trim = |s: String| {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    };
    if (-4..6).contains(&exp) {
        let decimals = (5 - exp).max(0) as usize;
        trim(format!("{x:.decimals$}"))
    } else {
        let s = format!("{x:.5e}");
        let (mantissa, e) = s.split_once('e').unwrap_or((&s, "0"));
        let e: i32 = e.parse().unwrap_or(0);
        format!(
            "{}e{}{:02}",
            trim(mantissa.to_string()),
            if e < 0 { '-' } else { '+' },
            e.abs()
        )
    }
}

/// The compass key for an exact compass azimuth, else the degrees as text.
pub fn direction_of(azimuth: f64) -> String {
    let az = azimuth.rem_euclid(360.0);
    DIRECTIONS
        .iter()
        .find(|(_, v)| (v - az).abs() < 1e-9)
        .map_or_else(|| py_g(az), |(k, _)| (*k).to_string())
}

fn field_num(m: &Map<String, Value>, key: &str) -> Result<f64> {
    m.get(key)
        .and_then(as_f64)
        .ok_or_else(|| Error::Message(format!("'{key}' must be a number")))
}

/// One form array → an engine plane on the shared inverter. Keys of
/// `existing` that the form doesn't show (loss parameters, for example) are
/// kept, so editing through the form loses nothing.
pub fn plane_from_array(
    array: &Map<String, Value>,
    inverter_w: f64,
    existing: Option<&Map<String, Value>>,
) -> Result<Value> {
    let mut plane = existing.cloned().unwrap_or_default();
    let direction = array
        .get(CONF_DIRECTION)
        .ok_or_else(|| Error::Message("missing direction".into()))?;
    plane.insert("azimuth".into(), json!(azimuth_of(direction)?));
    plane.insert("tilt".into(), json!(field_num(array, CONF_TILT)?));
    plane.insert("pdc0".into(), json!(field_num(array, CONF_KWP)? * 1000.0));
    plane.insert("ac_max".into(), json!(inverter_w));
    plane.insert("inverter".into(), json!(FORM_INVERTER));
    match array.get(CONF_POWER_ENTITY).filter(|v| truthy(Some(v))) {
        Some(entity) => plane.insert(CONF_POWER_ENTITY.into(), entity.clone()),
        None => plane.remove(CONF_POWER_ENTITY),
    };
    Ok(Value::Object(plane))
}

/// An engine plane → the values to prefill a form array with.
pub fn array_from_plane(plane: &Map<String, Value>) -> Result<Value> {
    let mut out = Map::new();
    out.insert(
        CONF_DIRECTION.into(),
        json!(direction_of(field_num(plane, "azimuth")?)),
    );
    out.insert(CONF_TILT.into(), json!(round_to(field_num(plane, "tilt")?, 1)));
    out.insert(
        CONF_KWP.into(),
        json!(round_to(field_num(plane, "pdc0")? / 1000.0, 3)),
    );
    if let Some(entity) = plane.get(CONF_POWER_ENTITY).filter(|v| truthy(Some(v))) {
        out.insert(CONF_POWER_ENTITY.into(), entity.clone());
    }
    Ok(Value::Object(out))
}

/// Whether the planes can be edited with the form without changing
/// behaviour: 1 to [`MAX_ARRAYS`] planes, on one shared inverter (or a single plane).
pub fn fits_form(planes: &[Value]) -> bool {
    if planes.is_empty() || planes.len() > MAX_ARRAYS {
        return false;
    }
    if planes.len() == 1 {
        return true;
    }
    let shared = planes
        .iter()
        .all(|p| p.get("inverter").is_some_and(|i| py_eq(i, &json!(FORM_INVERTER))));
    let limits: Option<Vec<f64>> = planes.iter().map(|p| p.get("ac_max").and_then(as_f64)).collect();
    shared && limits.is_some_and(|l| l.iter().all(|x| *x == l[0]))
}

/// Array count, inverter limit and per-array values to prefill the form with.
pub fn form_defaults(planes: &[Value]) -> Result<(usize, f64, Vec<Value>)> {
    if fits_form(planes) {
        let ac = planes[0]
            .get("ac_max")
            .and_then(as_f64)
            .unwrap_or(DEFAULT_INVERTER_W);
        let arrays = planes
            .iter()
            .map(|p| array_from_plane(p.as_object().unwrap_or(&Map::new())))
            .collect::<Result<_>>()?;
        Ok((planes.len(), ac, arrays))
    } else {
        Ok((1, DEFAULT_INVERTER_W, Vec::new()))
    }
}

/// Planes as the engine expects them, without integration-only keys.
pub fn engine_planes(planes: &[Value]) -> Vec<Value> {
    planes
        .iter()
        .map(|p| match p {
            Value::Object(m) => Value::Object(
                m.iter()
                    .filter(|(k, _)| !LOCAL_KEYS.contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
            other => other.clone(),
        })
        .collect()
}

/// A JSON object read with its keys in file order.
struct Ordered(Vec<(String, Value)>);

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Ordered;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut m: A,
            ) -> std::result::Result<Ordered, A::Error> {
                let mut out = Vec::new();
                while let Some(entry) = m.next_entry::<String, Value>()? {
                    out.push(entry);
                }
                Ok(Ordered(out))
            }
        }
        d.deserialize_map(V)
    }
}

/// Planes pasted in the JSON step: a list of plane objects, or the output of
/// `vulpes-cli calibrate-solar` (an object of `{params: plane}` entries,
/// taken in file order).
pub fn parse_planes(text: &str) -> Result<Vec<Value>> {
    let invalid = |why: &str| Error::Message(format!("invalid planes: {why}"));
    let data: Value = serde_json::from_str(text).map_err(|e| invalid(&e.to_string()))?;
    let planes: Vec<Value> = match data {
        Value::Array(list) => list,
        Value::Object(_) => {
            let ordered: Ordered = serde_json::from_str(text).map_err(|e| invalid(&e.to_string()))?;
            ordered
                .0
                .into_iter()
                .map(|(_, v)| match v.get("params") {
                    Some(p) if v.is_object() => Ok(p.clone()),
                    _ if v.is_object() => Ok(v),
                    _ => Err(invalid("expected objects")),
                })
                .collect::<Result<_>>()?
        }
        _ => return Err(invalid("expected a non-empty list of planes")),
    };
    if planes.is_empty() {
        return Err(invalid("expected a non-empty list of planes"));
    }
    for p in &planes {
        for key in ["azimuth", "tilt", "pdc0", "ac_max"] {
            p.get(key)
                .and_then(as_f64)
                .ok_or_else(|| invalid(&format!("'{key}' must be a number")))?;
        }
    }
    Ok(planes)
}

/// Take the proposed orientation and capacity, keep everything else (sensors, limits).
pub fn apply_proposal(planes: &[Value], result: &Value) -> Result<Vec<Value>> {
    let proposed = result
        .get("planes")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Message("the calibration has no planes".into()))?;
    if proposed.len() != planes.len() {
        return Err(Error::Message(
            "the calibration is for a different number of planes".into(),
        ));
    }
    planes
        .iter()
        .zip(proposed)
        .map(|(old, new)| {
            let mut plane = old.as_object().cloned().unwrap_or_default();
            let new = new.as_object().cloned().unwrap_or_default();
            plane.insert("azimuth".into(), json!(round_to(field_num(&new, "azimuth")?, 1)));
            plane.insert("tilt".into(), json!(round_to(field_num(&new, "tilt")?, 1)));
            plane.insert("pdc0".into(), int_value(field_num(&new, "pdc0")?.round()));
            Ok(Value::Object(plane))
        })
        .collect()
}

/// Whether the proposal differs noticeably from the given planes: more than
/// 0.5° in orientation or 0.5 % in capacity.
pub fn has_changes(result: &Value, planes: &[Value]) -> bool {
    let empty = Vec::new();
    let proposed = result.get("planes").and_then(Value::as_array).unwrap_or(&empty);
    if proposed.len() != planes.len() {
        return false;
    }
    let n = |p: &Value, k: &str| p.get(k).and_then(as_f64).unwrap_or(0.0);
    proposed.iter().zip(planes).any(|(new, old)| {
        let turn = ((n(new, "azimuth") - n(old, "azimuth") + 180.0).rem_euclid(360.0) - 180.0).abs();
        let tilt = (n(new, "tilt") - n(old, "tilt")).abs();
        let size = (n(new, "pdc0") - n(old, "pdc0")).abs() / n(old, "pdc0").max(1.0);
        turn > 0.5 || tilt > 0.5 || size > 0.005
    })
}

/// Relative reduction of the hourly RMSE by the proposal, in %.
pub fn improvement_pct(result: &Value) -> Option<f64> {
    let rmse = |k: &str| {
        result
            .get(k)
            .filter(|v| truthy(Some(v)))
            .and_then(|s| s.get("rmse_w"))
            .and_then(as_f64)
    };
    let (cur, new) = (rmse("score_current")?, rmse("score_proposed")?);
    if cur == 0.0 {
        return None;
    }
    Some(round_to(100.0 * (1.0 - new / cur), 1))
}

/// Status of the solar calibration for its sensor. A new proposal for
/// applied planes is `done` (there is something to apply), not `applied`.
pub fn calibration_status(
    running: bool,
    error: bool,
    applied: bool,
    proposal: bool,
    result: bool,
    stored: bool,
) -> &'static str {
    if running {
        "running"
    } else if error {
        "failed"
    } else if proposal {
        "done"
    } else if applied {
        "applied"
    } else if result {
        "done"
    } else if stored {
        "stale"
    } else {
        "not_run"
    }
}

/// Markdown summary of a calibration result for the options flow.
pub fn summarize(planes: &[Value], result: &Value) -> String {
    let n = |p: &Value, k: &str| p.get(k).and_then(as_f64).unwrap_or(0.0);
    let empty = Vec::new();
    let proposed = result.get("planes").and_then(Value::as_array).unwrap_or(&empty);
    let fits = result
        .get("fits")
        .and_then(Value::as_array)
        .filter(|f| !f.is_empty());
    let mut lines = Vec::new();
    for (i, (old, new)) in planes.iter().zip(proposed).enumerate() {
        let fit = fits.and_then(|f| f.get(i)).cloned().unwrap_or_else(|| json!({}));
        let method = fit
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("unchanged")
            .replace('_', " ");
        let hours = match fit.get("hours") {
            Some(Value::Number(h)) => h.to_string(),
            Some(Value::String(h)) => h.clone(),
            _ => "0".into(),
        };
        lines.push(format!(
            "- Array {}: {:.0}° / {:.0}° / {:.2} kWp → **{:.0}° / {:.0}° / {:.2} kWp** ({method}, {hours} clear hours)",
            i + 1,
            n(old, "azimuth"),
            n(old, "tilt"),
            n(old, "pdc0") / 1000.0,
            n(new, "azimuth"),
            n(new, "tilt"),
            n(new, "pdc0") / 1000.0,
        ));
    }
    let score = |k: &str| result.get(k).filter(|v| truthy(Some(v)));
    if let (Some(cur), Some(new)) = (score("score_current"), score("score_proposed")) {
        let where_ = if truthy(result.get("held_out")) {
            "held-out weeks"
        } else {
            "the same data it was fitted on"
        };
        lines.push(format!(
            "- Hourly forecast error (RMSE) on {where_}: {:.0} W → **{:.0} W** (bias {:+.0} → {:+.0} W)",
            n(cur, "rmse_w"),
            n(new, "rmse_w"),
            n(cur, "bias_w"),
            n(new, "bias_w"),
        ));
    }
    for w in result
        .get("warnings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        lines.push(format!(
            "- ⚠ {}",
            w.as_str().map_or_else(|| w.to_string(), String::from)
        ));
    }
    lines.join("\n")
}

fn planes_of(cfg: &Cfg) -> Vec<Value> {
    cfg.get(CONF_PLANES)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// What the calibration sensor and the options flow show, from the stored
/// calibration and the current settings.
pub fn calibration_view(cfg: &Cfg, stored: &Map<String, Value>, running: bool, error: bool) -> Value {
    let planes = planes_of(cfg);
    let current = Value::Array(engine_planes(&planes));
    let result = stored.get("result").filter(|r| truthy(Some(r)));
    let based_on = stored.get("based_on");
    let based_on_current = based_on.is_some_and(|b| py_eq(b, &current));
    let applied = result.is_some() && stored.get("applied").is_some_and(|a| py_eq(a, &current));
    let shown = result.filter(|_| based_on_current || applied);
    let proposal = result.filter(|r| based_on_current && has_changes(r, &planes));
    let improvement = shown.map(|r| {
        let base = based_on.and_then(Value::as_array).cloned().unwrap_or_default();
        if has_changes(r, &base) {
            improvement_pct(r).map_or(Value::Null, Value::from)
        } else {
            json!(0.0)
        }
    });
    json!({
        "applied": applied,
        "result": shown,
        "proposal": proposal,
        "improvement": improvement.unwrap_or(Value::Null),
        "status": calibration_status(running, error, applied, proposal.is_some(), shown.is_some(), result.is_some()),
        // Setups made with the array form calibrate once by themselves; JSON setups only when asked.
        "auto_run": text(cfg, CONF_SOLAR_SETUP) == "form" && !truthy(stored.get("auto_ran")),
    })
}

/// The notification text after a calibration run.
pub fn calibration_message(cfg: &Cfg, result: &Value) -> String {
    if !has_changes(result, &planes_of(cfg)) {
        let mut message = String::from("Solar calibration found nothing to change.");
        let warnings: Vec<String> = result
            .get("warnings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|w| format!("- {}", w.as_str().map_or_else(|| w.to_string(), String::from)))
            .collect();
        if !warnings.is_empty() {
            message.push_str("\n\n");
            message.push_str(&warnings.join("\n"));
        }
        return message;
    }
    let better = improvement_pct(result)
        .map(|pct| format!(" It lowers the hourly forecast error by {pct:.0}%."))
        .unwrap_or_default();
    format!(
        "Vulpes EMS has a calibrated solar setup ready.{better}\n\n\
         Review and apply it under Settings → Devices & services → Vulpes EMS → Configure → \
         Solar arrays → Apply calibration."
    )
}

/// The recorder statistics a calibration run needs: each array's string
/// power sensor (hourly mean W) and the solar energy statistics (hourly kWh).
pub fn calibration_inputs(cfg: &Cfg) -> Value {
    let planes = planes_of(cfg);
    if planes.is_empty() {
        return json!({"error": "no solar arrays configured"});
    }
    let mut power: Vec<String> = planes
        .iter()
        .filter_map(|p| {
            p.get(CONF_POWER_ENTITY)
                .filter(|v| truthy(Some(v)))
                .and_then(Value::as_str)
        })
        .map(String::from)
        .collect();
    power.sort();
    power.dedup();
    json!({"power_ids": power, "energy_ids": str_list(cfg, CONF_SOLAR_ENERGY)})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn directions_and_degrees() {
        assert_eq!(azimuth_of(&json!("s")).unwrap(), 180.0);
        assert_eq!(azimuth_of(&json!("NW")).unwrap(), 315.0);
        assert_eq!(azimuth_of(&json!("200")).unwrap(), 200.0);
        assert_eq!(azimuth_of(&json!(" 95° ")).unwrap(), 95.0);
        assert_eq!(azimuth_of(&json!(360)).unwrap(), 0.0);
        assert_eq!(azimuth_of(&json!(200.0)).unwrap(), 200.0);
        for bad in ["up", "-5", "361", ""] {
            assert!(azimuth_of(&json!(bad)).is_err(), "{bad}");
        }
        assert_eq!(direction_of(90.0), "e");
        assert_eq!(direction_of(450.0), "e");
        assert_eq!(direction_of(97.5), "97.5");
        assert_eq!(direction_of(-90.0), "w");
        assert_eq!(direction_of(123.456789), "123.457");
        assert_eq!(direction_of(200.0), "200");
    }

    #[test]
    fn python_g_format() {
        assert_eq!(py_g(97.5), "97.5");
        assert_eq!(py_g(0.5), "0.5");
        assert_eq!(py_g(0.00001), "1e-05");
        assert_eq!(py_g(1234567.0), "1.23457e+06");
    }

    #[test]
    fn plane_from_array_shares_the_inverter_and_keeps_extra_keys() {
        let array =
            obj(json!({"direction": "e", "tilt": 30, "kwp": 3.2, "power_entity": "sensor.pv1_power"}));
        let existing = obj(json!({"gamma": -0.0035, "power_entity": "sensor.old"}));
        let plane = plane_from_array(&array, 4000.0, Some(&existing)).unwrap();
        assert_eq!(
            plane,
            json!({"azimuth": 90.0, "tilt": 30.0, "pdc0": 3200.0, "ac_max": 4000.0, "inverter": 0,
                   "gamma": -0.0035, "power_entity": "sensor.pv1_power"})
        );
        // Clearing the sensor removes it.
        let cleared = plane_from_array(
            &obj(json!({"direction": "e", "tilt": 30, "kwp": 3.2})),
            4000.0,
            plane.as_object(),
        )
        .unwrap();
        assert!(cleared.get("power_entity").is_none());
        // A bad direction or a missing field is an error.
        assert!(
            plane_from_array(
                &obj(json!({"direction": "up", "tilt": 30, "kwp": 3})),
                4000.0,
                None
            )
            .is_err()
        );
        assert!(plane_from_array(&obj(json!({"direction": "e", "kwp": 3})), 4000.0, None).is_err());
    }

    #[test]
    fn form_round_trip() {
        let arrays = vec![
            json!({"direction": "e", "tilt": 25.0, "kwp": 3.2, "power_entity": "sensor.pv1_power"}),
            json!({"direction": "275", "tilt": 25.0, "kwp": 2.4}),
        ];
        let planes: Vec<Value> = arrays
            .iter()
            .map(|a| plane_from_array(a.as_object().unwrap(), 3600.0, None).unwrap())
            .collect();
        assert!(fits_form(&planes));
        let (count, inverter_w, prefill) = form_defaults(&planes).unwrap();
        assert_eq!((count, inverter_w), (2, 3600.0));
        assert_eq!(prefill, arrays);
        let again: Vec<Value> = prefill
            .iter()
            .zip(&planes)
            .map(|(a, p)| plane_from_array(a.as_object().unwrap(), inverter_w, p.as_object()).unwrap())
            .collect();
        assert_eq!(again, planes);
    }

    #[test]
    fn which_setups_the_form_can_edit() {
        let single = vec![json!({"azimuth": 180, "tilt": 35, "pdc0": 3000, "ac_max": 2800})];
        assert!(fits_form(&single));
        // Pasted microinverter planes: one inverter each, so not form-editable.
        let micro: Vec<Value> = (0..3)
            .map(|_| json!({"azimuth": 180, "tilt": 35, "pdc0": 400, "ac_max": 300}))
            .collect();
        assert!(!fits_form(&micro));
        assert_eq!(form_defaults(&micro).unwrap(), (1, DEFAULT_INVERTER_W, vec![]));
        let shared: Vec<Value> = micro
            .iter()
            .map(|p| {
                let mut p = p.clone();
                p["inverter"] = json!(0);
                p
            })
            .collect();
        assert!(fits_form(&shared));
        let mut uneven = shared.clone();
        uneven[1]["ac_max"] = json!(500);
        assert!(!fits_form(&uneven[..2]));
        let many: Vec<Value> = shared.iter().chain(&shared).cloned().collect();
        assert!(!fits_form(&many), "more than MAX_ARRAYS");
        assert!(!fits_form(&[]));
    }

    #[test]
    fn engine_planes_drop_local_keys() {
        let planes = vec![
            json!({"azimuth": 90, "tilt": 30, "pdc0": 3000, "ac_max": 4000, "power_entity": "sensor.x"}),
        ];
        assert_eq!(
            engine_planes(&planes),
            [json!({"azimuth": 90, "tilt": 30, "pdc0": 3000, "ac_max": 4000})]
        );
    }

    #[test]
    fn parse_planes_lists_and_cli_output() {
        let list = r#"[{"azimuth": 180, "tilt": 35, "pdc0": 3000, "ac_max": 2800}]"#;
        assert_eq!(parse_planes(list).unwrap().len(), 1);
        // calibrate-solar output keeps the file order of its entries.
        let cli = r#"{"west": {"params": {"azimuth": 270, "tilt": 30, "pdc0": 1, "ac_max": 1}},
                      "east": {"azimuth": 90, "tilt": 30, "pdc0": 1, "ac_max": 1}}"#;
        let planes = parse_planes(cli).unwrap();
        assert_eq!(
            (planes[0]["azimuth"].as_i64(), planes[1]["azimuth"].as_i64()),
            (Some(270), Some(90))
        );
        for bad in [
            "[]",
            "{}",
            "not json",
            "3",
            r#"[{"azimuth": 1}]"#,
            r#"[{"azimuth": "x", "tilt": 1, "pdc0": 1, "ac_max": 1}]"#,
            r#"{"a": 1}"#,
        ] {
            assert!(parse_planes(bad).is_err(), "{bad}");
        }
    }

    fn result() -> Value {
        json!({
            "planes": [
                {"azimuth": 95.4, "tilt": 24.6, "pdc0": 3212.7, "ac_max": 4000, "inverter": 0},
                {"azimuth": 270.0, "tilt": 30.0, "pdc0": 3000.0, "ac_max": 4000, "inverter": 0}
            ],
            "fits": [{"method": "string", "hours": 120, "rmse_w": 40.0}, {"method": "unchanged", "hours": 0, "rmse_w": 0.0}],
            "score_current": {"hours": 300, "mae_w": 150.0, "rmse_w": 200.0, "bias_w": 50.0},
            "score_proposed": {"hours": 300, "mae_w": 100.0, "rmse_w": 150.0, "bias_w": -5.0},
            "held_out": true,
            "warnings": ["planes without a string power sensor are kept as entered"]
        })
    }

    fn planes() -> Vec<Value> {
        vec![
            json!({"azimuth": 90.0, "tilt": 30.0, "pdc0": 3000.0, "ac_max": 4000.0, "inverter": 0, "power_entity": "sensor.pv1"}),
            json!({"azimuth": 270.0, "tilt": 30.0, "pdc0": 3000.0, "ac_max": 4000.0, "inverter": 0}),
        ]
    }

    #[test]
    fn apply_proposal_keeps_sensors_and_limits() {
        let applied = apply_proposal(&planes(), &result()).unwrap();
        assert_eq!(
            applied[0],
            json!({"azimuth": 95.4, "tilt": 24.6, "pdc0": 3213, "ac_max": 4000.0, "inverter": 0, "power_entity": "sensor.pv1"})
        );
        assert!(py_eq(&applied[1], &planes()[1]));
        assert!(apply_proposal(&planes()[..1], &result()).is_err());
    }

    #[test]
    fn changes_and_improvement() {
        assert!(has_changes(&result(), &planes()));
        let applied = apply_proposal(&planes(), &result()).unwrap();
        assert!(!has_changes(&result(), &applied), "rounding is not a change");
        assert!(!has_changes(&result(), &planes()[..1]));
        let north = vec![json!({"azimuth": 359.8, "tilt": 30.0, "pdc0": 3000.0})];
        assert!(
            !has_changes(
                &json!({"planes": [{"azimuth": 0.1, "tilt": 30.0, "pdc0": 3000.0}]}),
                &north
            ),
            "wraps around north"
        );
        assert_eq!(improvement_pct(&result()), Some(25.0));
        assert_eq!(
            improvement_pct(&json!({"score_current": null, "score_proposed": null})),
            None
        );
        assert_eq!(
            improvement_pct(&json!({"score_current": {"rmse_w": 0.0}, "score_proposed": {"rmse_w": 0.0}})),
            None
        );
    }

    #[test]
    fn summary_lists_changes_scores_and_warnings() {
        let text = summarize(&planes(), &result());
        assert!(
            text.contains(
                "Array 1: 90° / 30° / 3.00 kWp → **95° / 25° / 3.21 kWp** (string, 120 clear hours)"
            ),
            "{text}"
        );
        assert!(text.contains("Array 2:") && text.contains("(unchanged, 0 clear hours)"));
        assert!(
            text.contains("held-out weeks: 200 W → **150 W** (bias +50 → -5 W)"),
            "{text}"
        );
        assert!(text.contains("⚠ planes without a string power sensor"));
        let mut same = result();
        same["held_out"] = json!(false);
        assert!(summarize(&planes(), &same).contains("same data"));
        let mut unscored = result();
        unscored["score_current"] = Value::Null;
        assert!(!summarize(&planes(), &unscored).contains("RMSE"));
    }

    #[test]
    fn statuses() {
        let s = |running, error, applied, proposal, result, stored| {
            calibration_status(running, error, applied, proposal, result, stored)
        };
        assert_eq!(s(false, false, false, false, false, false), "not_run");
        assert_eq!(s(false, false, false, false, false, true), "stale");
        assert_eq!(s(false, false, false, false, true, true), "done");
        assert_eq!(s(false, false, true, false, true, true), "applied");
        // A new run on the applied planes proposes more changes: there is something to apply again.
        assert_eq!(s(false, false, true, true, true, true), "done");
        assert_eq!(s(false, true, false, true, false, false), "failed");
        assert_eq!(s(true, true, false, false, false, false), "running");
    }

    fn cfg_with(planes: Vec<Value>, setup: &str) -> Cfg {
        obj(json!({"planes": planes, "solar_setup": setup, "solar_energy": ["sensor.pv_energy"]}))
    }

    #[test]
    fn view_of_a_fresh_proposal_then_after_applying_it() {
        let c = cfg_with(planes(), "form");
        let stored = obj(json!({"result": result(), "based_on": engine_planes(&planes()), "auto_ran": true}));
        let v = calibration_view(&c, &stored, false, false);
        assert_eq!(
            (v["status"].as_str(), v["applied"].as_bool()),
            (Some("done"), Some(false))
        );
        assert_eq!(v["proposal"], result());
        assert_eq!(v["improvement"], json!(25.0));
        assert_eq!(v["auto_run"], json!(false));
        // Apply: the planes change and the stored copy says so.
        let applied = apply_proposal(&planes(), &result()).unwrap();
        let mut after = stored.clone();
        after.insert("applied".into(), Value::Array(engine_planes(&applied)));
        let v = calibration_view(&cfg_with(applied, "form"), &after, false, false);
        assert_eq!(v["status"], "applied");
        assert!(v["proposal"].is_null());
        assert_eq!(v["result"], result());
        assert_eq!(v["improvement"], json!(25.0));
    }

    #[test]
    fn view_of_a_result_for_other_planes_and_of_nothing() {
        let mut moved = planes();
        moved[1]["tilt"] = json!(10.0);
        let stored = obj(json!({"result": result(), "based_on": engine_planes(&planes())}));
        let v = calibration_view(&cfg_with(moved, "json"), &stored, false, false);
        assert_eq!(v["status"], "stale");
        assert!(v["result"].is_null() && v["improvement"].is_null());
        assert_eq!(
            v["auto_run"],
            json!(false),
            "JSON setups calibrate only when asked"
        );
        let v = calibration_view(&cfg_with(planes(), "form"), &Map::new(), true, false);
        assert_eq!(
            (v["status"].as_str(), v["auto_run"].as_bool()),
            (Some("running"), Some(true))
        );
        // A result that changes nothing improves nothing.
        let same = obj(
            json!({"result": {"planes": engine_planes(&planes())}, "based_on": engine_planes(&planes())}),
        );
        assert_eq!(
            calibration_view(&cfg_with(planes(), "form"), &same, false, false)["improvement"],
            json!(0.0)
        );
    }

    #[test]
    fn messages() {
        let c = cfg_with(planes(), "form");
        let ready = calibration_message(&c, &result());
        assert!(
            ready.starts_with(
                "Vulpes EMS has a calibrated solar setup ready. It lowers the hourly forecast error by 25%."
            ),
            "{ready}"
        );
        let nothing = calibration_message(&c, &json!({"planes": planes(), "warnings": ["w1"]}));
        assert_eq!(nothing, "Solar calibration found nothing to change.\n\n- w1");
    }

    #[test]
    fn inputs_for_a_run() {
        let c = cfg_with(planes(), "form");
        assert_eq!(
            calibration_inputs(&c),
            json!({"power_ids": ["sensor.pv1"], "energy_ids": ["sensor.pv_energy"]})
        );
        assert_eq!(
            calibration_inputs(&Cfg::new()),
            json!({"error": "no solar arrays configured"})
        );
    }
}
