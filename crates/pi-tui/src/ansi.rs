//! ANSI-aware string measurement and slicing.
//!
//! Ground truth: `packages/tui/src/utils.ts` (1,397 lines). This module ports
//! the load-bearing subset the renderer and the base components need:
//! `visibleWidth`, `sliceWithWidth`, `extractSegments`, `truncateToWidth`,
//! `wrapTextWithAnsi`, `applyBackgroundToLine`, `normalizeTerminalOutput` and
//! the `AnsiCodeTracker` they share. Editor/markdown word handling
//! (`wordSegmenter`, CJK break regexes, `getOsc8LinkAtColumn`) is 4b.
//!
//! Deviation: pi measures with `Intl.Segmenter` + `get-east-asian-width` +
//! an RGI-emoji probe (`utils.ts:182-248`). This port uses
//! `unicode-segmentation` (grapheme clusters) + `unicode-width` (East Asian
//! width) and an explicit regional-indicator rule. See `grapheme_width`.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Cursor marker a focused component emits at its cursor position.
/// `tui.ts:177`.
pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";

/// Style reset pi appends to every rendered line. `tui.ts:411`.
pub const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

/// Escape byte. Every escape sequence pi understands starts with it.
const ESC: u8 = 0x1b;

/// Width of a tab. `utils.ts:186` (tabs are three columns, not eight).
const TAB_WIDTH: usize = 3;

/// `utils.ts:453-490`: length in bytes of the ANSI/OSC/APC sequence starting
/// at `pos`, or 0 when there is none.
///
/// CSI runs to one of `m G K H J` (`utils.ts:461-468`), OSC/APC run to BEL or
/// ST (`ESC \`) (`utils.ts:472-480`).
pub fn ansi_code_len(s: &str, pos: usize) -> usize {
    let bytes = s.as_bytes();
    if pos >= bytes.len() || bytes[pos] != ESC {
        return 0;
    }
    match bytes.get(pos + 1) {
        Some(b'[') => bytes[pos + 2..]
            .iter()
            .position(|byte| matches!(byte, b'm' | b'G' | b'K' | b'H' | b'J'))
            .map_or(0, |offset| offset + 3),
        Some(b']' | b'_') => {
            let tail = &bytes[pos + 2..];
            if let Some(offset) = tail.iter().position(|byte| *byte == 0x07) {
                offset + 3
            } else {
                tail.iter()
                    .enumerate()
                    .find(|(offset, byte)| **byte == ESC && tail.get(offset + 1) == Some(&b'\\'))
                    .map_or(0, |(offset, _)| offset + 4)
            }
        }
        _ => 0,
    }
}

/// `utils.ts:421-428`.
pub fn extract_ansi_code(s: &str, pos: usize) -> Option<&str> {
    let len = ansi_code_len(s, pos);
    if len == 0 {
        return None;
    }
    s.get(pos..pos + len)
}

/// Width of one grapheme cluster in terminal columns.
///
/// pi: `graphemeWidth` (`utils.ts:182-248`). Port rules that survive without
/// the ICU regex probes:
/// * ASCII 0x20..0x7e → 1, tab → 3 (`utils.ts:183-187`);
/// * clusters made only of controls/marks → 0 (`utils.ts:196-199`);
/// * regional indicators → 2, because terminals draw isolated flags
///   double-width while a stream is still arriving (`utils.ts:211-217`);
/// * otherwise `unicode-width`'s East Asian width.
pub fn grapheme_width(segment: &str) -> usize {
    let mut chars = segment.chars();
    let Some(first) = chars.next() else {
        return 0;
    };
    if first == '\t' {
        return TAB_WIDTH;
    }
    if segment.chars().count() == 1 {
        let code = first as u32;
        if (0x20..=0x7e).contains(&code) {
            return 1;
        }
    }
    // Zero-width clusters: controls and combining marks take no cell
    // (`utils.ts:196-199`).
    if segment
        .chars()
        .all(|c| UnicodeWidthChar::width(c) == Some(0))
    {
        return 0;
    }
    if segment
        .chars()
        .all(|c| (0x1f1e6..=0x1f1ff).contains(&(c as u32)))
    {
        return 2;
    }
    UnicodeWidthStr::width(segment)
}

/// Width of a string made only of printable ASCII, tabs and ANSI escapes, or
/// `None` when it contains anything else. `utils.ts:430-451`.
fn ascii_visible_width(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut width = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x20..=0x7e => {
                width += 1;
                i += 1;
            }
            b'\t' => {
                width += TAB_WIDTH;
                i += 1;
            }
            ESC => {
                let len = ansi_code_len(s, i);
                if len == 0 {
                    return None;
                }
                i += len;
            }
            _ => return None,
        }
    }
    Some(width)
}

/// Width of `s` in terminal columns. `utils.ts:250-311`.
pub fn visible_width(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    if let Some(width) = ascii_visible_width(s) {
        return width;
    }
    // Tabs become three columns (`utils.ts:272-275`), then escapes are
    // stripped (`utils.ts:276-292`) before grapheme widths are summed.
    let mut expanded = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s.as_bytes()[i] == ESC {
            let len = ansi_code_len(s, i);
            if len > 0 {
                // Escapes are zero-width: dropped before measuring.
                i += len;
                continue;
            }
        }
        if s.as_bytes()[i] == b'\t' {
            expanded.push_str("   ");
            i += 1;
            continue;
        }
        let ch = s[i..].chars().next().expect("in bounds");
        expanded.push(ch);
        i += ch.len_utf8();
    }
    expanded.graphemes(true).map(grapheme_width).sum()
}

/// Remove ANSI, OSC and APC sequences, keeping visible text. `utils.ts:313-333`.
pub fn strip_terminal_sequences(s: &str) -> String {
    if !s.contains('\x1b') {
        return s.to_string();
    }
    let mut result = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if let Some(code) = extract_ansi_code(s, i) {
            i += code.len();
        } else {
            let ch = s[i..].chars().next().expect("in bounds");
            result.push(ch);
            i += ch.len_utf8();
        }
    }
    result
}

/// Thai/Lao AM decomposition that terminals render inconsistently.
/// `utils.ts:394-419` (`normalizeTerminalOutput`); the tab half is ported
/// verbatim, the Thai/Lao rewrite is left to 4b's text pipeline.
pub fn normalize_terminal_output(s: &str) -> String {
    if !s.contains('\t') {
        return s.to_string();
    }
    let mut result = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if let Some(code) = extract_ansi_code(s, i) {
            result.push_str(code);
            i += code.len();
        } else {
            let ch = s[i..].chars().next().expect("in bounds");
            if ch == '\t' {
                result.push_str("   ");
            } else {
                result.push(ch);
            }
            i += ch.len_utf8();
        }
    }
    result
}

/// End of the run of visible (non-escape) bytes starting at `i`.
///
/// A run never starts on an escape: pi's scanners skip escapes first and only
/// then segment the text between them (`utils.ts:339-345`).
fn visible_run_end(s: &str, i: usize) -> usize {
    let start = match extract_ansi_code(s, i) {
        Some(code) => i + code.len(),
        None => i,
    };
    let bytes = s.as_bytes();
    let mut end = start;
    while end < bytes.len() && extract_ansi_code(s, end).is_none() {
        end += 1;
    }
    end
}

