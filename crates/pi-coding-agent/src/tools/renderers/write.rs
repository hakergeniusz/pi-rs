//! Presentation for the write tool.
//!
//! Ground truth: pi-mono
//! `packages/coding-agent/src/core/tools/renderers/write.ts`.
//!
//! The write call shows the file content; the result renderer shows only
//! errors. Syntax highlighting is phase P6.

use pi_agent::types::AgentToolResult;
use serde_json::Value;

use crate::tools::render_utils::{normalize_display_text, replace_tabs, str};
use std::fmt::Write as _;

/// pi's collapsed preview line count (`write.ts:150`).
const WRITE_PREVIEW_LINES: usize = 10;

/// pi's `formatWriteCall` (`write.ts:106-139`): `write <path>` followed by the
/// file content.
fn format_write_call(args: Option<&Value>, _cwd: &str, expanded: bool) -> String {
    let raw_path = str(args.and_then(|args| args.get("file_path").or_else(|| args.get("path"))));
    let file_content = str(args.and_then(|args| args.get("content")));
    let path_display = match raw_path {
        None => "...".to_string(),
        Some(path) if path.is_empty() => "...".to_string(),
        Some(path) => crate::tools::render_utils::shorten_path(&path),
    };
    let mut text = format!("**write** {path_display}");

    match file_content {
        None => text.push_str("\n\n[invalid content arg - expected string]"),
        Some(content) if content.is_empty() => {}
        Some(content) => {
            let normalized = replace_tabs(&normalize_display_text(&content));
            let lines: Vec<&str> = normalized.split('\n').collect();
            let total_lines = lines.len();
            let max_lines = if expanded {
                lines.len()
            } else {
                WRITE_PREVIEW_LINES
            };
            let display_lines = &lines[..max_lines.min(lines.len())];
            let remaining = lines.len() - display_lines.len();
            let _ = write!(text, "\n\n{}", display_lines.join("\n"));
            if remaining > 0 {
                let _ = write!(
                    text,
                    "\n... ({remaining} more lines, {total_lines} total, to expand)"
                );
            }
        }
    }
    text
}

/// pi's `formatWriteResult` (`write.ts:141-155`): only errors render, as the
/// output text.
fn format_write_result(result: &AgentToolResult, _expanded: bool) -> String {
    if !result.is_error {
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
    format!("\n{output}")
}

/// pi's `writeRenderers` (`write.ts:158-192`).
pub const WRITE_RENDERERS: crate::tools::renderers::ToolRenderers =
    crate::tools::renderers::ToolRenderers {
        render_call: |args, cwd| format_write_call(args, cwd, true),
        render_result: format_write_result,
    };
