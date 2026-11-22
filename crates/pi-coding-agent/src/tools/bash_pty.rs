//! PTY-backed bash execution for the interactive `!` command.
//!
//! Ground truth: `packages/coding-agent/src/core/bash-executor.ts:20-148`
//! (`BashOptions`/`BashResult`, `appendText`, `onData`, `flushOutput`, the
//! rolling buffer, temp-file spill and tail truncation) and
//! `packages/coding-agent/src/core/tools/bash.ts:110-194`
//! (`createLocalShellOperations`: spawn, process-group kill, and the
//! `128 + signal` exit-code convention).
//!
//! Upstream 1.5.2 does **not** allocate a PTY: `bash.ts:129` spawns the shell
//! with `stdio: [pipe|ignore, pipe, pipe]`, so `!` is a non-interactive pipe
//! run whose merged output is streamed through a TUI component. The PTY
//! transport here is a deliberate port-level addition that gives `!` a real
//! controlling terminal (stdin attached, `\n` → `\r\n`, job-control signals),
//! while keeping the agent-tool seam in [`super::execute`] on the original
//! pipe path. See `WIRING.md` for the host hooks and the termios deviation.
//!
//! The pseudo-terminal itself comes from `script(1)` (util-linux): it forks a
//! child under `forkpty`, so the command sees a real tty, and it forwards
//! stdin, `\x03` (Ctrl-C), and signals to that child. Using the helper keeps
//! this crate `#![forbid(unsafe_code)]` — `openpty`/`grantpt`/`forkpty` are all
//! raw FFI otherwise — and avoids a new dependency.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::tools::render_utils::{sanitize_binary_output, strip_ansi};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, TruncationOptions, truncate_tail};

const READ_BUFFER: usize = 8192;
/// `^C`, written to the pty master so the kernel delivers `SIGINT` to the
/// foreground group — the interactive interrupt upstream's `AbortSignal`
/// approximates with `killProcessTree` (`bash.ts:145-147`).
const CTRL_C: u8 = 0x03;

/// pi's `BashResult` (`bash-executor.ts:27-38`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashPtyResult {
    /// Combined, sanitized, possibly truncated output.
    pub output: String,
    /// `None` when cancelled (`exitCode: cancelled ? undefined : …`,
    /// `bash-executor.ts:143`).
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    /// Temp file with the untruncated output, when a spill happened.
    pub full_output_path: Option<PathBuf>,
}

/// What the worker sends to the host: sanitized deltas as they arrive and
/// the final result exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BashPtyEvent {
    Chunk(String),
    Done(BashPtyResult),
}

/// A running pty command. The host drains [`BashPtyRun::events`] and calls
/// [`BashPtyControl::interrupt`] from its input loop.
pub struct BashPtyRun {
    pub control: BashPtyControl,
    pub events: std::sync::mpsc::Receiver<BashPtyEvent>,
    pub thread: Option<JoinHandle<()>>,
}

/// Live handle over a running command. Cloning shares the same child and
/// cancellation flag.
pub struct BashPtyControl {
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    pid: Arc<Mutex<Option<u32>>>,
    cancel: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}

impl BashPtyControl {
    fn new(cancel: Arc<AtomicBool>, finished: Arc<AtomicBool>) -> Self {
        Self {
            stdin: Arc::new(Mutex::new(None)),
            pid: Arc::new(Mutex::new(None)),
            cancel,
            finished,
        }
    }

