//! SSH transport: targets, host keys, platform detection and daemon deploy.
//!
//! Ground truth: `packages/env/src/ssh.ts:1-542`. Ported behaviour:
//! `RemotePlatform`/`SshTarget` and the three error classes (`:10-55`),
//! argument hardening with `checkField`/`configPath`/`sshArguments`
//! (`:59-123`), `runSsh` and its host-key diagnostics (`:125-178`),
//! `scanHostKey` and known-hosts editing (`:180-289`), the platform probes and
//! `detectPlatform` (`:291-356`), daemon naming/hash checking/deploy
//! (`:358-464`) and the lazy `sshConnection`/`connectSsh` shape (`:466-542`).
//!
//! # Deviations
//!
//! * All `ssh` invocations go through the injectable [`SshRunner`] trait;
//!   [`SystemSshRunner`] uses `tokio::process`. Tests use a fake runner and
//!   never contact a real host.
//! * SSH timeouts and the daemon upload timeout are passed to the runner but
//!   enforced by the host (no timers in the port).
//! * Known-hosts writes are atomic but do not take the per-file in-process
//!   serialization map from `ssh.ts:227-254` — the port is single-threaded
//!   per host and documents the gap.

use std::process::Stdio;
use std::{
    error::Error,
    fmt::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt, process::Command as TokioCommand};

use crate::{
    connection::{BoxFuture, CommandSource, ConnectionOptions, Daemon, ProcessDaemon},
    errors::RemoteError,
};

/// Timeouts from `ssh.ts:125-127`, forwarded to the runner.
pub const SSH_TIMEOUT_MS: u64 = 60_000;
/// Upload timeout (`ssh.ts:127`).
pub const UPLOAD_TIMEOUT_MS: u64 = 300_000;

/// A remote system the package ships a daemon for (`ssh.ts:10-20`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemotePlatform {
    /// Operating system.
    pub platform: Platform,
    /// CPU architecture.
    pub arch: Arch,
    /// Remote home directory, in the remote's own spelling.
    pub home: String,
    /// Windows: the shell `sshd` runs commands with.
    pub shell: Option<Shell>,
    /// Problems worth telling the owner about.
    pub warnings: Vec<String>,
}

/// Platform variants (`RemotePlatform["platform"]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// Linux.
    Linux,
    /// Android (Termux).
    Android,
    /// macOS.
    Darwin,
    /// Windows.
    Windows,
}

impl Platform {
    /// The daemon asset suffix.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Android => "android",
            Self::Darwin => "darwin",
            Self::Windows => "windows",
        }
    }
}

/// Architecture variants (`RemotePlatform["arch"]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    /// x86-64.
    X64,
    /// ARM64 / `AArch64`.
    Arm64,
}

impl Arch {
    /// The daemon asset suffix.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X64 => "x64",
            Self::Arm64 => "arm64",
        }
    }
}

/// The remote shell on Windows (`RemotePlatform["shell"]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    /// `cmd.exe`.
    Cmd,
    /// PowerShell.
    Powershell,
}

/// How to reach a machine with the system `ssh` (`SshTarget`, `ssh.ts:22-37`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshTarget {
    /// Host name or `~/.ssh/config` alias.
    pub host: String,
    /// Remote user.
    pub user: Option<String>,
    /// Remote port.
    pub port: Option<i64>,
    /// Identity file path.
    pub identity_file: Option<String>,
    /// Application-owned known-hosts file.
    pub known_hosts_file: String,
    /// Stable alias keys are stored under.
    pub host_key_alias: String,
    /// The `ssh` program; defaults to `ssh`.
    pub ssh: Option<String>,
    /// An `ssh` configuration file instead of `~/.ssh/config` (`-F`).
    pub config_file: Option<String>,
}

/// The remote host's key is not trusted yet (`HostKeyUnknownError`,
/// `ssh.ts:39`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyUnknownError {
    /// The diagnostic.
    pub message: String,
}

impl fmt::Display for HostKeyUnknownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for HostKeyUnknownError {}

/// The remote host's key changed (`HostKeyChangedError`, `ssh.ts:43`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyChangedError {
    /// The diagnostic.
    pub message: String,
}

impl fmt::Display for HostKeyChangedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for HostKeyChangedError {}

/// An `ssh` invocation that failed for another reason (`SshError`,
/// `ssh.ts:45-55`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshError {
    /// The diagnostic.
    pub message: String,
    /// The child's exit code, or `None` on a timeout/spawn error.
    pub exit_code: Option<i32>,
    /// Raw stderr.
    pub stderr: String,
}

impl fmt::Display for SshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for SshError {}

/// Result of one command run by an [`SshRunner`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandOutput {
    /// Captured stdout as UTF-8 (lossy).
    pub stdout: String,
    /// Captured stderr as UTF-8 (lossy).
    pub stderr: String,
    /// The child's exit code, if it exited.
    pub exit_code: Option<i32>,
}

/// Injectable SSH command runner. Implementations must not prompt or forward
/// anything; argument construction is [`ssh_arguments`]'s job.
pub trait SshRunner: Send + Sync {
    /// Run `program` with `args`, feed `stdin` and capture both streams.
    fn run(
        &self,
        program: &str,
        args: &[String],
        stdin: &[u8],
        timeout_ms: u64,
    ) -> BoxFuture<'_, Result<CommandOutput, RemoteError>>;
}

/// The real runner, using `tokio::process` (`runSsh`, `ssh.ts:129-178`).
pub struct SystemSshRunner;

impl SshRunner for SystemSshRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        stdin: &[u8],
        _timeout_ms: u64,
    ) -> BoxFuture<'_, Result<CommandOutput, RemoteError>> {
        let program = program.to_owned();
        let args = args.to_vec();
        let stdin = stdin.to_vec();
        Box::pin(async move {
            let mut child = TokioCommand::new(program)
                .args(&args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|error| RemoteError::new("spawn_error", error.to_string()))?;
            if let Some(mut writer) = child.stdin.take() {
                writer
                    .write_all(&stdin)
                    .await
                    .map_err(|error| RemoteError::new("spawn_error", error.to_string()))?;
            }
            let output = child
                .wait_with_output()
                .await
                .map_err(|error| RemoteError::new("spawn_error", error.to_string()))?;
            Ok(CommandOutput {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code(),
            })
        })
    }
}

