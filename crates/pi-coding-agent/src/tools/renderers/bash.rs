//! Presentation for the shell tools.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/renderers/bash.ts`.
//!
//! Shell renderers are shared by bash and powershell, which differ only in the
//! prompt they display (`bash.ts:73-75`). The interactive component (a
//! `Container` with a visual-line preview and a timer) is phase P6; what is
//! ported here is the *shape*: the call line, the result output with the
//! truncation footer stripped when a final result carries a full-output path,
//! and the warning line.

use pi_agent::types::AgentToolResult;
use serde_json::Value;

use crate::tools::render_utils::{get_text_output, str};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, format_size};
use std::fmt::Write as _;

/// pi's `BASH_PREVIEW_LINES` (`bash.ts:7`).
pub const BASH_PREVIEW_LINES: usize = 5;
/// pi's `BASH_UPDATE_THROTTLE_MS` (`bash.ts:8`).
pub const BASH_UPDATE_THROTTLE_MS: u64 = 100;

/// pi's `formatShellCall` (`bash.ts:11-17`): `${prompt} ${command}`, with the
/// timeout suffix. Rendered as markdown, so the command is a fenced inline
/// code span.
fn format_shell_call(args: Option<&Value>, prompt: &str) -> String {
    let command = str(args.and_then(|args| args.get("command")));
    let timeout = args
        .and_then(|args| args.get("timeout"))
        .and_then(Value::as_f64);
    let timeout_suffix = match timeout {
        Some(timeout) => format!(" (timeout {timeout}s)"),
        None => String::new(),
    };
    match command {
        None => format!("**{prompt} [invalid arg]**{timeout_suffix}"),
        Some(command) if command.is_empty() => format!("**{prompt} ...**{timeout_suffix}"),
        Some(command) => format!("**{prompt} `{command}`**{timeout_suffix}"),
    }
}

/// pi's `rebuildBashResultRenderComponent` (`bash.ts:20-69`), as markdown.
///
/// When a final result is truncated and carries a full-output path, the
/// footer the tool appended is stripped from the output and re-stated as a
/// warning line.
fn format_shell_result(result: &AgentToolResult, expanded: bool) -> String {
    let mut output = get_text_output(&result.content, false).trim().to_string();
    let truncation = result.details.get("truncation");
    let truncated = truncation
        .and_then(|value| value.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let full_output_path = result.details.get("fullOutputPath").and_then(Value::as_str);

    if !truncated && full_output_path.is_some() && output.ends_with(']') {
        if let Some(footer_start) = output.rfind("\n\n[") {
            if output[footer_start..].contains(full_output_path.unwrap_or("")) {
                output = output[..footer_start].trim_end().to_string();
            }
        }
    }

    let mut text = String::new();
    if !output.is_empty() {
        if expanded {
            let _ = write!(text, "\n{output}");
        } else {
            // The collapsed preview keeps the last BASH_PREVIEW_LINES lines.
            let lines: Vec<&str> = output.lines().collect();
            if lines.len() > BASH_PREVIEW_LINES {
                let hidden = lines.len() - BASH_PREVIEW_LINES;
                let _ = write!(
                    text,
                    "\n... ({hidden} earlier lines, to expand)\n{}",
                    lines[hidden..].join("\n")
                );
            } else {
                let _ = write!(text, "\n{output}");
            }
        }
    }

    if truncated || full_output_path.is_some() {
        let mut warnings: Vec<String> = Vec::new();
        if let Some(path) = full_output_path {
            warnings.push(format!("Full output: {path}"));
        }
        if truncated {
            let total_lines = truncation
                .and_then(|value| value.get("totalLines"))
                .and_then(Value::as_u64);
            let output_lines = truncation
                .and_then(|value| value.get("outputLines"))
                .and_then(Value::as_u64);
            let truncated_by = truncation
                .and_then(|value| value.get("truncatedBy"))
                .and_then(Value::as_str);
            let max_bytes = truncation
                .and_then(|value| value.get("maxBytes"))
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_MAX_BYTES as u64);
            match (truncated_by, output_lines, total_lines) {
                (Some("lines"), Some(shown), Some(total)) => {
                    warnings.push(format!("Truncated: showing {shown} of {total} lines"));
                }
                _ => warnings.push(format!(
                    "Truncated: {} lines shown ({} limit)",
                    output_lines.unwrap_or(0),
                    format_size(max_bytes as usize)
                )),
            }
        }
        let _ = write!(text, "\n[{}]", warnings.join(". "));
    }
    text
}

/// pi's `createShellRenderers("$")`.
fn bash_call(args: Option<&Value>, _cwd: &str) -> String {
    format_shell_call(args, "$")
}

/// pi's `createShellRenderers("PS>")` for the skipped Windows variant.
fn powershell_call(args: Option<&Value>, _cwd: &str) -> String {
    format_shell_call(args, "PS>")
}

/// pi's `createShellRenderers(prompt)` (`bash.ts:72-75`). pi threads an
/// arbitrary prompt through a closure; the two prompts pi builds are `$` for
/// bash and `PS>` for the skipped powershell variant, so a fn pointer pair
/// covers both.
pub fn create_shell_renderers(prompt: &'static str) -> crate::tools::renderers::ToolRenderers {
    let render_call: fn(Option<&Value>, &str) -> String = match prompt {
        "$" => bash_call,
        _ => powershell_call,
    };
    crate::tools::renderers::ToolRenderers {
        render_call,
        render_result: format_shell_result,
    }
}
