//! The native MCP server manager: the host half of the bundled `mcp`
//! extension (`packages/coding-agent/src/extensions/mcp/`), reduced to the
//! stdio subset this slice lands — connections, tool discovery, tool
//! calls, and the result mapping the bridge registers.
//!
//! What each upstream piece becomes:
//!
//! * `runtime.ts:141-456` (`McpServerConnection`) — a per-server entry in
//!   the worker thread: `connect` is `connectOnce` (`:361-409`, minus the
//!   HTTP retries/OAuth), `call_tool` is `withClient` (`:280-303`) with the
//!   stdio lazy-reconnect arm, and the `getClient` guards the tool
//!   definitions close over (`index.ts:424-438`) run at call time.
//! * `runtime.ts:100-124` (`createDefaultTransport`, stdio arm) —
//!   [`stdio_spawn_config`]: `~` expansion (`:92-98`), env
//!   `resolveConfigValueOrThrow` (`:113-116`), and the session-cwd-relative
//!   `cwd` (`:120`).
//! * `tools.ts:215-235` (`convertMcpResult`) + `content.ts:76-127`
//!   (`toLlmContent`) + `output-limit.ts:28-77` — [`convert_mcp_result`]:
//!   the model-facing blocks, the empty-error fallback (`tools.ts:224-226`),
//!   the 20 KiB middle truncation with the full-output temp file, and the
//!   `structuredContent` script result.
//!
//! Threading: `QuickJS` runs on the host thread, and the agent loop polls
//! tool executors from inside `crate::runtime::block_on` — the tokio
//! context a second `block_on` cannot enter. The manager therefore runs
//! its connections on a dedicated worker thread with its own current-thread
//! runtime; ops send requests over a channel and block on the reply
//! (bounded by the server's configured timeout, like every synchronous op).
//!
//! Not in this slice (recorded open): streamable HTTP transports and
//! OAuth/`needs-auth` (`oauth.ts`, the HTTP arm of `runtime.ts`), resources
//! (`resources.ts`), and the `/mcp` manager UI (`ui.ts`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value as Json, json};

use pi_mcp::types::ContentBlock;
use pi_mcp::{CallToolResult, McpClient, McpServerConfig};

/// Upstream's per-connection default timeout (`DEFAULT_TIMEOUT_SECONDS`,
/// `runtime.ts:50`).
const DEFAULT_TIMEOUT_SECONDS: f64 = 60.0;
/// Slack added to the reply window: the connect handshake and the tools
/// list each get the full timeout upstream, so two of them plus transport
/// overhead must fit.
const REPLY_SLACK: Duration = Duration::from_secs(5);
/// Model-facing text of an MCP result beyond this is cut in the middle
/// (`MCP_OUTPUT_MAX_BYTES`, `output-limit.ts:9`).
const MCP_OUTPUT_MAX_BYTES: usize = 20 * 1024;
/// How long a `!command` config value may run (`resolve-config-value.ts`
/// `executeWithConfiguredShell`, timeout 10000).
const COMMAND_VALUE_TIMEOUT: Duration = Duration::from_secs(10);

/// One configured server's connection state in the worker.
///
/// Both variants keep the spawn config so a failed stdio server can
/// reconnect lazily on the next call (`runtime.ts:280-303` — stdio has no
/// connect retries).
enum ServerState {
    Connected {
        client: McpClient,
        tools: Vec<Json>,
        timeout: Duration,
    },
    Failed {
        spawn: McpServerConfig,
        timeout: Duration,
        error: String,
    },
}

struct ServerEntry {
    state: ServerState,
}

/// One worker request.
enum Request {
    Connect {
        name: String,
        config: Json,
        cwd: String,
        reply: std::sync::mpsc::Sender<Result<ConnectOutcome, String>>,
    },
    CallTool {
        server: String,
        tool: String,
        args: Json,
        reply: std::sync::mpsc::Sender<Result<Json, String>>,
    },
    Drop {
        name: String,
    },
    ShutdownAll,
}

/// A successful connect: the offered tools as JSON (decoded `McpTool`s) and
/// the server's `initialize` instructions.
pub struct ConnectOutcome {
    pub tools: Vec<Json>,
    pub instructions: Option<String>,
}

/// Handle to the worker thread sharing one worker per host core.
///
/// `Clone` shares the worker; dropping the last handle closes the request
/// channel and the worker kills the connected server processes.
#[derive(Clone, Default)]
pub struct McpManager {
    requests: Arc<OnceLock<Mutex<std::sync::mpsc::Sender<Request>>>>,
    /// Per-server request timeouts, recorded by the worker on connect so
    /// the caller's reply window covers the worker's internal wait.
    timeouts: Arc<Mutex<HashMap<String, Duration>>>,
}

impl McpManager {
    /// The worker's request channel, spawning the thread on first use.
    fn sender(&self) -> std::sync::mpsc::Sender<Request> {
        self.requests
            .get_or_init(|| Mutex::new(spawn_worker(self.timeouts.clone())))
            .lock()
            .expect("mcp manager mutex poisoned")
            .clone()
    }

    /// The reply window for `server`: its configured timeout (twice over,
    /// for the handshake and the call) plus slack, or the default.
    fn reply_window(&self, server: &str, fallback: Duration) -> Duration {
        self.timeouts
            .lock()
            .expect("mcp timeouts mutex poisoned")
            .get(server)
            .map_or(fallback, |timeout| *timeout)
            * 2
            + REPLY_SLACK
    }

