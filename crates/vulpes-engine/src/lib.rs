//! Planning engine: one call from inputs gathered by the host (Home
//! Assistant or CLI) to a slot-by-slot plan. It fetches weather, forecasts
//! PV, load and missing prices, and runs the optimizer.
//!
//! The API is JSON in / JSON out ([`plan_json`]) so bindings stay thin and
//! requests can be saved and replayed for debugging.

mod calibrate;
mod engine;
mod types;

pub use calibrate::calibrate;
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
    /// A bug in the engine (a caught panic), not a problem with the request.
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Runs `f` and turns a panic into [`Error::Internal`]. A panic that crossed
/// the Python bindings would not be an ordinary exception there, and the
/// host's error handling would miss it.
fn guarded<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic without a message".into());
        Err(Error::Internal(message))
    })
}

/// JSON entry point: `request` is a [`PlanRequest`], the result a [`PlanResponse`].
pub fn plan_json(request: &str) -> Result<String> {
    let req: PlanRequest = serde_json::from_str(request).map_err(|e| Error::Request(e.to_string()))?;
    let resp = guarded(|| plan(&req))?;
    serde_json::to_string(&resp).map_err(|e| Error::Request(e.to_string()))
}

/// JSON entry point: `request` is a [`CalibrateRequest`], the result a [`CalibrateResponse`].
pub fn calibrate_json(request: &str) -> Result<String> {
    let req: CalibrateRequest = serde_json::from_str(request).map_err(|e| Error::Request(e.to_string()))?;
    let resp = guarded(|| calibrate(&req))?;
    serde_json::to_string(&resp).map_err(|e| Error::Request(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_becomes_an_internal_error() {
        let with_str: Result<()> = guarded(|| panic!("index out of bounds"));
        assert!(matches!(with_str, Err(Error::Internal(m)) if m == "index out of bounds"));
        let slot = 3;
        let formatted: Result<()> = guarded(|| panic!("no slot {slot}"));
        assert_eq!(formatted.unwrap_err().to_string(), "internal error: no slot 3");
    }

    #[test]
    fn results_pass_through_unchanged() {
        assert_eq!(guarded(|| Ok(7)).unwrap(), 7);
        let err: Result<()> = guarded(|| Err(Error::Request("bad".into())));
        assert!(matches!(err, Err(Error::Request(m)) if m == "bad"));
    }
}
