//! Home Assistant inputs: the battery SOC state, price lists in state
//! attributes, and the house load balance from energy statistics.

use crate::cfg::{Cfg, str_list};
use crate::settings::{
    CONF_BATTERY_CHARGE, CONF_BATTERY_DISCHARGE, CONF_GRID_EXPORT, CONF_GRID_IMPORT, CONF_SOLAR_ENERGY,
};
use crate::time::parse_ts;
use chrono_tz::Tz;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// Hourly recorder rows per statistic id: `(start, value)`, where `start` is
/// Unix seconds (or an ISO string) and `value` the hour's `change` or `mean`.
pub type EnergyRows = BTreeMap<String, Vec<(Value, Option<f64>)>>;

/// Some battery management systems report a little over 100 % when full (or
/// just below 0 %): such readings are clamped instead of rejected.
pub const SOC_TOLERANCE: f64 = 5.0;

/// A state of charge in % from a sensor state, or `None` if it isn't one.
pub fn soc_value(state: Option<&str>) -> Option<f64> {
    let value: f64 = state?.trim().parse().ok()?;
    if !(-SOC_TOLERANCE..=100.0 + SOC_TOLERANCE).contains(&value) {
        return None; // also NaN
    }
    Some(value.clamp(0.0, 100.0))
}

const TIME_KEYS: [&str; 6] = ["startsAt", "start", "from", "start_time", "time", "datetime"];
const VALUE_KEYS: [&str; 5] = ["total", "value", "price", "price_eur", "price_per_kwh"];

/// A published all-in import price for `[start, end)` (currency/kWh).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct KnownPrice {
    pub start: i64,
    pub end: i64,
    pub import: f64,
}

/// Find price lists (for example today/tomorrow) in a sensor's attributes,
/// given as ordered `(name, value)` pairs: where lists overlap, a later
/// list wins. Each list item needs a start time and a price; intervals end
/// at the next start, or after the typical spacing for the last item.
/// Prices in a unit with "ct" are converted to currency/kWh.
pub fn parse_price_attributes(attributes: &[(String, Value)], unit: Option<&str>, tz: Tz) -> Vec<KnownPrice> {
    let scale = if unit.is_some_and(|u| u.to_lowercase().contains("ct")) {
        0.01
    } else {
        1.0
    };
    let mut points: Vec<(f64, f64)> = Vec::new();
    for (_, value) in attributes {
        let Some(items) = value.as_array() else { continue };
        let Some(first) = items.first().and_then(Value::as_object) else {
            continue;
        };
        let tkey = TIME_KEYS.iter().find(|k| first.contains_key(**k));
        let vkey = VALUE_KEYS.iter().find(|k| first.contains_key(**k));
        let (Some(tkey), Some(vkey)) = (tkey, vkey) else {
            continue;
        };
        for item in items.iter().filter_map(Value::as_object) {
            let t = item.get(*tkey).and_then(|v| parse_ts(v, tz));
            // Numbers only: Python's bool is an int, but a price of True is no price.
            let v = item.get(*vkey).filter(|v| v.is_number()).and_then(Value::as_f64);
            if let (Some(t), Some(v)) = (t, v) {
                let price = v * scale;
                match points.iter_mut().find(|(s, _)| *s == t) {
                    Some(p) => p.1 = price,
                    None => points.push((t, price)),
                }
            }
        }
    }
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    let spacing = points
        .windows(2)
        .map(|w| w[1].0 - w[0].0)
        .filter(|g| *g > 0.0)
        .reduce(f64::min)
        .unwrap_or(3600.0);
    points
        .iter()
        .enumerate()
        .map(|(i, &(s, import))| {
            let end = match points.get(i + 1) {
                Some(&(next, _)) => next.min(s + spacing),
                None => s + spacing,
            };
            KnownPrice {
                start: s as i64,
                end: end as i64,
                import,
            }
        })
        .collect()
}

/// Statistics that add to (`plus`) or subtract from (`minus`) the house load.
pub struct LoadGroups {
    pub plus: Vec<String>,
    pub minus: Vec<String>,
}

impl LoadGroups {
    fn signed(&self) -> impl Iterator<Item = (f64, &String)> {
        self.plus
            .iter()
            .map(|s| (1.0, s))
            .chain(self.minus.iter().map(|s| (-1.0, s)))
    }
}

pub fn load_groups(cfg: &Cfg) -> LoadGroups {
    let list = |keys: &[&str]| keys.iter().flat_map(|k| str_list(cfg, k)).collect();
    LoadGroups {
        plus: list(&[CONF_GRID_IMPORT, CONF_SOLAR_ENERGY, CONF_BATTERY_DISCHARGE]),
        minus: list(&[CONF_GRID_EXPORT, CONF_BATTERY_CHARGE]),
    }
}

