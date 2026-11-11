//! Ports of the read tool tests from `tools.test.ts` and the reference
//! comparisons from `read-streaming.test.ts` (the whole-file algorithm is
//! the reference both paths must match).

mod common;

use common::{TempDir, signal, text_of};
use pi_agent::types::AbortSignal;
use pi_coding_agent::tools::read;
use serde_json::json;
use std::fmt::Write as _;

async fn read_file(
    path: &std::path::Path,
    offset: Option<f64>,
    limit: Option<f64>,
) -> Result<String, String> {
    let context = pi_coding_agent::tools::ToolContext::new("/tmp");
    let mut arguments = json!({ "path": path.display().to_string() });
    if let Some(offset) = offset {
        arguments["offset"] = json!(offset);
    }
    if let Some(limit) = limit {
        arguments["limit"] = json!(limit);
    }
    let result = read::execute(&arguments, &context, &AbortSignal::new()).await;
    result
        .map(|result| text_of(&result))
        .map_err(|error| error.0)
}

#[tokio::test]
async fn reads_file_contents_that_fit_within_limits() {
    let dir = TempDir::new("read");
    let test_file = dir.write("test.txt", "Hello, world!\nLine 2\nLine 3");
    let output = read_file(&test_file, None, None).await.expect("reads");
    assert_eq!(output, "Hello, world!\nLine 2\nLine 3");
    assert!(!output.contains("Use offset="));
}

#[tokio::test]
async fn handles_non_existent_files() {
    let dir = TempDir::new("read-missing");
    let missing = dir.join("nonexistent.txt");
    let error = read_file(&missing, None, None).await.unwrap_err();
    assert!(
        error.to_lowercase().contains("enoent") || error.to_lowercase().contains("not found"),
        "{error}"
    );
}

#[tokio::test]
async fn truncates_files_exceeding_line_limit() {
    let dir = TempDir::new("read-lines");
    let lines: Vec<String> = (0..2500).map(|i| format!("Line {}", i + 1)).collect();
    let test_file = dir.write("large.txt", &lines.join("\n"));
    let output = read_file(&test_file, None, None).await.expect("reads");
    assert!(output.contains("Line 1"));
    assert!(output.contains("Line 2000"));
    assert!(!output.contains("Line 2001"));
    assert!(output.contains("[Showing lines 1-2000 of 2500. Use offset=2001 to continue.]"));
}

#[tokio::test]
async fn truncates_when_byte_limit_exceeded() {
    let dir = TempDir::new("read-bytes");
    let lines: Vec<String> = (0..500)
        .map(|i| format!("Line {}: {}", i + 1, "x".repeat(200)))
        .collect();
    let test_file = dir.write("large-bytes.txt", &lines.join("\n"));
    let output = read_file(&test_file, None, None).await.expect("reads");
    assert!(output.contains("Line 1:"));
    let pattern = regex_lite();
    assert!(pattern.is_match(&output), "{output}");
}

#[tokio::test]
async fn handles_offset_parameter() {
    let dir = TempDir::new("read-offset");
    let lines: Vec<String> = (0..100).map(|i| format!("Line {}", i + 1)).collect();
    let test_file = dir.write("offset-test.txt", &lines.join("\n"));
    let output = read_file(&test_file, Some(51.0), None)
        .await
        .expect("reads");
    assert!(!output.contains("Line 50"));
    assert!(output.contains("Line 51"));
    assert!(output.contains("Line 100"));
    assert!(!output.contains("Use offset="));
}

#[tokio::test]
async fn handles_limit_parameter() {
    let dir = TempDir::new("read-limit");
    let lines: Vec<String> = (0..100).map(|i| format!("Line {}", i + 1)).collect();
    let test_file = dir.write("limit-test.txt", &lines.join("\n"));
    let output = read_file(&test_file, None, Some(10.0))
        .await
        .expect("reads");
    assert!(output.contains("Line 1"));
    assert!(output.contains("Line 10"));
    assert!(!output.contains("Line 11"));
    assert!(output.contains("[90 more lines in file. Use offset=11 to continue.]"));
}