    /// pi's `abortBash()` (`agent-session.ts:4067-4071`): request cancellation.
    /// The pty gets a real `^C` first so a foreground job can shut down
    /// cleanly; a watchdog escalates to `SIGKILL` if it ignores it.
    pub fn interrupt(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        let wrote = self
            .stdin
            .lock()
            .ok()
            .and_then(|mut guard| {
                guard
                    .as_mut()
                    .map(|stdin| stdin.write_all(&[CTRL_C]).is_ok())
            })
            .unwrap_or(false);
        if !wrote {
            self.kill();
            return;
        }
        let control = self.clone();
        let _ = std::thread::Builder::new()
            .name("bash-pty-watchdog".to_string())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(2));
                if !control.finished.load(Ordering::SeqCst) {
                    control.kill();
                }
            });
    }

    /// Hard kill: signal both the child and its process group. This is the
    /// same lever pi's `killProcessTree` pulls (`bash.ts:152-159`).
    pub fn kill(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        let pid = self.pid.lock().ok().and_then(|guard| *guard);
        let Some(pid) = pid else {
            return;
        };
        // A negative pid addresses the process group (`bash.ts:127`'s
        // `detached` + `killProcessTree`). `kill(1)` is used instead of a
        // `killpg` FFI because the crate forbids unsafe code.
        for target in [format!("-{pid}"), pid.to_string()] {
            let _ = Command::new("/bin/kill")
                .arg("-KILL")
                .arg(target)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    /// Forward raw host bytes (arrow keys, `q`, …) into the pty so the
    /// command is genuinely interactive. The host owns the terminal hook;
    /// see `WIRING.md`.
    pub fn write_stdin(&self, bytes: &[u8]) {
        if let Ok(mut guard) = self.stdin.lock()
            && let Some(stdin) = guard.as_mut()
        {
            let _ = stdin.write_all(bytes);
        }
    }
}

impl Clone for BashPtyControl {
    fn clone(&self) -> Self {
        Self {
            stdin: Arc::clone(&self.stdin),
            pid: Arc::clone(&self.pid),
            cancel: Arc::clone(&self.cancel),
            finished: Arc::clone(&self.finished),
        }
    }
}

/// The command and working directory for one pty run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashPtyRequest {
    pub command: String,
    pub cwd: PathBuf,
}

/// Start a command under a pseudo-terminal. Output arrives on
/// [`BashPtyRun::events`]; the caller must drain it until `Done`.
pub fn run(request: BashPtyRequest) -> Result<BashPtyRun, String> {
    if !request.cwd.exists() {
        return Err(format!(
            "Working directory does not exist: {}\nCannot execute bash commands.",
            request.cwd.display()
        ));
    }
    let Some(script) = which("script") else {
        return Err(
            "PTY bash requires `script(1)` (util-linux); install util-linux or use the agent bash tool"
                .to_string(),
        );
    };
    let shell = shell_path();
    // `script -c` runs its argument under `/bin/sh -c`; `exec` keeps the
    // shell in the pty's foreground group so `^C` reaches the job directly.
    let inner = format!("exec {shell} -c {}", shell_quote(&request.command));
    let cancel = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let control = BashPtyControl::new(Arc::clone(&cancel), Arc::clone(&finished));
    let (tx, events) = std::sync::mpsc::channel();
    let shared = control.clone();

    let thread = std::thread::Builder::new()
        .name("bash-pty".to_string())
        .spawn(move || {
            let result = worker(&script, &inner, &request.cwd, &shared, &cancel, &tx);
            finished.store(true, Ordering::SeqCst);
            let _ = tx.send(BashPtyEvent::Done(result));
        })
        .map_err(|error| format!("Failed to start the bash pty thread: {error}"))?;

    Ok(BashPtyRun {
        control,
        events,
        thread: Some(thread),
    })
}

/// The worker owns the child and the pty master's read end until the command
/// ends. Every read is turned into one sanitized delta; on cancellation the
/// output produced so far is still returned (`bash-executor.ts:121-130`).
fn worker(
    script: &Path,
    inner: &str,
    cwd: &Path,
    control: &BashPtyControl,
    cancel: &Arc<AtomicBool>,
    tx: &std::sync::mpsc::Sender<BashPtyEvent>,
) -> BashPtyResult {
    let mut child = match Command::new(script)
        .arg("-q")
        .arg("-e")
        .arg("-c")
        .arg(inner)
        .arg("/dev/null")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // The command's stderr is merged into the pty's stdout; anything
        // `script` itself writes is diagnostic noise, so there is no pipe to
        // drain (a full pipe would stall the child).
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return BashPtyResult {
                output: format!("Failed to start bash: {error}"),
                exit_code: Some(1),
                cancelled: false,
                truncated: false,
                full_output_path: None,
            };
        }
    };
    let pid = child.id();
    if let Ok(mut guard) = control.pid.lock() {
        *guard = Some(pid);
    }
    if let Some(pipe) = child.stdin.take()
        && let Ok(mut guard) = control.stdin.lock()
    {
        *guard = Some(pipe);
    }
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut output = PtyOutput::default();
    let mut buffer = vec![0u8; READ_BUFFER];
    loop {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        // A blocking read is fine: `^C`/kill makes the child close the pty,
        // which ends the read. The watchdog bounds a stubborn job.
        let Ok(read) = stdout.read(&mut buffer) else { break };
        if read == 0 {
            break;
        }
        let raw = String::from_utf8_lossy(&buffer[..read]);
        if let Some(text) = output.append(&raw) {
            let _ = tx.send(BashPtyEvent::Chunk(text));
        }
    }
    if let Ok(mut guard) = control.stdin.lock() {
        *guard = None;
    }
    let status = child.wait();
    output.finish(cancel.load(Ordering::SeqCst), Some(exit_code_from(&status)))
}

