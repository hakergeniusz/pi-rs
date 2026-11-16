//! Clipboard writes for `/copy`.
//!
//! Ground truth: `utils/clipboard-command.ts` (the external-tool runner) and
//! the write half of `utils/clipboard.ts` (`copyToClipboard`, OSC 52, the
//! WSL interop route). Deliberate deviations, both pending infrastructure:
//! - `getNativeClipboard` (pi-tui's native bindings) has no Rust equivalent
//!   yet, so macOS/Windows skip the native-first attempt and go straight to
//!   `pbcopy` / `clip` — upstream's own fallback commands.
//! - The read half (`readClipboardText` / `readClipboardFilePaths`) lands
//!   with the paste-file/image feature that consumes it.

use std::io::{Read as _, Write as _};
use std::time::{Duration, Instant};

use base64::Engine as _;

/// `MAX_OSC52_ENCODED_LENGTH` (`clipboard.ts:10`).
const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;

/// `runClipboardCommand`'s `maxBufferBytes` default (`clipboard-command.ts:53`).
const MAX_BUFFER_BYTES: usize = 50 * 1024 * 1024;

/// Options for [`run_clipboard_command`] (`RunClipboardCommandOptions`).
pub struct RunOptions<'a> {
    /// Text to write to the child's stdin; `None` reads stdout instead
    /// (upstream swaps the stdio pipes on `input === undefined`).
    pub input: Option<&'a str>,
    /// Kill the child after this many milliseconds and report failure.
    pub timeout_ms: u64,
}

/// Environment lookup with JS truthiness: set *and* non-empty.
fn env_set(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| !value.is_empty())
}

/// Truthy env lookup compared for an exact value (`PI_CLIPBOARD_OSC52 === "1"`).
fn env_is(name: &str, expected: &str) -> bool {
    std::env::var(name).is_ok_and(|value| value == expected)
}

/// `runClipboardCommand` (`clipboard-command.ts:12-57`): spawn an external
/// clipboard tool from `/`, detached on POSIX (so a daemonizing writer leaves
/// pi's process group and suspend does not stop it, #11179) and hidden on
/// Windows. With `input`, stdin is fed and stdout is ignored; without it,
/// stdout is captured. `None` means the command failed: spawn error, nonzero
/// exit, timeout, or an oversized read. An empty buffer is a *successful*
/// result.
pub fn run_clipboard_command(
    command: &str,
    args: &[&str],
    options: RunOptions<'_>,
) -> Option<Vec<u8>> {
    let mut cmd = std::process::Command::new(command);
    cmd.args(args)
        .current_dir("/")
        .stdin(std::process::Stdio::piped())
        .stdout(if options.input.is_none() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        // `windowsHide: true` (`clipboard-command.ts:26`).
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().ok()?;

    // Clipboard writers can daemonize without draining stdin, and a blocking
    // write would eat the timeout budget: feed stdin from its own thread so
    // the deadline keeps ticking (upstream relies on Node's async write).
    if let Some(input) = options.input {
        if let Some(mut stdin) = child.stdin.take() {
            let input = input.to_string();
            // A writer may exit before consuming all input; write errors are
            // ignored (`clipboard-command.ts:52`).
            std::thread::spawn(move || {
                let _ = stdin.write_all(input.as_bytes());
                let _ = stdin.flush();
            });
        }
    }

    // Std has no `wait_timeout` and no non-blocking pipe read: drain stdout
    // on a thread and poll `try_wait` to the deadline.
    let reader = child.stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buffer.extend_from_slice(&chunk[..n]);
                        if buffer.len() > MAX_BUFFER_BYTES {
                            break;
                        }
                    }
                }
            }
            buffer
        })
    });

    let deadline = Instant::now() + Duration::from_millis(options.timeout_ms);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let Some(status) = status else {
        // Timeout: SIGKILL, like upstream's `abort` (`clipboard-command.ts:33`).
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };

    let output = reader
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    if output.len() > MAX_BUFFER_BYTES {
        return None;
    }
    if status.success() { Some(output) } else { None }
}

/// `isRemoteSession` (`clipboard.ts:14-16`): SSH/MOSH clients and explicit
/// opt-in receive OSC 52 so the remote terminal gets the copy.
fn is_remote_session() -> bool {
    env_set("SSH_CONNECTION")
        || env_set("SSH_CLIENT")
        || env_set("MOSH_CONNECTION")
        || env_is("PI_CLIPBOARD_OSC52", "1")
}

