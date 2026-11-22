//! `pi remote`: the host consumer for the `pi-env` execution environment.
//!
//! Upstream has no `remote` subcommand — this is a deliberate host-side
//! addition that consumes the ported `packages/env/src` surface: the lazy
//! daemon connection (`connection.ts:76-85`, `:278-289`), `RemoteExecutionEnv`
//! (`remote-env.ts:350-973`), the daemon-side watcher (`watch.ts:1-162`) and
//! the SSH connection shape (`ssh.ts:466-542`). The inert default mirrors
//! upstream's offline gate (`packages/coding-agent/src/main.ts:110,616-619`):
//! nothing is spawned and no SSH process is started unless the user explicitly
//! configures `--daemon`/`--host` or the `PI_REMOTE_*` settings below.
//!
//! The `pi-env` port has no timers, so this host supplies the monotonic clock:
//! [`HostRemoteEnv::tick`] drives `Daemon::tick` (ping/keepalive) and
//! `RemoteWatcher::tick` (reconnect backoff). `run_exec` and `run_watch`
//! call it from a Tokio interval while an operation is in flight.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pi_env::connection::{
    CommandSource, ConnectionOptions, Daemon, PING_INTERVAL_MS, ProcessDaemon,
};
use pi_env::env::{
    Command, OutputSink, RemoteExecutionEnv, RemoteExecutionEnvOptions, ShellExecOptions,
    ShellExecResult,
};
use pi_env::errors::AbortSignal;
use pi_env::files::{FileChange, WatchTarget};
use pi_env::ssh::{SshConnectOptions, SshTarget, ssh_connection};
use pi_env::watch::RemoteWatchOptions;
use pi_env::{ExecutionError, RemoteError};

use crate::config::get_agent_dir;
use crate::runtime;

/// Environment variable naming the local daemon binary (`--daemon` wins).
pub const PI_REMOTE_DAEMON: &str = "PI_REMOTE_DAEMON";
/// Environment variable naming an SSH target (`--host` wins).
pub const PI_REMOTE_HOST: &str = "PI_REMOTE_HOST";
/// Working directory for relative paths in the remote environment.
pub const PI_REMOTE_CWD: &str = "PI_REMOTE_CWD";
/// File-namespace id; upstream uses `pi-env:<env name>` (`remote-env.ts:53`).
pub const PI_REMOTE_ID: &str = "PI_REMOTE_ID";
/// SSH port override.
pub const PI_REMOTE_PORT: &str = "PI_REMOTE_PORT";
/// SSH identity file override.
pub const PI_REMOTE_IDENTITY: &str = "PI_REMOTE_IDENTITY";
/// Application-owned known-hosts file override.
pub const PI_REMOTE_KNOWN_HOSTS: &str = "PI_REMOTE_KNOWN_HOSTS";
/// Upstream's offline flag (`main.ts:616`); it also disables this command.
pub const PI_OFFLINE: &str = "PI_OFFLINE";

const DEFAULT_ID: &str = "pi-env:cli";
const USAGE: &str = "Usage: pi remote [status] | pi remote exec [--daemon <path> | --host <user@host>] [--cwd <dir>] -- <command...> | pi remote watch [--daemon <path> | --host <user@host>] <path...>";

/// Upstream's `isTruthyEnvFlag` (`main.ts:110-113`), verbatim semantics.
fn is_truthy_env_flag(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
    })
}

/// Where the daemon lives. [`RemoteTarget::Disabled`] is the default and never
/// constructs a connection.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum RemoteTarget {
    /// No remote environment configured (the inert default).
    #[default]
    Disabled,
    /// A daemon started as a local child process.
    Local {
        /// Path to the `pi-env` daemon binary.
        daemon: String,
    },
    /// A daemon reached through the system `ssh`.
    Ssh {
        /// Host name or `~/.ssh/config` alias.
        host: String,
        /// Remote user, when the target string included `user@`.
        user: Option<String>,
    },
}

