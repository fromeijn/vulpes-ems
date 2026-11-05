//! PyO3 bindings: the Python module `vulpes_ems._core`.

use pyo3::prelude::*;
use vulpes_core::TimeGrid;

/// Start of the 15-minute slot containing `now` (Unix seconds).
#[pyfunction]
fn quarter_hour_floor(now: i64) -> PyResult<i64> {
    TimeGrid::aligned(now, TimeGrid::QUARTER_HOUR, 1)
        .map(|g| g.start)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
}

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add_function(wrap_pyfunction!(quarter_hour_floor, m)?)?;
    Ok(())
}
