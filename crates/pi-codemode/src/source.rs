//! Codemode source format: JavaScript with an optional options line.
//!
//! Ground truth: `packages/codemode/src/source.ts:1-125`. Ports
//! `parseCodemodeSource`, `CodemodeSourceError`, the supported `// @options:`
//! fields, and the Lark grammar constant used for grammar-constrained
//! sampling. The only adaptation is that the error carries a Rust `String`
//! message instead of being an `Error` subclass.

use thiserror::Error;

/// Prefix of the optional first-line options directive (`source.ts:11`).
pub const CODEMODE_OPTIONS_PREFIX: &str = "// @options:";

/// Lark grammar for providers with grammar-constrained tool input
/// (`source.ts:18-30`). Only the options-line shape is fixed; the JSON and
/// code are checked by [`parse_codemode_source`].
pub const CODEMODE_SOURCE_GRAMMAR: &str = r"
start: options_source | plain_source
options_source: OPTIONS_LINE NEWLINE SOURCE
plain_source: SOURCE

OPTIONS_LINE: /[ \t]*\/\/ @options:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
";

const SUPPORTED_FIELDS_TEXT: &str = "`max_output_tokens` and `timeout_ms`";
/// Largest delay `setTimeout` supports, which bounds `timeout_ms`.
const MAX_TIMEOUT_MS: u64 = 2_147_483_647;

/// Options carried by the first-line directive (`source.ts:32-37`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodemodeSourceOptions {
    /// Token budget for the script's output.
    pub max_output_tokens: Option<u64>,
    /// Hard deadline for the whole script, including tool calls.
    pub timeout_ms: Option<u64>,
}

/// A parsed source: the code plus the options it declared (`source.ts:39-43`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCodemodeSource {
    /// The script with the options line replaced by an empty line, so reported
    /// line numbers stay unchanged.
    pub code: String,
    /// Parsed options.
    pub options: CodemodeSourceOptions,
}

/// Error for empty input, invalid options, or a misplaced options line
/// (`source.ts:45-50`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0}")]
pub struct CodemodeSourceError(String);

impl CodemodeSourceError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

fn is_safe_integer(value: &serde_json::Value) -> Option<u64> {
    value.as_u64()
}

fn parse_options(directive: &str) -> Result<CodemodeSourceOptions, CodemodeSourceError> {
    if directive.is_empty() {
        return Err(CodemodeSourceError::new(format!(
            "@options must be a JSON object with supported fields {SUPPORTED_FIELDS_TEXT}"
        )));
    }
    let value: serde_json::Value = serde_json::from_str(directive).map_err(|error| {
        CodemodeSourceError::new(format!(
            "@options must be valid JSON with supported fields {SUPPORTED_FIELDS_TEXT}: {error}"
        ))
    })?;
    let fields = value.as_object().ok_or_else(|| {
        CodemodeSourceError::new(format!(
            "@options must be a JSON object with supported fields {SUPPORTED_FIELDS_TEXT}"
        ))
    })?;
    for key in fields.keys() {
        if key != "max_output_tokens" && key != "timeout_ms" {
            return Err(CodemodeSourceError::new(format!(
                "@options only supports {SUPPORTED_FIELDS_TEXT}; got `{key}`"
            )));
        }
    }
    let mut options = CodemodeSourceOptions::default();
    if let Some(value) = fields.get("max_output_tokens") {
        let parsed = is_safe_integer(value).ok_or_else(|| {
            CodemodeSourceError::new(
                "@options field `max_output_tokens` must be a non-negative safe integer",
            )
        })?;
        options.max_output_tokens = Some(parsed);
    }
    if let Some(value) = fields.get("timeout_ms") {
        let parsed = is_safe_integer(value)
            .filter(|number| *number > 0 && *number <= MAX_TIMEOUT_MS)
            .ok_or_else(|| {
                CodemodeSourceError::new(format!(
                    "@options field `timeout_ms` must be a positive integer up to {MAX_TIMEOUT_MS}"
                ))
            })?;
        options.timeout_ms = Some(parsed);
    }
    Ok(options)
}

