//! Tool-result text shaping.
//!
//! Ground truth: pi-mono
//! `packages/coding-agent/src/core/tools/render-utils.ts` (`getTextOutput`,
//! `normalizeDisplayText`, `replaceTabs`, `shortenPath`,
//! `formatReadLineRange`, `formatToolDuration`) plus the sanitizers it leans
//! on (`utils/shell.ts` `sanitizeBinaryOutput`, `utils/ansi.ts` `stripAnsi`).
//!
//! The TUI-facing renderers (colors, previews, timers) live in
//! [`crate::tools::renderers`]; what the loop and the CLI consume is the
//! shaping here.

use std::path::Path;

use pi_ai::types::ContentBlock;

/// pi's `sanitizeBinaryOutput` (`utils/shell.ts:157-166`): control characters
/// that crash width handling, plus the interlinear annotation characters.
/// Tab, newline, and carriage return survive.
pub fn sanitize_binary_output(text: &str) -> String {
    text.chars()
        .filter(|character| {
            let code = *character as u32;
            let is_stripped_control = matches!(code, 0x00..=0x08 | 0x0B | 0x0C | 0x0E..=0x1F);
            let is_interlinear = matches!(code, 0xFFF9..=0xFFFB);
            !is_stripped_control && !is_interlinear
        })
        .collect()
}

/// pi's `stripAnsi` (`utils/ansi.ts`): the ansi-regex pattern, hand-rolled.
pub fn strip_ansi(value: &str) -> String {
    if !value.contains('\u{1b}') && !value.contains('\u{9b}') {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' && character != '\u{9b}' {
            out.push(character);
            continue;
        }
        // ESC [ ... final byte, or ESC O <byte>, or a two-byte escape.
        match chars.peek() {
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                // OSC: terminated by BEL or ST.
                chars.next();
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

/// pi's `getTextOutput` (`render-utils.ts:53-78`): the text blocks of a
/// result, ANSI- and binary-sanitized and joined with newlines, plus a label
/// per image block when the caller will not draw the images itself.
///
/// `show_images` is pi's second argument: the caller draws the images when it
/// is true and the terminal can, and gets a label otherwise. This build has no
/// image drawing, so callers that cannot draw pass `false` and every image
/// gets its label.
pub fn get_text_output(content: &[ContentBlock], show_images: bool) -> String {
    let output = content
        .iter()
        .filter_map(|block| block.as_text())
        .map(|text| sanitize_binary_output(&strip_ansi(text)).replace('\r', ""))
        .collect::<Vec<String>>()
        .join("\n");
    if show_images {
        return output;
    }
    let indicators = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image(image) => Some(image_fallback(&image.mime_type)),
            _ => None,
        })
        .collect::<Vec<String>>()
        .join("\n");
    if indicators.is_empty() {
        return output;
    }
    if output.is_empty() {
        indicators
    } else {
        format!("{output}\n{indicators}")
    }
}

/// pi's `imageFallback` (`@earendil-works/pi-tui`): `[Image: <mime> <w>x<h>]`.
/// The dimensions pi reads out of the image data are not parsed here, so only
/// the mime type is shown.
pub fn image_fallback(mime_type: &str) -> String {
    format!("[Image: {mime_type}]")
}

/// pi's `normalizeDisplayText` (`render-utils.ts:49-51`).
pub fn normalize_display_text(text: &str) -> String {
    text.replace('\r', "")
}

/// pi's `replaceTabs` (`render-utils.ts:45-47`).
pub fn replace_tabs(text: &str) -> String {
    text.replace('\t', "   ")
}

/// pi's `shortenPath` (`render-utils.ts:10-13` + `shortenHomePath`): the home
/// directory becomes `~`.
pub fn shorten_path(path: &str) -> String {
    shorten_home_path(path, &std::env::var("HOME").unwrap_or_default())
}

/// pi's `shortenHomePath` (`utils/paths.ts:82-89`).
pub fn shorten_home_path(path: &str, home: &str) -> String {
    if home.is_empty() || !Path::new(path).is_absolute() || !is_path_within(home, path) {
        return path.to_string();
    }
    match Path::new(path).strip_prefix(home) {
        Ok(relative) if relative.as_os_str().is_empty() => "~".to_string(),
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => path.to_string(),
    }
}

/// pi's `isPathWithin` (`utils/paths.ts:66-73`).
pub fn is_path_within(parent: &str, child: &str) -> bool {
    let parent_key = parent.trim_end_matches('/');
    let child_key = child.trim_end_matches('/');
    if child_key == parent_key {
        return true;
    }
    child_key.starts_with(&format!("{parent_key}/"))
}

/// pi's `str` (`render-utils.ts:21-25`): `Some` for strings, `Some("")` for
/// null/undefined, `None` for anything else.
pub fn str(value: Option<&serde_json::Value>) -> Option<String> {
    match value {
        Some(serde_json::Value::String(text)) => Some(text.clone()),
        Some(serde_json::Value::Null) | None => Some(String::new()),
        Some(_) => None,
    }
}

fn parse_line_number(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::String(text) if !text.trim().is_empty() => {
            text.trim().parse::<f64>().ok()
        }
        serde_json::Value::Number(number) => number.as_f64().filter(|value| value.is_finite()),
        _ => None,
    }
}

/// pi's `formatReadLineRange` (`render-utils.ts:33-43`): format finite numbers
/// or numeric strings as a read line range, omitting malformed ranges.
pub fn format_read_line_range(
    offset: Option<&serde_json::Value>,
    limit: Option<&serde_json::Value>,
) -> String {
    // Strict schemas represent omitted optional arguments as null.
    let offset_missing = offset.is_none_or(serde_json::Value::is_null);
    let limit_missing = limit.is_none_or(serde_json::Value::is_null);
    if offset_missing && limit_missing {
        return String::new();
    }
    let start = if offset_missing {
        1.0
    } else {
        match offset.and_then(parse_line_number) {
            Some(start) => start,
            None => return String::new(),
        }
    };
    if limit_missing {
        return format!(":{start}");
    }
    let Some(count) = limit.and_then(parse_line_number) else {
        return String::new();
    };
    let end = start + count - 1.0;
    format!(
        ":{start}{}",
        if end == 0.0 {
            String::new()
        } else {
            format!("-{end}")
        }
    )
}

/// pi's `formatToolDuration` (`render-utils.ts:113-123`): `4.2s` below a
/// minute, then `1m 30s` and `2h 3m 4s`.
pub fn format_tool_duration(ms: f64) -> String {
    let seconds = ms / 1000.0;
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }
    let total_seconds = seconds.floor() as u64;
    let minutes = total_seconds / 60;
    let remainder = total_seconds % 60;
    if minutes < 60 {
        return format!("{minutes}m {remainder}s");
    }
    format!("{}h {}m {}s", minutes / 60, minutes % 60, remainder)
}
