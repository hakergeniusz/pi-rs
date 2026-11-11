//! The grep tool.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/grep.ts`.
//!
//! pi delegates the search to ripgrep and does the shaping itself: parse
//! `--json` match and context events (`grep.ts:238-268`), relativize paths
//! against the search root (`grep.ts:151-159`), render `path:line:` for match
//! lines and `path-line-` for context lines, cap lines at 500 characters,
//! cap matches at `limit` (default 100) while keeping the last match's
//! trailing context (`grep.ts:252-267`), cap the output at the byte limit
//! while still counting past it (`grep.ts:231-235`), and append pi's notices.
//!
//! ripgrep is the same subprocess pi spawns (`grep.ts:163-178`:
//! `rg --no-config --json --line-number --color=never --hidden ...`), so
//! `.gitignore` handling is ripgrep's, not a reimplementation, and UTF-16
//! files decode on ripgrep's side (pi issue 11423). pi resolves it through
//! `ensureTool`, which looks up the system binary and downloads a pinned
//! build when missing; this port looks the binary up on `PATH` and returns
//! pi's "not available" error when it is absent; it never downloads.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use pi_agent::types::AbortSignal;
use pi_agent::types::AgentToolResult;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::tools::path_utils::resolve_to_cwd;
use crate::tools::truncate::{
    DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH, TruncationOptions, format_size, truncate_head,
    truncate_line,
};
use crate::tools::{
    ToolContext, ToolError, optional_bool, optional_f64, optional_str, text_result,
};
use std::fmt::Write as _;

/// pi's `DEFAULT_LIMIT` (`grep.ts:45`).
pub const DEFAULT_LIMIT: usize = 100;

/// pi's grep tool description (`grep.ts:90`), with the limits filled in.
pub fn description() -> String {
    format!(
        "Search file contents for a pattern. Returns matching lines with file paths and line numbers. \
Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} matches or {}KB (whichever is hit first). \
Long lines are truncated to {GREP_MAX_LINE_LENGTH} chars.",
        DEFAULT_MAX_BYTES / 1024
    )
}

/// pi's `grepToolSystemPromptContribution` (`grep.ts:39-42`).
pub const PROMPT_SNIPPET: &str = "Search file contents for patterns (respects .gitignore)";

/// pi's `ensureTool("rg")` (`utils/tools-manager.ts`): the system binary on
/// `PATH`, or pi's error when it is absent.
fn resolve_rg() -> Result<PathBuf, ToolError> {
    which("rg").ok_or_else(|| {
        ToolError("ripgrep (rg) is not available and could not be downloaded".to_string())
    })
}

fn which(executable: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(executable))
        .find(|candidate| candidate.is_file())
}

/// pi's `decodeRgText` (`grep.ts:71-75`): a ripgrep JSON text field; non-UTF-8
/// data arrives base64-encoded in `bytes`.
fn decode_rg_text(data: Option<&Value>) -> String {
    let Some(data) = data else {
        return String::new();
    };
    if let Some(text) = data.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    if let Some(bytes) = data.get("bytes").and_then(Value::as_str) {
        return base64_decode(bytes);
    }
    String::new()
}

