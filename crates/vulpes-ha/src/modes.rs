//! Vulpes's own battery modes and how they relate to a user's setup.
//!
//! Vulpes defines a fixed set of modes. The user picks the ones their system
//! can do and sets their power. An existing mode entity (for example an
//! `input_select`) is optional: its states can be mapped to Vulpes modes to
//! compare the plan with what the house actually does.

use crate::Result;
use crate::cfg::{Cfg, as_f64, num, truthy};
use serde_json::{Map, Value, json};

pub const CONF_POWER_CONTROL: &str = "power_control";
pub const CONF_CHARGE_W: &str = "charge_w";
pub const CONF_DISCHARGE_W: &str = "discharge_w";
pub const CONF_PV_CURTAIL: &str = "pv_curtail";
pub const CONF_EXPORT_BLOCK: &str = "export_block";
pub const CONF_MODE_MAP: &str = "mode_map";

pub const POWER_CONTROLS: [&str; 2] = ["grid_setpoint", "battery_power"];

/// Mode key and whether it is enabled by default for new setups, in form order.
pub const MODES: [(&str, bool); 5] = [
    ("charge", true),       // charge from the grid at a set power
    ("hold", true),         // charge from solar surplus only, never discharge
    ("self_consume", true), // keep the grid at zero
    ("idle", false),        // battery does nothing
    ("discharge", false),   // discharge to the grid at a set power
];

/// Suffix of the variant of a mode with solar switched off.
pub const PV_OFF: &str = ":pv_off";
/// Suffix of the variant of a mode with export blocked (zero export).
pub const NO_EXPORT: &str = ":no_export";
/// Modes that get a zero-export variant: in charge and discharge, blocking
/// export means nothing or contradicts the mode.
const EXPORT_BLOCK_MODES: [&str; 3] = ["hold", "self_consume", "idle"];

/// The setting holding the power of a powered mode.
pub fn power_key(mode: &str) -> Option<&'static str> {
    match mode {
        "charge" => Some(CONF_CHARGE_W),
        "discharge" => Some(CONF_DISCHARGE_W),
        _ => None,
    }
}

pub fn power_control(cfg: &Cfg) -> &str {
    cfg.get(CONF_POWER_CONTROL)
        .and_then(Value::as_str)
        .unwrap_or("grid_setpoint")
}

/// The enabled modes, in form order. A mode without a setting uses its default.
pub fn enabled_modes(cfg: &Cfg) -> Vec<&'static str> {
    MODES
        .iter()
        .filter(|(m, default)| cfg.get(*m).map_or(*default, |v| truthy(Some(v))))
        .map(|(m, _)| *m)
        .collect()
}

/// Mode definitions for the planning engine (`ModeDef` JSON).
pub fn engine_modes(cfg: &Cfg) -> Result<Vec<Value>> {
    let control = power_control(cfg);
    let mut out = Vec::new();
    for key in enabled_modes(cfg) {
        let behaviour = match power_key(key) {
            Some(power) => {
                let sign = if key == "charge" { 1.0 } else { -1.0 };
                json!({"type": control, "w": sign * num(cfg, power)?.abs()})
            }
            None if key == "hold" => json!({"type": "no_discharge"}),
            None => json!({"type": key}),
        };
        let def = |name: String, pv_curtail: bool, no_export: bool| json!({"name": name, "behaviour": behaviour, "pv_curtail": pv_curtail, "no_export": no_export});
        out.push(def(key.to_string(), false, false));
        if truthy(cfg.get(CONF_PV_CURTAIL)) {
            out.push(def(format!("{key}{PV_OFF}"), true, false));
        }
        if truthy(cfg.get(CONF_EXPORT_BLOCK)) && EXPORT_BLOCK_MODES.contains(&key) {
            out.push(def(format!("{key}{NO_EXPORT}"), false, true));
        }
    }
    Ok(out)
}

