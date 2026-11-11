//! Presentation for the read tool.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/renderers/read.ts`.
//!
//! The compact-call classification (skill files, pi docs, resource files) and
//! the truncation notices are ported; syntax highlighting and theming are
//! phase P6.

use pi_agent::types::AgentToolResult;
use serde_json::Value;

use crate::tools::render_utils::{format_read_line_range, get_text_output, str};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size};
use std::fmt::Write as _;

/// pi's collapsed preview line count (`read.ts:151`).
const READ_PREVIEW_LINES: usize = 10;

/// pi's `ReadRenderArgs` (`read.ts:19`): `file_path` is the legacy spelling
/// some models send.
struct ReadRenderArgs<'a> {
    path: Option<&'a Value>,
    file_path: Option<&'a Value>,
    offset: Option<&'a Value>,
    limit: Option<&'a Value>,
}

impl<'a> ReadRenderArgs<'a> {
    fn from(args: Option<&'a Value>) -> ReadRenderArgs<'a> {
        ReadRenderArgs {
            path: args.and_then(|args| args.get("path")),
            file_path: args.and_then(|args| args.get("file_path")),
            offset: args.and_then(|args| args.get("offset")),
            limit: args.and_then(|args| args.get("limit")),
        }
    }

    /// pi's `str(args?.file_path ?? args?.path)`.
    fn path(&self) -> Option<String> {
        str(self.file_path.or(self.path))
    }
}

/// pi's `styledReadLineRange` (`read.ts:26-28`): the `:start-end` range a
/// read shows next to the path.
fn styled_read_line_range(args: &ReadRenderArgs<'_>) -> String {
    let range = format_read_line_range(args.offset, args.limit);
    if range.is_empty() {
        String::new()
    } else {
        format!(" `{range}`")
    }
}

/// pi's `formatReadCall` (`read.ts:29-31`): `read <path><range>`.
fn format_read_call(args: Option<&Value>, _cwd: &str) -> String {
    let args = ReadRenderArgs::from(args);
    let path_display = match args.path() {
        None => "[invalid arg]".to_string(),
        Some(path) if path.is_empty() => "...".to_string(),
        Some(path) => crate::tools::render_utils::shorten_path(&path),
    };
    format!("**read** {path_display}{}", styled_read_line_range(&args))
}

/// pi's `trimTrailingEmptyLines` (`read.ts:32-37`).
fn trim_trailing_empty_lines(lines: &[String]) -> &[String] {
    let mut end = lines.len();
    while end > 0 && lines[end - 1].is_empty() {
        end -= 1;
    }
    &lines[..end]
}

/// pi's `formatReadResult` (`read.ts:134-172`). The collapsed view renders
/// nothing: pi returns `""` when the row is not expanded and not an error.
fn format_read_result(result: &AgentToolResult, expanded: bool, is_error: bool) -> String {
    if !expanded && !is_error {
        return String::new();
    }

    let output = get_text_output(&result.content, false);
    let lines: Vec<String> = output
        .split('\n')
        .map(std::string::ToString::to_string)
        .collect();
    let lines = trim_trailing_empty_lines(&lines);
    let max_lines = if expanded {
        lines.len()
    } else {
        READ_PREVIEW_LINES
    };
    let display_lines = &lines[..max_lines.min(lines.len())];
    let remaining = lines.len() - display_lines.len();
    let mut text = format!("\n{}", display_lines.join("\n"));
    if remaining > 0 {
        let _ = write!(text, "\n... ({remaining} more lines, to expand)");
    }

    let truncation = result.details.get("truncation");
    let truncated = truncation
        .and_then(|value| value.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if truncated {
        let first_line_exceeds_limit = truncation
            .and_then(|value| value.get("firstLineExceedsLimit"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let truncated_by = truncation
            .and_then(|value| value.get("truncatedBy"))
            .and_then(Value::as_str);
        let max_bytes = truncation
            .and_then(|value| value.get("maxBytes"))
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_BYTES as u64);
        if first_line_exceeds_limit {
            let _ = write!(
                text,
                "\n[First line exceeds {} limit]",
                format_size(max_bytes as usize)
            );
        } else if truncated_by == Some("lines") {
            let output_lines = truncation
                .and_then(|value| value.get("outputLines"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let total_lines = truncation
                .and_then(|value| value.get("totalLines"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let _ = write!(
                text,
                "\n[Truncated: showing {output_lines} of {total_lines} lines ({DEFAULT_MAX_LINES} line limit)]"
            );
        } else {
            let output_lines = truncation
                .and_then(|value| value.get("outputLines"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let _ = write!(
                text,
                "\n[Truncated: {output_lines} lines shown ({} limit)]",
                format_size(max_bytes as usize)
            );
        }
    }
    text
}

/// pi's `readRenderers` (`read.ts:175-191`).
pub const READ_RENDERERS: crate::tools::renderers::ToolRenderers =
    crate::tools::renderers::ToolRenderers {
        render_call: format_read_call,
        render_result: |result, expanded| format_read_result(result, expanded, result.is_error),
    };
