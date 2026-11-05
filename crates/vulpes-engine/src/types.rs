use serde::{Deserialize, Serialize};
use vulpes_core::{BatteryParams, GridLimits, ModeDef};
use vulpes_price::LinearTariff;
use vulpes_solar::PlaneParams;

fn default_step() -> i64 {
    900
}
fn default_horizon() -> u32 {
    36
}
fn default_models() -> Vec<String> {
    vec!["knmi_harmonie_arome_netherlands".into(), "icon_d2".into()]
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Location {
    pub lat: f64,
    pub lon: f64,
}

/// Export pricing: equal to import under net metering, else `a · wholesale + b`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ExportTariff {
    pub net_metering: bool,
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
