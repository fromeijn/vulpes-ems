//! Core domain types for vulpes-ems: the planning time grid, generic ESS mode
//! behaviours and battery parameters.

pub mod battery;
pub mod grid;
pub mod mode;
pub mod store;

pub use battery::BatteryParams;
pub use grid::TimeGrid;
pub use mode::{Behaviour, ModeDef};
pub use store::{ForecastPoint, SeriesKind, Store};

/// Errors shared across vulpes crates.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("storage error: {0}")]
    Storage(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
