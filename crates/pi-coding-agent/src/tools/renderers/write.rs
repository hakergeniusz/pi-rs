//! Presentation for the write tool.
//!
//! Ground truth: pi-mono
//! `packages/coding-agent/src/core/tools/renderers/write.ts`.
//!
//! The write call shows the file content; the result renderer shows only
//! errors. Upstream highlights the content per language; the port has no
//! syntax grammars yet, so content lines take the plain `toolOutput` color
//! (upstream's own no-language fallback path). The incremental
//! highlight-cache machinery exists only to reuse highlighting across
//! streaming deltas and does not affect the rendered bytes.

use std::fmt::Write as _;

use pi_agent::types::AgentToolResult;
use pi_tui::theme::{Theme, ThemeColor};
use serde_json::Value;

use crate::tools::render_utils::{normalize_display_text, replace_tabs, str};
use crate::tools::renderers::{CallContext, ResultContext};

/// pi's collapsed preview line count (`write.ts:39`).
const WRITE_PREVIEW_LINES: usize = 10;

/// `trimTrailingEmptyLines` (`write.ts:101-107`).
fn trim_trailing_empty_lines(lines: Vec<String>) -> Vec<String> {
    let mut lines = lines;
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// `formatWriteCall` (`write.ts:108-140`): `write <path>` followed by the
/// file content.
fn format_write_call(args: Option<&Value>, theme: &Theme, expanded: bool) -> String {
    let raw_path = str(args.and_then(|args| args.get("file_path").or_else(|| args.get("path"))));
    let file_content = str(args.and_then(|args| args.get("content")));
    let path_display = crate::tools::renderers::render_tool_path(raw_path.as_deref(), theme, None);
    let mut text = format!(
        "{} {path_display}",
        theme.fg(ThemeColor::ToolTitle, "\x1b[1mwrite\x1b[22m")
    );

    match file_content {
        None => {
            let _ = write!(
                text,
                "\n\n{}",
                theme.fg(ThemeColor::Error, "[invalid content arg - expected string]")
            );
        }
        Some(content) if content.is_empty() => {}
        Some(content) => {
            let normalized = replace_tabs(&normalize_display_text(&content));
            let lines = trim_trailing_empty_lines(
                normalized.split('\n').map(ToString::to_string).collect(),
            );
            let total_lines = lines.len();
            let max_lines = if expanded {
                lines.len()
            } else {
                WRITE_PREVIEW_LINES
            };
            let display_lines = &lines[..max_lines.min(lines.len())];
            let remaining = lines.len() - display_lines.len();
            let _ = write!(
                text,
                "\n\n{}",
                display_lines
                    .iter()
                    .map(|line| theme.fg(ThemeColor::ToolOutput, &replace_tabs(line)))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            if remaining > 0 {
                let _ = write!(
                    text,
                    "{} {}{}",
                    theme.fg(
                        ThemeColor::Muted,
                        &format!("\n... ({remaining} more lines, {total_lines} total,")
                    ),
                    theme.fg(ThemeColor::Dim, "ctrl+o to expand"),
                    theme.fg(ThemeColor::Muted, ")")
                );
            }
        }
    }
    text
}

/// `formatWriteResult` (`write.ts:141-156`): only errors render, as the
/// output text.
fn format_write_result(result: &AgentToolResult, theme: &Theme, is_error: bool) -> String {
    if !is_error {
        return String::new();
    }
    let output = result
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .collect::<Vec<_>>()
        .join("\n");
    if output.is_empty() {
        return String::new();
    }
    format!("\n{}", theme.fg(ThemeColor::Error, &output))
}

/// pi's `writeRenderers` (`write.ts:158-192`).
pub const WRITE_RENDERERS: crate::tools::renderers::ToolRenderers =
    crate::tools::renderers::ToolRenderers {
        render_call: |args, context: &CallContext| {
            format_write_call(args, context.theme, context.expanded)
        },
        render_result: |_args, result: &AgentToolResult, context: &ResultContext| {
            format_write_result(result, context.theme, context.is_error)
        },
    };
