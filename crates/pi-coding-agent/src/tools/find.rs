//! The find tool.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/find.ts`.
//!
//! pi delegates to `fd` and does the shaping: results are relativized against
//! the search root with posix separators (`find.ts:14-24`), capped at `limit`
//! (default 1000) with one result past the limit telling "exactly N matches"
//! apart from "more than N" (`find.ts:239`), then at the byte limit, and pi's
//! notices are appended (`find.ts:322-336`).
//!
//! Two pi behaviors worth keeping, both about the glob semantics:
//!
//! * `--no-require-git` is only passed outside a git repository
//!   (`find.ts:188-202`): inside one, fd's git-aware behavior is left alone
//!   so a nested repo's `.gitignore` stops at its own boundary (pi issue 5960).
//! * a pattern containing `/` matches the *full path*, so it is prefixed with
//!   `**/` unless it already is (`find.ts:213-220`) — without that,
//!   `src/**/*.spec.ts` matches nothing — and a leading `./` is dropped
//!   because fd's full paths never contain "/./" (`find.ts:210-211`).
//!
//! pi resolves `fd` through `ensureTool`, which looks up the system binary and
//! downloads a pinned build when missing. This port looks the binary up on
//! `PATH` and returns pi's "not available" error when it is absent; it never
//! downloads.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use pi_agent::types::AbortSignal;
use pi_agent::types::AgentToolResult;
use serde_json::{Map, Value, json};
use tokio::process::Command;

use crate::tools::path_utils::{path_exists, resolve_to_cwd};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, TruncationOptions, format_size, truncate_head};
use crate::tools::{ToolContext, ToolError, optional_f64, optional_str, text_result};
use std::fmt::Write as _;

/// pi's `DEFAULT_LIMIT` (`find.ts:41`).
pub const DEFAULT_LIMIT: usize = 1000;

/// pi's find tool description (`find.ts:78`), with the limits filled in.
pub fn description() -> String {
    format!(
        "Search for files by glob pattern. Returns matching file paths relative to the search directory. \
Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} results or {}KB (whichever is hit first).",
        DEFAULT_MAX_BYTES / 1024
    )
}

/// pi's `findToolSystemPromptContribution` (`find.ts:34-37`).
pub const PROMPT_SNIPPET: &str = "Find files by glob pattern (respects .gitignore)";

/// pi's `ensureTool("fd")` (`utils/tools-manager.ts`): the system binary on
/// `PATH`, or pi's error when it is absent.
fn resolve_fd() -> Result<PathBuf, ToolError> {
    which("fd")
        .ok_or_else(|| ToolError("fd is not available and could not be downloaded".to_string()))
}

fn which(executable: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(executable))
        .find(|candidate| candidate.is_file())
}

/// pi's `relativizeFindResultPath` (`find.ts:14-24`): relative to the search
/// root, posix separators, trailing separator preserved.
pub fn relativize_find_result_path(result_path: &str, search_path: &Path) -> String {
    let had_trailing_separator = result_path.ends_with('/');
    let relative = if Path::new(result_path).is_absolute() {
        Path::new(result_path)
            .strip_prefix(search_path)
            .map_or_else(
                |_| result_path.to_string(),
                |path| path.to_string_lossy().into_owned(),
            )
    } else {
        result_path.to_string()
    };
    let posix_path = relative.split('/').collect::<Vec<&str>>().join("/");
    if had_trailing_separator && !posix_path.ends_with('/') {
        format!("{posix_path}/")
    } else {
        posix_path
    }
}

/// Whether the search path is inside a git repository — pi's `.git` walk up
/// (`find.ts:192-201`).
fn inside_git_repo(search_path: &Path) -> bool {
    let mut current = search_path.to_path_buf();
    loop {
        if path_exists(&current.join(".git")) {
            return true;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return false,
        }
    }
}

