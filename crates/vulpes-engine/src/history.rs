//! The integration's history: what was forecast and planned, what control
//! did, and what was measured, in a `vulpes-core` store. The file has the
//! same format and series keys as the CLI's `local/<house>/vulpes.db`, so a
//! copy works with `vulpes-cli eval-*` and `replay`.
//!
//! [`record`] runs once per 15-minute tick; [`export`] reads a period back
//! for the diagnostics download. Both take an open store, so they are tested
//! on an in-memory one; [`record_file`] and [`export_file`] open the file.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use vulpes_core::store::keys;
use vulpes_core::{ForecastPoint, PlanSlotRecord, SeriesKind, Store};

/// Slot length (s).
const SLOT_S: i64 = 900;
/// Forecasts are kept this many hours ahead of their issue time.
pub const FORECAST_LEAD_H: i64 = 48;
/// Vulpes's own forecasts are kept this long (days); the review needs 30.
pub const FORECAST_KEEP_DAYS: i64 = 35;
/// "1 hour ahead" and "24 hours ahead": the first plan of the hour before is
/// issued just after it starts, so the lead is a slot short of the full hour.
pub const LEAD_LATEST_S: i64 = 3600 - SLOT_S;
pub const LEAD_DAY_AHEAD_S: i64 = 24 * 3600 - SLOT_S;
/// Measured series and slot records are kept this long (days): enough for a yearly replay.
pub const HISTORY_KEEP_DAYS: i64 = 400;
/// Measured hourly series are rewritten this far back on each tick, so late
/// or corrected recorder statistics are picked up.
pub const REWRITE_H: i64 = 48;

/// One planned slot as the plan had it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecordSlot {
    pub start: i64,
    /// Full engine mode name (`hold:no_export`).
    pub mode: String,
    pub pv_w: f64,
    pub load_w: f64,
    pub import_price: f64,
    pub export_price: f64,
    pub price_known: bool,
}

/// The plan of this tick.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecordPlan {
    pub planned_at: i64,
    pub terminal_value_per_kwh: f64,
    pub slots: Vec<RecordSlot>,
}

/// Measured hourly energy per flow (kWh per hour, keyed by hour start).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct EnergyHours {
    pub grid_import: Vec<(i64, f64)>,
    pub grid_export: Vec<(i64, f64)>,
    pub solar: Vec<(i64, f64)>,
    pub battery_charge: Vec<(i64, f64)>,
    pub battery_discharge: Vec<(i64, f64)>,
}

/// What happened this tick. `plan` is absent when planning failed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RecordRequest {
    pub now: i64,
    #[serde(default)]
    pub plan: Option<RecordPlan>,
    /// Measured state of charge (%).
    #[serde(default)]
    pub soc_pct: Option<f64>,
    /// The Active switch was on.
    #[serde(default)]
    pub active: bool,
    /// The control script ran the planned command without error.
    #[serde(default)]
    pub followed: bool,
    /// The Vulpes mode the user's mode entity showed; "" when its state maps
    /// to no mode, absent without a mode entity or when it is unknown.
    #[serde(default)]
    pub user_mode: Option<String>,
    #[serde(default)]
    pub energy: EnergyHours,
    /// Hourly house load excluding the battery (W).
    #[serde(default)]
    pub load: Vec<(i64, f64)>,
    /// Hourly wholesale prices (EUR/kWh).
    #[serde(default)]
    pub wholesale: Vec<(i64, f64)>,
    /// Start of the local day the response's `today` covers (Unix seconds).
    #[serde(default)]
    pub day_start: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecordResponse {
    /// Forecast points written (0 when this hour's forecasts were already there).
    pub forecasts_written: usize,
    /// Start of the newest hour of measured grid energy in the store.
    pub newest_energy_ts: Option<i64>,
    /// The slot records of the local day from `day_start`, up to now.
    pub today: Vec<PlanSlotRecord>,
    pub warnings: Vec<String>,
}

