//! Active control: the variables Vulpes passes to the user's control script.
//!
//! Every 15 minutes, while the Active switch is on, Vulpes runs one script
//! that the user picked, and again when the plan is updated on request (the
//! `update_plan` action). The script turns the variables into inverter
//! actions. When there is no usable plan, Vulpes sends the safe command
//! instead: self-consume, with solar on and export allowed.
//!
//! The variable names and mode values are an API (`docs/control.md`).

use crate::cfg::{Cfg, as_f64, num_or, truthy};
use crate::modes::{power_control, script_power, split_mode};
use crate::time::iso_utc;
use serde_json::{Map, Value, json};

pub const SLOT_S: f64 = 900.0;
/// A plan older than this is not followed: the safe command is sent instead.
pub const STALE_AFTER_S: f64 = 1800.0;
pub const SAFE_MODE: &str = "self_consume";
/// How long the control script may run (s). Vulpes waits for it, so errors in its
/// actions show on the Battery control sensor.
pub const SCRIPT_TIMEOUT_S: f64 = 60.0;

/// States of the Battery control sensor.
pub const OFF: &str = "off";
pub const FOLLOWING: &str = "following_plan";
pub const FALLBACK: &str = "fallback";
pub const SCRIPT_ERROR: &str = "script_error";
pub const STATES: [&str; 4] = [OFF, FOLLOWING, FALLBACK, SCRIPT_ERROR];

fn variables(cfg: &Cfg, mode: &str, pv_off: bool, no_export: bool) -> Map<String, Value> {
    let v = json!({
        "mode": mode,
        "pv_off": pv_off,
        "no_export": no_export,
        "power_control": power_control(cfg),
        "power_w": script_power(cfg, mode),
        "soc_min": num_or(cfg, "min_soc", 0.0),
        "soc_max": num_or(cfg, "max_soc", 100.0),
    });
    v.as_object().cloned().unwrap_or_default()
}

/// Self-consume with solar on and export allowed, until the next quarter hour.
pub fn safe_command(cfg: &Cfg, now: f64, fallback: bool) -> Value {
    let mut cmd = variables(cfg, SAFE_MODE, false, false);
    cmd.insert("planned_grid_w".into(), Value::Null);
    cmd.insert("planned_battery_w".into(), Value::Null);
    cmd.insert("fallback".into(), json!(fallback));
    cmd.insert("until".into(), json!(iso_utc(now - now % SLOT_S + SLOT_S)));
    Value::Object(cmd)
}

/// The plan slot that contains `now`, if any.
pub fn current_slot(plan: &Value, now: f64) -> Option<&Value> {
    plan.get("slots")?.as_array()?.iter().find(|s| {
        s.get("start")
            .and_then(as_f64)
            .is_some_and(|start| start <= now && now < start + SLOT_S)
    })
}

/// Script variables for `now`: the planned slot, or the safe command.
///
/// The safe command is used when the last planning run failed, when the plan
/// is older than [`STALE_AFTER_S`], or when it has no slot for `now`. Slots
/// carry the Vulpes mode with `pv_off` and `no_export`, or a combined engine
/// mode name.
pub fn command(cfg: &Cfg, plan: Option<&Value>, now: f64, failed: bool) -> Value {
    let plan = match plan {
        Some(p) if !failed && truthy(Some(p)) => p,
        _ => return safe_command(cfg, now, true),
    };
    let fresh = plan
        .get("planned_at")
        .and_then(as_f64)
        .is_some_and(|at| now - at <= STALE_AFTER_S);
    let Some(slot) = current_slot(plan, now).filter(|_| fresh) else {
        return safe_command(cfg, now, true);
    };
    let name = slot.get("mode").and_then(Value::as_str).unwrap_or(SAFE_MODE);
    let (mode, pv_off, no_export) = if slot.get("pv_off").is_some() {
        (name, truthy(slot.get("pv_off")), truthy(slot.get("no_export")))
    } else {
        split_mode(name) // an engine slot that was not split yet
    };
    let watts = |key: &str| {
        slot.get(key)
            .and_then(as_f64)
            .map_or(json!(0), |w| json!(w.round() as i64))
    };
    let start = slot.get("start").and_then(as_f64).unwrap_or(now);
    let mut cmd = variables(cfg, mode, pv_off, no_export);
    cmd.insert("planned_grid_w".into(), watts("grid_w"));
    cmd.insert("planned_battery_w".into(), watts("battery_w"));
    cmd.insert("fallback".into(), json!(false));
    cmd.insert("until".into(), json!(iso_utc(start + SLOT_S)));
    Value::Object(cmd)
}