    /// Spawns the server, handshakes, and lists its tools
    /// (`connectOnce` + discovery, `runtime.ts:361-409`); spawn, handshake,
    /// transport, and env-resolution failures are the caller's error value
    /// to record as the server's failure state.
    pub fn connect(&self, name: &str, config: &Json, cwd: &str) -> Result<ConnectOutcome, String> {
        let window = config_timeout(config) * 2 + REPLY_SLACK;
        let (reply, reply_rx) = std::sync::mpsc::channel();
        self.sender()
            .send(Request::Connect {
                name: name.to_string(),
                config: config.clone(),
                cwd: cwd.to_string(),
                reply,
            })
            .map_err(|_| "MCP manager is not running".to_string())?;
        reply_rx
            .recv_timeout(window)
            .unwrap_or_else(|_| Err("MCP manager did not answer in time".to_string()))
    }

    /// Calls one tool on a connected server (`callTool` via `withClient`,
    /// `runtime.ts:248-303`), mapped by [`convert_mcp_result`].
    ///
    /// The guards the tool definitions close over run first (`getClient`,
    /// `index.ts:424-438`). The reply is `{"result": ...}` or
    /// `{"error": "..."}` — the execute wrapper throws the error, so a
    /// failed call is a tool error, and a protocol-level `isError` result
    /// stays a result.
    pub fn call_tool(&self, server: &str, tool: &str, args: Json) -> Json {
        let window = self.reply_window(server, config_default_window());
        let (reply, reply_rx) = std::sync::mpsc::channel();
        let outcome = self
            .sender()
            .send(Request::CallTool {
                server: server.to_string(),
                tool: tool.to_string(),
                args,
                reply,
            })
            .map_err(|_| "MCP manager is not running".to_string())
            .and_then(|()| {
                reply_rx
                    .recv_timeout(window)
                    .unwrap_or_else(|_| Err("MCP manager did not answer in time".to_string()))
            });
        match outcome {
            Ok(result) => json!({ "result": result }),
            Err(error) => json!({ "error": error }),
        }
    }

    /// Drops one server's connection, killing its process
    /// (`connection.close()`, `runtime.ts:444-455`).
    pub fn drop_server(&self, name: &str) {
        let _ = self.sender().send(Request::Drop {
            name: name.to_string(),
        });
    }

    /// Drops every connection (the `session_shutdown` close,
    /// `index.ts:1250-1256`).
    pub fn shutdown_all(&self) {
        let _ = self.sender().send(Request::ShutdownAll);
    }
}

/// The configured per-request timeout in seconds (`config.timeout ?? 60`,
/// `runtime.ts:212-214`).
fn config_timeout(config: &Json) -> Duration {
    let seconds = config
        .get("timeout")
        .and_then(Json::as_f64)
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    Duration::from_secs_f64(seconds)
}

/// The reply window when no timeout was recorded (the call raced the
/// connect).
fn config_default_window() -> Duration {
    Duration::from_secs_f64(DEFAULT_TIMEOUT_SECONDS)
}

/// Spawns the worker thread; returns its request channel.
fn spawn_worker(
    timeouts: Arc<Mutex<HashMap<String, Duration>>>,
) -> std::sync::mpsc::Sender<Request> {
    let (sender, receiver) = std::sync::mpsc::channel::<Request>();
    std::thread::Builder::new()
        .name("pi-ext-js-mcp".to_string())
        .spawn(move || worker_loop(&receiver, &timeouts))
        .expect("spawn MCP manager worker thread");
    sender
}

/// The worker: owns the tokio runtime and every connection. When all
/// manager handles drop, the channel closes and remaining clients shut
/// down (their transports kill the children).
fn worker_loop(
    receiver: &std::sync::mpsc::Receiver<Request>,
    timeouts: &Arc<Mutex<HashMap<String, Duration>>>,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build MCP manager runtime");
    let mut servers: HashMap<String, ServerEntry> = HashMap::new();
    while let Ok(request) = receiver.recv() {
        match request {
            Request::Connect {
                name,
                config,
                cwd,
                reply,
            } => {
                let timeout = config_timeout(&config);
                let outcome = match stdio_spawn_config(&name, &config, &cwd) {
                    Ok(spawn) => connect_with_spawn(&runtime, &mut servers, &name, spawn, timeout),
                    Err(error) => {
                        let _ = timeouts
                            .lock()
                            .expect("mcp timeouts mutex poisoned")
                            .remove(&name);
                        Err(error)
                    }
                };
                if outcome.is_ok() {
                    timeouts
                        .lock()
                        .expect("mcp timeouts mutex poisoned")
                        .insert(name.clone(), timeout);
                }
                let _ = reply.send(outcome);
            }
            Request::CallTool {
                server,
                tool,
                args,
                reply,
            } => {
                let outcome = call_tool_on(&runtime, &mut servers, &server, &tool, args);
                let _ = reply.send(outcome);
            }
            Request::Drop { name } => {
                drop_entry(&runtime, &mut servers, &name);
                timeouts
                    .lock()
                    .expect("mcp timeouts mutex poisoned")
                    .remove(&name);
            }
            Request::ShutdownAll => {
                shutdown_all(&runtime, &mut servers);
                timeouts
                    .lock()
                    .expect("mcp timeouts mutex poisoned")
                    .clear();
            }
        }
    }
    shutdown_all(&runtime, &mut servers);
    timeouts
        .lock()
        .expect("mcp timeouts mutex poisoned")
        .clear();
}