/// The settings view consumed by [`HostRemoteEnv::from_settings`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSettings {
    /// The selected transport; disabled by default.
    pub target: RemoteTarget,
    /// Working directory used to resolve relative paths.
    pub cwd: String,
    /// File-namespace id.
    pub id: String,
    /// SSH port override.
    pub port: Option<i64>,
    /// SSH identity file override.
    pub identity_file: Option<String>,
    /// Application-owned known-hosts file override.
    pub known_hosts_file: Option<String>,
    /// Whether `pi --offline` / `PI_OFFLINE` blocks the connection.
    pub offline: bool,
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self {
            target: RemoteTarget::Disabled,
            cwd: ".".to_owned(),
            id: DEFAULT_ID.to_owned(),
            port: None,
            identity_file: None,
            known_hosts_file: None,
            offline: false,
        }
    }
}

impl RemoteSettings {
    /// Read the namespaced `PI_REMOTE_*` settings and upstream's `PI_OFFLINE`.
    /// With no variables set this returns [`RemoteTarget::Disabled`], so a
    /// plain `pi` invocation cannot touch the network.
    #[must_use]
    pub fn from_env() -> Self {
        let mut settings = Self::default();
        settings.offline = is_truthy_env_flag(std::env::var(PI_OFFLINE).ok().as_deref());
        settings.cwd = std::env::var(PI_REMOTE_CWD).unwrap_or(settings.cwd);
        settings.id = std::env::var(PI_REMOTE_ID).unwrap_or(settings.id);
        settings.port = std::env::var(PI_REMOTE_PORT)
            .ok()
            .and_then(|value| value.parse().ok());
        settings.identity_file = std::env::var(PI_REMOTE_IDENTITY).ok();
        settings.known_hosts_file = std::env::var(PI_REMOTE_KNOWN_HOSTS).ok();
        // An explicit host wins: the user asked for a remote machine. Local
        // daemons are inert children, so they are the fallback.
        settings.target = if let Ok(spec) = std::env::var(PI_REMOTE_HOST) {
            let (user, host) = split_host_spec(&spec);
            RemoteTarget::Ssh { host, user }
        } else if let Ok(daemon) = std::env::var(PI_REMOTE_DAEMON) {
            RemoteTarget::Local { daemon }
        } else {
            RemoteTarget::Disabled
        };
        settings
    }
}

/// Split `user@host` without interpreting any other syntax.
fn split_host_spec(spec: &str) -> (Option<String>, String) {
    match spec.split_once('@') {
        Some((user, host)) if !user.is_empty() && !host.is_empty() => {
            (Some(user.to_owned()), host.to_owned())
        }
        _ => (None, spec.to_owned()),
    }
}

/// A host-owned environment over one daemon connection.
///
/// The daemon is kept as an `Arc<dyn Daemon>` so the same code path works for
/// a local `ProcessDaemon` and an SSH `SshConnection` (which now implements
/// `Daemon` by forwarding to its inner process daemon).
pub struct HostRemoteEnv {
    daemon: Arc<dyn Daemon>,
    env: RemoteExecutionEnv,
    watcher: Option<pi_env::RemoteWatcher>,
}

impl std::fmt::Debug for HostRemoteEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostRemoteEnv")
            .field("env", &self.env)
            .field("watching", &self.watcher.is_some())
            .finish_non_exhaustive()
    }
}

impl HostRemoteEnv {
    /// Build a host environment from settings. Returns `Ok(None)` when the
    /// target is disabled or offline — no process or socket is touched.
    #[allow(clippy::result_large_err)] // mirrors pi_env's by-value RemoteError
    pub fn from_settings(settings: &RemoteSettings) -> Result<Option<Self>, RemoteError> {
        if settings.offline {
            return Ok(None);
        }
        let daemon: Arc<dyn Daemon> = match &settings.target {
            RemoteTarget::Disabled => return Ok(None),
            RemoteTarget::Local { daemon } => Arc::new(ProcessDaemon::new(ConnectionOptions::new(
                CommandSource::fixed([daemon.clone()]),
            ))),
            RemoteTarget::Ssh { host, user } => {
                let known_hosts_file = settings
                    .known_hosts_file
                    .clone()
                    .unwrap_or_else(|| default_known_hosts().display().to_string());
                let target = SshTarget {
                    host: host.clone(),
                    user: user.clone(),
                    port: settings.port,
                    identity_file: settings.identity_file.clone(),
                    known_hosts_file,
                    // Upstream keys host decisions by a stable name, not by
                    // aliases, ports or jump hosts (`ssh.ts:31`).
                    host_key_alias: format!("pi-env-{host}"),
                    ssh: None,
                    config_file: None,
                };
                // `ssh_connection` is lazy: it does not run `ssh` until the
                // first request, and no host key is accepted automatically.
                Arc::new(ssh_connection(SshConnectOptions {
                    target,
                    binary: None,
                    login_shell: false,
                    on_log: None,
                }))
            }
        };
        Ok(Some(Self::with_daemon(
            daemon,
            settings.id.clone(),
            settings.cwd.clone(),
        )))
    }