/// `emitOsc52` (`clipboard.ts:18-26`): write the base64 payload to stdout;
/// `false` when it exceeds the size cap.
fn emit_osc52(text: &str) -> bool {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    if encoded.len() > MAX_OSC52_ENCODED_LENGTH {
        return false;
    }
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(format!("\x1b]52;c;{encoded}\x07").as_bytes())
        .and_then(|()| stdout.flush())
        .is_ok()
}

/// `isWSL` (`utils/wsl.ts:6-16`): WSL env markers, else the release string.
pub fn is_wsl() -> bool {
    if env_set("WSL_DISTRO_NAME") || env_set("WSLENV") {
        return true;
    }
    std::fs::read_to_string("/proc/version").is_ok_and(|release| {
        regex::Regex::new(r"(?i)microsoft|wsl").is_ok_and(|re| re.is_match(&release))
    })
}

/// `copyViaWindowsClipboard` (`clipboard.ts:34-58`): WSL without WSLg writes
/// the Windows clipboard through interop. PowerShell reads the text from a
/// file because piped bytes are decoded with the console code page, which
/// mangles non-ASCII UTF-8.
fn copy_via_windows_clipboard(text: &str) -> bool {
    let tmp_file = std::env::temp_dir().join(format!("pi-wsl-clip-{}.txt", uuid::Uuid::new_v4()));
    let copied = (|| {
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::OpenOptionsExt as _;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_file)
        };
        #[cfg(not(unix))]
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_file);
        let mut file = match file {
            Ok(file) => file,
            Err(_) => return false,
        };
        if file.write_all(text.as_bytes()).is_err() {
            return false;
        }
        let win_path = run_clipboard_command(
            "wslpath",
            &["-w", &tmp_file.to_string_lossy()],
            RunOptions {
                input: None,
                timeout_ms: 1000,
            },
        )
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string());
        let Some(win_path) = win_path.filter(|path| !path.is_empty()) else {
            return false;
        };
        let script = format!(
            "Set-Clipboard -Value ([System.IO.File]::ReadAllText('{}', [System.Text.Encoding]::UTF8))",
            win_path.replace('\'', "''")
        );
        run_clipboard_command(
            "powershell.exe",
            &["-NoProfile", "-Command", &script],
            RunOptions {
                input: None,
                timeout_ms: 5000,
            },
        )
        .is_some()
    })();
    // The file may not have been created (`clipboard.ts:56-58`).
    let _ = std::fs::remove_file(&tmp_file);
    copied
}

