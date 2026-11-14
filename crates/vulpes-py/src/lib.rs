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

/// Run a Home Assistant adapter op (`vulpes_ha::OPS`) on a JSON request;
/// returns its JSON result. Pure and fast (microseconds), so it keeps the GIL.
#[pyfunction]
fn ha(op: &str, request_json: &str) -> PyResult<String> {
    vulpes_ha::call(op, request_json).map_err(|e| PyValueError::new_err(e.to_string()))
}

/// Plan from Home Assistant inputs (settings, SOC state, price attributes,
/// energy statistics); returns the plan with Vulpes modes. Runs without the
/// GIL (it may fetch weather).
#[pyfunction]
fn plan_ha(py: Python<'_>, request_json: String) -> PyResult<String> {
    py.allow_threads(|| vulpes_ha::plan_ha_json(&request_json))
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

/// Calibrate the solar planes from Home Assistant statistics; returns the
/// result and the planes it is based on. Runs without the GIL (it fetches
/// archived weather).
#[pyfunction]
fn calibrate_ha(py: Python<'_>, request_json: String) -> PyResult<String> {
    py.allow_threads(|| vulpes_ha::calibrate_ha_json(&request_json))
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

/// Record a 15-minute tick in the history file at `db_path` (created with its
/// folder); returns agreement and data freshness. Runs without the GIL (disk I/O).
#[pyfunction]
fn record_ha(py: Python<'_>, db_path: String, request_json: String) -> PyResult<String> {
    py.allow_threads(|| vulpes_ha::record_ha_json(&db_path, &request_json))
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

/// Review the history file at `db_path` with the current settings: savings,
/// forecast quality and control. Runs without the GIL (disk I/O).
#[pyfunction]
fn review_ha(py: Python<'_>, db_path: String, request_json: String) -> PyResult<String> {
    py.allow_threads(|| vulpes_ha::review_ha_json(&db_path, &request_json))
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

/// The recent history from the file at `db_path`, for the diagnostics download.
/// Runs without the GIL (disk I/O).
#[pyfunction]
fn export_history(py: Python<'_>, db_path: String, request_json: String) -> PyResult<String> {
    py.allow_threads(|| vulpes_ha::export_history_json(&db_path, &request_json))
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
    m.add_function(wrap_pyfunction!(ha, m)?)?;
    m.add_function(wrap_pyfunction!(plan_ha, m)?)?;
    m.add_function(wrap_pyfunction!(calibrate_ha, m)?)?;
    m.add_function(wrap_pyfunction!(record_ha, m)?)?;
    m.add_function(wrap_pyfunction!(review_ha, m)?)?;
    m.add_function(wrap_pyfunction!(export_history, m)?)?;
    Ok(())
}
