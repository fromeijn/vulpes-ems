//! Open-Meteo client for 15-minute weather forecasts.
//!
//! Radiation variables are averages over the *preceding* step, so their
//! timestamps are shifted back one step to match the vulpes convention that a
//! timestamp marks the start of the interval it describes. Instantaneous
//! variables (temperature, wind, cloud cover) are kept as-is.

use crate::{Error, Location, Result};
use serde_json::Value;

const FORECAST_URL: &str = "https://api.open-meteo.com/v1/forecast";
const HISTORICAL_FORECAST_URL: &str = "https://historical-forecast-api.open-meteo.com/v1/forecast";
const PREVIOUS_RUNS_URL: &str = "https://previous-runs-api.open-meteo.com/v1/forecast";
const STEP_S: i64 = 900;

/// Variables needed for the PV physics chain and ML features.
pub const DEFAULT_VARS: &[&str] = &[
    "shortwave_radiation",
    "direct_normal_irradiance",
    "diffuse_radiation",
    "temperature_2m",
    "wind_speed_10m",
    "cloud_cover",
];

/// One variable from one model, as `(slot_start_ts, value)` pairs.
#[derive(Debug, Clone, PartialEq)]
pub struct WeatherSeries {
    pub model: String,
    pub var: String,
    pub points: Vec<(i64, f64)>,
}

/// What to request.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub location: Location,
    pub models: &'a [&'a str],
    pub vars: &'a [&'a str],
    pub range: Range,
}

#[derive(Debug, Clone, Copy)]
pub enum Range {
    /// Live forecast for the coming days (max 16), from UTC midnight of today, or of
    /// `past_days` earlier: a local day east of UTC starts before UTC midnight.
    Forecast { days: u8, past_days: u8 },
    /// Archived forecasts for an inclusive date range, `YYYY-MM-DD`, stitched
    /// from the freshest run for each hour (short lead time).
    Historical {
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
    },
    /// Archived forecasts as they stood `lead_days` (1..=7) before the valid time.
    PreviousRuns {
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
        lead_days: u8,
    },
}

/// The endpoint, its range parameters and the suffix of the variable names for `range`.
fn range_query(range: Range) -> (&'static str, Vec<(&'static str, String)>, String) {
    let dates = |start: chrono::NaiveDate, end: chrono::NaiveDate| {
        vec![("start_date", start.to_string()), ("end_date", end.to_string())]
    };
    match range {
        Range::Forecast { days, past_days } => {
            let mut params = vec![("forecast_days", days.to_string())];
            if past_days > 0 {
                params.push(("past_days", past_days.to_string()));
            }
            (FORECAST_URL, params, String::new())
        }
        Range::Historical { start, end } => (HISTORICAL_FORECAST_URL, dates(start, end), String::new()),
        Range::PreviousRuns {
            start,
            end,
            lead_days,
        } => (
            PREVIOUS_RUNS_URL,
            dates(start, end),
            format!("_previous_day{lead_days}"),
        ),
    }
}

pub fn fetch(req: &Request<'_>) -> Result<Vec<WeatherSeries>> {
    let mut all = fetch_many(&[req.location], req.models, req.vars, req.range)?;
    Ok(all.pop().unwrap_or_default())
}

/// The same variables, models and range at several locations, in one request.
/// The result has one entry per location, in the order of `locations`.
pub fn fetch_many(
    locations: &[Location],
    models: &[&str],
    vars: &[&str],
    range: Range,
) -> Result<Vec<Vec<WeatherSeries>>> {
    let (lats, lons) = coords(locations);
    let (url, range_params, suffix) = range_query(range);
    let requested: Vec<String> = vars.iter().map(|v| format!("{v}{suffix}")).collect();
    let mut r = crate::agent()
        .get(url)
        .query("latitude", lats)
        .query("longitude", lons)
        .query("minutely_15", requested.join(","))
        .query("models", models.join(","))
        .query("timeformat", "unixtime")
        .query("timezone", "UTC")
        .query("wind_speed_unit", "ms");
    for (k, v) in range_params {
        r = r.query(k, v);
    }
    let body: Value = r.call()?.body_mut().read_json()?;
    parse_many(&body, locations.len(), models, vars, &suffix)
}