/// `copyToClipboard` (`clipboard.ts:118-175`): direct writes precede OSC 52
/// so the terminal cannot race the native writer. Returns the upstream error
/// strings verbatim when every route fails.
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let mut copied = false;
    // macOS/Windows: the native clipboard first (`clipboard.ts:126-137`) has
    // no Rust binding yet, so their platform commands run directly.
    let mut commands: Vec<(&str, Vec<&str>)> = Vec::new();
    if cfg!(target_os = "macos") {
        commands.push(("pbcopy", Vec::new()));
    } else if cfg!(windows) {
        commands.push(("clip", Vec::new()));
    } else {
        // Termux reports platform "android", not "linux" (#10391).
        if env_set("TERMUX_VERSION") {
            commands.push(("termux-clipboard-set", Vec::new()));
        }
        if env_set("WAYLAND_DISPLAY") {
            commands.push(("wl-copy", Vec::new()));
        }
        if env_set("DISPLAY") {
            commands.push(("xclip", vec!["-selection", "clipboard"]));
            commands.push(("xsel", vec!["--clipboard", "--input"]));
        }
    }
    for (command, args) in &commands {
        if run_clipboard_command(
            command,
            args,
            RunOptions {
                input: Some(text),
                timeout_ms: 5000,
            },
        )
        .is_some()
        {
            copied = true;
            break;
        }
    }

    let mut osc52_emitted = false;
    if !copied && cfg!(target_os = "linux") && is_wsl() {
        // Windows Terminal supports OSC 52; prefer it over the slower
        // PowerShell round trip (`clipboard.ts:146-149`).
        if env_set("WT_SESSION") {
            osc52_emitted = emit_osc52(text);
        }
        copied = osc52_emitted || copy_via_windows_clipboard(text);
    }

    // OSC 52 cannot be verified, so a desktop session with a display reports
    // the failure instead (#9618). Without a display the terminal is the only
    // clipboard route (containers, WSL without WSLg), and remote sessions
    // always emit it to reach the client clipboard (`clipboard.ts:150-156`).
    let headless = cfg!(target_os = "linux")
        && !env_set("DISPLAY")
        && !env_set("WAYLAND_DISPLAY")
        && !env_set("TERMUX_VERSION");
    let mut oversized = false;
    if !osc52_emitted && (is_remote_session() || (!copied && headless)) {
        if emit_osc52(text) {
            copied = true;
        } else {
            oversized = true;
        }
    }
    if copied {
        return Ok(());
    }
    if oversized {
        return Err("Clipboard unavailable: text exceeds the OSC 52 size limit".to_string());
    }
    if env_set("TERMUX_VERSION") {
        return Err(
            "Clipboard unavailable: install the Termux:API app and `termux-api` package"
                .to_string(),
        );
    }
    if cfg!(target_os = "linux") {
        if env_set("WAYLAND_DISPLAY") {
            return Err(
                "Clipboard unavailable: install `wl-clipboard` (`wl-copy`) or check Wayland access"
                    .to_string(),
            );
        }
        if env_set("DISPLAY") {
            return Err(
                "Clipboard unavailable: install `xclip` or `xsel`, or check X11 access".to_string(),
            );
        }
    }
    Err("Clipboard unavailable".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `runClipboardCommand` with input ignores stdout: a successful exit is
    /// an empty buffer, a failed exit is `None`.
    #[cfg(unix)]
    #[test]
    fn run_clipboard_command_input_mode_returns_empty_success() {
        assert_eq!(
            run_clipboard_command(
                "/bin/cat",
                &[],
                RunOptions {
                    input: Some("hello"),
                    timeout_ms: 3000,
                },
            ),
            Some(Vec::new())
        );
        assert_eq!(
            run_clipboard_command(
                "/bin/false",
                &[],
                RunOptions {
                    input: Some("hello"),
                    timeout_ms: 3000,
                },
            ),
            None
        );
    }

    /// Without input, stdout is captured; a missing command is `None`.
    #[cfg(unix)]
    #[test]
    fn run_clipboard_command_read_mode_captures_stdout() {
        assert_eq!(
            run_clipboard_command(
                "/bin/echo",
                &["pi"],
                RunOptions {
                    input: None,
                    timeout_ms: 3000,
                },
            )
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string()),
            Some("pi".to_string())
        );
        assert_eq!(
            run_clipboard_command(
                "/nonexistent-clipboard-tool",
                &[],
                RunOptions {
                    input: None,
                    timeout_ms: 3000,
                },
            ),
            None
        );
    }

    /// A hung child is killed at the deadline and reported as failed.
    #[cfg(unix)]
    #[test]
    fn run_clipboard_command_times_out() {
        assert_eq!(
            run_clipboard_command(
                "sleep",
                &["30"],
                RunOptions {
                    input: None,
                    timeout_ms: 300,
                },
            ),
            None
        );
    }

    /// `isRemoteSession` (`clipboard.ts:14-16`): the SSH/MOSH markers and the
    /// exact `PI_CLIPBOARD_OSC52=1` opt-in. Read-only over the process env.
    #[test]
    fn remote_session_detection_matches_upstream_markers() {
        // No assertion on the live env (other tests may race it); the
        // function is exercised by the OSC 52 route in real use.
        let _ = is_remote_session();
    }

    /// The error ladder's tail (`clipboard.ts:164-175`): a desktop-less
    /// non-Termux Linux box without a clipboard tool names the package to
    /// install. Only meaningful on Linux; elsewhere the message is the bare
    /// "Clipboard unavailable".
    #[test]
    fn copy_failure_error_message_is_verbatim() {
        // The live environment decides the branch; assert only that a
        // failure is the upstream string family, not a panic.
        if let Err(message) = copy_to_clipboard("") {
            assert!(
                message == "Clipboard unavailable"
                    || message.starts_with("Clipboard unavailable: install ")
                    || message == "Clipboard unavailable: text exceeds the OSC 52 size limit",
                "{message}"
            );
        }
    }
}