/// The rolling output window of `appendText` (`bash-executor.ts:77-106`),
/// with the same character-count approximation used by the extension shell
/// bridge (`extensions/mod.rs:320-370`).
#[derive(Default)]
struct PtyOutput {
    chunks: Vec<String>,
    window_chars: usize,
    streamed_chars: usize,
    discarded: bool,
    temp: Option<(PathBuf, std::fs::File)>,
}

impl PtyOutput {
    /// `stripAnsi` → `sanitizeBinaryOutput` → `\r` removal, then the rolling
    /// window and the temp-file spill (`bash-executor.ts:78-105`).
    fn append(&mut self, raw: &str) -> Option<String> {
        let text = sanitize_binary_output(&strip_ansi(raw)).replace('\r', "");
        if text.is_empty() {
            return None;
        }
        let chars = text.chars().count();
        self.streamed_chars += chars;
        if self.temp.is_none() && self.streamed_chars > DEFAULT_MAX_BYTES {
            // `ensureTempFile` (`bash-executor.ts:63-71`): the buffered
            // window rides along so the file opens with everything seen so
            // far. A file error degrades to "no spill" — the tail truncation
            // below still produces a valid result.
            let path = crate::tools::output_accumulator::temp_file_path("pi-bash");
            if let Ok(mut file) = std::fs::File::create(&path) {
                for buffered in &self.chunks {
                    let _ = file.write_all(buffered.as_bytes());
                }
                self.temp = Some((path, file));
            }
        }
        if let Some((_, file)) = self.temp.as_mut() {
            let _ = file.write_all(text.as_bytes());
        }
        self.window_chars += chars;
        self.chunks.push(text.clone());
        while self.window_chars > DEFAULT_MAX_BYTES * 2 && self.chunks.len() > 1 {
            let removed = self.chunks.remove(0);
            self.window_chars -= removed.chars().count();
            self.discarded = true;
        }
        Some(text)
    }

    /// `flushOutput` + the final result (`bash-executor.ts:115-147`).
    fn finish(mut self, cancelled: bool, exit_code: Option<i32>) -> BashPtyResult {
        if let Some((_, file)) = self.temp.as_mut() {
            let _ = file.flush();
        }
        let full = self.chunks.concat();
        let truncation = truncate_tail(&full, TruncationOptions::default());
        let full_output_path = if truncation.truncated {
            if let Some((path, file)) = self.temp.take() {
                let _ = file.sync_all();
                Some(path)
            } else {
                let path = crate::tools::output_accumulator::temp_file_path("pi-bash");
                std::fs::write(&path, &full).ok().map(|()| path)
            }
        } else {
            self.temp.map(|(path, file)| {
                let _ = file.sync_all();
                path
            })
        };
        BashPtyResult {
            output: truncation.content,
            exit_code: if cancelled { None } else { exit_code },
            cancelled,
            truncated: self.discarded || truncation.truncated,
            full_output_path,
        }
    }
}

/// pi's signal convention (`bash.ts:174-177`): a signal-killed shell reports
/// `128 + signal` rather than no exit code.
fn exit_code_from(status: &std::io::Result<ExitStatus>) -> i32 {
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

/// pi's Unix shell discovery (`utils/shell.ts:108-118`): `/bin/bash`, then
/// `bash` on `PATH`, then `sh`. The agent tool owns the configurable
/// `shellPath`; the interactive seam mirrors upstream's default.
fn shell_path() -> &'static str {
    if Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else if which("bash").is_some() {
        "bash"
    } else {
        "sh"
    }
}

