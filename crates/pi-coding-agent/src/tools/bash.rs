//! The bash tool.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/bash.ts`
//! (+ `utils/shell.ts`).
//!
//! Behaviors ported:
//!
//! * stdout and stderr are interleaved into one stream, tail-truncated for
//!   the model, with the full output spilled to a temp file when it does not
//!   fit (`bash.ts:304-384`);
//! * a non-zero exit is an `isError` *result* with the output attached and
//!   the structured output shape codemode callers receive (`bash.ts:425-441`);
//! * a timeout kills the process tree and fails the call with
//!   `Command timed out after N seconds`, with whatever output arrived
//!   (`bash.ts:405-414`);
//! * an abort fails the call with `Command aborted` (`bash.ts:402-404`);
//! * partial results are published at most every 100 ms, pi's
//!   `BASH_UPDATE_THROTTLE_MS` (`renderers/bash.ts:8`);
//! * the child runs in its own process group and the whole group is signalled,
//!   the same lever pi's `killProcessTree` pulls without a `ps` scan.
//!
//! Porting note on timeouts: pi keeps waiting when background processes hold
//! the inherited pipes (`bash.ts:96-99`), because Node waits for stdio to
//! close. Here the process group kill ends the pipe holders too, so the
//! timeout path never hangs and the shellExit detail pi adds for that case is
//! unreachable.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use pi_agent::types::AbortSignal;
use pi_agent::types::AgentToolResult;
use serde_json::{Map, Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use crate::tools::output_accumulator::{
    OutputAccumulator, OutputAccumulatorOptions, truncation_footer,
};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
use crate::tools::{ToolContext, ToolError, optional_f64, required_str, text_block};

/// pi's `MAX_TIMEOUT_MS` (`bash.ts:21`).
const MAX_TIMEOUT_MS: u64 = 2_147_483_647;
/// pi's `STRUCTURED_OUTPUT_MAX_BYTES` (`bash.ts:23`).
const STRUCTURED_OUTPUT_MAX_BYTES: usize = 1024 * 1024;
/// pi's `BASH_UPDATE_THROTTLE_MS` (`renderers/bash.ts:8`).
pub const BASH_UPDATE_THROTTLE_MS: u64 = 100;
const READ_BUFFER: usize = 8192;

/// pi's `resolveTimeoutMs` (`bash.ts:26-37`).
fn resolve_timeout_ms(timeout: Option<f64>) -> Result<Option<u64>, ToolError> {
    let Some(timeout) = timeout else {
        return Ok(None);
    };
    if !timeout.is_finite() || timeout <= 0.0 {
        return Err(ToolError(
            "Invalid timeout: must be a finite number of seconds".to_string(),
        ));
    }
    let timeout_ms = timeout * 1000.0;
    if timeout_ms > MAX_TIMEOUT_MS as f64 {
        return Err(ToolError(format!(
            "Invalid timeout: maximum is {} seconds",
            format_number(MAX_TIMEOUT_MS as f64 / 1000.0)
        )));
    }
    Ok(Some(timeout_ms as u64))
}

/// JS number formatting for the places pi interpolates a number into a
/// message.
fn format_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e21 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// pi's `getShellConfig` (`utils/shell.ts:108-118`), Unix branch: the
/// configured `shellPath` if there is one (error when missing), else
/// `/bin/bash`, then `bash` on `PATH`, then `sh`. The args are always `["-c"]`
/// — pi never passes `-l`. The Windows branch ships with the skipped
/// `powershell` variant.
fn shell_config(custom: Option<&str>) -> Result<(String, Vec<String>), ToolError> {
    if let Some(path) = custom {
        if Path::new(path).exists() {
            return Ok((path.to_string(), vec!["-c".to_string()]));
        }
        return Err(ToolError(format!("Custom shell path not found: {path}")));
    }
    if Path::new("/bin/bash").exists() {
        return Ok(("/bin/bash".to_string(), vec!["-c".to_string()]));
    }
    if which("bash").is_some() {
        return Ok(("bash".to_string(), vec!["-c".to_string()]));
    }
    Ok(("sh".to_string(), vec!["-c".to_string()]))
}

/// A PATH lookup, the job pi's `findExecutableOnPath` does for shell
/// discovery.
pub(crate) fn which(executable: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(executable))
        .find(|candidate| candidate.is_file())
}

