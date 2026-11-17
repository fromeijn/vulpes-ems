//! Weather at the price reference points, for the wholesale price regression
//! (`vulpes_price::wholesale`): the day-ahead forecasts of past days, which
//! the regression trains on, and the latest forecast of the days to come.
//!
//! One weather model for both, so training and forecast see the same model.
//! The points are public and fixed (`WIND_POINTS`, `SOLAR_POINTS`): nothing
//! about the house goes into these requests.

use crate::{Error, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use vulpes_price::wholesale::{DriverRow, Drivers, RefPoint, SOLAR_POINTS, WIND_POINTS, hourly_means};
use vulpes_sources::{Location, open_meteo};

/// The weather model at the reference points. ECMWF IFS reaches past the
/// planning horizon and has day-ahead archives (previous runs).
pub const MODEL: &str = "ecmwf_ifs025";
const WIND_VAR: &str = "wind_speed_100m";
const SOLAR_VAR: &str = "shortwave_radiation";

/// What to fetch: the day-ahead history for an inclusive date range, the
/// forecast for the coming days, or both.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriversRequest {
    #[serde(default)]
    pub past: Option<DateRange>,
    /// Forecast days from today (UTC), 1 to 16.
    #[serde(default)]
    pub ahead_days: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DateRange {
    pub start: NaiveDate,
    pub end: NaiveDate,
}

/// Hourly `(hour start, wind index, solar index)` rows, `null` where not
/// requested or where the fetch failed; each failed part is in `errors`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DriversResponse {
    pub past: Option<Vec<DriverRow>>,
    pub ahead: Option<Vec<DriverRow>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// The Open-Meteo ranges for `req`: the previous-runs archive a day ahead,
/// and the live forecast from the start of yesterday. The overlap fills the
/// last hour of the history, where Open-Meteo has no solar value: in Europe
/// that hour is local midnight of today, a reference day of every forecast.
fn ranges(req: &DriversRequest) -> Result<(Option<open_meteo::Range>, Option<open_meteo::Range>)> {
    let past = match req.past {
        Some(r) if r.start > r.end => {
            return Err(Error::Request(format!(
                "past: start {} is after end {}",
                r.start, r.end
            )));
        }
        Some(r) => Some(open_meteo::Range::PreviousRuns {
            start: r.start,
            end: r.end,
            lead_days: 1,
        }),
        None => None,
    };
    let ahead = match req.ahead_days {
        Some(d) if !(1..=16).contains(&d) => {
            return Err(Error::Request(format!("ahead_days must be 1 to 16, got {d}")));
        }
        Some(days) => Some(open_meteo::Range::Forecast { days, past_days: 1 }),
        None => None,
    };
    Ok((past, ahead))
}

/// Hourly values of `var` at `point` from the fetched series.
fn point_hours(
    series: Vec<open_meteo::WeatherSeries>,
    var: &str,
    point: &RefPoint,
) -> Result<HashMap<i64, f64>> {
    let hours = series
        .into_iter()
        .find(|s| s.var == var)
        .map(|s| hourly_means(s.points))
        .unwrap_or_default();
    if hours.is_empty() {
        return Err(Error::Weather(format!(
            "no {var} from {MODEL} at {}",
            point.label
        )));
    }
    Ok(hours)
}

/// The driver rows from hourly weather per reference point (same order as
/// `WIND_POINTS` and `SOLAR_POINTS`).
pub fn rows(wind: &[HashMap<i64, f64>], solar: &[HashMap<i64, f64>]) -> Vec<DriverRow> {
    Drivers::from_points(wind, solar).to_rows()
}

fn fetch_range(range: open_meteo::Range) -> Result<Vec<DriverRow>> {
    let fetch = |p: &RefPoint, var: &str| -> Result<HashMap<i64, f64>> {
        let series = open_meteo::fetch(&open_meteo::Request {
            location: Location {
                lat: p.lat,
                lon: p.lon,
            },
            models: &[MODEL],
            vars: &[var],
            range,
        })
        .map_err(|e| Error::Weather(e.to_string()))?;
        point_hours(series, var, p)
    };
    let wind: Vec<_> = WIND_POINTS
        .iter()
        .map(|p| fetch(p, WIND_VAR))
        .collect::<Result<_>>()?;
    let solar: Vec<_> = SOLAR_POINTS
        .iter()
        .map(|p| fetch(p, SOLAR_VAR))
        .collect::<Result<_>>()?;
    Ok(rows(&wind, &solar))
}

/// Fetch the requested drivers: one request per reference point and range.
/// The parts fail on their own, so a failing history doesn't hold back the
/// forecast; only an invalid request is an error.
pub fn fetch(req: &DriversRequest) -> Result<DriversResponse> {
    let (past, ahead) = ranges(req)?;
    Ok(combine(past.map(fetch_range), ahead.map(fetch_range)))
}

/// The response from the fetched parts, with the failed ones as errors.
fn combine(past: Option<Result<Vec<DriverRow>>>, ahead: Option<Result<Vec<DriverRow>>>) -> DriversResponse {
    let mut errors = Vec::new();
    let mut part = |name: &str, r: Option<Result<Vec<DriverRow>>>| match r? {
        Ok(rows) => Some(rows),
        Err(e) => {
            errors.push(format!("{name}: {e}"));
            None
        }
    };
    let (past, ahead) = (part("history", past), part("forecast", ahead));
    DriversResponse { past, ahead, errors }
}

/// JSON entry point: `request` is a [`DriversRequest`], the result a [`DriversResponse`].
pub fn fetch_json(request: &str) -> Result<String> {
    let req: DriversRequest = serde_json::from_str(request).map_err(|e| Error::Request(e.to_string()))?;
    let resp = crate::guarded(|| fetch(&req))?;
    serde_json::to_string(&resp).map_err(|e| Error::Request(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, d).unwrap()
    }

    #[test]
    fn ranges_for_history_and_forecast() {
        let req = DriversRequest {
            past: Some(DateRange {
                start: day(1),
                end: day(3),
            }),
            ahead_days: Some(5),
        };
        let (past, ahead) = ranges(&req).unwrap();
        assert!(matches!(
            past,
            Some(open_meteo::Range::PreviousRuns { lead_days: 1, start, end }) if start == day(1) && end == day(3)
        ));
        assert!(matches!(
            ahead,
            Some(open_meteo::Range::Forecast {
                days: 5,
                past_days: 1
            })
        ));
        assert!(matches!(
            ranges(&DriversRequest::default()).unwrap(),
            (None, None)
        ));
    }

    #[test]
    fn invalid_ranges_are_rejected() {
        let backwards = DriversRequest {
            past: Some(DateRange {
                start: day(3),
                end: day(1),
            }),
            ahead_days: None,
        };
        assert!(ranges(&backwards).unwrap_err().to_string().contains("after end"));
        for days in [0, 17] {
            let req = DriversRequest {
                past: None,
                ahead_days: Some(days),
            };
            assert!(ranges(&req).is_err(), "{days}");
        }
        // Nothing requested: nothing fetched, no request made.
        assert_eq!(
            fetch(&DriversRequest::default()).unwrap(),
            DriversResponse::default()
        );
    }

    #[test]
    fn point_hours_averages_quarters_and_needs_data() {
        let p = &WIND_POINTS[0];
        let s = |var: &str, points: Vec<(i64, f64)>| open_meteo::WeatherSeries {
            model: MODEL.into(),
            var: var.into(),
            points,
        };
        let quarters = vec![(3600, 4.0), (4500, 6.0), (5400, 8.0), (6300, 10.0)];
        let h = point_hours(vec![s(SOLAR_VAR, vec![]), s(WIND_VAR, quarters)], WIND_VAR, p).unwrap();
        assert_eq!(h, [(3600, 7.0)].into());
        let err = point_hours(vec![s(SOLAR_VAR, vec![(0, 1.0)])], WIND_VAR, p).unwrap_err();
        assert!(err.to_string().contains("no wind_speed_100m"), "{err}");
        assert!(err.to_string().contains(p.label));
    }

    #[test]
    fn rows_combine_the_points() {
        let w1: HashMap<i64, f64> = [(0, 12.0), (3600, 12.0)].into();
        let w2: HashMap<i64, f64> = [(0, 3.0), (3600, 12.0)].into();
        let s1: HashMap<i64, f64> = [(0, 200.0)].into();
        let s2: HashMap<i64, f64> = [(0, 400.0)].into();
        let r = rows(&[w1, w2], &[s1, s2]);
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].0, r[0].1), (0, Some(0.5)));
        assert!((r[0].2.unwrap() - 0.3).abs() < 1e-12, "{r:?}");
        assert_eq!(r[1], (3600, Some(1.0), None));
        assert!(rows(&[], &[]).is_empty());
    }

    #[test]
    fn a_failing_part_keeps_the_other() {
        let rows = vec![(0, Some(0.5), Some(0.1))];
        let failed = || Err(Error::Weather("timeout".into()));
        let r = combine(Some(failed()), Some(Ok(rows.clone())));
        assert_eq!((r.past, r.ahead.as_ref()), (None, Some(&rows)));
        assert_eq!(r.errors.len(), 1);
        assert!(r.errors[0].starts_with("history:") && r.errors[0].contains("timeout"));
        let r = combine(Some(Ok(rows.clone())), Some(failed()));
        assert_eq!((r.past.as_ref(), r.ahead), (Some(&rows), None));
        assert!(r.errors[0].starts_with("forecast:"));
        assert_eq!(combine(None, None), DriversResponse::default());
        // Errors are in the JSON only when there are any.
        let json = serde_json::to_value(combine(Some(failed()), None)).unwrap();
        assert_eq!(json["past"], serde_json::Value::Null);
        assert_eq!(json["errors"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn json_round_trip() {
        let req: DriversRequest = serde_json::from_str(
            r#"{"past": {"start": "2026-11-01", "end": "2026-11-03"}, "ahead_days": 5}"#,
        )
        .unwrap();
        assert_eq!(req.ahead_days, Some(5));
        assert_eq!(
            serde_json::from_str::<DriversRequest>("{}").unwrap(),
            DriversRequest::default()
        );
        assert!(serde_json::from_str::<DriversRequest>(r#"{"pastt": null}"#).is_err());
        let resp = DriversResponse {
            past: Some(vec![(0, Some(0.5), None)]),
            ahead: None,
            errors: vec![],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"past":[[0,0.5,null]],"ahead":null}"#);
        assert_eq!(serde_json::from_str::<DriversResponse>(&json).unwrap(), resp);
        assert_eq!(fetch_json("{}").unwrap(), r#"{"past":null,"ahead":null}"#);
        assert!(fetch_json("not json").is_err());
    }
}
