//! ENTSO-E Transparency Platform client: day-ahead prices (A44), day-ahead
//! wind/solar generation forecasts (A69) and load forecasts (A65).
//!
//! The security token is a secret: it is only placed in the request URL and
//! is redacted from every error message this module returns.

use crate::{Error, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use quick_xml::Reader;
use quick_xml::events::Event;

const URL: &str = "https://web-api.tp.entsoe.eu/api";

/// Bidding-zone EIC codes.
pub mod zone {
    pub const NL: &str = "10YNL----------L";
    pub const DE_LU: &str = "10Y1001A1001A82H";
}

/// Production types for generation forecasts.
pub mod psr {
    pub const SOLAR: &str = "B16";
    pub const WIND_OFFSHORE: &str = "B18";
    pub const WIND_ONSHORE: &str = "B19";
}

/// One parsed time series: its production type (if any) and `(start, value)` points.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub psr_type: Option<String>,
    pub step_s: i64,
    pub points: Vec<(i64, f64)>,
}

#[derive(Debug, Clone, Copy)]
pub enum Query<'a> {
    /// Day-ahead prices (EUR/MWh) for a bidding zone.
    DayAheadPrices { zone: &'a str },
    /// Day-ahead wind and solar generation forecast (MW).
    WindSolarForecast { zone: &'a str },
    /// Day-ahead total load forecast (MW).
    LoadForecast { zone: &'a str },
}

fn fmt(t: DateTime<Utc>) -> String {
    t.format("%Y%m%d%H%M").to_string()
}

/// Fetch and parse. `[start, end)` must be at most one year; callers should
/// chunk longer ranges. Returns no series when ENTSO-E has no data.
pub fn fetch(token: &str, query: Query<'_>, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Vec<Series>> {
    let params = query_params(query, start, end);
    let redact = |msg: String| redact(&msg, token);
    let mut req = crate::agent().get(URL).query("securityToken", token);
    for (k, v) in &params {
        req = req.query(*k, v);
    }
    let body = match req.call() {
        Ok(mut resp) => resp
            .body_mut()
            .read_to_string()
            .map_err(|e| Error::Parse(redact(e.to_string())))?,
        // ENTSO-E answers "no data" with HTTP 400 and an acknowledgement document.
        Err(ureq::Error::StatusCode(400)) => return Ok(Vec::new()),
        Err(e) => return Err(Error::Parse(redact(format!("ENTSO-E request failed: {e}")))),
    };
    parse(&body)
}

/// Remove the security token from a message before it can be shown or logged.
fn redact(msg: &str, token: &str) -> String {
    if token.is_empty() {
        msg.to_string()
    } else {
        msg.replace(token, "<redacted>")
    }
}

/// Query parameters for a request, without the security token.
fn query_params(query: Query<'_>, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<(&'static str, String)> {
    let mut params: Vec<(&'static str, String)> = match query {
        Query::DayAheadPrices { zone } => vec![
            ("documentType", "A44".into()),
            ("in_Domain", zone.into()),
            ("out_Domain", zone.into()),
        ],
        Query::WindSolarForecast { zone } => {
            vec![
                ("documentType", "A69".into()),
                ("processType", "A01".into()),
                ("in_Domain", zone.into()),
            ]
        }
        Query::LoadForecast { zone } => vec![
            ("documentType", "A65".into()),
            ("processType", "A01".into()),
            ("outBiddingZone_Domain", zone.into()),
        ],
    };
    params.push(("periodStart", fmt(start)));
    params.push(("periodEnd", fmt(end)));
    params
}

fn parse_time(s: &str) -> Result<i64> {
    // e.g. 2026-11-05T23:00Z
    NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%dT%H:%MZ")
        .map(|t| t.and_utc().timestamp())
        .map_err(|e| Error::Parse(format!("bad ENTSO-E time '{s}': {e}")))
}

fn parse_resolution(s: &str) -> Result<i64> {
    match s.trim() {
        "PT15M" => Ok(900),
        "PT30M" => Ok(1800),
        "PT60M" | "PT1H" => Ok(3600),
        other => Err(Error::Parse(format!("unsupported resolution '{other}'"))),
    }
}

/// Parse an ENTSO-E market document. Handles curve type A03 (positions whose
/// value did not change are omitted and must be forward-filled) as well as
/// the plain A01 curve.
pub fn parse(xml: &str) -> Result<Vec<Series>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out = Vec::new();
    let mut path: Vec<String> = Vec::new();
    // Current time series / period state.
    let mut psr: Option<String> = None;
    let mut curve_a03 = false;
    let (mut p_start, mut p_end, mut step) = (0i64, 0i64, 0i64);
    let mut raw: Vec<(usize, f64)> = Vec::new();
    let mut position: Option<usize> = None;
    let mut points: Vec<(i64, f64)> = Vec::new();
    loop {
        match reader.read_event().map_err(|e| Error::Parse(e.to_string()))? {
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if name == "TimeSeries" {
                    psr = None;
                    curve_a03 = false;
                    points.clear();
                }
                if name == "Period" {
                    raw.clear();
                }
                path.push(name);
            }
            Event::End(_) => {
                let name = path.pop().unwrap_or_default();
                match name.as_str() {
                    "Point" => position = None,
                    "Period" => {
                        let n = ((p_end - p_start) / step.max(1)) as usize;
                        raw.sort_by_key(|r| r.0);
                        let mut last: Option<f64> = None;
                        let mut it = raw.iter().peekable();
                        for pos in 1..=n {
                            if let Some(&&(p, v)) = it.peek()
                                && p == pos
                            {
                                last = Some(v);
                                it.next();
                            } else if !curve_a03 {
                                last = None; // A01: a missing position is a gap
                            }
                            if let Some(v) = last {
                                points.push((p_start + (pos as i64 - 1) * step, v));
                            }
                        }
                    }
                    "TimeSeries" => out.push(Series {
                        psr_type: psr.take(),
                        step_s: step,
                        points: std::mem::take(&mut points),
                    }),
                    _ => {}
                }
            }
            Event::Text(t) => {
                let text = t.decode().map_err(|e| Error::Parse(e.to_string()))?.into_owned();
                let leaf = path.last().map(String::as_str).unwrap_or("");
                let parent = path
                    .len()
                    .checked_sub(2)
                    .and_then(|i| path.get(i))
                    .map(String::as_str)
                    .unwrap_or("");
                match (parent, leaf) {
                    ("MktPSRType", "psrType") => psr = Some(text),
                    ("TimeSeries", "curveType") => curve_a03 = text.trim() == "A03",
                    ("timeInterval", "start") if path.iter().any(|p| p == "Period") => {
                        p_start = parse_time(&text)?
                    }
                    ("timeInterval", "end") if path.iter().any(|p| p == "Period") => {
                        p_end = parse_time(&text)?
                    }
                    ("Period", "resolution") => step = parse_resolution(&text)?,
                    ("Point", "position") => {
                        position = Some(
                            text.trim()
                                .parse()
                                .map_err(|_| Error::Parse(format!("bad position '{text}'")))?,
                        )
                    }
                    ("Point", "price.amount" | "quantity") => {
                        let v: f64 = text
                            .trim()
                            .parse()
                            .map_err(|_| Error::Parse(format!("bad value '{text}'")))?;
                        if let Some(p) = position {
                            raw.push((p, v));
                        }
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRICES_A03: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Publication_MarketDocument xmlns="urn:iec62325.351:tc57wg16:451-3:publicationdocument:7:3">
  <TimeSeries>
    <mRID>1</mRID>
    <curveType>A03</curveType>
    <Period>
      <timeInterval><start>2026-11-05T23:00Z</start><end>2026-11-06T00:00Z</end></timeInterval>
      <resolution>PT15M</resolution>
      <Point><position>1</position><price.amount>80.5</price.amount></Point>
      <Point><position>3</position><price.amount>-1.25</price.amount></Point>
    </Period>
  </TimeSeries>
</Publication_MarketDocument>"#;

    #[test]
    fn forward_fills_a03_curves() {
        let s = parse(PRICES_A03).unwrap();
        assert_eq!(s.len(), 1);
        let t0 = parse_time("2026-11-05T23:00Z").unwrap();
        assert_eq!(s[0].step_s, 900);
        assert_eq!(
            s[0].points,
            vec![
                (t0, 80.5),
                (t0 + 900, 80.5),
                (t0 + 1800, -1.25),
                (t0 + 2700, -1.25)
            ]
        );
    }

    #[test]
    fn reads_generation_by_psr_type() {
        let xml = r#"<GL_MarketDocument>
  <TimeSeries>
    <curveType>A01</curveType>
    <MktPSRType><psrType>B16</psrType></MktPSRType>
    <Period>
      <timeInterval><start>2026-06-01T10:00Z</start><end>2026-06-01T12:00Z</end></timeInterval>
      <resolution>PT60M</resolution>
      <Point><position>1</position><quantity>4100</quantity></Point>
      <Point><position>2</position><quantity>4300</quantity></Point>
    </Period>
  </TimeSeries>
  <TimeSeries>
    <curveType>A01</curveType>
    <MktPSRType><psrType>B19</psrType></MktPSRType>
    <Period>
      <timeInterval><start>2026-06-01T10:00Z</start><end>2026-06-01T11:00Z</end></timeInterval>
      <resolution>PT60M</resolution>
      <Point><position>1</position><quantity>900</quantity></Point>
    </Period>
  </TimeSeries>
</GL_MarketDocument>"#;
        let s = parse(xml).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].psr_type.as_deref(), Some("B16"));
        assert_eq!(s[0].points.len(), 2);
        assert_eq!(s[1].psr_type.as_deref(), Some("B19"));
        assert_eq!(s[1].points[0].1, 900.0);
    }

    #[test]
    fn a01_gap_is_not_filled() {
        let xml = PRICES_A03.replace("A03", "A01");
        let s = parse(&xml).unwrap();
        assert_eq!(s[0].points.len(), 2);
    }

    #[test]
    fn token_is_redacted() {
        let msg = "GET https://x/api?securityToken=abc-123&documentType=A44 failed: abc-123";
        let out = redact(msg, "abc-123");
        assert!(
            !out.contains("abc-123") && out.matches("<redacted>").count() == 2,
            "{out}"
        );
        assert_eq!(redact("no token here", ""), "no token here");
    }

    #[test]
    fn request_parameters() {
        use chrono::TimeZone;
        let (s, e) = (
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap(),
        );
        let get = |q| query_params(q, s, e);
        let prices = get(Query::DayAheadPrices { zone: zone::NL });
        assert!(prices.contains(&("documentType", "A44".into())));
        assert!(
            prices.contains(&("in_Domain", zone::NL.into()))
                && prices.contains(&("out_Domain", zone::NL.into()))
        );
        assert!(
            prices.contains(&("periodStart", "202601010000".into()))
                && prices.contains(&("periodEnd", "202601020000".into()))
        );
        let ws = get(Query::WindSolarForecast { zone: zone::DE_LU });
        assert!(ws.contains(&("documentType", "A69".into())) && ws.contains(&("processType", "A01".into())));
        let load = get(Query::LoadForecast { zone: zone::NL });
        assert!(
            load.contains(&("documentType", "A65".into()))
                && load.contains(&("outBiddingZone_Domain", zone::NL.into()))
        );
        for p in [prices, ws, load] {
            assert!(p.iter().all(|(k, _)| *k != "securityToken"));
        }
    }

    #[test]
    fn times_and_resolutions() {
        assert_eq!(parse_time("2026-11-05T23:00Z").unwrap(), 1_793_919_600);
        assert_eq!(parse_time(" 2026-11-05T23:00Z ").unwrap(), 1_793_919_600);
        assert!(parse_time("2026-11-05 23:00").is_err());
        assert_eq!(parse_resolution("PT15M").unwrap(), 900);
        assert_eq!(parse_resolution("PT30M").unwrap(), 1800);
        assert_eq!(parse_resolution("PT60M").unwrap(), 3600);
        assert_eq!(parse_resolution("PT1H").unwrap(), 3600);
        assert!(parse_resolution("P1D").is_err());
    }
}
