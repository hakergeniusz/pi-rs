//! The read tool.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/read.ts`.
//!
//! Behaviors ported:
//!
//! * `offset` is 1-indexed; an offset past the end is an error that names the
//!   file's line count (`read.ts:313-315`);
//! * `limit` is honored first, then the head truncation (2000 lines / 50KB)
//!   decides what else to cut (`read.ts:317-344`);
//! * truncation appends an actionable continuation notice
//!   (`Use offset=N to continue.`), and a user-limited read that stopped early
//!   appends `[N more lines in file. ...]`;
//! * a first line larger than the byte limit does not return a partial line —
//!   it returns pi's `sed -n` fallback instruction (`read.ts:320-323`);
//! * image files are detected by content signature (`utils/mime.ts`) and
//!   returned as `[text note, image block]`, the same shape pi returns.
//!   Image processing goes through [`crate::images::process_image`]
//!   (`utils/image-process.ts`), so resize hints, conversion hints and
//!   omission notes follow upstream exactly.
//!
//! Two paths select lines, exactly as pi does: whole-file in memory for
//! unusual ranges (`read.ts:132-138`), and a streaming reader that stores
//! only the lines that will be shown for streamable ranges
//! (`read.ts:144-229`). A regular file is read up to its size when opened,
//! so text appended during the read is not counted (`read.ts:76-87`).
//!
//! Deviations: the builtin [`ToolContext`] carries no model metadata, so
//! `getNonVisionImageNote(ctx.model)` (`read.ts:274`) always takes its
//! "no model" arm and the conservative fallback resize options are used.
//! Pixel work uses [`crate::images::default_transformer`], the host-side
//! equivalent of pi's Photon backend: images within the inline limits still
//! pass through byte-for-byte, while oversized and BMP images are decoded,
//! resized/converted and attached instead of taking pi's omission paths
//! (`read.ts:282-285`). See `images/WIRING.md` for the mechanism deltas.

use std::fs;
use std::io::Read;

use pi_agent::types::AbortSignal;
use pi_agent::types::AgentToolResult;
use serde_json::{Value, json};

use crate::images::{
    ImageProcessResult, ImageSettings, ResizeOptions, default_transformer, read_failure_note,
    read_image_note, to_image_content,
};
use crate::tools::path_utils::resolve_read_path;
use crate::tools::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncationOptions, TruncationResult, format_size,
    truncate_head,
};
use crate::tools::{
    ToolContext, ToolError, io_error_node_message, optional_f64, optional_str, text_block,
    text_result,
};
use std::fmt::Write as _;

/// pi's read tool description (`read.ts:241`), with the limits filled in.
pub fn description() -> String {
    format!(
        "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent \
as attachments. For text files, output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). \
Use offset/limit for large files. When you need the full file, continue with offset until complete.",
        DEFAULT_MAX_BYTES / 1024
    )
}

/// pi's `readToolSystemPromptContribution` (`read.ts:20-23`).
pub const PROMPT_SNIPPET: &str = "Read file contents";
/// pi's `readToolSystemPromptContribution.guidelines`.
pub const PROMPT_GUIDELINES: [&str; 1] = ["Use read to examine files instead of cat or sed."];