    /// Build a host environment over an injected daemon (used by tests).
    #[must_use]
    pub fn with_daemon(
        daemon: Arc<dyn Daemon>,
        id: impl Into<String>,
        cwd: impl Into<String>,
    ) -> Self {
        let env = RemoteExecutionEnv::new(RemoteExecutionEnvOptions {
            daemon: daemon.clone(),
            id: id.into(),
            cwd: cwd.into(),
            shell_path: None,
            shell_env: None,
            watch: RemoteWatchOptions::default(),
        });
        Self {
            daemon,
            env,
            watcher: None,
        }
    }

    /// The underlying environment, for callers that need the full surface.
    #[must_use]
    pub fn env(&self) -> &RemoteExecutionEnv {
        &self.env
    }

    /// Advance the host-driven ping and watch-reconnect state machines.
    /// `now_ms` must be monotonic; the caller owns the clock.
    pub async fn tick(&self, now_ms: u64) {
        self.daemon.tick(now_ms).await;
        if let Some(watcher) = &self.watcher {
            watcher.tick(now_ms).await;
        }
    }

    /// Run a shell command on the daemon (`remote-env.ts:855-966`).
    pub async fn exec(
        &self,
        command: &str,
        on_output: Option<OutputSink>,
    ) -> Result<ShellExecResult, ExecutionError> {
        let options = ShellExecOptions {
            on_output,
            ..ShellExecOptions::default()
        };
        self.env
            .exec(
                &Command::Shell(command.to_owned()),
                Some(&options),
                &AbortSignal::new(),
            )
            .await
    }

    /// Open a daemon-side watcher for `paths` (`watch.ts:22-135`).
    pub async fn watch(&mut self, paths: &[String]) -> Result<(), pi_env::FileError> {
        let targets = paths
            .iter()
            .map(|path| WatchTarget {
                path: path.clone(),
                recursive: true,
                exclude_hidden: false,
                exclude_names: Vec::new(),
            })
            .collect::<Vec<_>>();
        let on_change: Arc<dyn Fn(FileChange) + Send + Sync> =
            Arc::new(|change| eprintln!("[pi remote] change: {change:?}"));
        let watcher = self
            .env
            .watch(&targets, on_change, &AbortSignal::new())
            .await?;
        self.watcher = Some(watcher);
        Ok(())
    }

    /// Stop watching, kill commands started by this environment, and close
    /// the daemon connection.
    pub async fn close(&self) {
        if let Some(watcher) = &self.watcher {
            watcher.stop();
        }
        self.env.cleanup().await;
        self.daemon.close().await;
    }
}

fn default_known_hosts() -> PathBuf {
    get_agent_dir().join("pi-env-known-hosts")
}

/// A monotonic millisecond clock for the host tick loop.
#[derive(Debug)]
struct HostClock(Instant);

