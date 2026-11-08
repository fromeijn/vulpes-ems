//! PyO3 bindings: the Python module `vulpes_ems._core`.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use vulpes_core::TimeGrid;

/// Start of the 15-minute slot containing `now` (Unix seconds).
#[pyfunction]
fn quarter_hour_floor(now: i64) -> PyResult<i64> {
    TimeGrid::aligned(now, TimeGrid::QUARTER_HOUR, 1)
        .map(|g| g.start)
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

/// Plan from a JSON `PlanRequest`; returns a JSON `PlanResponse`.
/// Runs without the GIL (it may fetch weather and takes ~0.1 s of CPU).
#[pyfunction]
fn plan(py: Python<'_>, request_json: String) -> PyResult<String> {
    py.allow_threads(|| vulpes_engine::plan_json(&request_json))
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

/// Calibrate solar planes from a JSON `CalibrateRequest`; returns a JSON
/// `CalibrateResponse`. Runs without the GIL (it fetches archived weather and
/// takes a few seconds of CPU).
#[pyfunction]
fn calibrate_solar(py: Python<'_>, request_json: String) -> PyResult<String> {
    py.allow_threads(|| vulpes_engine::calibrate_json(&request_json))
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

/// Hourly NL day-ahead wholesale prices `(hour_start, EUR/kWh excl. VAT)` from
/// EnergyZero for an inclusive date range `YYYY-MM-DD`.
#[pyfunction]
fn energyzero_hourly(py: Python<'_>, start: String, end: String) -> PyResult<Vec<(i64, f64)>> {
    let parse = |s: &str| {
        s.parse::<chrono::NaiveDate>()
            .map_err(|e| PyValueError::new_err(format!("bad date '{s}': {e}")))
    };
    let (start, end) = (parse(&start)?, parse(&end)?);
    py.allow_threads(|| vulpes_sources::energyzero::fetch_hourly(start, end))
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add_function(wrap_pyfunction!(quarter_hour_floor, m)?)?;
    m.add_function(wrap_pyfunction!(plan, m)?)?;
    m.add_function(wrap_pyfunction!(calibrate_solar, m)?)?;
    m.add_function(wrap_pyfunction!(energyzero_hourly, m)?)?;
    Ok(())
}
