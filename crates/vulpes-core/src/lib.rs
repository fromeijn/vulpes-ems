//! Core domain types for vulpes-ems: the planning time grid, generic ESS mode
//! behaviours and battery parameters.

pub mod battery;
pub mod grid;
pub mod mode;

pub use battery::BatteryParams;
pub use grid::TimeGrid;
pub use mode::{Behaviour, ModeDef};

/// Errors shared across vulpes crates.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid configuration: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, Error>;
