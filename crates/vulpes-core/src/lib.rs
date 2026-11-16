//! Core domain types for vulpes-ems: the planning time grid, generic ESS mode
//! behaviours, battery parameters, forecast error metrics and the SQLite store.

pub mod battery;
pub mod grid;
pub mod metrics;
pub mod mode;
pub mod store;

pub use battery::{BatteryParams, GridLimits};
pub use grid::TimeGrid;
pub use metrics::ErrorStats;
pub use mode::{Behaviour, ModeDef};
pub use store::{ForecastPoint, PlanSlotRecord, SeriesKind, Store, keys};

/// Errors shared across vulpes crates.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("storage error: {0}")]
    Storage(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
