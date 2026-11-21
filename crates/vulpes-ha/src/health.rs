//! Is Vulpes working right now? Live checks on plain facts the integration
//! reads from Home Assistant, merged with the review's checks into one
//! status, plus the repair issues that should be open. Report only: health
//! never changes what control does.

use crate::Result;
use crate::cfg::{Cfg, as_f64, obj, req_num, truthy};
use crate::inputs::{attribute_pairs, parse_price_attributes, soc_value};
use crate::time::zone;
use chrono::{DateTime, Timelike};
use serde_json::{Value, json};

/// A plan older than this (s) counts as no plan; control falls back after the same time.
pub const NO_PLAN_S: f64 = 1800.0;
/// A SOC sensor that reported nothing for this long (s) is suspect...
pub const SOC_SILENT_S: f64 = 3600.0;
/// ...when the plan expects the battery to move at least this much (points) in the coming hour.
pub const SOC_MOVE_PCT: f64 = 10.0;
/// After this local hour, tomorrow's prices should be known.
pub const PRICES_TOMORROW_HOUR: u32 = 15;
/// Energy statistics whose newest complete hour ended this long ago (s) are late / stopped.
pub const ENERGY_LATE_S: f64 = 3.0 * 3600.0;
pub const ENERGY_STOPPED_S: f64 = 24.0 * 3600.0;
/// Wholesale prices not refreshed for this long (s).
pub const WHOLESALE_OLD_S: f64 = 24.0 * 3600.0;
/// Repair issues: how long a problem lasts before the user is told.
pub const ISSUE_SOC_S: f64 = 3600.0;
pub const ISSUE_NO_PLAN_S: f64 = 3600.0;
pub const ISSUE_SCRIPT_FAILURES: u64 = 3;

/// Every repair issue id; each needs a title and description in `strings.json`.
pub const ISSUES: [&str; 4] = ["soc_unavailable", "no_plan", "energy_stopped", "script_failing"];

const OK: &str = "ok";
const DEGRADED: &str = "degraded";
const PROBLEM: &str = "problem";

fn check(name: &str, state: &str, detail: impl Into<String>) -> Value {
    json!({"check": name, "state": state, "detail": detail.into()})
}

fn rank(state: &str) -> u8 {
    match state {
        PROBLEM => 2,
        DEGRADED => 1,
        _ => 0,
    }
}

fn minutes(s: f64) -> String {
    if s >= 2.0 * 3600.0 {
        format!("{:.0} h", s / 3600.0)
    } else {
        format!("{:.0} min", s / 60.0)
    }
}

/// The price fallback warning is expected when the user chose no wholesale source.
fn counts(warning: &str, cfg: &Cfg) -> bool {
    let no_wholesale = cfg.get("wholesale_source").and_then(Value::as_str) != Some("energyzero");
    !(no_wholesale && warning.starts_with("no price forecast for"))
}

/// Wind and solar at the price reference points: fetched lately, and whether
/// the plan's prices used them.
fn price_drivers_check(age: Option<f64>, plan: Option<&Value>) -> Value {
    match age {
        Some(age) if age > WHOLESALE_OLD_S => check(
            "price_drivers",
            DEGRADED,
            format!("wind and solar forecast last fetched {} ago", minutes(age)),
        ),
        None => check("price_drivers", DEGRADED, "not fetched yet"),
        Some(_) => {
            let pf = plan.and_then(|p| p.get("price_forecast"));
            let method = pf.and_then(|p| p.get("method")).and_then(Value::as_str);
            let checked = pf.is_some_and(|p| p.get("check_days").is_some_and(|d| !d.is_null()));
            let detail = match method {
                Some("profile") if checked => {
                    "the price profile did better than the wind/solar correction lately, so it is used"
                }
                _ => "",
            };
            check("price_drivers", OK, detail)
        }
    }
}