/// An hourly energy change below minus this (kWh) is a counter reset, not rounding jitter.
pub const COUNTER_JITTER_KWH: f64 = 0.01;

/// Hourly values per statistic, keyed by hour start.
fn per_hour(stats: &EnergyRows) -> BTreeMap<i64, BTreeMap<&str, f64>> {
    let mut out: BTreeMap<i64, BTreeMap<&str, f64>> = BTreeMap::new();
    for (id, rows) in stats {
        for (start, change) in rows {
            if let (Some(t), Some(c)) = (parse_ts(start, Tz::UTC), change) {
                out.entry(t as i64).or_default().insert(id.as_str(), *c);
            }
        }
    }
    out
}

/// Hourly house load in W from hourly energy changes (kWh) per statistic.
///
/// An hour missing any of the series is skipped: it would give a wrong
/// balance. So is an hour where a counter went down (a meter reset or a
/// corrected statistic): energy counters only go up, and the balance of such
/// an hour can be hundreds of kW, which would distort the load profile for
/// weeks.
pub fn load_series(groups: &LoadGroups, stats: &EnergyRows) -> Vec<(i64, f64)> {
    let ids: Vec<&String> = groups.signed().map(|(_, s)| s).collect();
    if ids.is_empty() {
        return Vec::new();
    }
    per_hour(stats)
        .into_iter()
        .filter(|(_, vals)| ids.iter().all(|s| vals.contains_key(s.as_str())))
        .filter(|(_, vals)| ids.iter().all(|s| vals[s.as_str()] >= -COUNTER_JITTER_KWH))
        .map(|(t, vals)| {
            let kwh: f64 = groups.signed().map(|(sign, s)| sign * vals[s.as_str()]).sum();
            (t, kwh.max(0.0) * 1000.0)
        })
        .collect()
}

/// Hourly energy (kWh) summed over the statistics `ids`, for hours every one
/// of them has. Hours where a counter went down are left out (see [`load_series`]).
pub fn hourly_sums(ids: &[String], stats: &EnergyRows) -> Vec<(i64, f64)> {
    if ids.is_empty() {
        return Vec::new();
    }
    per_hour(stats)
        .into_iter()
        .filter(|(_, vals)| {
            ids.iter()
                .all(|s| vals.get(s.as_str()).is_some_and(|v| *v >= -COUNTER_JITTER_KWH))
        })
        .map(|(t, vals)| (t, ids.iter().map(|s| vals[s.as_str()]).sum::<f64>().max(0.0)))
        .collect()
}

/// Recorder rows → `(hour_start, value × scale)`, skipping empty hours, sorted.
pub fn hourly_series(rows: &[(Value, Option<f64>)], scale: f64) -> Vec<(i64, f64)> {
    let mut out: Vec<(i64, f64)> = rows
        .iter()
        .filter_map(|(start, v)| Some((parse_ts(start, Tz::UTC)? as i64, (*v)? * scale)))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    out
}

