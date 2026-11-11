use serde::{Deserialize, Serialize};
use vulpes_core::{BatteryParams, GridLimits, ModeDef};
use vulpes_price::LinearTariff;
use vulpes_solar::PlaneParams;

fn default_step() -> i64 {
    900
}
fn default_horizon() -> u32 {
    84
}
fn default_models() -> Vec<String> {
    vec!["knmi_harmonie_arome_netherlands".into(), "icon_d2".into()]
}
fn default_tail_model() -> Option<String> {
    Some("ecmwf_ifs025".into())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Location {
    pub lat: f64,
    pub lon: f64,
}

/// Export pricing: `a · wholesale + b`. Equal to the import tariff's `a` and
/// `b` when export is paid at the import price (net metering).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ExportTariff {
    pub a: f64,
    pub b: f64,
}

/// A published all-in price for `[start, end)`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct KnownPrice {
    pub start: i64,
    pub end: i64,
    pub import: f64,
}

/// Weather for one slot (start time), when supplied by the caller instead of fetched.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WeatherSlot {
    pub start: i64,
    pub ghi: f64,
    pub dni: f64,
    pub dhi: f64,
    pub temp_air: f64,
    pub wind_speed: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanRequest {
    /// Unix seconds; planning starts at the slot containing `now`.
    pub now: i64,
    #[serde(default = "default_step")]
    pub step_s: i64,
    #[serde(default = "default_horizon")]
    pub horizon_h: u32,
    pub location: Location,
    /// IANA time zone, e.g. `Europe/Amsterdam`.
    pub timezone: String,
    /// Public-holiday calendar: `nl` or `none`.
    #[serde(default)]
    pub country: String,
    pub battery: BatteryParams,
    pub grid: GridLimits,
    pub modes: Vec<ModeDef>,
    pub soc_wh: f64,
    pub planes: Vec<PlaneParams>,
    #[serde(default = "default_models")]
    pub weather_models: Vec<String>,
    /// Longer-range weather model for the slots `weather_models` don't reach;
    /// `null` for none.
    #[serde(default = "default_tail_model")]
    pub weather_tail_model: Option<String>,
    /// Hourly house load history `(hour_start, W)`, excluding the battery.
    #[serde(default)]
    pub load_history: Vec<(i64, f64)>,
    #[serde(default)]
    pub known_prices: Vec<KnownPrice>,
    pub tariff: LinearTariff,
    pub export: ExportTariff,
    /// Hourly wholesale history `(hour_start, EUR/kWh)` for forecasting unpublished prices.
    #[serde(default)]
    pub wholesale_history: Vec<(i64, f64)>,
    /// Optional weather override (tests, offline use); otherwise fetched from Open-Meteo.
    #[serde(default)]
    pub weather: Option<Vec<WeatherSlot>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SlotPlan {
    pub start: i64,
    pub mode: String,
    pub soc_pct: f64,
    pub battery_w: f64,
    pub grid_w: f64,
    pub pv_w: f64,
    pub load_w: f64,
    pub import_price: f64,
    pub export_price: f64,
    pub price_known: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanSummary {
    pub mode_now: String,
    /// Expected cost over the horizon (excluding the terminal credit).
    pub cost: f64,
    pub pv_kwh_24h: f64,
    pub load_kwh_24h: f64,
    pub terminal_value_per_kwh: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanResponse {
    pub version: String,
    pub summary: PlanSummary,
    pub slots: Vec<SlotPlan>,
    pub warnings: Vec<String>,
}

pub use vulpes_solar::calibrate::Score;

/// Calibrate the PV planes from measured history (see `docs/solar.md`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrateRequest {
    pub location: Location,
    /// Current planes: the starting point, and the baseline the proposal is scored against.
    pub planes: Vec<PlaneParams>,
    /// Per plane, in the same order: hourly mean DC power `(hour_start, W)` of
    /// the string that feeds only that plane. An empty list = not measured.
    #[serde(default)]
    pub strings: Vec<Vec<(i64, f64)>>,
    /// Hourly mean AC power `(hour_start, W)` of the whole PV system.
    #[serde(default)]
    pub total: Vec<(i64, f64)>,
    #[serde(default = "default_models")]
    pub weather_models: Vec<String>,
    /// Short-lead weather for fitting (tests, offline use); otherwise fetched
    /// from the Open-Meteo historical-forecast API.
    #[serde(default)]
    pub weather: Option<Vec<WeatherSlot>>,
    /// Day-ahead weather for scoring; otherwise fetched from the Open-Meteo
    /// previous-runs API (`_previous_day1`).
    #[serde(default)]
    pub weather_day_ahead: Option<Vec<WeatherSlot>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitMethod {
    /// Orientation and capacity fitted to the plane's own string power.
    String,
    /// Capacity scaled to match the system total; orientation as entered.
    TotalScale,
    /// Not enough data: kept as entered.
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlaneFit {
    pub method: FitMethod,
    /// Clear hours the fit used.
    pub hours: usize,
    /// Fit error on those hours (W).
    pub rmse_w: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrateResponse {
    pub version: String,
    /// Proposed planes, same order as the request.
    pub planes: Vec<PlaneParams>,
    pub fits: Vec<PlaneFit>,
    /// Hourly forecast error of the current and the proposed planes, with
    /// day-ahead weather. `None` without a measurement to score against.
    pub score_current: Option<Score>,
    pub score_proposed: Option<Score>,
    /// Whether the scores are on held-out weeks (fit on even ISO weeks, scored
    /// on odd ones). False when there was too little data to split.
    pub held_out: bool,
    /// Measured data range used, `[from, to)` in Unix seconds.
    pub from: i64,
    pub to: i64,
    pub warnings: Vec<String>,
}