fn series(
    store: &mut Store,
    key: &str,
    unit: &str,
    kind: SeriesKind,
    step: i64,
    rows: &[(i64, f64)],
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    store.ensure_series(key, unit, kind)?;
    store.put_samples(key, step, rows)?;
    Ok(())
}

/// Hourly means of the plan's slots, for hours all four quarter hours of which are planned.
fn hourly_means(slots: &[RecordSlot], f: impl Fn(&RecordSlot) -> f64) -> BTreeMap<i64, f64> {
    let mut acc: BTreeMap<i64, (f64, usize)> = BTreeMap::new();
    for s in slots {
        let e = acc.entry(s.start - s.start.rem_euclid(3600)).or_insert((0.0, 0));
        e.0 += f(s);
        e.1 += 1;
    }
    acc.into_iter()
        .filter(|(_, (_, n))| *n == (3600 / SLOT_S) as usize)
        .map(|(h, (sum, n))| (h, sum / n as f64))
        .collect()
}

/// A forecast variable read from a planned slot.
type SlotValue = fn(&RecordSlot) -> f64;

/// Write the forecasts of the first plan of an hour: hourly means of PV,
/// load and import price, and whether the price was published, for leads
/// of 1 to [`FORECAST_LEAD_H`] hours.
fn record_forecasts(store: &mut Store, plan: &RecordPlan) -> Result<usize> {
    let hour = plan.planned_at - plan.planned_at.rem_euclid(3600);
    if store
        .latest_issue(keys::VULPES_SOURCE, "pv_w")?
        .is_some_and(|t| t >= hour)
    {
        return Ok(0);
    }
    let horizon = hour + 3600..=hour + FORECAST_LEAD_H * 3600;
    let vars: [(&str, SlotValue); 4] = [
        ("pv_w", |s| s.pv_w),
        ("load_w", |s| s.load_w),
        ("import_price", |s| s.import_price),
        ("price_known", |s| f64::from(u8::from(s.price_known))),
    ];
    let mut n = 0;
    for (var, f) in vars {
        let points: Vec<ForecastPoint> = hourly_means(&plan.slots, f)
            .into_iter()
            .filter(|(h, _)| horizon.contains(h))
            .map(|(valid_ts, value)| ForecastPoint {
                valid_ts,
                issued_ts: plan.planned_at,
                value,
            })
            .collect();
        n += store.put_forecasts(keys::VULPES_SOURCE, var, &points)?;
    }
    Ok(n)
}

fn prune(store: &mut Store, now: i64) -> Result<()> {
    store.prune_forecasts(keys::VULPES_SOURCE, now - FORECAST_KEEP_DAYS * 86_400)?;
    let before = now - HISTORY_KEEP_DAYS * 86_400;
    store.prune_plan_slots(before)?;
    for key in [
        keys::SOC,
        keys::GRID_IMPORT,
        keys::GRID_EXPORT,
        keys::PV_TOTAL,
        keys::BATTERY_CHARGE,
        keys::BATTERY_DISCHARGE,
        keys::LOAD_HOUSE,
        keys::PV_TOTAL_POWER,
        keys::WHOLESALE_NL,
    ] {
        store.prune_samples(key, before)?;
    }
    Ok(())
}