/// The `latitude` and `longitude` query values: the rounded coordinates,
/// comma-separated.
fn coords(locations: &[Location]) -> (String, String) {
    let join = |f: fn(&Location) -> f64| {
        locations
            .iter()
            .map(|l| f(&l.rounded()).to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    (join(|l| l.lat), join(|l| l.lon))
}

/// A response for `n` locations: an array with one block per location (the
/// first without `location_id`), or a single block for one location.
pub(crate) fn parse_many(
    body: &Value,
    n: usize,
    models: &[&str],
    vars: &[&str],
    suffix: &str,
) -> Result<Vec<Vec<WeatherSeries>>> {
    let Value::Array(blocks) = body else {
        let series = parse(body, models, vars, suffix)?;
        return match n {
            1 => Ok(vec![series]),
            _ => Err(Error::Parse(format!("expected {n} locations, got 1"))),
        };
    };
    if blocks.len() != n {
        return Err(Error::Parse(format!(
            "expected {n} locations, got {}",
            blocks.len()
        )));
    }
    let mut out: Vec<Option<Vec<WeatherSeries>>> = vec![None; n];
    for (i, block) in blocks.iter().enumerate() {
        let id = block
            .get("location_id")
            .and_then(Value::as_u64)
            .map_or(i, |id| id as usize);
        match out.get_mut(id) {
            Some(slot @ None) => *slot = Some(parse(block, models, vars, suffix)?),
            _ => return Err(Error::Parse(format!("unexpected location_id {id}"))),
        }
    }
    Ok(out.into_iter().flatten().collect())
}

/// `suffix` is appended to variable names in the response (e.g. `_previous_day1`);
/// returned series use the plain variable name.
pub(crate) fn parse(
    body: &Value,
    models: &[&str],
    vars: &[&str],
    suffix: &str,
) -> Result<Vec<WeatherSeries>> {
    if body.get("error").and_then(Value::as_bool) == Some(true) {
        return Err(Error::Parse(format!("open-meteo error: {}", body["reason"])));
    }
    let block = body
        .get("minutely_15")
        .ok_or_else(|| Error::Parse("missing minutely_15 block".into()))?;
    let times: Vec<i64> = block["time"]
        .as_array()
        .ok_or_else(|| Error::Parse("missing time array".into()))?
        .iter()
        .map(|t| t.as_i64().ok_or_else(|| Error::Parse("non-integer time".into())))
        .collect::<Result<_>>()?;

    let mut out = Vec::new();
    for &model in models {
        for &var in vars {
            // With a single model Open-Meteo omits the `_<model>` suffix.
            let key = if models.len() == 1 {
                format!("{var}{suffix}")
            } else {
                format!("{var}{suffix}_{model}")
            };
            let Some(values) = block.get(&key).and_then(Value::as_array) else {
                continue;
            };
            let shift = if is_preceding_average(var) { STEP_S } else { 0 };
            let points = times
                .iter()
                .zip(values)
                .filter_map(|(&t, v)| v.as_f64().map(|v| (t - shift, v)))
                .collect::<Vec<_>>();
            if !points.is_empty() {
                out.push(WeatherSeries {
                    model: model.to_string(),
                    var: var.to_string(),
                    points,
                });
            }
        }
    }
    Ok(out)
}

fn is_preceding_average(var: &str) -> bool {
    var.contains("radiation") || var.contains("irradiance")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_multi_model_and_shifts_radiation() {
        let body = json!({
            "minutely_15": {
                "time": [1000, 1900, 2800],
                "shortwave_radiation_m1": [10.0, null, 30.0],
                "temperature_2m_m1": [5.0, 6.0, 7.0],
                "shortwave_radiation_m2": [11.0, 21.0, 31.0]
            }
        });
        let s = parse(
            &body,
            &["m1", "m2"],
            &["shortwave_radiation", "temperature_2m"],
            "",
        )
        .unwrap();
        assert_eq!(s.len(), 3); // m2 has no temperature
        assert_eq!(s[0].points, vec![(100, 10.0), (1900, 30.0)]); // shifted, null skipped
        assert_eq!(s[1].points, vec![(1000, 5.0), (1900, 6.0), (2800, 7.0)]); // not shifted
        assert_eq!(s[2].model, "m2");
    }

    #[test]
    fn range_queries() {
        let (url, params, suffix) = range_query(Range::Forecast {
            days: 4,
            past_days: 1,
        });
        assert_eq!(url, FORECAST_URL);
        assert_eq!(
            params,
            vec![("forecast_days", "4".to_string()), ("past_days", "1".to_string())]
        );
        assert_eq!(suffix, "");
        let (_, params, _) = range_query(Range::Forecast {
            days: 4,
            past_days: 0,
        });
        assert_eq!(params, vec![("forecast_days", "4".to_string())]);
        let day = |d| chrono::NaiveDate::from_ymd_opt(2026, 6, d).unwrap();
        let (url, params, suffix) = range_query(Range::PreviousRuns {
            start: day(1),
            end: day(3),
            lead_days: 1,
        });
        assert_eq!(url, PREVIOUS_RUNS_URL);
        assert_eq!(
            params,
            vec![
                ("start_date", "2026-06-01".to_string()),
                ("end_date", "2026-06-03".to_string())
            ]
        );
        assert_eq!(suffix, "_previous_day1");
        let (url, _, suffix) = range_query(Range::Historical {
            start: day(1),
            end: day(3),
        });
        assert_eq!((url, suffix.as_str()), (HISTORICAL_FORECAST_URL, ""));
    }

    #[test]
    fn single_model_has_no_suffix() {
        let body = json!({"minutely_15": {"time": [900], "temperature_2m": [1.5]}});
        let s = parse(&body, &["m1"], &["temperature_2m"], "").unwrap();
        assert_eq!(s[0].points, vec![(900, 1.5)]);
    }

    #[test]
    fn strips_previous_run_suffix() {
        let body = json!({"minutely_15": {"time": [900], "temperature_2m_previous_day1_m1": [3.0], "temperature_2m_previous_day1_m2": [4.0]}});
        let s = parse(&body, &["m1", "m2"], &["temperature_2m"], "_previous_day1").unwrap();
        assert_eq!((s[0].var.as_str(), s[1].points[0].1), ("temperature_2m", 4.0));
    }

    #[test]
    fn parses_several_locations_by_location_id() {
        let block = |id: Option<u64>, v: f64| {
            let mut b = json!({"minutely_15": {"time": [900], "wind_speed_100m": [v]}});
            if let Some(id) = id {
                b["location_id"] = json!(id);
            }
            b
        };
        // Out of order on purpose: the first block has no location_id.
        let body = json!([block(None, 1.0), block(Some(2), 3.0), block(Some(1), 2.0)]);
        let s = parse_many(&body, 3, &["m"], &["wind_speed_100m"], "").unwrap();
        let firsts: Vec<f64> = s.iter().map(|l| l[0].points[0].1).collect();
        assert_eq!(firsts, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn one_location_comes_as_an_object() {
        let body = json!({"minutely_15": {"time": [900], "wind_speed_100m": [4.0]}});
        let s = parse_many(&body, 1, &["m"], &["wind_speed_100m"], "").unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0][0].points, vec![(900, 4.0)]);
    }

    #[test]
    fn wrong_location_counts_are_errors() {
        let block = json!({"minutely_15": {"time": [900], "wind_speed_100m": [4.0]}});
        let vars = &["wind_speed_100m"];
        assert!(parse_many(&block, 2, &["m"], vars, "").is_err());
        assert!(parse_many(&json!([block.clone()]), 2, &["m"], vars, "").is_err());
        let dup = json!([block.clone(), {"location_id": 0, "minutely_15": block["minutely_15"]}]);
        assert!(parse_many(&dup, 2, &["m"], vars, "").is_err());
        assert!(parse_many(&json!([]), 0, &["m"], vars, "").unwrap().is_empty());
        // An API error for several locations comes as one object.
        let err = json!({"error": true, "reason": "too many locations"});
        let msg = parse_many(&err, 2, &["m"], vars, "").unwrap_err().to_string();
        assert!(msg.contains("too many locations"), "{msg}");
    }

    #[test]
    fn coordinates_are_rounded_and_comma_separated() {
        let l = |lat, lon| Location { lat, lon };
        assert_eq!(
            coords(&[l(52.6, 3.9), l(54.304, 9.496)]),
            ("52.6,54.3".to_string(), "3.9,9.5".to_string())
        );
        assert_eq!(
            coords(&[l(52.123, 4.987)]),
            ("52.12".to_string(), "4.99".to_string())
        );
    }

    #[test]
    fn reports_api_errors() {
        let body = json!({"error": true, "reason": "bad model"});
        assert!(parse(&body, &["x"], &["y"], "").is_err());
    }
}