/// pi's `createReadToolDefinition(...).execute` (`read.ts:247-358`).
// A verbatim 1:1 port of the TS function; keeping it in one piece makes the
// side-by-side review against the source possible.
#[allow(clippy::too_many_lines)]
// The tool seam is uniformly `async`; keeping the signature means the
// executor treats every built-in the same way.
#[allow(clippy::unused_async)]
pub async fn execute(
    arguments: &Value,
    context: &ToolContext,
    signal: &AbortSignal,
) -> Result<AgentToolResult, ToolError> {
    if signal.aborted() {
        return Err(ToolError("Operation aborted".to_string()));
    }
    let path = optional_str(arguments, "path")
        .ok_or_else(|| ToolError("Expected required property 'path'".to_string()))?;
    let offset = optional_f64(arguments, "offset");
    let limit = optional_f64(arguments, "limit");

    let absolute_path = resolve_read_path(&path, &context.cwd);
    // Check if file exists and is readable (`read.ts:272`), and let the raw fs
    // error escape with node's message shape.
    fs::File::open(&absolute_path)
        .map_err(|error| ToolError(io_error_node_message(&error, &absolute_path, "access")))?;
    if signal.aborted() {
        return Err(ToolError("Operation aborted".to_string()));
    }

    if let Some(mime_type) = detect_supported_image_mime_type_from_file(&absolute_path) {
        return execute_image(&absolute_path, mime_type);
    }

    // Read text content. Convert offset from 1-indexed input to 0-indexed
    // line access (`read.ts:299-310`).
    let start_line = start_line_of(offset);
    let selected = if is_streamable_range(offset, limit) {
        select_lines_from_stream(&absolute_path, start_line, limit, signal)?
    } else {
        let text = read_text_file(&absolute_path)?;
        select_lines_from_buffer(&text, start_line, limit)
    };
    if signal.aborted() {
        return Err(ToolError("Operation aborted".to_string()));
    }

    let start_line_display = start_line + 1;
    let total_file_lines = selected.total_file_lines;
    let truncation = &selected.truncation;

    // Check if offset is out of bounds (`read.ts:313-315`).
    if start_line >= total_file_lines {
        return Err(ToolError(format!(
            "Offset {} is beyond end of file ({total_file_lines} lines total)",
            display_number(offset.unwrap_or(f64::NAN))
        )));
    }

    // A user-specified limit is honored before truncation (`read.ts:317-318`).
    let user_limited_lines: Option<i64> = limit.map(|limit| {
        ((start_line as f64 + limit).min(total_file_lines as f64) - start_line as f64) as i64
    });
    let output_text = if truncation.first_line_exceeds_limit {
        // First line alone exceeds the byte limit. Point the model at a bash
        // fallback (`read.ts:320-323`).
        let first_line_size = format_size(selected.first_line_bytes);
        format!(
            "[Line {start_line_display} is {first_line_size}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {path} | head -c {DEFAULT_MAX_BYTES}]",
            format_size(DEFAULT_MAX_BYTES)
        )
    } else if truncation.truncated {
        // Truncation occurred. Build an actionable continuation notice
        // (`read.ts:325-335`).
        let end_line_display = start_line_display + truncation.output_lines - 1;
        let next_offset = end_line_display + 1;
        let mut text = truncation.content.clone();
        if truncation.truncated_by == Some(crate::tools::truncate::TruncatedBy::Lines) {
            let _ = write!(
                text,
                "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines}. Use offset={next_offset} to continue.]"
            );
        } else {
            let _ = write!(
                text,
                "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines} ({} limit). Use offset={next_offset} to continue.]",
                format_size(DEFAULT_MAX_BYTES)
            );
        }
        text
    } else if user_limited_lines
        .is_some_and(|limited| start_line as i64 + limited < total_file_lines as i64)
    {
        // User-specified limit stopped early, but the file still has more
        // content (`read.ts:336-340`).
        let limited = user_limited_lines.expect("checked above");
        let remaining = total_file_lines as i64 - (start_line as i64 + limited);
        let next_offset = start_line as i64 + limited + 1;
        format!(
            "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
            truncation.content
        )
    } else {
        // No truncation and no remaining user-limited content (`read.ts:343`).
        truncation.content.clone()
    };

    // `details = { truncation }` only for the two truncation paths
    // (`read.ts:324`, `read.ts:335`).
    let details = if truncation.first_line_exceeds_limit || truncation.truncated {
        json!({ "truncation": crate::tools::truncate::truncation_json(truncation) })
    } else {
        json!({})
    };

    Ok(AgentToolResult {
        details,
        structured_content: Some(serde_json::Value::String(output_text.clone())),
        ..text_result(output_text)
    })
}

/// The image half of pi's execute (`read.ts:278-297`).
fn execute_image(
    absolute_path: &std::path::Path,
    mime_type: &str,
) -> Result<AgentToolResult, ToolError> {
    let bytes = fs::read(absolute_path)
        .map_err(|error| ToolError(io_error_node_message(&error, absolute_path, "open")))?;
    // No model metadata reaches a builtin tool, so upstream's
    // `getNonVisionImageNote(ctx.model)` takes its no-model arm and
    // `fallbackResizeOptions` applies. Pixel work uses the host-side Photon
    // replacement so oversized/BMP images become real attachments.
    match crate::images::process_image(
        &bytes,
        mime_type,
        &ImageSettings::default(),
        &ResizeOptions::default(),
        default_transformer(),
    ) {
        ImageProcessResult::Failed { message } => {
            let note = read_failure_note(mime_type, &message, false);
            Ok(AgentToolResult {
                structured_content: Some(json!(note)),
                ..text_result(note)
            })
        }
        ImageProcessResult::Processed {
            data,
            mime_type: processed_mime_type,
            hints,
        } => {
            let note = read_image_note(&processed_mime_type, &hints, false);
            // pi's `toReadOutput` (`read.ts:99-103`): the image shape for
            // programmatic callers.
            Ok(AgentToolResult {
                content: vec![
                    text_block(note.clone()),
                    pi_ai::types::ContentBlock::Image(to_image_content(
                        data.clone(),
                        processed_mime_type.clone(),
                    )),
                ],
                structured_content: Some(json!({
                    "type": "image",
                    "data": data,
                    "mimeType": processed_mime_type,
                    "note": note,
                })),
                ..AgentToolResult::default()
            })
        }
    }
}

