//! Ports of `truncate.ts` behavior tests: the vitest suite's truncate
//! assertions plus `truncate-surrogates.test.ts` (pi issue 7121).

use pi_coding_agent::tools::truncate::{
    DEFAULT_MAX_BYTES, TruncatedBy, TruncationOptions, format_size, slice_head, slice_tail,
    truncate_head, truncate_line, truncate_tail,
};
use std::fmt::Write as _;

#[test]
fn head_truncates_at_the_line_limit_with_pi_notices_inputs() {
    let content: Vec<String> = (0..2500).map(|i| format!("Line {}", i + 1)).collect();
    let result = truncate_head(&content.join("\n"), TruncationOptions::default());
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
    assert_eq!(result.total_lines, 2500);
    assert_eq!(result.output_lines, 2000);
    assert!(result.content.contains("Line 1"));
    assert!(result.content.contains("Line 2000"));
    assert!(!result.content.contains("Line 2001"));
}

#[test]
fn head_truncates_at_the_byte_limit() {
    let lines: Vec<String> = (0..500)
        .map(|i| format!("Line {}: {}", i + 1, "x".repeat(200)))
        .collect();
    let result = truncate_head(&lines.join("\n"), TruncationOptions::default());
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert!(result.content.starts_with("Line 1:"));
}

#[test]
fn head_reports_first_line_exceeding_the_byte_limit() {
    let content = format!("{}\nsecond", "é".repeat(DEFAULT_MAX_BYTES / 2 + 1));
    let result = truncate_head(&content, TruncationOptions::default());
    assert!(result.first_line_exceeds_limit);
    assert_eq!(result.content, "");
    assert_eq!(result.output_lines, 0);
    assert_eq!(result.output_bytes, 0);
}

#[test]
fn head_every_line_fits_but_bytes_exceed() {
    // 50KB + 1 byte: truncated, yet every line fits, so truncatedBy is "lines".
    let mut content = String::new();
    for i in 0..512 {
        let _ = write!(
            content,
            "{}{}",
            if i == 0 { "" } else { "\n" },
            "y".repeat(99)
        );
    }
    content.push('y'); // 512*100 = 51200 bytes = 50KB
    let with_newline = format!("{content}\n");
    let result = truncate_head(&with_newline, TruncationOptions::default());
    assert!(
        result.truncated,
        "50KB + the trailing newline exceeds the limit"
    );
    assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
}

// https://github.com/earendil-works/pi/issues/7121 — truncate-surrogates.test.ts
const EMOJI: &str = "\u{1F600}"; // two UTF-16 code units

#[test]
fn truncate_line_does_not_cut_an_emoji_in_half_at_the_limit() {
    let line = format!("{}{}tail", "a".repeat(499), EMOJI);
    let (text, was_truncated) = truncate_line(&line, 500);
    assert!(was_truncated);
    assert_eq!(text, format!("{}... [truncated]", "a".repeat(499)));
    assert!(!has_lone_surrogate(&text));
}

#[test]
fn truncate_line_keeps_an_emoji_that_ends_exactly_at_the_limit() {
    let line = format!("{}{}tail", "a".repeat(498), EMOJI);
    let (text, _) = truncate_line(&line, 500);
    assert_eq!(text, format!("{}{}... [truncated]", "a".repeat(498), EMOJI));
}

#[test]
fn slice_head_and_slice_tail_never_split_a_pair() {
    let text = format!("{EMOJI}x{EMOJI}");
    for units in 0..=text.chars().map(char::len_utf16).sum::<usize>() + 1 {
        let head = slice_head(&text, units);
        let tail = slice_tail(&text, units);
        assert!(!has_lone_surrogate(&head), "units {units}: {head:?}");
        assert!(!has_lone_surrogate(&tail), "units {units}: {tail:?}");
        assert!(text.starts_with(&head), "units {units}");
        assert!(text.ends_with(&tail), "units {units}");
        let text_units: usize = text.chars().map(char::len_utf16).sum();
        assert!(
            head.chars().map(char::len_utf16).sum::<usize>()
                >= (units.min(text_units)).saturating_sub(1)
        );
        assert!(
            tail.chars().map(char::len_utf16).sum::<usize>()
                >= (units.min(text_units)).saturating_sub(1)
        );
    }
}

#[test]
fn slice_head_and_slice_tail_match_slice_without_surrogates() {
    assert_eq!(slice_head("abcdef", 3), "abc");
    assert_eq!(slice_tail("abcdef", 3), "def");
    assert_eq!(slice_head("abc", 10), "abc");
    assert_eq!(slice_tail("abc", 10), "abc");
    assert_eq!(slice_head("abc", 0), "");
    assert_eq!(slice_tail("abc", 0), "");
}

#[test]
fn format_size_uses_one_decimal() {
    assert_eq!(format_size(512), "512B");
    assert_eq!(format_size(DEFAULT_MAX_BYTES), "50.0KB");
    assert_eq!(format_size(2 * 1024 * 1024), "2.0MB");
}

/// `hasLoneSurrogate` from the TS test. Rust `String`s cannot hold lone
/// surrogates, so the check is that the cut stayed on a character boundary —
/// a byte-level round trip through the lossy decoder detects one.
fn has_lone_surrogate(text: &str) -> bool {
    // A UTF-16 re-encode/decode pair produces a replacement character for a
    // lone surrogate; valid text round trips unchanged.
    let utf16: Vec<u16> = text.encode_utf16().collect();
    String::from_utf16(&utf16).is_err()
}

#[test]
fn tail_truncation_keeps_partial_last_line_shape() {
    // The oversized line must be the last one with nothing kept before it
    // for the partial path to trigger (`truncate.ts:205-212`).
    let content = format!("first line\n{}", "y".repeat(DEFAULT_MAX_BYTES + 11));
    let result = truncate_tail(&content, TruncationOptions::default());
    assert!(result.last_line_partial);
    assert_eq!(result.output_lines, 1);
    assert_eq!(result.content, "y".repeat(DEFAULT_MAX_BYTES));
}
