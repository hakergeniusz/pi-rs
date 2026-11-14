//! Presentation for the find tool.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/renderers/find.ts`.

use std::fmt::Write as _;

use pi_agent::types::AgentToolResult;
use pi_tui::theme::{Theme, ThemeColor};
use serde_json::Value;

use crate::tools::render_utils::{get_text_output, shorten_path, str};
use crate::tools::renderers::read::expand_more_lines;
use crate::tools::renderers::{CallContext, ResultContext};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, format_size};

/// pi's collapsed preview line count (`find.ts:28`).
const FIND_PREVIEW_LINES: usize = 20;

/// pi's `formatFindCall` (`find.ts:17-32`): `find <pattern> in <path>` with
/// an optional `(limit N)` suffix.
fn format_find_call(args: Option<&Value>, theme: &Theme) -> String {
    let pattern = str(args.and_then(|args| args.get("pattern")));
    let raw_path = str(args.and_then(|args| args.get("path")));
    let path = raw_path.map(|path| {
        if path.is_empty() {
            ".".to_string()
        } else {
            shorten_path(&path)
        }
    });
    let limit = args
        .and_then(|args| args.get("limit"))
        .and_then(Value::as_u64);
    let invalid_arg = crate::tools::renderers::invalid_arg_text(theme);
    let mut text = format!(
        "{} ",
        theme.fg(ThemeColor::ToolTitle, "\x1b[1mfind\x1b[22m")
    );
    match pattern {
        None => text.push_str(&invalid_arg),
        Some(pattern) => text.push_str(&theme.fg(ThemeColor::Accent, &pattern)),
    }
    let _ = write!(
        text,
        " {}",
        theme.fg(
            ThemeColor::ToolOutput,
            &format!(" in {}", path.unwrap_or(invalid_arg))
        )
    );
    if let Some(limit) = limit {
        let _ = write!(
            text,
            "{}",
            theme.fg(ThemeColor::ToolOutput, &format!(" (limit {limit})"))
        );
    }
    text
}

/// pi's `formatFindResult` (`find.ts:34-63`).
fn format_find_result(result: &AgentToolResult, theme: &Theme, expanded: bool) -> String {
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

/// pi's `findRenderers` (`find.ts:66-77`).
pub const FIND_RENDERERS: crate::tools::renderers::ToolRenderers =
    crate::tools::renderers::ToolRenderers {
        render_call: |args, context: &CallContext| format_find_call(args, context.theme),
        render_result: |_args, result: &AgentToolResult, context: &ResultContext| {
            format_find_result(result, context.theme, context.expanded)
        },
    };
