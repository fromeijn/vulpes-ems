//! EnergyZero day-ahead electricity prices (NL, EUR/kWh excl. VAT).
//! No API key needed. Interim wholesale source until ENTSO-E is wired up.

use crate::{Error, Result};
use chrono::{DateTime, Duration, NaiveDate};
use serde::Deserialize;

const URL: &str = "https://api.energyzero.nl/v1/energyprices";

#[derive(Deserialize)]
struct Response {
    #[serde(rename = "Prices")]
    prices: Vec<Price>,
}

#[derive(Deserialize)]
struct Price {
    #[serde(rename = "readingDate")]
    reading_date: String,
    price: f64,
}

/// Hourly prices `(slot_start_ts, eur_per_kwh_excl_vat)` for an inclusive UTC
/// date range, fetched in chunks of at most 31 days.
pub fn fetch_hourly(start: NaiveDate, end: NaiveDate) -> Result<Vec<(i64, f64)>> {
    let agent = crate::agent();
    let mut out = Vec::new();
    let mut from = start;
    while from <= end {
        let till = (from + Duration::days(30)).min(end);
        let resp: Response = agent
            .get(URL)
            .query("fromDate", format!("{from}T00:00:00.000Z"))
            .query("tillDate", format!("{till}T23:59:59.999Z"))
            .query("interval", "4")
            .query("usageType", "1")
            .query("inclBtw", "false")
            .call()?
            .body_mut()
            .read_json()?;
        out.extend(parse(resp)?);
        from = till + Duration::days(1);
    }
    out.sort_by_key(|p| p.0);
    out.dedup_by_key(|p| p.0);
    Ok(out)
}

fn parse(resp: Response) -> Result<Vec<(i64, f64)>> {
    resp.prices
        .into_iter()
        .map(|p| {
            DateTime::parse_from_rfc3339(&p.reading_date)
                .map(|t| (t.timestamp(), p.price))
                .map_err(|e| Error::Parse(format!("bad readingDate '{}': {e}", p.reading_date)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prices() {
        let r: Response = serde_json::from_str(
            r#"{"Prices":[{"readingDate":"2025-06-01T00:00:00Z","price":0.08656},
                          {"readingDate":"2025-06-01T01:00:00Z","price":-0.01}]}"#,
        )
        .unwrap();
        assert_eq!(
            parse(r).unwrap(),
            vec![(1_748_736_000, 0.08656), (1_748_739_600, -0.01)]
        );
    }
}
