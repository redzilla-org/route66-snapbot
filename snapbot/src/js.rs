//! JavaScript value semantics the former Node handler's signed and returned
//! fields depended on. The signed manifest must stay byte-identical for
//! verifiers, so String(), Number(), toISOString() and the ASCII clamp are
//! reproduced here instead of being approximated at each call site.

use serde_json::Value;

/// `String(v)` for a JSON value (undefined/null handled by callers).
pub fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => num_to_string(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|x| match x {
                Value::Null => String::new(),
                other => js_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// JS Number -> string for the common cases (integers print without ".0").
pub fn num_to_string(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".to_string() } else { "-Infinity".to_string() };
    }
    if f == f.trunc() && f.abs() < 1e21 {
        return format!("{}", f as i128);
    }
    format!("{f}")
}

/// `Number(v)`: NaN for anything non-numeric.
pub fn js_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                0.0
            } else {
                t.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        Some(_) => f64::NAN,
    }
}

/// JS truthiness.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => {
            let f = n.as_f64().unwrap_or(0.0);
            f != 0.0 && !f.is_nan()
        }
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `a ?? b` over optional JSON fields: the first present, non-null value.
pub fn coalesce<'a>(vals: &[Option<&'a Value>]) -> Option<&'a Value> {
    vals.iter().flatten().find(|v| !v.is_null()).copied()
}

/// metadataValue(v, limit): String(v ?? ""), every code unit outside
/// printable ASCII replaced by "?", clamped to `limit` code units.
pub fn metadata_value(v: Option<&Value>, limit: usize) -> String {
    let s = match v {
        None | Some(Value::Null) => String::new(),
        Some(x) => js_string(x),
    };
    metadata_str(&s, limit)
}

pub fn metadata_str(s: &str, limit: usize) -> String {
    let mut out = String::with_capacity(s.len().min(limit));
    for c in s.chars() {
        if (' '..='~').contains(&c) {
            out.push(c);
        } else {
            // A non-BMP character is two UTF-16 code units, i.e. "??" in JS.
            for _ in 0..c.len_utf16() {
                out.push('?');
            }
        }
    }
    out.truncate(limit.min(out.len()));
    out
}

/// `new Date().toISOString()`.
pub fn now_iso() -> String {
    iso_from_millis(chrono::Utc::now().timestamp_millis())
}

pub fn iso_from_millis(ms: i64) -> String {
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).unwrap_or_default();
    dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// An AWS SDK DateTime rendered as JS toISOString().
pub fn iso_from_smithy(d: &aws_smithy_types::DateTime) -> String {
    let ms = d.secs() * 1000 + (d.subsec_nanos() / 1_000_000) as i64;
    iso_from_millis(ms)
}

/// `capturedAt.replace(/[-:]/g, "").replace(/\.\d{3}Z$/, "Z")`.
pub fn compact_stamp(iso: &str) -> String {
    let s: String = iso.chars().filter(|c| *c != '-' && *c != ':').collect();
    match s.rfind('.') {
        Some(i) if s.ends_with('Z') && s.len() - i == 5 => format!("{}Z", &s[..i]),
        _ => s,
    }
}

/// Date.now().
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// JSON.stringify of an optional value ("undefined" when absent).
pub fn json_stringify(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(v) => serde_json::to_string(v).unwrap_or_default(),
    }
}

/// encodeURIComponent.
pub fn encode_uri_component(s: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
    const SET: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'!')
        .remove(b'~')
        .remove(b'*')
        .remove(b'\'')
        .remove(b'(')
        .remove(b')');
    utf8_percent_encode(s, SET).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn metadata_value_matches_the_js_clamp() {
        // WHY: these strings land on signed manifest lines; a UTF-16 surrogate
        // pair is two "?" in JS and the clamp counts code units.
        assert_eq!(metadata_value(Some(&json!("a\u{e9}b\u{1F600}c")), 100), "a?b??c");
        assert_eq!(metadata_value(Some(&json!(42)), 10), "42");
        assert_eq!(metadata_value(None, 10), "");
        assert_eq!(metadata_value(Some(&json!("abcdef")), 3), "abc");
    }

    #[test]
    fn timestamps_render_like_to_iso_string() {
        assert_eq!(iso_from_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(compact_stamp("2026-09-28T01:02:03.456Z"), "20260928T010203Z");
    }
}