/// Standard base64 decode, for `decodeRgText`'s `bytes` field.
fn base64_decode(text: &str) -> String {
    const INVALID: u8 = 255;
    let mut table = [INVALID; 256];
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for (index, &character) in alphabet.iter().enumerate() {
        table[character as usize] = index as u8;
    }
    let mut out: Vec<u8> = Vec::with_capacity(text.len() / 4 * 3);
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        let value = table[byte as usize];
        if value == INVALID {
            continue;
        }
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One parsed `{"type":"match"|"context"}` event.
struct RgEvent {
    file_path: String,
    line_number: usize,
    line_text: String,
    is_match: bool,
}

/// pi's `createGrepToolDefinition(...).execute` (`grep.ts:93-330`).
// A verbatim 1:1 port of the TS function; keeping it in one piece makes the
// side-by-side review against the source possible.
#[allow(clippy::too_many_lines)]
pub async fn execute(
    arguments: &Value,
    context: &ToolContext,
    signal: &AbortSignal,
) -> Result<AgentToolResult, ToolError> {
    if signal.aborted() {
        return Err(ToolError("Operation aborted".to_string()));
    }
    let pattern = optional_str(arguments, "pattern")
        .ok_or_else(|| ToolError("Expected required property 'pattern'".to_string()))?;
    let rg_path = resolve_rg()?;

    let search_argument = optional_str(arguments, "path").unwrap_or_else(|| ".".to_string());
    let search_path = resolve_to_cwd(&search_argument, &context.cwd);
    // `ops.isDirectory` throws when the path does not exist (`grep.ts:140-145`).
    let is_directory = match tokio::fs::metadata(&search_path).await {
        Ok(metadata) => metadata.is_dir(),
        Err(_) => {
            return Err(ToolError(format!(
                "Path not found: {}",
                search_path.display()
            )));
        }
    };

    // Clamp so String() never yields exponent notation, which rg rejects
    // (`grep.ts:147-149`).
    let context_value = match optional_f64(arguments, "context") {
        Some(context) if context > 0.0 => context.floor() as usize,
        _ => 0,
    };
    let effective_limit = match optional_f64(arguments, "limit") {
        Some(limit) => limit.floor().max(1.0) as usize,
        None => DEFAULT_LIMIT,
    };

    // --no-config: the tool contract (literal, glob, limit, ...) must not
    // depend on ambient user config like RIPGREP_CONFIG_PATH (`grep.ts:161-163`).
    let mut command = Command::new(&rg_path);
    command
        .arg("--no-config")
        .arg("--json")
        .arg("--line-number")
        .arg("--color=never")
        .arg("--hidden")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if optional_bool(arguments, "ignoreCase") {
        command.arg("--ignore-case");
    }
    if optional_bool(arguments, "literal") {
        command.arg("--fixed-strings");
    }
    // rg never matches a leading "./" ("./src/*.ts" finds nothing), so drop
    // it; "!" negates (`grep.ts:166-167`).
    if let Some(glob) = optional_str(arguments, "glob") {
        let normalized = match glob.strip_prefix('!') {
            Some(rest) => format!("!{}", rest.trim_start_matches("./")),
            None => glob.trim_start_matches("./").to_string(),
        };
        command.arg("--glob").arg(normalized);
    }
    // rg emits the surrounding lines as `context` events, so no matched file
    // is ever read here (`grep.ts:168-169`).
    if context_value > 0 {
        command.arg("--context").arg(context_value.to_string());
    }
    // rg roots --glob at its own cwd, so a directory search runs inside
    // searchPath (`grep.ts:170-173`). It searches "." because rg
    // canonicalizes its cwd: with an absolute path, a symlinked searchPath
    // would no longer lie under the glob root.
    let search_target: &std::ffi::OsStr = if is_directory {
        std::ffi::OsStr::new(".")
    } else {
        search_path.as_os_str()
    };
    command.arg("--").arg(&pattern).arg(search_target);
    if is_directory {
        command.current_dir(&search_path);
    }

    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ToolError("ripgrep (rg) is not available and could not be downloaded".to_string())
        } else {
            ToolError(format!("Failed to run ripgrep: {error}"))
        }
    })?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let stderr_reader = tokio::spawn(async move {
        let mut buffer = [0u8; 4096];
        let mut collected = String::new();
        loop {
            match stderr.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(count) => collected.push_str(&String::from_utf8_lossy(&buffer[..count])),
            }
        }
        collected
    });

    // The event loop pi drives from readline (`grep.ts:238-268`).
    let mut match_count = 0usize;
    let mut match_limit_reached = false;
    let mut lines_truncated = false;
    let mut aborted = false;
    let mut killed_due_to_limit = false;
    let mut output_lines: Vec<String> = Vec::new();
    // Size of the full output, including lines past the byte cap that are
    // counted but not stored (`grep.ts:207-209`).
    let mut output_bytes = 0usize;
    let mut output_line_count = 0usize;
    let mut last_file: Option<String> = None;
    // Set at the last allowed match. rg still sends that match's trailing
    // context, kept up to trailingContextEnd, and the limit notice waits for
    // proof of one more match (`grep.ts:211-215`).
    let mut limit_hit = false;
    let mut trailing_context_end = 0usize;
    let mut done = false;

    let mut reader = BufReader::new(stdout);
    let mut line_buffer = String::new();
    loop {
        if signal.aborted() {
            aborted = true;
            let _ = child.start_kill();
            break;
        }
        line_buffer.clear();
        match reader.read_line(&mut line_buffer).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = line_buffer.trim_end_matches(['\n', '\r']);
        if done || line.trim().is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        if event_type != "match" && event_type != "context" {
            continue;
        }
        let Some(data) = event.get("data") else {
            continue;
        };
        let file_path = decode_rg_text(data.get("path"));
        let Some(line_number) = data.get("line_number").and_then(Value::as_u64) else {
            continue;
        };
        let line_text = decode_rg_text(data.get("lines"));
        if file_path.is_empty() {
            continue;
        }
        let event = RgEvent {
            file_path,
            line_number: line_number as usize,
            line_text,
            is_match: event_type == "match",
        };

        if !limit_hit {
            push_line(
                &event,
                event.is_match,
                &search_path,
                is_directory,
                &mut last_file,
                &mut output_lines,
                &mut output_bytes,
                &mut output_line_count,
                &mut lines_truncated,
            );
            if !event.is_match {
                continue;
            }
            match_count += 1;
            if match_count >= effective_limit {
                limit_hit = true;
                trailing_context_end = event.line_number + context_value;
            }
            continue;
        }
        let in_window = last_file.as_deref() == Some(event.file_path.as_str())
            && event.line_number <= trailing_context_end;
        // A match inside the last match's trailing context is shown as
        // context (`grep.ts:262-264`).
        if in_window {
            push_line(
                &event,
                false,
                &search_path,
                is_directory,
                &mut last_file,
                &mut output_lines,
                &mut output_bytes,
                &mut output_line_count,
                &mut lines_truncated,
            );
        }
        // Only another match proves that the limit cut results off
        // (`grep.ts:265-267`).
        if event.is_match {
            match_limit_reached = true;
        }
        if match_limit_reached && !(in_window && event.line_number < trailing_context_end) {
            done = true;
            killed_due_to_limit = true;
            // pi's `finish()` kills the child to stop the stream; the reader
            // then sees EOF and the remaining events are ignored.
            let _ = child.start_kill();
        }
    }
    // Reap the child so its exit status is observable, and drain stderr.
    let status = child.wait().await;
    let stderr_text = stderr_reader.await.unwrap_or_default();

    if aborted {
        return Err(ToolError("Operation aborted".to_string()));
    }
    let status_code = status.ok().and_then(|status| status.code());
    if !killed_due_to_limit && status_code.is_some_and(|code| code != 0 && code != 1) {
        let error_message = if stderr_text.trim().is_empty() {
            format!(
                "ripgrep exited with code {}",
                status_code.expect("checked above")
            )
        } else {
            stderr_text.trim().to_string()
        };
        return Err(ToolError(error_message));
    }
    if match_count == 0 {
        return Ok(text_result("No matches found"));
    }

    let raw_output = output_lines.join("\n");
    // Apply byte truncation. There is no line limit here because the match
    // limit already capped rows (`grep.ts:292-296`).
    let mut truncation = truncate_head(
        &raw_output,
        TruncationOptions {
            max_lines: Some(usize::MAX),
            ..TruncationOptions::default()
        },
    );
    truncation.total_lines = output_line_count;
    truncation.total_bytes = output_bytes.saturating_sub(1);
    let mut output = truncation.content.clone();
    let mut details = Map::new();
    // Build actionable notices for truncation and match limits
    // (`grep.ts:298-317`).
    let mut notices: Vec<String> = Vec::new();
    if match_limit_reached {
        notices.push(format!(
            "{effective_limit} matches limit reached. Use limit={} for more, or refine pattern",
            effective_limit * 2
        ));
        details.insert("matchLimitReached".to_string(), json!(effective_limit));
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.insert(
            "truncation".to_string(),
            crate::tools::truncate::truncation_json(&truncation),
        );
    }
    if lines_truncated {
        notices.push(format!(
            "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
        ));
        details.insert("linesTruncated".to_string(), json!(true));
    }
    if !notices.is_empty() {
        let _ = write!(output, "\n\n[{}]", notices.join(". "));
    }

    Ok(AgentToolResult {
        details: if details.is_empty() {
            json!({})
        } else {
            Value::Object(details)
        },
        ..text_result(output)
    })
}