/// `Invalid <name>: <JSON string>` for a field with a leading `-`, whitespace
/// or control characters (`checkField`, `ssh.ts:60-64`).
fn check_field(name: &str, value: &str) -> Result<(), RemoteError> {
    let invalid = value.is_empty()
        || value.starts_with('-')
        || value.chars().any(|character| {
            character.is_whitespace() || character.is_control() || character == '\u{7f}'
        });
    if invalid {
        return Err(RemoteError::new(
            "invalid",
            format!(
                "Invalid {name}: {}",
                serde_json::to_string(value).unwrap_or_default()
            ),
        ));
    }
    Ok(())
}

/// Quote an `ssh` option path, doubling `%` (`configPath`, `ssh.ts:66-70`).
fn config_path(name: &str, path: &str) -> Result<String, RemoteError> {
    if path.is_empty()
        || path
            .chars()
            .any(|character| character == '"' || character.is_control() || character == '\u{7f}')
    {
        return Err(RemoteError::new(
            "invalid",
            format!(
                "Invalid {name}: {}",
                serde_json::to_string(path).unwrap_or_default()
            ),
        ));
    }
    Ok(format!("\"{}\"", path.replace('%', "%%")))
}

/// The arguments up to the host (`sshArguments`, `ssh.ts:77-123`). No prompts,
/// forwarding, control connections, configured commands or locale forwarding;
/// host keys are checked strictly under a fixed alias.
pub fn ssh_arguments(
    target: &SshTarget,
    strict_host_keys: bool,
    known_hosts_file: &str,
) -> Result<Vec<String>, RemoteError> {
    check_field("host", &target.host)?;
    check_field("host key alias", &target.host_key_alias)?;
    if let Some(user) = &target.user {
        check_field("user", user)?;
    }
    if let Some(port) = target.port
        && !(1..=65_535).contains(&port)
    {
        return Err(RemoteError::new("invalid", format!("Invalid port: {port}")));
    }
    let mut args = Vec::new();
    if let Some(config) = &target.config_file {
        args.push("-F".into());
        args.push(config.clone());
    }
    args.extend(["-T".into(), "-a".into(), "-x".into()]);
    let option = |setting: &str| ["-o".to_owned(), setting.to_owned()];
    args.extend(option("BatchMode=yes"));
    args.extend(option("ClearAllForwardings=yes"));
    args.extend(option("ForwardAgent=no"));
    args.extend(option("ForwardX11=no"));
    args.extend(option("ControlMaster=no"));
    args.extend(option("ControlPath=none"));
    args.extend(option("RemoteCommand=none"));
    args.extend(option("PermitLocalCommand=no"));
    args.extend(option("SendEnv=-*"));
    args.extend(option("ConnectTimeout=15"));
    args.extend(option("ServerAliveInterval=15"));
    args.extend(option(if strict_host_keys {
        "StrictHostKeyChecking=yes"
    } else {
        "StrictHostKeyChecking=accept-new"
    }));
    args.extend(option(&format!(
        "UserKnownHostsFile={}",
        config_path("known hosts file", known_hosts_file)?
    )));
    args.extend(option("GlobalKnownHostsFile=none"));
    args.extend(option("HashKnownHosts=no"));
    args.extend(option(&format!("HostKeyAlias={}", target.host_key_alias)));
    if let Some(user) = &target.user {
        args.extend(["-l".into(), user.clone()]);
    }
    if let Some(port) = target.port {
        args.extend(["-p".into(), port.to_string()]);
    }
    if let Some(identity) = &target.identity_file {
        args.extend(option(&format!(
            "IdentityFile={}",
            config_path("identity file", identity)?
        )));
        args.extend(option("IdentitiesOnly=yes"));
    }
    args.push("--".into());
    args.push(target.host.clone());
    Ok(args)
}

/// Options for one `run_ssh` call.
pub struct SshRunOptions {
    /// Bytes fed to stdin.
    pub stdin: Vec<u8>,
    /// Override the arguments (used by `scanHostKey`).
    pub args: Option<Vec<String>>,
    /// Timeout forwarded to the runner.
    pub timeout_ms: u64,
}

impl Default for SshRunOptions {
    fn default() -> Self {
        Self {
            stdin: Vec::new(),
            args: None,
            timeout_ms: SSH_TIMEOUT_MS,
        }
    }
}

/// Run one command over `ssh`, classifying host-key failures
/// (`runSsh`, `ssh.ts:129-178`).
pub async fn run_ssh(
    runner: &dyn SshRunner,
    target: &SshTarget,
    command: &str,
    options: SshRunOptions,
) -> Result<String, RemoteError> {
    let base_args = match options.args {
        Some(args) => args,
        None => ssh_arguments(target, true, &target.known_hosts_file)?,
    };
    let mut argv = base_args;
    argv.push(command.to_owned());
    let output = runner
        .run(
            target.ssh.as_deref().unwrap_or("ssh"),
            &argv,
            &options.stdin,
            options.timeout_ms,
        )
        .await?;
    if output.exit_code == Some(0) {
        return Ok(output.stdout);
    }
    let diagnostics = output.stderr;
    if diagnostics.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        return Err(RemoteError::new(
            "host_key_changed",
            format!(
                "The host key of {} changed; remove the old key to continue",
                target.host
            ),
        ));
    }
    if diagnostics.contains("Host key verification failed")
        || diagnostics
            .lines()
            .any(|line| line.trim_start().starts_with("No ") && line.contains("host key is known"))
    {
        return Err(RemoteError::new(
            "host_key_unknown",
            format!("The host key of {} is not trusted yet", target.host),
        ));
    }
    let code = output
        .exit_code
        .map_or_else(|| "null".to_owned(), |code| code.to_string());
    let message = format!(
        "ssh {} failed with exit code {code}: {}",
        target.host,
        diagnostics.trim()
    );
    let mut error = RemoteError::new("ssh_error", message);
    error.fields.insert(
        "exitCode".into(),
        output.exit_code.map_or(Value::Null, Value::from),
    );
    error
        .fields
        .insert("stderr".into(), Value::String(diagnostics));
    Err(error)
}

