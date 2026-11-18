//! How well Vulpes performs, from the history file: what it saved against
//! plain self-consumption, how good the forecasts were, and whether the
//! battery followed the plan. See `docs/monitoring.md`.
//!
//! Costs are settled per complete local day with [`vulpes_opt::slot_cost`],
//! and the counterfactuals run through [`vulpes_opt::step`], the one slot
//! physics shared with the optimizer and the CLI replay.

use crate::history::{LEAD_DAY_AHEAD_S, LEAD_LATEST_S, open};
use crate::{Error, ExportTariff, Result};
use chrono::{Days, NaiveDate};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use vulpes_core::store::keys;
use vulpes_core::{BatteryParams, Behaviour, ErrorStats, GridLimits, ModeDef, PlanSlotRecord, Store};
use vulpes_opt::{slot_cost, step};
use vulpes_price::LinearTariff;

const SLOT_S: i64 = 900;
const HOUR: i64 = 3600;
/// Days the review looks back.
pub const LOOKBACK_DAYS: u64 = 30;
/// A window's figures need this many complete (or scored) days.
pub const MIN_DAYS: usize = 3;
/// A day still incomplete this long after it ended is given up on for the totals.
pub const GIVE_UP_S: i64 = 48 * HOUR;
/// SOC tracking error above this (percentage points per hour) is degraded.
pub const SOC_TRACKING_LIMIT_PCT: f64 = 5.0;
/// SOC tracking needs this many scored hours.
pub const MIN_SOC_HOURS: usize = 12;

const META_THROUGH: &str = "review.finalised_through";
const META_SAVED_ACTUAL: &str = "review.total_saved_actual";
const META_SAVED_PLAN: &str = "review.total_saved_plan";
const META_SINCE: &str = "review.since";
/// The last response, and what it was computed from (see [`cache_key`]).
const META_CACHE: &str = "review.cache";
const META_CACHE_KEY: &str = "review.cache_key";

/// What the review needs besides the history: the current settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRequest {
    pub now: i64,
    pub timezone: String,
    pub battery: BatteryParams,
    pub grid: GridLimits,
    pub modes: Vec<ModeDef>,
    pub tariff: LinearTariff,
    pub export: ExportTariff,
    /// Which measured series the settings provide.
    #[serde(default)]
    pub has_export: bool,
    #[serde(default)]
    pub has_solar: bool,
    #[serde(default)]
    pub has_battery: bool,
    /// Solar planes are configured.
    #[serde(default)]
    pub has_pv: bool,
}

/// MAE, RMSE and bias, in the unit of the series.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Score {
    pub n: usize,
    pub mae: f64,
    pub rmse: f64,
    pub bias: f64,
}

impl Score {
    fn of(e: &ErrorStats) -> Option<Self> {
        Some(Self {
            n: e.n,
            mae: e.mae()?,
            rmse: e.rmse()?,
            bias: e.bias()?,
        })
    }
}

/// Forecast quality of PV or load (W).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ForecastScores {
    /// Issued at least a day ahead: what the plan's big decisions rest on.
    pub day_ahead: Option<Score>,
    /// Issued in the hour before.
    pub latest: Option<Score>,
    /// The same hour the day before, measured: the naive baseline.
    pub persistence: Option<Score>,
    pub hours: usize,
    pub days: usize,
    /// Mean measured power over the scored hours (W).
    pub mean_w: Option<f64>,
    /// Day-ahead MAE and bias as % of `mean_w`.
    pub error_pct: Option<f64>,
    pub bias_pct: Option<f64>,
    /// 1 − RMSE(day-ahead) / RMSE(persistence), on the hours both have.
    pub skill: Option<f64>,
}

/// Forecast quality of import prices that were not yet published a day ahead (currency/kWh).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PriceScores {
    pub day_ahead: Option<Score>,
    pub persistence: Option<Score>,
    pub hours: usize,
}

/// Whether control ran the plan, and whether the battery did what the plan expected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ControlScores {
    pub slots_active: usize,
    pub slots_followed: usize,
    pub uptime_pct: Option<f64>,
    /// Hours where control followed the plan all hour and SOC was measured at both ends.
    pub soc_hours: usize,
    /// Replayed vs measured SOC change per hour (percentage points).
    pub soc_mae_pct: Option<f64>,
    pub soc_bias_pct: Option<f64>,
}

/// Figures over a run of days.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Window {
    /// Complete days (with costs) in the window.
    pub days: usize,
    /// Share of the window's slots with the Active switch on (%).
    pub active_share_pct: Option<f64>,
    /// Self-consumption cost minus actual cost, corrected for the energy left in the battery.
    pub saved_actual: Option<f64>,
    /// The same for the plan replayed on measured load and PV.
    pub saved_plan: Option<f64>,
    pub cost_baseline: Option<f64>,
    pub cost_actual: Option<f64>,
    pub cost_plan: Option<f64>,
    pub solar: ForecastScores,
    pub load: ForecastScores,
    pub price: PriceScores,
    pub control: ControlScores,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Windows {
    pub yesterday: Window,
    pub days_7: Window,
    pub days_30: Window,
}