/// Split an optional first-line `// @options: {...}` from the script
/// (`source.ts:96-125`).
///
/// An options line that is not the first line is rejected rather than
/// silently ignored. On success the directive line becomes an empty line so
/// JavaScript line numbers match the original input.
pub fn parse_codemode_source(input: &str) -> Result<ParsedCodemodeSource, CodemodeSourceError> {
    if input.trim().is_empty() {
        return Err(CodemodeSourceError::new(
            "Expected JavaScript source text (non-empty). Provide JS only, optionally with a first line `// @options: {\"max_output_tokens\": 1000}`.",
        ));
    }
    let lines: Vec<&str> = input.split('\n').collect();
    if let Some(index) = lines
        .iter()
        .enumerate()
        .find(|(index, line)| *index > 0 && line.trim_start().starts_with(CODEMODE_OPTIONS_PREFIX))
        .map(|(index, _)| index)
    {
        return Err(CodemodeSourceError::new(format!(
            "The {CODEMODE_OPTIONS_PREFIX} line must be the first line of the input; found one on line {}",
            index + 1
        )));
    }
    let newline = input.find('\n');
    let first_line = lines[0].strip_suffix('\r').unwrap_or(lines[0]);
    let trimmed = first_line.trim_start();
    if !trimmed.starts_with(CODEMODE_OPTIONS_PREFIX) {
        return Ok(ParsedCodemodeSource {
            code: input.to_string(),
            options: CodemodeSourceOptions::default(),
        });
    }
    let code = match newline {
        Some(index) => input[index..].to_string(),
        None => String::new(),
    };
    if code.trim().is_empty() {
        return Err(CodemodeSourceError::new(
            "The @options line must be followed by JavaScript source on subsequent lines",
        ));
    }
    let directive = trimmed[CODEMODE_OPTIONS_PREFIX.len()..].trim();
    Ok(ParsedCodemodeSource {
        code,
        options: parse_options(directive)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_source_is_returned_unchanged() {
        let parsed = parse_codemode_source("return 1 + 1;").expect("parse");
        assert_eq!(parsed.code, "return 1 + 1;");
        assert_eq!(parsed.options, CodemodeSourceOptions::default());
    }

    #[test]
    fn options_line_is_replaced_by_an_empty_line() {
        let input =
            "// @options: {\"max_output_tokens\": 2000, \"timeout_ms\": 30000}\ntext('hi');";
        let parsed = parse_codemode_source(input).expect("parse");
        assert_eq!(parsed.code, "\ntext('hi');");
        assert_eq!(parsed.options.max_output_tokens, Some(2000));
        assert_eq!(parsed.options.timeout_ms, Some(30_000));
    }

    #[test]
    fn crlf_options_line_is_accepted() {
        let input = "  // @options: {\"timeout_ms\": 5}\r\nreturn 1;";
        let parsed = parse_codemode_source(input).expect("parse");
        assert_eq!(parsed.code, "\nreturn 1;");
        assert_eq!(parsed.options.timeout_ms, Some(5));
    }

    #[test]
    fn rejects_empty_input() {
        assert!(parse_codemode_source("   \n  ").is_err());
    }

    #[test]
    fn rejects_misplaced_options_line() {
        let error = parse_codemode_source("text('a');\n// @options: {}").expect_err("must fail");
        assert!(error.to_string().contains("line 2"));
    }

    #[test]
    fn rejects_unknown_and_invalid_options() {
        assert!(parse_codemode_source("// @options: {\"foo\": 1}\nreturn 1;").is_err());
        assert!(parse_codemode_source("// @options: {\"timeout_ms\": 0}\nreturn 1;").is_err());
        assert!(parse_codemode_source("// @options: {bad}\nreturn 1;").is_err());
        assert!(parse_codemode_source("// @options:\nreturn 1;").is_err());
        assert!(parse_codemode_source("// @options: {}\n   ").is_err());
    }

    #[test]
    fn options_null_fields_are_rejected() {
        assert!(parse_codemode_source("// @options: {\"timeout_ms\": null}\nreturn 1;").is_err());
        assert!(
            parse_codemode_source("// @options: {\"max_output_tokens\": null}\nreturn 1;").is_err()
        );
    }
}