/// The host key lines and fingerprints captured by `scanHostKey`
/// (`ssh.ts:185-210`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyScan {
    /// Known-hosts lines.
    pub lines: Vec<String>,
    /// `ssh-keygen -lf` output lines.
    pub fingerprints: Vec<String>,
}

/// `scanHostKey` against an explicit scratch directory (`ssh.ts:185-210`),
/// factored out so tests never touch a real temp location with real keys.
pub async fn scan_host_key_with(
    runner: &dyn SshRunner,
    target: &SshTarget,
    directory: &Path,
) -> Result<HostKeyScan, RemoteError> {
    let scanned = directory.join("known_hosts");
    let path = scanned.to_string_lossy().into_owned();
    let args = ssh_arguments(target, false, &path)?;
    match run_ssh(
        runner,
        target,
        "exit 0",
        SshRunOptions {
            args: Some(args),
            ..Default::default()
        },
    )
    .await
    {
        Ok(_) => {}
        // Authentication may fail before the key is recorded; that is the
        // expected path for `accept-new` (`ssh.ts:189-192`).
        Err(ref error) if error.code == "ssh_error" || error.code == "host_key_unknown" => {}
        Err(error) => return Err(error),
    }
    let lines = if scanned.exists() {
        std::fs::read_to_string(&scanned)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if lines.is_empty() {
        return Err(RemoteError::new(
            "unknown",
            format!("No host key received from {}", target.host),
        ));
    }
    let keygen_args = vec!["-lf".to_owned(), path];
    let fingerprints = runner
        .run("ssh-keygen", &keygen_args, &[], SSH_TIMEOUT_MS)
        .await?
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect();
    Ok(HostKeyScan {
        lines,
        fingerprints,
    })
}

/// `scanHostKey` (`ssh.ts:185-210`) with a unique scratch directory.
pub async fn scan_host_key(
    runner: &dyn SshRunner,
    target: &SshTarget,
) -> Result<HostKeyScan, RemoteError> {
    let directory =
        std::env::temp_dir().join(format!("pi-env-hostkey-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&directory)
        .map_err(|error| RemoteError::new("unknown", error.to_string()))?;
    let result = scan_host_key_with(runner, target, &directory).await;
    let _ = std::fs::remove_dir_all(&directory);
    result
}

/// A parsed host-key line (`parseHostKeyLine`, `ssh.ts:212-225`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyLine {
    /// Full trimmed line to store.
    pub line: String,
    /// Key type (`ssh-ed25519`, …).
    pub key_type: String,
    /// Base64 key material.
    pub key: String,
}

fn key_type_ok(text: &str) -> bool {
    let suffix_ok = |rest: &str, base64: bool| {
        !rest.is_empty()
            && rest.chars().all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || character == '-'
                    || (base64 && (character == '@' || character == '.'))
            })
    };
    if let Some(rest) = text.strip_prefix("ssh-") {
        suffix_ok(rest, false)
    } else if let Some(rest) = text.strip_prefix("ecdsa-sha2-") {
        suffix_ok(rest, false)
    } else if let Some(rest) = text.strip_prefix("sk-") {
        suffix_ok(rest, true)
    } else {
        false
    }
}

fn key_base64_ok(text: &str) -> bool {
    let body = text.trim_end_matches('=');
    !body.is_empty()
        && text.len() - body.len() <= 2
        && body.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '+' || character == '/'
        })
}

/// Parse a plain known-hosts line for `alias`, rejecting markers, patterns
/// and hashed names (`ssh.ts:212-225`).
pub fn parse_host_key_line(alias: &str, line: &str) -> Result<HostKeyLine, RemoteError> {
    let trimmed = line.trim();
    let mut fields = trimmed.split_whitespace();
    let host = fields.next();
    let key_type = fields.next();
    let key = fields.next();
    let valid =
        host == Some(alias) && key_type.is_some_and(key_type_ok) && key.is_some_and(key_base64_ok);
    if !valid {
        return Err(RemoteError::new(
            "invalid",
            format!(
                "Not a host key line for {alias}: {}",
                serde_json::to_string(line).unwrap_or_default()
            ),
        ));
    }
    Ok(HostKeyLine {
        line: trimmed.to_owned(),
        key_type: key_type.unwrap_or_default().to_owned(),
        key: key.unwrap_or_default().to_owned(),
    })
}

/// Add trusted lines for an alias (`acceptHostKey`'s change function,
/// `ssh.ts:261-282`). A key of the same type that differs is refused.
pub fn apply_accept(
    existing: &[String],
    lines: &[&str],
    alias: &str,
    host: &str,
) -> Result<Vec<String>, RemoteError> {
    let accepted = lines
        .iter()
        .map(|line| parse_host_key_line(alias, line))
        .collect::<Result<Vec<_>, _>>()?;
    let mut trusted = std::collections::BTreeMap::new();
    for line in existing {
        let mut fields = line.split_whitespace();
        if let (Some(name), Some(key_type), Some(key)) =
            (fields.next(), fields.next(), fields.next())
            && name == alias
        {
            trusted.insert(key_type.to_owned(), key.to_owned());
        }
    }
    let mut added = Vec::new();
    for entry in accepted {
        match trusted.get(&entry.key_type) {
            Some(known) if known == &entry.key => {}
            Some(_) => {
                return Err(RemoteError::new(
                    "host_key_changed",
                    format!(
                        "The {} host key of {host} changed; forget the old key first",
                        entry.key_type
                    ),
                ));
            }
            None => {
                trusted.insert(entry.key_type.clone(), entry.key.clone());
                added.push(entry.line);
            }
        }
    }
    let mut result = existing.to_vec();
    result.extend(added);
    Ok(result)
}

/// Remove every line for an alias (`forgetHostKey`, `ssh.ts:284-289`).
#[must_use]
pub fn apply_forget(existing: &[String], alias: &str) -> Vec<String> {
    existing
        .iter()
        .filter(|line| line.split_whitespace().next() != Some(alias))
        .cloned()
        .collect()
}