/// pi's `SelectedLines` (`read.ts:113-118`): the lines a read shows, with the
/// file's line count.
struct SelectedLines {
    total_file_lines: usize,
    truncation: TruncationResult,
    /// Bytes of the first selected line, for the notice when it alone exceeds
    /// the byte limit.
    first_line_bytes: usize,
}

/// pi's `isStreamableRange` (`read.ts:124-129`): other values (fractional,
/// negative limit) keep the whole-file path, whose array slicing defines what
/// they select, for example limit -1 as all lines but the last.
fn is_streamable_range(offset: Option<f64>, limit: Option<f64>) -> bool {
    let offset_ok = offset.is_none_or(|offset| offset.fract() == 0.0 && offset.is_finite());
    let limit_ok =
        limit.is_none_or(|limit| limit.fract() == 0.0 && limit.is_finite() && limit >= 0.0);
    offset_ok && limit_ok
}

/// pi's `const startLine = offset ? Math.max(0, offset - 1) : 0`
/// (`read.ts:300`), as a 0-indexed line.
fn start_line_of(offset: Option<f64>) -> usize {
    match offset {
        Some(offset) if offset != 0.0 => (offset - 1.0).max(0.0) as usize,
        _ => 0,
    }
}

/// JS number formatting for the places pi interpolates an argument into a
/// message (`Offset ${offset} ...`).
fn display_number(value: f64) -> String {
    if value.is_nan() {
        "undefined".to_string()
    } else if value.fract() == 0.0 && value.abs() < 1e21 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// pi's `selectLinesFromBuffer` (`read.ts:132-138`): select lines from a
/// whole file in memory, for unusual ranges.
fn select_lines_from_buffer(text: &str, start_line: usize, limit: Option<f64>) -> SelectedLines {
    let all_lines: Vec<&str> = text.split('\n').collect();
    let end_line = limit.map_or(all_lines.len() as i64, |limit| {
        ((start_line as f64 + limit).min(all_lines.len() as f64)) as i64
    });
    let selected = js_slice(&all_lines, start_line as i64, end_line);
    let truncation = truncate_head(&selected.join("\n"), TruncationOptions::default());
    let first_line_bytes = if truncation.first_line_exceeds_limit {
        all_lines.get(start_line).map_or(0, |line| line.len())
    } else {
        0
    };
    SelectedLines {
        total_file_lines: all_lines.len(),
        truncation,
        first_line_bytes,
    }
}

/// JS `Array.prototype.slice(start, end)` with the negative-index and clamp
/// semantics the whole-file path depends on (`read.ts:134-135`).
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

/// pi's `selectLinesFromStream` (`read.ts:144-229`): select lines from a file
/// read in chunks. Only lines truncateHead can show are stored; the rest are
/// counted, so memory does not grow with the file. Stops reading when the
/// signal aborts.
// A verbatim 1:1 port of the TS function; keeping it in one piece makes the
// side-by-side review against the source possible.
#[allow(clippy::too_many_lines)]
fn select_lines_from_stream(
    path: &std::path::Path,
    start_line: usize,
    limit: Option<f64>,
    signal: &AbortSignal,
) -> Result<SelectedLines, ToolError> {
    let end_line: u64 = limit.map_or(u64::MAX, |limit| {
        (start_line as u64).saturating_add(limit as u64)
    });

    let mut file = fs::File::open(path)
        .map_err(|error| ToolError(io_error_node_message(&error, path, "open")))?;
    let metadata = file
        .metadata()
        .map_err(|error| ToolError(error.to_string()))?;
    // A regular file is read up to its size when opened (`read.ts:80`), so
    // text appended during the read is not counted; a file reporting size 0
    // (such as /proc files) and other file types run until EOF.
    let byte_limit = if metadata.is_file() && metadata.len() > 0 {
        Some(metadata.len())
    } else {
        None
    };

    let mut stored_lines: Vec<String> = Vec::new();
    let mut stored_bytes = 0usize;
    let mut stored_past_caps = false;
    let mut line = 0u64;
    let mut selected_lines = 0usize;
    let mut selected_bytes = 0usize;
    let mut first_line_bytes = 0usize;
    let mut last_line_empty = true;
    // The selected line being read: its text, while it is stored, and its size.
    let mut pieces: Option<Vec<String>> = Some(Vec::new());
    let mut line_bytes = 0usize;
    let mut line_has_text = false;
    // Bytes of an incomplete UTF-8 sequence carried to the next chunk
    // (`TextDecoder.decode(chunk, {stream: true})`).
    let mut pending_utf8: Vec<u8> = Vec::new();

    macro_rules! append_to_line {
        ($text:expr, $start:expr, $end:expr) => {{
            line_has_text = true;
            if let Some(stored) = pieces.as_mut() {
                let piece = &$text[$start..$end];
                // A line over the byte cap is never shown, and a first line
                // over it is only measured: never store all of a 1 GB line
                // (`read.ts:170-174`).
                if line_bytes <= DEFAULT_MAX_BYTES {
                    stored.push((*piece).to_string());
                }
                line_bytes += piece.len();
            }
        }};
    }

    macro_rules! finish_line {
        () => {{
            if let Some(pending) = pieces.as_ref() {
                if selected_lines == 0 {
                    first_line_bytes = line_bytes;
                }
                // Storing continues one line past the caps: truncateHead
                // ignores a trailing empty line, so the line after the cap
                // must be there for it to decide truncation as for the full
                // text (`read.ts:178-180`).
                if stored_lines.len() > DEFAULT_MAX_LINES || stored_bytes > DEFAULT_MAX_BYTES {
                    stored_past_caps = true;
                }
                let text = pending.join("");
                stored_bytes += text.len() + usize::from(!stored_lines.is_empty());
                stored_lines.push(text);
            }
            selected_lines += 1;
            last_line_empty = !line_has_text;
            pieces = if stored_past_caps {
                None
            } else {
                Some(Vec::new())
            };
            line_bytes = 0;
            line_has_text = false;
        }};
    }

    macro_rules! select {
        ($text:expr) => {{
            let text: &str = $text;
            let mut position = 0usize;
            // Start of the selected text in this piece, measured as a whole:
            // the joined lines' size.
            let mut selection_start: Option<usize> = None;
            loop {
                let selected = line >= start_line as u64 && line < end_line;
                if selected && selection_start.is_none() {
                    selection_start = Some(position);
                }
                let newline = text[position..]
                    .find('\n')
                    .map(|relative| position + relative);
                let end = newline.unwrap_or(text.len());
                if selected && end > position {
                    append_to_line!(text, position, end);
                }
                let Some(newline) = newline else {
                    if let Some(start) = selection_start {
                        selected_bytes += text[start..].len();
                    }
                    break;
                };
                if selected {
                    finish_line!();
                    if line + 1 == end_line {
                        if let Some(start) = selection_start {
                            selected_bytes += text[start..newline].len();
                        }
                        selection_start = None;
                    }
                }
                line += 1;
                position = newline + 1;
            }
        }};
    }

    let mut chunk = vec![0u8; 64 * 1024];
    let mut bytes_read_total = 0u64;
    loop {
        if signal.aborted() {
            return Err(ToolError("Operation aborted".to_string()));
        }
        // Stop at the size the file had when opened, or run to EOF.
        let want = match byte_limit {
            Some(limit) => {
                let remaining = limit - bytes_read_total;
                if remaining == 0 {
                    break;
                }
                (chunk.len() as u64).min(remaining) as usize
            }
            None => chunk.len(),
        };
        let read = match file.read(&mut chunk[..want]) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) => return Err(ToolError(io_error_node_message(&error, path, "read"))),
        };
        bytes_read_total += read as u64;
        let text = decode_streaming(&mut pending_utf8, &chunk[..read]);
        select!(&text);
    }
    // Flush the decoder and close the selection, like
    // `select(decoder.decode())` and the trailing `finishLine`
    // (`read.ts:220-221`).
    let text = decode_streaming(&mut pending_utf8, &[]);
    select!(&text);
    if line >= start_line as u64 && line < end_line {
        finish_line!();
    }
    // The trailing `finishLine` above resets state nothing reads afterwards;
    // quiet the dead-store warnings that produces.
    let _ = (&stored_bytes, &line_bytes, &line_has_text, &pieces);

    let truncation = TruncationResult {
        // truncateHead's counts for the joined selected lines, which drop a
        // trailing empty line (`read.ts:223-227`).
        total_lines: if selected_bytes == 0 {
            0
        } else {
            selected_lines - usize::from(last_line_empty)
        },
        total_bytes: selected_bytes,
        ..truncate_head(&stored_lines.join("\n"), TruncationOptions::default())
    };
    Ok(SelectedLines {
        total_file_lines: line as usize + 1,
        truncation,
        first_line_bytes,
    })
}

