//! Shared truncation utilities for tool outputs.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/truncate.ts`.
//!
//! Truncation is based on two independent limits — whichever is hit first wins:
//!
//! * Line limit (default: 2000 lines)
//! * Byte limit (default: 50KB)
//!
//! Never returns partial lines, except for the bash tail edge case where a
//! single line larger than the byte limit is cut at a character boundary.
//!
//! Porting note: pi counts bytes with `Buffer.byteLength`, and indexes strings
//! in UTF-16 code units. Byte counts are identical here. The one place pi
//! slices a string by code unit — [`truncate_line`], [`slice_head`],
//! [`slice_tail`] — counts `len_utf16` and cuts on the same units, so it
//! agrees with pi on every input a Rust `String` can hold. The residual
//! difference is one case Rust cannot express: when the cut lands in the middle
//! of an astral-plane character, pi's `String.prototype.slice` leaves a lone
//! UTF-16 surrogate in the result, while [`slice_head`] / [`slice_tail`] round
//! the cut to the whole character. Only the character straddling the boundary
//! differs; lengths and the `wasTruncated` flag do not.

/// pi's `DEFAULT_MAX_LINES` (`truncate.ts:11`).
pub const DEFAULT_MAX_LINES: usize = 2000;
/// pi's `DEFAULT_MAX_BYTES` (50KB) (`truncate.ts:12`).
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// pi's `GREP_MAX_LINE_LENGTH` (`truncate.ts:13`).
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// pi's `TruncationResult` (`truncate.ts:15-38`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncationResult {
    /// The truncated content.
    pub content: String,
    /// Whether truncation occurred.
    pub truncated: bool,
    /// Which limit was hit, or `None` when not truncated.
    pub truncated_by: Option<TruncatedBy>,
    /// Total number of lines in the original content.
    pub total_lines: usize,
    /// Total number of bytes in the original content.
    pub total_bytes: usize,
    /// Number of complete lines in the truncated output.
    pub output_lines: usize,
    /// Number of bytes in the truncated output.
    pub output_bytes: usize,
    /// Whether the last line was partially truncated (tail edge case only).
    pub last_line_partial: bool,
    /// Whether the first line alone exceeded the byte limit (head truncation).
    pub first_line_exceeds_limit: bool,
    /// The line limit that was applied.
    pub max_lines: usize,
    /// The byte limit that was applied.
    pub max_bytes: usize,
}

/// pi's `truncatedBy` union (`truncate.ts:21`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

/// pi's `TruncationOptions` (`truncate.ts:40-45`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TruncationOptions {
    /// Maximum number of lines (default: 2000).
    pub max_lines: Option<usize>,
    /// Maximum number of bytes (default: 50KB).
    pub max_bytes: Option<usize>,
}

impl TruncationOptions {
    /// `TruncationOptions` with only the line limit set.
    pub fn max_lines(max_lines: usize) -> TruncationOptions {
        TruncationOptions {
            max_lines: Some(max_lines),
            ..TruncationOptions::default()
        }
    }

    fn resolve(self) -> (usize, usize) {
        (
            self.max_lines.unwrap_or(DEFAULT_MAX_LINES),
            self.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
        )
    }
}