/// Write a known-hosts file atomically (`changeKnownHosts`, `ssh.ts:230-254`).
pub fn write_known_hosts(path: &Path, lines: &[String]) -> Result<(), RemoteError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| RemoteError::new("unknown", error.to_string()))?;
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let content = if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    };
    std::fs::write(&temporary, content)
        .map_err(|error| RemoteError::new("unknown", error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600));
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(RemoteError::new("unknown", error.to_string()));
    }
    Ok(())
}

/// POSIX detection script run by the login shell (`ssh.ts:292`).
pub const POSIX_PROBE: &str = "sh -c 'echo PI-ENV-PROBE; uname -s; uname -m; uname -o 2>/dev/null || echo -; printf \"%s\\n\" \"$HOME\" \"${TMPDIR:--}\" \"${LD_PRELOAD:--}\"'";

/// Wrap a script in PowerShell's encoded-command form (`powershell`,
/// `ssh.ts:294-296`).
#[must_use]
pub fn powershell(script: &str) -> String {
    let bytes = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    format!(
        "powershell -NoProfile -NonInteractive -EncodedCommand {}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// Windows detection script (`ssh.ts:298-300`).
#[must_use]
pub fn windows_probe() -> String {
    powershell(
        "'PI-ENV-PROBE'; 'Windows'; [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString(); '-'; $HOME; '-'; '-'",
    )
}

/// The raw probe answers, with login-shell banners skipped (`ssh.ts:309-318`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    /// `uname -s` (or `Windows`).
    pub system: String,
    /// `uname -m`.
    pub machine: String,
    /// `uname -o`.
    pub os: String,
    /// `$HOME`.
    pub home: String,
    /// `$TMPDIR`.
    pub tmpdir: String,
    /// `$LD_PRELOAD`.
    pub preload: String,
}

/// `parseProbe` (`ssh.ts:311-318`).
pub fn parse_probe(host: &str, output: &str) -> Result<Probe, RemoteError> {
    let lines = output.lines().collect::<Vec<_>>();
    let Some(start) = lines.iter().position(|line| *line == "PI-ENV-PROBE") else {
        return Err(RemoteError::new(
            "unknown",
            format!("Unexpected answer from {host}: {}", output.trim()),
        ));
    };
    let field = |offset: usize, default: &str| -> String {
        lines
            .get(start + 1 + offset)
            .map_or_else(|| default.to_owned(), |line| (*line).to_owned())
    };
    Ok(Probe {
        system: field(0, ""),
        machine: field(1, ""),
        os: field(2, ""),
        home: field(3, ""),
        tmpdir: field(4, "-"),
        preload: field(5, "-"),
    })
}

/// `normalizeArch` (`ssh.ts:302-307`).
pub fn normalize_arch(machine: &str) -> Result<Arch, RemoteError> {
    match machine.to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => Ok(Arch::X64),
        "aarch64" | "arm64" => Ok(Arch::Arm64),
        other => Err(RemoteError::new(
            "unknown",
            format!("Unsupported remote architecture: {other}"),
        )),
    }
}

/// Termux warnings (`ssh.ts:320-329`), with upstream's exact wording.
#[must_use]
pub fn termux_warnings(probe: &Probe) -> Vec<String> {
    let mut warnings = Vec::new();
    if probe.tmpdir == "-" {
        warnings
            .push("TMPDIR is not set; Termux's sshd normally sets it to $PREFIX/tmp.".to_owned());
    }
    if !probe.preload.contains("termux-exec") {
        warnings.push("termux-exec is not loaded (LD_PRELOAD); scripts with #!/usr/bin/env shebangs will fail.".to_owned());
    }
    warnings.push("Android may suspend Termux; run termux-wake-lock on the device to keep the connection alive.".to_owned());
    warnings
}

/// `detectPlatform` (`ssh.ts:331-356`): `uname` through the login shell, or
/// PowerShell on Windows / Git Bash.
pub async fn detect_platform(
    runner: &dyn SshRunner,
    target: &SshTarget,
) -> Result<RemotePlatform, RemoteError> {
    let mut probe = match run_ssh(runner, target, POSIX_PROBE, SshRunOptions::default()).await {
        Ok(output) => parse_probe(&target.host, &output)?,
        Err(error) if error.code == "host_key_unknown" || error.code == "host_key_changed" => {
            return Err(error);
        }
        Err(_) => parse_probe(
            &target.host,
            &run_ssh(runner, target, &windows_probe(), SshRunOptions::default()).await?,
        )?,
    };
    if probe.system.starts_with("MINGW")
        || probe.system.starts_with("MSYS")
        || probe.system.starts_with("CYGWIN")
    {
        probe = parse_probe(
            &target.host,
            &run_ssh(runner, target, &windows_probe(), SshRunOptions::default()).await?,
        )?;
    }
    let arch = normalize_arch(&probe.machine)?;
    match probe.system.as_str() {
        "Windows" => {
            let shell = if run_ssh(runner, target, "echo %OS%", SshRunOptions::default())
                .await?
                .contains("Windows_NT")
            {
                Shell::Cmd
            } else {
                Shell::Powershell
            };
            Ok(RemotePlatform {
                platform: Platform::Windows,
                arch,
                home: probe.home,
                shell: Some(shell),
                warnings: Vec::new(),
            })
        }
        "Darwin" => Ok(RemotePlatform {
            platform: Platform::Darwin,
            arch,
            home: probe.home,
            shell: None,
            warnings: Vec::new(),
        }),
        "Linux" => {
            let android = probe.os == "Android";
            let warnings = if android {
                termux_warnings(&probe)
            } else {
                Vec::new()
            };
            Ok(RemotePlatform {
                platform: if android {
                    Platform::Android
                } else {
                    Platform::Linux
                },
                arch,
                home: probe.home,
                shell: None,
                warnings,
            })
        }
        other => Err(RemoteError::new(
            "unknown",
            format!("Unsupported remote system: {other}"),
        )),
    }
}