/// Record one tick.
pub fn record(store: &mut Store, req: &RecordRequest) -> Result<RecordResponse> {
    let slot = req.now - req.now.rem_euclid(SLOT_S);
    let mut warnings = Vec::new();
    let mut forecasts_written = 0;
    let current = req.plan.as_ref().and_then(|p| {
        p.slots
            .iter()
            .find(|s| s.start <= req.now && req.now < s.start + SLOT_S)
    });
    if let Some(plan) = &req.plan {
        forecasts_written = record_forecasts(store, plan)?;
        if current.is_none() {
            warnings.push("the plan has no slot for now".into());
        }
    }
    store.put_plan_slot(&PlanSlotRecord {
        ts: slot,
        mode: current.map(|s| s.mode.clone()),
        import_price: current.map(|s| s.import_price),
        export_price: current.map(|s| s.export_price),
        terminal_value: req.plan.as_ref().map(|p| p.terminal_value_per_kwh),
        active: req.active,
        followed: req.active && req.followed,
        user_mode: req.user_mode.clone(),
    })?;
    if let Some(soc) = req.soc_pct.filter(|s| s.is_finite()) {
        series(store, keys::SOC, "%", SeriesKind::Mean, SLOT_S, &[(slot, soc)])?;
    }
    let since = req.now - req.now.rem_euclid(3600) - REWRITE_H * 3600;
    let recent = |rows: &[(i64, f64)]| -> Vec<(i64, f64)> {
        rows.iter()
            .copied()
            .filter(|(t, v)| *t >= since && v.is_finite())
            .collect()
    };
    let e = &req.energy;
    for (key, rows) in [
        (keys::GRID_IMPORT, &e.grid_import),
        (keys::GRID_EXPORT, &e.grid_export),
        (keys::PV_TOTAL, &e.solar),
        (keys::BATTERY_CHARGE, &e.battery_charge),
        (keys::BATTERY_DISCHARGE, &e.battery_discharge),
    ] {
        series(store, key, "kWh", SeriesKind::Sum, 3600, &recent(rows))?;
    }
    let pv_power: Vec<(i64, f64)> = recent(&e.solar)
        .into_iter()
        .map(|(t, kwh)| (t, kwh * 1000.0))
        .collect();
    series(
        store,
        keys::PV_TOTAL_POWER,
        "W",
        SeriesKind::Mean,
        3600,
        &pv_power,
    )?;
    series(
        store,
        keys::LOAD_HOUSE,
        "W",
        SeriesKind::Mean,
        3600,
        &recent(&req.load),
    )?;
    // Wholesale prices: the recent past and whatever is published ahead.
    let wholesale: Vec<(i64, f64)> = req
        .wholesale
        .iter()
        .copied()
        .filter(|(t, v)| *t >= since && v.is_finite())
        .collect();
    series(
        store,
        keys::WHOLESALE_NL,
        "EUR/kWh",
        SeriesKind::Mean,
        3600,
        &wholesale,
    )?;
    if forecasts_written > 0 {
        prune(store, req.now)?;
    }
    let today = match req.day_start {
        Some(start) => store.plan_slots(start, slot + SLOT_S)?,
        None => Vec::new(),
    };
    Ok(RecordResponse {
        forecasts_written,
        newest_energy_ts: store.last_sample_ts(keys::GRID_IMPORT, 3600)?,
        today,
        warnings,
    })
}

/// A period to export.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExportRequest {
    pub from: i64,
    pub to: i64,
}

/// The history of a period, for the diagnostics download.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExportResponse {
    pub from: i64,
    pub to: i64,
    pub plan_slots: Vec<PlanSlotRecord>,
    /// Series key → `[start, value]`, with the series' step.
    pub samples: BTreeMap<String, Vec<(i64, f64)>>,
    /// Forecast variable → the forecasts the review scores, `[valid, value]`
    /// at about a day and an hour ahead (the file keeps every issue).
    pub forecasts: BTreeMap<String, ForecastLeads>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ForecastLeads {
    pub day_ahead: Vec<(i64, f64)>,
    pub latest: Vec<(i64, f64)>,
}

