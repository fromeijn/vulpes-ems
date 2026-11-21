//! EnergyZero day-ahead electricity prices (NL, EUR/kWh excl. VAT).
//! No API key needed. Interim wholesale source until ENTSO-E is wired up.

use crate::{Error, Result};
use chrono::{DateTime, Duration, NaiveDate};
use serde::Deserialize;

const URL: &str = "https://api.energyzero.nl/v1/energyprices";
/// The newer endpoint: per 15 minutes or per hour, for one market day and the
/// one before it.
const PRICES_URL: &str = "https://public.api.energyzero.nl/public/v1/prices";

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

/// The published wholesale prices `(start, end, eur_per_kwh_excl_vat)` of the
/// market days around `today` (a date in the market's own time zone), per 15
/// minutes or per hour: tomorrow and today once tomorrow is published, else
/// today and yesterday. One request, two when tomorrow is not published yet.
pub fn fetch_published(today: NaiveDate, quarter: bool) -> Result<Vec<(i64, i64, f64)>> {
    let agent = crate::agent();
    let interval = if quarter {
        "INTERVAL_QUARTER"
    } else {
        "INTERVAL_HOUR"
    };
    let get = |date: NaiveDate| -> Result<Option<PricesResponse>> {
        let resp = agent
            .get(PRICES_URL)
            .query("energyType", "ENERGY_TYPE_ELECTRICITY")
            .query("date", date.format("%d-%m-%Y").to_string())
            .query("interval", interval)
            .call();
        match resp {
            // "no prices found for the given date": not published yet.
            Err(ureq::Error::StatusCode(404)) => Ok(None),
            r => Ok(Some(r?.body_mut().read_json()?)),
        }
    };
    let resp = match get(today + Duration::days(1))? {
        Some(r) => r,
        None => get(today)?.unwrap_or_default(),
    };
    parse_published(resp)
}

#[derive(Deserialize, Default)]
struct PricesResponse {
    #[serde(default)]
    base: Vec<Block>,
}

#[derive(Deserialize)]
struct Block {
    start: String,
    end: String,
    price: BlockPrice,
}

#[derive(Deserialize)]
struct BlockPrice {
    /// A decimal string, e.g. `"0.19309"`.
    value: String,
}

fn parse_published(resp: PricesResponse) -> Result<Vec<(i64, i64, f64)>> {
    let time = |s: &str| {
        DateTime::parse_from_rfc3339(s)
            .map(|t| t.timestamp())
            .map_err(|e| Error::Parse(format!("bad time '{s}': {e}")))
    };
    let mut out = resp
        .base
        .into_iter()
        .map(|b| {
            let price: f64 = b
                .price
                .value
                .parse()
                .map_err(|e| Error::Parse(format!("bad price '{}': {e}", b.price.value)))?;
            let (start, end) = (time(&b.start)?, time(&b.end)?);
            if end <= start || !price.is_finite() {
                return Err(Error::Parse(format!("bad price block {}..{}", b.start, b.end)));
            }
            Ok((start, end, price))
        })
        .collect::<Result<Vec<_>>>()?;
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

    #[test]
    fn parses_published_quarters_and_hours() {
        let r: PricesResponse = serde_json::from_str(
            r#"{"interval": "INTERVAL_QUARTER", "range": {},
                "base": [
                  {"start": "2026-11-20T23:15:00Z", "end": "2026-11-20T23:30:00Z", "price": {"value": "0.19"}},
                  {"start": "2026-11-20T23:00:00Z", "end": "2026-11-20T23:15:00Z", "price": {"value": "0.19309"}}
                ],
                "all_in": [{"start": "2026-11-20T23:00:00Z", "end": "2026-11-20T23:15:00Z", "price": {"value": "0.28"}}]}"#,
        )
        .unwrap();
        // Sorted, the wholesale (`base`) price, not the all-in one.
        assert_eq!(
            parse_published(r).unwrap(),
            vec![
                (1_795_215_600, 1_795_216_500, 0.19309),
                (1_795_216_500, 1_795_217_400, 0.19)
            ]
        );
        let hour: PricesResponse = serde_json::from_str(
            r#"{"base": [{"start": "2026-11-20T23:00:00Z", "end": "2026-11-21T00:00:00Z", "price": {"value": "-0.01"}}]}"#,
        )
        .unwrap();
        assert_eq!(
            parse_published(hour).unwrap(),
            vec![(1_795_215_600, 1_795_219_200, -0.01)]
        );
    }

    #[test]
    fn published_errors_and_empty() {
        let parse = |s: &str| parse_published(serde_json::from_str(s).unwrap());
        assert!(parse(r#"{}"#).unwrap().is_empty());
        assert!(parse(r#"{"base": []}"#).unwrap().is_empty());
        let block = |start: &str, end: &str, v: &str| {
            format!(r#"{{"base": [{{"start": "{start}", "end": "{end}", "price": {{"value": "{v}"}}}}]}}"#)
        };
        let (a, b) = ("2026-11-20T23:00:00Z", "2026-11-20T23:15:00Z");
        assert!(parse(&block(a, b, "n/a")).is_err());
        assert!(parse(&block(a, b, "NaN")).is_err());
        assert!(parse(&block("yesterday", b, "0.1")).is_err());
        assert!(parse(&block(b, a, "0.1")).is_err(), "end before start");
    }

    #[test]
    fn bad_dates_are_errors_and_empty_is_fine() {
        let r: Response =
            serde_json::from_str(r#"{"Prices": [{"readingDate": "yesterday", "price": 0.1}]}"#).unwrap();
        assert!(parse(r).is_err());
        let r: Response = serde_json::from_str(r#"{"Prices": []}"#).unwrap();
        assert!(parse(r).unwrap().is_empty());
    }
}