impl HostClock {
    fn new() -> Self {
        Self(Instant::now())
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// POSIX single-quote escaping for one argv word, matching `shellQuote`-style
/// call sites that hand a command string to `exec`.
fn quote_shell(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// Output seam for the CLI; tests capture lines instead of printing them.
pub type RemoteOutput = Arc<dyn Fn(&str) + Send + Sync>;

/// Wiring seam for `handle_remote_command_with`.
pub struct RemoteCommandOptions {
    /// Where human-readable lines go.
    pub output: RemoteOutput,
    /// When set, replaces the settings-built daemon (tests only).
    pub injected_daemon: Option<Arc<dyn Daemon>>,
}

impl Default for RemoteCommandOptions {
    fn default() -> Self {
        Self {
            output: Arc::new(|line| println!("{line}")),
            injected_daemon: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubCommand {
    Status,
    Exec,
    Watch,
}

struct ParsedCommand {
    subcommand: SubCommand,
    settings: RemoteSettings,
    words: Vec<String>,
}

fn parse_remote_args(args: &[String]) -> Result<ParsedCommand, String> {
    let mut settings = RemoteSettings::from_env();
    let mut subcommand = if args.first().is_some_and(|word| word == "status") {
        SubCommand::Status
    } else {
        SubCommand::Exec
    };
    let mut words = Vec::new();
    let mut index = usize::from(subcommand == SubCommand::Status);

    while index < args.len() {
        let word = args[index].as_str();
        match word {
            "--help" | "-h" => {
                return Err(format!(
                    "{USAGE}\n\nNo remote connection is made by default."
                ));
            }
            "--offline" => settings.offline = true,
            "--daemon" | "--host" | "--cwd" | "--id" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(format!("missing value for {word}"));
                };
                index += 1;
                match word {
                    "--daemon" => {
                        settings.target = RemoteTarget::Local {
                            daemon: value.clone(),
                        }
                    }
                    "--host" => {
                        let (user, host) = split_host_spec(value);
                        settings.target = RemoteTarget::Ssh { host, user };
                    }
                    "--cwd" => settings.cwd.clone_from(value),
                    "--id" => settings.id.clone_from(value),
                    _ => unreachable!("value-taking options are matched above"),
                }
            }
            "--" => {
                words.extend(args[index + 1..].iter().cloned());
                break;
            }
            "exec" | "watch" if words.is_empty() && subcommand == SubCommand::Exec => {
                subcommand = if word == "watch" {
                    SubCommand::Watch
                } else {
                    SubCommand::Exec
                };
            }
            _ if word.starts_with('-') => return Err(format!("unknown option: {word}")),
            _ => words.push(word.to_owned()),
        }
        index += 1;
    }
    Ok(ParsedCommand {
        subcommand,
        settings,
        words,
    })
}

const fn describe_target(target: &RemoteTarget) -> &'static str {
    match target {
        RemoteTarget::Disabled => "disabled",
        RemoteTarget::Local { .. } => "local daemon",
        RemoteTarget::Ssh { .. } => "ssh",
    }
}

/// Route `pi remote ...` before the main agent parser, like `pi mcp`.
/// Returns `None` when `args` does not start with `remote`.
pub fn handle_remote_command(args: &[String]) -> Option<i32> {
    handle_remote_command_with(args, &RemoteCommandOptions::default())
}

/// The injectable form of [`handle_remote_command`].
pub fn handle_remote_command_with(args: &[String], options: &RemoteCommandOptions) -> Option<i32> {
    let rest = args
        .split_first()
        .filter(|(word, _)| word.as_str() == "remote")?
        .1;
    let parsed = match parse_remote_args(rest) {
        Ok(parsed) => parsed,
        Err(message) => {
            (options.output)(&message);
            return Some(2);
        }
    };
    if parsed.subcommand == SubCommand::Status {
        (options.output)(&format!(
            "remote target: {} (id: {}, cwd: {})",
            describe_target(&parsed.settings.target),
            parsed.settings.id,
            parsed.settings.cwd
        ));
        return Some(0);
    }
    if parsed.settings.offline {
        (options.output)("pi remote is disabled by offline mode (pi --offline / PI_OFFLINE=1).");
        return Some(2);
    }
    if parsed.settings.target == RemoteTarget::Disabled {
        (options.output)(
            "pi remote is disabled: set PI_REMOTE_DAEMON or PI_REMOTE_HOST, or pass --daemon/--host.",
        );
        return Some(2);
    }
    if parsed.words.is_empty() {
        (options.output)(USAGE);
        return Some(2);
    }

    let host = match &options.injected_daemon {
        Some(daemon) => HostRemoteEnv::with_daemon(
            Arc::clone(daemon),
            parsed.settings.id.clone(),
            parsed.settings.cwd.clone(),
        ),
        None => match HostRemoteEnv::from_settings(&parsed.settings) {
            Ok(Some(host)) => host,
            Ok(None) => {
                (options.output)("pi remote is disabled.");
                return Some(2);
            }
            Err(error) => {
                (options.output)(&format!("pi remote: {error}"));
                return Some(1);
            }
        },
    };

    match parsed.subcommand {
        SubCommand::Status => unreachable!("status returned above"),
        SubCommand::Exec => Some(run_exec(&host, &parsed.words, &options.output)),
        SubCommand::Watch => Some(run_watch(host, &parsed.words, &options.output)),
    }
}

/// Drive one shell command and keep the daemon alive with host ticks while it
/// runs. Returns the command's exit status (or 1 on a client-side error).
fn run_exec(host: &HostRemoteEnv, words: &[String], output: &RemoteOutput) -> i32 {
    let command = words
        .iter()
        .map(|word| quote_shell(word))
        .collect::<Vec<_>>()
        .join(" ");
    let sink = Arc::clone(output);
    let on_output: OutputSink = Arc::new(move |text, _info| sink(text));
    let clock = HostClock::new();
    let outcome = runtime::block_on(async {
        let exec = host.exec(&command, Some(on_output));
        let ticks = async {
            let mut interval = tokio::time::interval(Duration::from_millis(PING_INTERVAL_MS));
            // Consume the immediate first tick, then pace with the daemon's
            // keepalive interval (`connection.ts:14`).
            interval.tick().await;
            loop {
                interval.tick().await;
                host.tick(clock.now_ms()).await;
            }
        };
        let result = tokio::select! {
            result = exec => result,
            _ = ticks => unreachable!("the tick loop never completes"),
        };
        host.close().await;
        result
    });
    match outcome {
        Ok(result) => {
            (output)(&format!("exit {}", result.exit_code));
            i32::try_from(result.exit_code).unwrap_or(1).clamp(0, 255)
        }
        Err(error) => {
            (output)(&format!("pi remote: {error}"));
            1
        }
    }
}

/// Keep a watcher open until Ctrl-C, driving its reconnect ticks.
fn run_watch(host: HostRemoteEnv, paths: &[String], output: &RemoteOutput) -> i32 {
    runtime::block_on(async move {
        let mut host = host;
        if let Err(error) = host.watch(paths).await {
            (output)(&format!("pi remote: {error}"));
            return 1;
        }
        (output)("watching (ctrl-c to stop)");
        let clock = HostClock::new();
        let mut interval = tokio::time::interval(Duration::from_millis(PING_INTERVAL_MS));
        interval.tick().await;
        loop {
            tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    if let Err(error) = result {
                        (output)(&format!("pi remote: {error}"));
                    }
                    break;
                }
                _ = interval.tick() => host.tick(clock.now_ms()).await,
            }
        }
        host.close().await;
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_env::connection::Event;
    use pi_env::test_support::{FakeDaemon, info_with_features, reply};
    use serde_json::{Map, Value};
    use std::sync::Mutex;

    fn output_capture() -> (RemoteOutput, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let output: RemoteOutput = Arc::new(move |line| {
            sink.lock().expect("lines lock").push(line.to_owned());
        });
        (output, lines)
    }

    fn output_event(kind: &str, stream: &str, payload: &[u8]) -> Event {
        let mut json = Map::new();
        json.insert("kind".into(), Value::from(kind));
        json.insert("stream".into(), Value::from(stream));
        Event {
            json,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn defaults_are_inert() {
        let settings = RemoteSettings::default();
        assert_eq!(settings.target, RemoteTarget::Disabled);
        assert!(!settings.offline);
        assert!(
            HostRemoteEnv::from_settings(&settings)
                .expect("settings")
                .is_none()
        );

        let offline = RemoteSettings {
            target: RemoteTarget::Local {
                daemon: "/no/such/daemon".into(),
            },
            offline: true,
            ..RemoteSettings::default()
        };
        assert!(
            HostRemoteEnv::from_settings(&offline)
                .expect("offline")
                .is_none()
        );
    }

    #[test]
    fn local_settings_do_not_start_a_daemon_until_used() {
        let settings = RemoteSettings {
            target: RemoteTarget::Local {
                daemon: "/no/such/daemon".into(),
            },
            ..RemoteSettings::default()
        };
        let host = HostRemoteEnv::from_settings(&settings)
            .expect("settings")
            .expect("local host");
        // A tick on an unstarted ProcessDaemon is a no-op: it must not spawn.
        runtime::block_on(host.tick(1000));
    }

    #[test]
    fn exec_uses_the_injected_daemon_and_forwards_ticks() {
        let daemon = Arc::new(FakeDaemon::new(info_with_features(&[])));
        let mut json = Map::new();
        json.insert("exitCode".into(), Value::from(3));
        daemon.push_reply(
            "exec",
            reply(json, Vec::new()),
            vec![output_event("output", "stdout", b"hello\n")],
            Some((42, 1)),
        );
        let shared: Arc<dyn Daemon> = daemon.clone();
        let host = HostRemoteEnv::with_daemon(shared, "pi-env:test", "/work");
        runtime::block_on(host.tick(7));

        let (output, lines) = output_capture();
        let status = handle_remote_command_with(
            &[
                "remote".into(),
                "exec".into(),
                "--daemon".into(),
                "/no/such/daemon".into(),
                "--".into(),
                "printf".into(),
                "hello".into(),
            ],
            &RemoteCommandOptions {
                injected_daemon: Some(daemon.clone()),
                output,
            },
        );
        assert_eq!(status, Some(3));
        let lines = lines.lock().expect("lines lock");
        assert!(lines.iter().any(|line| line.contains("hello")));
        assert!(lines.iter().any(|line| line == "exit 3"));
        drop(lines);
        assert!(daemon.calls().iter().any(|call| call == "exec"));
        assert_eq!(daemon.ticks(), vec![7]);
        // The daemon owns process cleanup; the client does not kill a finished exec.
        assert_eq!(daemon.killed(), Vec::<(u64, u64)>::new());
    }

    #[test]
    fn watch_opens_and_forwards_ticks() {
        let daemon = Arc::new(FakeDaemon::new(info_with_features(&[])));
        let mut ready = Map::new();
        ready.insert("kind".into(), Value::from("ready"));
        ready.insert("mode".into(), Value::from("native"));
        daemon.push_reply(
            "watch",
            reply(Map::new(), Vec::new()),
            vec![Event {
                json: ready,
                payload: Vec::new(),
            }],
            None,
        );
        let shared: Arc<dyn Daemon> = daemon.clone();
        let mut host = HostRemoteEnv::with_daemon(shared, "pi-env:test", "/work");
        runtime::block_on(async {
            host.watch(&["/work".to_owned()]).await.expect("watch");
            host.tick(1234).await;
        });
        assert!(daemon.calls().iter().any(|call| call == "watch"));
        assert_eq!(daemon.ticks(), vec![1234]);
    }

    #[test]
    fn offline_and_disabled_never_build_a_connection() {
        let (output, lines) = output_capture();
        let status = handle_remote_command_with(
            &[
                "remote".into(),
                "exec".into(),
                "--host".into(),
                "box".into(),
                "--offline".into(),
                "echo".into(),
            ],
            &RemoteCommandOptions {
                injected_daemon: None,
                output,
            },
        );
        assert_eq!(status, Some(2));
        let lines = lines.lock().expect("lines lock");
        assert!(lines.iter().any(|line| line.contains("offline mode")));
        drop(lines);

        let (output, lines) = output_capture();
        let status = handle_remote_command_with(
            &["remote".into()],
            &RemoteCommandOptions {
                injected_daemon: None,
                output,
            },
        );
        assert_eq!(status, Some(2));
        let lines = lines.lock().expect("lines lock");
        assert!(lines.iter().any(|line| line.contains("disabled")));
    }

    #[test]
    fn shell_quoting_keeps_spaces_and_quotes_inside_one_word() {
        assert_eq!(quote_shell("hello"), "hello");
        assert_eq!(quote_shell("a b"), "'a b'");
        assert_eq!(quote_shell("it's"), "'it'\\''s'");
    }
}