/// `TextDecoder.decode(chunk, {stream: true})` plus the flush in `finish`: an
/// incomplete trailing sequence is held back, invalid bytes become one
/// replacement character per maximal invalid subsequence.
fn decode_streaming(pending_utf8: &mut Vec<u8>, data: &[u8]) -> String {
    let mut buffer = std::mem::take(pending_utf8);
    buffer.extend_from_slice(data);
    match std::str::from_utf8(&buffer) {
        Ok(text) => {
            pending_utf8.clear();
            text.to_string()
        }
        Err(error) => {
            let valid_up_to = error.valid_up_to();
            let text = String::from_utf8_lossy(&buffer[..valid_up_to]).into_owned();
            let incomplete = &buffer[valid_up_to..];
            *pending_utf8 = if error.error_len().is_none() {
                incomplete.to_vec()
            } else {
                // Genuinely invalid bytes: emit replacements and continue.
                String::from_utf8_lossy(incomplete).into_owned();
                Vec::new()
            };
            text
        }
    }
}

/// `Buffer.toString("utf-8")` for the whole-file path.
fn read_text_file(path: &std::path::Path) -> Result<String, ToolError> {
    let bytes =
        fs::read(path).map_err(|error| ToolError(io_error_node_message(&error, path, "read")))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// pi's `detectSupportedImageMimeType` (`utils/mime.ts:9-29`): content
/// signature, not extension. Delegates to the images module's sniffer so the
/// read tool and the `@file` processor share one detection rule.
pub fn detect_supported_image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    crate::images::detect_supported_image_format(bytes).map(crate::images::ImageFormat::mime_type)
}

