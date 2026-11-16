//! Reading settings and other JSON from Home Assistant the way the Python
//! integration did: `cfg.get(key, default)`, `float(value)` and truthiness.
//!
//! Settings stay a JSON map rather than a typed struct, so keys this crate
//! doesn't know (from newer or older versions, or plane loss parameters)
//! pass through untouched.

use crate::{Error, Result};
use serde_json::{Map, Value};

pub type Cfg = Map<String, Value>;

/// `float(value)` in Python: numbers, booleans and numeric strings.
pub fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Python truthiness of a JSON value; a missing value is false.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `float(cfg[key])`: an error when missing or not a number.
pub fn num(cfg: &Cfg, key: &str) -> Result<f64> {
    let v = cfg
        .get(key)
        .ok_or_else(|| Error::Message(format!("missing setting '{key}'")))?;
    as_f64(v).ok_or_else(|| Error::Message(format!("setting '{key}' is not a number")))
}

/// `float(cfg.get(key, default))`; a null value counts as missing.
pub fn num_or(cfg: &Cfg, key: &str, default: f64) -> f64 {
    cfg.get(key)
        .filter(|v| !v.is_null())
        .and_then(as_f64)
        .unwrap_or(default)
}

/// `cfg.get(key) or ""` for a string.
pub fn text<'a>(cfg: &'a Cfg, key: &str) -> &'a str {
    cfg.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `cfg.get(key, [])` for a list of strings (statistic ids).
pub fn str_list(cfg: &Cfg, key: &str) -> Vec<String> {
    cfg.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default()
}

/// Python `==` between JSON values: numbers compare by value (`1 == 1.0`,
/// `False == 0`), lists and dicts element by element.
pub fn py_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| py_eq(v, w)))
        }
        (Value::Number(_) | Value::Bool(_), Value::Number(_) | Value::Bool(_)) => as_f64(a) == as_f64(b),
        _ => a == b,
    }
}

/// A required object field of a request.
pub fn obj<'a>(req: &'a Value, key: &str) -> Result<&'a Cfg> {
    req.get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| Error::Message(format!("'{key}' must be an object")))
}

/// An optional object field: missing or null gives an empty map.
pub fn obj_or_empty(req: &Value, key: &str) -> Cfg {
    req.get(key)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// A required number field of a request.
pub fn req_num(req: &Value, key: &str) -> Result<f64> {
    req.get(key)
        .and_then(as_f64)
        .ok_or_else(|| Error::Message(format!("'{key}' must be a number")))
}

/// Python `round(x, digits)`: rounds the exact binary value, as Python does.
pub fn round_to(x: f64, digits: usize) -> f64 {
    format!("{x:.digits$}").parse().unwrap_or(x)
}

/// A JSON number that is an integer when the value is whole and fits, so
/// Python sees `int` where it used to compute an `int`.
pub fn int_value(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() < 9.0e15 {
        Value::from(x as i64)
    } else {
        Value::from(x)
    }
}

/// `[ts, value]` pairs under `key` as `(i64, f64)`; malformed pairs are skipped,
/// a missing key gives none.
pub fn ts_pairs(req: &Value, key: &str) -> Vec<(i64, f64)> {
    req.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| Some((p.get(0).and_then(as_f64)? as i64, p.get(1).and_then(as_f64)?)))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(v: Value) -> Cfg {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn ts_pairs_skip_malformed_rows() {
        let r = json!({"w": [[3600, 0.1], [7200.0, "0.2"], ["x", 1], [1], null]});
        assert_eq!(ts_pairs(&r, "w"), [(3600, 0.1), (7200, 0.2)]);
        assert!(ts_pairs(&r, "missing").is_empty());
        assert!(ts_pairs(&json!({"w": 3}), "w").is_empty());
    }

    #[test]
    fn python_float_conversion() {
        assert_eq!(as_f64(&json!(3)), Some(3.0));
        assert_eq!(as_f64(&json!(2.5)), Some(2.5));
        assert_eq!(as_f64(&json!(" 95 ")), Some(95.0));
        assert_eq!(as_f64(&json!(true)), Some(1.0));
        assert_eq!(as_f64(&json!("n/a")), None);
        assert_eq!(as_f64(&json!(null)), None);
        assert_eq!(as_f64(&json!([1])), None);
    }

    #[test]
    fn python_truthiness() {
        for v in [
            json!(true),
            json!(1),
            json!(0.5),
            json!("x"),
            json!([0]),
            json!({"a": 1}),
        ] {
            assert!(truthy(Some(&v)), "{v}");
        }
        for v in [
            json!(false),
            json!(0),
            json!(0.0),
            json!(""),
            json!([]),
            json!({}),
            json!(null),
        ] {
            assert!(!truthy(Some(&v)), "{v}");
        }
        assert!(!truthy(None));
    }

    #[test]
    fn settings_accessors() {
        let c = cfg(json!({"a": 2, "b": null, "s": "x", "l": ["p", 3, "q"]}));
        assert_eq!(num(&c, "a").unwrap(), 2.0);
        assert!(
            num(&c, "missing")
                .unwrap_err()
                .to_string()
                .contains("missing setting 'missing'")
        );
        assert!(num(&c, "s").is_err());
        assert_eq!(num_or(&c, "b", 7.0), 7.0);
        assert_eq!(num_or(&c, "zz", 7.0), 7.0);
        assert_eq!(text(&c, "s"), "x");
        assert_eq!(text(&c, "a"), "");
        assert_eq!(str_list(&c, "l"), ["p", "q"]);
        assert!(str_list(&c, "zz").is_empty());
    }

    #[test]
    fn python_equality() {
        assert!(py_eq(
            &json!([{"a": 1, "b": [2.0]}]),
            &json!([{"b": [2], "a": 1.0}])
        ));
        assert!(py_eq(&json!(false), &json!(0)));
        assert!(!py_eq(&json!({"a": 1}), &json!({"a": 1, "b": 2})));
        assert!(!py_eq(&json!("1"), &json!(1)));
        assert!(!py_eq(&json!([1, 2]), &json!([1])));
    }

    #[test]
    fn rounding_like_python() {
        assert_eq!(round_to(24.64, 1), 24.6);
        assert_eq!(round_to(3.2127, 3), 3.213);
        // 2.675 is stored as 2.67499999…, so Python's round(2.675, 2) is 2.67.
        assert_eq!(round_to(2.675, 2), 2.67);
        assert_eq!(round_to(-0.04, 1), -0.0);
    }

    #[test]
    fn whole_numbers_become_integers() {
        assert_eq!(int_value(3000.0), json!(3000));
        assert_eq!(int_value(-1200.0), json!(-1200));
        assert_eq!(int_value(2.5), json!(2.5));
    }

    #[test]
    fn request_fields() {
        let r = json!({"cfg": {"a": 1}, "n": "4", "x": 1});
        assert_eq!(obj(&r, "cfg").unwrap().len(), 1);
        assert!(obj(&r, "x").is_err());
        assert!(obj_or_empty(&r, "missing").is_empty());
        assert_eq!(req_num(&r, "n").unwrap(), 4.0);
        assert!(req_num(&r, "missing").is_err());
    }
}
