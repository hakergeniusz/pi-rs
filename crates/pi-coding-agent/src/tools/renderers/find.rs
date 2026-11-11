//! Presentation for the find tool.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/renderers/find.ts`.

use pi_agent::types::AgentToolResult;
use serde_json::Value;

use crate::tools::render_utils::{get_text_output, str};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, format_size};
use std::fmt::Write as _;

/// pi's collapsed preview line count (`find.ts:28`).
const FIND_PREVIEW_LINES: usize = 20;

/// pi's `formatFindCall` (`find.ts:16-35`): `find <pattern> in <path>` with an
/// optional `(limit N)` suffix.
fn format_find_call(args: Option<&Value>, _cwd: &str) -> String {
    let pattern = str(args.and_then(|args| args.get("pattern")));
    let raw_path = str(args.and_then(|args| args.get("path")));
    let path = raw_path.map(|path| {
        if path.is_empty() {
            ".".to_string()
        } else {
            crate::tools::render_utils::shorten_path(&path)
        }
    });
    let limit = args
        .and_then(|args| args.get("limit"))
        .and_then(Value::as_f64);
    let mut text = String::from("**find** ");
    match pattern {
        None => text.push_str("[invalid arg]"),
        Some(pattern) => {
            let _ = write!(text, "`{pattern}`");
        }
    }
    match path {
        None => text.push_str(" in [invalid arg]"),
        Some(path) => {
            let _ = write!(text, " in {path}");
        }
    }
    if let Some(limit) = limit {
        let _ = write!(text, " (limit {limit})");
    }
    text
}

/// pi's `formatFindResult` (`find.ts:37-64`).
fn format_find_result(result: &AgentToolResult, expanded: bool) -> String {
    let output = get_text_output(&result.content, false).trim().to_string();
    let mut text = String::new();
    if !output.is_empty() {
        let lines: Vec<&str> = output.lines().collect();
        let max_lines = if expanded {
            lines.len()
        } else {
            FIND_PREVIEW_LINES
        };
        let display_lines = &lines[..max_lines.min(lines.len())];
        let remaining = lines.len() - display_lines.len();
        let _ = write!(text, "\n{}", display_lines.join("\n"));
        if remaining > 0 {
            let _ = write!(text, "\n... ({remaining} more lines, to expand)");
        }
    }

    let result_limit = result
        .details
        .get("resultLimitReached")
        .and_then(Value::as_u64);
    let truncation = result.details.get("truncation");
    let truncated = truncation
        .and_then(|value| value.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if result_limit.is_some() || truncated {
        let mut warnings: Vec<String> = Vec::new();
        if let Some(result_limit) = result_limit {
            warnings.push(format!("{result_limit} results limit"));
        }
        if truncated {
            let max_bytes = truncation
                .and_then(|value| value.get("maxBytes"))
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_MAX_BYTES as u64);
            warnings.push(format!("{} limit", format_size(max_bytes as usize)));
        }
        let _ = write!(text, "\n[Truncated: {}]", warnings.join(", "));
    }
    text
}

/// pi's `findRenderers` (`find.ts:67-76`).
pub const FIND_RENDERERS: crate::tools::renderers::ToolRenderers =
    crate::tools::renderers::ToolRenderers {
        render_call: format_find_call,
        render_result: format_find_result,
    };
