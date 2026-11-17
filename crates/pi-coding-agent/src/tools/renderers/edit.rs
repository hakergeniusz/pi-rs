//! Presentation for the edit tool, and the diff renderer it shares.
//!
//! Ground truth: pi-mono
//! `packages/coding-agent/src/core/tools/renderers/edit.ts` plus
//! `modes/interactive/components/diff.ts`.
//!
//! Upstream computes a live diff preview from the streaming arguments
//! (`computeEditsDiff` in an async renderCall); the port renders the preview
//! once the result arrives (`details.diff`), which is what a settled row
//! shows either way. The stream preview lands with the renderer-state work.

use pi_agent::types::AgentToolResult;
use pi_tui::theme::{Theme, ThemeColor};
use serde_json::Value;

use crate::tools::render_utils::{replace_tabs, str};
use crate::tools::renderers::{CallContext, ResultContext};

/// `formatEditCall` (`edit.ts:83-86`): `edit <path>`.
fn format_edit_call(args: Option<&Value>, theme: &Theme) -> String {
    let raw_path = str(args.and_then(|args| args.get("file_path").or_else(|| args.get("path"))));
    let path_display = crate::tools::renderers::render_tool_path(raw_path.as_deref(), theme, None);
    format!(
        "{} {path_display}",
        theme.fg(ThemeColor::ToolTitle, "\x1b[1medit\x1b[22m")
    )
}

/// `theme.inverse` (`theme.ts:327-329`): inverse video + reset.
fn inverse(text: &str) -> String {
    format!("\x1b[7m{text}\x1b[27m")
}

/// `formatEditResult` (`edit.ts:87-114`): the result diff, or the error text.
fn format_edit_result(
    args: Option<&Value>,
    result: &AgentToolResult,
    theme: &Theme,
    is_error: bool,
) -> String {
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
        return theme.fg(ThemeColor::Error, &error_text);
    }
    let result_diff = result.details.get("diff").and_then(Value::as_str);
    match result_diff {
        Some(diff) if !diff.is_empty() => render_diff(diff),
        // Upstream renders nothing when the row already shows the live
        // preview diff; the port's stream preview lands with renderer state.
        _ => {
            let _ = str(args.and_then(|args| args.get("file_path")));
            String::new()
        }
    }
}

/// pi's `editRenderers` (`edit.ts:173-252`).
pub const EDIT_RENDERERS: crate::tools::renderers::ToolRenderers =
    crate::tools::renderers::ToolRenderers {
        render_call: |args, context: &CallContext| format_edit_call(args, context.theme),
        render_result: |args, result: &AgentToolResult, context: &ResultContext| {
            format_edit_result(args, result, context.theme, context.is_error)
        },
    };

// ============================================================
// diff.ts — colored diff rendering with intra-line highlighting.
// ============================================================

/// One parsed diff line (`parseDiffLine`, `diff.ts:8-12`). The format is
/// `+123 content` / `-123 content` / ` 123 content`.
struct ParsedDiffLine<'a> {
    prefix: char,
    line_num: &'a str,
    content: &'a str,
}

fn parse_diff_line(line: &str) -> Option<ParsedDiffLine<'_>> {
    let mut chars = line.chars();
    let prefix = chars.next()?;
    if prefix != '+' && prefix != '-' && prefix != ' ' {
        return None;
    }
    let rest = chars.as_str();
    // `^([+-\s])(\s*\d*)\s(.*)$` with backtracking: the number group is
    // leading whitespace then digits, greedy, and exactly one whitespace
    // separates it from the content (`diff.ts:8-12`).
    // `char::is_whitespace` takes `self` by value, so it cannot stand in for
    // the closure here; `is_ascii_digit` (an `&self` method) can.
    let ws_max = rest.chars().take_while(|c| c.is_whitespace()).count();
    for ws in (0..=ws_max).rev() {
        let after_ws = &rest[ws..];
        let digits = after_ws.chars().take_while(char::is_ascii_digit).count();
        for d in (0..=digits).rev() {
            let after_digits = &after_ws[d..];
            if let Some(content) = after_digits.strip_prefix(char::is_whitespace) {
                return Some(ParsedDiffLine {
                    prefix,
                    line_num: &rest[..ws + d],
                    content,
                });
            }
        }
    }
    None
}