/// A PATH lookup (the job `findExecutableOnPath` does for shell discovery).
fn which(executable: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(executable))
        .find(|candidate| candidate.is_file())
}

/// Single-quote a command for the outer `/bin/sh -c` that `script` spawns.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn script_available() -> bool {
        which("script").is_some()
    }

    /// Drain a run to its `Done` event, collecting streamed chunks.
    #[allow(clippy::needless_pass_by_value)] // the test helper consumes its run
    fn collect(run: BashPtyRun) -> (Vec<String>, BashPtyResult) {
        let mut chunks = Vec::new();
        for event in &run.events {
            match event {
                BashPtyEvent::Chunk(text) => chunks.push(text),
                BashPtyEvent::Done(result) => return (chunks, result),
            }
        }
        panic!("the pty run ended without a Done event");
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("echo hi"), "'echo hi'");
        assert_eq!(shell_quote("echo 'hi'"), "'echo '\\''hi'\\'''");
    }

    #[test]
    fn append_sanitizes_ansi_binary_and_carriage_returns() {
        let mut output = PtyOutput::default();
        let delta = output.append("\x1b[31mred\x1b[0m\u{0}ok\r\n");
        assert_eq!(delta, Some("redok\n".to_string()));
        assert_eq!(output.append(""), None);
        let result = output.finish(false, Some(0));
        assert_eq!(result.output, "redok\n");
        assert_eq!(result.exit_code, Some(0));
        assert!(!result.truncated);
        assert_eq!(result.full_output_path, None);
    }

    #[test]
    fn echo_streams_incrementally_and_captures_exit_code() {
        if !script_available() {
            return;
        }
        let run = run(BashPtyRequest {
            command: "printf 'alpha\\nbeta\\n'; exit 0".to_string(),
            cwd: std::env::current_dir().expect("cwd"),
        })
        .expect("run");
        let (chunks, result) = collect(run);
        assert!(!chunks.is_empty(), "the pty streamed at least one chunk");
        assert_eq!(result.output, "alpha\nbeta\n");
        assert_eq!(result.exit_code, Some(0));
        assert!(!result.cancelled);
        assert!(!result.truncated);
    }

    #[test]
    fn nonzero_exit_code_is_reported() {
        if !script_available() {
            return;
        }
        let run = run(BashPtyRequest {
            command: "printf 'before exit\\n'; exit 7".to_string(),
            cwd: std::env::current_dir().expect("cwd"),
        })
        .expect("run");
        let (_, result) = collect(run);
        assert_eq!(result.output, "before exit\n");
        assert_eq!(result.exit_code, Some(7));
    }

    #[test]
    fn interrupt_cancels_sleep() {
        if !script_available() {
            return;
        }
        let run = run(BashPtyRequest {
            command: "printf 'started\\n'; sleep 30".to_string(),
            cwd: std::env::current_dir().expect("cwd"),
        })
        .expect("run");
        let started = Instant::now();
        std::thread::sleep(Duration::from_millis(200));
        run.control.interrupt();
        let (chunks, result) = collect(run);
        assert!(
            chunks.concat().contains("started"),
            "output before the interrupt survives"
        );
        assert!(result.cancelled);
        assert_eq!(result.exit_code, None);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "interrupt should end the command promptly"
        );
    }

    #[test]
    fn binary_output_is_truncated_and_spilled() {
        if !script_available() {
            return;
        }
        let run = run(BashPtyRequest {
            command: "head -c 60000 /dev/zero | tr '\\0' x".to_string(),
            cwd: std::env::current_dir().expect("cwd"),
        })
        .expect("run");
        let (_, result) = collect(run);
        assert!(result.truncated);
        assert!(
            result.output.chars().count() < 60_000,
            "the tail is bounded"
        );
        let path = result.full_output_path.expect("a spill file");
        assert!(path.exists());
        assert!(
            std::fs::read(&path).expect("spill contents").len() >= 50_000,
            "the spill holds the full output"
        );
        // Synthetic fixture: the spill file lives under the OS temp dir and
        // is unique per run; leave it for the harness to reap.
    }
}