/// pi's `appendStatus` (`bash.ts:386`).
fn append_status(text: &str, status: &str) -> String {
    if text.is_empty() {
        status.to_string()
    } else {
        format!("{text}\n\n{status}")
    }
}

/// pi's `formatOutput` (`bash.ts:366-384`): the model-facing text plus the
/// details it came with. `empty_text` is pi's `(no output)` default; the
/// error paths pass `""`.
fn format_output(
    snapshot: &crate::tools::output_accumulator::OutputSnapshot,
    last_line_bytes: usize,
    empty_text: &str,
) -> (String, Value) {
    let mut text = if snapshot.content.is_empty() {
        empty_text.to_string()
    } else {
        snapshot.content.clone()
    };
    let mut details = Map::new();
    if snapshot.truncation.truncated {
        text.push_str(&truncation_footer(snapshot, last_line_bytes));
        details.insert(
            "truncation".to_string(),
            crate::tools::truncate::truncation_json(&snapshot.truncation),
        );
        details.insert(
            "fullOutputPath".to_string(),
            match snapshot.full_output_path.as_ref() {
                Some(path) => json!(path.display().to_string()),
                None => Value::Null,
            },
        );
    }
    (text, Value::Object(details))
}

/// pi's `createBashToolDefinition(...).execute` (`bash.ts:289-446`).
// A verbatim 1:1 port of the TS function; keeping it in one piece makes the
// side-by-side review against the source possible.
#[allow(clippy::too_many_lines)]
pub async fn execute(
    arguments: &Value,
    context: &ToolContext,
    signal: &AbortSignal,
    updates: Option<&tokio::sync::mpsc::UnboundedSender<AgentToolResult>>,
) -> Result<AgentToolResult, ToolError> {
    let command = required_str(arguments, "command")?;
    let requested_timeout = optional_f64(arguments, "timeout");
    let timeout_ms = resolve_timeout_ms(requested_timeout)?;

    // `commandPrefix ? \`${commandPrefix}\n${command}\` : command`
    // (`bash.ts:296`).
    let resolved_command = match &context.command_prefix {
        Some(prefix) => format!("{prefix}\n{command}"),
        None => command,
    };
    let cwd = context.cwd.clone();
    if !cwd.exists() {
        return Err(ToolError(format!(
            "Working directory does not exist: {}\nCannot execute bash commands.",
            cwd.display()
        )));
    }
    let (shell, shell_args) = shell_config(context.shell_path.as_deref())?;

    if let Some(updates) = updates {
        // pi's initial empty update, so a consumer can render "running"
        // (`bash.ts:347`).
        let _ = updates.send(AgentToolResult::default());
    }

    let started_at = Instant::now();
    let mut accumulator = OutputAccumulator::new(OutputAccumulatorOptions::with_prefix("pi-bash"));
    let mut last_update_at = Instant::now();

    let mut child = spawn(&shell, &shell_args, &resolved_command, &cwd, context)?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");

    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut stdout_buffer = vec![0u8; READ_BUFFER];
    let mut stderr_buffer = vec![0u8; READ_BUFFER];
    let mut timed_out = false;

    while stdout_open || stderr_open {
        if let Some(deadline) = timeout_ms {
            if started_at.elapsed() >= Duration::from_millis(deadline) {
                timed_out = true;
                break;
            }
        }
        if signal.aborted() {
            kill_tree(&mut child);
            let _ = child.wait().await;
            let snapshot = finish_output(&mut accumulator);
            let (text, _) = format_output(&snapshot, accumulator.last_line_bytes(), "");
            return Err(ToolError(append_status(&text, "Command aborted")));
        }
        tokio::select! {
            read = stdout.read(&mut stdout_buffer), if stdout_open => {
                match read {
                    Ok(0) | Err(_) => stdout_open = false,
                    Ok(count) => {
                        accumulator.append(&stdout_buffer[..count]);
                        maybe_publish(&mut accumulator, updates, &mut last_update_at);
                    }
                }
            }
            read = stderr.read(&mut stderr_buffer), if stderr_open => {
                match read {
                    Ok(0) | Err(_) => stderr_open = false,
                    Ok(count) => {
                        accumulator.append(&stderr_buffer[..count]);
                        maybe_publish(&mut accumulator, updates, &mut last_update_at);
                    }
                }
            }
            // A command that produces no output would otherwise sit in select
            // until its next read completes; the tick bounds how late the
            // timeout and the abort flag are noticed.
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }

    if timed_out {
        // Also when the shell already exited: on Unix the group kill still
        // reaches background processes holding the pipes (`bash.ts:152-159`).
        kill_tree(&mut child);
    }
    // pi waits for the child even on the timeout path, so the kill has landed
    // before the output is snapshotted.
    let status = child.wait().await;
    let snapshot = finish_output(&mut accumulator);
    if let Some(updates) = updates {
        let _ = updates.send(AgentToolResult {
            content: vec![text_block(snapshot.content.clone())],
            details: json!({
                "truncation": if snapshot.truncation.truncated {
                    crate::tools::truncate::truncation_json(&snapshot.truncation)
                } else {
                    Value::Null
                },
                "fullOutputPath": snapshot
                    .full_output_path
                    .as_ref()
                    .map(|path| path.display().to_string()),
            }),
            ..AgentToolResult::default()
        });
    }
    let last_line_bytes = accumulator.last_line_bytes();
    let (text, details) = format_output(&snapshot, last_line_bytes, "(no output)");

    if timed_out {
        return Err(ToolError(append_status(
            &text,
            &format!(
                "Command timed out after {} seconds",
                format_number(requested_timeout.unwrap_or_default())
            ),
        )));
    }
    if signal.aborted() {
        return Err(ToolError(append_status(&text, "Command aborted")));
    }

    let exit_code = exit_code_from(&status);
    let wall_time_seconds = round_tenth(started_at.elapsed().as_secs_f64());
    let full_output = accumulator
        .read_full_output(STRUCTURED_OUTPUT_MAX_BYTES)
        .map_err(|error| ToolError(error.to_string()))?;
    let mut structured = Map::new();
    structured.insert("output".to_string(), json!(full_output.content));
    structured.insert("truncated".to_string(), json!(full_output.truncated));
    if full_output.truncated {
        if let Some(path) = snapshot.full_output_path.as_ref() {
            structured.insert(
                "full_output_path".to_string(),
                json!(path.display().to_string()),
            );
        }
    }
    structured.insert("exit_code".to_string(), json!(exit_code));
    structured.insert("wall_time_seconds".to_string(), json!(wall_time_seconds));

    if exit_code != 0 {
        return Ok(AgentToolResult {
            content: vec![text_block(append_status(
                &text,
                &format!("Command exited with code {exit_code}"),
            ))],
            details,
            structured_content: Some(Value::Object(structured)),
            is_error: true,
            ..AgentToolResult::default()
        });
    }

    Ok(AgentToolResult {
        content: vec![text_block(text)],
        details,
        structured_content: Some(Value::Object(structured)),
        ..AgentToolResult::default()
    })
}

/// pi's `finishOutput` (`bash.ts:356-364`): stop accepting output, flush the
/// decoder, publish the final snapshot, and close the temp file.
fn finish_output(
    accumulator: &mut OutputAccumulator,
) -> crate::tools::output_accumulator::OutputSnapshot {
    accumulator.finish();
    let snapshot = accumulator.snapshot(true);
    accumulator.close_temp_file();
    snapshot
}

fn spawn(
    shell: &str,
    shell_args: &[String],
    command: &str,
    cwd: &Path,
    context: &ToolContext,
) -> Result<Child, ToolError> {
    let mut builder = Command::new(shell);
    builder
        .args(shell_args)
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env_clear();
    for (key, value) in context.shell_env() {
        builder.env(key, value);
    }
    #[cfg(unix)]
    {
        // pi spawns detached and kills the tree (`bash.ts:127`); a fresh
        // process group is the same lever.
        builder.process_group(0);
    }
    builder
        .spawn()
        .map_err(|error| ToolError(format!("Failed to run {shell}: {error}")))
}

/// pi's exit-code convention (`bash.ts:174-177`): a signal-killed shell has no
/// code, so the standard 128 + signal is used, and a missing code at all is a
/// failure.
fn exit_code_from(status: &std::io::Result<std::process::ExitStatus>) -> i32 {
    match status {
        Ok(status) => match status.code() {
            Some(code) => code,
            None => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    128 + status.signal().unwrap_or(0)
                }
                #[cfg(not(unix))]
                {
                    1
                }
            }
        },
        Err(_) => 1,
    }
}