/// Result of `slice_with_width`: the sliced text and its visible width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slice {
    pub text: String,
    pub width: usize,
}

/// `utils.ts:1268-1325`. With `strict`, a wide cluster that would straddle
/// the right edge is dropped instead of overflowing it.
pub fn slice_with_width(line: &str, start_col: usize, length: usize, strict: bool) -> Slice {
    if length == 0 {
        return Slice {
            text: String::new(),
            width: 0,
        };
    }
    let end_col = start_col + length;
    let mut result = String::new();
    let mut result_width = 0usize;
    let mut current_col = 0usize;
    let mut i = 0usize;
    let mut pending_ansi = String::new();

    while i < line.len() {
        if let Some(code) = extract_ansi_code(line, i) {
            if current_col >= start_col && current_col < end_col {
                result.push_str(&pending_ansi);
                result.push_str(code);
                pending_ansi.clear();
            } else if current_col < start_col {
                pending_ansi.push_str(code);
            }
            i += code.len();
            continue;
        }
        let text_end = visible_run_end(line, i);
        for segment in line[i..text_end].graphemes(true) {
            let w = grapheme_width(segment);
            let in_range = current_col >= start_col && current_col < end_col;
            let fits = !strict || current_col + w <= end_col;
            if in_range && fits {
                result.push_str(&pending_ansi);
                pending_ansi.clear();
                result.push_str(segment);
                result_width += w;
            }
            current_col += w;
            if current_col >= end_col {
                break;
            }
        }
        i = text_end;
        if current_col >= end_col {
            break;
        }
    }
    Slice {
        text: result,
        width: result_width,
    }
}

/// `utils.ts:1263-1266`.
pub fn slice_by_column(line: &str, start_col: usize, length: usize, strict: bool) -> String {
    slice_with_width(line, start_col, length, strict).text
}

/// The before/after pair `extractSegments` returns. `utils.ts:1326-1396`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Segments {
    pub before: String,
    pub before_width: usize,
    pub after: String,
    pub after_width: usize,
}

/// `utils.ts:1326-1396`. One pass over the line: `before` covers columns
/// `[0, before_end)`, `after` covers `[after_start, after_start + after_len)`
/// and inherits the SGR state active at `after_start`.
pub fn extract_segments(
    line: &str,
    before_end: usize,
    after_start: usize,
    after_len: usize,
    strict_after: bool,
) -> Segments {
    let mut segments = Segments::default();
    let mut current_col = 0usize;
    let mut i = 0usize;
    let mut pending_ansi_before = String::new();
    let mut after_started = false;
    let after_end = after_start + after_len;
    let mut tracker = AnsiCodeTracker::default();

    while i < line.len() {
        if let Some(code) = extract_ansi_code(line, i) {
            tracker.process(code);
            if current_col < before_end {
                pending_ansi_before.push_str(code);
            } else if current_col >= after_start && current_col < after_end && after_started {
                segments.after.push_str(code);
            }
            i += code.len();
            continue;
        }
        let text_end = visible_run_end(line, i);
        for segment in line[i..text_end].graphemes(true) {
            let w = grapheme_width(segment);
            if current_col < before_end && current_col + w <= before_end {
                segments.before.push_str(&pending_ansi_before);
                pending_ansi_before.clear();
                segments.before.push_str(segment);
                segments.before_width += w;
            } else if current_col >= after_start
                && current_col < after_end
                && (!strict_after || current_col + w <= after_end)
            {
                if !after_started {
                    segments.after.push_str(&tracker.active_codes());
                    after_started = true;
                }
                segments.after.push_str(segment);
                segments.after_width += w;
            }
            current_col += w;
            let done = if after_len == 0 {
                current_col >= before_end
            } else {
                current_col >= after_end
            };
            if done {
                break;
            }
        }
        i = text_end;
        let done = if after_len == 0 {
            current_col >= before_end
        } else {
            current_col >= after_end
        };
        if done {
            break;
        }
    }
    segments
}

/// Pad `line` to `width` and hand it to `bg_fn`. `utils.ts:1099-1110`.
/// `utils.ts:896-898`. pi's `flattenLines` is a V8 string-flattening hint
/// with no observable effect; the port keeps it as an explicit no-op so the
/// call sites stay one-to-one with pi's.
pub fn flatten_lines(_lines: &mut [String]) {}

pub fn apply_background_to_line(
    line: &str,
    width: usize,
    bg_fn: &dyn Fn(&str) -> String,
) -> String {
    let visible = visible_width(line);
    let padding = width.saturating_sub(visible);
    let mut with_padding = String::with_capacity(line.len() + padding);
    with_padding.push_str(line);
    with_padding.push_str(&" ".repeat(padding));
    bg_fn(&with_padding)
}

/// Finish a truncation: close anything still open in the prefix, reset, then
/// the ellipsis. `utils.ts:155-175`.
///
/// The reset and the OSC 8 close are what make truncation safe: without them
/// the ellipsis is drawn in the clipped text's colour, and an open hyperlink
/// swallows every byte after it on that line.
fn finalize_truncated_result(
    text: &str,
    text_width: usize,
    ellipsis: &str,
    ellipsis_width: usize,
    max_width: usize,
    pad: bool,
) -> String {
    let hyperlink_close = active_osc8_close(text);
    let mut result = String::with_capacity(text.len() + ellipsis.len() + 16);
    result.push_str(text);
    result.push_str(&hyperlink_close);
    result.push_str(SGR_RESET);
    if !ellipsis.is_empty() {
        result.push_str(ellipsis);
        result.push_str(SGR_RESET);
    }
    let width = text_width + ellipsis_width;
    if pad {
        result.push_str(&" ".repeat(max_width.saturating_sub(width)));
    }
    result
}

/// The OSC 8 close `prefix` still needs, or empty when no link is open.
/// `utils.ts:520-540`.
///
/// pi scans for the last OSC 8 sequence and emits a close for its
/// terminator. A sequence with an empty url *is* the close, so a prefix that
/// ends closed needs nothing; a prefix with no OSC 8 at all needs nothing
/// either. A body with no `;` separator is malformed and carries no answer at
/// all, so it leaves the link found before it alone.
pub fn active_osc8_close(prefix: &str) -> String {
    if !prefix.contains("\x1b]8;") {
        return String::new();
    }
    let mut active: Option<u8> = None;
    let mut index = 0usize;
    while index < prefix.len() {
        if prefix.as_bytes()[index] == 0x1b {
            match ansi_code_len(prefix, index) {
                length if length > 0 => {
                    match parse_osc8(&prefix[index..index + length]) {
                        // `Some(url)` opens a link, an empty url closes it.
                        Osc8Parse::Open(_, terminator) => active = Some(terminator),
                        Osc8Parse::Close => active = None,
                        Osc8Parse::NotOsc8 => {}
                    }
                    index += length;
                    continue;
                }
                _ => {}
            }
        }
        index += prefix[index..].chars().next().map_or(1, char::len_utf8);
    }
    match active {
        Some(terminator) => format_osc8_close(terminator),
        None => String::new(),
    }
}