/// pi's `pushLine` (`grep.ts:220-236`).
#[allow(clippy::too_many_arguments)]
fn push_line(
    event: &RgEvent,
    render_as_match: bool,
    search_path: &Path,
    is_directory: bool,
    last_file: &mut Option<String>,
    output_lines: &mut Vec<String>,
    output_bytes: &mut usize,
    output_line_count: &mut usize,
    lines_truncated: &mut bool,
) {
    *last_file = Some(event.file_path.clone());
    let relative_path = format_path(&event.file_path, search_path, is_directory);
    // Strip only the line terminator: a lone CR inside a line is content, as
    // in the read tool (`grep.ts:223-224`).
    let sanitized = event.line_text.trim_end_matches(['\n', '\r']);
    // Truncate long lines so grep output stays compact (`grep.ts:225-227`).
    let (text, was_truncated) = truncate_line(sanitized, GREP_MAX_LINE_LENGTH);
    if was_truncated {
        *lines_truncated = true;
    }
    // The caller picks the rendering: a match inside the last allowed match's
    // trailing context is shown as context (`grep.ts:262-264`).
    let line = if render_as_match {
        format!("{relative_path}:{}: {text}", event.line_number)
    } else {
        format!("{relative_path}-{}- {text}", event.line_number)
    };
    // Byte truncation below drops everything past the cap, so keep counting
    // but stop storing (`grep.ts:231-235`). outputBytes counts a newline after
    // every line; the joined output has one fewer.
    if output_bytes.saturating_sub(1) <= DEFAULT_MAX_BYTES {
        output_lines.push(line.clone());
    }
    *output_bytes += line.len() + 1;
    *output_line_count += 1;
}