/// pi's `splitLinesForCounting` (`truncate.ts:47-56`): a trailing newline does
/// not start a new line.
fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// pi's `formatSize` (`truncate.ts:61-69`).
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn unchanged(
    content: &str,
    lines: &[&str],
    max_lines: usize,
    max_bytes: usize,
) -> TruncationResult {
    TruncationResult {
        content: content.to_string(),
        truncated: false,
        truncated_by: None,
        total_lines: lines.len(),
        total_bytes: content.len(),
        output_lines: lines.len(),
        output_bytes: content.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// pi's `truncateHead` (`truncate.ts:78-160`): keep the first N lines/bytes,
/// never a partial line.
///
/// When the first line alone exceeds the byte limit the content comes back
/// empty with `first_line_exceeds_limit` set.
pub fn truncate_head(content: &str, options: TruncationOptions) -> TruncationResult {
    let (max_lines, max_bytes) = options.resolve();
    let lines = split_lines_for_counting(content);
    let total_bytes = content.len();

    if lines.len() <= max_lines && total_bytes <= max_bytes {
        return unchanged(content, &lines, max_lines, max_bytes);
    }

    let first_line_bytes = lines[0].len();
    if first_line_bytes > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines: lines.len(),
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut kept_bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;

    for (index, line) in lines.iter().enumerate() {
        if index >= max_lines {
            break;
        }
        // +1 for the newline that joins this line to the previous one
        // (`truncate.ts:128`).
        let line_bytes = line.len() + usize::from(index > 0);
        if kept_bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        kept.push(line);
        kept_bytes += line_bytes;
    }

    // If we exited due to the line limit (`truncate.ts:140-142`).
    if kept.len() >= max_lines && kept_bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output = kept.join("\n");
    TruncationResult {
        output_bytes: output.len(),
        output_lines: kept.len(),
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines: lines.len(),
        total_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// pi's `truncateTail` (`truncate.ts:168-241`): keep the last N lines/bytes.
///
/// The one place pi may return a partial line: when not even a single line fits
/// the byte limit, the end of that line is kept (`last_line_partial`).
pub fn truncate_tail(content: &str, options: TruncationOptions) -> TruncationResult {
    let (max_lines, max_bytes) = options.resolve();
    let lines = split_lines_for_counting(content);
    let total_bytes = content.len();

    if lines.len() <= max_lines && total_bytes <= max_bytes {
        return unchanged(content, &lines, max_lines, max_bytes);
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut kept_bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;
    let mut owned_partial: Option<String> = None;

    let mut index = lines.len();
    while index > 0 && kept.len() < max_lines {
        index -= 1;
        let line = lines[index];
        let line_bytes = line.len() + usize::from(!kept.is_empty());
        if kept_bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            // Edge case: nothing kept yet and this line exceeds the byte limit,
            // so take the end of the line (`truncate.ts:207-213`).
            if kept.is_empty() {
                owned_partial = Some(truncate_string_to_bytes_from_end(line, max_bytes));
                last_line_partial = true;
            }
            break;
        }
        kept.insert(0, line);
        kept_bytes += line_bytes;
    }

    if kept.len() >= max_lines && kept_bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let had_partial = owned_partial.is_some();
    let output = match owned_partial {
        // The partial line replaces the tail; nothing was kept with it.
        Some(partial) => partial,
        None => kept.join("\n"),
    };
    TruncationResult {
        output_bytes: output.len(),
        // pi unshifts the partial line into the kept lines, so it counts.
        output_lines: kept.len() + usize::from(had_partial),
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines: lines.len(),
        total_bytes,
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// pi's `truncateStringToBytesFromEnd` (`truncate.ts:247-262`): cut to
/// `max_bytes` from the end, snapping forward to a character boundary.
fn truncate_string_to_bytes_from_end(text: &str, max_bytes: usize) -> String {
    let bytes = text.as_bytes();
    if bytes.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = bytes.len() - max_bytes;
    while start < bytes.len() && is_utf8_continuation(bytes[start]) {
        start += 1;
    }
    text[start..].to_string()
}

/// A UTF-8 continuation byte (`10xxxxxx`) never starts a character.
fn is_utf8_continuation(byte: u8) -> bool {
    byte & 0xc0 == 0x80
}

/// pi's `truncateLine` (`truncate.ts:268-276`): cap one line at `max_chars`
/// UTF-16 code units, with a `... [truncated]` suffix. Returns
/// `(text, was_truncated)`.
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    let head = slice_head(line, max_chars);
    if head.len() == line.len() {
        return (line.to_string(), false);
    }
    (format!("{head}... [truncated]"), true)
}

/// pi's `sliceHead` (`truncate.ts:279-285`): at most the first `max_units`
/// UTF-16 code units, without splitting a surrogate pair.
pub fn slice_head(text: &str, max_units: usize) -> String {
    let mut units = 0usize;
    let mut end = text.len();
    for (offset, ch) in text.char_indices() {
        let width = ch.len_utf16();
        if units + width > max_units {
            end = offset;
            break;
        }
        units += width;
    }
    text[..end].to_string()
}

/// pi's `sliceTail` (`truncate.ts:288-294`): at most the last `max_units`
/// UTF-16 code units, without splitting a surrogate pair.
pub fn slice_tail(text: &str, max_units: usize) -> String {
    if max_units == 0 {
        return String::new();
    }
    // Walk back accepting whole characters while they fit; `start` tracks the
    // earliest character accepted, so an empty acceptance leaves `start` at
    // the end and returns "".
    let mut units = 0usize;
    let mut start = text.len();
    for (offset, ch) in text.char_indices().rev() {
        let width = ch.len_utf16();
        if units + width > max_units {
            break;
        }
        units += width;
        start = offset;
    }
    text[start..].to_string()
}

/// The truncation report as the tools put it in `details`; field order matches
/// the TS object literal, which session files serialize.
pub fn truncation_json(truncation: &TruncationResult) -> serde_json::Value {
    serde_json::json!({
        "content": truncation.content,
        "truncated": truncation.truncated,
        "truncatedBy": match truncation.truncated_by {
            Some(TruncatedBy::Lines) => serde_json::json!("lines"),
            Some(TruncatedBy::Bytes) => serde_json::json!("bytes"),
            None => serde_json::Value::Null,
        },
        "totalLines": truncation.total_lines,
        "totalBytes": truncation.total_bytes,
        "outputLines": truncation.output_lines,
        "outputBytes": truncation.output_bytes,
        "lastLinePartial": truncation.last_line_partial,
        "firstLineExceedsLimit": truncation.first_line_exceeds_limit,
        "maxLines": truncation.max_lines,
        "maxBytes": truncation.max_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_keeps_lines_within_both_limits() {
        let result = truncate_head("a\nb\nc", TruncationOptions::default());
        assert!(!result.truncated);
        assert_eq!(result.content, "a\nb\nc");
        assert_eq!(result.total_lines, 3);
        assert_eq!(result.output_lines, 3);
    }

    #[test]
    fn head_cuts_at_the_line_limit() {
        let content = (0..2500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_head(&content, TruncationOptions::default());
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(result.total_lines, 2500);
        assert_eq!(result.output_lines, 2000);
        // `outputBytes` is the byte length of the returned content
        // (`truncate.ts:145,154`), not the limit that cut it.
        let expected_bytes = (0..2000).map(|i| format!("line {i}").len()).sum::<usize>() + 1999;
        assert_eq!(result.output_bytes, expected_bytes);
    }

    #[test]
    fn head_cuts_at_the_byte_limit_mid_lines() {
        // 200 lines of 1000 bytes cross the 50KB limit between whole lines:
        // 51 lines fit (1000 + 50 * 1001), the 52nd would overflow.
        let content = std::iter::repeat_n("x".repeat(1000), 200)
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_head(&content, TruncationOptions::default());
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
        assert!(!result.first_line_exceeds_limit);
        assert_eq!(result.output_lines, 51);
        assert_eq!(result.output_bytes, 51 * 1000 + 50);
    }

    #[test]
    fn head_reports_a_first_line_over_the_byte_limit() {
        let content = format!("{}\nsecond", "é".repeat(DEFAULT_MAX_BYTES / 2 + 1));
        let result = truncate_head(&content, TruncationOptions::default());
        assert!(result.truncated);
        assert!(result.first_line_exceeds_limit);
        assert_eq!(result.content, "");
        assert_eq!(result.output_lines, 0);
    }

    #[test]
    fn a_trailing_newline_is_not_an_extra_line() {
        let result = truncate_head("a\nb\n", TruncationOptions::default());
        assert_eq!(result.total_lines, 2);
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        let content = "one\ntwo\nthree\nfour";
        let result = truncate_tail(content, TruncationOptions::max_lines(2));
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(result.content, "three\nfour");
        assert_eq!(result.output_lines, 2);
        assert_eq!(result.total_lines, 4);
    }

    #[test]
    fn tail_cuts_at_the_byte_limit() {
        let content = format!("{}\ntail", "x".repeat(DEFAULT_MAX_BYTES));
        let result = truncate_tail(&content, TruncationOptions::default());
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
        assert_eq!(result.content, "tail");
    }

    #[test]
    fn tail_keeps_a_partial_last_line_when_nothing_fits() {
        // The oversized line must be the LAST one and nothing may be kept
        // before it, or the partial path never triggers
        // (`truncate.ts:205-212`).
        let content = format!("first line\n{}", "y".repeat(DEFAULT_MAX_BYTES + 11));
        let result = truncate_tail(&content, TruncationOptions::default());
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
        assert!(result.last_line_partial);
        // The end of the oversized line is what survives.
        assert_eq!(result.content, "y".repeat(DEFAULT_MAX_BYTES));
        assert_eq!(result.output_lines, 1);
    }

    #[test]
    fn format_size_matches_js_to_fixed() {
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(1024), "1.0KB");
        assert_eq!(format_size(DEFAULT_MAX_BYTES), "50.0KB");
        assert_eq!(format_size(1024 * 1024), "1.0MB");
    }

    #[test]
    fn truncate_line_caps_utf16_units() {
        let (text, was) = truncate_line("short", GREP_MAX_LINE_LENGTH);
        assert_eq!(text, "short");
        assert!(!was);
        let long = "a".repeat(600);
        let (text, was) = truncate_line(&long, GREP_MAX_LINE_LENGTH);
        assert!(was);
        assert_eq!(text, format!("{}... [truncated]", "a".repeat(500)));
    }

    #[test]
    fn truncation_json_matches_the_ts_field_order() {
        let result = truncate_head("a\nb", TruncationOptions::default());
        let value = truncation_json(&result);
        let keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            vec![
                "content",
                "truncated",
                "truncatedBy",
                "totalLines",
                "totalBytes",
                "outputLines",
                "outputBytes",
                "lastLinePartial",
                "firstLineExceedsLimit",
                "maxLines",
                "maxBytes"
            ]
        );
    }
}
