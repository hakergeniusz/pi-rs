//! Presentation for the grep tool.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/renderers/grep.ts`.

use pi_agent::types::AgentToolResult;
use serde_json::Value;

use crate::tools::render_utils::{get_text_output, str};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, format_size};
use std::fmt::Write as _;

/// pi's collapsed preview line count (`grep.ts:27`).
const GREP_PREVIEW_LINES: usize = 15;

/// pi's `formatGrepCall` (`grep.ts:17-34`): `grep /pattern/ in <path>` with
/// optional glob and limit suffixes.
fn format_grep_call(args: Option<&Value>, _cwd: &str) -> String {
    let pattern = str(args.and_then(|args| args.get("pattern")));
    let raw_path = str(args.and_then(|args| args.get("path")));
    let path = raw_path.map(|path| {
        if path.is_empty() {
            ".".to_string()
        } else {
            crate::tools::render_utils::shorten_path(&path)
        }
    });
    let glob = str(args.and_then(|args| args.get("glob")));
    let limit = args
        .and_then(|args| args.get("limit"))
        .and_then(Value::as_f64);
    let mut text = String::from("**grep** ");
    match pattern {
        None => text.push_str("[invalid arg]"),
        Some(pattern) => {
            let _ = write!(text, "`/{pattern}`/");
        }
    }
    match path {
        None => text.push_str(" in [invalid arg]"),
        Some(path) => {
            let _ = write!(text, " in {path}");
        }
    }
    if let Some(glob) = glob.filter(|glob| !glob.is_empty()) {
        let _ = write!(text, " ({glob})");
    }
    if let Some(limit) = limit {
        let _ = write!(text, " limit {limit}");
    }
    text
}

/// pi's `formatGrepResult` (`grep.ts:36-68`).
fn format_grep_result(result: &AgentToolResult, expanded: bool) -> String {
    let output = get_text_output(&result.content, false).trim().to_string();
    let mut text = String::new();
    if !output.is_empty() {
        let lines: Vec<&str> = output.lines().collect();
        let max_lines = if expanded {
            lines.len()
        } else {
            GREP_PREVIEW_LINES
        };
        let display_lines = &lines[..max_lines.min(lines.len())];
        let remaining = lines.len() - display_lines.len();
        let _ = write!(text, "\n{}", display_lines.join("\n"));
        if remaining > 0 {
            let _ = write!(text, "\n... ({remaining} more lines, to expand)");
        }
    }

    let match_limit = result
        .details
        .get("matchLimitReached")
        .and_then(Value::as_u64);
    let truncation = result.details.get("truncation");
    let truncated = truncation
        .and_then(|value| value.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let lines_truncated = result
        .details
        .get("linesTruncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if match_limit.is_some() || truncated || lines_truncated {
        let mut warnings: Vec<String> = Vec::new();
        if let Some(match_limit) = match_limit {
            warnings.push(format!("{match_limit} matches limit"));
        }
        if truncated {
            let max_bytes = truncation
                .and_then(|value| value.get("maxBytes"))
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_MAX_BYTES as u64);
            warnings.push(format!("{} limit", format_size(max_bytes as usize)));
        }
        if lines_truncated {
            warnings.push("some lines truncated".to_string());
        }
        let _ = write!(text, "\n[Truncated: {}]", warnings.join(", "));
    }
    text
}

/// pi's `grepRenderers` (`grep.ts:71-81`).
pub const GREP_RENDERERS: crate::tools::renderers::ToolRenderers =
    crate::tools::renderers::ToolRenderers {
        render_call: format_grep_call,
        render_result: format_grep_result,
    };
