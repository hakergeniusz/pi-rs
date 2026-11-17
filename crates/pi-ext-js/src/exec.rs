//! `pi.exec`: the process primitive extensions get
//! (`core/exec.ts:49-162`, `execCommand`).
//!
//! Ported semantics: no shell (`shell: false`), stdin ignored, combined
//! stdout/stderr byte cap (`maxBuffer`, default 256 MiB) whose overflow
//! truncates output, terminates the command, and reports `truncated: true`
//! with a nonzero code; a timeout or abort terminates with SIGTERM first
//! and SIGKILL after a 5 s grace, reporting `killed: true`; a
//! signal-terminated command gets the shell-convention exit code
//! `128 + signal` (`#12088`); a spawn failure (ENOENT) is a failed
//! execution with code 1 and the error as stderr, not a host error; a
//! truncated result never reads as success.
//!
//! E1 differences: abort signals do not cross the engine boundary yet (E1
//! runs host actions synchronously), so the `signal` option is accepted
//! and ignored — recorded in HANDOFF.md.

use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

/// Combined stdout/stderr byte cap: upstream's default
/// `min(256 MiB, MAX_STRING_LENGTH)` where `MAX_STRING_LENGTH` is V8's
/// 512 MiB-ish string limit (`2^29 - 24`).
const DEFAULT_MAX_BUFFER: u64 = 256 * 1024 * 1024;
/// V8's `bufferConstants.MAX_STRING_LENGTH`, the cap upstream validates
/// against.
const MAX_STRING_LENGTH: u64 = (1 << 29) - 24;
/// SIGTERM grace before SIGKILL (`exec.ts:12`).
const FORCE_KILL_DELAY: Duration = Duration::from_secs(5);
/// Poll interval while waiting for the child.
const WAIT_POLL: Duration = Duration::from_millis(25);

/// The `ExecResult` upstream returns (`exec.ts:34-43`).
#[derive(Debug, Clone, Serialize)]
pub struct ExecResult {
    /// Collected stdout (within the cap).
    pub stdout: String,
    /// Collected stderr (within the cap).
    pub stderr: String,
    /// Exit code, or `128 + signal` when a signal terminated the command
    /// (143 for SIGTERM).
    pub code: i32,
    /// The command was terminated by pi: timeout or output over `maxBuffer`.
    pub killed: bool,
    /// Output exceeded `maxBuffer`; stdout and stderr are incomplete and
    /// `code` is nonzero.
    pub truncated: bool,
}

/// One `pi.exec` call, returning the JSON the op hands back to JS
/// (`{stdout, stderr, code, killed, truncated}` or `{execError}`).
pub fn exec(
    command: &str,
    args: &[String],
    cwd: &str,
    options: &serde_json::Value,
) -> serde_json::Value {
    let max_buffer = match options.get("maxBuffer") {
        None | Some(serde_json::Value::Null) => DEFAULT_MAX_BUFFER,
        Some(value) => {
            let Some(raw) = value.as_u64() else {
                return exec_error(&format!(
                    "maxBuffer must be an integer between 0 and {MAX_STRING_LENGTH} bytes"
                ));
            };
            if raw > MAX_STRING_LENGTH {
                return exec_error(&format!(
                    "maxBuffer must be an integer between 0 and {MAX_STRING_LENGTH} bytes"
                ));
            }
            raw
        }
    };
    let timeout_ms = options
        .get("timeout")
        .and_then(serde_json::Value::as_u64)
        .filter(|timeout| *timeout > 0);
    let cwd = options
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(cwd);

    let child = Command::new(command)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        // Child-process errors (e.g. ENOENT) are failed executions, not
        // host failures (exec.ts:132-137).
        Err(error) => {
            return serde_json::json!({
                "stdout": "", "stderr": error.to_string(), "code": 1, "killed": false, "truncated": false
            });
        }
    };

    let cap = Arc::new(Mutex::new(CapState {
        bytes: 0,
        truncated: false,
    }));
    let pid = child.id();
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_handle = stdout_pipe.map(|pipe| spawn_reader(pipe, cap.clone(), max_buffer, pid));
    let stderr_handle = stderr_pipe.map(|pipe| spawn_reader(pipe, cap.clone(), max_buffer, pid));

    let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
    let mut killed = false;
    let mut force_kill_at: Option<Instant> = None;
    let status = loop {
        if let Some(deadline) = deadline
            && !killed
            && Instant::now() >= deadline
        {
            killed = true;
            send_signal(pid, 15); // SIGTERM
            force_kill_at = Some(Instant::now() + FORCE_KILL_DELAY);
        }
        if let Some(at) = force_kill_at
            && Instant::now() >= at
        {
            send_signal(pid, 9); // SIGKILL
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(WAIT_POLL),
            Err(error) => {
                return serde_json::json!({
                    "stdout": "", "stderr": error.to_string(), "code": 1, "killed": killed, "truncated": false
                });
            }
        }
    };

    let stdout = stdout_handle
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr_handle
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    let truncated = cap.lock().expect("cap mutex poisoned").truncated;
    // Truncation killed the command too (upstream `killProcess` sets
    // `killed` for timeout, abort, and overflow alike).
    let killed = killed || truncated;

    // A signal-killed command has no exit code; use the shell convention
    // (exec.ts:151-153). A truncated result must never read as success.
    let mut code = status
        .code()
        .unwrap_or_else(|| status.signal().map_or(1, |signal| 128 + signal));
    if truncated && code == 0 {
        code = 1;
    }
    serde_json::to_value(ExecResult {
        stdout,
        stderr,
        code,
        killed,
        truncated,
    })
    .unwrap_or_else(|_| exec_error("exec result could not be serialized"))
}