/// The control status: off, following the plan, in fallback, or the script failed.
pub fn state_of(active: bool, last: Option<&Value>, error: Option<&str>) -> &'static str {
    if !active {
        OFF
    } else if error.is_some_and(|e| !e.is_empty()) {
        SCRIPT_ERROR
    } else if last.is_none_or(|l| l.is_null() || truthy(l.get("fallback"))) {
        FALLBACK
    } else {
        FOLLOWING
    }
}

/// What one run of the control script leaves: the command as sent (with the time it
/// was `sent`), the error (none when it ran fine), and how many runs in a row failed.
/// `error` is the exception's text; `timed_out` when it ran past [`SCRIPT_TIMEOUT_S`].
pub fn script_result(cmd: &Value, error: Option<&str>, timed_out: bool, failures: u64, now: f64) -> Value {
    let error = if timed_out {
        Some(format!("did not finish within {SCRIPT_TIMEOUT_S:.0} s"))
    } else {
        error.filter(|e| !e.is_empty()).map(String::from)
    };
    let mut sent = cmd.as_object().cloned().unwrap_or_default();
    sent.insert("sent".into(), json!(iso_utc(now)));
    json!({
        "last_command": sent,
        "script_failures": if error.is_some() { failures + 1 } else { 0 },
        "last_error": error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: f64 = 1_794_139_200.0; // 2026-11-08 12:00 UTC

    fn cfg() -> Cfg {
        json!({"power_control": "grid_setpoint", "charge_w": 3000, "discharge_w": -2500, "min_soc": 11, "max_soc": 100})
            .as_object()
            .unwrap()
            .clone()
    }

    fn slot(i: u32, mode: &str, pv_off: bool, no_export: bool) -> Value {
        json!({"start": T0 + 900.0 * f64::from(i), "mode": mode, "pv_off": pv_off, "no_export": no_export,
               "grid_w": 2999.6, "battery_w": -1200.4})
    }

    fn plan(slots: Vec<Value>, planned_at: f64) -> Value {
        json!({"planned_at": planned_at, "slots": slots})
    }

    fn is_safe(cmd: &Value) -> bool {
        cmd["mode"] == "self_consume"
            && cmd["pv_off"] == false
            && cmd["no_export"] == false
            && cmd["fallback"] == true
            && cmd["planned_grid_w"].is_null()
    }

    #[test]
    fn follows_the_slot_that_contains_now() {
        let p = plan(
            vec![
                slot(0, "hold", false, false),
                slot(1, "charge", false, false),
                slot(2, "discharge", false, false),
            ],
            T0,
        );
        assert_eq!(
            command(&cfg(), Some(&p), T0 + 900.0 + 20.0, false),
            json!({
                "mode": "charge", "pv_off": false, "no_export": false, "power_control": "grid_setpoint",
                "power_w": 3000, "soc_min": 11.0, "soc_max": 100.0, "planned_grid_w": 3000,
                "planned_battery_w": -1200, "fallback": false, "until": "2026-11-08T12:30:00+00:00"
            })
        );
    }

    #[test]
    fn power_is_unsigned_and_only_for_powered_modes() {
        for (mode, power) in [
            ("charge", 3000),
            ("discharge", 2500),
            ("hold", 0),
            ("self_consume", 0),
            ("idle", 0),
        ] {
            let cmd = command(
                &cfg(),
                Some(&plan(vec![slot(0, mode, false, false)], T0)),
                T0,
                false,
            );
            assert_eq!(
                (cmd["mode"].as_str(), cmd["power_w"].as_i64()),
                (Some(mode), Some(power))
            );
        }
    }

    #[test]
    fn battery_power_control_is_passed_on() {
        let mut c = cfg();
        c.insert("power_control".into(), json!("battery_power"));
        let cmd = command(
            &c,
            Some(&plan(vec![slot(0, "charge", false, false)], T0)),
            T0,
            false,
        );
        assert_eq!(cmd["power_control"], "battery_power");
    }

    #[test]
    fn pv_off_and_no_export_flags() {
        let p = plan(
            vec![slot(0, "self_consume", false, true), slot(1, "idle", true, false)],
            T0,
        );
        let first = command(&cfg(), Some(&p), T0, false);
        let second = command(&cfg(), Some(&p), T0 + 900.0, false);
        assert_eq!(
            (&first["mode"], &first["pv_off"], &first["no_export"]),
            (&json!("self_consume"), &json!(false), &json!(true))
        );
        assert_eq!(
            (&second["mode"], &second["pv_off"], &second["no_export"]),
            (&json!("idle"), &json!(true), &json!(false))
        );
    }

    #[test]
    fn engine_mode_names_are_split_when_slots_were_not() {
        let s = json!({"start": T0, "mode": "hold:no_export", "grid_w": 0.0, "battery_w": 0.0});
        let cmd = command(&cfg(), Some(&plan(vec![s], T0)), T0, false);
        assert_eq!(
            (&cmd["mode"], &cmd["pv_off"], &cmd["no_export"]),
            (&json!("hold"), &json!(false), &json!(true))
        );
    }

    #[test]
    fn fallback_when_planning_failed() {
        let cmd = command(
            &cfg(),
            Some(&plan(vec![slot(0, "charge", false, false)], T0)),
            T0,
            true,
        );
        assert!(is_safe(&cmd));
        assert_eq!(cmd["until"], "2026-11-08T12:15:00+00:00");
    }

    #[test]
    fn fallback_without_a_plan_or_slots() {
        assert!(is_safe(&command(&cfg(), None, T0, false)));
        assert!(is_safe(&command(&cfg(), Some(&plan(vec![], T0)), T0, false)));
        let no_time = json!({"slots": [slot(0, "charge", false, false)]});
        assert!(is_safe(&command(&cfg(), Some(&no_time), T0, false)));
        assert!(is_safe(&command(&cfg(), Some(&json!({})), T0, false)));
    }

    #[test]
    fn fallback_when_now_is_outside_the_plan() {
        let p = plan(vec![slot(0, "charge", false, false)], T0);
        assert!(is_safe(&command(&cfg(), Some(&p), T0 + 900.0, false)));
        assert!(is_safe(&command(&cfg(), Some(&p), T0 - 1.0, false)));
    }

    #[test]
    fn stale_plan_boundary() {
        let p = plan((0..4).map(|i| slot(i, "charge", false, false)).collect(), T0);
        let fresh = command(&cfg(), Some(&p), T0 + STALE_AFTER_S, false);
        assert!(fresh["fallback"] == false && fresh["mode"] == "charge");
        assert!(is_safe(&command(
            &cfg(),
            Some(&p),
            T0 + STALE_AFTER_S + 1.0,
            false
        )));
    }

    #[test]
    fn safe_command_when_switched_off_is_not_a_fallback() {
        let cmd = safe_command(&cfg(), T0 + 1.0, false);
        assert_eq!(
            (&cmd["mode"], &cmd["fallback"], &cmd["until"]),
            (
                &json!("self_consume"),
                &json!(false),
                &json!("2026-11-08T12:15:00+00:00")
            )
        );
    }

    #[test]
    fn soc_limits_default_when_missing() {
        let cmd = safe_command(&Cfg::new(), T0, true);
        assert_eq!(
            (&cmd["soc_min"], &cmd["soc_max"], &cmd["power_control"]),
            (&json!(0.0), &json!(100.0), &json!("grid_setpoint"))
        );
    }

    #[test]
    fn control_states() {
        let following = json!({"fallback": false});
        assert_eq!(state_of(false, Some(&following), None), OFF);
        assert_eq!(state_of(true, None, None), FALLBACK);
        assert_eq!(state_of(true, Some(&json!(null)), None), FALLBACK);
        assert_eq!(state_of(true, Some(&json!({"fallback": true})), None), FALLBACK);
        assert_eq!(state_of(true, Some(&following), None), FOLLOWING);
        assert_eq!(
            state_of(true, Some(&following), Some("Action notify.x not found")),
            SCRIPT_ERROR
        );
        assert_eq!(state_of(true, Some(&following), Some("")), FOLLOWING);
        assert_eq!(state_of(false, Some(&following), Some("boom")), OFF);
    }

    #[test]
    fn script_results_count_failures_in_a_row() {
        let cmd = json!({"mode": "hold", "fallback": false});
        let ok = script_result(&cmd, None, false, 2, 1_794_139_200.0);
        assert_eq!(
            ok,
            json!({
                "last_command": {"mode": "hold", "fallback": false, "sent": "2026-11-08T12:00:00+00:00"},
                "script_failures": 0,
                "last_error": null,
            })
        );
        let failed = script_result(&cmd, Some("Action notify.x not found"), false, 2, 0.0);
        assert_eq!(failed["script_failures"], 3);
        assert_eq!(failed["last_error"], "Action notify.x not found");
        let slow = script_result(&cmd, None, true, 0, 0.0);
        assert_eq!(slow["last_error"], "did not finish within 60 s");
        assert_eq!(slow["script_failures"], 1);
        // An empty error text counts as no error; a command that isn't an object still gets `sent`.
        assert_eq!(script_result(&cmd, Some(""), false, 5, 0.0)["script_failures"], 0);
        assert_eq!(
            script_result(&json!(null), None, false, 0, 0.0)["last_command"],
            json!({"sent": "1970-01-01T00:00:00+00:00"})
        );
    }
}
