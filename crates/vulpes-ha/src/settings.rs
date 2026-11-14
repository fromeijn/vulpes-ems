//! Settings: the current copy, migrations of old config entries, and the
//! input checks of the config and options flows.

use crate::cfg::{Cfg, as_f64, num, num_or, str_list, truthy};
use crate::inputs::{EnergyRows, load_groups, load_series, parse_price_attributes, soc_value};
use crate::modes::{
    CONF_CHARGE_W, CONF_DISCHARGE_W, CONF_MODE_MAP, CONF_POWER_CONTROL, CONF_PV_CURTAIL, MODES,
    POWER_CONTROLS, enabled_modes, power_control,
};
use crate::{Error, Result};
use chrono_tz::Tz;
use serde_json::{Map, Value, json};

pub const CONF_SOC_ENTITY: &str = "soc_entity";
pub const CONF_PRICE_ENTITY: &str = "price_entity";
pub const CONF_GRID_IMPORT: &str = "grid_import";
pub const CONF_GRID_EXPORT: &str = "grid_export";
pub const CONF_SOLAR_ENERGY: &str = "solar_energy";
pub const CONF_BATTERY_CHARGE: &str = "battery_charge";
pub const CONF_BATTERY_DISCHARGE: &str = "battery_discharge";
pub const CONF_MIN_SOC: &str = "min_soc";
pub const CONF_MAX_SOC: &str = "max_soc";
pub const CONF_TARIFF_A: &str = "tariff_a";
pub const CONF_TARIFF_B: &str = "tariff_b";
pub const CONF_EXPORT_A: &str = "export_a";
pub const CONF_EXPORT_B: &str = "export_b";
const CONF_MODES_V1: &str = "modes";
const CONF_NET_METERING_V2: &str = "net_metering";

/// The config entry version the current settings format has.
pub const CONFIG_VERSION: u32 = 3;

/// The energy statistics fields, in form order.
pub const ENERGY_KEYS: [&str; 5] = [
    CONF_GRID_IMPORT,
    CONF_GRID_EXPORT,
    CONF_SOLAR_ENERGY,
    CONF_BATTERY_CHARGE,
    CONF_BATTERY_DISCHARGE,
];

/// Every error key the checks here can return; each needs a string in
/// `strings.json` (checked by `tests/test_strings.py`).
pub const ERROR_KEYS: [&str; 12] = [
    "soc_unavailable",
    "no_prices",
    "grid_import_required",
    "duplicate_statistic",
    "no_statistics",
    "no_complete_hours",
    "soc_range",
    "export_needs_import_a",
    "charge_w_too_high",
    "discharge_w_too_high",
    "no_modes",
    "no_mode_options",
];

/// Current settings: the options flow saves a complete copy, which replaces
/// the setup data. Merging them would bring cleared fields back.
pub fn settings(data: &Cfg, options: &Cfg) -> Cfg {
    if options.is_empty() {
        data.clone()
    } else {
        options.clone()
    }
}

/// Bring settings of config entry `version` to [`CONFIG_VERSION`].
pub fn migrate(cfg: &Cfg, version: u32) -> Result<Cfg> {
    if version > CONFIG_VERSION {
        return Err(Error::Message(format!(
            "settings version {version} is newer than this version of Vulpes ({CONFIG_VERSION})"
        )));
    }
    let mut out = cfg.clone();
    if version < 2 {
        out = migrate_v1(&out)?;
    }
    if version < 3 {
        out = migrate_v2(&out);
    }
    Ok(out)
}