/// Read a period of history back.
pub fn export(store: &Store, req: &ExportRequest) -> Result<ExportResponse> {
    let mut samples = BTreeMap::new();
    for (key, step) in [
        (keys::SOC, SLOT_S),
        (keys::GRID_IMPORT, 3600),
        (keys::GRID_EXPORT, 3600),
        (keys::PV_TOTAL, 3600),
        (keys::BATTERY_CHARGE, 3600),
        (keys::BATTERY_DISCHARGE, 3600),
        (keys::LOAD_HOUSE, 3600),
        (keys::WHOLESALE_NL, 3600),
    ] {
        samples.insert(key.to_string(), store.samples(key, step, req.from, req.to)?);
    }
    let mut forecasts = BTreeMap::new();
    for var in ["pv_w", "load_w", "import_price", "price_known"] {
        let at = |lead| store.forecasts_at_lead(keys::VULPES_SOURCE, var, req.from, req.to, lead);
        forecasts.insert(
            var.to_string(),
            ForecastLeads {
                day_ahead: at(LEAD_DAY_AHEAD_S)?,
                latest: at(LEAD_LATEST_S)?,
            },
        );
    }
    Ok(ExportResponse {
        from: req.from,
        to: req.to,
        plan_slots: store.plan_slots(req.from, req.to)?,
        samples,
        forecasts,
    })
}

/// Open (or create, with its folder) the history store at `path`.
pub fn open(path: &str) -> Result<Store> {
    if path.is_empty() {
        return Err(Error::Request("no history file given".into()));
    }
    if let Some(dir) = Path::new(path).parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Request(format!("cannot create {}: {e}", dir.display())))?;
    }
    Ok(Store::open(path)?)
}

/// Record a tick in the history file at `path` (created with its folder).
pub fn record_file(path: &str, req: &RecordRequest) -> Result<RecordResponse> {
    let mut store = open(path)?;
    let resp = record(&mut store, req)?;
    store.checkpoint()?;
    Ok(resp)
}

