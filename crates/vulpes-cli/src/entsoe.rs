//! ENTSO-E backfill. The token comes from `ENTSOE_TOKEN` or `local/entsoe.token`
//! and is never printed.

use anyhow::{Context, Result, bail};
use chrono::{Duration, NaiveDate};
use vulpes_core::{ForecastPoint, SeriesKind, Store};
use vulpes_sources::entsoe::{self, Query, psr};

pub(crate) fn token() -> Result<String> {
    pick_token(std::env::var("ENTSOE_TOKEN").ok(), || {
        std::fs::read_to_string("local/entsoe.token")
    })
}

/// The environment wins when set and non-empty; otherwise the token file.
fn pick_token(env: Option<String>, file: impl FnOnce() -> std::io::Result<String>) -> Result<String> {
    if let Some(t) = env.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        return Ok(t.to_string());
    }
    let t = file().context("no ENTSO-E token: set ENTSOE_TOKEN or create local/entsoe.token")?;
    let t = t.trim();
    if t.is_empty() {
        bail!("local/entsoe.token is empty");
    }
    Ok(t.to_string())
}

/// Day-ahead forecasts must be published by 18:00 Brussels time on the day
/// before the (Brussels-local) delivery day. Returns that deadline for the
/// value at `t`, used as its issue time so nothing is known too early.
fn day_ahead_deadline(t: i64) -> i64 {
    use chrono::TimeZone;
    let tz = chrono_tz::Europe::Brussels;
    let Some(local) = tz.timestamp_opt(t, 0).single() else {
        return t - 30 * 3600; // unreachable for valid timestamps; stay conservative
    };
    let day_before = local.date_naive() - Duration::days(1);
    day_before
        .and_hms_opt(18, 0, 0)
        .and_then(|d| tz.from_local_datetime(&d).single())
        .map_or(t - 30 * 3600, |d| d.timestamp())
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
    for (start, end) in crate::date_chunks(from, to, 31) {
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
                let pts: Vec<ForecastPoint> = s
                    .points
                    .iter()
                    .map(|&(t, v)| ForecastPoint {
                        valid_ts: t,
                        issued_ts: day_ahead_deadline(t),
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
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn no_file() -> io::Result<String> {
        Err(io::Error::new(io::ErrorKind::NotFound, "missing"))
    }

    #[test]
    fn token_sources_and_precedence() {
        assert_eq!(
            pick_token(Some(" env-tok \n".into()), no_file).unwrap(),
            "env-tok"
        );
        assert_eq!(
            pick_token(Some("env".into()), || Ok("file".into())).unwrap(),
            "env"
        );
        assert_eq!(
            pick_token(None, || Ok("  file-tok\n".into())).unwrap(),
            "file-tok"
        );
        assert_eq!(
            pick_token(Some("   ".into()), || Ok("file".into())).unwrap(),
            "file"
        );
        assert!(pick_token(None, no_file).is_err());
        assert!(pick_token(None, || Ok(" \n".into())).is_err());
    }

    #[test]
    fn zone_codes() {
        assert_eq!(zone_code("nl").unwrap(), entsoe::zone::NL);
        assert_eq!(zone_code("DE-LU").unwrap(), entsoe::zone::DE_LU);
        assert_eq!(zone_code("de_lu").unwrap(), entsoe::zone::DE_LU);
        let err = zone_code("fr").unwrap_err().to_string();
        assert!(err.contains("unknown zone 'FR'"), "{err}");
    }

    #[test]
    fn day_ahead_deadline_uses_the_local_delivery_day() {
        // Winter (CET, UTC+1): 2026-01-15 12:00 UTC → deadline 14 Jan 18:00 CET = 17:00 UTC.
        assert_eq!(day_ahead_deadline(1_768_478_400), 1_768_410_000);
        // 23:30 UTC on 14 Jan is already 15 Jan in Brussels: same deadline.
        assert_eq!(day_ahead_deadline(1_768_433_400), 1_768_410_000);
        // Summer (CEST, UTC+2): 2026-07-15 12:00 UTC → 14 Jul 18:00 CEST = 16:00 UTC.
        assert_eq!(day_ahead_deadline(1_784_116_800), 1_784_044_800);
        // Always before delivery.
        for t in (1_767_225_600..1_798_761_600).step_by(3600 * 7) {
            let d = day_ahead_deadline(t);
            assert!(d < t && t - d <= 30 * 3600 + 3600, "{t} -> {d}");
        }
    }
}