/// pi's `detectSupportedImageMimeTypeFromFile` (`utils/mime.ts:31-41`).
fn detect_supported_image_mime_type_from_file(path: &std::path::Path) -> Option<&'static str> {
    let mut file = fs::File::open(path).ok()?;
    let mut buffer = vec![0u8; 4100];
    let mut read = 0;
    while read < buffer.len() {
        match file.read(&mut buffer[read..]) {
            Ok(0) | Err(_) => break,
            Ok(count) => read += count,
        }
    }
    detect_supported_image_mime_type(&buffer[..read])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use image::GenericImageView;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

    /// Synthetic PNG with a valid signature/IHDR and the given header size.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&13_u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[0_u8; 9]); // color/compression/filter/interlace + CRC
        bytes
    }

    /// Synthetic, header-valid BMP (unsupported for inline attachment).
    fn bmp() -> Vec<u8> {
        let mut bytes = vec![b'B', b'M'];
        bytes.extend_from_slice(&200_u32.to_le_bytes()); // declared file size (no pixel data follows)
        bytes.extend_from_slice(&[0_u8; 4]); // reserved
        bytes.extend_from_slice(&100_u32.to_le_bytes()); // pixel data offset past EOF (unprocessable)
        bytes.extend_from_slice(&12_u32.to_le_bytes()); // BITMAPCOREHEADER
        bytes.extend_from_slice(&1_u16.to_le_bytes()); // width
        bytes.extend_from_slice(&1_u16.to_le_bytes()); // height
        bytes.extend_from_slice(&1_u16.to_le_bytes()); // color planes
        bytes.extend_from_slice(&24_u16.to_le_bytes()); // bits per pixel
        bytes.extend_from_slice(&[0_u8; 4]);
        bytes
    }

    async fn read_bytes(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> AgentToolResult {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("write synthetic image");
        let context = ToolContext::new(dir.path());
        let arguments = json!({ "path": path.display().to_string() });
        execute(&arguments, &context, &AbortSignal::new())
            .await
            .expect("read succeeds")
    }

    #[tokio::test]
    async fn reads_an_image_as_a_text_note_and_image_block() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bytes = png(2, 2);
        let result = read_bytes(&dir, "image.png", &bytes).await;

        assert_eq!(result.content.len(), 2);
        assert_eq!(text_of(&result.content[0]), "Read image file [image/png]");
        let pi_ai::types::ContentBlock::Image(image) = &result.content[1] else {
            panic!("expected an image block");
        };
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(image.data, BASE64_STANDARD.encode(&bytes));
        assert_eq!(
            result.structured_content,
            Some(json!({
                "type": "image",
                "data": BASE64_STANDARD.encode(&bytes),
                "mimeType": "image/png",
                "note": "Read image file [image/png]",
            }))
        );
    }

    #[tokio::test]
    async fn unprocessable_image_returns_the_upstream_omission_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = read_bytes(&dir, "image.bmp", &bmp()).await;

        assert_eq!(result.content.len(), 1);
        assert_eq!(
            text_of(&result.content[0]),
            format!(
                "Read image file [image/bmp]\n{}",
                crate::images::UNCONVERTIBLE_IMAGE
            )
        );
        assert_eq!(
            result.structured_content,
            Some(json!(text_of(&result.content[0])))
        );
    }

    #[tokio::test]
    async fn oversized_image_takes_the_could_not_resize_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = read_bytes(&dir, "large.png", &png(4000, 3000)).await;

        assert_eq!(result.content.len(), 1);
        assert_eq!(
            text_of(&result.content[0]),
            format!(
                "Read image file [image/png]\n{}",
                crate::images::COULD_NOT_RESIZE
            )
        );
    }

    fn text_of(block: &pi_ai::types::ContentBlock) -> &str {
        block.as_text().expect("a text block")
    }

    // Valid images generated by the `image` crate; no real user files.
    use image::codecs::{bmp::BmpEncoder, png::PngEncoder};
    use image::{ExtendedColorType, ImageEncoder, RgbImage};

    fn real_rgb(width: u32, height: u32) -> RgbImage {
        let mut image = RgbImage::new(width, height);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = image::Rgb([
                u8::try_from(x * 37 % 256).unwrap_or_default(),
                u8::try_from(y * 91 % 256).unwrap_or_default(),
                0x40,
            ]);
        }
        image
    }

    fn real_png(width: u32, height: u32) -> Vec<u8> {
        let image = real_rgb(width, height);
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgb8,
            )
            .expect("encode synthetic png");
        bytes
    }

    #[tokio::test]
    async fn reads_a_valid_bmp_as_a_converted_png_attachment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let image = real_rgb(3, 2);
        let mut bmp = Vec::new();
        BmpEncoder::new(&mut bmp)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgb8,
            )
            .expect("encode synthetic bmp");
        let result = read_bytes(&dir, "image.bmp", &bmp).await;

        assert_eq!(result.content.len(), 2);
        assert_eq!(
            text_of(&result.content[0]),
            "Read image file [image/png]\n[Image converted from image/bmp to image/png.]"
        );
        let pi_ai::types::ContentBlock::Image(attached) = &result.content[1] else {
            panic!("expected an image block");
        };
        assert_eq!(attached.mime_type, "image/png");
        let decoded =
            image::load_from_memory(&BASE64_STANDARD.decode(&attached.data).expect("base64"))
                .expect("decode converted png");
        assert_eq!(decoded.dimensions(), (3, 2));
        assert_eq!(
            result
                .structured_content
                .as_ref()
                .and_then(|value| value.get("type")),
            Some(&json!("image"))
        );
    }

    #[tokio::test]
    async fn oversized_real_image_is_resized_and_attached() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Just over the 2000px width cap; the compressed fixture stays tiny.
        let result = read_bytes(&dir, "large.png", &real_png(2001, 1)).await;

        assert_eq!(result.content.len(), 2);
        assert!(
            text_of(&result.content[0]).contains(
                "Read image file [image/png]\n\
                 [Image: original 2001x1, displayed at 2000x1. Multiply coordinates by 1.00 to map to original image.]"
            ),
            "{}",
            text_of(&result.content[0])
        );
        let pi_ai::types::ContentBlock::Image(attached) = &result.content[1] else {
            panic!("expected an image block");
        };
        assert_eq!(attached.mime_type, "image/png");
        let decoded =
            image::load_from_memory(&BASE64_STANDARD.decode(&attached.data).expect("base64"))
                .expect("decode resized png");
        assert_eq!(decoded.dimensions(), (2000, 1));
    }
}
