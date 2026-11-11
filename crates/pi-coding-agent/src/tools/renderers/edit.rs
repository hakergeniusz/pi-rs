//! Presentation for the edit tool.
//!
//! Ground truth: pi-mono
//! `packages/coding-agent/src/core/tools/renderers/edit.ts`.
//!
//! The interactive component renders a live diff preview while the model
//! streams the arguments and a background-colored header; that is phase P6.
//! What is ported here is the shape: the call line `edit <path>`, the diff
//! body (the display diff from the result details, prefixed per line), and
//! the error text.

use pi_agent::types::AgentToolResult;
use serde_json::Value;

use crate::tools::render_utils::str;

/// pi's `formatEditCall` (`edit.ts:82-85`): `edit <path>`.
fn format_edit_call(args: Option<&Value>, _cwd: &str) -> String {
    let path_display =
        match str(args.and_then(|args| args.get("file_path").or_else(|| args.get("path")))) {
            None => "...".to_string(),
            Some(path) if path.is_empty() => "...".to_string(),
            Some(path) => crate::tools::render_utils::shorten_path(&path),
        };
    format!("**edit** {path_display}")
}

/// pi's `formatEditResult` (`edit.ts:87-113`): the result diff as a fenced
/// diff block, or the error text.
fn format_edit_result(result: &AgentToolResult, expanded: bool) -> String {
    let _ = expanded;
    let is_error = result.is_error;
    let result_diff = if is_error {
        None
    } else {
        result.details.get("diff").and_then(Value::as_str)
    };
    if is_error {
        let error_text = result
            .content
            .iter()
            .filter_map(|block| block.as_text())
            .collect::<Vec<_>>()
            .join("\n");
        if error_text.is_empty() {
            return String::new();
        }
        return format!("\n{error_text}");
    }
    match result_diff {
        Some(diff) if !diff.is_empty() => format!("\n```diff\n{diff}\n```"),
        _ => String::new(),
    }
}

/// pi's `editRenderers` (`edit.ts:116-252`).
pub const EDIT_RENDERERS: crate::tools::renderers::ToolRenderers =
    crate::tools::renderers::ToolRenderers {
        render_call: format_edit_call,
        render_result: format_edit_result,
    };
