//! Timestamps in the forms Home Assistant hands them over: Unix seconds, or
//! ISO 8601 strings as integrations put them in state attributes.

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::Value;

/// Unix seconds from a number or an ISO 8601 string, like Python's
/// `datetime.fromisoformat(s).timestamp()`. A string without an offset is
/// read in `tz` (the Home Assistant time zone). Booleans count as numbers,
/// as they do in Python.
pub fn parse_ts(v: &Value, tz: Tz) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::String(s) => parse_iso(s, tz),
        _ => None,
    }
}

fn parse_iso(s: &str, tz: Tz) -> Option<f64> {
    let mut s = s.trim().to_string();
    if s.len() > 10 && s.as_bytes()[10] == b' ' {
        s.replace_range(10..11, "T");
    }
    if let Some(stripped) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
        s = format!("{stripped}+00:00");
    }
    const OFFSET: [&str; 4] = [
        "%Y-%m-%dT%H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M:%S%.f%z",
        "%Y-%m-%dT%H:%M%:z",
        "%Y-%m-%dT%H:%M%z",
    ];
    for f in OFFSET {
        if let Ok(t) = DateTime::parse_from_str(&s, f) {
            return Some(seconds(t.timestamp(), t.timestamp_subsec_nanos()));
        }
    }
    const NAIVE: [&str; 2] = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"];
    let naive = NAIVE
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(&s, f).ok())
        .or_else(|| {
            NaiveDate::parse_from_str(&s, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
        })?;
    let t = tz.from_local_datetime(&naive).earliest()?;
    Some(seconds(t.timestamp(), t.timestamp_subsec_nanos()))
}

fn seconds(secs: i64, nanos: u32) -> f64 {
    secs as f64 + f64::from(nanos) / 1e9
}

/// `datetime.fromtimestamp(ts, UTC).isoformat()`: `2026-11-08T12:15:00+00:00`,
/// with microseconds only when there are any.
pub fn iso_utc(ts: f64) -> String {
    let mut secs = ts.floor() as i64;
    let mut micros = ((ts - ts.floor()) * 1e6).round_ties_even() as i64;
    if micros >= 1_000_000 {
        secs += 1;
        micros -= 1_000_000;
    }
    let t = Utc.timestamp_opt(secs, 0).single().unwrap_or_default();
    let base = t.format("%Y-%m-%dT%H:%M:%S").to_string();
    if micros == 0 {
        format!("{base}+00:00")
    } else {
        format!("{base}.{micros:06}+00:00")
    }
}

/// A time zone by IANA name; UTC when unknown or empty.
pub fn zone(name: &str) -> Tz {
    name.parse().unwrap_or(Tz::UTC)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const T0: f64 = 1_794_096_000.0; // 2026-11-08 00:00 UTC
    const AMS: Tz = chrono_tz::Europe::Amsterdam;

    #[test]
    fn iso_strings_with_an_offset() {
        for s in [
            "2026-11-08T00:00:00+00:00",
            "2026-11-08 00:00:00+00:00",
            "2026-11-08T00:00:00Z",
            "2026-11-08T01:00:00+01:00",
            "2026-11-08T01:00:00+0100",
            "2026-11-08T00:00+00:00",
        ] {
            assert_eq!(parse_ts(&json!(s), AMS), Some(T0), "{s}");
        }
        assert_eq!(
            parse_ts(&json!("2026-11-08T00:00:00.5+00:00"), AMS),
            Some(T0 + 0.5)
        );
    }

    #[test]
    fn naive_strings_are_local_time() {
        // Amsterdam is UTC+1 in November.
        assert_eq!(parse_ts(&json!("2026-11-08T01:00:00"), AMS), Some(T0));
        assert_eq!(parse_ts(&json!("2026-11-08T01:00"), AMS), Some(T0));
        assert_eq!(parse_ts(&json!("2026-11-08"), Tz::UTC), Some(T0));
    }

    #[test]
    fn numbers_and_junk() {
        assert_eq!(parse_ts(&json!(T0), AMS), Some(T0));
        assert_eq!(parse_ts(&json!(1794096000), AMS), Some(T0));
        assert_eq!(parse_ts(&json!(true), AMS), Some(1.0));
        assert_eq!(parse_ts(&json!("soon"), AMS), None);
        assert_eq!(parse_ts(&json!(null), AMS), None);
        assert_eq!(parse_ts(&json!({"a": 1}), AMS), None);
    }

    #[test]
    fn utc_iso_format_like_python() {
        assert_eq!(iso_utc(T0 + 12.25 * 3600.0), "2026-11-08T12:15:00+00:00");
        assert_eq!(iso_utc(T0 + 0.25), "2026-11-08T00:00:00.250000+00:00");
        assert_eq!(iso_utc(T0 - 0.0000001), "2026-11-08T00:00:00+00:00");
    }

    #[test]
    fn zones() {
        assert_eq!(zone("Europe/Amsterdam"), AMS);
        assert_eq!(zone(""), Tz::UTC);
        assert_eq!(zone("Mars/Base"), Tz::UTC);
    }
}
