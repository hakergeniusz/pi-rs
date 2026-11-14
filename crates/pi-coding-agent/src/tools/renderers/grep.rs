//! Presentation for the grep tool.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/renderers/grep.ts`.

use std::fmt::Write as _;

use pi_agent::types::AgentToolResult;
use pi_tui::theme::{Theme, ThemeColor};
use serde_json::Value;

use crate::tools::render_utils::{get_text_output, shorten_path, str};
use crate::tools::renderers::read::expand_more_lines;
use crate::tools::renderers::{CallContext, ResultContext};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, format_size};

/// pi's collapsed preview line count (`grep.ts:27`).
const GREP_PREVIEW_LINES: usize = 15;

/// pi's `formatGrepCall` (`grep.ts:17-35`): `grep /pattern/ in <path>` with
/// optional glob and limit suffixes.
fn format_grep_call(args: Option<&Value>, theme: &Theme) -> String {
    let pattern = str(args.and_then(|args| args.get("pattern")));
    let raw_path = str(args.and_then(|args| args.get("path")));
    let path = raw_path.map(|path| {
        if path.is_empty() {
            ".".to_string()
        } else {
            shorten_path(&path)
        }
    });
    let glob = str(args.and_then(|args| args.get("glob")));
    let limit = args
        .and_then(|args| args.get("limit"))
        .and_then(Value::as_u64);
    let invalid_arg = crate::tools::renderers::invalid_arg_text(theme);
    let mut text = format!(
        "{} ",
        theme.fg(ThemeColor::ToolTitle, "\x1b[1mgrep\x1b[22m")
    );
    match pattern {
        None => text.push_str(&invalid_arg),
        Some(pattern) => text.push_str(&theme.fg(ThemeColor::Accent, &format!("/{pattern}/"))),
    }
    let _ = write!(
        text,
        " {}",
        theme.fg(
            ThemeColor::ToolOutput,
            &format!(" in {}", path.unwrap_or(invalid_arg))
        )
    );
    if let Some(glob) = glob.filter(|glob| !glob.is_empty()) {
        let _ = write!(
            text,
            "{}",
            theme.fg(ThemeColor::ToolOutput, &format!(" ({glob})"))
        );
    }
    if let Some(limit) = limit {
        let _ = write!(
            text,
            "{}",
            theme.fg(ThemeColor::ToolOutput, &format!(" limit {limit}"))
        );
    }
    text
}

/// pi's `formatGrepResult` (`grep.ts:37-68`).
fn format_grep_result(result: &AgentToolResult, theme: &Theme, expanded: bool) -> String {
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

/// pi's `grepRenderers` (`grep.ts:71-82`).
pub const GREP_RENDERERS: crate::tools::renderers::ToolRenderers =
    crate::tools::renderers::ToolRenderers {
        render_call: |args, context: &CallContext| format_grep_call(args, context.theme),
        render_result: |_args, result: &AgentToolResult, context: &ResultContext| {
            format_grep_result(result, context.theme, context.expanded)
        },
    };