#[tokio::test]
async fn handles_offset_plus_limit_together() {
    let dir = TempDir::new("read-range");
    let lines: Vec<String> = (0..100).map(|i| format!("Line {}", i + 1)).collect();
    let test_file = dir.write("offset-limit-test.txt", &lines.join("\n"));
    let output = read_file(&test_file, Some(41.0), Some(20.0))
        .await
        .expect("reads");
    assert!(!output.contains("Line 40"));
    assert!(output.contains("Line 41"));
    assert!(output.contains("Line 60"));
    assert!(!output.contains("Line 61"));
    assert!(output.contains("[40 more lines in file. Use offset=61 to continue.]"));
}

#[tokio::test]
async fn shows_error_when_offset_is_beyond_file_length() {
    let dir = TempDir::new("read-short");
    let test_file = dir.write("short.txt", "Line 1\nLine 2\nLine 3");
    let error = read_file(&test_file, Some(100.0), None).await.unwrap_err();
    assert!(
        error.contains("Offset 100 is beyond end of file (3 lines total)"),
        "{error}"
    );
}

#[tokio::test]
async fn includes_truncation_details_when_truncated() {
    let dir = TempDir::new("read-details");
    let lines: Vec<String> = (0..2500).map(|i| format!("Line {}", i + 1)).collect();
    let test_file = dir.write("large-file.txt", &lines.join("\n"));
    let context = pi_coding_agent::tools::ToolContext::new("/tmp");
    let arguments = json!({ "path": test_file.display().to_string() });
    let result = read::execute(&arguments, &context, &signal())
        .await
        .expect("reads");
    let truncation = &result.details["truncation"];
    assert_eq!(truncation["truncated"], json!(true));
    assert_eq!(truncation["truncatedBy"], json!("lines"));
    assert_eq!(truncation["totalLines"], json!(2500));
    assert_eq!(truncation["outputLines"], json!(2000));
}

#[tokio::test]
async fn detects_image_mime_type_from_file_magic_not_extension() {
    let dir = TempDir::new("read-image");
    let png_base64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAX+XDSwAAAABJRU5ErkJggg==";
    let png_bytes = base64_decode(png_base64);
    let test_file = dir.join("image.txt");
    std::fs::write(&test_file, &png_bytes).expect("written");
    let context = pi_coding_agent::tools::ToolContext::new("/tmp");
    let arguments = json!({ "path": test_file.display().to_string() });
    let result = read::execute(&arguments, &context, &signal())
        .await
        .expect("reads");
    let text = text_of(&result);
    assert!(text.contains("Read image file [image/png]"), "{text}");
    let image = result.content.iter().find_map(|block| match block {
        pi_ai::types::ContentBlock::Image(image) => Some(image),
        _ => None,
    });
    let image = image.expect("image block present");
    assert_eq!(image.mime_type, "image/png");
    assert_ne!(image.data, "");
    // Programmatic callers get the same image as a block.
    let structured = result.structured_content.expect("structured content");
    assert_eq!(structured["type"], json!("image"));
    assert_eq!(structured["mimeType"], json!("image/png"));
    assert_eq!(structured["note"], json!(text));
    assert_eq!(structured["data"], json!(image.data));
}

#[tokio::test]
async fn treats_files_with_image_extension_but_non_image_content_as_text() {
    let dir = TempDir::new("read-fake-image");
    let test_file = dir.write("not-an-image.png", "definitely not a png");
    let output = read_file(&test_file, None, None).await.expect("reads");
    assert!(output.contains("definitely not a png"));
}

