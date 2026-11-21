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
//! [`to_string`] instead of `serde_json::to_string`.
//!
//! # Two halves of float byte-fidelity
//!
//! A load→serialize cycle must agree with pi's `JSON.parse` → `JSON.stringify`
//! on floating point tokens. That requires *both* halves to match V8:
//!
//! 1. **Parse to the same double.** The workspace enables `serde_json`'s
//!    `float_roundtrip` feature, which routes long decimals through a correctly
//!    rounded parser. The default fast path (`significand as f64`, then one
//!    multiply/divide by a power of ten) can land one ULP away from V8 for
//!    ~17-digit literals — e.g. `51.248178375505404` used to load as
//!    `51.24817837550541`, which then rewrites as a different token. This was
//!    the deferred P4 dependency request; no raw-token passthrough is needed
//!    because correct parsing plus V8 formatting already reproduces V8 exactly,
//!    *including* canonicalizing a non-V8 spelling the way `JSON.stringify`
//!    does.
//! 2. **Format with V8's `Number::toString` rules.** [`format_f64`] recovers the
//!    shortest round-tripping digits from Rust's `{:e}` formatter and re-applies
//!    ECMAScript's notation thresholds. V8 and Rust both emit the shortest
//!    decimal that round-trips to the same double, so a V8-written token is
//!    reproduced byte-for-byte (see the golden-fixture regression test).

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
/// `null`. Rust's `{:e}` and V8's `DoubleToStringConverter` both produce the
/// shortest round-tripping digits for a given double, so only the notation
/// thresholds below differ from `ryu`/`serde_json`'s defaults.
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

    /// A load→serialize cycle through the canonical [`crate::session::Record`]
    /// path must produce V8's token for every float shape. The first five
    /// literals are `serde_json`'s own `float_roundtrip` regression samples:
    /// the default parser lands one ULP away on each, so the feature is what
    /// makes these assertions hold. The long decimals pin shortest-round-trip
    /// canonicalization (>17 digits) and the exponent forms pin ECMAScript's
    /// explicit `+` and unpadded exponent spelling.
    #[test]
    fn exotic_float_literals_follow_v8_number_formatting() {
        for (literal, expected) in [
            // 1-ULP samples from serde_json's `test_roundtrip_f64`.
            ("51.248178375505404", "51.248178375505404"),
            ("-93.31137037688033", "-93.31137037688033"),
            ("-36.573994842753436", "-36.573994842753436"),
            ("52.314008204106244", "52.314008204106244"),
            ("97.45365320034685", "97.45365320034685"),
            // Finite samples from random IEEE-754 bit patterns.
            ("2.0030397744267762e-253", "2.0030397744267762e-253"),
            ("7.101215824554616e260", "7.101215824554616e+260"),
            ("1.769268377902049e74", "1.769268377902049e+74"),
            ("-1.6727517818542075e58", "-1.6727517818542075e+58"),
            ("3.9287532173373315e299", "3.9287532173373315e+299"),
            // >17 significant digits: exact decimal expansions collapse to the
            // same shortest token V8 would write.
            (
                "0.1000000000000000055511151231257827021181583404541015625",
                "0.1",
            ),
            ("51.24817837550540499", "51.248178375505404"),
            (
                "1.00000000000000011102230246251565404236316680908203125",
                "1",
            ),
            // ECMAScript notation thresholds and exponent spelling.
            ("1e21", "1e+21"),
            ("1e-7", "1e-7"),
        ] {
            let line = format!(r#"{{"type":"custom","value":{literal}}}"#);
            let record = crate::session::Record::parse_line_strict(&line).unwrap();
            assert_eq!(
                record.to_line(),
                format!(r#"{{"type":"custom","value":{expected}}}"#),
                "literal {literal} did not follow V8 formatting"
            );
        }
    }

    /// Regression for the deferred P4 finding: enabling `float_roundtrip`
    /// must not disturb the 38 byte-exact lines generated by the real TS pi.
    #[test]
    fn golden_fixture_lines_round_trip_byte_exactly() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/golden");
        let fixtures = [
            "01-plain-user-assistant.jsonl",
            "02-tool-read-round-trip.jsonl",
            "03-model-change-thinking-low.jsonl",
            "04-multi-turn-with-name.jsonl",
            "05-resume-thinking-level-change.jsonl",
        ];
        let mut checked = 0usize;
        for name in fixtures {
            let path = std::path::Path::new(dir).join(name);
            let text = std::fs::read_to_string(path).unwrap();
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let record = crate::session::Record::parse_line_strict(line).unwrap();
                assert_eq!(
                    record.to_line(),
                    line,
                    "fixture {name} changed a line on the raw Record path"
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 38, "all lines of the five golden fixtures");
    }
}