/// Connects (or reconnects) one server, replacing any existing entry.
fn connect_with_spawn(
    runtime: &tokio::runtime::Runtime,
    servers: &mut HashMap<String, ServerEntry>,
    name: &str,
    spawn: McpServerConfig,
    timeout: Duration,
) -> Result<ConnectOutcome, String> {
    let attempt = runtime.block_on(async {
        let client = McpClient::connect_with_timeout(&spawn, timeout)
            .await
            .map_err(|error| error.to_string())?;
        // Servers without the tools capability (prompts or resources only)
        // do not answer tools/list (`runtime.ts:387-388`).
        let tools = if client.capabilities.tools.is_some() {
            client
                .tools_list_with_timeout(timeout)
                .await
                .map_err(|error| error.to_string())?
                .tools
        } else {
            Vec::new()
        };
        let tools = tools
            .iter()
            .map(|tool| serde_json::to_value(tool).unwrap_or(Json::Null))
            .collect::<Vec<_>>();
        let instructions = client.instructions.clone();
        Ok::<_, String>((client, tools, instructions))
    });
    let (client, tools, instructions) = match attempt {
        Ok(attempt) => attempt,
        Err(error) => {
            servers.insert(
                name.to_string(),
                ServerEntry {
                    state: ServerState::Failed {
                        spawn,
                        timeout,
                        error: error.clone(),
                    },
                },
            );
            return Err(error);
        }
    };
    let previous = servers.insert(
        name.to_string(),
        ServerEntry {
            state: ServerState::Connected {
                client,
                tools: tools.clone(),
                timeout,
            },
        },
    );
    if let Some(previous) = previous
        && let ServerState::Connected { client, .. } = previous.state
    {
        let _ = runtime.block_on(client.shutdown());
    }
    Ok(ConnectOutcome {
        tools,
        instructions,
    })
}

/// Drops one entry, killing a connected process.
fn drop_entry(
    runtime: &tokio::runtime::Runtime,
    servers: &mut HashMap<String, ServerEntry>,
    name: &str,
) {
    if let Some(entry) = servers.remove(name)
        && let ServerState::Connected { client, .. } = entry.state
    {
        let _ = runtime.block_on(client.shutdown());
    }
}

/// Shuts every connection down.
fn shutdown_all(runtime: &tokio::runtime::Runtime, servers: &mut HashMap<String, ServerEntry>) {
    for (_, entry) in servers.drain() {
        if let ServerState::Connected { client, .. } = entry.state {
            let _ = runtime.block_on(client.shutdown());
        }
    }
}

/// One tool call with the execution-time guards, then the wire call.
fn call_tool_on(
    runtime: &tokio::runtime::Runtime,
    servers: &mut HashMap<String, ServerEntry>,
    server: &str,
    tool: &str,
    args: Json,
) -> Result<Json, String> {
    // `!current` and disabled both answer the disabled error
    // (`index.ts:427-428`); the runner hides a disabled server's tools, so
    // a surviving call is stale.
    let Some(entry) = servers.get_mut(server) else {
        return Err(format!("MCP server \"{server}\" is disabled."));
    };
    let reconnect = match &entry.state {
        ServerState::Failed { spawn, timeout, .. } => Some((spawn.clone(), *timeout)),
        ServerState::Connected { .. } => None,
    };
    if let Some((spawn, timeout)) = reconnect {
        // Lazy reconnect: the next call opens a fresh connection
        // (`withClient` → `getClient` → `open`, `runtime.ts:280-303`).
        if connect_with_spawn(runtime, servers, server, spawn, timeout).is_err()
            && let ServerState::Failed { error, .. } = &servers[server].state
        {
            // `connectFailed`'s error text (`runtime.ts:419`).
            return Err(format!(
                "MCP server \"{server}\" failed to connect: {error}"
            ));
        }
    }
    let Some(entry) = servers.get(server) else {
        return Err(format!("MCP server \"{server}\" is disabled."));
    };
    let ServerState::Connected {
        client,
        tools,
        timeout,
    } = &entry.state
    else {
        return Err(format!("MCP server \"{server}\" is disabled."));
    };
    // A tool the server no longer offers is no longer available
    // (`index.ts:433-435`).
    let offered = tools
        .iter()
        .any(|candidate| candidate.get("name").and_then(Json::as_str) == Some(tool));
    if !offered {
        return Err(format!(
            "MCP tool \"{server}/{tool}\" is no longer available."
        ));
    }
    let arguments = if args.is_object() {
        Some(args)
    } else {
        Some(json!({}))
    };
    let result = runtime.block_on(async {
        client
            .tools_call(tool, arguments, *timeout)
            .await
            .map_err(|error| error.to_string())
    })?;
    Ok(convert_mcp_result(server, tool, &result))
}