/// The daemon binary this package ships (`packagedDaemon`, `ssh.ts:358-371`).
#[must_use]
pub fn packaged_daemon(remote: &RemotePlatform, package_root: &Path) -> PathBuf {
    let name = format!(
        "pi-env-{}-{}",
        remote.platform.as_str(),
        remote.arch.as_str()
    );
    package_root
        .join("bin")
        .join(name)
        .join(if remote.platform == Platform::Windows {
            "pi-env.exe"
        } else {
            "pi-env"
        })
}

/// The daemon for the machine the port runs on (`localDaemon`, `ssh.ts:364-371`).
pub fn local_daemon(package_root: &Path) -> Result<PathBuf, RemoteError> {
    let platform = match std::env::consts::OS {
        "windows" => Platform::Windows,
        "macos" => Platform::Darwin,
        "linux" => Platform::Linux,
        "android" => Platform::Android,
        other => {
            return Err(RemoteError::new(
                "unknown",
                format!("No pi-env daemon for {other}"),
            ));
        }
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => Arch::X64,
        "aarch64" => Arch::Arm64,
        other => return Err(normalize_arch(other).expect_err("unsupported local arch")),
    };
    Ok(packaged_daemon(
        &RemotePlatform {
            platform,
            arch,
            home: String::new(),
            shell: None,
            warnings: Vec::new(),
        },
        package_root,
    ))
}

fn quote_posix(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn quote_powershell(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Where a daemon with this content lives (`daemonPath`, `ssh.ts:381-387`).
#[must_use]
pub fn daemon_path(remote: &RemotePlatform, sha256: &str) -> String {
    let name = format!("pi-env-{}", &sha256[..32.min(sha256.len())]);
    if remote.platform == Platform::Windows {
        format!("{}\\.pi\\mobile\\tools\\{name}.exe", remote.home)
    } else {
        format!("{}/.pi/mobile/tools/{name}", remote.home)
    }
}

/// POSIX hash helper (`POSIX_HASH`, `ssh.ts:389`).
pub const POSIX_HASH: &str = "hash() { if command -v sha256sum >/dev/null 2>&1; then sha256sum \"$1\" | cut -d' ' -f1; elif command -v shasum >/dev/null 2>&1; then shasum -a 256 \"$1\" | cut -d' ' -f1; elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 \"$1\" | sed 's/.*= //'; else echo none; fi; }";

/// Parse a `checkDaemon` answer (`ssh.ts:391-401`).
#[must_use]
pub fn parse_check_daemon(output: &str, windows: bool) -> String {
    if windows {
        return if output.trim().ends_with("present") {
            "present"
        } else {
            "missing"
        }
        .to_owned();
    }
    output
        .trim()
        .lines()
        .last()
        .unwrap_or("missing")
        .trim()
        .to_owned()
}

/// Verify or deploy the daemon and return its remote path
/// (`deployDaemon`, `ssh.ts:403-464`).
pub async fn deploy_daemon(
    runner: &dyn SshRunner,
    target: &SshTarget,
    remote: &RemotePlatform,
    binary: &Path,
) -> Result<String, RemoteError> {
    if !binary.exists() {
        return Err(RemoteError::new(
            "unknown",
            format!(
                "No pi-env daemon for {}-{} at {}",
                remote.platform.as_str(),
                remote.arch.as_str(),
                binary.display()
            ),
        ));
    }
    let bytes =
        std::fs::read(binary).map_err(|error| RemoteError::new("unknown", error.to_string()))?;
    let sha256 = hex_digest(&Sha256::digest(&bytes));
    let file = daemon_path(remote, &sha256);
    let check = if remote.platform == Platform::Windows {
        powershell(&format!(
            "$f = {}; if ((Test-Path -LiteralPath $f) -and ((Get-FileHash -Algorithm SHA256 -LiteralPath $f).Hash.ToLower() -eq '{sha256}')) {{ 'present' }} else {{ 'missing' }}",
            quote_powershell(&file)
        ))
    } else {
        format!(
            "sh -c {}",
            quote_posix(&format!(
                "{POSIX_HASH}; f={}; if [ -f \"$f\" ] && [ \"$(hash \"$f\")\" = {sha256} ]; then echo present; elif [ \"$(hash /dev/null)\" = none ]; then echo nohash; else echo missing; fi",
                quote_posix(&file)
            ))
        )
    };
    let state = parse_check_daemon(
        &run_ssh(runner, target, &check, SshRunOptions::default()).await?,
        remote.platform == Platform::Windows,
    );
    if state == "present" {
        return Ok(file);
    }
    if state == "nohash" {
        return Err(RemoteError::new(
            "unknown",
            format!(
                "{} has no sha256sum, shasum or openssl to verify pi-env",
                target.host
            ),
        ));
    }
    if remote.platform == Platform::Windows {
        let upload = powershell(&format!(
            "$ErrorActionPreference = 'Stop'; $f = {}; $d = Split-Path -Parent $f; New-Item -ItemType Directory -Force -Path $d | Out-Null; $t = Join-Path $d ('.pi-env-' + [guid]::NewGuid().ToString() + '.tmp'); $text = New-Object System.Text.StringBuilder; foreach ($line in $input) {{ if ($line -eq 'PI-ENV-END') {{ break }}; [void]$text.Append($line) }}; $bytes = [Convert]::FromBase64String($text.ToString()); $out = [IO.File]::Open($t, 'CreateNew', 'Write', 'None'); $out.Write($bytes, 0, $bytes.Length); $out.Close(); if ((Get-FileHash -Algorithm SHA256 -LiteralPath $t).Hash.ToLower() -ne '{sha256}') {{ Remove-Item -LiteralPath $t; throw 'pi-env upload is corrupt' }}; for ($i = 0; ; $i++) {{ try {{ Move-Item -Force -LiteralPath $t -Destination $f; break }} catch {{ if ($i -ge 20) {{ throw }}; Start-Sleep -Milliseconds 250 }} }}; Get-ChildItem -LiteralPath $d -Filter 'pi-env-*.exe' | Where-Object {{ $_.FullName -ne $f }} | ForEach-Object {{ Remove-Item -LiteralPath $_.FullName -ErrorAction SilentlyContinue }}; 'deployed'",
            quote_powershell(&file)
        ));
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let mut stdin = encoded.as_bytes().to_vec();
        stdin.extend_from_slice(b"PI-ENV-END\n");
        run_ssh(
            runner,
            target,
            &upload,
            SshRunOptions {
                stdin,
                timeout_ms: UPLOAD_TIMEOUT_MS,
                ..Default::default()
            },
        )
        .await?;
    } else {
        let upload = [
            "set -e".to_owned(),
            POSIX_HASH.to_owned(),
            format!("f={}", quote_posix(&file)),
            "d=$(dirname \"$f\")".to_owned(),
            "mkdir -p \"$d\"".to_owned(),
            "chmod 700 \"$d\"".to_owned(),
            "t=$(mktemp \"$d/.pi-env.XXXXXX\")".to_owned(),
            "cat > \"$t\"".to_owned(),
            format!("if [ \"$(hash \"$t\")\" != {sha256} ]; then rm -f \"$t\"; echo \"pi-env upload is corrupt\" >&2; exit 1; fi"),
            "chmod 700 \"$t\"".to_owned(),
            "mv -f \"$t\" \"$f\"".to_owned(),
            "for old in \"$d\"/pi-env-*; do [ \"$old\" = \"$f\" ] || rm -f \"$old\"; done".to_owned(),
            "echo deployed".to_owned(),
        ]
        .join("\n");
        run_ssh(
            runner,
            target,
            &format!("sh -c {}", quote_posix(&upload)),
            SshRunOptions {
                stdin: bytes,
                timeout_ms: UPLOAD_TIMEOUT_MS,
                ..Default::default()
            },
        )
        .await?;
    }
    Ok(file)
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// The command that starts the daemon (`launchCommand`, `ssh.ts:477-487`).
#[must_use]
pub fn launch_command(remote: &RemotePlatform, file: &str, login_shell: bool) -> String {
    if remote.platform == Platform::Windows {
        if remote.shell == Some(Shell::Powershell) {
            return format!("& {}", quote_powershell(file));
        }
        return if file.contains(' ') {
            format!("\"{file}\"")
        } else {
            file.to_owned()
        };
    }
    if login_shell {
        format!(
            "exec \"$SHELL\" -lc 'exec \"$0\" \"$@\"' {}",
            quote_posix(file)
        )
    } else {
        quote_posix(file)
    }
}

/// Options for `sshConnection`/`connectSsh` (`SshConnectOptions`,
/// `ssh.ts:466-475`).
#[derive(Clone)]
pub struct SshConnectOptions {
    /// The SSH target.
    pub target: SshTarget,
    /// Binary to deploy; defaults to the one beside the daemon.
    pub binary: Option<PathBuf>,
    /// Start through the user's login shell on POSIX.
    pub login_shell: bool,
    /// Receives warnings and SSH diagnostics.
    pub on_log: Option<crate::connection::LogSink>,
}

#[derive(Default)]
struct SshState {
    remote: Option<RemotePlatform>,
    verified: Option<String>,
}

/// What an SSH connection learned about its target (`sshConnection`'s return,
/// `ssh.ts:516-528`).
pub struct SshConnection {
    daemon: ProcessDaemon,
    state: Arc<Mutex<SshState>>,
}

impl SshConnection {
    /// The detected platform, once known (`remote()`, `ssh.ts:524-527`).
    #[must_use]
    pub fn remote(&self) -> Option<RemotePlatform> {
        self.state.lock().expect("ssh state lock").remote.clone()
    }

    /// The daemon-shaped connection to hand to an environment.
    #[must_use]
    pub fn daemon(&self) -> &dyn Daemon {
        &self.daemon
    }
}

/// Forward the [`Daemon`] surface so a host can keep one
/// `Arc<dyn Daemon>` whether the daemon was spawned locally or reached over
/// SSH. This is a pure delegation to [`SshConnection::daemon`]; no extra
/// connection or state is introduced.
impl Daemon for SshConnection {
    fn info(
        &self,
        abort: Option<crate::errors::AbortSignal>,
    ) -> BoxFuture<'_, Result<crate::connection::RemoteInfo, RemoteError>> {
        self.daemon.info(abort)
    }

    fn request(
        &self,
        op: &str,
        json: crate::JsonMap,
        options: crate::connection::RequestOptions,
    ) -> BoxFuture<'_, Result<crate::connection::Reply, RemoteError>> {
        self.daemon.request(op, json, options)
    }

    fn kill(&self, id: u64, session: u64) -> BoxFuture<'_, ()> {
        self.daemon.kill(id, session)
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        self.daemon.close()
    }

    fn tick(&self, now_ms: u64) -> BoxFuture<'_, ()> {
        self.daemon.tick(now_ms)
    }
}