/// Export a period of the history file at `path`.
pub fn export_file(path: &str, req: &ExportRequest) -> Result<ExportResponse> {
    export(&open(path)?, req)
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3600;
    const T0: i64 = 1_794_139_200; // 2026-11-08 12:00 UTC

    fn slot(start: i64, mode: &str) -> RecordSlot {
        RecordSlot {
            start,
            mode: mode.into(),
            pv_w: 1000.0,
            load_w: 400.0,
            import_price: 0.25,
            export_price: 0.05,
            price_known: start < T0 + 12 * H,
        }
    }

    fn plan(at: i64, hours: i64) -> RecordPlan {
        let start = at - at.rem_euclid(SLOT_S);
        RecordPlan {
            planned_at: at,
            terminal_value_per_kwh: 0.2,
            slots: (0..hours * 4)
                .map(|k| slot(start + k * SLOT_S, "hold:no_export"))
                .collect(),
        }
    }

    fn request(now: i64) -> RecordRequest {
        RecordRequest {
            now,
            plan: Some(plan(now, 84)),
            soc_pct: Some(55.0),
            active: true,
            followed: true,
            user_mode: Some("hold".into()),
            energy: EnergyHours {
                grid_import: (0..72).map(|h| (T0 - 72 * H + h * H, 0.4)).collect(),
                solar: vec![(T0 - H, 1.5)],
                ..Default::default()
            },
            load: vec![(T0 - H, 450.0)],
            wholesale: vec![(T0 - H, 0.1), (T0 + 10 * H, 0.12), (T0 - 100 * H, 0.3)],
            day_start: Some(T0 - 12 * H),
        }
    }

    #[test]
    fn a_tick_writes_slot_soc_energy_and_forecasts() {
        let mut s = Store::open_in_memory().unwrap();
        let resp = record(&mut s, &request(T0 + 20)).unwrap();
        // 48 lead hours × 4 variables.
        assert_eq!(resp.forecasts_written, 4 * 48);
        assert_eq!(resp.newest_energy_ts, Some(T0 - H));
        assert!(resp.warnings.is_empty());
        let slots = s.plan_slots(T0, T0 + SLOT_S).unwrap();
        assert_eq!(
            slots,
            [PlanSlotRecord {
                ts: T0,
                mode: Some("hold:no_export".into()),
                import_price: Some(0.25),
                export_price: Some(0.05),
                terminal_value: Some(0.2),
                active: true,
                followed: true,
                user_mode: Some("hold".into()),
            }]
        );
        assert_eq!(resp.today, slots);
        assert_eq!(s.samples(keys::SOC, SLOT_S, T0, T0 + 1).unwrap(), [(T0, 55.0)]);
        // Only the last 48 hours of energy are (re)written.
        assert_eq!(s.samples(keys::GRID_IMPORT, H, 0, i64::MAX).unwrap().len(), 48);
        assert_eq!(
            s.samples(keys::PV_TOTAL_POWER, H, 0, i64::MAX).unwrap(),
            [(T0 - H, 1500.0)]
        );
        assert_eq!(
            s.samples(keys::LOAD_HOUSE, H, 0, i64::MAX).unwrap(),
            [(T0 - H, 450.0)]
        );
        assert_eq!(
            s.samples(keys::WHOLESALE_NL, H, 0, i64::MAX).unwrap(),
            [(T0 - H, 0.1), (T0 + 10 * H, 0.12)]
        );
        // Forecasts by lead: the plan issued at 12:00:20 is the "1 hour ahead" forecast of
        // 13:00 (just under an hour) and the "24 hours ahead" one of 12:00 the next day.
        let lead = |h: i64| h * H - SLOT_S;
        assert_eq!(
            s.forecasts_at_lead(keys::VULPES_SOURCE, "pv_w", T0 + H, T0 + 2 * H, lead(1))
                .unwrap(),
            [(T0 + H, 1000.0)]
        );
        assert_eq!(
            s.forecasts_at_lead(
                keys::VULPES_SOURCE,
                "price_known",
                T0 + 24 * H,
                T0 + 25 * H,
                lead(24)
            )
            .unwrap(),
            [(T0 + 24 * H, 0.0)]
        );
        // Not the current hour (lead 0), nothing beyond 48 hours.
        assert!(
            s.forecasts_range(keys::VULPES_SOURCE, "pv_w", T0, T0 + H)
                .unwrap()
                .is_empty()
        );
        assert!(
            s.forecasts_range(keys::VULPES_SOURCE, "pv_w", T0 + 49 * H, i64::MAX)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn forecasts_once_per_hour_and_idempotent_ticks() {
        let mut s = Store::open_in_memory().unwrap();
        assert!(record(&mut s, &request(T0 + 20)).unwrap().forecasts_written > 0);
        // Later ticks in the same hour write no forecasts; the same tick twice changes nothing.
        assert_eq!(
            record(&mut s, &request(T0 + 900 + 20)).unwrap().forecasts_written,
            0
        );
        let before = export(
            &s,
            &ExportRequest {
                from: 0,
                to: i64::MAX,
            },
        )
        .unwrap();
        record(&mut s, &request(T0 + 900 + 20)).unwrap();
        assert_eq!(
            export(
                &s,
                &ExportRequest {
                    from: 0,
                    to: i64::MAX
                }
            )
            .unwrap(),
            before
        );
        // The next hour writes again.
        assert!(record(&mut s, &request(T0 + H + 20)).unwrap().forecasts_written > 0);
    }

    #[test]
    fn a_failed_plan_still_records_control_and_measurements() {
        let mut s = Store::open_in_memory().unwrap();
        let mut r = request(T0 + 20);
        r.plan = None;
        r.followed = false;
        let resp = record(&mut s, &r).unwrap();
        assert_eq!(resp.forecasts_written, 0);
        let slot = &s.plan_slots(T0, T0 + 1).unwrap()[0];
        assert_eq!(
            (slot.mode.as_deref(), slot.active, slot.followed),
            (None, true, false)
        );
        assert_eq!(s.samples(keys::SOC, SLOT_S, T0, T0 + 1).unwrap(), [(T0, 55.0)]);
    }

    #[test]
    fn followed_needs_active_and_a_plan_slot_for_now() {
        let mut s = Store::open_in_memory().unwrap();
        let mut r = request(T0 + 20);
        r.active = false;
        record(&mut s, &r).unwrap();
        assert!(!s.plan_slots(T0, T0 + 1).unwrap()[0].followed);
        let mut late = request(T0 + 20);
        late.plan = Some(plan(T0 - 10 * H, 2));
        let resp = record(&mut s, &late).unwrap();
        assert_eq!(resp.warnings, ["the plan has no slot for now"]);
        assert_eq!(s.plan_slots(T0, T0 + 1).unwrap()[0].mode, None);
    }

    #[test]
    fn empty_tick_and_missing_series() {
        let mut s = Store::open_in_memory().unwrap();
        let r = RecordRequest {
            now: T0,
            plan: None,
            soc_pct: None,
            active: false,
            followed: false,
            user_mode: None,
            energy: EnergyHours::default(),
            load: vec![],
            wholesale: vec![],
            day_start: None,
        };
        let resp = record(&mut s, &r).unwrap();
        assert_eq!((resp.newest_energy_ts, resp.today.len()), (None, 0));
        assert_eq!(
            s.plan_slots(T0, T0 + 1).unwrap().len(),
            1,
            "the slot is recorded even so"
        );
    }

    #[test]
    fn old_rows_are_pruned_when_an_hour_starts() {
        let mut s = Store::open_in_memory().unwrap();
        record(&mut s, &request(T0 + 20)).unwrap();
        let later = T0 + 38 * 86_400 + 20;
        record(&mut s, &request(later)).unwrap();
        // Forecasts of the first tick are older than 35 days: gone. Slots and measurements stay.
        assert!(
            s.forecasts_range(keys::VULPES_SOURCE, "pv_w", 0, T0 + 49 * H)
                .unwrap()
                .is_empty()
        );
        assert_eq!(s.plan_slots(T0, T0 + 1).unwrap().len(), 1);
        let much_later = T0 + 401 * 86_400 + 20;
        record(&mut s, &request(much_later)).unwrap();
        assert!(s.plan_slots(T0, T0 + 1).unwrap().is_empty());
        assert!(s.samples(keys::SOC, SLOT_S, T0, T0 + 1).unwrap().is_empty());
    }

    #[test]
    fn hourly_means_need_whole_hours() {
        let slots: Vec<RecordSlot> = (0..6).map(|k| slot(T0 + k * SLOT_S, "x")).collect();
        let m = hourly_means(&slots, |s| s.pv_w);
        assert_eq!(m.len(), 1, "the second hour has only two quarters");
    }

    #[test]
    fn export_covers_the_period() {
        let mut s = Store::open_in_memory().unwrap();
        record(&mut s, &request(T0 + 20)).unwrap();
        let e = export(
            &s,
            &ExportRequest {
                from: T0,
                to: T0 + 2 * H,
            },
        )
        .unwrap();
        assert_eq!(e.plan_slots.len(), 1);
        assert_eq!(e.samples[keys::SOC], [(T0, 55.0)]);
        assert_eq!(e.forecasts["pv_w"].latest, [(T0 + H, 1000.0)]);
        assert!(
            e.forecasts["pv_w"].day_ahead.is_empty(),
            "issued only an hour ahead"
        );
        assert!(
            e.samples[keys::GRID_IMPORT].is_empty(),
            "measured energy is before the period"
        );
    }

    #[test]
    fn json_round_trips() {
        let r = request(T0);
        let back: RecordRequest = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        assert!(serde_json::from_str::<RecordRequest>(r#"{"now": 1, "bogus": 2}"#).is_err());
        let mut s = Store::open_in_memory().unwrap();
        let resp = record(&mut s, &r).unwrap();
        let back: RecordResponse = serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn files_are_created_with_their_folder() {
        let dir = std::env::temp_dir().join(format!("vulpes-history-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("entry").join("vulpes.db");
        let path = path.to_str().unwrap();
        let out = record_file(path, &request(T0)).unwrap();
        assert_eq!(out.forecasts_written, 4 * 48);
        let back = export_file(
            path,
            &ExportRequest {
                from: 0,
                to: i64::MAX,
            },
        )
        .unwrap();
        assert_eq!(back.plan_slots.len(), 1);
        assert!(record_file("", &request(T0)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