/// Costs of one local day.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DayResult {
    pub date: String,
    pub start: i64,
    pub complete: bool,
    pub cost_baseline: Option<f64>,
    pub cost_actual: Option<f64>,
    pub cost_plan: Option<f64>,
    pub saved_actual: Option<f64>,
    pub saved_plan: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Totals {
    /// The first day counted.
    pub since: Option<String>,
    /// The running totals; `None` until the first day is counted.
    pub saved_actual: Option<f64>,
    pub saved_plan: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Check {
    pub check: String,
    /// `ok`, `degraded` or `not_enough_data`.
    pub state: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewResponse {
    pub version: String,
    pub days: Vec<DayResult>,
    pub windows: Windows,
    pub totals: Totals,
    pub checks: Vec<Check>,
    pub warnings: Vec<String>,
}

/// Everything read from the store for the period under review.
struct Data {
    hourly: HashMap<&'static str, HashMap<i64, f64>>,
    soc: HashMap<i64, f64>,
    slots: HashMap<i64, PlanSlotRecord>,
    fc_day_ahead: HashMap<&'static str, HashMap<i64, f64>>,
    fc_latest: HashMap<&'static str, HashMap<i64, f64>>,
}

impl Data {
    fn load(store: &Store, from: i64, to: i64) -> Result<Self> {
        let mut hourly = HashMap::new();
        for key in [
            keys::GRID_IMPORT,
            keys::GRID_EXPORT,
            keys::PV_TOTAL,
            keys::BATTERY_CHARGE,
            keys::BATTERY_DISCHARGE,
            keys::LOAD_HOUSE,
            keys::WHOLESALE_NL,
        ] {
            hourly.insert(key, store.samples(key, HOUR, from, to)?.into_iter().collect());
        }
        let soc = store
            .samples(keys::SOC, SLOT_S, from, to + HOUR)?
            .into_iter()
            .collect();
        let slots = store
            .plan_slots(from, to)?
            .into_iter()
            .map(|s| (s.ts, s))
            .collect();
        let mut fc_day_ahead = HashMap::new();
        let mut fc_latest = HashMap::new();
        for var in ["pv_w", "load_w", "import_price", "price_known"] {
            let da = store.forecasts_at_lead(keys::VULPES_SOURCE, var, from, to, LEAD_DAY_AHEAD_S)?;
            fc_day_ahead.insert(var, da.into_iter().collect());
            let l = store.forecasts_at_lead(keys::VULPES_SOURCE, var, from, to, LEAD_LATEST_S)?;
            fc_latest.insert(var, l.into_iter().collect());
        }
        Ok(Self {
            hourly,
            soc,
            slots,
            fc_day_ahead,
            fc_latest,
        })
    }

    fn get(&self, key: &str, hour: i64) -> Option<f64> {
        self.hourly.get(key)?.get(&hour).copied()
    }

    /// Measured SOC (%) at `ts`: the sample of that slot, else the nearest within half an hour.
    fn soc_at(&self, ts: i64) -> Option<f64> {
        self.soc.get(&ts).copied().or_else(|| {
            [-SLOT_S, SLOT_S, -2 * SLOT_S, 2 * SLOT_S]
                .iter()
                .find_map(|d| self.soc.get(&(ts + d)).copied())
        })
    }
}

/// The measured flows and prices of one hour, when complete.
struct Hour {
    load_w: f64,
    pv_w: f64,
    import_kwh: f64,
    export_kwh: f64,
    throughput_kwh: f64,
    /// Import and export price of each of the hour's slots.
    prices: Vec<(f64, f64)>,
}

struct Review<'a> {
    req: &'a ReviewRequest,
    data: &'a Data,
    self_consume: ModeDef,
}

impl Review<'_> {
    /// The import and export price of the slot at `ts`, as the plan had it. A slot without
    /// a plan (a failed tick, Home Assistant down) takes, in this order: the hour's published
    /// price from an earlier plan's forecasts, the wholesale price through the tariff, or the
    /// mean of the hour's other recorded slots. Only an hour with none of these is unknown.
    fn price(&self, ts: i64) -> Option<(f64, f64)> {
        let recorded = |ts: i64| {
            let s = self.data.slots.get(&ts)?;
            s.import_price.zip(s.export_price)
        };
        if let Some(p) = recorded(ts) {
            return Some(p);
        }
        let r = self.req;
        let h = ts - ts.rem_euclid(HOUR);
        let latest = |var: &str| self.data.fc_latest.get(var).and_then(|m| m.get(&h)).copied();
        // All four quarter hours were published when the forecast was issued.
        if latest("price_known").is_some_and(|k| k >= 1.0)
            && let Some(p) =
                latest("import_price").and_then(|i| Some((i, r.export.from_import(i, &r.tariff)?)))
        {
            return Some(p);
        }
        if let Some(w) = self.data.get(keys::WHOLESALE_NL, h) {
            return Some((r.tariff.price(w), r.export.a * w + r.export.b));
        }
        let known: Vec<(f64, f64)> = (0..HOUR / SLOT_S)
            .filter_map(|k| recorded(h + k * SLOT_S))
            .collect();
        let n = known.len() as f64;
        (n > 0.0).then(|| {
            (
                known.iter().map(|p| p.0).sum::<f64>() / n,
                known.iter().map(|p| p.1).sum::<f64>() / n,
            )
        })
    }

    fn hour(&self, h: i64) -> Option<Hour> {
        let r = self.req;
        let d = self.data;
        let pv_kwh = match (r.has_solar, r.has_pv) {
            (true, _) => d.get(keys::PV_TOTAL, h)?,
            (false, false) => 0.0,
            (false, true) => return None, // PV without statistics: the load is unknown
        };
        // Without battery statistics the house load includes the battery's own power: the
        // costs and savings would run a second battery on top of it, so they stay unknown.
        if !r.has_battery {
            return None;
        }
        let throughput = d.get(keys::BATTERY_CHARGE, h)? + d.get(keys::BATTERY_DISCHARGE, h)?;
        let prices: Option<Vec<(f64, f64)>> =
            (0..HOUR / SLOT_S).map(|k| self.price(h + k * SLOT_S)).collect();
        Some(Hour {
            load_w: d.get(keys::LOAD_HOUSE, h)?,
            pv_w: pv_kwh * 1000.0,
            import_kwh: d.get(keys::GRID_IMPORT, h)?,
            export_kwh: if r.has_export {
                d.get(keys::GRID_EXPORT, h)?
            } else {
                0.0
            },
            throughput_kwh: throughput,
            prices: prices?,
        })
    }

    fn mode_at(&self, ts: i64) -> &ModeDef {
        let name = self.data.slots.get(&ts).and_then(|s| s.mode.as_deref());
        name.and_then(|n| self.req.modes.iter().find(|m| m.name == n))
            .unwrap_or(&self.self_consume)
    }

    /// Simulate `[start, end)` from `soc0_wh` with a mode per slot; returns (cost, end SOC Wh).
    fn simulate(&self, hours: &[(i64, Hour)], soc0_wh: f64, mode: &dyn Fn(i64) -> ModeDef) -> (f64, f64) {
        let b = &self.req.battery;
        let mut soc = soc0_wh;
        let mut cost = 0.0;
        for (h, hr) in hours {
            for (k, (imp, exp)) in hr.prices.iter().enumerate() {
                let ts = h + k as i64 * SLOT_S;
                let o = step(&mode(ts), hr.load_w, hr.pv_w, soc, b, &self.req.grid, 0.25);
                cost += slot_cost(o.grid_w, o.battery_w, *imp, *exp, 0.25, b.wear_cost_per_kwh);
                soc = o.soc_wh;
            }
        }
        (cost, soc)
    }

    fn day(&self, date: NaiveDate, start: i64, end: i64) -> DayResult {
        let blank = DayResult {
            date: date.to_string(),
            start,
            complete: false,
            cost_baseline: None,
            cost_actual: None,
            cost_plan: None,
            saved_actual: None,
            saved_plan: None,
        };
        let hours: Option<Vec<(i64, Hour)>> = (0..(end - start) / HOUR)
            .map(|k| start + k * HOUR)
            .map(|h| self.hour(h).map(|hr| (h, hr)))
            .collect();
        let cap = self.req.battery.capacity_wh;
        let (Some(hours), Some(soc0), Some(soc1)) = (hours, self.data.soc_at(start), self.data.soc_at(end))
        else {
            return blank;
        };
        let (soc0, soc1) = (soc0 / 100.0 * cap, soc1 / 100.0 * cap);
        let wear = self.req.battery.wear_cost_per_kwh;
        let actual: f64 = hours
            .iter()
            .map(|(_, hr)| {
                let n = hr.prices.len() as f64;
                let imp = hr.prices.iter().map(|p| p.0).sum::<f64>() / n;
                let exp = hr.prices.iter().map(|p| p.1).sum::<f64>() / n;
                slot_cost(hr.import_kwh * 1000.0, 0.0, imp, exp, 1.0, 0.0)
                    + slot_cost(
                        -hr.export_kwh * 1000.0,
                        hr.throughput_kwh * 1000.0,
                        imp,
                        exp,
                        1.0,
                        wear,
                    )
            })
            .sum();
        let sc = self.self_consume.clone();
        let (base, base_end) = self.simulate(&hours, soc0, &|_| sc.clone());
        let (plan, plan_end) = self.simulate(&hours, soc0, &|ts| self.mode_at(ts).clone());
        // Energy left in the battery is worth the terminal value of the day's last plan.
        let slots = (end - start) / SLOT_S;
        let tv = (0..slots)
            .rev()
            .find_map(|k| {
                self.data
                    .slots
                    .get(&(start + k * SLOT_S))
                    .and_then(|s| s.terminal_value)
            })
            .unwrap_or(0.0);
        let left = |soc_end: f64| (soc_end - soc1) / 1000.0 * tv;
        DayResult {
            complete: true,
            cost_baseline: Some(base),
            cost_actual: Some(actual),
            cost_plan: Some(plan),
            saved_actual: Some(base - actual - left(base_end)),
            saved_plan: Some(base - plan - (base_end - plan_end) / 1000.0 * tv),
            ..blank
        }
    }

    /// Forecast scores of `var` against measured `actual` over the hours of the local days `spans`.
    fn forecast(
        &self,
        var: &str,
        spans: &[(i64, i64)],
        actual: &dyn Fn(i64) -> Option<f64>,
        daylight_only: bool,
    ) -> ForecastScores {
        let (mut da, mut latest, mut persistence) = (
            ErrorStats::default(),
            ErrorStats::default(),
            ErrorStats::default(),
        );
        let (mut da_common, mut pe_common) = (ErrorStats::default(), ErrorStats::default());
        let (mut sum, mut n) = (0.0, 0usize);
        let mut days = std::collections::HashSet::new();
        for (start, h) in spans
            .iter()
            .flat_map(|&(s, e)| (s..e).step_by(HOUR as usize).map(move |h| (s, h)))
        {
            let Some(a) = actual(h) else { continue };
            let f_da = self.data.fc_day_ahead.get(var).and_then(|m| m.get(&h)).copied();
            if daylight_only && a <= 0.0 && f_da.is_none_or(|f| f <= 0.0) {
                continue;
            }
            let prev = actual(h - 24 * HOUR);
            if let Some(f) = f_da {
                da.add(f, a);
                sum += a;
                n += 1;
                days.insert(start); // local days, as the windows and MIN_DAYS count them
                if let Some(p) = prev {
                    da_common.add(f, a);
                    pe_common.add(p, a);
                }
            }
            if let Some(f) = self.data.fc_latest.get(var).and_then(|m| m.get(&h)) {
                latest.add(*f, a);
            }
            if let Some(p) = prev {
                persistence.add(p, a);
            }
        }
        let mean_w = (n > 0).then(|| sum / n as f64);
        let pct = |x: Option<f64>| x.zip(mean_w.filter(|m| *m > 0.0)).map(|(x, m)| 100.0 * x / m);
        let ready = days.len() >= MIN_DAYS;
        let skill = da_common
            .rmse()
            .zip(pe_common.rmse())
            .and_then(|(d, p)| (p > 0.0).then(|| 1.0 - d / p));
        ForecastScores {
            day_ahead: Score::of(&da).filter(|_| ready),
            latest: Score::of(&latest).filter(|_| ready),
            persistence: Score::of(&persistence).filter(|_| ready),
            hours: n,
            days: days.len(),
            mean_w: mean_w.filter(|_| ready),
            error_pct: pct(da.mae()).filter(|_| ready),
            bias_pct: pct(da.bias()).filter(|_| ready),
            skill: skill.filter(|_| ready),
        }
    }

    fn slot_mean_price(&self, h: i64) -> Option<f64> {
        let p: Option<Vec<f64>> = (0..HOUR / SLOT_S)
            .map(|k| {
                self.data
                    .slots
                    .get(&(h + k * SLOT_S))
                    .and_then(|s| s.import_price)
            })
            .collect();
        p.map(|p| p.iter().sum::<f64>() / p.len() as f64)
    }

    fn prices(&self, hours: &[i64]) -> PriceScores {
        let (mut da, mut pe) = (ErrorStats::default(), ErrorStats::default());
        let mut n = 0;
        for &h in hours {
            let Some(a) = self.slot_mean_price(h) else {
                continue;
            };
            let published = self
                .data
                .fc_day_ahead
                .get("price_known")
                .and_then(|m| m.get(&h))
                .copied();
            let Some(f) = self.data.fc_day_ahead.get("import_price").and_then(|m| m.get(&h)) else {
                continue;
            };
            // Only prices that were really forecast: not yet published a day ahead.
            if published.is_some_and(|k| k > 0.0) {
                continue;
            }
            n += 1;
            da.add(*f, a);
            if let Some(p) = self.slot_mean_price(h - 24 * HOUR) {
                pe.add(p, a);
            }
        }
        let ready = n >= MIN_DAYS * 6;
        PriceScores {
            day_ahead: Score::of(&da).filter(|_| ready),
            persistence: Score::of(&pe).filter(|_| ready),
            hours: n,
        }
    }

    fn control(&self, hours: &[i64]) -> ControlScores {
        let mut c = ControlScores::default();
        let mut soc = ErrorStats::default();
        let cap = self.req.battery.capacity_wh;
        for &h in hours {
            let slots: Vec<Option<&PlanSlotRecord>> = (0..HOUR / SLOT_S)
                .map(|k| self.data.slots.get(&(h + k * SLOT_S)))
                .collect();
            c.slots_active += slots.iter().flatten().filter(|s| s.active).count();
            c.slots_followed += slots.iter().flatten().filter(|s| s.active && s.followed).count();
            let all_followed = slots
                .iter()
                .all(|s| s.is_some_and(|s| s.active && s.followed && s.mode.is_some()));
            let (Some(s0), Some(s1)) = (self.data.soc.get(&h), self.data.soc.get(&(h + HOUR))) else {
                continue;
            };
            let Some(hr) = self.hour(h).filter(|_| all_followed) else {
                continue;
            };
            let (_, end) = self.simulate(&[(h, hr)], s0 / 100.0 * cap, &|ts| self.mode_at(ts).clone());
            soc.add(100.0 * end / cap - s0, s1 - s0);
        }
        c.uptime_pct = (c.slots_active > 0).then(|| 100.0 * c.slots_followed as f64 / c.slots_active as f64);
        c.soc_hours = soc.n;
        if soc.n >= MIN_SOC_HOURS {
            c.soc_mae_pct = soc.mae();
            c.soc_bias_pct = soc.bias();
        }
        c
    }

    fn window(&self, days: &[(NaiveDate, i64, i64, DayResult)]) -> Window {
        let complete: Vec<&DayResult> = days.iter().map(|d| &d.3).filter(|d| d.complete).collect();
        let spans: Vec<(i64, i64)> = days.iter().map(|(_, s, e, _)| (*s, *e)).collect();
        let hours: Vec<i64> = spans
            .iter()
            .flat_map(|&(s, e)| (s..e).step_by(HOUR as usize))
            .collect();
        let recorded: Vec<&PlanSlotRecord> = hours
            .iter()
            .flat_map(|h| (0..HOUR / SLOT_S).map(move |k| h + k * SLOT_S))
            .filter_map(|ts| self.data.slots.get(&ts))
            .collect();
        let slots_total = recorded.len();
        let active = recorded.iter().filter(|s| s.active).count();
        let ready = complete.len() >= MIN_DAYS || (days.len() == 1 && complete.len() == 1);
        let sum = |f: fn(&DayResult) -> Option<f64>| -> Option<f64> {
            ready.then(|| complete.iter().filter_map(|d| f(d)).sum())
        };
        let d = self.data;
        Window {
            days: complete.len(),
            active_share_pct: (slots_total > 0).then(|| 100.0 * active as f64 / slots_total as f64),
            saved_actual: sum(|d| d.saved_actual),
            saved_plan: sum(|d| d.saved_plan),
            cost_baseline: sum(|d| d.cost_baseline),
            cost_actual: sum(|d| d.cost_actual),
            cost_plan: sum(|d| d.cost_plan),
            solar: self.forecast(
                "pv_w",
                &spans,
                &|h| match (self.req.has_solar, self.req.has_pv) {
                    (true, true) => d.get(keys::PV_TOTAL, h).map(|kwh| kwh * 1000.0),
                    _ => None,
                },
                true,
            ),
            load: self.forecast("load_w", &spans, &|h| d.get(keys::LOAD_HOUSE, h), false),
            price: self.prices(&hours),
            control: self.control(&hours),
        }
    }
}