/// SGR reset. `utils.ts:160`.
const SGR_RESET: &str = "\x1b[0m";

/// `utils.ts:75-153`: keep the leading graphemes that fit in `max_width`,
/// never splitting a cluster and never splitting an escape sequence.
fn truncate_fragment_to_width(text: &str, max_width: usize) -> Slice {
    if max_width == 0 || text.is_empty() {
        return Slice {
            text: String::new(),
            width: 0,
        };
    }
    if text.is_ascii() && text.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        let clipped = text
            .as_bytes()
            .iter()
            .take(max_width)
            .map(|&b| b as char)
            .collect::<String>();
        return Slice {
            width: clipped.len(),
            text: clipped,
        };
    }

    let has_ansi = text.contains('\x1b');
    let has_tabs = text.contains('\t');
    if !has_ansi && !has_tabs {
        let mut result = String::new();
        let mut width = 0usize;
        for segment in text.graphemes(true) {
            let w = grapheme_width(segment);
            if width + w > max_width {
                break;
            }
            result.push_str(segment);
            width += w;
        }
        return Slice {
            text: result,
            width,
        };
    }

    let mut result = String::new();
    let mut width = 0usize;
    let mut i = 0usize;
    let mut pending_ansi = String::new();
    while i < text.len() {
        if let Some(code) = extract_ansi_code(text, i) {
            pending_ansi.push_str(code);
            i += code.len();
            continue;
        }
        if text.as_bytes()[i] == b'\t' {
            if width + TAB_WIDTH > max_width {
                break;
            }
            result.push_str(&pending_ansi);
            pending_ansi.clear();
            result.push('\t');
            width += TAB_WIDTH;
            i += 1;
            continue;
        }
        let end = visible_run_end(text, i);
        for segment in text[i..end].graphemes(true) {
            let w = grapheme_width(segment);
            if width + w > max_width {
                return Slice {
                    text: result,
                    width,
                };
            }
            result.push_str(&pending_ansi);
            pending_ansi.clear();
            result.push_str(segment);
            width += w;
        }
        i = end;
    }
    Slice {
        text: result,
        width,
    }
}

/// `utils.ts:1121-1257`. Truncate to `max_width` visible columns, appending
/// `ellipsis` when something was dropped, optionally padding to `max_width`.
// Long because it ports the upstream branchy happy-path/ASCII/ANSI+tab cases
// in one function; splitting it would obscure the TS line mapping.
#[allow(clippy::too_many_lines)]
pub fn truncate_to_width(text: &str, max_width: usize, ellipsis: &str, pad: bool) -> String {
    if max_width == 0 {
        return String::new();
    }
    if text.is_empty() {
        return if pad {
            " ".repeat(max_width)
        } else {
            String::new()
        };
    }

    let ellipsis_width = visible_width(ellipsis);
    if ellipsis_width >= max_width {
        let text_width = visible_width(text);
        if text_width <= max_width {
            return if pad {
                format!("{}{}", text, " ".repeat(max_width - text_width))
            } else {
                text.to_string()
            };
        }
        let clipped = truncate_fragment_to_width(ellipsis, max_width);
        if clipped.width == 0 {
            return if pad {
                " ".repeat(max_width)
            } else {
                String::new()
            };
        }
        return finalize_truncated_result("", 0, &clipped.text, clipped.width, max_width, pad);
    }

    if is_printable_ascii(text) {
        if text.len() <= max_width {
            return if pad {
                format!("{}{}", text, " ".repeat(max_width - text.len()))
            } else {
                text.to_string()
            };
        }
        let target_width = max_width - ellipsis_width;
        let clipped: String = text
            .as_bytes()
            .iter()
            .take(target_width)
            .map(|&b| b as char)
            .collect();
        return finalize_truncated_result(
            &clipped,
            target_width,
            ellipsis,
            ellipsis_width,
            max_width,
            pad,
        );
    }

    let target_width = max_width - ellipsis_width;
    let mut result = String::new();
    let mut pending_ansi = String::new();
    let mut visible_so_far = 0usize;
    let mut kept_width = 0usize;
    let mut keep_contiguous_prefix = true;
    let mut overflowed = false;

    let has_ansi = text.contains('\x1b');
    let has_tabs = text.contains('\t');
    let mut i = 0usize;
    let exhausted_input = if !has_ansi && !has_tabs {
        for segment in text.graphemes(true) {
            let width = grapheme_width(segment);
            if keep_contiguous_prefix && kept_width + width <= target_width {
                result.push_str(segment);
                kept_width += width;
            } else {
                keep_contiguous_prefix = false;
            }
            visible_so_far += width;
            if visible_so_far > max_width {
                overflowed = true;
                break;
            }
        }
        !overflowed
    } else {
        while i < text.len() {
            if let Some(code) = extract_ansi_code(text, i) {
                pending_ansi.push_str(code);
                i += code.len();
                continue;
            }
            if text.as_bytes()[i] == b'\t' {
                if keep_contiguous_prefix && kept_width + TAB_WIDTH <= target_width {
                    result.push_str(&pending_ansi);
                    pending_ansi.clear();
                    result.push('\t');
                    kept_width += TAB_WIDTH;
                } else {
                    keep_contiguous_prefix = false;
                    pending_ansi.clear();
                }
                visible_so_far += TAB_WIDTH;
                if visible_so_far > max_width {
                    overflowed = true;
                    break;
                }
                i += 1;
                continue;
            }
            let mut end = i;
            while end < text.len()
                && text.as_bytes()[end] != b'\t'
                && extract_ansi_code(text, end).is_none()
            {
                end += 1;
            }
            for segment in text[i..end].graphemes(true) {
                let width = grapheme_width(segment);
                if keep_contiguous_prefix && kept_width + width <= target_width {
                    result.push_str(&pending_ansi);
                    pending_ansi.clear();
                    result.push_str(segment);
                    kept_width += width;
                } else {
                    keep_contiguous_prefix = false;
                    pending_ansi.clear();
                }
                visible_so_far += width;
                if visible_so_far > max_width {
                    overflowed = true;
                    break;
                }
            }
            if overflowed {
                break;
            }
            i = end;
        }
        i >= text.len()
    };

    if !overflowed && exhausted_input {
        return if pad {
            format!(
                "{}{}",
                text,
                " ".repeat(max_width.saturating_sub(visible_so_far))
            )
        } else {
            text.to_string()
        };
    }
    finalize_truncated_result(
        &result,
        kept_width,
        ellipsis,
        ellipsis_width,
        max_width,
        pad,
    )
}

/// `utils.ts:65-72`.
fn is_printable_ascii(s: &str) -> bool {
    s.bytes().all(|code| (0x20..=0x7e).contains(&code))
}

/// Kind of token `split_into_tokens_with_ansi` produced.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum TokenKind {
    Space,
    Word,
}