fn command_builder(
    options: SshConnectOptions,
    runner: Arc<dyn SshRunner>,
    state: Arc<Mutex<SshState>>,
) -> CommandSource {
    CommandSource::Dynamic(Arc::new(move || {
        let options = options.clone();
        let runner = runner.clone();
        let state = state.clone();
        Box::pin(async move {
            let mut detected = None;
            {
                let current = state.lock().expect("ssh state lock");
                if let Some(remote) = &current.remote {
                    detected = Some(remote.clone());
                }
            }
            let remote = if let Some(remote) = detected {
                remote
            } else {
                let remote = detect_platform(runner.as_ref(), &options.target).await?;
                if let Some(log) = &options.on_log {
                    for warning in &remote.warnings {
                        log(&format!("{warning}\n"));
                    }
                }
                state.lock().expect("ssh state lock").remote = Some(remote.clone());
                remote
            };
            let verified = state.lock().expect("ssh state lock").verified.take();
            let package_root = PathBuf::from(".");
            let file = if let Some(file) = verified {
                file
            } else {
                let binary = options
                    .binary
                    .clone()
                    .unwrap_or_else(|| packaged_daemon(&remote, &package_root));
                deploy_daemon(runner.as_ref(), &options.target, &remote, &binary).await?
            };
            let mut argv = vec![options.target.ssh.clone().unwrap_or_else(|| "ssh".into())];
            argv.extend(ssh_arguments(
                &options.target,
                true,
                &options.target.known_hosts_file,
            )?);
            argv.push(launch_command(&remote, &file, options.login_shell));
            Ok(argv)
        })
    }))
}

