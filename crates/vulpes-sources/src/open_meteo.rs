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
    /// Live forecast for the coming days (max 16).
    Forecast { days: u8 },
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

pub fn fetch(req: &Request<'_>) -> Result<Vec<WeatherSeries>> {
    let loc = req.location.rounded();
    let dates = |start: chrono::NaiveDate, end: chrono::NaiveDate| {
        vec![("start_date", start.to_string()), ("end_date", end.to_string())]
    };
    let (url, range_params, suffix) = match req.range {
        Range::Forecast { days } => (
            FORECAST_URL,
            vec![("forecast_days", days.to_string())],
            String::new(),
        ),
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
    };
    let requested: Vec<String> = req.vars.iter().map(|v| format!("{v}{suffix}")).collect();
    let mut r = crate::agent()
        .get(url)
        .query("latitude", loc.lat.to_string())
        .query("longitude", loc.lon.to_string())
        .query("minutely_15", requested.join(","))
        .query("models", req.models.join(","))
        .query("timeformat", "unixtime")
        .query("timezone", "UTC")
        .query("wind_speed_unit", "ms");
    for (k, v) in range_params {
        r = r.query(k, v);
    }
    let body: Value = r.call()?.body_mut().read_json()?;
    parse(&body, req.models, req.vars, &suffix)
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
    fn reports_api_errors() {
        let body = json!({"error": true, "reason": "bad model"});
        assert!(parse(&body, &["x"], &["y"], "").is_err());
    }
}