/// Engine mode name → (Vulpes mode, solar off, export blocked).
pub fn split_mode(name: &str) -> (&str, bool, bool) {
    if let Some(base) = name.strip_suffix(PV_OFF) {
        (base, true, false)
    } else if let Some(base) = name.strip_suffix(NO_EXPORT) {
        (base, false, true)
    } else {
        (name, false, false)
    }
}

/// Vulpes mode, solar off, export blocked → engine mode name.
pub fn join_mode(mode: &str, pv_off: bool, no_export: bool) -> String {
    if pv_off {
        format!("{mode}{PV_OFF}")
    } else if no_export {
        format!("{mode}{NO_EXPORT}")
    } else {
        mode.to_string()
    }
}

/// The Vulpes mode a state of the user's mode entity corresponds to. When
/// several modes map to the same state, the first in form order wins.
pub fn mode_of_state(cfg: &Cfg, state: &str) -> Option<String> {
    let map = cfg.get(CONF_MODE_MAP).and_then(Value::as_object)?;
    let known = MODES.iter().map(|(m, _)| *m);
    let mut keys: Vec<&str> = known.filter(|m| map.contains_key(*m)).collect();
    let mut rest: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|k| !keys.contains(k))
        .collect();
    rest.sort_unstable();
    keys.extend(rest);
    keys.into_iter()
        .find(|k| map.get(*k).and_then(Value::as_str) == Some(state))
        .map(String::from)
}

/// Whether the user's mode entity agrees with the planned mode: `None` when
/// the entity's state is unknown. A state that maps to no mode disagrees.
pub fn agrees(cfg: &Cfg, state: Option<&str>, mode_now: &str) -> Option<bool> {
    let state = state.filter(|s| !matches!(*s, "unknown" | "unavailable"))?;
    Some(mode_of_state(cfg, state).as_deref() == Some(mode_now))
}

/// The comparison mapping from the compare step: enabled modes with a state picked.
pub fn mode_map(cfg: &Cfg, input: &Map<String, Value>) -> Map<String, Value> {
    enabled_modes(cfg)
        .into_iter()
        .filter_map(|m| {
            input
                .get(m)
                .filter(|v| truthy(Some(v)))
                .map(|v| (m.to_string(), v.clone()))
        })
        .collect()
}