/// Start of local day `d`, on the whole UTC hour at or before local midnight: the hourly
/// samples and forecasts are keyed on UTC hours, also where the offset has 30 or 45 minutes.
/// (Slot records are per quarter hour, so agreement in the integration uses the exact
/// midnight of [`crate::time::local_midnight`].)
fn midnight(tz: Tz, d: NaiveDate) -> Option<i64> {
    let t = crate::time::local_midnight(tz, d)?;
    Some(t - t.rem_euclid(HOUR))
}

/// What a review's result depends on besides the stored history of past days: the
/// settings, the local date, and the newest hour of energy data. The figures cover
/// complete past days only, so they can change only when one of these does (new hourly
/// statistics come with every hour, so corrected ones are picked up within the hour).
fn cache_key(store: &Store, req: &ReviewRequest, today: NaiveDate) -> Result<String> {
    let settings = ReviewRequest {
        now: 0,
        ..req.clone()
    };
    let settings = serde_json::to_string(&settings).map_err(|e| Error::Request(e.to_string()))?;
    let newest = store.last_sample_ts(keys::GRID_IMPORT, HOUR)?;
    Ok(format!(
        "{}|{today}|{newest:?}|{settings}",
        env!("CARGO_PKG_VERSION")
    ))
}

/// Review the history up to `now`, and add newly complete days to the totals.
pub fn review(store: &mut Store, req: &ReviewRequest) -> Result<ReviewResponse> {
    Ok(review_cached(store, req)?.0)
}

