//! Planning engine: one call from inputs gathered by the host (Home
//! Assistant or CLI) to a slot-by-slot plan. It fetches weather, forecasts
//! PV, load and missing prices, and runs the optimizer.
//!
//! The API is JSON in / JSON out ([`plan_json`]) so bindings stay thin and
//! requests can be saved and replayed for debugging.

mod engine;
mod types;

pub use engine::plan;
pub use types::*;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid request: {0}")]
    Request(String),
    #[error("weather forecast unavailable: {0}")]
    Weather(String),
    #[error(transparent)]
    Core(#[from] vulpes_core::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// JSON entry point: `request` is a [`PlanRequest`], the result a [`PlanResponse`].
pub fn plan_json(request: &str) -> Result<String> {
    let req: PlanRequest = serde_json::from_str(request).map_err(|e| Error::Request(e.to_string()))?;
    let resp = plan(&req)?;
    serde_json::to_string(&resp).map_err(|e| Error::Request(e.to_string()))
}