/// pi's `scheduleOutputUpdate` / `emitOutputUpdate` throttle
/// (`bash.ts:331-344`).
fn maybe_publish(
    accumulator: &mut OutputAccumulator,
    updates: Option<&tokio::sync::mpsc::UnboundedSender<AgentToolResult>>,
    last_update_at: &mut Instant,
) {
    let Some(updates) = updates else {
        return;
    };
    if last_update_at.elapsed() < Duration::from_millis(BASH_UPDATE_THROTTLE_MS) {
        return;
    }
    *last_update_at = Instant::now();
    let snapshot = accumulator.snapshot(true);
    let _ = updates.send(AgentToolResult {
        content: vec![text_block(snapshot.content.clone())],
        details: json!({
            "truncation": if snapshot.truncation.truncated {
                crate::tools::truncate::truncation_json(&snapshot.truncation)
            } else {
                Value::Null
            },
            "fullOutputPath": snapshot
                .full_output_path
                .as_ref()
                .map(|path| path.display().to_string()),
        }),
        ..AgentToolResult::default()
    });
}

/// pi's `killProcessTree` (`utils/shell.ts:196-220`).
///
/// pi walks `ps` and signals every descendant. This port puts the child in its
/// own process group and signals the group, which is the same lever without
/// the ps scan; `kill` is spawned rather than calling `killpg` because the
/// crate is `#![forbid(unsafe_code)]`. The direct child is killed either way,
/// so a missing `kill` degrades to "the shell dies, its children are
/// reparented".
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // A negative pid addresses the process group.
        let _ = std::process::Command::new("/bin/kill")
            .arg("-TERM")
            .arg(format!("-{pid}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.start_kill();
}

/// pi's `Math.round(ms / 100) / 10` (`bash.ts:423`).
fn round_tenth(seconds: f64) -> f64 {
    (seconds * 10.0).round() / 10.0
}

/// pi's bash tool description (`bash.ts:283`), with the truncation numbers
/// filled in.
pub fn description() -> String {
    format!(
        "Execute a bash command in the current working directory. Returns stdout and stderr. \
Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). \
If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.",
        DEFAULT_MAX_BYTES / 1024
    )
}

/// pi's `bashToolSystemPromptContribution` (`bash.ts:44-47`).
pub const PROMPT_SNIPPET: &str = "Execute bash commands (ls, grep, find, etc.)";
/// pi's `bashToolSystemPromptContribution.guidelines`.
pub const PROMPT_GUIDELINES: [&str; 1] =
    ["You can inspect PI_* environment variables for current model and session details."];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_validation_matches_pi() {
        assert_eq!(resolve_timeout_ms(None).expect("no timeout"), None);
        assert_eq!(
            resolve_timeout_ms(Some(0.05)).expect("fractional"),
            Some(50)
        );
        let error = resolve_timeout_ms(Some(0.0)).unwrap_err();
        assert_eq!(
            error.0,
            "Invalid timeout: must be a finite number of seconds"
        );
        let error = resolve_timeout_ms(Some(-1.0)).unwrap_err();
        assert_eq!(
            error.0,
            "Invalid timeout: must be a finite number of seconds"
        );
        let error = resolve_timeout_ms(Some(2_147_483.648)).unwrap_err();
        assert_eq!(error.0, "Invalid timeout: maximum is 2147483.647 seconds");
    }

    #[test]
    fn shell_config_prefers_bin_bash_then_sh() {
        let (shell, args) = shell_config(None).expect("a shell");
        assert!(matches!(shell.as_str(), "/bin/bash" | "bash" | "sh"));
        assert_eq!(args, vec!["-c".to_string()], "pi never passes -l");
        let error = shell_config(Some("/no/such/shell-xyz")).unwrap_err();
        assert_eq!(error.0, "Custom shell path not found: /no/such/shell-xyz");
    }

    #[test]
    fn append_status_joins_with_a_blank_line() {
        assert_eq!(append_status("", "status"), "status");
        assert_eq!(append_status("text", "status"), "text\n\nstatus");
    }
}