/// The stdio spawn config from a server entry (`createDefaultTransport`'s
/// stdio arm, `runtime.ts:112-123`). Streamable HTTP entries are a
/// recorded open item and fail to connect instead of silently doing
/// nothing.
fn stdio_spawn_config(name: &str, config: &Json, cwd: &str) -> Result<McpServerConfig, String> {
    if config.get("url").is_some_and(Json::is_string) {
        return Err(format!(
            "MCP server \"{name}\": the streamable HTTP transport is not implemented by this build yet (stdio servers only)"
        ));
    }
    let Some(command) = config.get("command").and_then(Json::as_str) else {
        return Err(format!(
            "MCP server \"{name}\": config has no \"command\" (stdio servers only in this build)"
        ));
    };
    let args = config
        .get("args")
        .and_then(Json::as_array)
        .map(|args| {
            args.iter()
                .filter_map(|arg| arg.as_str())
                .map(expand_home)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut env = HashMap::new();
    if let Some(record) = config.get("env").and_then(Json::as_object) {
        for (key, value) in record {
            let Some(raw) = value.as_str() else {
                continue;
            };
            let resolved = resolve_config_value_or_throw(
                raw,
                &format!("MCP server \"{name}\" env \"{key}\""),
            )?;
            env.insert(key.clone(), resolved);
        }
    }
    let working_dir = std::path::PathBuf::from(cwd).join(expand_home(
        config.get("cwd").and_then(Json::as_str).unwrap_or("."),
    ));
    Ok(McpServerConfig {
        name: name.to_string(),
        command: expand_home(command),
        args,
        env,
        working_dir: Some(working_dir),
    })
}

/// `expandHome` (`runtime.ts:92-98`): `~` and `~/…` name the home directory.
fn expand_home(value: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if value == "~" {
        return home;
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return std::path::Path::new(&home)
            .join(rest)
            .to_string_lossy()
            .into_owned();
    }
    value.to_string()
}

/// Resolved config value parts: literals and env names
/// (`parseConfigValueTemplate`, `resolve-config-value.ts:33-90`).
enum Part {
    Literal(String),
    Env(String),
}

/// `resolveConfigValueOrThrow` (`resolve-config-value.ts:229-252`).
///
/// `!cmd` runs through the shell (10 s cap, trimmed stdout),
/// `$VAR`/`${VAR}` interpolate the environment with `$$`/`$!` escapes, and
/// a missing variable fails with the exact upstream message.
fn resolve_config_value_or_throw(config: &str, description: &str) -> Result<String, String> {
    resolve_config_value(config).ok_or_else(|| missing_value_error(config, description))
}

fn missing_value_error(config: &str, description: &str) -> String {
    if let Some(command) = config.strip_prefix('!') {
        return format!("Failed to resolve {description} from shell command: {command}");
    }
    let missing = missing_config_value_env_var_names(config);
    match missing.as_slice() {
        [name] => format!("Failed to resolve {description} from environment variable: {name}"),
        names if !names.is_empty() => {
            format!(
                "Failed to resolve {description} from environment variables: {}",
                names.join(", ")
            )
        }
        _ => format!("Failed to resolve {description}"),
    }
}

/// `resolveConfigValue` (`resolve-config-value.ts:160-168`).
fn resolve_config_value(config: &str) -> Option<String> {
    if let Some(command) = config.strip_prefix('!') {
        return execute_command_value(command);
    }
    resolve_template(&parse_config_value_template(config))
}

/// `resolveTemplate` (`resolve-config-value.ts:119-131`): a missing
/// variable resolves the whole value to `undefined`.
fn resolve_template(parts: &[Part]) -> Option<String> {
    let mut resolved = String::new();
    for part in parts {
        match part {
            Part::Literal(value) => resolved.push_str(value),
            Part::Env(name) => {
                let value = std::env::var(name).ok().filter(|value| !value.is_empty())?;
                resolved.push_str(&value);
            }
        }
    }
    Some(resolved)
}

/// `executeCommand` (`resolve-config-value.ts:180-201` default-shell arm):
/// trimmed stdout of the shell command, `None` on failure, or on the 10 s
/// cap (the child is killed, like `spawnSync`'s `timeout`).
fn execute_command_value(command: &str) -> Option<String> {
    let cached = command_result_cache()
        .lock()
        .expect("config value command cache poisoned")
        .get(command)
        .cloned();
    if let Some(cached) = cached {
        return cached;
    }
    let mut child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let reader = stdout.map(|pipe| {
        std::thread::Builder::new()
            .name("pi-ext-js-mcp-cmdval".to_string())
            .spawn(move || {
                let mut pipe = pipe;
                let mut text = String::new();
                let _ = std::io::Read::read_to_string(&mut pipe, &mut text);
                text
            })
            .expect("spawn config-value reader thread")
    });
    let (status_tx, status_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("pi-ext-js-mcp-cmdwait".to_string())
        .spawn(move || {
            let status = child.wait();
            let _ = status_tx.send(status.ok().and_then(|status| status.code()));
        })
        .expect("spawn config-value waiter thread");
    let exit_code = if let Ok(exit_code) = status_rx.recv_timeout(COMMAND_VALUE_TIMEOUT) {
        exit_code
    } else {
        // The cap hit: terminate the child and reap it.
        kill_process(pid);
        let _ = status_rx.recv();
        None
    };
    let text = reader
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    let value = if exit_code == Some(0) {
        let text = text.trim().to_string();
        (!text.is_empty()).then_some(text)
    } else {
        None
    };
    command_result_cache()
        .lock()
        .expect("config value command cache poisoned")
        .insert(command.to_string(), value.clone());
    value
}

/// Kills a spawned child after the cap (`spawnSync` terminates it); ESRCH
/// from a race with the child's own exit is ignored, like `send_signal`
/// (`exec.rs`).
#[cfg(unix)]
fn kill_process(pid: u32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
}

#[cfg(not(unix))]
fn kill_process(pid: u32) {
    let _ = pid;
}

/// Command results are cached for the process lifetime
/// (`commandResultCache`, `resolve-config-value.ts:10`).
fn command_result_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `getMissingConfigValueEnvVarNames` (`resolve-config-value.ts:132-134`).
fn missing_config_value_env_var_names(config: &str) -> Vec<String> {
    let mut names = Vec::new();
    for part in parse_config_value_template(config) {
        if let Part::Env(name) = part
            && std::env::var(&name).map_or(true, |value| value.is_empty())
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    names
}

/// `parseConfigValueTemplate` (`resolve-config-value.ts:33-90`).
fn parse_config_value_template(config: &str) -> Vec<Part> {
    let mut parts: Vec<Part> = Vec::new();
    let append_literal = |parts: &mut Vec<Part>, value: &str| {
        if value.is_empty() {
            return;
        }
        if let Some(Part::Literal(last)) = parts.last_mut() {
            last.push_str(value);
        } else {
            parts.push(Part::Literal(value.to_string()));
        }
    };
    let mut index = 0usize;
    let bytes = config.as_bytes();
    while index < config.len() {
        let Some(dollar) = config[index..].find('$') else {
            append_literal(&mut parts, &config[index..]);
            break;
        };
        let dollar = index + dollar;
        append_literal(&mut parts, &config[index..dollar]);
        let next = bytes.get(dollar + 1).copied();
        if next == Some(b'$') || next == Some(b'!') {
            append_literal(&mut parts, if next == Some(b'$') { "$" } else { "!" });
            index = dollar + 2;
            continue;
        }
        if next == Some(b'{') {
            let Some(end) = config[dollar + 2..].find('}') else {
                append_literal(&mut parts, "$");
                index = dollar + 1;
                continue;
            };
            let end = dollar + 2 + end;
            let name = &config[dollar + 2..end];
            if is_env_var_name(name) {
                parts.push(Part::Env(name.to_string()));
            } else {
                append_literal(&mut parts, &config[dollar..=end]);
            }
            index = end + 1;
            continue;
        }
        let name = env_var_name_prefix(&config[dollar + 1..]);
        if let Some(name) = name {
            index = dollar + 1 + name.len();
            parts.push(Part::Env(name));
            continue;
        }
        append_literal(&mut parts, "$");
        index = dollar + 1;
    }
    parts
}

/// `ENV_VAR_NAME_RE` (`resolve-config-value.ts:12`).
fn is_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `ENV_VAR_NAME_PREFIX_RE` (`resolve-config-value.ts:13`): the leading
/// valid name of a `$NAME` reference, if any.
fn env_var_name_prefix(value: &str) -> Option<String> {
    let mut name = String::new();
    for (index, c) in value.char_indices() {
        let ok = if index == 0 {
            c.is_ascii_alphabetic() || c == '_'
        } else {
            c.is_ascii_alphanumeric() || c == '_'
        };
        if !ok {
            break;
        }
        name.push(c);
    }
    (!name.is_empty()).then_some(name)
}

/// `convertMcpResult` (`tools.ts:215-235`): the model-facing content, the
/// `details`, the script-facing `structuredContent`, and the error flag.
#[must_use]
pub fn convert_mcp_result(server: &str, tool: &str, result: &CallToolResult) -> Json {
    let mut content = if result.content.is_empty() {
        // Without content blocks, toLlmContent falls back to the structured
        // content as JSON (`tools.ts:221-223`).
        to_llm_content_fallback(result)
    } else {
        result
            .content
            .iter()
            .flat_map(|block| block_to_content(server, block))
            .collect::<Vec<_>>()
    };
    if result.is_error && text_of(&content).is_empty() {
        content.push(json!({
            "type": "text",
            "text": format!("MCP tool {server}/{tool} returned an error"),
        }));
    }
    let (content, full_output_path) = limit_mcp_content(content);
    let mut details = json!({ "server": server, "tool": tool });
    if let Some(path) = &full_output_path {
        details["fullOutputPath"] = json!(path);
    }
    // The script result is the `CallToolResult` without `_meta`
    // (`tools.ts:228-232`); this port's decoder never carries `_meta`.
    let mut script_result = json!({ "content": result.content });
    if let Some(structured) = &result.structured_content {
        script_result["structuredContent"] = structured.clone();
    }
    if result.is_error {
        script_result["isError"] = json!(true);
    }
    let mut output = json!({
        "content": content,
        "details": details,
        "structuredContent": script_result,
    });
    if result.is_error {
        output["isError"] = json!(true);
    }
    output
}

/// `toLlmContent`'s empty-content fallback (`content.ts:112-127`): a result
/// without blocks but with structured content becomes its JSON.
fn to_llm_content_fallback(result: &CallToolResult) -> Vec<Json> {
    if let Some(structured) = &result.structured_content {
        vec![json!({
            "type": "text",
            "text": serde_json::to_string_pretty(structured).unwrap_or_default(),
        })]
    } else {
        Vec::new()
    }
}

/// Model-facing content of one block (`blockToLlmContent`,
/// `content.ts:76-107`, overridden by `blockToContent`).
///
/// `server` only feeds the `read_mcp_resource` suffix of resource links,
/// which the port does not register yet — the arms never read it.
fn block_to_content(_server: &str, block: &ContentBlock) -> Vec<Json> {
    match block {
        ContentBlock::Text { text } => vec![json!({ "type": "text", "text": text })],
        ContentBlock::Image { data, mime_type } => {
            vec![json!({ "type": "image", "data": data, "mimeType": mime_type })]
        }
        ContentBlock::Audio { mime_type, .. } => {
            vec![json!({ "type": "text", "text": format!("[audio {mime_type} omitted]") })]
        }
        ContentBlock::ResourceLink {
            uri,
            name,
            title,
            description,
            mime_type,
            size,
        } => {
            // `blockToContent`'s resource_link arm (`tools.ts:178-188`);
            // `readableResources` is false without the resource tools, so
            // no `read` suffix is appended.
            let mut details = Vec::new();
            if let Some(mime_type) = mime_type {
                details.push(mime_type.clone());
            }
            if let Some(size) = size {
                details.push(format_size(*size));
            }
            let details = if details.is_empty() {
                String::new()
            } else {
                format!(" ({})", details.join(", "))
            };
            let description = description
                .as_ref()
                .map_or_else(String::new, |text| format!(": {text}"));
            let label = title.clone().unwrap_or_else(|| name.clone());
            vec![json!({
                "type": "text",
                "text": format!("[Resource {uri} \"{label}\"{details}{description}]"),
            })]
        }
        ContentBlock::Resource { resource } => {
            // `blockToContent`'s resource arm (`tools.ts:189-201`).
            let uri = resource
                .get("uri")
                .and_then(Json::as_str)
                .unwrap_or_default();
            if let Some(text) = resource.get("text").and_then(Json::as_str) {
                return vec![json!({ "type": "text", "text": text })];
            }
            let Some(blob) = resource.get("blob").and_then(Json::as_str) else {
                return vec![json!({
                    "type": "text",
                    "text": format!("[binary resource {uri} (unknown type) omitted]"),
                })];
            };
            let mime_type = resource.get("mimeType").and_then(Json::as_str);
            if let Some(mime) = mime_type.filter(|mime| mime.starts_with("image/")) {
                return vec![json!({
                    "type": "image",
                    "data": blob,
                    "mimeType": mime,
                })];
            }
            if is_text_mime_type(mime_type) {
                return vec![json!({
                    "type": "text",
                    "text": String::from_utf8_lossy(&base64_decode(blob)).into_owned(),
                })];
            }
            let kind = format!(
                "{}, {}",
                mime_type.unwrap_or("unknown type"),
                format_size(blob.len() as f64)
            );
            match write_output_file(".txt", blob.as_bytes()) {
                Ok(path) => vec![json!({
                    "type": "text",
                    "text": format!("[Binary resource {uri} ({kind}) saved to {path}]"),
                })],
                Err(reason) => vec![json!({
                    "type": "text",
                    "text": format!("[Binary resource {uri} ({kind}) could not be saved: {reason}]"),
                })],
            }
        }
    }
}

/// `textOf` (`tools.ts:71-76`): the text blocks joined with newlines.
fn text_of(content: &[Json]) -> String {
    content
        .iter()
        .filter_map(|block| block.get("text").and_then(Json::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `isTextMimeType` (`tools.ts:166-170`): blobs of these types show as text.
fn is_text_mime_type(mime_type: Option<&str>) -> bool {
    let Some(mime_type) = mime_type else {
        return false;
    };
    let base = mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    base.starts_with("text/")
        || base == "application/json"
        || base.ends_with("+json")
        || base.ends_with("+xml")
}

/// The base64 the blob arrived in (`Buffer.from(blob, "base64")`); a decode
/// failure falls back to the raw ASCII bytes, which the lossy UTF-8
/// conversion renders as-is.
fn base64_decode(blob: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(blob.trim())
        .unwrap_or_else(|_| blob.as_bytes().to_vec())
}

/// `formatSize` (`core/tools/truncate.ts:61-69`).
#[must_use]
pub fn format_size(bytes: f64) -> String {
    if bytes < 1024.0 {
        format!("{bytes}B")
    } else if bytes < 1024.0 * 1024.0 {
        format!("{:.1}KB", bytes / 1024.0)
    } else {
        format!("{:.1}MB", bytes / (1024.0 * 1024.0))
    }
}

/// `limitMcpContent` (`output-limit.ts:28-48`): the model-facing content
/// within the limit, and where the full text went when it was saved.
fn limit_mcp_content(content: Vec<Json>) -> (Vec<Json>, Option<String>) {
    let combined = text_of(&content);
    let Some(truncation) = truncate_middle(&combined, MCP_OUTPUT_MAX_BYTES) else {
        return (content, None);
    };
    let (where_note, full_output_path) = match write_output_file(".txt", combined.as_bytes()) {
        Ok(path) => (
            format!("\n\n[Full output: {path} (read it with offset/limit)]"),
            Some(path),
        ),
        Err(error) => (
            format!("\n\n[Could not save the full output: {error}]"),
            None,
        ),
    };
    let tokens = truncation.total_bytes.div_ceil(4);
    let text = format!(
        "Warning: truncated output (original token count: {tokens})\nTotal output lines: {}\n\n{}{where_note}",
        truncation.total_lines, truncation.content,
    );
    let mut limited = vec![json!({ "type": "text", "text": text })];
    limited.extend(
        content
            .into_iter()
            .filter(|block| block.get("type").and_then(Json::as_str) == Some("image")),
    );
    (limited, full_output_path)
}

/// Text cut in the middle, with the size of the whole text
/// (`MiddleTruncation`, `output-limit.ts:50-56`).
struct MiddleTruncation {
    content: String,
    total_bytes: usize,
    total_lines: usize,
}

/// `truncateMiddle` (`output-limit.ts:62-77`): keep start and end, half of
/// `max_bytes` each, cutting only at character boundaries.
fn truncate_middle(text: &str, max_bytes: usize) -> Option<MiddleTruncation> {
    let buf = text.as_bytes();
    if buf.len() <= max_bytes {
        return None;
    }
    let is_boundary = |index: usize| index >= buf.len() || (buf[index] & 0xc0) != 0x80;
    let mut head_end = max_bytes / 2;
    while head_end > 0 && !is_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = buf.len() - (max_bytes - max_bytes / 2);
    while tail_start < buf.len() && !is_boundary(tail_start) {
        tail_start += 1;
    }
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let tail = String::from_utf8_lossy(&buf[tail_start..]).into_owned();
    let removed_chars = String::from_utf8_lossy(&buf[head_end..tail_start])
        .chars()
        .count();
    // A final newline ends the last line instead of starting another.
    let total_lines = text.split('\n').count() - usize::from(text.ends_with('\n'));
    Some(MiddleTruncation {
        content: format!("{head}…{removed_chars} chars truncated…{tail}"),
        total_bytes: buf.len(),
        total_lines,
    })
}

/// `writeOutputFile` (`utils/output-files.ts:24-30`): a `pi-mcp-<hex>`
/// file in the OS temp directory, mode 0600 on unix, never following a
/// link placed at the path.
fn write_output_file(extension: &str, data: &[u8]) -> Result<String, String> {
    let directory = std::env::temp_dir();
    for _ in 0..64 {
        let path = directory.join(format!("pi-mcp-{:016x}{extension}", unique_suffix()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                std::io::Write::write_all(&mut file, data).map_err(|error| error.to_string())?;
                return Ok(path.to_string_lossy().into_owned());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("could not create an output file".to_string())
}

/// Entropy for the output-file name: a process-local counter mixed with
/// nanoseconds (replaces `randomBytes(8)`, `utils/output-files.ts:20`).
fn unique_suffix() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0u64, |elapsed| elapsed.as_nanos() as u64);
    counter.rotate_left(32) ^ nanos
}

/// The synthetic stdio MCP server the bridge tests drive: a bash script
/// answering `initialize`, `tools/list`, and `tools/call` over
/// newline-delimited JSON-RPC (synthetic fixture; the client's request
/// key order `jsonrpc,id,method,params` makes the `case` matches exact).
#[cfg(test)]
const FIXTURE_SERVER_SCRIPT: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"synth","version":"1.0.0"},"instructions":"Synthetic server."}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echoes.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}},{"name":"boom","description":"Fails.","inputSchema":{"type":"object"}},{"name":"odd-name","description":"Odd.","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      case "$line" in
        *'"boom"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"it failed"}],"isError":true}}\n' "$id" ;;
        *) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"echo-ok"}],"structuredContent":{"ok":true}}}\n' "$id" ;;
      esac ;;
    *'"method":"ping"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
  esac
