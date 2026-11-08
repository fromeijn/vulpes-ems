//! Small helpers for turning stored series into hourly maps.

use anyhow::Result;
use std::collections::HashMap;
use vulpes_core::Store;

/// Hourly means of 15-minute points, keyed by hour start. Hours without all
/// four quarters are dropped, so a partial hour never looks complete.
pub(crate) fn hourly_means(points: impl IntoIterator<Item = (i64, f64)>) -> HashMap<i64, f64> {
    let mut acc: HashMap<i64, (f64, u32)> = HashMap::new();
    for (t, v) in points {
        let e = acc.entry(t - t.rem_euclid(3600)).or_default();
        e.0 += v;
        e.1 += 1;
    }
    acc.into_iter()
        .filter(|(_, (_, n))| *n == 4)
        .map(|(h, (s, n))| (h, s / n as f64))
        .collect()
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_complete_hours_only() {
        let pts = [
            (3600, 1.0),
            (4500, 2.0),
            (5400, 3.0),
            (6300, 6.0),
            (7200, 5.0),
            (8100, 5.0),
        ];
        let m = hourly_means(pts);
        assert_eq!(m.len(), 1, "the second hour has only two quarters");
        assert_eq!(m[&3600], 3.0);
    }

    #[test]
    fn negative_timestamps_and_empty_input() {
        let pts = [(-3600, 4.0), (-2700, 4.0), (-1800, 4.0), (-900, 8.0)];
        assert_eq!(hourly_means(pts)[&-3600], 5.0);
        assert!(hourly_means(Vec::new()).is_empty());
    }
}