/// [`review`], and whether it computed anything (false: the stored result was still valid).
fn review_cached(store: &mut Store, req: &ReviewRequest) -> Result<(ReviewResponse, bool)> {
    req.battery.validate()?;
    let tz: Tz = req
        .timezone
        .parse()
        .map_err(|_| Error::Request(format!("unknown time zone '{}'", req.timezone)))?;
    let today =
        crate::time::local_date(req.now, tz).ok_or_else(|| Error::Request("now is out of range".into()))?;
    let key = cache_key(store, req, today)?;
    if store.meta(META_CACHE_KEY)?.as_deref() == Some(key.as_str())
        && let Some(resp) = store
            .meta(META_CACHE)?
            .and_then(|c| serde_json::from_str::<ReviewResponse>(&c).ok())
    {
        return Ok((resp, false));
    }
    // Local days from LOOKBACK_DAYS ago to yesterday, plus the day before for persistence.
    let first = today - Days::new(LOOKBACK_DAYS);
    let dates: Vec<NaiveDate> = first.iter_days().take_while(|d| *d < today).collect();
    let bounds: Vec<(NaiveDate, i64, i64)> = dates
        .iter()
        .filter_map(|d| Some((*d, midnight(tz, *d)?, midnight(tz, d.succ_opt()?)?)))
        .collect();
    let from = bounds.first().map_or(req.now, |b| b.1) - 24 * HOUR;
    let to = bounds.last().map_or(req.now, |b| b.2);
    let data = Data::load(store, from, to)?;
    let r = Review {
        req,
        data: &data,
        self_consume: ModeDef {
            name: "self_consume".into(),
            behaviour: Behaviour::SelfConsume,
            pv_curtail: false,
            no_export: false,
        },
    };
    let days: Vec<(NaiveDate, i64, i64, DayResult)> = bounds
        .iter()
        .map(|&(d, s, e)| (d, s, e, r.day(d, s, e)))
        .collect();
    let tail = |n: usize| &days[days.len().saturating_sub(n)..];
    let windows = Windows {
        yesterday: r.window(tail(1)),
        days_7: r.window(tail(7)),
        days_30: r.window(tail(30)),
    };

    // Totals: add each newly complete day once; give up on days that stay incomplete.
    let through: Option<i64> = store.meta(META_THROUGH)?.and_then(|v| v.parse().ok());
    let (mut saved_actual, mut saved_plan) = (
        store
            .meta(META_SAVED_ACTUAL)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0),
        store
            .meta(META_SAVED_PLAN)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0),
    );
    let mut counting = store.meta(META_SINCE)?.is_some();
    for (date, start, end, day) in days.iter().filter(|d| through.is_none_or(|t| d.1 > t)) {
        let through = start.to_string();
        if day.complete {
            // One transaction per day: a crash can't count a day and forget that it did.
            let actual = saved_actual + day.saved_actual.unwrap_or(0.0);
            let plan = saved_plan + day.saved_plan.unwrap_or(0.0);
            let (actual_s, plan_s, since) = (actual.to_string(), plan.to_string(), date.to_string());
            let mut pairs = vec![
                (META_SAVED_ACTUAL, actual_s.as_str()),
                (META_SAVED_PLAN, plan_s.as_str()),
                (META_THROUGH, through.as_str()),
            ];
            if !counting {
                pairs.push((META_SINCE, since.as_str()));
            }
            store.set_meta_all(&pairs)?;
            (saved_actual, saved_plan, counting) = (actual, plan, true);
        } else if req.now - end < GIVE_UP_S {
            break; // may still complete: wait, so the days after it keep their order
        } else if counting {
            store.set_meta_all(&[(META_THROUGH, through.as_str())])?;
        } // else nothing counted yet: no need to remember skipped days
    }

    let mut warnings = Vec::new();
    let unknown = data
        .slots
        .values()
        .filter(|s| {
            s.mode
                .as_ref()
                .is_some_and(|m| !req.modes.iter().any(|d| &d.name == m))
        })
        .count();
    if unknown > 0 {
        warnings.push(format!(
            "{unknown} slots had a mode that is no longer enabled; replayed as self-consume"
        ));
    }
    let checks = checks(req, &windows);
    let resp = ReviewResponse {
        version: env!("CARGO_PKG_VERSION").into(),
        days: days.into_iter().map(|d| d.3).collect(),
        windows,
        totals: Totals {
            since: store.meta(META_SINCE)?,
            saved_actual: counting.then_some(saved_actual),
            saved_plan: counting.then_some(saved_plan),
        },
        checks,
        warnings,
    };
    let cached = serde_json::to_string(&resp).map_err(|e| Error::Request(e.to_string()))?;
    store.set_meta_all(&[(META_CACHE_KEY, key.as_str()), (META_CACHE, cached.as_str())])?;
    Ok((resp, true))
}