/// A connection that does nothing until its first request
/// (`sshConnection`, `ssh.ts:492-528`), using an injected runner.
#[must_use]
// By value to mirror upstream's `sshConnection(opts, runner)` public shape.
#[allow(clippy::needless_pass_by_value)]
pub fn ssh_connection_with(
    options: SshConnectOptions,
    runner: Arc<dyn SshRunner>,
) -> SshConnection {
    let state = Arc::new(Mutex::new(SshState::default()));
    let command = command_builder(options.clone(), runner, state.clone());
    let mut connection_options = ConnectionOptions::new(command);
    connection_options.on_log.clone_from(&options.on_log);
    SshConnection {
        daemon: ProcessDaemon::new(connection_options),
        state,
    }
}

/// [`ssh_connection_with`] with the real system `ssh`.
#[must_use]
pub fn ssh_connection(options: SshConnectOptions) -> SshConnection {
    ssh_connection_with(options, Arc::new(SystemSshRunner))
}

/// Detect and deploy now, then return a connection
/// (`connectSsh`, `ssh.ts:530-542`), using an injected runner.
pub async fn connect_ssh_with(
    options: SshConnectOptions,
    runner: Arc<dyn SshRunner>,
) -> Result<(SshConnection, RemotePlatform), RemoteError> {
    let remote = detect_platform(runner.as_ref(), &options.target).await?;
    let package_root = PathBuf::from(".");
    let binary = options
        .binary
        .clone()
        .unwrap_or_else(|| packaged_daemon(&remote, &package_root));
    let verified = deploy_daemon(runner.as_ref(), &options.target, &remote, &binary).await?;
    if let Some(log) = &options.on_log {
        for warning in &remote.warnings {
            log(&format!("{warning}\n"));
        }
    }
    let state = Arc::new(Mutex::new(SshState {
        remote: Some(remote.clone()),
        verified: Some(verified),
    }));
    let command = command_builder(options.clone(), runner, state.clone());
    let mut connection_options = ConnectionOptions::new(command);
    connection_options.on_log.clone_from(&options.on_log);
    Ok((
        SshConnection {
            daemon: ProcessDaemon::new(connection_options),
            state,
        },
        remote,
    ))
}

