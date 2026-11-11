//! The ls tool.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/ls.ts`.
//!
//! Entries sorted case-insensitively (JS `localeCompare` order for the
//! languages the tool output holds), `/` suffix for directories, dotfiles
//! included, entry limit (default 500) and byte limit applied, notices
//! appended. A limit below 1 means 1 (`ls.ts:84`).

use std::fs;
use std::path::Path;

use pi_agent::types::AbortSignal;
use pi_agent::types::AgentToolResult;
use serde_json::{Map, Value, json};

use crate::tools::path_utils::{path_exists, resolve_to_cwd};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, TruncationOptions, format_size, truncate_head};
use crate::tools::{ToolContext, ToolError, optional_f64, optional_str, text_result};
use std::fmt::Write as _;

/// pi's `DEFAULT_LIMIT` (`ls.ts:22`).
pub const DEFAULT_LIMIT: usize = 500;

/// pi's `createLsToolDefinition(...).execute` (`ls.ts:64-168`).
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
    let path_argument = optional_str(arguments, "path").unwrap_or_else(|| ".".to_string());
    let dir_path = resolve_to_cwd(&path_argument, &context.cwd);
    // A limit of 0 would list nothing and report "(empty directory)", so it
    // means 1 (`ls.ts:83-84`). `Math.floor` first, then clamp.
    let effective_limit = match optional_f64(arguments, "limit") {
        Some(limit) => {
            let floor = limit.floor();
            if floor < 1.0 { 1usize } else { floor as usize }
        }
        None => DEFAULT_LIMIT,
    };

    // Check if path exists (`ls.ts:87-90`).
    if !path_exists(&dir_path) {
        return Err(ToolError(format!("Path not found: {}", dir_path.display())));
    }

    // Check if path is a directory (`ls.ts:93-97`).
    let metadata = fs::metadata(&dir_path).map_err(|error| ToolError(error.to_string()))?;
    if !metadata.is_dir() {
        return Err(ToolError(format!(
            "Not a directory: {}",
            dir_path.display()
        )));
    }

    // Read directory entries (`ls.ts:100-106`).
    let entries = match fs::read_dir(&dir_path) {
        Ok(entries) => entries,
        Err(error) => return Err(ToolError(format!("Cannot read directory: {error}"))),
    };
    let mut names: Vec<String> = Vec::new();
    // Entries pi cannot read are skipped by the read loop's catch.
    for entry in entries.flatten() {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }

    // Sort alphabetically, case-insensitive (`ls.ts:109`).
    names.sort_by_key(|a| a.to_lowercase());

    // Format entries with directory indicators (`ls.ts:112-130`).
    let mut results: Vec<String> = Vec::new();
    let mut entry_limit_reached = false;
    for name in &names {
        if results.len() >= effective_limit {
            entry_limit_reached = true;
            break;
        }
        let full_path = dir_path.join(name);
        // `path.join` on a directory whose string form ends with the
        // separator keeps the separator; resolve the display the same way.
        let mut suffix = "";
        match fs::metadata(&full_path) {
            Ok(metadata) => {
                if metadata.is_dir() {
                    suffix = "/";
                }
            }
            // Skip entries we cannot stat (`ls.ts:126-128`).
            Err(_) => continue,
        }
        results.push(format!("{name}{suffix}"));
    }

    if results.is_empty() {
        return Ok(text_result("(empty directory)"));
    }

    let raw_output = results.join("\n");
    // Apply byte truncation. There is no separate line limit because entry
    // count is already capped (`ls.ts:140-141`).
    let truncation = truncate_head(
        &raw_output,
        TruncationOptions {
            max_lines: Some(usize::MAX),
            ..TruncationOptions::default()
        },
    );
    let mut output = truncation.content.clone();
    let mut details = Map::new();
    // Build actionable notices for truncation and entry limits (`ls.ts:145-153`).
    let mut notices: Vec<String> = Vec::new();
    if entry_limit_reached {
        notices.push(format!(
            "{effective_limit} entries limit reached. Use limit={} for more",
            effective_limit * 2
        ));
        details.insert("entryLimitReached".to_string(), json!(effective_limit));
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.insert(
            "truncation".to_string(),
            crate::tools::truncate::truncation_json(&truncation),
        );
    }
    if !notices.is_empty() {
        let _ = write!(output, "\n\n[{}]", notices.join(". "));
    }

    Ok(AgentToolResult {
        // pi's `details` is `undefined` when empty; pi-agent's result always
        // carries the field, and its empty shape is `{}`.
        details: if details.is_empty() {
            json!({})
        } else {
            Value::Object(details)
        },
        ..text_result(output)
    })
}

/// Whether `path` is a directory that exists — pi's `ops.exists` +
/// `ops.stat` pair as one check (`ls.ts:42-46`).
pub fn is_directory(path: &Path) -> bool {
    path_exists(path) && fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// pi's ls tool description (`ls.ts:61`), with the limits filled in.
pub fn description() -> String {
    format!(
        "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. \
Includes dotfiles. Output is truncated to {DEFAULT_LIMIT} entries or {}KB (whichever is hit first).",
        DEFAULT_MAX_BYTES / 1024
    )
}

/// pi's `lsToolSystemPromptContribution` (`ls.ts:15-18`).
pub const PROMPT_SNIPPET: &str = "List directory contents";
