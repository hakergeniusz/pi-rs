//! Presentation for the ls tool.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/renderers/ls.ts`.

use pi_agent::types::AgentToolResult;
use serde_json::Value;

use crate::tools::render_utils::{get_text_output, str};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, format_size};
use std::fmt::Write as _;

/// pi's collapsed preview line count (`ls.ts:23`).
const LS_PREVIEW_LINES: usize = 20;

/// pi's `formatLsCall` (`ls.ts:14-22`): `ls <path>` with an optional
/// `(limit N)` suffix.
fn format_ls_call(args: Option<&Value>, cwd: &str) -> String {
    let limit = args
        .and_then(|args| args.get("limit"))
        .and_then(Value::as_f64);
    let path_display = match str(args.and_then(|args| args.get("path"))) {
        None => "[invalid arg]".to_string(),
        Some(path) if path.is_empty() => ".".to_string(),
        Some(path) => crate::tools::render_utils::shorten_path(&path),
    };
    let mut text = format!("**ls** {path_display}");
    if let Some(limit) = limit {
        let _ = write!(text, " (limit {limit})");
    }
    let _ = cwd;
    text
}

/// pi's `formatLsResult` (`ls.ts:24-54`).
fn format_ls_result(result: &AgentToolResult, expanded: bool) -> String {
    let output = get_text_output(&result.content, false).trim().to_string();
    let mut text = String::new();
    if !output.is_empty() {
        let lines: Vec<&str> = output.lines().collect();
        let max_lines = if expanded {
            lines.len()
        } else {
            LS_PREVIEW_LINES
        };
        let display_lines = &lines[..max_lines.min(lines.len())];
        let remaining = lines.len() - display_lines.len();
        let _ = write!(text, "\n{}", display_lines.join("\n"));
        if remaining > 0 {
            let _ = write!(text, "\n... ({remaining} more lines, to expand)");
        }
    }

    let entry_limit = result
        .details
        .get("entryLimitReached")
        .and_then(Value::as_u64);
    let truncation = result.details.get("truncation");
    let truncated = truncation
        .and_then(|value| value.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if entry_limit.is_some() || truncated {
        let mut warnings: Vec<String> = Vec::new();
        if let Some(entry_limit) = entry_limit {
            warnings.push(format!("{entry_limit} entries limit"));
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

/// pi's `lsRenderers` (`ls.ts:57-70`).
pub const LS_RENDERERS: crate::tools::renderers::ToolRenderers =
    crate::tools::renderers::ToolRenderers {
        render_call: format_ls_call,
        render_result: format_ls_result,
    };
