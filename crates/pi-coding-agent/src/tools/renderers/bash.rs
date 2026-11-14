//! Presentation for the shell tools.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/renderers/bash.ts`.
//!
//! Shell renderers are shared by bash and powershell, which differ only in
//! the prompt they display (`createShellRenderers`). The collapsed preview
//! keeps the *last* five lines of output (`VisualLinePreview` keep:"end");
//! the full-output-path footer is trimmed from the preview when a final
//! truncated result would otherwise repeat it. Elapsed/Took timing lines
//! arrive through the result context's `timer_text`.

use std::fmt::Write as _;

use pi_agent::types::AgentToolResult;
use pi_tui::theme::{Theme, ThemeColor};
use serde_json::Value;

use crate::tools::render_utils::{get_text_output, str};
use crate::tools::renderers::{CallContext, ResultContext};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, format_size};

/// pi's `BASH_PREVIEW_LINES` (`bash.ts:18`).
pub const BASH_PREVIEW_LINES: usize = 5;
/// pi's `BASH_UPDATE_THROTTLE_MS` (`bash.ts:19`).
pub const BASH_UPDATE_THROTTLE_MS: u64 = 100;

/// `formatShellCall` (`bash.ts:20-26`).
fn format_shell_call(args: Option<&Value>, prompt: &str, theme: &Theme) -> String {
    let command = args.and_then(|args| str(args.get("command")));
    let timeout = args
        .and_then(|args| args.get("timeout"))
        .and_then(Value::as_u64);
    let timeout_suffix = timeout
        .map(|timeout| theme.fg(ThemeColor::Muted, &format!(" (timeout {timeout}s)")))
        .unwrap_or_default();
    let command_display = match command {
        None => crate::tools::renderers::invalid_arg_text(theme),
        Some(command) if command.is_empty() => theme.fg(ThemeColor::ToolOutput, "..."),
        Some(command) => command,
    };
    format!(
        "{}{}",
        theme.fg(
            ThemeColor::ToolTitle,
            &format!("\x1b[1m{prompt} {command_display}\x1b[22m")
        ),
        timeout_suffix
    )
}

/// `rebuildBashResultRenderComponent` (`bash.ts:27-90`) as styled text.
fn format_shell_result(
    result: &AgentToolResult,
    theme: &Theme,
    expanded: bool,
    timer_text: Option<String>,
) -> String {
    let mut text = String::new();
    let mut output = get_text_output(&result.content, false).trim().to_string();
    let truncation = result.details.get("truncation");
    let truncated = truncation
        .and_then(|value| value.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let full_output_path = result.details.get("fullOutputPath").and_then(Value::as_str);
    if truncated && full_output_path.is_some() && output.ends_with(']') {
        // Trim the "[Full output: ...]" footer the tool appended, so the
        // preview does not repeat it (`bash.ts:42-47`).
        if let Some(footer_start) = output.rfind("\n\n[") {
            if output[footer_start..].contains(full_output_path.unwrap_or_default()) {
                output = output[..footer_start].trim_end().to_string();
            }
        }
    }

    if !output.is_empty() {
        let styled = output
            .split('\n')
            .map(|line| theme.fg(ThemeColor::ToolOutput, line))
            .collect::<Vec<_>>()
            .join("\n");
        if expanded {
            let _ = write!(text, "\n{styled}");
        } else {
            // keep:"end" preview (`bash.ts:58-68`).
            let lines: Vec<&str> = styled.split('\n').collect();
            if lines.len() > BASH_PREVIEW_LINES {
                let hidden = lines.len() - BASH_PREVIEW_LINES;
                let _ = write!(
                    text,
                    "\n{} {}{}",
                    theme.fg(ThemeColor::Muted, &format!("... ({hidden} earlier lines,")),
                    theme.fg(ThemeColor::Dim, "ctrl+o to expand"),
                    theme.fg(ThemeColor::Muted, ")")
                );
                let _ = write!(text, "\n{}", lines[hidden..].join("\n"));
            } else {
                let _ = write!(text, "\n{styled}");
            }
        }
    }

    if truncated || full_output_path.is_some() {
        let mut warnings: Vec<String> = Vec::new();
        if let Some(path) = full_output_path {
            warnings.push(format!("Full output: {path}"));
        }
        if truncated {
            let truncated_by = truncation
                .and_then(|value| value.get("truncatedBy"))
                .and_then(Value::as_str);
            let max_bytes = truncation
                .and_then(|value| value.get("maxBytes"))
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_MAX_BYTES as u64);
            if truncated_by == Some("lines") {
                let output_lines = truncation
                    .and_then(|value| value.get("outputLines"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let total_lines = truncation
                    .and_then(|value| value.get("totalLines"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                warnings.push(format!(
                    "Truncated: showing {output_lines} of {total_lines} lines"
                ));
            } else {
                warnings.push(format!(
                    "Truncated: {} lines shown ({} limit)",
                    truncation
                        .and_then(|value| value.get("outputLines"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    format_size(max_bytes as usize)
                ));
            }
        }
        let _ = write!(
            text,
            "\n{}",
            theme.fg(ThemeColor::Warning, &format!("[{}]", warnings.join(". ")))
        );
    }

    if let Some(timer_text) = timer_text {
        let _ = write!(text, "\n{}", theme.fg(ThemeColor::Muted, &timer_text));
    }
    text
}

/// Shell renderers shared by bash and powershell (`bash.ts:93-109`). pi
/// threads an arbitrary prompt through a closure; the two prompts pi builds
/// are `$` for bash and `PS>` for the skipped powershell variant, so a fn
/// pointer pair covers both.
pub fn create_shell_renderers(prompt: &'static str) -> crate::tools::renderers::ToolRenderers {
    let render_call: fn(Option<&Value>, &CallContext) -> String = match prompt {
        "$" => |args, context: &CallContext| format_shell_call(args, "$", context.theme),
        _ => |args, context: &CallContext| format_shell_call(args, "PS>", context.theme),
    };
    crate::tools::renderers::ToolRenderers {
        render_call,
        render_result: |_args, result: &AgentToolResult, context: &ResultContext| {
            format_shell_result(
                result,
                context.theme,
                context.expanded,
                context.timer_text.clone(),
            )
        },
    }
}