done
"#;

/// Writes the fixture server to `directory` as an executable script and
/// returns its path (the bridge tests point the server config at it).
#[cfg(test)]
pub(crate) fn write_fixture_server(
    directory: &std::path::Path,
) -> std::io::Result<std::path::PathBuf> {
    let path = directory.join("synth-server.sh");
    std::fs::write(&path, FIXTURE_SERVER_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_values_resolve_env_templates_and_commands() {
        // Literal.
        assert_eq!(resolve_config_value("plain").as_deref(), Some("plain"));
        // `$$` and `$!` escape.
        assert_eq!(resolve_config_value("a$$b").as_deref(), Some("a$b"));
        assert_eq!(resolve_config_value("a!b").as_deref(), Some("a!b"));
        // A missing variable resolves the whole value to undefined.
        assert_eq!(resolve_config_value("${PI_RS_MISSING_VAR_X}"), None);
        // A command value runs through the shell.
        assert_eq!(
            resolve_config_value("!printf hello").as_deref(),
            Some("hello")
        );
        // A failing command resolves to undefined.
        assert_eq!(resolve_config_value("!exit 3"), None);
    }

    #[test]
    fn missing_env_error_names_the_variable() {
        let error = missing_value_error("${PI_RS_MISSING_VAR_X}", "MCP server \"s\" env \"K\"");
        assert_eq!(
            error,
            "Failed to resolve MCP server \"s\" env \"K\" from environment variable: PI_RS_MISSING_VAR_X"
        );
        let error = missing_value_error("!exit 3", "MCP server \"s\" env \"K\"");
        assert_eq!(
            error,
            "Failed to resolve MCP server \"s\" env \"K\" from shell command: exit 3"
        );
    }

    #[test]
    fn expand_home_replaces_tilde() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return;
        }
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/bin/srv"), format!("{home}/bin/srv"));
        assert_eq!(expand_home("/bin/srv"), "/bin/srv");
    }

    #[test]
    fn result_mapping_follows_tools_ts() {
        let result: CallToolResult = serde_json::from_value(json!({
            "content": [
                {"type": "text", "text": "line one"},
                {"type": "image", "data": "AAAA", "mimeType": "image/png"},
                {"type": "audio", "data": "AAAA", "mimeType": "audio/wav"},
            ],
            "isError": false,
        }))
        .expect("decode");
        let mapped = convert_mcp_result("synth", "echo", &result);
        let content = mapped["content"].as_array().expect("content");
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["text"], "line one");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[2]["text"], "[audio audio/wav omitted]");
        // The script result is the CallToolResult without _meta.
        let script = &mapped["structuredContent"];
        assert_eq!(script["content"][0]["text"], "line one");
        assert!(script.get("isError").is_none(), "no error flag when false");
        assert_eq!(mapped["details"]["server"], "synth");
        assert_eq!(mapped["details"]["tool"], "echo");
        assert!(mapped.get("isError").is_none());
    }

    #[test]
    fn empty_error_results_get_the_fallback_text() {
        let result: CallToolResult =
            serde_json::from_value(json!({"content": [], "isError": true})).expect("decode");
        let mapped = convert_mcp_result("synth", "boom", &result);
        assert_eq!(
            mapped["content"][0]["text"],
            "MCP tool synth/boom returned an error"
        );
        assert_eq!(mapped["isError"], true);
        assert_eq!(mapped["structuredContent"]["isError"], true);
    }

    #[test]
    fn structured_only_results_become_json_text() {
        let result: CallToolResult = serde_json::from_value(json!({
            "content": [],
            "structuredContent": {"count": 2},
        }))
        .expect("decode");
        let mapped = convert_mcp_result("synth", "count", &result);
        assert_eq!(mapped["content"][0]["text"], "{\n  \"count\": 2\n}");
    }

    #[test]
    fn resource_blocks_map_to_text_and_images() {
        let result: CallToolResult = serde_json::from_value(json!({
            "content": [
                {"type": "resource_link", "uri": "file:///a.txt", "name": "a",
                 "mimeType": "text/plain", "size": 30},
                {"type": "resource", "resource": {"uri": "file:///b.txt", "text": "body"}},
                {"type": "resource", "resource": {"uri": "file:///c.png",
                 "blob": "AAAA", "mimeType": "image/png"}},
            ],
        }))
        .expect("decode");
        let mapped = convert_mcp_result("synth", "res", &result);
        let content = mapped["content"].as_array().expect("content");
        assert_eq!(
            content[0]["text"],
            "[Resource file:///a.txt \"a\" (text/plain, 30B)]"
        );
        assert_eq!(content[1]["text"], "body");
        assert_eq!(content[2]["type"], "image");
    }

    #[test]
    fn oversized_text_truncates_the_middle_and_saves_the_full_output() {
        let text = "x".repeat(MCP_OUTPUT_MAX_BYTES + 200);
        let result: CallToolResult = serde_json::from_value(json!({
            "content": [{"type": "text", "text": text}],
        }))
        .expect("decode");
        let mapped = convert_mcp_result("synth", "big", &result);
        let out = mapped["content"][0]["text"].as_str().expect("text");
        assert!(
            out.starts_with("Warning: truncated output (original token count: "),
            "{out}"
        );
        assert!(out.contains("chars truncated…"), "{out}");
        let path = mapped["details"]["fullOutputPath"].as_str().expect("path");
        let saved = std::fs::read_to_string(path).expect("saved file");
        assert_eq!(saved.len(), MCP_OUTPUT_MAX_BYTES + 200);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn truncate_middle_keeps_half_head_and_tail() {
        let text = "abcdefghij".repeat(100);
        let truncation = truncate_middle(&text, 64).expect("truncated");
        assert!(
            truncation.content.starts_with("abcdefghij"),
            "{}",
            truncation.content
        );
        assert!(truncation.content.contains("chars truncated…"));
        assert_eq!(truncation.total_bytes, 1000);
        assert_eq!(truncation.total_lines, 1);
        assert!(truncate_middle("short", 64).is_none());
    }

    #[test]
    fn format_size_matches_upstream_units() {
        assert_eq!(format_size(30.0), "30B");
        assert_eq!(format_size(2048.0), "2.0KB");
        assert_eq!(format_size(3.0 * 1024.0 * 1024.0), "3.0MB");
    }

    #[test]
    fn base64_decode_round_trips_ascii_and_utf8() {
        assert_eq!(base64_decode("SGVsbG8="), b"Hello".to_vec());
        assert_eq!(base64_decode("4pyT"), "✓".as_bytes().to_vec());
    }
}