/// One word-level diff part (`Diff.diffWords` output shape).
struct WordPart {
    removed: bool,
    added: bool,
    value: String,
}

/// Tokenize into words and whitespace runs (`Diff.diffWords` tokenization).
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut current_is_space: Option<bool> = None;
    for character in text.chars() {
        let is_space = character.is_whitespace();
        if current_is_space == Some(is_space) || current.is_empty() {
            current.push(character);
        } else {
            tokens.push(std::mem::take(&mut current));
            current.push(character);
        }
        current_is_space = Some(is_space);
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Longest-common-subsequence diff over tokens, grouped into parts. Lines are
/// short, so the O(n·m) table is fine.
fn diff_words(old: &str, new: &str) -> Vec<WordPart> {
    let old_tokens = tokenize(old);
    let new_tokens = tokenize(new);
    let n = old_tokens.len();
    let m = new_tokens.len();
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if old_tokens[i] == new_tokens[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut parts: Vec<WordPart> = Vec::new();
    let push = |parts: &mut Vec<WordPart>, removed: bool, added: bool, value: String| {
        if value.is_empty() {
            return;
        }
        if let Some(last) = parts.last_mut()
            && last.removed == removed
            && last.added == added
        {
            last.value.push_str(&value);
            return;
        }
        parts.push(WordPart {
            removed,
            added,
            value,
        });
    };
    let mut i = 0;
    let mut j = 0;
    while i < n && j < m {
        if old_tokens[i] == new_tokens[j] {
            push(&mut parts, false, false, old_tokens[i].clone());
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            push(&mut parts, true, false, old_tokens[i].clone());
            i += 1;
        } else {
            push(&mut parts, false, true, new_tokens[j].clone());
            j += 1;
        }
    }
    for token in &old_tokens[i..] {
        push(&mut parts, true, false, token.clone());
    }
    for token in &new_tokens[j..] {
        push(&mut parts, false, true, token.clone());
    }
    parts
}

/// `renderIntraLineDiff` (`diff.ts:25-74`): word-level diff with inverse on
/// changed parts, common indentation separated out first.
fn render_intra_line_diff(old_content: &str, new_content: &str) -> (String, String) {
    let mut common_indent_length = 0;
    let old_chars: Vec<char> = old_content.chars().collect();
    let new_chars: Vec<char> = new_content.chars().collect();
    while common_indent_length < old_chars.len()
        && common_indent_length < new_chars.len()
        && old_chars[common_indent_length] == new_chars[common_indent_length]
        && old_chars[common_indent_length].is_whitespace()
    {
        common_indent_length += 1;
    }
    let common_indent: String = old_chars[..common_indent_length].iter().collect();
    let old_body: String = old_chars[common_indent_length..].iter().collect();
    let new_body: String = new_chars[common_indent_length..].iter().collect();

    let word_diff = diff_words(&old_body, &new_body);
    let mut removed_line = common_indent.clone();
    let mut added_line = common_indent;
    for part in &word_diff {
        if part.removed {
            removed_line.push_str(&inverse(&part.value));
        } else if part.added {
            added_line.push_str(&inverse(&part.value));
        } else {
            removed_line.push_str(&part.value);
            added_line.push_str(&part.value);
        }
    }
    (removed_line, added_line)
}

/// `renderDiff` (`diff.ts:87-155`): a colored diff with intra-line change
/// highlighting — context lines dim, removed red, added green, changed words
/// inverse.
pub fn render_diff(diff_text: &str) -> String {
    let theme = crate::modes::interactive::theme::theme();
    let lines: Vec<&str> = diff_text.split('\n').collect();
    let mut result: Vec<String> = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        let Some(parsed) = parse_diff_line(lines[i]) else {
            result.push(theme.fg(ThemeColor::ToolDiffContext, lines[i]));
            i += 1;
            continue;
        };

        if parsed.prefix == '-' {
            // Collect the consecutive removed block, then the added block.
            let mut removed_lines: Vec<(&str, &str)> = Vec::new();
            while i < lines.len()
                && let Some(parsed) = parse_diff_line(lines[i])
            {
                if parsed.prefix != '-' {
                    break;
                }
                removed_lines.push((parsed.line_num, parsed.content));
                i += 1;
            }
            let mut added_lines: Vec<(&str, &str)> = Vec::new();
            while let Some(parsed) = parse_diff_line(lines.get(i).copied().unwrap_or_default()) {
                if parsed.prefix != '+' {
                    break;
                }
                added_lines.push((parsed.line_num, parsed.content));
                i += 1;
            }

            // Intra-line highlighting only for a single-line modification
            // (`diff.ts:121-141`).
            if removed_lines.len() == 1 && added_lines.len() == 1 {
                let removed = removed_lines[0];
                let added = added_lines[0];
                let (removed_line, added_line) =
                    render_intra_line_diff(&replace_tabs(removed.1), &replace_tabs(added.1));
                result.push(theme.fg(
                    ThemeColor::ToolDiffRemoved,
                    &format!("-{} {removed_line}", removed.0),
                ));
                result.push(theme.fg(
                    ThemeColor::ToolDiffAdded,
                    &format!("+{} {added_line}", added.0),
                ));
            } else {
                for (line_num, content) in removed_lines {
                    result.push(theme.fg(
                        ThemeColor::ToolDiffRemoved,
                        &format!("-{line_num} {}", replace_tabs(content)),
                    ));
                }
                for (line_num, content) in added_lines {
                    result.push(theme.fg(
                        ThemeColor::ToolDiffAdded,
                        &format!("+{line_num} {}", replace_tabs(content)),
                    ));
                }
            }
        } else if parsed.prefix == '+' {
            result.push(theme.fg(
                ThemeColor::ToolDiffAdded,
                &format!("+{} {}", parsed.line_num, replace_tabs(parsed.content)),
            ));
            i += 1;
        } else {
            result.push(theme.fg(
                ThemeColor::ToolDiffContext,
                &format!(" {} {}", parsed.line_num, replace_tabs(parsed.content)),
            ));
            i += 1;
        }
    }

    result.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_tui::ansi::strip_terminal_sequences;

    #[test]
    fn diff_lines_color_and_pair_for_intra_line_highlighting() {
        // A single-line modification (exactly one removed, one added) gets
        // the intra-line treatment (`diff.ts:121-141`).
        let diff = "\u{0020}1 unchanged\n-2 old value\n+2 new value\n\u{0020}3 tail\n";
        let rendered = render_diff(diff);
        let plain: String = strip_terminal_sequences(&rendered);
        assert!(plain.contains(" 1 unchanged"), "{plain}");
        assert!(plain.contains("-2"), "{plain}");
        assert!(plain.contains("+2"), "{plain}");
        // The inverse highlight marks the changed words.
        assert!(
            rendered.contains("\x1b[7m"),
            "expected inverse on changed words"
        );

        // A 1:2 block renders as-is, no intra-line inverse.
        let block = render_diff("-2 old value\n+2 new value\n+3 tail\n");
        assert!(
            !block.contains("\x1b[7m"),
            "multi-line blocks stay plain: {block}"
        );
    }

    #[test]
    fn parse_handles_numbers_and_context() {
        let parsed = parse_diff_line("+123 added text").expect("added");
        assert_eq!(parsed.prefix, '+');
        assert_eq!(parsed.line_num, "123");
        assert_eq!(parsed.content, "added text");
        // `-1x` has no separator after the number, so upstream's regex
        // rejects it; `-1 ` parses with empty content.
        assert!(parse_diff_line("-1x").is_none());
        let parsed = parse_diff_line("-1 ").expect("removed with empty content");
        assert_eq!(parsed.prefix, '-');
        assert_eq!(parsed.content, "");
        assert!(parse_diff_line("plain text").is_none());
    }
}