/// Hourly energy per statistic (kWh) → total mean power (W), for the hours
/// every statistic has. Never negative.
pub fn total_power(energy: &BTreeMap<String, Vec<(i64, f64)>>) -> Vec<(i64, f64)> {
    if energy.is_empty() {
        return Vec::new();
    }
    let mut hours: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
    for series in energy.values() {
        for &(t, kwh) in series {
            hours.entry(t).or_default().push(kwh);
        }
    }
    hours
        .into_iter()
        .filter(|(_, v)| v.len() == energy.len())
        .map(|(t, v)| (t, v.iter().sum::<f64>().max(0.0) * 1000.0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const T0: f64 = 1_794_096_000.0; // 2026-11-08 00:00 UTC
    const T: i64 = T0 as i64;

    fn attrs(v: Value) -> Vec<(String, Value)> {
        v.as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    // --- SOC ------------------------------------------------------------------

    #[test]
    fn soc_values() {
        let cases = [
            (Some("55.5"), Some(55.5)),
            (Some("0"), Some(0.0)),
            (Some("100"), Some(100.0)),
            (Some("100.4"), Some(100.0)), // a full battery that reports slightly over 100 %
            (Some("105"), Some(100.0)),
            (Some("-1"), Some(0.0)),
            (Some("106"), None),
            (Some("-6"), None),
            (Some("nan"), None),
            (Some("unavailable"), None),
            (None, None),
        ];
        for (state, want) in cases {
            assert_eq!(soc_value(state), want, "{state:?}");
        }
    }

    // --- prices ---------------------------------------------------------------

    #[test]
    fn price_lists_from_attributes() {
        let a = vec![
            (
                "today".to_string(),
                json!([
                    {"start": "2026-11-08T00:00:00+00:00", "price": 0.20},
                    {"start": "2026-11-08T01:00:00+00:00", "price": 0.25}
                ]),
            ),
            (
                "tomorrow".into(),
                json!([{"start": "2026-11-08T02:00:00+00:00", "price": 0.30}]),
            ),
            ("friendly_name".into(), json!("Price")),
        ];
        assert_eq!(
            parse_price_attributes(&a, Some("EUR/kWh"), Tz::UTC),
            [
                KnownPrice {
                    start: T,
                    end: T + 3600,
                    import: 0.20
                },
                KnownPrice {
                    start: T + 3600,
                    end: T + 7200,
                    import: 0.25
                },
                // The last one: typical spacing.
                KnownPrice {
                    start: T + 7200,
                    end: T + 10_800,
                    import: 0.30
                },
            ]
        );
    }

    #[test]
    fn price_lists_in_cents_and_quarter_hours() {
        let items: Vec<Value> = (0..3)
            .map(|i| json!({"startsAt": T0 + 900.0 * f64::from(i), "total": 20.0 + f64::from(i)}))
            .collect();
        let p = parse_price_attributes(&[("prices".into(), Value::Array(items))], Some("ct/kWh"), Tz::UTC);
        assert_eq!(
            p.iter().map(|x| x.end - x.start).collect::<Vec<_>>(),
            [900, 900, 900]
        );
        assert!((p[0].import - 0.20).abs() < 1e-12);
    }

    #[test]
    fn epex_spot_hourly_price_list() {
        // EPEX Spot: `data` with start_time/end_time/price_per_kwh, plus a scalar price next to it.
        let hour = |h: i32| format!("2026-11-08T{h:02}:00:00+00:00");
        let data: Vec<Value> = (0..3)
            .map(|h| json!({"start_time": hour(h), "end_time": hour(h + 1), "price_per_kwh": 0.20 + f64::from(h) / 100.0}))
            .collect();
        let a = attrs(json!({"data": data, "price_per_kwh": 0.20, "unit_of_measurement": "€/kWh"}));
        let p = parse_price_attributes(&a, Some("€/kWh"), Tz::UTC);
        assert_eq!(
            p.iter()
                .map(|x| (x.start - T, x.end - x.start))
                .collect::<Vec<_>>(),
            [(0, 3600), (3600, 3600), (7200, 3600)]
        );
        for (x, want) in p.iter().zip([0.20, 0.21, 0.22]) {
            assert!((x.import - want).abs() < 1e-12);
        }
    }

    #[test]
    fn attributes_without_prices() {
        assert!(parse_price_attributes(&[], None, Tz::UTC).is_empty());
        let a = attrs(json!({"options": ["a", "b"], "list": [], "x": [{"foo": 1}]}));
        assert!(parse_price_attributes(&a, None, Tz::UTC).is_empty());
        // Unparsable times and non-numeric prices are skipped.
        let a = attrs(
            json!({"today": [{"start": "soon", "price": 0.2}, {"start": T0, "price": "n/a"}, {"start": T0, "price": true}]}),
        );
        assert!(parse_price_attributes(&a, None, Tz::UTC).is_empty());
    }

    #[test]
    fn a_later_list_wins_where_lists_overlap() {
        let a = vec![
            ("raw".to_string(), json!([{"start": T0, "price": 0.1}])),
            (
                "today".to_string(),
                json!([{"start": T0, "price": 0.3}, "not an item"]),
            ),
        ];
        let p = parse_price_attributes(&a, None, Tz::UTC);
        assert_eq!(
            p,
            [KnownPrice {
                start: T,
                end: T + 3600,
                import: 0.3
            }]
        );
    }

    // --- load balance -----------------------------------------------------------

    fn rows(changes: &[Option<f64>]) -> Vec<(Value, Option<f64>)> {
        changes
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_some())
            .map(|(i, c)| (json!(T0 + 3600.0 * i as f64), *c))
            .collect()
    }

    fn groups(plus: &[&str], minus: &[&str]) -> LoadGroups {
        LoadGroups {
            plus: plus.iter().map(|s| s.to_string()).collect(),
            minus: minus.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn balances_and_skips_incomplete_hours() {
        let g = groups(&["sensor.import", "sensor.solar"], &["sensor.export"]);
        let mut stats = EnergyRows::new();
        stats.insert("sensor.import".into(), rows(&[Some(0.5), Some(0.2), Some(0.1)]));
        stats.insert("sensor.solar".into(), rows(&[Some(0.0), None, Some(2.0)]));
        stats.insert("sensor.export".into(), rows(&[Some(0.0), Some(0.0), Some(2.5)]));
        // Hour 1 lacks solar; hour 2 is negative (export > import + solar) and clamps to 0.
        assert_eq!(load_series(&g, &stats), [(T, 500.0), (T + 7200, 0.0)]);
    }

    #[test]
    fn skips_hours_where_a_counter_went_down() {
        // A counter reset gives a huge negative change; as a balance it would be hundreds of kW.
        let g = groups(&["sensor.import", "sensor.discharge"], &["sensor.charge"]);
        let mut stats = EnergyRows::new();
        stats.insert("sensor.import".into(), rows(&[Some(0.5); 4]));
        stats.insert(
            "sensor.discharge".into(),
            rows(&[Some(0.0), Some(-2000.0), Some(0.0), Some(-0.001)]),
        );
        stats.insert(
            "sensor.charge".into(),
            rows(&[Some(0.1), Some(-2500.0), Some(0.1), Some(0.0)]),
        );
        // Hour 1 is the reset (it would be 500 kW); hour 3 is rounding jitter and stays.
        let h = load_series(&g, &stats);
        assert_eq!(
            h.iter().map(|x| x.0).collect::<Vec<_>>(),
            [T, T + 7200, T + 10_800]
        );
        for ((_, w), want) in h.iter().zip([400.0, 400.0, 499.0]) {
            assert!((w - want).abs() < 1e-9, "{w}");
        }
        // A reset on a subtracted series alone is skipped too.
        stats.insert("sensor.discharge".into(), rows(&[Some(0.0); 4]));
        let h = load_series(&g, &stats);
        assert_eq!(
            h.iter().map(|x| x.0).collect::<Vec<_>>(),
            [T, T + 7200, T + 10_800]
        );
    }

    #[test]
    fn load_series_empty() {
        assert!(load_series(&groups(&[], &[]), &EnergyRows::new()).is_empty());
        assert!(load_series(&groups(&["sensor.a"], &[]), &EnergyRows::new()).is_empty());
        let mut stats = EnergyRows::new();
        stats.insert("sensor.a".into(), vec![]);
        assert!(load_series(&groups(&["sensor.a"], &[]), &stats).is_empty());
    }

    #[test]
    fn load_groups_from_settings() {
        let cfg = json!({
            "grid_import": ["sensor.i"], "grid_export": ["sensor.e"], "solar_energy": ["sensor.s"],
            "battery_charge": ["sensor.bc"], "battery_discharge": ["sensor.bd"]
        });
        let g = load_groups(cfg.as_object().unwrap());
        assert_eq!(g.plus, ["sensor.i", "sensor.s", "sensor.bd"]);
        assert_eq!(g.minus, ["sensor.e", "sensor.bc"]);
        let empty = load_groups(&Cfg::new());
        assert!(empty.plus.is_empty() && empty.minus.is_empty());
    }

    #[test]
    fn hourly_sums_over_a_group() {
        let mut stats = EnergyRows::new();
        stats.insert("a".into(), rows(&[Some(0.5), Some(0.2), Some(-3.0), Some(0.1)]));
        stats.insert("b".into(), rows(&[Some(0.25), None, Some(0.1), Some(-0.001)]));
        let ids = ["a".to_string(), "b".to_string()];
        // Hour 1 lacks b, hour 2 is a counter reset; jitter in hour 3 is kept (and clamped at 0 in sum).
        let sums = hourly_sums(&ids, &stats);
        assert_eq!(sums.iter().map(|x| x.0).collect::<Vec<_>>(), [T, T + 10_800]);
        assert!((sums[0].1 - 0.75).abs() < 1e-12 && (sums[1].1 - 0.099).abs() < 1e-12);
        assert!(hourly_sums(&[], &stats).is_empty());
        assert!(hourly_sums(&["c".to_string()], &stats).is_empty());
    }

    // --- solar statistics ---------------------------------------------------------

    #[test]
    fn hourly_series_from_rows() {
        let r = vec![
            (json!(T0 + 3600.0), Some(1.5)),
            (json!("2026-11-08T00:00:00+00:00"), Some(2.0)),
            (json!(T0 + 7200.0), None),
            (json!(null), Some(3.0)),
        ];
        assert_eq!(hourly_series(&r, 1.0), [(T, 2.0), (T + 3600, 1.5)]);
        assert_eq!(hourly_series(&r, 1000.0)[0].1, 2000.0);
        assert!(hourly_series(&[], 1.0).is_empty());
    }

    #[test]
    fn total_power_sums_complete_hours() {
        let mut e = BTreeMap::new();
        e.insert("sensor.a".to_string(), vec![(0, 1.0), (3600, 0.5)]);
        e.insert("sensor.b".to_string(), vec![(0, 0.25), (7200, 1.0)]);
        assert_eq!(total_power(&e), [(0, 1250.0)]);
        let mut neg = BTreeMap::new();
        neg.insert("sensor.a".to_string(), vec![(0, -0.01)]);
        assert_eq!(total_power(&neg), [(0, 0.0)], "never negative");
        assert!(total_power(&BTreeMap::new()).is_empty());
    }
}
