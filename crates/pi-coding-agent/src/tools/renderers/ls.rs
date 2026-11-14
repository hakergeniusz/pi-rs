//! Presentation for the ls tool.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/renderers/ls.ts`.

use std::fmt::Write as _;

use pi_agent::types::AgentToolResult;
use pi_tui::theme::{Theme, ThemeColor};
use serde_json::Value;

use crate::tools::render_utils::{get_text_output, str};
use crate::tools::renderers::read::expand_more_lines;
use crate::tools::renderers::{CallContext, ResultContext, render_tool_path};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, format_size};

/// pi's collapsed preview line count (`ls.ts:23`).
const LS_PREVIEW_LINES: usize = 20;

/// pi's `formatLsCall` (`ls.ts:17-25`): `ls <path>` with an optional
/// `(limit N)` suffix.
fn format_ls_call(args: Option<&Value>, theme: &Theme, cwd: &str) -> String {
    let limit = args
        .and_then(|args| args.get("limit"))
        .and_then(Value::as_u64);
    let raw_path = str(args.and_then(|args| args.get("path")));
    let path_display = render_tool_path(raw_path.as_deref(), theme, Some("."));
    let mut text = format!(
        "{} {path_display}",
        theme.fg(ThemeColor::ToolTitle, "\x1b[1mls\x1b[22m")
    );
    if let Some(limit) = limit {
        let _ = write!(
            text,
            "{}",
            theme.fg(ThemeColor::ToolOutput, &format!(" (limit {limit})"))
        );
    }
    let _ = cwd;
    text
}

/// pi's `formatLsResult` (`ls.ts:27-56`).
fn format_ls_result(result: &AgentToolResult, theme: &Theme, expanded: bool) -> String {
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
        let _ = write!(
            text,
            "\n{}",
            display_lines
                .iter()
                .map(|line| theme.fg(ThemeColor::ToolOutput, line))
                .collect::<Vec<_>>()
                .join("\n")
        );
        if remaining > 0 {
            let _ = write!(text, "{}", expand_more_lines(theme, remaining));
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
        let _ = write!(
            text,
            "\n{}",
            theme.fg(
                ThemeColor::Warning,
                &format!("[Truncated: {}]", warnings.join(", "))
            )
        );
    }
    text
}

/// pi's `lsRenderers` (`ls.ts:59-70`).
pub const LS_RENDERERS: crate::tools::renderers::ToolRenderers =
    crate::tools::renderers::ToolRenderers {
        render_call: |args, context: &CallContext| format_ls_call(args, context.theme, context.cwd),
        render_result: |_args, result: &AgentToolResult, context: &ResultContext| {
            format_ls_result(result, context.theme, context.expanded)
        },
    };
