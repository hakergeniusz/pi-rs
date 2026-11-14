//! Presentation for the read tool.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/renderers/read.ts`.
//!
//! The compact-call classification (skill files, resource files) and the
//! truncation notices are ported. Upstream's pi-docs classification keys off
//! the npm package's own README/docs tree; this port has no such tree, so
//! that arm is absent. Syntax highlighting is deferred (no grammars yet) —
//! expanded results show plain `toolOutput` lines.

use std::fmt::Write as _;

use pi_agent::types::AgentToolResult;
use pi_tui::theme::{Theme, ThemeColor};
use serde_json::Value;

use crate::tools::render_utils::{
    format_read_line_range, get_text_output, replace_tabs, shorten_path, str,
};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size};

/// pi's collapsed preview line count (`read.ts:151`).
const READ_PREVIEW_LINES: usize = 10;

/// Resource files the compact read call labels instead of the generic form
/// (`COMPACT_RESOURCE_FILE_NAMES`, `read.ts:26`).
const RESOURCE_FILES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

/// pi's `ReadRenderArgs` (`read.ts:27`): `file_path` is the legacy spelling
/// some models send.
struct ReadRenderArgs<'a> {
    path: Option<&'a Value>,
    file_path: Option<&'a Value>,
    offset: Option<&'a Value>,
    limit: Option<&'a Value>,
}

