//! Byte-exact `JSON.stringify` compatible writer.
//!
//! pi writes every session record with `JSON.stringify`
//! (`core/session-manager.ts` `_persist`/`_rewriteFile`/`forkFrom`), so byte
//! compatibility means matching *V8's* formatting, not `serde_json`'s:
//!
//! * strings: only `"`, `\` and C0 controls are escaped (`\b \t \n \f \r`, else
//!   `\u00xx`); everything else, including DEL and non-ASCII, is emitted raw UTF-8
//! * numbers: integers print as integers; floats use the ECMAScript
//!   `Number::toString` algorithm — shortest round-trip digits, positional
//!   notation for `1e-7 <= |x| < 1e21`, exponential (`1e+21`, `1e-7`) outside,
//!   and no negative zero
//!
//! `serde_json`'s float formatting disagrees (it prints `1e21` where V8 prints
//! `1e+21`, and `3.0` where V8 prints `3`), so every record line goes through
//! [`to_string`] instead of `serde_json::to_string`. Parsing still uses
//! `serde_json`; without its `float_roundtrip` feature the fast float parser
//! can be off by one ULP for a few decimals, so an ultra-long decimal literal
//! can rewrite as its shortest form — a reported dependency request.

use std::fmt::Write as _;

use serde_json::Value;

/// `JSON.stringify(value)` for the value shapes `serde_json` produces.
pub fn to_string(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out);
    out
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&format_number(number)),
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(fields) => {
            out.push('{');
            for (index, (key, item)) in fields.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_value(item, out);
            }
            out.push('}');
        }
    }
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

fn format_number(number: &serde_json::Number) -> String {
    if let Some(value) = number.as_i64() {
        return value.to_string();
    }
    if let Some(value) = number.as_u64() {
        return value.to_string();
    }
    match number.as_f64() {
        Some(value) => format_f64(value),
        // Unreachable for values serde_json produced, but never panic.
        None => number.to_string(),
    }
}

/// ECMAScript `Number::toString` (radix 10) for a finite double.
///
/// `-0` serializes as `0` (as `JSON.stringify` does) and non-finite values as
/// `null`.
pub fn format_f64(value: f64) -> String {
    if !value.is_finite() {
        return "null".to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }
    let negative = value < 0.0;
    let magnitude = value.abs();

    // Rust's LowerExp gives the shortest round-trip digits in scientific form:
    // `d.ddddde<exp>`; from that we recover `digits` and `n`, where the value is
    // `0.<digits> * 10^n` (ECMAScript's `s * 10^(n-k)`).
    let scientific = format!("{magnitude:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("LowerExp always has an exponent");
    let exponent: i32 = exponent.parse().expect("LowerExp exponent is an integer");
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let k = digits.len() as i32;
    let n = exponent + 1;

    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if k <= n && n <= 21 {
        out.push_str(digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(digits);
    } else {
        let e = n - 1;
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if e >= 0 { '+' } else { '-' });
        out.push_str(&e.abs().to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_match_v8() {
        for (input, expected) in [
            ("0", "0"),
            ("1", "1"),
            ("-0.0", "0"),
            ("123456789012345678", "123456789012345678"),
            // The shortest-round-trip forms: `0.0002192` parses back to the
            // same double as the long decimal, so V8 prints the short one.
            ("0.0002192", "0.0002192"),
            ("0.0000025", "0.0000025"),
            ("1e21", "1e+21"),
            ("1e-7", "1e-7"),
            ("1.5e-7", "1.5e-7"),
            ("0.000001", "0.000001"),
            ("1e20", "100000000000000000000"),
            ("-1.25", "-1.25"),
            ("3.0", "3"),
            ("0.0", "0"),
        ] {
            let value: Value = serde_json::from_str(input).unwrap();
            assert_eq!(to_string(&value), expected, "input {input}");
        }
    }

    #[test]
    fn strings_match_v8_escaping() {
        let value = json!("a\"b\\c\nd\te\u{1}f\u{7f}gé\u{1f600}");
        assert_eq!(
            to_string(&value),
            "\"a\\\"b\\\\c\\nd\\te\\u0001f\u{7f}gé\u{1f600}\""
        );
    }

    #[test]
    fn objects_keep_insertion_order() {
        let value = json!({"type": "custom", "customType": "x", "id": "1", "parentId": null});
        assert_eq!(
            to_string(&value),
            r#"{"type":"custom","customType":"x","id":"1","parentId":null}"#
        );
    }
}
