//! ENTSO-E backfill. The token comes from `ENTSOE_TOKEN` or `local/entsoe.token`
//! and is never printed.

use anyhow::{Context, Result, bail};
use chrono::{Duration, NaiveDate};
use vulpes_core::{ForecastPoint, SeriesKind, Store};
use vulpes_sources::entsoe::{self, Query, psr};

pub(crate) fn token() -> Result<String> {
    if let Ok(t) = std::env::var("ENTSOE_TOKEN")
        && !t.trim().is_empty()
    {
        return Ok(t.trim().to_string());
    }
    let t = std::fs::read_to_string("local/entsoe.token")
        .context("no ENTSO-E token: set ENTSOE_TOKEN or create local/entsoe.token")?;
    let t = t.trim().to_string();
    if t.is_empty() {
        bail!("local/entsoe.token is empty");
    }
    Ok(t)
}

fn zone_code(zone: &str) -> Result<&'static str> {
    Ok(match zone.to_ascii_uppercase().as_str() {
        "NL" => entsoe::zone::NL,
        "DE-LU" | "DE_LU" => entsoe::zone::DE_LU,
        z => bail!("unknown zone '{z}' (supported: NL, DE-LU)"),
    })
}

/// Backfill day-ahead prices (`price.entsoe.<zone>`, EUR/kWh) and fundamentals
/// (`entsoe:<zone>` forecasts: solar, wind_onshore, wind_offshore, load in MW,
/// marked as issued at the regulatory publication deadline).
pub(crate) fn backfill(store: &mut Store, zone: &str, from: NaiveDate, to: NaiveDate) -> Result<()> {
    let token = token()?;
    let code = zone_code(zone)?;
    let label = zone.to_ascii_lowercase();
    let price_key = format!("price.entsoe.{label}");
    store.ensure_series(&price_key, "EUR/kWh", SeriesKind::Mean)?;
    let mut start = from;
    while start <= to {
        let end = (start + Duration::days(30)).min(to);
        let (t0, t1) = (
            start.and_hms_opt(0, 0, 0).context("date")?.and_utc(),
            (end + Duration::days(1))
                .and_hms_opt(0, 0, 0)
                .context("date")?
                .and_utc(),
        );
        let mut counts = (0, 0);
        for s in entsoe::fetch(&token, Query::DayAheadPrices { zone: code }, t0, t1)? {
            let pts: Vec<(i64, f64)> = s.points.iter().map(|&(t, v)| (t, v / 1000.0)).collect();
            counts.0 += store.put_samples(&price_key, s.step_s, &pts)?;
        }
        let source = format!("entsoe:{label}");
        let mut put = |var: &str, series: Vec<entsoe::Series>| -> Result<usize> {
            let mut n = 0;
            for s in series {
                // Day-ahead forecasts must be published by 18:00 Brussels time
                // the day before delivery: mark them issued at 17:00 UTC on D-1.
                let pts: Vec<ForecastPoint> = s
                    .points
                    .iter()
                    .map(|&(t, v)| ForecastPoint {
                        valid_ts: t,
                        issued_ts: t - t.rem_euclid(86_400) - 7 * 3600,
                        value: v,
                    })
                    .collect();
                n += store.put_forecasts(&source, var, &pts)?;
            }
            Ok(n)
        };
        let ws = entsoe::fetch(&token, Query::WindSolarForecast { zone: code }, t0, t1)?;
        for (psr_code, var) in [
            (psr::SOLAR, "solar"),
            (psr::WIND_ONSHORE, "wind_onshore"),
            (psr::WIND_OFFSHORE, "wind_offshore"),
        ] {
            let sel: Vec<_> = ws
                .iter()
                .filter(|s| s.psr_type.as_deref() == Some(psr_code))
                .cloned()
                .collect();
            counts.1 += put(var, sel)?;
        }
        counts.1 += put(
            "load",
            entsoe::fetch(&token, Query::LoadForecast { zone: code }, t0, t1)?,
        )?;
        println!(
            "{start}..{end}: {} prices, {} forecast values",
            counts.0, counts.1
        );
        start = end + Duration::days(1);
    }
    Ok(())
}