impl<'a> ReadRenderArgs<'a> {
    fn from(args: Option<&'a Value>) -> ReadRenderArgs<'a> {
        ReadRenderArgs {
            path: args.and_then(|args| args.get("path")),
            file_path: args.and_then(|args| args.get("file_path")),
            offset: args.and_then(|args| args.get("offset")),
            limit: args.and_then(|args| args.get("limit")),
        }
    }

    /// pi's `str(args?.file_path ?? args?.path)`.
    fn path(&self) -> Option<String> {
        str(self.file_path.or(self.path))
    }
}

/// The `... (N more lines, <key> to expand)` suffix shared by the grep/find/ls
/// renderers too (`read.ts:136`, `grep.ts:54`, `find.ts:51`, `ls.ts:44`).
pub(crate) fn expand_more_lines(theme: &Theme, remaining: usize) -> String {
    format!(
        "{} {}{}",
        theme.fg(
            ThemeColor::Muted,
            &format!("\n... ({remaining} more lines,")
        ),
        theme.fg(ThemeColor::Dim, "ctrl+o to expand"),
        theme.fg(ThemeColor::Muted, ")")
    )
}

/// `trimTrailingEmptyLines` (`read.ts:36-42`).
pub(crate) fn trim_trailing_empty_lines(lines: Vec<String>) -> Vec<String> {
    let mut lines = lines;
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// `styledReadLineRange` (`read.ts:28-31`): the `:start-end` range, warning
/// colored.
fn styled_read_line_range(args: &ReadRenderArgs<'_>, theme: &Theme) -> String {
    let range = format_read_line_range(args.offset, args.limit);
    if range.is_empty() {
        String::new()
    } else {
        theme.fg(ThemeColor::Warning, &range)
    }
}

/// `formatReadCall` (`read.ts:32-35`): `read <path><range>`.
fn format_read_call(args: &ReadRenderArgs<'_>, theme: &Theme) -> String {
    let path_display = match args.path() {
        None => "...".to_string(),
        Some(path) if path.is_empty() => "...".to_string(),
        Some(path) => shorten_path(&path),
    };
    format!(
        "{} {}{}",
        theme.fg(ThemeColor::ToolTitle, "\x1b[1mread\x1b[22m"),
        theme.fg(ThemeColor::Accent, &path_display),
        styled_read_line_range(args, theme)
    )
}

/// The compact classification (`CompactReadClassification`, `read.ts:22-25`).
struct CompactReadClassification {
    label: String,
    skill: bool,
}

/// `formatPathRelativeToCwdOrAbsolute` (`utils/paths.ts`): the path relative
/// to the cwd when inside it, otherwise absolute.
fn relative_to_cwd_or_absolute(absolute: &str, cwd: &str) -> String {
    let cwd_path = std::path::Path::new(cwd);
    match std::path::Path::new(absolute).strip_prefix(cwd_path) {
        Ok(relative) if relative.as_os_str().is_empty() => ".".to_string(),
        Ok(relative) => relative.to_string_lossy().to_string(),
        Err(_) => absolute.to_string(),
    }
}

/// `getCompactReadClassification` (`read.ts:64-85`) without the pi-docs arm.
fn get_compact_read_classification(
    args: &ReadRenderArgs<'_>,
    cwd: &str,
) -> Option<CompactReadClassification> {
    let raw_path = args.path()?;
    let absolute_path =
        crate::tools::path_utils::resolve_to_cwd(&raw_path, std::path::Path::new(cwd));
    let file_name = absolute_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    if file_name == "SKILL.md" {
        let label = absolute_path
            .parent()
            .and_then(|parent| parent.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or(file_name);
        return Some(CompactReadClassification { label, skill: true });
    }
    if RESOURCE_FILES.contains(&file_name.as_str()) {
        return Some(CompactReadClassification {
            label: relative_to_cwd_or_absolute(&absolute_path.to_string_lossy(), cwd),
            skill: false,
        });
    }
    None
}

/// `formatCompactReadCall` (`read.ts:86-108`).
fn format_compact_read_call(
    classification: &CompactReadClassification,
    args: &ReadRenderArgs<'_>,
    theme: &Theme,
) -> String {
    let expand_hint = format!(" {}", theme.fg(ThemeColor::Dim, "(ctrl+o to expand)"));
    let range = styled_read_line_range(args, theme);
    if classification.skill {
        format!(
            "{} {}{}{}",
            theme.fg(ThemeColor::CustomMessageLabel, "\x1b[1m[skill]\x1b[22m "),
            theme.fg(ThemeColor::CustomMessageText, &classification.label),
            range,
            expand_hint
        )
    } else {
        format!(
            "{} {}{}{}",
            theme.fg(ThemeColor::ToolTitle, "\x1b[1mread resource\x1b[22m"),
            theme.fg(ThemeColor::Accent, &classification.label),
            range,
            expand_hint
        )
    }
}

/// `formatReadResult` (`read.ts:109-150`). The collapsed view renders the
/// image-model warning only: pi returns "" when not expanded and not an
/// error.
fn format_read_result(
    _args: &ReadRenderArgs<'_>,
    result: &AgentToolResult,
    theme: &Theme,
    expanded: bool,
    is_error: bool,
) -> String {
    let unsupported_by = result
        .details
        .get("imageUnsupportedByModel")
        .and_then(Value::as_str);
    let image_warning = unsupported_by
        .map(|by| {
            format!(
                "\n{}",
                theme.fg(
                    ThemeColor::Warning,
                    &format!("[{by} does not support images; it saw a placeholder instead]")
                )
            )
        })
        .unwrap_or_default();
    if !expanded && !is_error {
        return image_warning;
    }

    let output = get_text_output(&result.content, false);
    let lines: Vec<String> = output.split('\n').map(ToString::to_string).collect();
    let lines = trim_trailing_empty_lines(lines);
    let max_lines = if expanded {
        lines.len()
    } else {
        READ_PREVIEW_LINES
    };
    let display_lines = &lines[..max_lines.min(lines.len())];
    let remaining = lines.len() - display_lines.len();
    let mut text = format!(
        "\n{}",
        display_lines
            .iter()
            .map(|line| theme.fg(ThemeColor::ToolOutput, &replace_tabs(line)))
            .collect::<Vec<_>>()
            .join("\n")
    );
    if remaining > 0 {
        let _ = write!(text, "{}", expand_more_lines(theme, remaining));
    }

    let truncation = result.details.get("truncation");
    let truncated = truncation
        .and_then(|value| value.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if truncated {
        let first_line_exceeds_limit = truncation
            .and_then(|value| value.get("firstLineExceedsLimit"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let truncated_by = truncation
            .and_then(|value| value.get("truncatedBy"))
            .and_then(Value::as_str);
        let max_bytes = truncation
            .and_then(|value| value.get("maxBytes"))
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_BYTES as u64);
        let warning = if first_line_exceeds_limit {
            format!(
                "[First line exceeds {} limit]",
                format_size(max_bytes as usize)
            )
        } else if truncated_by == Some("lines") {
            let output_lines = truncation
                .and_then(|value| value.get("outputLines"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let total_lines = truncation
                .and_then(|value| value.get("totalLines"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            format!(
                "[Truncated: showing {output_lines} of {total_lines} lines ({DEFAULT_MAX_LINES} line limit)]"
            )
        } else {
            let output_lines = truncation
                .and_then(|value| value.get("outputLines"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            format!(
                "[Truncated: {output_lines} lines shown ({} limit)]",
                format_size(max_bytes as usize)
            )
        };
        let _ = write!(text, "\n{}", theme.fg(ThemeColor::Warning, &warning));
    }
    format!("{text}{image_warning}")
}

/// pi's `readRenderers` (`read.ts:152-177`).
pub const READ_RENDERERS: crate::tools::renderers::ToolRenderers =
    crate::tools::renderers::ToolRenderers {
        render_call: |args, context| {
            let args = ReadRenderArgs::from(args);
            let classification = if context.expanded {
                None
            } else {
                get_compact_read_classification(&args, context.cwd)
            };
            match &classification {
                Some(classification) => {
                    format_compact_read_call(classification, &args, context.theme)
                }
                None => format_read_call(&args, context.theme),
            }
        },
        render_result: |args, result, context| {
            format_read_result(
                &ReadRenderArgs::from(args),
                result,
                context.theme,
                context.expanded,
                context.is_error,
            )
        },
    };