fn check(check: &str, state: &str, detail: impl Into<String>) -> Check {
    Check {
        check: check.into(),
        state: state.into(),
        detail: detail.into(),
    }
}

/// The review's health checks, on the last 7 days.
fn checks(req: &ReviewRequest, w: &Windows) -> Vec<Check> {
    let w7 = &w.days_7;
    let forecast = |name: &str, s: &ForecastScores| match (s.day_ahead, s.persistence, s.skill) {
        (Some(_), Some(_), Some(skill)) if skill < 0.0 => check(
            name,
            "degraded",
            format!(
                "day-ahead forecast worse than repeating yesterday (skill {:.0}%)",
                100.0 * skill
            ),
        ),
        (Some(_), _, _) => check(name, "ok", ""),
        _ => check(
            name,
            "not_enough_data",
            format!("{} of {MIN_DAYS} days scored", s.days),
        ),
    };
    let mut out = vec![forecast("load_forecast", &w7.load)];
    if req.has_pv {
        out.push(forecast("solar_forecast", &w7.solar));
    }
    out.push(match w7.control.soc_mae_pct {
        Some(e) if e > SOC_TRACKING_LIMIT_PCT => check(
            "soc_tracking",
            "degraded",
            format!("the battery's SOC differed {e:.1} points per hour from the plan's physics"),
        ),
        Some(_) => check("soc_tracking", "ok", ""),
        None if w7.control.slots_active == 0 => check("soc_tracking", "ok", "not active"),
        None => check(
            "soc_tracking",
            "not_enough_data",
            format!("{} of {MIN_SOC_HOURS} hours scored", w7.control.soc_hours),
        ),
    });
    let mut missing = Vec::new();
    if req.has_pv && !req.has_solar {
        missing.push("solar energy statistics (needed for the house load and savings)");
    }
    if !req.has_battery {
        missing.push("battery charge/discharge statistics (needed for the house load and savings)");
    }
    out.push(if missing.is_empty() {
        check("coverage", "ok", "")
    } else {
        check("coverage", "degraded", format!("missing: {}", missing.join("; ")))
    });
    out
}