/// Turn 0.1.0 settings (user-named modes with behaviours) into Vulpes modes.
/// Each old mode is matched to a Vulpes mode by its behaviour; its name
/// becomes the comparison mapping, so the shadow comparison keeps working.
pub fn migrate_v1(cfg: &Cfg) -> Result<Cfg> {
    let Some(old_modes) = cfg.get(CONF_MODES_V1) else {
        return Ok(cfg.clone());
    };
    let mut new: Cfg = cfg
        .iter()
        .filter(|(k, _)| *k != CONF_MODES_V1)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (m, _) in MODES {
        new.insert(m.into(), json!(false));
    }
    let mut mapping = Map::new();
    for mode in old_modes.as_array().into_iter().flatten() {
        let b = mode.get("behaviour").cloned().unwrap_or_else(|| json!({}));
        let kind = b.get("type").and_then(Value::as_str).unwrap_or("");
        let w = b.get("w").and_then(as_f64).unwrap_or(0.0);
        let key = if POWER_CONTROLS.contains(&kind) {
            let key = if w >= 0.0 { "charge" } else { "discharge" };
            new.insert(CONF_POWER_CONTROL.into(), json!(kind));
            new.insert(
                if key == "charge" {
                    CONF_CHARGE_W
                } else {
                    CONF_DISCHARGE_W
                }
                .into(),
                json!(w.abs()),
            );
            key
        } else {
            match kind {
                "no_discharge" => "hold",
                "self_consume" => "self_consume",
                "idle" => "idle",
                _ => continue,
            }
        };
        if mapping.contains_key(key) {
            continue; // two old modes with the same behaviour: keep the first
        }
        let name = mode
            .get("name")
            .cloned()
            .ok_or_else(|| Error::Message("a version 1 mode has no name".into()))?;
        new.insert(key.into(), json!(true));
        mapping.insert(key.into(), name);
        if truthy(mode.get("pv_curtail")) {
            new.insert(CONF_PV_CURTAIL.into(), json!(true));
        }
    }
    new.insert(CONF_MODE_MAP.into(), Value::Object(mapping));
    new.entry(CONF_CHARGE_W).or_insert(json!(3000.0));
    new.entry(CONF_DISCHARGE_W).or_insert(json!(3000.0));
    Ok(new)
}

/// Drop the net metering switch of settings version 2. Export is always
/// `export a × wholesale + export b`. With the switch on, export was paid at
/// the import price: the import tariff becomes the export tariff, so the plan
/// stays the same.
pub fn migrate_v2(cfg: &Cfg) -> Cfg {
    let Some(switch) = cfg.get(CONF_NET_METERING_V2) else {
        return cfg.clone();
    };
    let mut new: Cfg = cfg
        .iter()
        .filter(|(k, _)| *k != CONF_NET_METERING_V2)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if truthy(Some(switch)) {
        if let (Some(a), Some(b)) = (cfg.get(CONF_TARIFF_A), cfg.get(CONF_TARIFF_B)) {
            new.insert(CONF_EXPORT_A.into(), a.clone());
            new.insert(CONF_EXPORT_B.into(), b.clone());
        }
    }
    new
}

pub type Errors = Map<String, Value>;

fn error(field: &str, key: &str) -> Errors {
    let mut e = Map::new();
    e.insert(field.into(), json!(key));
    e
}

/// Errors for the battery step.
pub fn check_battery(cfg: &Cfg) -> Result<Errors> {
    Ok(if num(cfg, CONF_MIN_SOC)? >= num(cfg, CONF_MAX_SOC)? {
        error("base", "soc_range")
    } else {
        Errors::new()
    })
}

/// Errors for the tariff step. The engine derives the wholesale price from
/// the import price, which a fixed import price (a = 0) doesn't allow. The
/// export price then cannot follow the wholesale price either, so Export a
/// has to be 0 too.
pub fn check_tariff(cfg: &Cfg) -> Result<Errors> {
    Ok(
        if num(cfg, CONF_TARIFF_A)? == 0.0 && num(cfg, CONF_EXPORT_A)? != 0.0 {
            error(CONF_EXPORT_A, "export_needs_import_a")
        } else {
            Errors::new()
        },
    )
}

/// Errors for the modes step: at least one mode, set powers within the
/// battery or grid limits, and a mode entity with options to compare with.
///
/// With battery power control the battery runs at the set power, so it must
/// fit the battery. With a grid setpoint the house load sits between grid and
/// battery, so the setpoint only has to fit the grid connection.
/// `mode_options` are the options of the chosen mode entity (`None` without one).
pub fn check_modes(cfg: &Cfg, mode_options: Option<&[Value]>) -> Result<Errors> {
    let enabled = enabled_modes(cfg);
    if enabled.is_empty() {
        return Ok(error("base", "no_modes"));
    }
    let battery = power_control(cfg) == "battery_power";
    let mut errors = Errors::new();
    for mode in enabled {
        let (power, limit) = match mode {
            "charge" => (
                CONF_CHARGE_W,
                if battery { "max_charge_w" } else { "max_import_w" },
            ),
            "discharge" => (
                CONF_DISCHARGE_W,
                if battery {
                    "max_discharge_w"
                } else {
                    "max_export_w"
                },
            ),
            _ => continue,
        };
        if num_or(cfg, power, 0.0) > num(cfg, limit)? {
            errors.insert(power.into(), json!(format!("{power}_too_high")));
        }
    }
    if errors.is_empty() && mode_options.is_some_and(<[Value]>::is_empty) {
        errors.insert("mode_entity".into(), json!("no_mode_options"));
    }
    Ok(errors)
}