/// [`connect_ssh_with`] with the real system `ssh`.
pub async fn connect_ssh(
    options: SshConnectOptions,
) -> Result<(SshConnection, RemotePlatform), RemoteError> {
    connect_ssh_with(options, Arc::new(SystemSshRunner)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::FakeSshRunner;

    fn target() -> SshTarget {
        SshTarget {
            host: "box".into(),
            user: Some("pi".into()),
            port: Some(2222),
            identity_file: Some("/keys/id".into()),
            known_hosts_file: "/tmp/known_hosts".into(),
            host_key_alias: "pi-env-test".into(),
            ssh: None,
            config_file: Some("/tmp/config".into()),
        }
    }

    #[test]
    fn ssh_arguments_match_upstream_hardening() {
        let args = ssh_arguments(&target(), true, "/tmp/known_hosts").expect("args");
        let expected = [
            "-F",
            "/tmp/config",
            "-T",
            "-a",
            "-x",
            "-o",
            "BatchMode=yes",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "ForwardAgent=no",
            "-o",
            "ForwardX11=no",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "RemoteCommand=none",
            "-o",
            "PermitLocalCommand=no",
            "-o",
            "SendEnv=-*",
            "-o",
            "ConnectTimeout=15",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "UserKnownHostsFile=\"/tmp/known_hosts\"",
            "-o",
            "GlobalKnownHostsFile=none",
            "-o",
            "HashKnownHosts=no",
            "-o",
            "HostKeyAlias=pi-env-test",
            "-l",
            "pi",
            "-p",
            "2222",
            "-o",
            "IdentityFile=\"/keys/id\"",
            "-o",
            "IdentitiesOnly=yes",
            "--",
            "box",
        ];
        assert_eq!(args, expected);
    }

    #[test]
    fn ssh_arguments_reject_unsafe_fields_with_exact_messages() {
        let mut host_target = target();
        host_target.host = "-oProxyCommand=evil".into();
        assert_eq!(
            ssh_arguments(&host_target, true, &host_target.known_hosts_file)
                .expect_err("host")
                .message,
            "Invalid host: \"-oProxyCommand=evil\""
        );
        let mut target_port = target();
        target_port.port = Some(70_000);
        assert_eq!(
            ssh_arguments(&target_port, true, &target_port.known_hosts_file)
                .expect_err("port")
                .message,
            "Invalid port: 70000"
        );
        assert_eq!(
            config_path("known hosts file", "/a%b").expect("quoted"),
            "\"/a%%b\""
        );
    }

    #[tokio::test]
    async fn run_ssh_classifies_host_key_failures() {
        let runner = FakeSshRunner::new();
        runner.push_output(CommandOutput {
            stdout: String::new(),
            stderr: "Host key verification failed.".into(),
            exit_code: Some(255),
        });
        let error = run_ssh(&runner, &target(), "exit 0", SshRunOptions::default())
            .await
            .expect_err("unknown key");
        assert_eq!(error.code, "host_key_unknown");
        assert_eq!(error.message, "The host key of box is not trusted yet");
        runner.push_output(CommandOutput {
            stdout: String::new(),
            stderr: "REMOTE HOST IDENTIFICATION HAS CHANGED!".into(),
            exit_code: Some(255),
        });
        let error = run_ssh(&runner, &target(), "exit 0", SshRunOptions::default())
            .await
            .expect_err("changed key");
        assert_eq!(error.code, "host_key_changed");
        runner.push_output(CommandOutput {
            stdout: String::new(),
            stderr: "no route".into(),
            exit_code: Some(1),
        });
        let error = run_ssh(&runner, &target(), "exit 0", SshRunOptions::default())
            .await
            .expect_err("ssh error");
        assert_eq!(error.code, "ssh_error");
        assert_eq!(error.message, "ssh box failed with exit code 1: no route");
        assert_eq!(error.field_str("stderr"), Some("no route"));
    }

    #[test]
    fn parses_probes_and_platform_architectures() {
        let output = "banner\nPI-ENV-PROBE\nLinux\nx86_64\nGNU/Linux\n/home/pi\n/tmp\n-\n";
        let probe = parse_probe("box", output).expect("probe");
        assert_eq!(probe.system, "Linux");
        assert_eq!(probe.home, "/home/pi");
        assert_eq!(probe.tmpdir, "/tmp");
        assert_eq!(normalize_arch(&probe.machine).expect("arch"), Arch::X64);
        assert_eq!(normalize_arch("aarch64").expect("arch"), Arch::Arm64);
        assert_eq!(
            parse_probe("box", "no marker").expect_err("probe").message,
            "Unexpected answer from box: no marker"
        );
        // `tmpdir` is set and LD_PRELOAD lacks termux-exec: only two warnings.
        assert_eq!(termux_warnings(&probe).len(), 2);
    }

    #[tokio::test]
    async fn detects_linux_and_windows_platforms() {
        let runner = FakeSshRunner::new();
        runner.push_output(CommandOutput {
            stdout: "PI-ENV-PROBE\nLinux\naarch64\nGNU/Linux\n/home/pi\n-\n-\n".into(),
            stderr: String::new(),
            exit_code: Some(0),
        });
        let platform = detect_platform(&runner, &target()).await.expect("platform");
        assert_eq!(platform.platform, Platform::Linux);
        assert_eq!(platform.arch, Arch::Arm64);

        let runner = FakeSshRunner::new();
        runner.push_output(CommandOutput {
            stdout: "sh: sh not found".into(),
            stderr: String::new(),
            exit_code: Some(127),
        });
        runner.push_output(CommandOutput {
            stdout: "PI-ENV-PROBE\nWindows\nAMD64\n-\nC:\\Users\\pi\n-\n-\n".into(),
            stderr: String::new(),
            exit_code: Some(0),
        });
        runner.push_output(CommandOutput {
            stdout: "Windows_NT".into(),
            stderr: String::new(),
            exit_code: Some(0),
        });
        let platform = detect_platform(&runner, &target()).await.expect("platform");
        assert_eq!(platform.platform, Platform::Windows);
        assert_eq!(platform.shell, Some(Shell::Cmd));
    }

    #[test]
    fn parses_host_key_lines_and_check_states() {
        let line = "pi-env-test ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIK keycomment";
        let parsed = parse_host_key_line("pi-env-test", line).expect("line");
        assert_eq!(parsed.key_type, "ssh-ed25519");
        assert_eq!(parsed.key, "AAAAC3NzaC1lZDI1NTE5AAAAIK");
        assert_eq!(
            parse_host_key_line("other", line).expect_err("alias").code,
            "invalid"
        );
        assert_eq!(parse_check_daemon("noise\npresent\n", false), "present");
        assert_eq!(parse_check_daemon("missing", true), "missing");
    }

    #[test]
    fn accept_and_forget_host_keys_follow_upstream_rules() {
        let existing = vec![
            "pi-env-test ssh-ed25519 OLDKEY".to_owned(),
            "other ssh-ed25519 KEEP".to_owned(),
        ];
        let error = apply_accept(
            &existing,
            &["pi-env-test ssh-ed25519 NEWKEY"],
            "pi-env-test",
            "box",
        )
        .expect_err("changed");
        assert_eq!(
            error.message,
            "The ssh-ed25519 host key of box changed; forget the old key first"
        );
        let forgotten = apply_forget(&existing, "pi-env-test");
        assert_eq!(forgotten, vec!["other ssh-ed25519 KEEP".to_owned()]);
        let added = apply_accept(
            &forgotten,
            &["pi-env-test ssh-ed25519 NEWKEY"],
            "pi-env-test",
            "box",
        )
        .expect("accept");
        assert_eq!(added.len(), 2);
        assert_eq!(added[1], "pi-env-test ssh-ed25519 NEWKEY");
    }

    #[test]
    fn daemon_paths_and_launch_commands_are_platform_exact() {
        let remote = RemotePlatform {
            platform: Platform::Linux,
            arch: Arch::X64,
            home: "/home/pi".into(),
            shell: None,
            warnings: Vec::new(),
        };
        assert_eq!(
            daemon_path(&remote, &"a".repeat(64)),
            "/home/pi/.pi/mobile/tools/pi-env-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(launch_command(&remote, "/x/pi-env", false), "'/x/pi-env'");
        assert_eq!(
            launch_command(&remote, "/x/pi-env", true),
            "exec \"$SHELL\" -lc 'exec \"$0\" \"$@\"' '/x/pi-env'"
        );
        let windows = RemotePlatform {
            platform: Platform::Windows,
            arch: Arch::X64,
            home: "C:\\Users\\pi".into(),
            shell: Some(Shell::Powershell),
            warnings: Vec::new(),
        };
        assert_eq!(
            launch_command(&windows, "C:\\x\\pi-env.exe", false),
            "& 'C:\\x\\pi-env.exe'"
        );
    }

    #[tokio::test]
    async fn deploy_verifies_before_uploading() {
        let directory =
            std::env::temp_dir().join(format!("pi-env-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&directory).expect("temp dir");
        let binary = directory.join("pi-env");
        std::fs::write(&binary, b"binary").expect("binary");
        let runner = FakeSshRunner::new();
        runner.push_output(CommandOutput {
            stdout: "missing\n".into(),
            stderr: String::new(),
            exit_code: Some(0),
        });
        runner.push_output(CommandOutput {
            stdout: "deployed\n".into(),
            stderr: String::new(),
            exit_code: Some(0),
        });
        let remote = RemotePlatform {
            platform: Platform::Linux,
            arch: Arch::X64,
            home: "/home/pi".into(),
            shell: None,
            warnings: Vec::new(),
        };
        let file = deploy_daemon(&runner, &target(), &remote, &binary)
            .await
            .expect("deploy");
        assert!(file.starts_with("/home/pi/.pi/mobile/tools/pi-env-"));
        let commands = runner.commands();
        assert_eq!(commands.len(), 2);
        assert!(commands[1].1.iter().any(|arg| arg.contains("sh -c")));
        let _ = std::fs::remove_dir_all(&directory);
    }
}