fn exec_error(message: &str) -> serde_json::Value {
    serde_json::json!({ "execError": message })
}

/// Shared output-cap accounting (`takeWithinCap`, `exec.ts:105-117`): the
/// combined byte count, and whether the cap was hit (readers stop and
/// close their pipes once it is).
struct CapState {
    bytes: u64,
    truncated: bool,
}

/// Sends one signal to the spawned pid. The child may have exited between
/// the check and the signal; that race is harmless (upstream guards with
/// `proc.killed` for the same reason), and ESRCH is ignored.
fn send_signal(pid: u32, signal: i32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let Ok(signal) = Signal::try_from(signal) else {
        return;
    };
    let _ = kill(Pid::from_raw(pid as i32), signal);
}

fn spawn_reader(
    mut pipe: impl Read + Send + 'static,
    cap: Arc<Mutex<CapState>>,
    max_buffer: u64,
    pid: u32,
) -> std::thread::JoinHandle<String> {
    std::thread::Builder::new()
        .name("pi-ext-js-exec-reader".to_string())
        .spawn(move || {
            let mut out: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                {
                    let state = cap.lock().expect("cap mutex poisoned");
                    if state.truncated {
                        // Close the pipe so writers and descendants cannot
                        // keep the command busy after the cap (exec.ts:102-104).
                        drop(pipe);
                        return String::from_utf8_lossy(&out).into_owned();
                    }
                }
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        let mut state = cap.lock().expect("cap mutex poisoned");
                        if state.truncated {
                            drop(pipe);
                            return String::from_utf8_lossy(&out).into_owned();
                        }
                        let remaining = max_buffer.saturating_sub(state.bytes);
                        let take = (read as u64).min(remaining) as usize;
                        state.bytes += take as u64;
                        out.extend_from_slice(&chunk[..take]);
                        if take < read {
                            state.truncated = true;
                            drop(state);
                            send_signal(pid, 15); // SIGTERM
                            drop(pipe);
                            return String::from_utf8_lossy(&out).into_owned();
                        }
                    }
                }
            }
            String::from_utf8_lossy(&out).into_owned()
        })
        .expect("spawn exec reader thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(command: &str, args: &[&str], options: &serde_json::Value) -> serde_json::Value {
        exec(
            command,
            &args
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>(),
            "/tmp",
            options,
        )
    }

    #[test]
    fn captures_stdout_and_exit_code() {
        let result = run("/bin/sh", &["-c", "echo hello"], &serde_json::json!({}));
        assert_eq!(result["stdout"], "hello\n");
        assert_eq!(result["code"], 0);
        assert_eq!(result["truncated"], false);
        assert_eq!(result["killed"], false);
    }

    #[test]
    fn spawn_failure_is_a_failed_execution() {
        let result = run("/nonexistent-command-e1", &[], &serde_json::json!({}));
        assert_eq!(result["code"], 1);
        assert!(
            result["stderr"]
                .as_str()
                .expect("stderr")
                .contains("No such file")
        );
    }

    #[test]
    fn max_buffer_overflow_truncates_and_fails() {
        let result = run(
            "/bin/sh",
            &["-c", "printf 'abcdef'"],
            &serde_json::json!({ "maxBuffer": 3 }),
        );
        assert_eq!(result["stdout"], "abc");
        assert_eq!(result["truncated"], true);
        assert_eq!(result["killed"], true);
        assert_ne!(result["code"], 0);
    }

    #[test]
    fn invalid_max_buffer_is_a_range_error() {
        let result = run(
            "/bin/echo",
            &["x"],
            &serde_json::json!({ "maxBuffer": (1 << 29) - 23 }),
        );
        assert!(
            result["execError"]
                .as_str()
                .expect("error")
                .contains("maxBuffer must be an integer")
        );
    }

    #[test]
    fn timeout_kills_the_command() {
        // Spawn the binary directly: `sh -c "sleep 30"` is unsafe here —
        // dash (CI's /bin/sh) forks instead of exec-ing, so SIGTERM kills
        // the shell and the orphaned sleep keeps the pipe write ends open
        // until its natural exit (hit on CI 2026-11-15; bash exec-optimizes,
        // which masked it locally).
        let started = Instant::now();
        let result = run(
            "/bin/sleep",
            &["30"],
            &serde_json::json!({ "timeout": 100 }),
        );
        assert_eq!(result["killed"], true);
        assert_eq!(result["code"], 143);
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