#[tokio::test]
async fn reads_local_files_the_same_way_the_reference_algorithm_does() {
    let dir = TempDir::new("read-reference");
    let mut text = String::new();
    for i in 0..3000 {
        let _ = write!(text, "{} {}", i, "w".repeat(i % 40));
        text.push('\n');
    }
    let path = dir.write("local.txt", &text);
    for (offset, limit) in [(None, None), (Some(1500.0), Some(10.0))] {
        let output = read_file(&path, offset, limit).await.expect("reads");
        assert_eq!(
            output,
            reference_read(&text, &path.display().to_string(), offset, limit),
            "offset {offset:?} limit {limit:?}"
        );
    }
    // A directory read surfaces the filesystem error pi lets escape.
    let error = read_file(dir.path(), None, None).await.unwrap_err();
    assert!(error.contains("EISDIR"), "{error}");
}

#[tokio::test]
async fn streaming_selection_matches_the_reference_algorithm_on_boundary_cases() {
    // The TEXT_CASES of read-streaming.test.ts, compared against the
    // reference algorithm the way that suite does.
    let cases: Vec<(&str, String)> = vec![
        ("empty", String::new()),
        ("no trailing newline", "alpha\nbeta\ngamma".to_string()),
        ("trailing newline", "alpha\nbeta\n".to_string()),
        ("CRLF", "alpha\r\nbeta\r\n\r\ngamma\r\n".to_string()),
        (
            "multi-byte and BOM",
            "\u{FEFF}é€\u{1F600}\nñ\u{10348}字\n\u{1F600}\u{1F600}\u{1F600}".to_string(),
        ),
        (
            "2001 lines",
            (0..2001)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        (
            "2001 lines, an empty line and more",
            format!(
                "{}\n\nmore",
                (0..2001)
                    .map(|i| format!("line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        ),
        (
            "first line over 50KB",
            format!(
                "{}\nsecond",
                "é".repeat(pi_coding_agent::tools::truncate::DEFAULT_MAX_BYTES / 2 + 1)
            ),
        ),
        (
            "line over 50KB after short lines",
            format!("a\nb\n{}\nc", "z".repeat(60 * 1024)),
        ),
    ];
    let dir = TempDir::new("read-boundaries");
    for (name, text) in cases {
        let path = dir.write("case.txt", &text);
        let total = text.split('\n').count() as f64;
        for offset in [None, Some(2.0), Some(3.0), Some(total), Some(total + 1.0)] {
            for limit in [None, Some(0.0), Some(1.0), Some(total)] {
                let outcome = read_file(&path, offset, limit).await;
                let expected = reference_outcome(&text, &path.display().to_string(), offset, limit);
                match (&outcome, &expected) {
                    (Ok(text), Ok(expected)) => assert_eq!(
                        text, expected,
                        "case {name}, offset {offset:?}, limit {limit:?}"
                    ),
                    (Err(error), Err(expected)) => assert_eq!(
                        error, expected,
                        "case {name}, offset {offset:?}, limit {limit:?}"
                    ),
                    _ => panic!(
                        "case {name}, offset {offset:?}, limit {limit:?}: {outcome:?} vs {expected:?}"
                    ),
                }
            }
        }
    }
}

/// The read tool's text path before it streamed (pi #9654), kept as the
/// reference the streaming path must match — the TS test's `referenceRead`.
fn reference_read(
    text_content: &str,
    path: &str,
    offset: Option<f64>,
    limit: Option<f64>,
) -> String {
    let all_lines: Vec<&str> = text_content.split('\n').collect();
    let total_file_lines = all_lines.len();
    let start_line = offset
        .filter(|offset| *offset != 0.0)
        .map_or(0usize, |offset| ((offset - 1.0).max(0.0)) as usize);
    let start_line_display = start_line + 1;
    if start_line >= all_lines.len() {
        return format!(
            "Offset {} is beyond end of file ({total_file_lines} lines total)",
            display(offset)
        );
    }
    let end_line = limit.map_or(all_lines.len() as i64, |limit| {
        ((start_line as f64 + limit).min(total_file_lines as f64)) as i64
    });
    let selected = js_slice(&all_lines, start_line as i64, end_line);
    let truncation = pi_coding_agent::tools::truncate::truncate_head(
        &selected.join("\n"),
        pi_coding_agent::tools::truncate::TruncationOptions::default(),
    );
    let user_limited_lines = limit.map(|limit| {
        ((start_line as f64 + limit).min(total_file_lines as f64) - start_line as f64) as i64
    });
    let max_bytes = pi_coding_agent::tools::truncate::DEFAULT_MAX_BYTES;
    if truncation.first_line_exceeds_limit {
        let first_line_size =
            pi_coding_agent::tools::truncate::format_size(all_lines[start_line].len());
        format!(
            "[Line {start_line_display} is {first_line_size}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {path} | head -c {max_bytes}]",
            pi_coding_agent::tools::truncate::format_size(max_bytes)
        )
    } else if truncation.truncated {
        let end_line_display = start_line_display + truncation.output_lines - 1;
        let next_offset = end_line_display + 1;
        let mut output_text = truncation.content.clone();
        if truncation.truncated_by == Some(pi_coding_agent::tools::truncate::TruncatedBy::Lines) {
            let _ = write!(
                output_text,
                "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines}. Use offset={next_offset} to continue.]"
            );
        } else {
            let _ = write!(
                output_text,
                "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines} ({} limit). Use offset={next_offset} to continue.]",
                pi_coding_agent::tools::truncate::format_size(max_bytes)
            );
        }
        output_text
    } else if user_limited_lines
        .is_some_and(|limited| start_line as i64 + limited < total_file_lines as i64)
    {
        let limited = user_limited_lines.expect("checked above");
        let remaining = total_file_lines as i64 - (start_line as i64 + limited);
        let next_offset = start_line as i64 + limited + 1;
        format!(
            "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
            truncation.content
        )
    } else {
        truncation.content.clone()
    }
}

fn reference_outcome(
    text: &str,
    path: &str,
    offset: Option<f64>,
    limit: Option<f64>,
) -> Result<String, String> {
    let all_lines: Vec<&str> = text.split('\n').collect();
    let start_line = offset
        .filter(|offset| *offset != 0.0)
        .map_or(0usize, |offset| ((offset - 1.0).max(0.0)) as usize);
    if start_line >= all_lines.len() {
        return Err(format!(
            "Offset {} is beyond end of file ({} lines total)",
            display(offset),
            all_lines.len()
        ));
    }
    Ok(reference_read(text, path, offset, limit))
}

fn display(offset: Option<f64>) -> String {
    match offset {
        Some(offset) if offset.fract() == 0.0 && offset.abs() < 1e21 => {
            format!("{}", offset as i64)
        }
        Some(offset) => format!("{offset}"),
        None => "undefined".to_string(),
    }
}

fn js_slice<'a>(lines: &[&'a str], start: i64, end: i64) -> Vec<&'a str> {
    let length = lines.len() as i64;
    let mut start = start;
    if start < 0 {
        start = (length + start).max(0);
    }
    let mut end = end;
    if end < 0 {
        end += length;
    }
    let start = start.clamp(0, length) as usize;
    let end = end.clamp(0, length) as usize;
    if start >= end {
        return Vec::new();
    }
    lines[start..end].to_vec()
}

/// The byte-limit notice pattern the byte-limit test asserts:
/// `\[Showing lines 1-\d+ of 500 \(.* limit\)\. Use offset=\d+ to continue\.\]`
fn regex_lite() -> regex::Regex {
    regex::Regex::new(r"\[Showing lines 1-\d+ of 500 \(.* limit\)\. Use offset=\d+ to continue\.\]")
        .expect("pattern parses")
}

fn base64_decode(text: &str) -> Vec<u8> {
    let mut table = [255u8; 256];
    for (index, character) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
        .iter()
        .enumerate()
    {
        table[*character as usize] = index as u8;
    }
    let mut out = Vec::new();
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        let value = table[byte as usize];
        if value == 255 {
            continue;
        }
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    out
}
