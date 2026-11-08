//! Tolerant JSON parsing for streamed tool-call arguments (P1 subset of TS
//! `utils/json-parse.ts` / `parseStreamingJson`).
//!
//! TS implements a full incremental tolerant parser with snapshots. For P1 we
//! accumulate raw argument text and parse leniently on demand: valid JSON
//! parses normally; common stream truncations (unterminated strings and
//! containers) are repaired by closing what is open. Malformed arguments
//! surface as `arguments_error` on the finished tool call, never as panics.

/// Attempt a strict parse first, then light repair of truncated JSON.
pub fn parse_tool_call_arguments(
    text: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    if text.trim().is_empty() {
        return Ok(serde_json::Map::new());
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        return match value {
            serde_json::Value::Object(map) => Ok(map),
            other => Ok(serde_json::Map::from_iter([("value".to_string(), other)])),
        };
    }
    match repair_truncated(text) {
        Some(value) => match value {
            serde_json::Value::Object(map) => Ok(map),
            other => Ok(serde_json::Map::from_iter([("value".to_string(), other)])),
        },
        None => Err(format!("Invalid JSON in tool call arguments: {text}")),
    }
}

/// Close unterminated strings/containers so a truncated stream prefix parses.
/// Returns `None` when the text cannot be repaired into valid JSON.
fn repair_truncated(text: &str) -> Option<serde_json::Value> {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for ch in text.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' | '[' => stack.push(ch),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    let mut repaired = text.to_string();
    // A dangling escape or backslash would corrupt the closing quote.
    if escaped {
        repaired.push('\\');
    }
    if in_string {
        repaired.push('"');
    }
    for open in stack.iter().rev() {
        repaired.push(match open {
            '{' => '}',
            _ => ']',
        });
    }
    // Remove commas repair exposed before closing brackets (e.g. `{"a":1,` → `{"a":1}`).
    let repaired = repaired.replace(",}", "}").replace(",]", "]");
    serde_json::from_str(&repaired).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_json() {
        let args = parse_tool_call_arguments(r#"{"path":"a.txt","limit":10}"#).unwrap();
        assert_eq!(args.get("path").and_then(|v| v.as_str()), Some("a.txt"));
    }

    #[test]
    fn repairs_truncated_string() {
        let args = parse_tool_call_arguments(r#"{"path":"src/ma"#).unwrap();
        assert_eq!(args.get("path").and_then(|v| v.as_str()), Some("src/ma"));
    }

    #[test]
    fn repairs_truncated_container() {
        let args = parse_tool_call_arguments(r#"{"items":[1,2"#).unwrap();
        assert_eq!(args["items"].to_string(), "[1,2]");
    }

    #[test]
    fn repairs_trailing_comma() {
        let args = parse_tool_call_arguments(r#"{"a":1,"#).unwrap();
        assert_eq!(args["a"].to_string(), "1");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_tool_call_arguments("not json at all }{").is_err());
    }

    #[test]
    fn empty_is_empty_object() {
        assert!(parse_tool_call_arguments("  ").unwrap().is_empty());
    }
}