/// All selected energy statistics, in form order, without repeats.
pub fn energy_ids(cfg: &Cfg) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for k in ENERGY_KEYS {
        for s in str_list(cfg, k) {
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

/// Statistics chosen more than once across the energy fields, in first-seen order.
pub fn duplicate_statistics(cfg: &Cfg) -> Vec<String> {
    let all: Vec<String> = ENERGY_KEYS.iter().flat_map(|k| str_list(cfg, k)).collect();
    let mut out: Vec<String> = Vec::new();
    for s in &all {
        if all.iter().filter(|x| *x == s).count() > 1 && !out.contains(s) {
            out.push(s.clone());
        }
    }
    out
}

/// What the source step needs to know to check the sources.
pub struct SourceInputs<'a> {
    pub soc_state: Option<&'a str>,
    /// The price entity's attributes as ordered `(name, value)` pairs, or `None` without the entity.
    pub price_attributes: Option<&'a [(String, Value)]>,
    pub price_unit: Option<&'a str>,
    pub tz: Tz,
    /// Hourly energy changes per statistic, once fetched.
    pub stats: Option<&'a EnergyRows>,
}

/// The outcome of checking the source step.
#[derive(Debug, PartialEq)]
pub struct SourceCheck {
    pub errors: Errors,
    pub statistic: String,
    /// Statistics to fetch before calling again with `stats`; empty when done.
    pub fetch: Vec<String>,
}

/// Check the source step: the SOC sensor, the price entity, and the energy
/// statistics. The statistics need a recorder query, so a first call without
/// `stats` returns the ids to fetch, and a second call with them finishes.
pub fn check_sources(cfg: &Cfg, inp: &SourceInputs<'_>) -> SourceCheck {
    let mut errors = Errors::new();
    let done = |errors: Errors, statistic: String| SourceCheck {
        errors,
        statistic,
        fetch: Vec::new(),
    };
    if soc_value(inp.soc_state).is_none() {
        errors.insert(CONF_SOC_ENTITY.into(), json!("soc_unavailable"));
    }
    let prices = inp
        .price_attributes
        .map(|a| parse_price_attributes(a, inp.price_unit, inp.tz))
        .unwrap_or_default();
    if prices.is_empty() {
        errors.insert(CONF_PRICE_ENTITY.into(), json!("no_prices"));
    }
    if str_list(cfg, CONF_GRID_IMPORT).is_empty() {
        errors.insert(CONF_GRID_IMPORT.into(), json!("grid_import_required"));
        return done(errors, String::new());
    }
    let duplicates = duplicate_statistics(cfg);
    if !duplicates.is_empty() {
        errors.insert("base".into(), json!("duplicate_statistic"));
        return done(errors, duplicates.join(", "));
    }
    let ids = energy_ids(cfg);
    let Some(stats) = inp.stats else {
        return SourceCheck {
            errors,
            statistic: String::new(),
            fetch: ids,
        };
    };
    let empty: Vec<&String> = ids
        .iter()
        .filter(|i| stats.get(*i).is_none_or(|rows| rows.is_empty()))
        .collect();
    if !empty.is_empty() {
        errors.insert("base".into(), json!("no_statistics"));
        let list: Vec<&str> = empty.iter().map(|s| s.as_str()).collect();
        return done(errors, list.join(", "));
    }
    if load_series(&load_groups(cfg), stats).is_empty() {
        errors.insert("base".into(), json!("no_complete_hours"));
    }
    done(errors, String::new())
}

/// Source step defaults from the Energy Dashboard preferences. Energy
/// statistics are lists of `sensor.` statistic ids (only entity-based
/// statistics can be chosen with an entity selector). The price entity is set
/// when the grid sources name exactly one.
pub fn dashboard_defaults(prefs: Option<&Value>) -> Cfg {
    let mut out: Vec<(&str, Vec<String>)> = ENERGY_KEYS.iter().map(|k| (*k, Vec::new())).collect();
    let mut prices: Vec<String> = Vec::new();
    let add = |out: &mut Vec<(&str, Vec<String>)>, key: &str, v: Option<&Value>| {
        if let Some(s) = v.and_then(Value::as_str).filter(|s| !s.is_empty()) {
            out.iter_mut()
                .find(|(k, _)| *k == key)
                .unwrap()
                .1
                .push(s.to_string());
        }
    };
    let sources = prefs
        .and_then(|p| p.get("energy_sources"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let price = |v: Option<&Value>, prices: &mut Vec<String>| {
        if let Some(p) = v.and_then(Value::as_str).filter(|s| !s.is_empty()) {
            prices.push(p.to_string());
        }
    };
    for src in &sources {
        match src.get("type").and_then(Value::as_str) {
            Some("grid") => {
                // Unified format (stat_energy_from/to on the source) or the legacy flow_from/flow_to lists.
                add(&mut out, CONF_GRID_IMPORT, src.get("stat_energy_from"));
                add(&mut out, CONF_GRID_EXPORT, src.get("stat_energy_to"));
                price(src.get("entity_energy_price"), &mut prices);
                for flow in src
                    .get("flow_from")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    add(&mut out, CONF_GRID_IMPORT, flow.get("stat_energy_from"));
                    price(flow.get("entity_energy_price"), &mut prices);
                }
                for flow in src.get("flow_to").and_then(Value::as_array).into_iter().flatten() {
                    add(&mut out, CONF_GRID_EXPORT, flow.get("stat_energy_to"));
                }
            }
            Some("solar") => add(&mut out, CONF_SOLAR_ENERGY, src.get("stat_energy_from")),
            Some("battery") => {
                add(&mut out, CONF_BATTERY_DISCHARGE, src.get("stat_energy_from"));
                add(&mut out, CONF_BATTERY_CHARGE, src.get("stat_energy_to"));
            }
            _ => {}
        }
    }
    let mut defaults: Cfg = out
        .into_iter()
        .map(|(k, v)| {
            let ids: Vec<Value> = v
                .into_iter()
                .filter(|s| s.starts_with("sensor."))
                .map(Value::from)
                .collect();
            (k.to_string(), Value::Array(ids))
        })
        .collect();
    prices.sort();
    prices.dedup();
    if let [only] = prices.as_slice() {
        if only.starts_with("sensor.") {
            defaults.insert(CONF_PRICE_ENTITY.into(), json!(only));
        }
    }
    defaults
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::mode_of_state;

    fn cfg(v: Value) -> Cfg {
        v.as_object().unwrap().clone()
    }

    fn v1() -> Cfg {
        cfg(json!({
            "soc_entity": "sensor.battery_soc",
            "mode_entity": "input_select.ess_mode",
            "modes": [
                {"name": "Charging", "behaviour": {"type": "grid_setpoint", "w": 2500.0}, "pv_curtail": false},
                {"name": "Grid", "behaviour": {"type": "no_discharge"}, "pv_curtail": false},
                {"name": "Net Zero", "behaviour": {"type": "self_consume"}, "pv_curtail": false},
                {"name": "Discharging", "behaviour": {"type": "grid_setpoint", "w": -2800.0}, "pv_curtail": false}
            ]
        }))
    }

    #[test]
    fn migrate_v1_keeps_behaviour_and_comparison() {
        let new = migrate_v1(&v1()).unwrap();
        assert!(!new.contains_key("modes"));
        assert_eq!(new["soc_entity"], "sensor.battery_soc");
        assert_eq!(
            enabled_modes(&new),
            ["charge", "hold", "self_consume", "discharge"]
        );
        assert_eq!(
            (&new["power_control"], &new["charge_w"], &new["discharge_w"]),
            (&json!("grid_setpoint"), &json!(2500.0), &json!(2800.0))
        );
        assert_eq!(
            new["mode_map"],
            json!({"charge": "Charging", "hold": "Grid", "self_consume": "Net Zero", "discharge": "Discharging"})
        );
        assert_eq!(mode_of_state(&new, "Net Zero").as_deref(), Some("self_consume"));
        assert_eq!(mode_of_state(&new, "Something else"), None);
        // Same engine behaviour as before, under Vulpes names.
        let old: Map<String, Value> = v1()["modes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| (m["name"].as_str().unwrap().to_string(), m["behaviour"].clone()))
            .collect();
        for m in crate::modes::engine_modes(&new).unwrap() {
            let name = new["mode_map"][m["name"].as_str().unwrap()].as_str().unwrap();
            assert!(crate::cfg::py_eq(&m["behaviour"], &old[name]), "{m}");
        }
    }

    #[test]
    fn migrate_v1_edge_cases() {
        assert_eq!(migrate_v1(&Cfg::new()).unwrap(), Cfg::new());
        let v2 = migrate_v1(&v1()).unwrap();
        assert_eq!(migrate_v1(&v2).unwrap(), v2);
        // Unknown behaviours are skipped; the first of two equal behaviours wins; pv_curtail carries over.
        let odd = cfg(json!({"modes": [
            {"name": "A", "behaviour": {"type": "weird"}},
            {"name": "B", "behaviour": {"type": "idle"}, "pv_curtail": true},
            {"name": "C", "behaviour": {"type": "idle"}}
        ]}));
        let new = migrate_v1(&odd).unwrap();
        assert_eq!(new["mode_map"], json!({"idle": "B"}));
        assert_eq!(new["pv_curtail"], json!(true));
        assert_eq!(
            (&new["charge_w"], &new["discharge_w"]),
            (&json!(3000.0), &json!(3000.0))
        );
    }

    #[test]
    fn migrate_v2_turns_net_metering_into_the_export_tariff() {
        let v2 = cfg(
            json!({"tariff_a": 1.21, "tariff_b": 0.13, "export_a": 1.1, "export_b": -0.02, "charge": true}),
        );
        let mut on = v2.clone();
        on.insert("net_metering".into(), json!(true));
        let mut want = v2.clone();
        want.insert("export_a".into(), json!(1.21));
        want.insert("export_b".into(), json!(0.13));
        assert_eq!(migrate_v2(&on), want);
        let mut off = v2.clone();
        off.insert("net_metering".into(), json!(false));
        assert_eq!(migrate_v2(&off), v2);
    }

    #[test]
    fn migrate_v2_leaves_migrated_and_empty_settings_alone() {
        assert_eq!(migrate_v2(&Cfg::new()), Cfg::new());
        let v3 = cfg(json!({"tariff_a": 1.21, "tariff_b": 0.13, "export_a": 1.0, "export_b": 0.0}));
        assert_eq!(migrate_v2(&v3), v3);
        // A switch without an import tariff (never saved by the flow) is only dropped.
        assert_eq!(
            migrate_v2(&cfg(json!({"net_metering": true, "export_a": 1.0}))),
            cfg(json!({"export_a": 1.0}))
        );
    }

    #[test]
    fn migrate_runs_every_step_from_the_entry_version() {
        let mut v1 = v1();
        v1.insert("net_metering".into(), json!(true));
        v1.insert("tariff_a".into(), json!(1.2));
        v1.insert("tariff_b".into(), json!(0.1));
        let new = migrate(&v1, 1).unwrap();
        assert!(!new.contains_key("modes") && !new.contains_key("net_metering"));
        assert_eq!(new["export_a"], json!(1.2));
        // From version 2 only the second step runs; version 3 is current.
        assert!(migrate(&v1, 2).unwrap().contains_key("modes"));
        assert_eq!(migrate(&v1, 3).unwrap(), v1);
        assert!(migrate(&v1, 4).is_err());
    }

    #[test]
    fn options_replace_data_so_cleared_fields_stay_cleared() {
        let data = cfg(json!({"mode_entity": "input_select.ess_mode", "charge": true}));
        assert_eq!(settings(&data, &Cfg::new()), data);
        assert!(!settings(&data, &cfg(json!({"charge": true}))).contains_key("mode_entity"));
    }

    #[test]
    fn battery_soc_range() {
        assert!(
            check_battery(&cfg(json!({"min_soc": 10, "max_soc": 100})))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            Value::Object(check_battery(&cfg(json!({"min_soc": 50, "max_soc": 50}))).unwrap()),
            json!({"base": "soc_range"})
        );
        assert_eq!(
            Value::Object(check_battery(&cfg(json!({"min_soc": 80, "max_soc": 20}))).unwrap()),
            json!({"base": "soc_range"})
        );
        assert!(check_battery(&Cfg::new()).is_err());
    }

    #[test]
    fn fixed_import_price_needs_fixed_export_price() {
        let check = |a: Value, e: Value| {
            Value::Object(check_tariff(&cfg(json!({"tariff_a": a, "export_a": e}))).unwrap())
        };
        assert_eq!(check(json!(1.21), json!(1.0)), json!({}));
        assert_eq!(check(json!(1.21), json!(0)), json!({}));
        // A fixed price on both sides, as after migrating net metering with Import a = 0.
        assert_eq!(check(json!(0), json!(0.0)), json!({}));
        assert_eq!(
            check(json!(0), json!(1.0)),
            json!({"export_a": "export_needs_import_a"})
        );
        assert_eq!(
            check(json!(0.0), json!(0.01)),
            json!({"export_a": "export_needs_import_a"})
        );
    }

    fn limits(extra: Value) -> Cfg {
        let mut c = cfg(
            json!({"max_charge_w": 3000, "max_discharge_w": 2500, "max_import_w": 10000, "max_export_w": 8000}),
        );
        c.extend(cfg(extra));
        c
    }

    #[test]
    fn battery_power_uses_battery_limits() {
        let base = json!({"power_control": "battery_power", "charge": true, "discharge": true});
        let mut ok = limits(base.clone());
        ok.extend(cfg(json!({"charge_w": 3000, "discharge_w": 2500})));
        assert!(check_modes(&ok, None).unwrap().is_empty());
        let mut high = limits(base);
        high.extend(cfg(json!({"charge_w": 3001, "discharge_w": 2600})));
        assert_eq!(
            Value::Object(check_modes(&high, None).unwrap()),
            json!({"charge_w": "charge_w_too_high", "discharge_w": "discharge_w_too_high"})
        );
    }

    #[test]
    fn grid_setpoint_uses_grid_limits() {
        // Above the battery limits is fine: the house load sits in between.
        let ok = limits(
            json!({"power_control": "grid_setpoint", "charge": true, "discharge": true,
                               "charge_w": 5000, "discharge_w": 5000}),
        );
        assert!(check_modes(&ok, None).unwrap().is_empty());
        let high = limits(
            json!({"power_control": "grid_setpoint", "charge": true, "discharge": true,
                                 "charge_w": 12000, "discharge_w": 5000}),
        );
        assert_eq!(
            Value::Object(check_modes(&high, None).unwrap()),
            json!({"charge_w": "charge_w_too_high"})
        );
    }

    #[test]
    fn disabled_modes_are_not_checked_and_none_enabled_is_an_error() {
        let off = limits(
            json!({"power_control": "battery_power", "charge": false, "discharge": false,
                                "charge_w": 99999, "discharge_w": 99999}),
        );
        assert!(check_modes(&off, None).unwrap().is_empty());
        let none = cfg(json!({"charge": false, "hold": false, "self_consume": false}));
        assert_eq!(
            Value::Object(check_modes(&none, None).unwrap()),
            json!({"base": "no_modes"})
        );
    }

    #[test]
    fn a_mode_entity_needs_options() {
        let c = cfg(json!({"charge": false}));
        assert!(check_modes(&c, None).unwrap().is_empty());
        assert!(check_modes(&c, Some(&[json!("Auto")])).unwrap().is_empty());
        assert_eq!(
            Value::Object(check_modes(&c, Some(&[])).unwrap()),
            json!({"mode_entity": "no_mode_options"})
        );
    }

    #[test]
    fn duplicates_and_ids_across_fields() {
        let c = cfg(json!({
            "grid_import": ["sensor.a", "sensor.b"],
            "grid_export": ["sensor.c"],
            "solar_energy": ["sensor.a"],
            "battery_charge": ["sensor.c", "sensor.c"]
        }));
        assert_eq!(duplicate_statistics(&c), ["sensor.a", "sensor.c"]);
        assert!(
            duplicate_statistics(&cfg(
                json!({"grid_import": ["sensor.a"], "grid_export": ["sensor.b"]})
            ))
            .is_empty()
        );
        assert_eq!(energy_ids(&c), ["sensor.a", "sensor.b", "sensor.c"]);
    }

    // --- source step ---------------------------------------------------------

    const T0: f64 = 1_794_096_000.0;

    fn prices() -> Vec<(String, Value)> {
        vec![("today".into(), json!([{"start": T0, "price": 0.2}]))]
    }

    fn rows(changes: &[Option<f64>]) -> Vec<(Value, Option<f64>)> {
        changes
            .iter()
            .enumerate()
            .map(|(i, c)| (json!(T0 + 3600.0 * i as f64), *c))
            .collect()
    }

    fn check(
        c: &Cfg,
        soc: Option<&str>,
        prices: Option<&[(String, Value)]>,
        stats: Option<&EnergyRows>,
    ) -> SourceCheck {
        check_sources(
            c,
            &SourceInputs {
                soc_state: soc,
                price_attributes: prices,
                price_unit: None,
                tz: Tz::UTC,
                stats,
            },
        )
    }

    #[test]
    fn sources_ask_for_statistics_then_check_them() {
        let c = cfg(json!({"grid_import": ["sensor.i"], "grid_export": ["sensor.e"]}));
        let p = prices();
        let first = check(&c, Some("50"), Some(&p), None);
        assert!(first.errors.is_empty());
        assert_eq!(first.fetch, ["sensor.i", "sensor.e"]);
        let mut stats = EnergyRows::new();
        stats.insert("sensor.i".into(), rows(&[Some(0.5)]));
        stats.insert("sensor.e".into(), rows(&[Some(0.0)]));
        let done = check(&c, Some("50"), Some(&p), Some(&stats));
        assert_eq!(
            done,
            SourceCheck {
                errors: Errors::new(),
                statistic: String::new(),
                fetch: vec![]
            }
        );
    }

    #[test]
    fn source_errors() {
        let c = cfg(json!({"grid_import": ["sensor.i"], "grid_export": ["sensor.e"]}));
        let p = prices();
        let r = check(&c, Some("unavailable"), None, None);
        assert_eq!(r.errors["soc_entity"], "soc_unavailable");
        assert_eq!(r.errors["price_entity"], "no_prices");
        // No grid import: stop there.
        let r = check(&cfg(json!({"grid_import": []})), Some("50"), Some(&p), None);
        assert_eq!(
            Value::Object(r.errors),
            json!({"grid_import": "grid_import_required"})
        );
        assert!(r.fetch.is_empty());
        // A statistic in two fields: stop there and name it.
        let dup = cfg(json!({"grid_import": ["sensor.i"], "solar_energy": ["sensor.i"]}));
        let r = check(&dup, Some("50"), Some(&p), None);
        assert_eq!(
            (r.errors["base"].as_str(), r.statistic.as_str()),
            (Some("duplicate_statistic"), "sensor.i")
        );
        // A statistic without rows, then one without complete hours.
        let mut stats = EnergyRows::new();
        stats.insert("sensor.i".into(), rows(&[Some(0.5)]));
        let r = check(&c, Some("50"), Some(&p), Some(&stats));
        assert_eq!(
            (r.errors["base"].as_str(), r.statistic.as_str()),
            (Some("no_statistics"), "sensor.e")
        );
        stats.insert("sensor.e".into(), rows(&[None, Some(0.1)]));
        let r = check(&c, Some("50"), Some(&p), Some(&stats));
        assert_eq!(r.errors["base"], "no_complete_hours");
    }

    #[test]
    fn every_error_a_check_returns_is_listed() {
        // Failing input for every check; each returned key needs a string in strings.json.
        let mut seen: Vec<String> = Vec::new();
        let mut collect = |e: Errors| seen.extend(e.values().filter_map(Value::as_str).map(String::from));
        collect(check_battery(&cfg(json!({"min_soc": 50, "max_soc": 10}))).unwrap());
        collect(check_tariff(&cfg(json!({"tariff_a": 0, "export_a": 1}))).unwrap());
        collect(
            check_modes(
                &cfg(json!({"charge": false, "hold": false, "self_consume": false})),
                None,
            )
            .unwrap(),
        );
        collect(
            check_modes(
                &limits(json!({"discharge": true, "charge_w": 1e6, "discharge_w": 1e6})),
                None,
            )
            .unwrap(),
        );
        collect(check_modes(&cfg(json!({"charge": false})), Some(&[])).unwrap());
        let p = prices();
        collect(check(&cfg(json!({})), None, None, None).errors);
        collect(
            check(
                &cfg(json!({"grid_import": ["a"], "solar_energy": ["a"]})),
                Some("1"),
                Some(&p),
                None,
            )
            .errors,
        );
        let two = cfg(json!({"grid_import": ["a"], "grid_export": ["b"]}));
        let mut stats = EnergyRows::new();
        stats.insert("a".into(), rows(&[Some(1.0)]));
        collect(check(&two, Some("1"), Some(&p), Some(&stats)).errors);
        stats.insert("b".into(), rows(&[None, Some(1.0)]));
        collect(check(&two, Some("1"), Some(&p), Some(&stats)).errors);
        for key in &seen {
            assert!(
                ERROR_KEYS.contains(&key.as_str()),
                "{key} is missing from ERROR_KEYS"
            );
        }
        for key in [
            "soc_unavailable",
            "no_prices",
            "grid_import_required",
            "duplicate_statistic",
            "no_statistics",
            "no_complete_hours",
            "soc_range",
            "export_needs_import_a",
            "charge_w_too_high",
            "discharge_w_too_high",
            "no_modes",
            "no_mode_options",
        ] {
            assert!(seen.iter().any(|s| s == key), "{key} was not produced");
        }
    }

    // --- Energy Dashboard defaults -------------------------------------------

    #[test]
    fn dashboard_legacy_format() {
        let prefs = json!({"energy_sources": [
            {"type": "grid",
             "flow_from": [
                {"stat_energy_from": "sensor.import_t1", "entity_energy_price": "sensor.price"},
                {"stat_energy_from": "sensor.import_t2", "entity_energy_price": "sensor.price"}],
             "flow_to": [{"stat_energy_to": "sensor.export_t1"}]},
            {"type": "solar", "stat_energy_from": "sensor.pv"},
            {"type": "battery", "stat_energy_from": "sensor.bat_out", "stat_energy_to": "sensor.bat_in"}
        ]});
        let d = dashboard_defaults(Some(&prefs));
        assert_eq!(d["grid_import"], json!(["sensor.import_t1", "sensor.import_t2"]));
        assert_eq!(d["grid_export"], json!(["sensor.export_t1"]));
        assert_eq!(d["solar_energy"], json!(["sensor.pv"]));
        assert_eq!(d["battery_discharge"], json!(["sensor.bat_out"]));
        assert_eq!(d["battery_charge"], json!(["sensor.bat_in"]));
        assert_eq!(d["price_entity"], "sensor.price");
    }

    #[test]
    fn dashboard_unified_format_and_external_statistics() {
        let prefs = json!({"energy_sources": [
            {"type": "grid", "stat_energy_from": "sensor.import", "stat_energy_to": "sensor.export",
             "entity_energy_price": "sensor.price"},
            {"type": "solar", "stat_energy_from": "solar:external"},
            {"type": "gas", "stat_energy_from": "sensor.gas"}
        ]});
        let d = dashboard_defaults(Some(&prefs));
        assert_eq!(d["grid_import"], json!(["sensor.import"]));
        assert_eq!(d["grid_export"], json!(["sensor.export"]));
        assert_eq!(d["solar_energy"], json!([]));
        assert_eq!(d["price_entity"], "sensor.price");
    }

    #[test]
    fn dashboard_price_only_when_unambiguous() {
        let two = json!({"energy_sources": [{"type": "grid", "flow_from": [
            {"stat_energy_from": "sensor.t1", "entity_energy_price": "sensor.price_a"},
            {"stat_energy_from": "sensor.t2", "entity_energy_price": "sensor.price_b"}]}]});
        assert!(!dashboard_defaults(Some(&two)).contains_key("price_entity"));
        let number =
            json!({"energy_sources": [{"type": "grid", "flow_from": [{"stat_energy_from": "sensor.t1"}]}]});
        assert!(!dashboard_defaults(Some(&number)).contains_key("price_entity"));
    }

    #[test]
    fn dashboard_without_prefs() {
        let empty: Cfg = ENERGY_KEYS.iter().map(|k| (k.to_string(), json!([]))).collect();
        assert_eq!(dashboard_defaults(None), empty);
        assert_eq!(dashboard_defaults(Some(&json!({}))), empty);
    }
}