/// JSON entry point: review the history file at `path`.
pub fn review_json(path: &str, request: &str) -> Result<String> {
    let req: ReviewRequest = serde_json::from_str(request).map_err(|e| Error::Request(e.to_string()))?;
    let mut store = open(path)?;
    let (resp, computed) = review_cached(&mut store, &req)?;
    if computed {
        store.checkpoint()?;
    }
    serde_json::to_string(&resp).map_err(|e| Error::Request(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use vulpes_core::{ForecastPoint, SeriesKind};

    const AMS: Tz = chrono_tz::Europe::Amsterdam;

    fn midnight_of(y: i32, m: u32, d: u32) -> i64 {
        midnight(AMS, NaiveDate::from_ymd_opt(y, m, d).unwrap()).unwrap()
    }

    fn mode(name: &str, behaviour: Behaviour) -> ModeDef {
        ModeDef {
            name: name.into(),
            behaviour,
            pv_curtail: false,
            no_export: false,
        }
    }

    /// A lossless 10 kWh battery, a 1 kW house without PV, 0.10 at night (0-6 h) and 0.30 by day.
    fn request(now: i64) -> ReviewRequest {
        ReviewRequest {
            now,
            timezone: "Europe/Amsterdam".into(),
            battery: BatteryParams {
                capacity_wh: 10_000.0,
                max_charge_w: 3000.0,
                max_discharge_w: 3000.0,
                charge_eff: 1.0,
                discharge_eff: 1.0,
                min_soc: 0.0,
                max_soc: 1.0,
                wear_cost_per_kwh: 0.0,
            },
            grid: GridLimits {
                max_import_w: 17_250.0,
                max_export_w: 17_250.0,
            },
            modes: vec![
                mode("charge", Behaviour::GridSetpoint { w: 3000.0 }),
                mode("self_consume", Behaviour::SelfConsume),
            ],
            tariff: LinearTariff { a: 1.0, b: 0.0 },
            export: ExportTariff { a: 0.0, b: 0.05 },
            has_export: true,
            has_solar: false,
            has_battery: true,
            has_pv: false,
        }
    }

    fn price(ts: i64) -> f64 {
        let local_hour = DateTime::from_timestamp(ts, 0)
            .unwrap()
            .with_timezone(&AMS)
            .format("%H")
            .to_string();
        if local_hour.parse::<u32>().unwrap() < 6 {
            0.10
        } else {
            0.30
        }
    }

    /// Measured history of the local day starting at `start` (ending at `end`) where the
    /// house imported everything: the battery stayed at `soc0` %, and the plan said `plan_mode`.
    fn write_day(s: &mut Store, start: i64, end: i64, plan_mode: &dyn Fn(i64) -> &'static str, soc0: f64) {
        write_day_without(s, start, end, plan_mode, soc0, &|_| false);
    }

    /// [`write_day`] without slot records where `missing` (Home Assistant was down).
    fn write_day_without(
        s: &mut Store,
        start: i64,
        end: i64,
        plan_mode: &dyn Fn(i64) -> &'static str,
        soc0: f64,
        missing: &dyn Fn(i64) -> bool,
    ) {
        for (k, unit, kind) in [
            (keys::GRID_IMPORT, "kWh", SeriesKind::Sum),
            (keys::GRID_EXPORT, "kWh", SeriesKind::Sum),
            (keys::BATTERY_CHARGE, "kWh", SeriesKind::Sum),
            (keys::BATTERY_DISCHARGE, "kWh", SeriesKind::Sum),
            (keys::LOAD_HOUSE, "W", SeriesKind::Mean),
            (keys::SOC, "%", SeriesKind::Mean),
        ] {
            s.ensure_series(k, unit, kind).unwrap();
        }
        let hours: Vec<i64> = (start..end).step_by(HOUR as usize).collect();
        let rows = |v: f64| hours.iter().map(|h| (*h, v)).collect::<Vec<_>>();
        s.put_samples(keys::GRID_IMPORT, HOUR, &rows(1.0)).unwrap();
        s.put_samples(keys::GRID_EXPORT, HOUR, &rows(0.0)).unwrap();
        s.put_samples(keys::BATTERY_CHARGE, HOUR, &rows(0.0)).unwrap();
        s.put_samples(keys::BATTERY_DISCHARGE, HOUR, &rows(0.0)).unwrap();
        s.put_samples(keys::LOAD_HOUSE, HOUR, &rows(1000.0)).unwrap();
        let socs: Vec<(i64, f64)> = (start..=end)
            .step_by(SLOT_S as usize)
            .map(|t| (t, soc0))
            .collect();
        s.put_samples(keys::SOC, SLOT_S, &socs).unwrap();
        for ts in (start..end).step_by(SLOT_S as usize).filter(|t| !missing(*t)) {
            s.put_plan_slot(&PlanSlotRecord {
                ts,
                mode: Some(plan_mode(ts).into()),
                import_price: Some(price(ts)),
                export_price: Some(0.05),
                terminal_value: Some(0.2),
                active: true,
                followed: true,
                user_mode: None,
            })
            .unwrap();
        }
    }

    fn charge_at_night(ts: i64) -> &'static str {
        let h: u32 = DateTime::from_timestamp(ts, 0)
            .unwrap()
            .with_timezone(&AMS)
            .format("%H")
            .to_string()
            .parse()
            .unwrap();
        if h < 5 { "charge" } else { "self_consume" }
    }

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-9)
    }

    #[test]
    fn a_hand_computed_day_of_charging_cheap() {
        let mut s = Store::open_in_memory().unwrap();
        let (d0, d1) = (midnight_of(2026, 11, 10), midnight_of(2026, 11, 11));
        write_day(&mut s, d0, d1, &charge_at_night, 0.0);
        let resp = review(&mut s, &request(d1 + 12 * HOUR)).unwrap();
        let day = resp.days.last().unwrap();
        assert!(day.complete, "{day:?}");
        // Self-consumption with an empty battery imports all 24 kWh: 6 × 0.10 + 18 × 0.30.
        assert!(close(day.cost_baseline, 6.0), "{day:?}");
        // Measured: the same imports, battery still empty at the end.
        assert!(
            close(day.cost_actual, 6.0) && close(day.saved_actual, 0.0),
            "{day:?}"
        );
        // The plan charges 10 kWh at 0.10 (3 kWh/h for 5 hours), covers 10 hours from the battery,
        // and imports 9 hours at 0.30: 1.5 + 2.7 = 4.2, ending empty.
        assert!(close(day.cost_plan, 4.2) && close(day.saved_plan, 1.8), "{day:?}");
        assert_eq!(resp.windows.yesterday.days, 1);
        assert!(close(resp.windows.yesterday.saved_plan, 1.8));
        assert_eq!(resp.windows.days_7.saved_plan, None, "a week needs three days");
        assert!(resp.warnings.is_empty());
    }

    #[test]
    fn energy_left_in_the_battery_counts_at_the_terminal_value() {
        let mut s = Store::open_in_memory().unwrap();
        let (d0, d1) = (midnight_of(2026, 11, 10), midnight_of(2026, 11, 11));
        write_day(&mut s, d0, d1, &|_| "self_consume", 0.0);
        // The day ends with 5 kWh measured in the battery (50 %), the simulation empty.
        s.put_samples(keys::SOC, SLOT_S, &[(d1, 50.0)]).unwrap();
        let resp = review(&mut s, &request(d1 + 12 * HOUR)).unwrap();
        let day = resp.days.last().unwrap();
        // saved = baseline − actual − (sim end − actual end) × value = 0 − (0 − 5 kWh) × 0.2 = 1.0.
        assert!(close(day.saved_actual, 1.0), "{day:?}");
        assert!(close(day.saved_plan, 0.0), "{day:?}");
    }

    #[test]
    fn incomplete_days_are_left_out() {
        let mut s = Store::open_in_memory().unwrap();
        let days: Vec<i64> = (6..=11).map(|d| midnight_of(2026, 11, d)).collect();
        for w in days.windows(2) {
            write_day(&mut s, w[0], w[1], &charge_at_night, 0.0);
        }
        // A missing hour of load on one day, a missing SOC at the end of another.
        s.prune_samples(keys::LOAD_HOUSE, i64::MAX).unwrap();
        let rows: Vec<(i64, f64)> = (days[0]..days[5])
            .step_by(HOUR as usize)
            .filter(|h| *h != days[1] + 5 * HOUR)
            .map(|h| (h, 1000.0))
            .collect();
        s.put_samples(keys::LOAD_HOUSE, HOUR, &rows).unwrap();
        let resp = review(&mut s, &request(days[5] + 12 * HOUR)).unwrap();
        let complete: Vec<bool> = resp.days.iter().rev().take(5).map(|d| d.complete).collect();
        assert_eq!(complete, [true, true, true, false, true]);
        assert_eq!(resp.windows.days_7.days, 4);
        assert!(close(resp.windows.days_7.saved_plan, 4.0 * 1.8));
    }

    #[test]
    fn dst_days_have_23_and_25_hours() {
        // The changed hour is a night hour: 5 night hours in March, 7 in October.
        for (start, end, night, hours) in [
            ((2026, 3, 29), (2026, 3, 30), 5.0, 23.0),
            ((2026, 10, 25), (2026, 10, 26), 7.0, 25.0),
        ] {
            let mut s = Store::open_in_memory().unwrap();
            let (d0, d1) = (
                midnight_of(start.0, start.1, start.2),
                midnight_of(end.0, end.1, end.2),
            );
            write_day(&mut s, d0, d1, &|_| "self_consume", 0.0);
            let resp = review(&mut s, &request(d1 + 12 * HOUR)).unwrap();
            let day = resp.days.last().unwrap();
            assert!(day.complete);
            assert!(
                close(day.cost_baseline, night * 0.1 + (hours - night) * 0.3),
                "{hours}: {day:?}"
            );
        }
    }

    #[test]
    fn missing_statistics_and_an_empty_store() {
        let mut s = Store::open_in_memory().unwrap();
        let resp = review(&mut s, &request(midnight_of(2026, 11, 11))).unwrap();
        assert!(resp.days.iter().all(|d| !d.complete));
        assert_eq!(
            resp.totals,
            Totals {
                since: None,
                saved_actual: None,
                saved_plan: None
            }
        );
        let states: Vec<&str> = resp.checks.iter().map(|c| c.state.as_str()).collect();
        assert_eq!(states, ["not_enough_data", "ok", "ok"]);
        // PV planes without solar statistics: no costs, and the coverage check says why.
        let (d0, d1) = (midnight_of(2026, 11, 10), midnight_of(2026, 11, 11));
        write_day(&mut s, d0, d1, &|_| "self_consume", 0.0);
        let mut r = request(d1 + HOUR);
        r.has_pv = true;
        let resp = review(&mut s, &r).unwrap();
        assert!(!resp.days.last().unwrap().complete);
        let coverage = resp.checks.iter().find(|c| c.check == "coverage").unwrap();
        assert_eq!(coverage.state, "degraded");
        assert!(coverage.detail.contains("solar energy statistics"));
        // Without battery statistics the load includes the battery: no costs either.
        let mut r = request(d1 + HOUR);
        r.has_battery = false;
        let resp = review(&mut s, &r).unwrap();
        let day = resp.days.last().unwrap();
        assert!(!day.complete && day.saved_actual.is_none() && day.saved_plan.is_none());
        assert_eq!(resp.windows.yesterday.saved_actual, None);
        assert_eq!(resp.totals.since, None);
        let coverage = resp.checks.iter().find(|c| c.check == "coverage").unwrap();
        assert_eq!(coverage.state, "degraded");
        assert!(coverage.detail.contains("battery charge/discharge statistics"));
    }

    #[test]
    fn modes_no_longer_enabled_replay_as_self_consume() {
        let mut s = Store::open_in_memory().unwrap();
        let (d0, d1) = (midnight_of(2026, 11, 10), midnight_of(2026, 11, 11));
        write_day(&mut s, d0, d1, &|_| "discharge", 0.0);
        let resp = review(&mut s, &request(d1 + HOUR)).unwrap();
        assert!(close(resp.days.last().unwrap().saved_plan, 0.0));
        assert!(resp.warnings[0].contains("96 slots"), "{:?}", resp.warnings);
    }

    #[test]
    fn totals_count_each_day_once() {
        let mut s = Store::open_in_memory().unwrap();
        let d: Vec<i64> = (9..=12).map(|d| midnight_of(2026, 11, d)).collect();
        write_day(&mut s, d[0], d[1], &charge_at_night, 0.0);
        write_day(&mut s, d[1], d[2], &charge_at_night, 0.0);
        let first = review(&mut s, &request(d[2] + HOUR)).unwrap();
        assert_eq!(first.totals.since.as_deref(), Some("2026-11-09"));
        assert!((first.totals.saved_plan.unwrap() - 3.6).abs() < 1e-9);
        let again = review(&mut s, &request(d[2] + 2 * HOUR)).unwrap();
        assert_eq!(again.totals, first.totals);
        write_day(&mut s, d[2], d[3], &charge_at_night, 0.0);
        let next = review(&mut s, &request(d[3] + HOUR)).unwrap();
        assert!((next.totals.saved_plan.unwrap() - 5.4).abs() < 1e-9);
    }

    #[test]
    fn an_incomplete_day_holds_the_totals_until_it_is_given_up() {
        let mut s = Store::open_in_memory().unwrap();
        let d: Vec<i64> = (8..=11).map(|d| midnight_of(2026, 11, d)).collect();
        write_day(&mut s, d[0], d[1], &charge_at_night, 0.0);
        write_day(&mut s, d[2], d[3], &charge_at_night, 0.0); // the 9th is missing
        // The 10th is complete, but the 9th ended less than 48 h ago: wait.
        let soon = review(&mut s, &request(d[2] + 12 * HOUR)).unwrap();
        assert!((soon.totals.saved_plan.unwrap() - 1.8).abs() < 1e-9);
        // Two days on, the 9th is given up and the 10th counts.
        let later = review(&mut s, &request(d[3] + 26 * HOUR)).unwrap();
        assert!((later.totals.saved_plan.unwrap() - 3.6).abs() < 1e-9);
    }

    fn forecasts(s: &mut Store, var: &str, hours: &[i64], value: &dyn Fn(i64) -> f64, known: Option<f64>) {
        let pts: Vec<ForecastPoint> = hours
            .iter()
            .map(|h| ForecastPoint {
                valid_ts: *h,
                issued_ts: h - 25 * HOUR + 20,
                value: value(*h),
            })
            .collect();
        s.put_forecasts(keys::VULPES_SOURCE, var, &pts).unwrap();
        if let Some(k) = known {
            let pts: Vec<ForecastPoint> = pts.iter().map(|p| ForecastPoint { value: k, ..*p }).collect();
            s.put_forecasts(keys::VULPES_SOURCE, "price_known", &pts).unwrap();
        }
    }

    #[test]
    fn forecast_and_price_scores() {
        let mut s = Store::open_in_memory().unwrap();
        let d: Vec<i64> = (6..=11).map(|d| midnight_of(2026, 11, d)).collect();
        for w in d.windows(2) {
            write_day(&mut s, w[0], w[1], &|_| "self_consume", 0.0);
        }
        let hours: Vec<i64> = (d[0]..d[5]).step_by(HOUR as usize).collect();
        // A perfect load forecast; a price forecast 0.05 too high for the unpublished half.
        forecasts(&mut s, "load_w", &hours, &|_| 1000.0, None);
        let (unpublished, published): (Vec<i64>, Vec<i64>) = hours.iter().partition(|h| (*h / HOUR) % 2 == 0);
        forecasts(
            &mut s,
            "import_price",
            &unpublished,
            &|h| price(h) + 0.05,
            Some(0.0),
        );
        forecasts(&mut s, "import_price", &published, &|h| price(h) + 1.0, Some(1.0));
        let w = review(&mut s, &request(d[5] + 12 * HOUR)).unwrap().windows.days_7;
        let load = w.load.day_ahead.unwrap();
        assert_eq!((load.mae, load.rmse, load.bias), (0.0, 0.0, 0.0));
        assert_eq!(w.load.error_pct, Some(0.0));
        assert_eq!(
            w.load.persistence.unwrap().mae,
            0.0,
            "a flat load repeats perfectly"
        );
        // Only the unpublished hours are scored.
        assert_eq!(w.price.hours, unpublished.len());
        assert!((w.price.day_ahead.unwrap().bias - 0.05).abs() < 1e-9);
        // No PV planes: no solar scores.
        assert_eq!(w.solar.day_ahead, None);
        assert_eq!(w.control.uptime_pct, Some(100.0));
        assert_eq!(w.active_share_pct, Some(100.0));
    }

    /// The slot record at `ts` as a failed tick leaves it: no mode and no prices.
    fn failed_tick(s: &mut Store, ts: i64) {
        s.put_plan_slot(&PlanSlotRecord {
            ts,
            mode: None,
            import_price: None,
            export_price: None,
            terminal_value: None,
            active: true,
            followed: false,
            user_mode: None,
        })
        .unwrap();
    }

    #[test]
    fn a_slot_without_a_plan_takes_the_published_price_from_an_earlier_plan() {
        // No wholesale prices (any country but NL): before, this day was incomplete.
        let mut s = Store::open_in_memory().unwrap();
        let (d0, d1) = (midnight_of(2026, 11, 10), midnight_of(2026, 11, 11));
        // 08:00-09:00 local: one slot missing (a restart), and one failed tick.
        let h = d0 + 8 * HOUR;
        write_day_without(&mut s, d0, d1, &|_| "self_consume", 0.0, &|t| t == h + 2 * SLOT_S);
        failed_tick(&mut s, h + SLOT_S);
        // An earlier plan had the hour at a published 0.50 (the recorded slots say 0.30).
        forecasts(&mut s, "import_price", &[h], &|_| 0.50, Some(1.0));
        let day = review(&mut s, &request(d1 + HOUR)).unwrap().days.pop().unwrap();
        assert!(day.complete, "{day:?}");
        // 6 h × 0.10 + 17 h × 0.30, plus the hour: 2 slots at 0.30 and 2 at 0.50 (1 kWh/h).
        assert!(close(day.cost_baseline, 0.6 + 5.1 + 0.15 + 0.25), "{day:?}");
    }

    #[test]
    fn a_slot_without_a_plan_or_forecast_takes_the_mean_of_its_hour() {
        let mut s = Store::open_in_memory().unwrap();
        let (d0, d1) = (midnight_of(2026, 11, 10), midnight_of(2026, 11, 11));
        let h = d0 + 8 * HOUR;
        write_day_without(&mut s, d0, d1, &|_| "self_consume", 0.0, &|t| t == h + 3 * SLOT_S);
        failed_tick(&mut s, h);
        // An unpublished price forecast is not used.
        forecasts(&mut s, "import_price", &[h], &|_| 9.0, Some(0.0));
        let day = review(&mut s, &request(d1 + HOUR)).unwrap().days.pop().unwrap();
        assert!(day.complete && close(day.cost_baseline, 6.0), "{day:?}");
    }

    #[test]
    fn an_hour_without_any_price_leaves_the_day_incomplete() {
        let mut s = Store::open_in_memory().unwrap();
        let (d0, d1) = (midnight_of(2026, 11, 10), midnight_of(2026, 11, 11));
        write_day(&mut s, d0, d1, &|_| "self_consume", 0.0);
        let h = d0 + 8 * HOUR;
        for k in 0..4 {
            failed_tick(&mut s, h + k * SLOT_S);
        }
        let day = review(&mut s, &request(d1 + HOUR)).unwrap().days.pop().unwrap();
        assert!(!day.complete, "{day:?}");
        // With the wholesale price stored (it comes with the next hour's data), the tariff fills it in.
        s.ensure_series(keys::WHOLESALE_NL, "EUR/kWh", SeriesKind::Mean)
            .unwrap();
        s.put_samples(keys::WHOLESALE_NL, HOUR, &[(h, 0.30)]).unwrap();
        s.put_samples(keys::GRID_IMPORT, HOUR, &[(d1, 1.0)]).unwrap();
        let day = review(&mut s, &request(d1 + 2 * HOUR))
            .unwrap()
            .days
            .pop()
            .unwrap();
        assert!(day.complete && close(day.cost_baseline, 6.0), "{day:?}");
    }

    #[test]
    fn the_result_is_reused_until_new_data_a_new_day_or_other_settings() {
        let mut s = Store::open_in_memory().unwrap();
        let (d0, d1) = (midnight_of(2026, 11, 10), midnight_of(2026, 11, 11));
        write_day(&mut s, d0, d1, &|_| "self_consume", 0.0);
        let (first, computed) = review_cached(&mut s, &request(d1 + HOUR)).unwrap();
        assert!(computed);
        // A changed price of a past slot without new energy data: the stored result stands.
        let cheaper = PlanSlotRecord {
            ts: d0 + 8 * HOUR,
            mode: Some("self_consume".into()),
            import_price: Some(0.0),
            export_price: Some(0.05),
            terminal_value: Some(0.2),
            active: true,
            followed: true,
            user_mode: None,
        };
        s.put_plan_slot(&cheaper).unwrap();
        let (again, computed) = review_cached(&mut s, &request(d1 + 2 * HOUR)).unwrap();
        assert!(!computed);
        assert_eq!(again, first);
        // A new hour of energy data: computed again, with the new price.
        s.put_samples(keys::GRID_IMPORT, HOUR, &[(d1, 1.0)]).unwrap();
        let (fresh, computed) = review_cached(&mut s, &request(d1 + 2 * HOUR)).unwrap();
        assert!(computed);
        assert!(close(fresh.days.last().unwrap().cost_baseline, 6.0 - 0.3 / 4.0));
        // Other settings: computed again.
        let mut r = request(d1 + 2 * HOUR);
        r.battery.capacity_wh = 5000.0;
        assert!(review_cached(&mut s, &r).unwrap().1);
        assert!(!review_cached(&mut s, &r).unwrap().1);
        // A new day: computed again.
        assert!(review_cached(&mut s, &request(d1 + 26 * HOUR)).unwrap().1);
    }

    #[test]
    fn half_hour_time_zones_review_whole_utc_hours() {
        // Kolkata is UTC+5:30: local midnight is 18:30 UTC, the review day starts at 18:00.
        let tz: Tz = "Asia/Kolkata".parse().unwrap();
        let d: Vec<i64> = (6..=10)
            .map(|d| midnight(tz, NaiveDate::from_ymd_opt(2026, 11, d).unwrap()).unwrap())
            .collect();
        assert_eq!(d[0].rem_euclid(HOUR), 0);
        assert_eq!(d[1] - d[0], 24 * HOUR);
        let mut s = Store::open_in_memory().unwrap();
        for w in d.windows(2) {
            write_day(&mut s, w[0], w[1], &|_| "self_consume", 0.0);
        }
        let hours: Vec<i64> = (d[0]..d[4]).step_by(HOUR as usize).collect();
        forecasts(&mut s, "load_w", &hours, &|_| 1100.0, None);
        let mut r = request(d[4] + 12 * HOUR);
        r.timezone = "Asia/Kolkata".into();
        let resp = review(&mut s, &r).unwrap();
        let complete = resp.days.iter().filter(|d| d.complete).count();
        assert_eq!(complete, 4, "{:?}", resp.days);
        let load = resp.windows.days_7.load;
        assert_eq!((load.days, load.hours), (4, 96));
        assert!((load.day_ahead.unwrap().bias - 100.0).abs() < 1e-9);
    }

    #[test]
    fn scored_days_are_local_days() {
        // Two local days in New York (UTC-5) touch three UTC days: still two days, not enough.
        let tz: Tz = "America/New_York".parse().unwrap();
        let d: Vec<i64> = (9..=11)
            .map(|d| midnight(tz, NaiveDate::from_ymd_opt(2026, 11, d).unwrap()).unwrap())
            .collect();
        let mut s = Store::open_in_memory().unwrap();
        for w in d.windows(2) {
            write_day(&mut s, w[0], w[1], &|_| "self_consume", 0.0);
        }
        let hours: Vec<i64> = (d[0]..d[2]).step_by(HOUR as usize).collect();
        forecasts(&mut s, "load_w", &hours, &|_| 1000.0, None);
        let mut r = request(d[2] + 12 * HOUR);
        r.timezone = "America/New_York".into();
        let resp = review(&mut s, &r).unwrap();
        let load = &resp.windows.days_7.load;
        assert_eq!((load.days, load.hours), (2, 48));
        assert_eq!((load.day_ahead, load.error_pct), (None, None));
        let check = resp.checks.iter().find(|c| c.check == "load_forecast").unwrap();
        assert_eq!(check.detail, format!("2 of {MIN_DAYS} days scored"));
    }

    #[test]
    fn soc_tracking_compares_replayed_and_measured_soc() {
        let mut s = Store::open_in_memory().unwrap();
        let d: Vec<i64> = (6..=11).map(|d| midnight_of(2026, 11, d)).collect();
        for w in d.windows(2) {
            write_day(&mut s, w[0], w[1], &|_| "self_consume", 50.0);
        }
        // Self-consume with 1 kW load drains 10 % per hour; measured SOC stayed at 50 %.
        let w = review(&mut s, &request(d[5] + 12 * HOUR)).unwrap().windows.days_7;
        assert!(w.control.soc_hours >= MIN_SOC_HOURS);
        let e = w.control.soc_mae_pct.unwrap();
        assert!((e - 10.0).abs() < 1e-9, "{e}");
        let resp = review(&mut s, &request(d[5] + 12 * HOUR)).unwrap();
        assert_eq!(
            resp.checks
                .iter()
                .find(|c| c.check == "soc_tracking")
                .unwrap()
                .state,
            "degraded"
        );
    }

    #[test]
    fn json_round_trips_and_nothing_is_nan() {
        let mut s = Store::open_in_memory().unwrap();
        let (d0, d1) = (midnight_of(2026, 11, 10), midnight_of(2026, 11, 11));
        write_day(&mut s, d0, d1, &charge_at_night, 0.0);
        let r = request(d1 + HOUR);
        let back: ReviewRequest = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back.now, r.now);
        let resp = review(&mut s, &r).unwrap();
        let text = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<ReviewResponse>(&text).unwrap(), resp);
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        fn finite(v: &serde_json::Value) -> bool {
            match v {
                serde_json::Value::Number(n) => n.as_f64().is_some_and(f64::is_finite),
                serde_json::Value::Array(a) => a.iter().all(finite),
                serde_json::Value::Object(o) => o.values().all(finite),
                _ => true,
            }
        }
        assert!(finite(&v));
        assert!(serde_json::from_str::<ReviewRequest>(r#"{"now": 1}"#).is_err());
    }
}