/// pi's `createFindToolDefinition(...).execute` (`find.ts:81-354`).
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
    let search_argument = optional_str(arguments, "path").unwrap_or_else(|| ".".to_string());
    let search_path = resolve_to_cwd(&search_argument, &context.cwd);
    // fd treats --max-results 0 as unlimited, so a limit below 1 means 1
    // (`find.ts:113`).
    let effective_limit = match optional_f64(arguments, "limit") {
        Some(limit) => limit.floor().max(1.0) as usize,
        None => DEFAULT_LIMIT,
    };

    if !path_exists(&search_path) {
        return Err(ToolError(format!(
            "Path not found: {}",
            search_path.display()
        )));
    }

    let fd_path = resolve_fd()?;
    if signal.aborted() {
        return Err(ToolError("Operation aborted".to_string()));
    }

    let mut command = Command::new(&fd_path);
    command
        .arg("--color=never")
        .arg("--hidden")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // fd normally ignores .gitignore outside git repos, so keep
    // --no-require-git there (`find.ts:188-202`).
    if !inside_git_repo(&search_path) {
        command.arg("--no-require-git");
    }

    // fd's --glob matches against the basename unless --full-path is set;
    // a path-containing pattern like 'src/**/*.spec.ts' needs a leading '**/'
    // to match anything (`find.ts:206-220`).
    let mut effective_pattern = pattern.clone();
    let is_path_pattern = {
        let dropped = effective_pattern.trim_start_matches("./").to_string();
        effective_pattern = dropped;
        effective_pattern.contains('/')
    };
    if is_path_pattern
        && !effective_pattern.starts_with('/')
        && !effective_pattern.starts_with("**/")
        && effective_pattern != "**"
    {
        effective_pattern = format!("**/{effective_pattern}");
    }
    // One result past the limit tells "exactly N matches" apart from "more
    // than N" (`find.ts:238-239`).
    command
        .arg("--glob")
        .arg("--max-results")
        .arg((effective_limit + 1).to_string());
    if is_path_pattern {
        command.arg("--full-path");
    }
    command.arg("--").arg(&effective_pattern).arg(&search_path);

    let output = command.output().await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ToolError("fd is not available and could not be downloaded".to_string())
        } else {
            ToolError(format!("Failed to run fd: {error}"))
        }
    })?;
    if signal.aborted() {
        return Err(ToolError("Operation aborted".to_string()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    let mut lines: Vec<String> = Vec::new();
    for raw_line in stdout.lines() {
        let line = raw_line.trim_end_matches('\r').trim();
        if !line.is_empty() {
            lines.push(line.to_string());
        }
    }

    if !output.status.success() {
        let message = if stderr.trim().is_empty() {
            format!("fd exited with code {}", output.status.code().unwrap_or(-1))
        } else {
            stderr.trim().to_string()
        };
        // pi still returns whatever came out when fd produced output
        // (`find.ts:299-303`).
        if lines.is_empty() {
            return Err(ToolError(message));
        }
    }

    if lines.is_empty() {
        return Ok(text_result("No files found matching pattern"));
    }

    // pi counts results up to the limit and stops there: the line handler
    // returns early once `resultCount === effectiveLimit`, so results past
    // the limit are neither relativized nor counted (`find.ts:276-286`).
    let result_limit_reached = lines.len() > effective_limit;
    let relativized: Vec<String> = lines
        .iter()
        .take(effective_limit)
        .map(|line| relativize_find_result_path(line, &search_path))
        .collect();
    let result_count = relativized.len();
    // Byte size of all results within the limit, each counted with a trailing
    // newline (`find.ts:254-255`). Results past the byte cap are counted but
    // not stored; truncateHead drops them anyway.
    let mut stored: Vec<&str> = Vec::new();
    let mut output_bytes = 0usize;
    for result in &relativized {
        if output_bytes.saturating_sub(1) <= DEFAULT_MAX_BYTES {
            stored.push(result);
        }
        output_bytes += result.len() + 1;
    }
    // Stored results extend past the byte cap when more exist, so
    // truncateHead decides truncation as for the full output; only the totals
    // need the unstored rest (`find.ts:314-320`).
    let mut truncation = truncate_head(
        &stored.join("\n"),
        TruncationOptions {
            max_lines: Some(usize::MAX),
            ..TruncationOptions::default()
        },
    );
    truncation.total_lines = result_count;
    truncation.total_bytes = output_bytes - 1;

    let mut result = truncation.content.clone();
    let mut details = Map::new();
    let mut notices: Vec<String> = Vec::new();
    if result_limit_reached {
        notices.push(format!(
            "{effective_limit} results limit reached. Use limit={} for more, or refine pattern",
            effective_limit * 2
        ));
        details.insert("resultLimitReached".to_string(), json!(effective_limit));
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.insert(
            "truncation".to_string(),
            crate::tools::truncate::truncation_json(&truncation),
        );
    }
    if !notices.is_empty() {
        let _ = write!(result, "\n\n[{}]", notices.join(". "));
    }

    Ok(AgentToolResult {
        details: if details.is_empty() {
            json!({})
        } else {
            Value::Object(details)
        },
        ..text_result(result)
    })
}