/// pi's `formatPath` (`grep.ts:151-159`): relative to the search root when
/// that is possible, otherwise the basename.
fn format_path(file_path: &str, search_path: &Path, is_directory: bool) -> String {
    if is_directory {
        // `path.relative(searchPath, path.resolve(searchPath, filePath))`.
        let resolved = if Path::new(file_path).is_absolute() {
            PathBuf::from(file_path)
        } else {
            search_path.join(file_path)
        };
        if let Ok(relative) = resolved.strip_prefix(search_path) {
            let relative = relative.to_string_lossy().replace('\\', "/");
            if !relative.is_empty() && !relative.starts_with("..") {
                return relative;
            }
        }
    }
    Path::new(file_path).file_name().map_or_else(
        || file_path.to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decode_round_trips() {
        assert_eq!(base64_decode("aGVsbG8="), "hello");
        assert_eq!(base64_decode(""), "");
    }

    #[test]
    fn format_path_relativizes_inside_the_root() {
        let root = Path::new("/tmp/repo");
        assert_eq!(format_path("/tmp/repo/src/a.rs", root, true), "src/a.rs");
        assert_eq!(format_path("/tmp/repo/a.rs", root, true), "a.rs");
        assert_eq!(format_path("/outside/a.rs", root, true), "a.rs");
        // A single-file search shows the basename only.
        assert_eq!(format_path("/tmp/repo/src/a.rs", root, false), "a.rs");
    }
}
