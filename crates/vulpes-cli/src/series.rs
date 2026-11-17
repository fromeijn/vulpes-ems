//! Small helpers for turning stored series into hourly maps.

use anyhow::Result;
use std::collections::HashMap;
use vulpes_core::Store;

pub(crate) use vulpes_price::wholesale::hourly_means;

/// Hourly means of a stored 15-minute forecast variable (latest issue).
pub(crate) fn hourly_forecast(store: &Store, source: &str, var: &str) -> Result<HashMap<i64, f64>> {
    Ok(hourly_means(store.forecasts_asof(
        source,
        var,
        0,
        i64::MAX / 2,
        i64::MAX,
    )?))
}