/// Health from a snapshot of facts. `snap` holds `cfg`, `now`, `timezone`,
/// `started_at`, `refresh_ok`, `last_success` and `refresh_error` of the
/// planner, the `plan`, `soc_state` with `soc_last_changed` and
/// `soc_last_reported`, `price_attributes` with `price_unit`,
/// `wholesale_fetched_at`, `price_drivers_fetched_at` (the last forecast at
/// the price reference points), `newest_energy_ts`, `active`, `control_script`,
/// `last_command`, `last_error`, `script_failures`, `history_error`,
/// `review_error` and the review's `review_checks`.
pub fn health(snap: &Value) -> Result<Value> {
    let cfg = obj(snap, "cfg")?;
    let now = req_num(snap, "now")?;
    let tz = zone(snap.get("timezone").and_then(Value::as_str).unwrap_or("UTC"));
    let num = |k: &str| snap.get(k).and_then(as_f64);
    let text = |k: &str| snap.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    let plan = snap.get("plan").filter(|p| p.is_object());
    let mut checks = Vec::new();
    let mut issues = Vec::new();

    // Planning.
    let last_success = num("last_success");
    let since_plan = last_success.map(|t| now - t);
    let error = text("refresh_error").unwrap_or("planning failed");
    let started = num("started_at").unwrap_or(now);
    // The first plan runs in the background after startup: until it ends,
    // nothing has failed (`refresh_ok` stays true and there is no error).
    let first_pending = truthy(snap.get("refresh_ok")) && text("refresh_error").is_none();
    // Until then, checks of what the first plan fetches or records say so instead of failing.
    let waiting = last_success.is_none() && first_pending && now - started <= NO_PLAN_S;
    let pending = |name: &str| check(name, OK, "waiting for the first plan");
    checks.push(match (truthy(snap.get("refresh_ok")), since_plan) {
        (true, Some(age)) if age <= NO_PLAN_S => check("plan", OK, ""),
        (_, Some(age)) if age <= NO_PLAN_S => check("plan", DEGRADED, error),
        (_, Some(age)) => check("plan", PROBLEM, format!("no plan for {}: {error}", minutes(age))),
        (_, None) if waiting => check("plan", OK, "waiting for the first plan"),
        (_, None) if first_pending => check(
            "plan",
            PROBLEM,
            format!("no plan yet after {}", minutes(now - started)),
        ),
        (_, None) => check("plan", PROBLEM, error),
    });
    if since_plan.unwrap_or(now - started) > ISSUE_NO_PLAN_S {
        issues.push("no_plan");
    }

    // Battery SOC sensor.
    let soc = soc_value(text("soc_state"));
    checks.push(match soc {
        None => {
            if num("soc_last_changed").is_some_and(|t| now - t >= ISSUE_SOC_S) {
                issues.push("soc_unavailable");
            }
            check(
                "soc",
                PROBLEM,
                format!(
                    "state '{}' is not a state of charge",
                    text("soc_state").unwrap_or("missing")
                ),
            )
        }
        Some(pct) => {
            let silent = num("soc_last_reported")
                .map(|t| now - t)
                .filter(|s| *s >= SOC_SILENT_S);
            // What the plan expects in the coming hour (4 slots).
            let expected = plan
                .and_then(|p| p.get("slots")?.as_array()?.get(3)?.get("soc_pct")?.as_f64())
                .map(|planned| (planned - pct).abs());
            match (silent, expected) {
                (Some(s), Some(e)) if e >= SOC_MOVE_PCT => check(
                    "soc",
                    DEGRADED,
                    format!(
                        "not updated for {} while the plan moves the battery {e:.0} points this hour",
                        minutes(s)
                    ),
                ),
                _ => check("soc", OK, ""),
            }
        }
    });

    // Prices.
    let known = attribute_pairs(snap, "price_attributes")
        .map(|a| parse_price_attributes(&a, text("price_unit"), tz))
        .unwrap_or_default();
    let local = DateTime::from_timestamp(now as i64, 0).map(|t| t.with_timezone(&tz));
    let last_known = known.iter().map(|p| p.end).max();
    checks.push(
        if !known
            .iter()
            .any(|p| (p.start as f64) <= now && now < p.end as f64)
        {
            check("prices", PROBLEM, "the price sensor has no price for now")
        } else if let Some(local) = local.filter(|l| l.hour() >= PRICES_TOMORROW_HOUR) {
            // The start of tomorrow's last hour.
            let day_end = local
                .date_naive()
                .succ_opt()
                .and_then(|d| vulpes_engine::time::local_time(tz, d, 23));
            if last_known.zip(day_end).is_some_and(|(k, e)| k < e) {
                check(
                    "prices",
                    DEGRADED,
                    format!("no prices for tomorrow yet after {PRICES_TOMORROW_HOUR}:00"),
                )
            } else {
                check("prices", OK, "")
            }
        } else {
            check("prices", OK, "")
        },
    );

    // Energy statistics (newest complete hour in the history file).
    checks.push(match num("newest_energy_ts").map(|t| now - (t + 3600.0)) {
        None if waiting => pending("energy_data"),
        None => check("energy_data", PROBLEM, "no energy statistics recorded"),
        Some(age) if age > ENERGY_STOPPED_S => {
            issues.push("energy_stopped");
            check(
                "energy_data",
                PROBLEM,
                format!("the newest energy statistics are {} old", minutes(age)),
            )
        }
        Some(age) if age > ENERGY_LATE_S => check(
            "energy_data",
            DEGRADED,
            format!("the newest energy statistics are {} old", minutes(age)),
        ),
        Some(_) => check("energy_data", OK, ""),
    });

    // Wholesale prices, when used.
    if cfg.get("wholesale_source").and_then(Value::as_str) == Some("energyzero") {
        checks.push(match num("wholesale_fetched_at").map(|t| now - t) {
            Some(age) if age <= WHOLESALE_OLD_S => check("wholesale", OK, ""),
            Some(age) => check(
                "wholesale",
                DEGRADED,
                format!("last fetched {} ago", minutes(age)),
            ),
            None if waiting => pending("wholesale"),
            None => check("wholesale", DEGRADED, "not fetched yet"),
        });
        checks.push(match num("price_drivers_fetched_at").map(|t| now - t) {
            None if waiting => pending("price_drivers"),
            age => price_drivers_check(age, plan),
        });
    }

    // What the engine had to guess.
    let warnings: Vec<String> = plan
        .and_then(|p| p.get("warnings")?.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|w| w.as_str().map(String::from))
        .filter(|w| counts(w, cfg))
        .collect();
    checks.push(if warnings.is_empty() {
        check("forecast_inputs", OK, "")
    } else {
        check("forecast_inputs", DEGRADED, warnings.join("; "))
    });

    // Control.
    if truthy(snap.get("control_script")) {
        let failures = num("script_failures").unwrap_or(0.0) as u64;
        // Only while active: once control is handed back no script runs, so nothing could close it.
        if truthy(snap.get("active")) && failures >= ISSUE_SCRIPT_FAILURES {
            issues.push("script_failing");
        }
        checks.push(if !truthy(snap.get("active")) {
            check("control", OK, "not active")
        } else if let Some(e) = text("last_error") {
            check("control", PROBLEM, format!("the control script failed: {e}"))
        } else if snap.get("last_command").is_none_or(Value::is_null) {
            check("control", OK, "waiting for the first plan")
        } else if snap
            .get("last_command")
            .is_none_or(|c| !c.is_object() || truthy(c.get("fallback")))
        {
            check("control", PROBLEM, "no usable plan: the safe mode was sent")
        } else {
            check("control", OK, "")
        });
    }

    // The history file.
    let history = [text("history_error"), text("review_error")];
    checks.push(match history.iter().flatten().next() {
        Some(e) => check("history", DEGRADED, *e),
        None => check("history", OK, ""),
    });

    // The review's own checks.
    checks.extend(
        snap.get("review_checks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    );

    let status = checks
        .iter()
        .filter_map(|c| c.get("state").and_then(Value::as_str))
        .max_by_key(|s| rank(s))
        .map_or(OK, |s| if rank(s) == 0 { OK } else { s });
    // The checks that aren't ok, as `check: detail` lines.
    let problems: Vec<String> = checks
        .iter()
        .filter(|c| {
            c.get("state")
                .and_then(Value::as_str)
                .is_some_and(|s| rank(s) > 0)
        })
        .map(|c| {
            let text = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("");
            format!("{}: {}", text("check"), text("detail"))
        })
        .collect();
    Ok(json!({"status": status, "checks": checks, "problems": problems, "issues": issues}))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1_794_139_200.0; // 2026-11-08 12:00 UTC = 13:00 Amsterdam

    fn prices(from: f64, hours: i32) -> Value {
        let items: Vec<Value> = (0..hours)
            .map(|h| json!({"start": from + 3600.0 * f64::from(h), "price": 0.2}))
            .collect();
        json!([["today", items]])
    }

    fn plan(soc_in_an_hour: f64, warnings: Vec<&str>) -> Value {
        let slots: Vec<Value> = (0..8)
            .map(|k| json!({"soc_pct": if k == 3 { soc_in_an_hour } else { 50.0 }}))
            .collect();
        json!({"slots": slots, "warnings": warnings})
    }

    /// A healthy snapshot.
    fn snap() -> Value {
        json!({
            "cfg": {"wholesale_source": "energyzero"},
            "now": NOW, "timezone": "Europe/Amsterdam", "started_at": NOW - 86_400.0,
            "refresh_ok": true, "last_success": NOW - 40.0, "refresh_error": null,
            "plan": plan(52.0, vec![]),
            "soc_state": "50", "soc_last_changed": NOW - 600.0, "soc_last_reported": NOW - 60.0,
            "price_attributes": prices(NOW - 13.0 * 3600.0, 36), "price_unit": "EUR/kWh",
            "wholesale_fetched_at": NOW - 3600.0, "price_drivers_fetched_at": NOW - 3600.0,
            "newest_energy_ts": NOW - 2.0 * 3600.0,
            "active": true, "control_script": "script.x", "last_command": {"fallback": false},
            "last_error": null, "script_failures": 0, "history_error": null, "review_error": null,
            "review_checks": [{"check": "load_forecast", "state": "not_enough_data", "detail": ""}]
        })
    }

    fn state_of(h: &Value, name: &str) -> String {
        h["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["check"] == name)
            .unwrap()["state"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn with(changes: Value) -> Value {
        let mut s = snap();
        for (k, v) in changes.as_object().unwrap() {
            s[k] = v.clone();
        }
        health(&s).unwrap()
    }

    #[test]
    fn a_healthy_snapshot_is_ok() {
        let h = health(&snap()).unwrap();
        assert_eq!(h["status"], "ok", "{h}");
        assert_eq!(h["issues"], json!([]));
        assert_eq!(
            state_of(&h, "load_forecast"),
            "not_enough_data",
            "review checks are merged"
        );
    }

    #[test]
    fn planning_failures() {
        let h = with(
            json!({"refresh_ok": false, "refresh_error": "planning failed: weather", "last_success": NOW - 900.0}),
        );
        assert_eq!(
            (h["status"].as_str(), state_of(&h, "plan").as_str()),
            (Some("degraded"), "degraded")
        );
        // Exactly at the limit is still degraded, one second over is a problem.
        assert_eq!(
            state_of(
                &with(json!({"refresh_ok": false, "last_success": NOW - NO_PLAN_S})),
                "plan"
            ),
            "degraded"
        );
        let h = with(json!({"refresh_ok": false, "last_success": NOW - NO_PLAN_S - 1.0}));
        assert_eq!(
            (h["status"].as_str(), state_of(&h, "plan").as_str()),
            (Some("problem"), "problem")
        );
        assert_eq!(h["issues"], json!([]), "the repair issue waits an hour");
        let h = with(json!({"refresh_ok": false, "last_success": NOW - ISSUE_NO_PLAN_S - 1.0}));
        assert_eq!(h["issues"], json!(["no_plan"]));
        // Never planned since startup.
        let h = with(json!({"refresh_ok": false, "last_success": null, "started_at": NOW - 600.0}));
        assert_eq!(
            (state_of(&h, "plan").as_str(), &h["issues"]),
            ("problem", &json!([]))
        );
        let h = with(json!({"refresh_ok": false, "last_success": null, "started_at": NOW - 7200.0}));
        assert_eq!(h["issues"], json!(["no_plan"]));
    }

    #[test]
    fn the_first_plan_after_startup_is_awaited() {
        let pending = |since: f64| {
            with(
                json!({"refresh_ok": true, "last_success": null, "refresh_error": null,
                        "plan": null, "started_at": NOW - since, "newest_energy_ts": null,
                        "wholesale_fetched_at": null, "price_drivers_fetched_at": null,
                        "last_command": null}),
            )
        };
        let h = pending(30.0);
        // Only what the first plan fetches, records or sends waits; the SOC and price
        // sensors are checked as always.
        for name in ["plan", "energy_data", "wholesale", "price_drivers", "control"] {
            let c = h["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["check"] == name)
                .unwrap()
                .clone();
            assert_eq!(
                (name, c["state"].as_str(), c["detail"].as_str()),
                (name, Some("ok"), Some("waiting for the first plan"))
            );
        }
        assert_eq!(h["status"], "ok", "{:?}", h["problems"]);
        let plan = h["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["check"] == "plan")
            .unwrap()
            .clone();
        assert_eq!(
            (plan["state"].as_str(), plan["detail"].as_str()),
            (Some("ok"), Some("waiting for the first plan"))
        );
        assert_eq!(h["issues"], json!([]));
        // Still nothing after half an hour: a problem, and the repair issue after an hour.
        let h = pending(NO_PLAN_S + 1.0);
        assert_eq!(state_of(&h, "plan"), "problem");
        assert_eq!(h["issues"], json!([]));
        assert_eq!(pending(ISSUE_NO_PLAN_S + 1.0)["issues"], json!(["no_plan"]));
        // A failed first plan is a problem at once.
        let h = with(
            json!({"refresh_ok": false, "last_success": null, "refresh_error": "weather unavailable",
                            "plan": null, "started_at": NOW - 30.0}),
        );
        assert_eq!(state_of(&h, "plan"), "problem");
    }

    #[test]
    fn soc_sensor() {
        let h = with(json!({"soc_state": "unavailable", "soc_last_changed": NOW - 600.0}));
        assert_eq!(
            (state_of(&h, "soc").as_str(), &h["issues"]),
            ("problem", &json!([]))
        );
        let h = with(json!({"soc_state": "unavailable", "soc_last_changed": NOW - ISSUE_SOC_S}));
        assert_eq!(h["issues"], json!(["soc_unavailable"]));
        // Silent for an hour: only suspect when the plan moves the battery 10 points or more.
        assert_eq!(
            state_of(&with(json!({"soc_last_reported": NOW - 3600.0})), "soc"),
            "ok"
        );
        let moving = json!({"soc_last_reported": NOW - 3600.0, "plan": plan(60.0, vec![])});
        assert_eq!(state_of(&with(moving), "soc"), "degraded");
        let recent = json!({"soc_last_reported": NOW - 3599.0, "plan": plan(60.0, vec![])});
        assert_eq!(state_of(&with(recent), "soc"), "ok");
    }

    #[test]
    fn price_checks() {
        let none = with(json!({"price_attributes": null}));
        assert_eq!(state_of(&none, "prices"), "problem");
        let past = with(json!({"price_attributes": prices(NOW - 10.0 * 3600.0, 5)}));
        assert_eq!(state_of(&past, "prices"), "problem", "no price for now");
        // 13:00 local: today's prices are enough.
        assert_eq!(
            state_of(
                &with(json!({"price_attributes": prices(NOW - 13.0 * 3600.0, 24)})),
                "prices"
            ),
            "ok"
        );
        // 15:00 local without tomorrow: degraded; with tomorrow: ok.
        let at15 = NOW + 2.0 * 3600.0;
        let today = json!({"now": at15, "price_attributes": prices(NOW - 13.0 * 3600.0, 24)});
        assert_eq!(state_of(&with(today), "prices"), "degraded");
        let both = json!({"now": at15, "price_attributes": prices(NOW - 13.0 * 3600.0, 48)});
        assert_eq!(state_of(&with(both), "prices"), "ok");
    }

    #[test]
    fn energy_statistics_and_wholesale() {
        let age = |a: f64| json!({"newest_energy_ts": NOW - 3600.0 - a});
        assert_eq!(state_of(&with(age(ENERGY_LATE_S)), "energy_data"), "ok");
        assert_eq!(
            state_of(&with(age(ENERGY_LATE_S + 1.0)), "energy_data"),
            "degraded"
        );
        let stopped = with(age(ENERGY_STOPPED_S + 1.0));
        assert_eq!(
            (state_of(&stopped, "energy_data").as_str(), &stopped["issues"]),
            ("problem", &json!(["energy_stopped"]))
        );
        assert_eq!(
            state_of(&with(json!({"newest_energy_ts": null})), "energy_data"),
            "problem"
        );
        assert_eq!(
            state_of(
                &with(json!({"wholesale_fetched_at": NOW - WHOLESALE_OLD_S - 1.0})),
                "wholesale"
            ),
            "degraded"
        );
        assert_eq!(
            state_of(&with(json!({"wholesale_fetched_at": null})), "wholesale"),
            "degraded"
        );
        let h = with(json!({"cfg": {"wholesale_source": "none"}, "wholesale_fetched_at": null}));
        assert!(
            h["checks"]
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c["check"] != "wholesale" && c["check"] != "price_drivers")
        );
    }

    #[test]
    fn price_drivers() {
        assert_eq!(state_of(&with(json!({})), "price_drivers"), "ok");
        let old = with(json!({"price_drivers_fetched_at": NOW - WHOLESALE_OLD_S - 1.0}));
        assert_eq!(state_of(&old, "price_drivers"), "degraded");
        assert_eq!(
            old["issues"],
            json!([]),
            "no repair issue: prices still come from the profile"
        );
        assert_eq!(
            state_of(&with(json!({"price_drivers_fetched_at": null})), "price_drivers"),
            "degraded"
        );
        let detail = |pf: Value| {
            let mut p = plan(52.0, vec![]);
            p["price_forecast"] = pf;
            let h = with(json!({"plan": p}));
            let c = h["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["check"] == "price_drivers")
                .cloned();
            let c = c.unwrap();
            (
                c["state"].as_str().unwrap().to_string(),
                c["detail"].as_str().unwrap().to_string(),
            )
        };
        let (state, d) = detail(json!({"method": "profile", "check_days": 14}));
        assert_eq!(state, "ok");
        assert!(d.contains("did better"), "{d}");
        assert_eq!(detail(json!({"method": "regression", "check_days": 14})).1, "");
        assert_eq!(detail(json!({"method": "profile", "check_days": null})).1, "");
    }

    #[test]
    fn engine_warnings() {
        let w = with(
            json!({"plan": plan(52.0, vec!["weather reaches 192 of 336 slots ahead; the plan ends there"])}),
        );
        assert_eq!(state_of(&w, "forecast_inputs"), "degraded");
        // The price fallback is expected without a wholesale source, not with one.
        let fallback = vec!["no price forecast for 235 slots; used the mean known price"];
        let none = json!({"cfg": {"wholesale_source": "none"}, "plan": plan(52.0, fallback.clone())});
        assert_eq!(state_of(&with(none), "forecast_inputs"), "ok");
        assert_eq!(
            state_of(&with(json!({"plan": plan(52.0, fallback)})), "forecast_inputs"),
            "degraded"
        );
    }

    #[test]
    fn control() {
        assert_eq!(
            state_of(&with(json!({"last_error": "Action x not found"})), "control"),
            "problem"
        );
        assert_eq!(
            state_of(&with(json!({"last_command": {"fallback": true}})), "control"),
            "problem"
        );
        // Active, but nothing sent since the start: the first plan is still running.
        let waiting = with(json!({"last_command": null}));
        assert_eq!(state_of(&waiting, "control"), "ok");
        let control = waiting["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["check"] == "control")
            .unwrap()
            .clone();
        assert_eq!(control["detail"], "waiting for the first plan");
        assert_eq!(
            state_of(&with(json!({"active": false, "last_error": "x"})), "control"),
            "ok"
        );
        assert_eq!(with(json!({"script_failures": 2}))["issues"], json!([]));
        assert_eq!(
            with(json!({"script_failures": 3}))["issues"],
            json!(["script_failing"])
        );
        // Handed back: no script runs any more, so the issue must not stay open.
        assert_eq!(
            with(json!({"active": false, "script_failures": 9}))["issues"],
            json!([])
        );
        let h = with(json!({"control_script": null}));
        assert!(
            h["checks"]
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c["check"] != "control")
        );
    }

    #[test]
    fn problems_list_the_checks_that_are_not_ok() {
        assert_eq!(health(&snap()).unwrap()["problems"], json!([]));
        let h = with(json!({"history_error": "disk full", "last_error": "boom"}));
        assert_eq!(
            h["problems"],
            json!(["control: the control script failed: boom", "history: disk full"])
        );
    }

    #[test]
    fn history_and_review_checks_and_the_worst_state_wins() {
        assert_eq!(
            state_of(&with(json!({"history_error": "disk full"})), "history"),
            "degraded"
        );
        assert_eq!(
            state_of(&with(json!({"review_error": "x"})), "history"),
            "degraded"
        );
        let review =
            json!({"review_checks": [{"check": "soc_tracking", "state": "degraded", "detail": "x"}]});
        assert_eq!(with(review)["status"], "degraded");
        let both = json!({"history_error": "x", "soc_state": "unknown"});
        assert_eq!(with(both)["status"], "problem");
        assert!(health(&json!({"now": NOW})).is_err(), "settings are required");
    }

    #[test]
    fn issue_ids_are_listed() {
        let mut seen = std::collections::HashSet::new();
        for v in [
            json!({"refresh_ok": false, "last_success": null, "started_at": 0.0}),
            json!({"soc_state": "x", "soc_last_changed": 0.0}),
            json!({"newest_energy_ts": 0.0}),
            json!({"script_failures": 9}),
        ] {
            for i in with(v)["issues"].as_array().unwrap() {
                seen.insert(i.as_str().unwrap().to_string());
            }
        }
        assert_eq!(seen.len(), ISSUES.len());
        assert!(seen.iter().all(|i| ISSUES.contains(&i.as_str())));
    }
}