/// A mode's set power as the script gets it: unsigned and rounded, 0 for
/// modes without a power or without the setting.
pub fn script_power(cfg: &Cfg, mode: &str) -> i64 {
    power_key(mode)
        .and_then(|k| cfg.get(k))
        .filter(|v| !v.is_null())
        .and_then(as_f64)
        .map_or(0, |w| w.abs().round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(v: Value) -> Cfg {
        v.as_object().unwrap().clone()
    }

    fn names(c: &Cfg) -> Vec<String> {
        engine_modes(c)
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn defaults_for_a_new_setup() {
        assert_eq!(enabled_modes(&Cfg::new()), ["charge", "hold", "self_consume"]);
    }

    #[test]
    fn grid_setpoint_signs() {
        let c = cfg(json!({"charge": true, "discharge": true, "charge_w": 2500, "discharge_w": -2800}));
        let modes = engine_modes(&c).unwrap();
        let by = |n: &str| modes.iter().find(|m| m["name"] == n).unwrap().clone();
        assert_eq!(
            by("charge")["behaviour"],
            json!({"type": "grid_setpoint", "w": 2500.0})
        );
        assert_eq!(
            by("discharge")["behaviour"],
            json!({"type": "grid_setpoint", "w": -2800.0})
        );
        assert_eq!(by("hold")["behaviour"], json!({"type": "no_discharge"}));
        assert_eq!(by("self_consume")["behaviour"], json!({"type": "self_consume"}));
        assert!(!names(&c).contains(&"idle".to_string()));
    }

    #[test]
    fn battery_power_and_pv_off_variants() {
        let c = cfg(json!({
            "power_control": "battery_power", "hold": false, "self_consume": false,
            "charge_w": 2000, "pv_curtail": true
        }));
        let out = engine_modes(&c).unwrap();
        assert_eq!(names(&c), ["charge", "charge:pv_off"]);
        assert_eq!(
            out[1],
            json!({
                "name": "charge:pv_off",
                "behaviour": {"type": "battery_power", "w": 2000.0},
                "pv_curtail": true,
                "no_export": false
            })
        );
        assert_eq!(split_mode("charge:pv_off"), ("charge", true, false));
        assert_eq!(split_mode("hold"), ("hold", false, false));
    }

    #[test]
    fn no_export_variants_only_where_export_can_be_blocked() {
        let c = cfg(json!({
            "idle": true, "discharge": true, "charge_w": 2000, "discharge_w": 2000, "export_block": true
        }));
        assert_eq!(
            names(&c),
            [
                "charge",
                "hold",
                "hold:no_export",
                "self_consume",
                "self_consume:no_export",
                "idle",
                "idle:no_export",
                "discharge"
            ]
        );
        let variant = engine_modes(&c)
            .unwrap()
            .into_iter()
            .find(|m| m["name"] == "self_consume:no_export")
            .unwrap();
        assert_eq!(
            variant,
            json!({
                "name": "self_consume:no_export",
                "behaviour": {"type": "self_consume"},
                "pv_curtail": false,
                "no_export": true
            })
        );
        assert_eq!(
            split_mode("self_consume:no_export"),
            ("self_consume", false, true)
        );
        let mut off = c.clone();
        off.insert("export_block".into(), json!(false));
        assert!(
            engine_modes(&off)
                .unwrap()
                .iter()
                .all(|m| m["no_export"] == false)
        );
    }

    #[test]
    fn a_powered_mode_needs_its_power() {
        let c = cfg(json!({"discharge": true}));
        assert!(engine_modes(&c).unwrap_err().to_string().contains("charge_w"));
    }

    #[test]
    fn join_is_the_inverse_of_split() {
        for name in ["hold", "charge:pv_off", "idle:no_export"] {
            let (m, p, n) = split_mode(name);
            assert_eq!(join_mode(m, p, n), name);
        }
    }

    #[test]
    fn mode_of_state_and_agreement() {
        let c = cfg(json!({"mode_map": {"self_consume": "Net Zero", "hold": "Grid", "discharge": "Grid"}}));
        assert_eq!(mode_of_state(&c, "Net Zero").as_deref(), Some("self_consume"));
        // Two modes on one state: the first in form order (hold before discharge).
        assert_eq!(mode_of_state(&c, "Grid").as_deref(), Some("hold"));
        assert_eq!(mode_of_state(&c, "Other"), None);
        assert_eq!(mode_of_state(&Cfg::new(), "Grid"), None);
        assert_eq!(agrees(&c, Some("Grid"), "hold"), Some(true));
        assert_eq!(agrees(&c, Some("Grid"), "charge"), Some(false));
        assert_eq!(agrees(&c, Some("Unmapped"), "charge"), Some(false));
        assert_eq!(agrees(&c, Some("unavailable"), "hold"), None);
        assert_eq!(agrees(&c, Some("unknown"), "hold"), None);
        assert_eq!(agrees(&c, None, "hold"), None);
    }

    #[test]
    fn compare_step_mapping_keeps_enabled_modes_with_a_choice() {
        let c = cfg(json!({"idle": false}));
        let input = cfg(json!({"charge": "Charging", "hold": "", "idle": "Idle", "bogus": "X"}));
        assert_eq!(Value::Object(mode_map(&c, &input)), json!({"charge": "Charging"}));
    }

    #[test]
    fn script_power_is_unsigned_and_rounded() {
        let c = cfg(json!({"charge_w": 3000, "discharge_w": -2500.4, "x": null}));
        assert_eq!(script_power(&c, "charge"), 3000);
        assert_eq!(script_power(&c, "discharge"), 2500);
        assert_eq!(script_power(&c, "hold"), 0);
        assert_eq!(script_power(&Cfg::new(), "charge"), 0);
        assert_eq!(script_power(&cfg(json!({"charge_w": null})), "charge"), 0);
    }
}