/// `utils.ts:799-894`: split into whitespace/word tokens, attaching pending
/// escape sequences to the next visible character. CJK breaks inside a word
/// stay part of it (pi's `cjkBreakRegex` splits per cluster; that rule is
/// 4b's autocomplete boundary work).
fn split_into_tokens_with_ansi(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut pending_ansi = String::new();
    let mut current_kind: Option<TokenKind> = None;
    let mut i = 0usize;

    let flush =
        |tokens: &mut Vec<String>, current: &mut String, current_kind: &mut Option<TokenKind>| {
            if current.is_empty() {
                return;
            }
            tokens.push(std::mem::take(current));
            *current_kind = None;
        };

    while i < text.len() {
        if let Some(code) = extract_ansi_code(text, i) {
            pending_ansi.push_str(code);
            i += code.len();
            continue;
        }
        let end = visible_run_end(text, i);
        let chunk = &text[i..end];
        let ascii = is_printable_ascii(chunk);
        if ascii {
            for ch in chunk.chars() {
                let kind = if ch == ' ' {
                    TokenKind::Space
                } else {
                    TokenKind::Word
                };
                if !current.is_empty() && current_kind != Some(kind) {
                    flush(&mut tokens, &mut current, &mut current_kind);
                }
                current.push_str(&pending_ansi);
                pending_ansi.clear();
                current_kind = Some(kind);
                current.push(ch);
            }
        } else {
            for segment in chunk.graphemes(true) {
                let kind = if segment == " " {
                    TokenKind::Space
                } else {
                    TokenKind::Word
                };
                if !current.is_empty() && current_kind != Some(kind) {
                    flush(&mut tokens, &mut current, &mut current_kind);
                }
                current.push_str(&pending_ansi);
                pending_ansi.clear();
                current_kind = Some(kind);
                current.push_str(segment);
            }
        }
        i = end;
    }

    if !pending_ansi.is_empty() {
        if !current.is_empty() {
            current.push_str(&pending_ansi);
        } else if let Some(last) = tokens.last_mut() {
            last.push_str(&pending_ansi);
        } else {
            current.push_str(&pending_ansi);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// `utils.ts:1022-1097`: break one over-long token at cluster boundaries.
fn break_long_word(word: &str, width: usize, tracker: &mut AnsiCodeTracker) -> Vec<String> {
    enum Segment {
        Ansi(String),
        Grapheme(String),
    }

    let mut segments: Vec<Segment> = Vec::new();
    let mut i = 0usize;
    while i < word.len() {
        if let Some(code) = extract_ansi_code(word, i) {
            segments.push(Segment::Ansi(code.to_string()));
            i += code.len();
            continue;
        }
        let end = visible_run_end(word, i);
        for segment in word[i..end].graphemes(true) {
            segments.push(Segment::Grapheme(segment.to_string()));
        }
        i = end;
    }

    let mut lines: Vec<String> = Vec::new();
    let mut current_line = tracker.active_codes();
    let mut current_width = 0usize;
    for segment in segments {
        match segment {
            Segment::Ansi(code) => {
                tracker.process(&code);
                current_line.push_str(&code);
            }
            Segment::Grapheme(grapheme) => {
                if grapheme.is_empty() {
                    continue;
                }
                let w = visible_width(&grapheme);
                if current_width + w > width {
                    current_line.push_str(&tracker.line_end_reset());
                    lines.push(std::mem::take(&mut current_line));
                    current_line = tracker.active_codes();
                    current_width = 0;
                }
                current_line.push_str(&grapheme);
                current_width += w;
            }
        }
    }
    if !current_line.is_empty() {
        lines.push(current_line);
    }
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

/// `utils.ts:900-1004`. Word wrap; no padding, no background. Active SGR
/// codes are re-emitted on each continuation line.
pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut result: Vec<String> = Vec::new();
    let mut tracker = AnsiCodeTracker::default();

    for input_line in split_newlines(text) {
        let prefix = if result.is_empty() {
            String::new()
        } else {
            tracker.active_codes()
        };
        for wrapped in wrap_single_line(&format!("{prefix}{input_line}"), width) {
            result.push(wrapped);
        }
        update_tracker_from_text(input_line, &mut tracker);
    }
    if result.is_empty() {
        vec![String::new()]
    } else {
        result
    }
}

/// Split on `\r\n`, `\r` or `\n`. `utils.ts:911`.
fn split_newlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                lines.push(&text[start..i]);
                if bytes.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
                i += 1;
                start = i;
            }
            b'\n' => {
                lines.push(&text[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    lines.push(&text[start..]);
    lines
}

/// `utils.ts:925-1004`. The tracker is local to one wrapped line, like pi's.
fn wrap_single_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    if visible_width(line) <= width {
        return vec![line.to_string()];
    }

    let mut tracker = AnsiCodeTracker::default();
    let mut wrapped: Vec<String> = Vec::new();
    let tokens = split_into_tokens_with_ansi(line);
    let mut current_line = String::new();
    let mut current_visible_length = 0usize;

    for token in tokens {
        let token_visible_length = visible_width(&token);
        let is_whitespace = token.trim().is_empty();

        if token_visible_length > width && !is_whitespace {
            if !current_line.is_empty() {
                current_line.push_str(&tracker.line_end_reset());
                wrapped.push(std::mem::take(&mut current_line));
            }
            let broken = break_long_word(&token, width, &mut tracker);
            let last = broken.len().saturating_sub(1);
            for line in broken.iter().take(last) {
                wrapped.push(line.clone());
            }
            current_line.clone_from(&broken[last]);
            current_visible_length = visible_width(&current_line);
            // breakLongWord folded the token's escapes into the tracker.
            continue;
        }

        if current_visible_length + token_visible_length > width && current_visible_length > 0 {
            let mut line_to_wrap = current_line.trim_end().to_string();
            line_to_wrap.push_str(&tracker.line_end_reset());
            wrapped.push(line_to_wrap);
            if is_whitespace {
                current_line = tracker.active_codes();
                current_visible_length = 0;
            } else {
                current_line = format!("{}{}", tracker.active_codes(), token);
                current_visible_length = token_visible_length;
            }
        } else {
            current_line.push_str(&token);
            current_visible_length += token_visible_length;
        }
        update_tracker_from_text(&token, &mut tracker);
    }

    if !current_line.is_empty() {
        wrapped.push(current_line);
    }
    if wrapped.is_empty() {
        return vec![String::new()];
    }
    wrapped
        .into_iter()
        .map(|line| line.trim_end().to_string())
        .collect()
}

/// Feed every escape sequence in `text` to `tracker`. `utils.ts:771-784`.
pub fn update_tracker_from_text(text: &str, tracker: &mut AnsiCodeTracker) {
    let mut i = 0usize;
    while i < text.len() {
        if let Some(code) = extract_ansi_code(text, i) {
            tracker.process(code);
            i += code.len();
        } else {
            let end = visible_run_end(text, i);
            i = end;
        }
    }
}

/// Active SGR state, so styling survives line breaks. `utils.ts:539-769`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
// One flag per SGR mode, mirroring upstream's tracker state fields.
#[allow(clippy::struct_excessive_bools)]
pub struct AnsiCodeTracker {
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    blink: bool,
    inverse: bool,
    hidden: bool,
    strikethrough: bool,
    fg: Option<String>,
    bg: Option<String>,
    /// OSC 8 hyperlink target and terminator, when open.
    hyperlink: Option<(String, u8)>,
}

impl AnsiCodeTracker {
    /// `utils.ts:560-...` (`process`).
    pub fn process(&mut self, ansi_code: &str) {
        // pi assigns every non-`undefined` answer, so a close (`null`) clears
        // the link and a malformed body (`undefined`) leaves it alone
        // (`utils.ts:563-567`).
        match parse_osc8(ansi_code) {
            Osc8Parse::Open(url, terminator) => {
                self.hyperlink = Some((url, terminator));
                return;
            }
            Osc8Parse::Close => {
                self.hyperlink = None;
                return;
            }
            Osc8Parse::NotOsc8 => {}
        }
        if !ansi_code.ends_with('m') {
            return;
        }
        let Some(params) = ansi_code
            .strip_prefix("\x1b[")
            .and_then(|rest| rest.strip_suffix('m'))
        else {
            return;
        };
        if params.is_empty() || params == "0" {
            self.reset();
            return;
        }
        let parts: Vec<&str> = params.split(';').collect();
        let mut i = 0;
        while i < parts.len() {
            let Ok(code) = parts[i].parse::<u32>() else {
                i += 1;
                continue;
            };
            match code {
                38 | 48 => {
                    if parts.get(i + 1) == Some(&"5") && parts.get(i + 2).is_some() {
                        let color = format!("{};{};{}", parts[i], parts[i + 1], parts[i + 2]);
                        if code == 38 {
                            self.fg = Some(color);
                        } else {
                            self.bg = Some(color);
                        }
                        i += 3;
                        continue;
                    }
                    if parts.get(i + 1) == Some(&"2")
                        && parts.get(i + 2).is_some()
                        && parts.get(i + 3).is_some()
                        && parts.get(i + 4).is_some()
                    {
                        let color = format!(
                            "{};2;{};{};{}",
                            parts[i],
                            parts[i + 2],
                            parts[i + 3],
                            parts[i + 4]
                        );
                        if code == 38 {
                            self.fg = Some(color);
                        } else {
                            self.bg = Some(color);
                        }
                        i += 5;
                        continue;
                    }
                }
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                5 => self.blink = true,
                7 => self.inverse = true,
                8 => self.hidden = true,
                9 => self.strikethrough = true,
                21 => self.bold = false,
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                25 => self.blink = false,
                27 => self.inverse = false,
                28 => self.hidden = false,
                29 => self.strikethrough = false,
                39 => self.fg = None,
                49 => self.bg = None,
                other => {
                    if (30..=37).contains(&other) || (90..=97).contains(&other) {
                        self.fg = Some(other.to_string());
                    } else if (40..=47).contains(&other) || (100..=107).contains(&other) {
                        self.bg = Some(other.to_string());
                    }
                }
            }
            i += 1;
        }
    }

    /// SGR reset does not touch the OSC 8 hyperlink. `utils.ts:703-716`.
    fn reset(&mut self) {
        self.bold = false;
        self.dim = false;
        self.italic = false;
        self.underline = false;
        self.blink = false;
        self.inverse = false;
        self.hidden = false;
        self.strikethrough = false;
        self.fg = None;
        self.bg = None;
    }

    /// The OSC 8 target covering the current position, if a link is open.
    /// `utils.ts:699-702`.
    pub fn active_link(&self) -> Option<&str> {
        match &self.hyperlink {
            Some((url, _)) if !url.is_empty() => Some(url),
            _ => None,
        }
    }

    /// Escape prefix that restores the tracked state. `utils.ts:723-741`.
    pub fn active_codes(&self) -> String {
        let mut codes: Vec<&str> = Vec::new();
        if self.bold {
            codes.push("1");
        }
        if self.dim {
            codes.push("2");
        }
        if self.italic {
            codes.push("3");
        }
        if self.underline {
            codes.push("4");
        }
        if self.blink {
            codes.push("5");
        }
        if self.inverse {
            codes.push("7");
        }
        if self.hidden {
            codes.push("8");
        }
        if self.strikethrough {
            codes.push("9");
        }
        if let Some(fg) = &self.fg {
            codes.push(fg);
        }
        if let Some(bg) = &self.bg {
            codes.push(bg);
        }
        let mut result = if codes.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", codes.join(";"))
        };
        if let Some((url, terminator)) = &self.hyperlink {
            result.push_str(&format_osc8_hyperlink(url, *terminator));
        }
        result
    }

    /// Background code alone, or empty when none is active. `utils.ts:743-745`.
    pub fn active_background_code(&self) -> String {
        match &self.bg {
            Some(code) => format!("\x1b[{code}m"),
            None => String::new(),
        }
    }

    /// Whether anything at all is active. `utils.ts:747-762`.
    pub fn has_active_codes(&self) -> bool {
        self.bold
            || self.dim
            || self.italic
            || self.underline
            || self.blink
            || self.inverse
            || self.hidden
            || self.strikethrough
            || self.fg.is_some()
            || self.bg.is_some()
            || self.hyperlink.is_some()
    }

    /// Codes needed at end of line: underline off (not a full reset, the
    /// background must survive into the padding) and close the hyperlink.
    /// `utils.ts:764-779`.
    pub fn line_end_reset(&self) -> String {
        let mut result = String::new();
        if self.underline {
            result.push_str("\x1b[24m");
        }
        if let Some((_, terminator)) = &self.hyperlink {
            result.push_str(&format_osc8_close(*terminator));
        }
        result
    }
}

/// Parse an OSC 8 sequence into `(url, terminator)`; `None` when `code` is not
/// one. `utils.ts:492-511`.
fn parse_osc8_hyperlink(ansi_code: &str) -> Option<(String, u8)> {
    match parse_osc8(ansi_code) {
        Osc8Parse::Open(url, terminator) => Some((url, terminator)),
        // A close carries no url, and "not OSC 8" is not a link at all; the
        // tracker only needs to know which of the two it saw, and treats both
        // as "no hyperlink open".
        Osc8Parse::Close | Osc8Parse::NotOsc8 => None,
    }
}

/// pi's three answers, `ActiveHyperlink | null | undefined` (`utils.ts:492-511`):
/// an open link, a close, and `undefined` — which is both "not OSC 8 at all" and
/// "an OSC 8 whose body has no `;` separator", and leaves the active link alone
/// in both cases.
enum Osc8Parse {
    Open(String, u8),
    Close,
    NotOsc8,
}

fn parse_osc8(ansi_code: &str) -> Osc8Parse {
    let Some(rest) = ansi_code.strip_prefix("\x1b]8;") else {
        return Osc8Parse::NotOsc8;
    };
    let (body, terminator) = if let Some(body) = rest.strip_suffix('\x07') {
        (body, 0x07u8)
    } else if let Some(body) = rest.strip_suffix("\x1b\\") {
        (body, 0x1bu8)
    } else {
        return Osc8Parse::NotOsc8;
    };
    // body is `params;url`; without the `;` the body is malformed and pi's
    // `parseOsc8Hyperlink` answers `undefined` (`utils.ts:497-499`), i.e. the
    // sequence says nothing about the active link, so it is left unchanged.
    let Some(separator) = body.find(';') else {
        return Osc8Parse::NotOsc8;
    };
    let url = &body[separator + 1..];
    if url.is_empty() {
        return Osc8Parse::Close;
    }
    Osc8Parse::Open(url.to_string(), terminator)
}

/// The OSC 8 hyperlink covering `column`, if any. `getOsc8LinkAtColumn`,
/// `utils.ts:359-381`.
///
/// Walks the line the way the terminal does: escape sequences update the
/// active link without advancing the column, and text is measured in
/// grapheme clusters so a wide glyph is one cell range rather than two.
pub fn osc8_link_at_column(line: &str, column: usize) -> Option<String> {
    let mut active_url: Option<String> = None;
    let mut current_col = 0usize;
    let mut i = 0usize;
    while i < line.len() {
        if let Some(code) = extract_ansi_code(line, i) {
            if let Some((url, _)) = parse_osc8_hyperlink(code) {
                active_url = (!url.is_empty()).then_some(url);
            }
            i += code.len();
            continue;
        }
        let mut text_end = i;
        while text_end < line.len() && extract_ansi_code(line, text_end).is_none() {
            text_end += 1;
        }
        for grapheme in line[i..text_end].graphemes(true) {
            let width = grapheme_width(grapheme);
            if column >= current_col && column < current_col + width {
                return active_url;
            }
            current_col += width;
        }
        i = text_end;
    }
    None
}

fn format_osc8_hyperlink(url: &str, terminator: u8) -> String {
    // The original terminator is preserved: some terminals only make
    // BEL-terminated links clickable (`utils.ts:568-572`).
    if terminator == 0x07 {
        format!("\x1b]8;;{url}\x07")
    } else {
        format!("\x1b]8;;{url}\x1b\\")
    }
}

fn format_osc8_close(terminator: u8) -> String {
    format_osc8_hyperlink("", terminator)
}

/// Background color active at the end of `text`. `utils.ts:786-797`.
pub fn active_background_ansi(text: &str) -> String {
    let mut tracker = AnsiCodeTracker::default();
    update_tracker_from_text(text, &mut tracker);
    tracker.active_background_code()
}

/// Cell range occupied by the grapheme cluster covering `column`.
/// `utils.ts:335-356` (`getGraphemeCellRange`).
///
/// The range is in **terminal columns**, not bytes: callers slice the line
/// with it directly.
pub fn grapheme_cell_range(line: &str, column: usize) -> Option<(usize, usize)> {
    let mut current_col = 0usize;
    let mut i = 0usize;
    while i < line.len() {
        if let Some(code) = extract_ansi_code(line, i) {
            i += code.len();
            continue;
        }
        let mut text_end = i;
        while text_end < line.len() && extract_ansi_code(line, text_end).is_none() {
            text_end += 1;
        }
        for segment in line[i..text_end].graphemes(true) {
            let width = grapheme_width(segment);
            if width > 0 && column >= current_col && column < current_col + width {
                return Some((current_col, current_col + width));
            }
            current_col += width;
        }
        i = text_end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_code_len_matches_pi_scanners() {
        // utils.ts:453-490
        assert_eq!(ansi_code_len("\x1b[31m", 0), 5);
        assert_eq!(ansi_code_len("\x1b[2K", 0), 4);
        assert_eq!(ansi_code_len("\x1b]8;;http://x\x07", 0), 14);
        assert_eq!(ansi_code_len("\x1b]8;;http://x\x1b\\", 0), 15);
        assert_eq!(ansi_code_len(CURSOR_MARKER, 0), CURSOR_MARKER.len());
        assert_eq!(ansi_code_len("plain", 0), 0);
        assert_eq!(ansi_code_len("\x1b", 0), 0);
    }

    #[test]
    fn visible_width_counts_columns_not_bytes() {
        assert_eq!(visible_width(""), 0);
        assert_eq!(visible_width("abc"), 3);
        assert_eq!(visible_width("\x1b[31mabc\x1b[39m"), 3);
        assert_eq!(visible_width("\ta"), 4);
        assert_eq!(visible_width("日本語"), 6);
        assert_eq!(visible_width("e\u{301}"), 1);
        assert_eq!(visible_width("\x1b[31mé\x1b[39m"), 1);
        // Regional indicators are drawn double-width while streaming.
        assert_eq!(visible_width("\u{1f1e6}\u{1f1e7}"), 2);
        // Combining-only cluster takes no cell.
        assert_eq!(visible_width("\u{301}"), 0);
    }

    #[test]
    fn strip_sequences_keeps_text() {
        assert_eq!(strip_terminal_sequences("\x1b[1mhi\x1b[22m"), "hi");
        assert_eq!(strip_terminal_sequences("plain"), "plain");
    }

    #[test]
    fn slice_with_width_keeps_styles_inside_range() {
        let line = "\x1b[31maaa\x1b[32mbbb\x1b[39m";
        // Columns [2,4) hold the third "a" and the first "b"; the red SGR
        // was opened before the range and is re-emitted so the slice renders.
        let slice = slice_with_width(line, 2, 2, false);
        assert_eq!(slice.width, 2);
        assert_eq!(slice.text, "\x1b[31ma\x1b[32mb");
        let inner = slice_with_width("\x1b[31mabcd\x1b[39m", 1, 2, false);
        assert_eq!(inner.width, 2);
        assert_eq!(inner.text, "\x1b[31mbc");
    }

    #[test]
    fn slice_is_strict_at_wide_cluster_boundary() {
        let line = "a日b";
        assert_eq!(visible_width(line), 4);
        let loose = slice_with_width(line, 1, 1, false);
        assert_eq!(loose.width, 2);
        let strict = slice_with_width(line, 1, 1, true);
        assert_eq!(strict.width, 0);
        assert_eq!(strict.text, "");
    }

    #[test]
    fn extract_segments_inherits_style_into_after() {
        // Columns: aa=0..1, (green) bbbb=2..5, cc=6..7.
        let segments = extract_segments("\x1b[31maa\x1b[32mbbbbcccc", 2, 6, 2, false);
        assert_eq!(segments.before, "\x1b[31maa");
        assert_eq!(segments.before_width, 2);
        assert_eq!(segments.after_width, 2);
        // The green fg active inside the overlay region is re-emitted on the
        // "after" segment, so content after the overlay keeps its styling.
        assert_eq!(segments.after, "\x1b[32mcc");

        // A reset sitting exactly on the boundary clears it, like pi: the
        // scanner folds escapes into the tracker when it reaches them, and
        // at column 6 the reset has already been seen.
        let reset_on_edge = extract_segments("\x1b[31maa\x1b[32mbbbb\x1b[39mcc", 2, 6, 2, false);
        assert_eq!(reset_on_edge.after, "cc");
    }

    #[test]
    fn truncate_to_width_appends_ellipsis_only_when_needed() {
        // pi brackets the ellipsis with resets even for unstyled text
        // (`utils.ts:155-175`, `truncate-to-width.test.ts:40`).
        assert_eq!(truncate_to_width("hello", 10, "...", false), "hello");
        assert_eq!(
            truncate_to_width("hello world", 8, "...", false),
            "hello\x1b[0m...\x1b[0m"
        );
        assert_eq!(truncate_to_width("hello", 8, "...", true), "hello   ");
        assert_eq!(truncate_to_width("", 4, "...", true), "    ");
        assert_eq!(truncate_to_width("hello", 0, "...", false), "");
        assert_eq!(truncate_to_width("abcdef", 3, "", false), "abc\x1b[0m");
    }

    #[test]
    fn truncate_to_width_respects_styled_text() {
        let styled = "\x1b[31mhello world\x1b[39m";
        let truncated = truncate_to_width(styled, 8, "...", false);
        assert_eq!(visible_width(&truncated), 8);
        assert_eq!(truncated, "\x1b[31mhello\x1b[0m...\x1b[0m");
    }

    /// `truncate-to-width.test.ts:7-11`: huge input still fits, and the
    /// ellipsis is bracketed by a reset.
    #[test]
    fn truncate_to_width_keeps_output_within_width_for_huge_unicode_input() {
        let text = "\u{1f642}\u{754c}".repeat(100_000);
        let truncated = truncate_to_width(&text, 40, "\u{2026}", false);
        assert!(visible_width(&truncated) <= 40);
        assert!(
            truncated.ends_with("\u{2026}\x1b[0m"),
            "got {:?}",
            &truncated[truncated.len().saturating_sub(20)..]
        );
    }

    /// `truncate-to-width.test.ts:14-21`: kept text keeps its colour, and the
    /// ellipsis is bracketed by resets on both sides.
    #[test]
    fn truncate_to_width_preserves_styling_and_brackets_the_ellipsis() {
        let styled = format!("\x1b[31m{}\x1b[0m", "hello ".repeat(1000));
        let truncated = truncate_to_width(&styled, 20, "\u{2026}", false);
        assert!(visible_width(&truncated) <= 20);
        assert!(truncated.contains("\x1b[31m"));
        assert!(truncated.ends_with("\x1b[0m\u{2026}\x1b[0m"));
    }

    /// `truncate-to-width.test.ts:24-29`: a BEL-terminated OSC 8 link is
    /// closed before the reset and the ellipsis.
    #[test]
    fn truncate_to_width_closes_a_bel_terminated_osc8_link() {
        let open = "\x1b]8;;https://example.com\x07";
        let close = "\x1b]8;;\x07";
        let text = format!("{open}some-longer-label-here{close}");
        assert_eq!(
            truncate_to_width(&text, 15, "...", false),
            format!("{open}some-longer-{close}\x1b[0m...\x1b[0m")
        );
    }

    /// `truncate-to-width.test.ts:24-29`, ST-terminated: the close must use
    /// the same terminator the open used, or the terminal keeps the link on.
    #[test]
    fn truncate_to_width_closes_an_st_terminated_osc8_link() {
        let open = "\x1b]8;;https://example.com\x1b\\";
        let close = "\x1b]8;;\x1b\\";
        let text = format!("{open}some-longer-label-here{close}");
        assert_eq!(
            truncate_to_width(&text, 15, "...", false),
            format!("{open}some-longer-{close}\x1b[0m...\x1b[0m")
        );
    }

    /// `truncate-to-width.test.ts:32-37`: a bare `ESC` that starts nothing.
    #[test]
    fn truncate_to_width_survives_malformed_ansi_prefixes() {
        let text = format!("abc\x1bnot-ansi {}", "\u{1f642}".repeat(1000));
        let truncated = truncate_to_width(&text, 20, "\u{2026}", false);
        assert!(visible_width(&truncated) <= 20);
    }

    /// `truncate-to-width.test.ts:39-41`.
    #[test]
    fn truncate_to_width_clips_a_wide_ellipsis() {
        assert_eq!(truncate_to_width("abcdef", 1, "\u{1f642}", false), "");
        assert_eq!(
            truncate_to_width("abcdef", 2, "\u{1f642}", false),
            "\x1b[0m\u{1f642}\x1b[0m"
        );
        assert!(visible_width(&truncate_to_width("abcdef", 2, "\u{1f642}", false)) <= 2);
    }

    /// `truncate-to-width.test.ts:44-47`: fitting text is returned untouched,
    /// even when the ellipsis would not have fit.
    #[test]
    fn truncate_to_width_returns_fitting_text_untouched() {
        assert_eq!(truncate_to_width("a", 2, "\u{1f642}", false), "a");
        assert_eq!(
            truncate_to_width("\u{754c}", 2, "\u{1f642}", false),
            "\u{754c}"
        );
    }

    /// `truncate-to-width.test.ts:49-52`.
    #[test]
    fn truncate_to_width_pads_to_the_requested_width() {
        let truncated = truncate_to_width(
            "\u{1f642}\u{754c}\u{1f642}\u{754c}\u{1f642}\u{754c}",
            8,
            "\u{2026}",
            true,
        );
        assert_eq!(visible_width(&truncated), 8);
    }

    /// `truncate-to-width.test.ts:54-58`: a truncation without an ellipsis
    /// still ends in a reset.
    #[test]
    fn truncate_to_width_adds_a_trailing_reset_without_an_ellipsis() {
        let text = format!("\x1b[31m{}", "hello".repeat(100));
        let truncated = truncate_to_width(&text, 10, "", false);
        assert!(visible_width(&truncated) <= 10);
        assert!(truncated.ends_with("\x1b[0m"));
    }

    /// `truncate-to-width.test.ts:60-63`: the kept prefix is contiguous — a
    /// wide grapheme that does not fit ends the keep, it is not skipped.
    #[test]
    fn truncate_to_width_keeps_a_contiguous_prefix() {
        assert_eq!(
            truncate_to_width("\u{1f642}\t\u{754c} \x1b_abc\x07", 7, "\u{2026}", true),
            "\u{1f642}\t\x1b[0m\u{2026}\x1b[0m "
        );
    }

    /// `utils.ts:520-540`: a prefix that already closed its link needs no
    /// second close; one with no link at all needs none.
    ///
    /// Every expected value here was measured under node from pi's own
    /// `getActiveOsc8Close` (`/tmp/ai-agent/osc8_measure.mjs`).
    #[test]
    fn active_osc8_close_only_closes_an_open_link() {
        assert_eq!(active_osc8_close("plain"), "");
        assert_eq!(active_osc8_close("\x1b]8;;\x07plain"), "");
        assert_eq!(active_osc8_close("\x1b]8;;u\x07a\x1b]8;;\x07"), "");
        assert_eq!(active_osc8_close("\x1b]8;;u\x1b\\a"), "\x1b]8;;\x1b\\");
        assert_eq!(active_osc8_close("\x1b]8;;\x1b\\a"), "");
        // Only the first `;` separates params from url.
        assert_eq!(active_osc8_close("\x1b]8;;http://a;b\x07a"), "\x1b]8;;\x07");
    }

    /// `utils.ts:497-499`: a body with no `;` separator is `undefined`, which
    /// leaves the active link exactly where it was. A malformed sequence is
    /// not a close — pi keeps the link open and emits a close for it.
    /// (audit-12 4.6; measured under node, `/tmp/ai-agent/osc8_measure.mjs`.)
    #[test]
    fn a_malformed_osc8_body_leaves_the_active_link_untouched() {
        // No link open yet, so there is still nothing to close.
        assert_eq!(active_osc8_close("\x1b]8;x\x07a"), "");
        assert_eq!(active_osc8_close("\x1b]8;x\x1b\\a"), "");
        assert_eq!(active_osc8_close("\x1b]8;id=7\x07a"), "");
        // An empty body is malformed too: `\x1b]8;` has no separator.
        assert_eq!(active_osc8_close("\x1b]8;\x07a"), "");
        assert_eq!(active_osc8_close("\x1b]8;\x1b\\a"), "");
        // The audited case: an open link followed by a malformed one keeps
        // the open link, and the close carries *its* terminator.
        assert_eq!(
            active_osc8_close("\x1b]8;;https://x\x07label\x1b]8;x\x07"),
            "\x1b]8;;\x07"
        );
        assert_eq!(
            active_osc8_close("\x1b]8;;https://x\x1b\\label\x1b]8;x\x1b\\"),
            "\x1b]8;;\x1b\\"
        );
        // A malformed sequence does not close an open link; only `;url` with
        // an empty url does.
        assert_eq!(
            active_osc8_close("\x1b]8;;https://x\x07label\x1b]8;\x07"),
            "\x1b]8;;\x07"
        );
    }

    /// `utils.ts:563-567`: the tracker assigns every non-`undefined` parse
    /// answer, so a close clears the link while a malformed body leaves it.
    #[test]
    fn tracker_clears_its_link_on_a_close_and_keeps_it_on_a_malformed_body() {
        let mut tracker = AnsiCodeTracker::default();
        tracker.process("\x1b]8;;https://x\x07");
        assert_eq!(tracker.active_link(), Some("https://x"));
        // An SGR reset does not touch the hyperlink (`utils.ts:703-705`).
        tracker.process("\x1b[0m");
        assert_eq!(tracker.active_link(), Some("https://x"));
        // A malformed body is not a close.
        tracker.process("\x1b]8;x\x07");
        assert_eq!(tracker.active_link(), Some("https://x"));
        tracker.process("\x1b]8;\x07");
        assert_eq!(tracker.active_link(), Some("https://x"));
        // A close is: `\x1b]8;;` with an empty url.
        tracker.process("\x1b]8;;\x07");
        assert_eq!(tracker.active_link(), None);
        assert!(!tracker.has_active_codes());
    }

    /// The same six measured rows through the full truncate path
    /// (`finalizeTruncatedResult`, `utils.ts:154-175`).
    #[test]
    fn finalize_truncated_result_matches_pi_on_malformed_osc8_bodies() {
        // measured: "abc\x1b[0m   ", "abc\x1b[0m...\x1b[0m  "
        assert_eq!(
            finalize_truncated_result("abc", 3, "", 0, 6, true),
            "abc\x1b[0m   "
        );
        assert_eq!(
            finalize_truncated_result("abc", 3, "...", 3, 8, true),
            "abc\x1b[0m...\x1b[0m  "
        );
        // measured: "\x1b]8;;u\x1b\\abc\x1b]8;;\x1b\\\x1b[0m...\x1b[0m"
        assert_eq!(
            finalize_truncated_result("\x1b]8;;u\x1b\\abc", 3, "...", 3, 8, false),
            "\x1b]8;;u\x1b\\abc\x1b]8;;\x1b\\\x1b[0m...\x1b[0m"
        );
        // measured: "abcdef\x1b[0m...\x1b[0m" (padding saturates at 0)
        assert_eq!(
            finalize_truncated_result("abcdef", 6, "...", 3, 4, true),
            "abcdef\x1b[0m...\x1b[0m"
        );
        // measured: "\x1b]8;;https://x\x07label\x1b]8;x\x07\x1b]8;;\x07\x1b[0m...\x1b[0m"
        assert_eq!(
            finalize_truncated_result(
                "\x1b]8;;https://x\x07label\x1b]8;x\x07",
                5,
                "...",
                3,
                12,
                false
            ),
            "\x1b]8;;https://x\x07label\x1b]8;x\x07\x1b]8;;\x07\x1b[0m...\x1b[0m"
        );
    }

    #[test]
    fn wrap_breaks_on_words_and_long_words() {
        let lines = wrap_text_with_ansi("hello world again", 11);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "hello world");
        assert_eq!(lines[1], "again");

        let long = wrap_text_with_ansi("abcdefghij", 4);
        assert_eq!(long, vec!["abcd", "efgh", "ij"]);

        assert_eq!(wrap_text_with_ansi("", 4), vec![""]);
        assert_eq!(wrap_text_with_ansi("short", 40), vec!["short"]);
    }

    #[test]
    fn wrap_reopens_active_style_on_continuation_lines() {
        let lines = wrap_text_with_ansi("\x1b[31mred words here\x1b[39m", 5);
        assert!(lines.len() >= 2);
        assert!(lines[1].starts_with("\x1b[31m"), "second line reopens red");
    }

    #[test]
    fn wrap_handles_hard_newlines() {
        let lines = wrap_text_with_ansi("a\nb", 10);
        assert_eq!(lines, vec!["a", "b"]);
    }

    #[test]
    fn tracker_tracks_sgr_state() {
        let mut tracker = AnsiCodeTracker::default();
        tracker.process("\x1b[1m");
        tracker.process("\x1b[4m");
        tracker.process("\x1b[38;2;10;20;30m");
        assert_eq!(tracker.active_codes(), "\x1b[1;4;38;2;10;20;30m");
        assert!(tracker.has_active_codes());
        assert_eq!(tracker.line_end_reset(), "\x1b[24m");
        tracker.process("\x1b[0m");
        assert_eq!(tracker.active_codes(), "");
        assert!(!tracker.has_active_codes());
    }

    #[test]
    fn tracker_keeps_background_out_of_line_end_reset() {
        let mut tracker = AnsiCodeTracker::default();
        tracker.process("\x1b[48;5;240m");
        assert_eq!(tracker.active_background_code(), "\x1b[48;5;240m");
        assert_eq!(tracker.line_end_reset(), "");
        assert_eq!(
            active_background_ansi("\x1b[48;5;240mtext"),
            "\x1b[48;5;240m"
        );
    }

    #[test]
    fn normalize_expands_tabs_only() {
        assert_eq!(normalize_terminal_output("a\tb"), "a   b");
        assert_eq!(normalize_terminal_output("\x1b[1m\t"), "\x1b[1m   ");
    }

    #[test]
    fn apply_background_pads_before_styling() {
        let styled = apply_background_to_line("ab", 5, &|text| format!("[{text}]"));
        assert_eq!(styled, "[ab   ]");
    }

    #[test]
    fn grapheme_cell_range_finds_wide_cluster() {
        // The range is in columns: `a` is 0..1, `日` is 1..3, `b` is 3..4.
        let line = "a日b";
        assert_eq!(grapheme_cell_range(line, 0), Some((0, 1)));
        assert_eq!(grapheme_cell_range(line, 1), Some((1, 3)));
        assert_eq!(grapheme_cell_range(line, 3), Some((3, 4)));
        assert_eq!(grapheme_cell_range(line, 4), None);
        // Escapes are skipped and never counted as columns.
        let styled = "\x1b[31ma日b\x1b[39m";
        assert_eq!(grapheme_cell_range(styled, 1), Some((1, 3)));
    }
}
