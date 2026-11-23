//! The native MCP server manager: the host half of the bundled `mcp`
//! extension (`packages/coding-agent/src/extensions/mcp/`) — connections
//! over stdio or streamable HTTP, tool discovery, tool calls, and the
//! result mapping the bridge registers.
//!
//! What each upstream piece becomes:
//!
//! * `runtime.ts:141-456` (`McpServerConnection`) — a per-server entry in
//!   the worker thread: `connect` is `open`/`connectOnce` (`:361-409`, the
//!   URL-server transient retries at `:371-376`; OAuth/`needs-auth` waits
//!   for the OAuth port), `call_tool` is `withClient` (`:280-303`) with
//!   the stdio lazy-reconnect arm and the HTTP session-expired retry, and
//!   the `getClient` guards the tool definitions close over
//!   (`index.ts:424-438`) run at call time.
//! * `runtime.ts:100-124` (`createDefaultTransport`) — [`spawn_config`]:
//!   the stdio arm (`~` expansion `:92-98`, env
//!   `resolveConfigValueOrThrow` `:113-116`, the session-cwd-relative
//!   `cwd` `:120`) and the HTTP arm (the `resolveHeadersOrThrow`
//!   headers, `resolve-config-value.ts:271-282`).
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
//! The HTTP transports' background stream tasks are polled during the
//! `block_on` windows the calls provide.
//!
//! The fidelity additions port four more upstream mechanisms:
//!
//! * `startupWaitMs` and the bounded first-prompt wait (`index.ts:87-94`,
//!   `:145-148`, `:294-296`, `:1154-1175`): [`McpManager::begin_connect`]
//!   starts a connection without blocking and [`StartupWait`] is a host-driven
//!   tick (no timers) that reports `Ready`/`Waiting`/`TimedOut`. Upstream's
//!   per-server `ready` promise (`index.ts:112-113`, `:594-608`) outlives that
//!   one-shot bound: the worker publishes every background connect outcome to
//!   the manager's shared queue, so a server that settles *after* the bound
//!   still reaches the host through [`McpManager::take_settled_connects`] on a
//!   later tick and registers its tools (`index.ts:1160-1162`).
//! * `notifications/tools/list_changed` (`runtime.ts:382-384`, `:432-442`):
//!   [`McpManager::try_drain_notifications`] asks the worker to run the
//!   non-blocking `try_recv_inbound` loop, re-list tools, and publish
//!   [`ToolChange`]s for re-registration; the host drains that shared queue
//!   with [`McpManager::take_tool_changes`]. The drain is an event on the
//!   worker's own request loop — there is no blocking barrier round-trip.
//!   [`McpManager::poll_notifications`] is the older, ack-waiting form kept for
//!   hosts that want the drain to be synchronous. A failed refresh records
//!   upstream's exact `Failed to refresh tools: …` text.
//! * The stderr tail on connect failures (`runtime.ts:51`, `:400-405`,
//!   `:411-420`): stdio servers get a bounded (2 000-char) stderr tail that
//!   is appended to the connect error as upstream does.
//! * Block-level `_meta`/`annotations` (`tools.ts:228-232`, `:237-273`): they
//!   survive the wire decode and are forwarded in the script result, while
//!   the top-level `_meta` is dropped. Tool-name `*`-patterns are matched
//!   with [`pi_mcp::types::tool_name_pattern_matches`], the faithful port of
//!   `core/mcp-servers.ts:17-34`.
//!
//! Not in this slice (recorded open): OAuth/`needs-auth` (`oauth.ts` and
//! the HTTP arm's sign-in flow), resources (`resources.ts`), and the
//! `/mcp` manager UI (`ui.ts`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{TryRecvError, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};

use pi_mcp::auth_provider::AuthProvider;
use pi_mcp::oauth::auth_provider::{McpAuthProvider, McpAuthProviderOptions};
use pi_mcp::oauth::{MCP_AUTH_FILE_NAME, McpOAuthCredentialStore, McpOAuthSettings};
use pi_mcp::resources::READ_MCP_RESOURCE_TOOL;
use pi_mcp::types::{ContentBlock, InboundMessage};
use pi_mcp::{CallToolResult, McpClient, McpError, McpServerConfig, McpTool, McpTransportConfig};

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
/// Delays between attempts to connect to an HTTP server that failed with a
/// transient error (`CONNECT_RETRY_DELAYS_MS`, `runtime.ts:54`).
const CONNECT_RETRY_DELAYS_MS: [u64; 2] = [250, 1_000];
/// Default bound on the first prompt's wait for startup connections
/// (`DEFAULT_STARTUP_WAIT_MS`, `index.ts:94`).
pub const DEFAULT_STARTUP_WAIT_MS: u64 = 10_000;
/// Characters of a failed stdio server's stderr kept for the connect error
/// (`STDERR_TAIL_CHARS`, `runtime.ts:51`).
const STDERR_TAIL_CHARS: usize = 2_000;
/// Env key carrying the stdio stderr sink path into the child process (a
/// port implementation detail; upstream reads `StdioTransport.stderr`).
const STDERR_PATH_ENV: &str = "PI_MCP_STDERR_PATH";

/// One configured server's connection state in the worker.
///
/// Both variants keep the spawn config so a failed server can reconnect
/// lazily on the next call (`runtime.ts:280-303` — stdio has no connect
/// retries) and a session-expired HTTP server can reconnect with a fresh
/// session (`withClient`'s retry, `runtime.ts:289-295`).
enum ServerState {
    Connected {
        client: McpClient,
        tools: Vec<Json>,
        /// `capabilities.resources` (`runtime.ts:393`) — only these servers
        /// are reachable through the MCP resource tools.
        has_resources: bool,
        timeout: Duration,
        spawn: McpServerConfig,
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
        reply: Option<std::sync::mpsc::Sender<Result<ConnectOutcome, String>>>,
        /// Background connects (`begin_connect`) also publish their outcome
        /// to the manager's per-server readiness queue, so it survives the
        /// one-shot startup wait. Synchronous `connect` calls do not.
        publish_settled: bool,
    },
    CallTool {
        server: String,
        tool: String,
        args: Json,
        reply: std::sync::mpsc::Sender<Result<Json, String>>,
    },
    /// Resource reads/lists (`resources.ts:100-107` — the native resource
    /// tools). Each reply follows the `{"result": …}` / `{"error": …}`
    /// convention so the execute wrapper can throw on an error.
    ListResourcePage {
        server: String,
        cursor: Option<String>,
        reply: std::sync::mpsc::Sender<Result<Json, String>>,
    },
    ListResources {
        server: String,
        reply: std::sync::mpsc::Sender<Result<Json, String>>,
    },
    ListTemplatePage {
        server: String,
        cursor: Option<String>,
        reply: std::sync::mpsc::Sender<Result<Json, String>>,
    },
    ListTemplates {
        server: String,
        reply: std::sync::mpsc::Sender<Result<Json, String>>,
    },
    ReadResource {
        server: String,
        uri: String,
        reply: std::sync::mpsc::Sender<Result<Json, String>>,
    },
    Drop {
        name: String,
    },
    /// Names of connected servers advertising the resources capability
    /// (`McpResourceServer[]`, `runtime.ts:393`).
    ServersWithResources {
        reply: std::sync::mpsc::Sender<Vec<String>>,
    },
    /// Host-driven event: drain server-initiated notifications with the
    /// worker's non-blocking `try_recv_inbound` loop and re-list tools on
    /// `notifications/tools/list_changed` (`runtime.ts:382-384`). The ack is
    /// sent after the changes are published to the shared queue; hosts that
    /// do not need the barrier drop it immediately.
    DrainNotifications {
        reply: std::sync::mpsc::Sender<()>,
    },
    ShutdownAll,
}

/// A server's tool list changed after a `notifications/tools/list_changed`
/// notification (`runtime.ts:432-442`). The host re-registers the tools in
/// `tools`; on a failed refresh `error` is upstream's exact
/// `Failed to refresh tools: …` message and `tools` is empty.
#[derive(Debug, Clone)]
pub struct ToolChange {
    pub server: String,
    pub tools: Vec<Json>,
    pub error: Option<String>,
}

/// A successful connect: the offered tools as JSON (decoded `McpTool`s) and
/// the server's `initialize` instructions.
#[derive(Debug, Clone)]
pub struct ConnectOutcome {
    pub tools: Vec<Json>,
    pub instructions: Option<String>,
    /// `capabilities.resources.is_some()` (`runtime.ts:393`).
    pub has_resources: bool,
}

/// One background connection's outcome, keyed by server name. The manager
/// owns the queue so a connection settling after the startup bound still
/// reaches the host (`index.ts:112-113`, `:594-608`).
pub type SettledConnects = Vec<(String, Result<ConnectOutcome, String>)>;

/// Handle to the worker thread sharing one worker per host core.
///
/// `Clone` shares the worker; dropping the last handle closes the request
/// channel and the worker kills the connected server processes.
#[derive(Clone, Default)]
pub struct McpManager {
    requests: Arc<OnceLock<Mutex<tokio::sync::mpsc::UnboundedSender<Request>>>>,
    /// Per-server request timeouts, recorded by the worker on connect so
    /// the caller's reply window covers the worker's internal wait.
    timeouts: Arc<Mutex<HashMap<String, Duration>>>,
    /// Agent config dir holding `mcp-auth.json`; `None` disables OAuth
    /// attachment (headless engines that do not host credentials).
    auth_dir: Option<std::path::PathBuf>,
    /// Tool re-registrations from `tools/list_changed`, drained by the host
    /// (`runtime.ts:432-442`).
    tool_changes: Arc<Mutex<Vec<ToolChange>>>,
    /// Outcomes of background connects not yet drained by the host. Keyed by
    /// server name (upstream's per-server `ready`) so late connections are
    /// delivered on a later tick instead of being lost with a dropped wait.
    settled_connects: Arc<Mutex<SettledConnects>>,
    /// Acks of [`Self::try_drain_notifications`] requests, reaped without
    /// blocking on the next tick. The changes themselves live in
    /// `tool_changes`; an ack only means "this drain ran".
    pending_drains: Arc<Mutex<Vec<std::sync::mpsc::Receiver<()>>>>,
}

impl McpManager {
    /// Attach the config dir whose `mcp-auth.json` holds MCP OAuth states.
    /// Must be called before the first connection (the worker starts lazily).
    pub fn set_auth_dir(&mut self, dir: std::path::PathBuf) {
        self.auth_dir = Some(dir);
    }

    /// The worker's request channel, spawning the thread on first use.
    fn sender(&self) -> tokio::sync::mpsc::UnboundedSender<Request> {
        self.requests
            .get_or_init(|| {
                Mutex::new(spawn_worker(
                    self.timeouts.clone(),
                    self.tool_changes.clone(),
                    self.settled_connects.clone(),
                    self.auth_dir.clone(),
                ))
            })
            .lock()
            .expect("mcp manager mutex poisoned")
            .clone()
    }

    /// Starts one connection without waiting for it, returning a handle the
    /// host's [`StartupWait`] polls (`index.ts:1142` starts connections in
    /// the background). The outcome is published to the manager's shared queue
    /// (upstream's per-server `ready`, `index.ts:112-113`, `:594-608`), so it
    /// is still available to [`Self::take_settled_connects`] after the bounded
    /// startup wait has been dropped.
    pub fn begin_connect(&self, name: &str, config: &Json, cwd: &str) -> PendingConnect {
        // A dead worker cannot settle anything; record the failure in the
        // same queue so the host still gets a per-server report.
        let sent = self
            .sender()
            .send(Request::Connect {
                name: name.to_string(),
                config: config.clone(),
                cwd: cwd.to_string(),
                reply: None,
                publish_settled: true,
            })
            .is_ok();
        if !sent {
            self.settled_connects
                .lock()
                .expect("mcp settled connects mutex poisoned")
                .push((
                    name.to_string(),
                    Err("MCP manager is not running".to_string()),
                ));
        }
        PendingConnect {
            name: name.to_string(),
            settled: self.settled_connects.clone(),
        }
    }

    /// Outcomes of background connections that have settled and not yet been
    /// drained by the host. Non-blocking; late connections appear here even
    /// after [`StartupWait`] timed out.
    pub fn take_settled_connects(&self) -> SettledConnects {
        std::mem::take(
            &mut *self
                .settled_connects
                .lock()
                .expect("mcp settled connects mutex poisoned"),
        )
    }

    /// Non-blocking notification drain. The worker's `try_recv_inbound` loop
    /// runs as an event on its own request channel; the host never waits on a
    /// barrier here. Acks of earlier drains are reaped on the same pass
    /// ([`Self::take_tool_changes`] also reaps them).
    pub fn try_drain_notifications(&self) {
        let (reply, receiver) = channel();
        if self
            .sender()
            .send(Request::DrainNotifications { reply })
            .is_ok()
        {
            self.pending_drains
                .lock()
                .expect("mcp pending drains mutex poisoned")
                .push(receiver);
        }
        self.reap_drains();
    }

    /// The synchronous form of [`Self::try_drain_notifications`], kept for
    /// hosts that used `poll_notifications` as a barrier. It waits for this
    /// drain's own ack instead of an unrelated manager round-trip.
    pub fn poll_notifications(&self) {
        let (reply, receiver) = channel();
        if self
            .sender()
            .send(Request::DrainNotifications { reply })
            .is_ok()
        {
            let _ = receiver.recv_timeout(REPLY_SLACK);
        }
        self.reap_drains();
    }

    /// Drops acks of drains that already ran (or whose worker is gone).
    fn reap_drains(&self) {
        self.pending_drains
            .lock()
            .expect("mcp pending drains mutex poisoned")
            .retain(|receiver| matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    /// Tool re-registrations accumulated since the last drain. Non-blocking:
    /// the worker publishes them into this shared queue as soon as a drain
    /// observes `notifications/tools/list_changed`.
    pub fn take_tool_changes(&self) -> Vec<ToolChange> {
        self.reap_drains();
        std::mem::take(
            &mut *self
                .tool_changes
                .lock()
                .expect("mcp tool changes mutex poisoned"),
        )
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
                reply: Some(reply),
                publish_settled: false,
            })
            .map_err(|_| "MCP manager is not running".to_string())?;
        reply_rx
            .recv_timeout(window)
            .unwrap_or_else(|_| Err("MCP manager did not answer in time".to_string()))
    }

    /// Names of connected servers that advertise resources
    /// (`runtime.ts:393`), for building the MCP resource tool definitions.
    pub fn servers_with_resources(&self) -> Vec<String> {
        let (reply, reply_rx) = std::sync::mpsc::channel();
        if self
            .sender()
            .send(Request::ServersWithResources { reply })
            .is_err()
        {
            return Vec::new();
        }
        reply_rx.recv_timeout(REPLY_SLACK).unwrap_or_default()
    }

    /// One resource request, replied to as `{"result": …}` or
    /// `{"error": …}` so the JS execute wrapper can throw on an error.
    fn resource_request(
        &self,
        server: &str,
        build: impl FnOnce(std::sync::mpsc::Sender<Result<Json, String>>) -> Request,
    ) -> Json {
        let window = self.reply_window(server, config_default_window());
        let (reply, reply_rx) = std::sync::mpsc::channel();
        let outcome = self
            .sender()
            .send(build(reply))
            .map_err(|_| "MCP manager is not running".to_string())
            .and_then(|()| {
                reply_rx
                    .recv_timeout(window)
                    .unwrap_or_else(|_| Err("MCP manager did not answer in time".to_string()))
            });
        match outcome {
            Ok(value) => json!({ "result": value }),
            Err(error) => json!({ "error": error }),
        }
    }

    /// One `resources/list` page (`client.ts:306-309`).
    pub fn list_resources_page(&self, server: &str, cursor: Option<&str>) -> Json {
        self.resource_request(server, |reply| Request::ListResourcePage {
            server: server.to_string(),
            cursor: cursor.map(str::to_string),
            reply,
        })
    }

    /// Every resource, following `nextCursor`.
    pub fn list_resources(&self, server: &str) -> Json {
        self.resource_request(server, |reply| Request::ListResources {
            server: server.to_string(),
            reply,
        })
    }

    /// One `resources/templates/list` page.
    pub fn list_resource_templates_page(&self, server: &str, cursor: Option<&str>) -> Json {
        self.resource_request(server, |reply| Request::ListTemplatePage {
            server: server.to_string(),
            cursor: cursor.map(str::to_string),
            reply,
        })
    }

    /// Every resource template, following `nextCursor`.
    pub fn list_resource_templates(&self, server: &str) -> Json {
        self.resource_request(server, |reply| Request::ListTemplates {
            server: server.to_string(),
            reply,
        })
    }

    /// `resources/read` for one URI.
    pub fn read_resource(&self, server: &str, uri: &str) -> Json {
        self.resource_request(server, |reply| Request::ReadResource {
            server: server.to_string(),
            uri: uri.to_string(),
            reply,
        })
    }

    /// Build the final tool result for one of the three MCP resource tools
    /// (`resources.ts:179-193`, `:236-260`): payload/text, truncation details
    /// and structured content, all in the shape the tool bridge expects.
    pub fn resource_tool_result(&self, tool: &str, params: &Json) -> Json {
        use pi_mcp::resources as res;
        let servers = self.servers_with_resources();
        match tool {
            res::LIST_MCP_RESOURCES_TOOL => {
                self.list_tool_payload(tool, params, "resources", &servers, false)
            }
            res::LIST_MCP_RESOURCE_TEMPLATES_TOOL => {
                self.list_tool_payload(tool, params, "resourceTemplates", &servers, true)
            }
            res::READ_MCP_RESOURCE_TOOL => self.read_tool_result(tool, params, &servers),
            _ => json!({ "error": format!("Unknown MCP resource tool: {tool}") }),
        }
    }

    fn list_tool_payload(
        &self,
        tool: &str,
        params: &Json,
        key: &str,
        servers: &[String],
        templates: bool,
    ) -> Json {
        use pi_mcp::resources as res;
        let reply_result = |reply: Json| -> Result<Json, McpError> {
            reply.get("result").cloned().ok_or_else(|| {
                McpError::Malformed(
                    reply
                        .get("error")
                        .and_then(Json::as_str)
                        .unwrap_or("resource list failed")
                        .to_string(),
                )
            })
        };
        let page = |server: &str, cursor: Option<&str>| -> Result<res::Page, McpError> {
            let reply = if templates {
                self.list_resource_templates_page(server, cursor)
            } else {
                self.list_resources_page(server, cursor)
            };
            let value = reply_result(reply)?;
            if templates {
                let parsed = res::parse_list_resource_templates_page(&value)?;
                Ok(res::Page {
                    items: parsed
                        .resource_templates
                        .into_iter()
                        .map(|item| serde_json::to_value(item).unwrap_or(Json::Null))
                        .collect(),
                    next_cursor: parsed.next_cursor,
                })
            } else {
                let parsed = res::parse_list_resources_page(&value)?;
                Ok(res::Page {
                    items: parsed
                        .resources
                        .into_iter()
                        .map(|item| serde_json::to_value(item).unwrap_or(Json::Null))
                        .collect(),
                    next_cursor: parsed.next_cursor,
                })
            }
        };
        let all = |server: &str| -> Result<Vec<Json>, McpError> {
            let reply = if templates {
                self.list_resource_templates(server)
            } else {
                self.list_resources(server)
            };
            let value = reply_result(reply)?;
            if templates {
                let parsed = res::parse_list_resource_templates_page(&value)?;
                Ok(parsed
                    .resource_templates
                    .into_iter()
                    .map(|item| serde_json::to_value(item).unwrap_or(Json::Null))
                    .collect())
            } else {
                let parsed = res::parse_list_resources_page(&value)?;
                Ok(parsed
                    .resources
                    .into_iter()
                    .map(|item| serde_json::to_value(item).unwrap_or(Json::Null))
                    .collect())
            }
        };
        match res::list_payload(Some(params), key, servers, page, all) {
            Ok(payload) => Self::finish_tool(tool, params, &payload),
            Err(error) => json!({ "error": res::error_message(&error) }),
        }
    }

    fn read_tool_result(&self, tool: &str, params: &Json, servers: &[String]) -> Json {
        use pi_mcp::resources as res;
        let uri = match res::string_argument(Some(params), "uri") {
            Ok(Some(uri)) => uri,
            Ok(None) => return json!({ "error": "uri must be provided" }),
            Err(error) => return json!({ "error": res::error_message(&error) }),
        };
        let server = match res::string_argument(Some(params), "server") {
            Ok(Some(server)) => server,
            Ok(None) => return json!({ "error": "server must be provided" }),
            Err(error) => return json!({ "error": res::error_message(&error) }),
        };
        if !servers.contains(&server) {
            return json!({
                "error": res::find_server_error(&server, servers.iter().map(String::as_str))
            });
        }
        let reply = self.read_resource(&server, &uri);
        let Some(value) = reply.get("result") else {
            return json!({
                "error": reply
                    .get("error")
                    .and_then(Json::as_str)
                    .unwrap_or("resource read failed")
            });
        };
        let read = match res::parse_read_resource_result(value) {
            Ok(read) => read,
            Err(error) => return json!({ "error": res::error_message(&error) }),
        };
        let blocks = res::contents_to_model_blocks(&read, &uri, |bytes, extension| {
            write_output_file(extension, bytes)
        });
        let blocks = if blocks.is_empty() {
            vec![json!({
                "type": "text",
                "text": format!("Resource {uri} is empty.")
            })]
        } else {
            blocks
        };
        let (content, full_output_path) = limit_mcp_content(blocks);
        let mut details = json!({ "server": server, "tool": tool });
        if let Some(path) = full_output_path {
            details["fullOutputPath"] = json!(path);
        }
        json!({
            "content": content,
            "details": details,
            "structuredContent": res::read_structured_content(&server, &uri, &read),
        })
    }

    fn finish_tool(tool: &str, params: &Json, payload: &Json) -> Json {
        let text = serde_json::to_string(&payload).unwrap_or_default();
        let (content, full_output_path) = limit_mcp_content(vec![json!({
            "type": "text",
            "text": text,
        })]);
        let mut details = json!({
            "server": params.get("server").and_then(Json::as_str).unwrap_or(""),
            "tool": tool,
        });
        if let Some(path) = full_output_path {
            details["fullOutputPath"] = json!(path);
        }
        json!({
            "content": content,
            "details": details,
            "structuredContent": payload,
        })
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
        self.settled_connects
            .lock()
            .expect("mcp settled connects mutex poisoned")
            .retain(|(settled_name, _)| settled_name != name);
    }

    /// Drops every connection (the `session_shutdown` close,
    /// `index.ts:1250-1256`).
    pub fn shutdown_all(&self) {
        let _ = self.sender().send(Request::ShutdownAll);
        self.settled_connects
            .lock()
            .expect("mcp settled connects mutex poisoned")
            .clear();
    }
}

/// One in-flight startup connection, polled by [`StartupWait`] instead of
/// blocking the host (upstream's `server.ready` promise, `index.ts:113`,
/// `:1160-1162`). The handle only names the server and shares the manager's
/// outcome queue; dropping it after the startup bound cannot lose the
/// connection's result, which stays available to
/// [`McpManager::take_settled_connects`].
pub struct PendingConnect {
    name: String,
    settled: Arc<Mutex<SettledConnects>>,
}

/// The bounded wait the first prompt performs for servers whose tools are
/// declared to the model (`index.ts:87-94`, `:145-148`, `:294-296`,
/// `:1154-1175`). Entirely host-driven: the host supplies `now` on each
/// tick, so no timer thread or `sleep` is involved. The wait is one-shot,
/// but the connections it polls are not: their outcomes live in the
/// manager's queue and are drained by the host on later ticks.
pub struct StartupWait {
    started: Instant,
    wait: Duration,
    pending: Vec<PendingConnect>,
    settled: Vec<(String, Result<ConnectOutcome, String>)>,
    waited: bool,
}

/// What the host should do on one [`StartupWait::tick`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupTick {
    /// No connections are pending (or the first prompt already waited).
    Ready,
    /// Still connecting; `remaining` is the time left before the bound.
    Waiting { remaining: Duration },
    /// The bound elapsed with connections still pending; upstream notifies
    /// that their tools become available once connected (`index.ts:1172-1174`).
    TimedOut,
}

impl StartupWait {
    /// A wait of `wait_ms` starting now. The default is
    /// [`DEFAULT_STARTUP_WAIT_MS`] (upstream `index.ts:94,296`).
    #[must_use]
    pub fn new(wait_ms: u64) -> Self {
        Self::new_with_start(wait_ms, Instant::now())
    }

    /// [`Self::new`] with an explicit start instant, for host-driven tests.
    #[must_use]
    pub fn new_with_start(wait_ms: u64, started: Instant) -> Self {
        Self {
            started,
            wait: Duration::from_millis(wait_ms),
            pending: Vec::new(),
            settled: Vec::new(),
            waited: false,
        }
    }

    /// Registers a server whose tools are `direct` and still connecting
    /// (`index.ts:1160-1162`).
    pub fn add(&mut self, name: &str, config: &Json, cwd: &str, manager: &McpManager) {
        self.pending.push(manager.begin_connect(name, config, cwd));
    }

    /// Advances the wait. Settled connections move into [`Self::drain_settled`];
    /// the first prompt proceeds when none remain or the bound elapsed. A
    /// second call is a no-op, matching upstream's one-shot `waitedForStartup`.
    /// Outcomes still in the queue after the wait is dropped are picked up by
    /// [`McpManager::take_settled_connects`] on a later host tick.
    pub fn tick(&mut self, now: Instant) -> StartupTick {
        if self.waited {
            return StartupTick::Ready;
        }
        let mut pending = Vec::new();
        for connect in self.pending.drain(..) {
            // The manager publishes the outcome under the server's name;
            // non-blocking, like upstream's settled `ready` promise.
            let mut queue = connect
                .settled
                .lock()
                .expect("mcp settled connects mutex poisoned");
            let position = queue.iter().position(|(name, _)| name == &connect.name);
            if let Some(position) = position {
                self.settled.push((connect.name, queue.remove(position).1));
            } else {
                drop(queue);
                pending.push(connect);
            }
        }
        self.pending = pending;
        if self.pending.is_empty() {
            self.waited = true;
            return StartupTick::Ready;
        }
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= self.wait {
            self.waited = true;
            return StartupTick::TimedOut;
        }
        StartupTick::Waiting {
            // `elapsed >= self.wait` returned above, so this is infallible.
            remaining: self.wait.checked_sub(elapsed).unwrap(),
        }
    }

    /// Outcomes of connections that settled during earlier ticks.
    pub fn drain_settled(&mut self) -> Vec<(String, Result<ConnectOutcome, String>)> {
        std::mem::take(&mut self.settled)
    }

    /// Whether the first prompt has already waited (upstream
    /// `waitedForStartup`, `index.ts:295`).
    #[must_use]
    pub fn waited(&self) -> bool {
        self.waited
    }

    /// Connections still in flight.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
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
    tool_changes: Arc<Mutex<Vec<ToolChange>>>,
    settled_connects: Arc<Mutex<SettledConnects>>,
    auth_dir: Option<std::path::PathBuf>,
) -> tokio::sync::mpsc::UnboundedSender<Request> {
    // A tokio request channel lets the worker `block_on(recv())`: its
    // current-thread runtime keeps driving the transports' reader tasks
    // while idle, so a notification arriving *between* host ticks is queued
    // for the next non-blocking drain. A std `recv()` froze those tasks.
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Request>();
    std::thread::Builder::new()
        .name("pi-ext-js-mcp".to_string())
        .spawn(move || {
            worker_loop(
                &mut receiver,
                &timeouts,
                &tool_changes,
                &settled_connects,
                auth_dir.as_deref(),
            );
        })
        .expect("spawn MCP manager worker thread");
    sender
}

/// The worker: owns the tokio runtime and every connection. When all
/// manager handles drop, the channel closes and remaining clients shut
/// down (their transports kill the children).
#[allow(clippy::too_many_lines)] // mirrors upstream's single worker loop; split when extending
fn worker_loop(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<Request>,
    timeouts: &Arc<Mutex<HashMap<String, Duration>>>,
    tool_changes: &Arc<Mutex<Vec<ToolChange>>>,
    settled_connects: &Arc<Mutex<SettledConnects>>,
    auth_dir: Option<&std::path::Path>,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build MCP manager runtime");
    let mut servers: HashMap<String, ServerEntry> = HashMap::new();
    while let Some(request) = runtime.block_on(receiver.recv()) {
        // Notifications are an explicit host-tick event now
        // (`Request::DrainNotifications`), not a side effect of an unrelated
        // wake-up; the drain runs the non-blocking try_recv loop there.
        match request {
            Request::Connect {
                name,
                config,
                cwd,
                reply,
                publish_settled,
            } => {
                let timeout = config_timeout(&config);
                let outcome = match spawn_config(&name, &config, &cwd) {
                    Ok(spawn) => {
                        match connect_with_retries(
                            &runtime,
                            &mut servers,
                            &name,
                            &spawn,
                            timeout,
                            auth_dir,
                        ) {
                            Ok(outcome) => Ok(outcome),
                            // `connect_with_spawn` records the stderr-tail
                            // augmented text in the Failed entry; surface
                            // that exact text (`runtime.ts:411-420`).
                            Err(error) => {
                                Err(failed_error_text(&servers, &name, &error.to_string()))
                            }
                        }
                    }
                    Err(error) => Err(error),
                };
                if outcome.is_ok() {
                    timeouts
                        .lock()
                        .expect("mcp timeouts mutex poisoned")
                        .insert(name.clone(), timeout);
                } else {
                    timeouts
                        .lock()
                        .expect("mcp timeouts mutex poisoned")
                        .remove(&name);
                }
                if publish_settled {
                    // Per-server readiness outlives the one-shot startup
                    // wait (`index.ts:112-113`, `:594-608`): a late result
                    // is still delivered to the host's next tick.
                    let published = match &outcome {
                        Ok(value) => Ok(value.clone()),
                        Err(error) => Err(error.clone()),
                    };
                    settled_connects
                        .lock()
                        .expect("mcp settled connects mutex poisoned")
                        .push((name.clone(), published));
                }
                if let Some(reply) = reply {
                    let _ = reply.send(outcome);
                }
            }
            Request::CallTool {
                server,
                tool,
                args,
                reply,
            } => {
                let outcome = call_tool_on(&runtime, &mut servers, &server, &tool, args, auth_dir);
                let _ = reply.send(outcome);
            }
            Request::ListResourcePage {
                server,
                cursor,
                reply,
            } => {
                let outcome = resource_reply(
                    &runtime,
                    &mut servers,
                    &server,
                    auth_dir,
                    |client, timeout| {
                        let page = runtime
                            .block_on(client.resources_list_page(cursor.as_deref(), timeout))?;
                        Ok(serde_json::to_value(page).unwrap_or(Json::Null))
                    },
                );
                let _ = reply.send(outcome);
            }
            Request::ListResources { server, reply } => {
                let outcome = resource_reply(
                    &runtime,
                    &mut servers,
                    &server,
                    auth_dir,
                    |client, timeout| {
                        let items = runtime.block_on(client.resources_list(timeout))?;
                        Ok(serde_json::to_value(items).unwrap_or(Json::Null))
                    },
                );
                let _ = reply.send(outcome);
            }
            Request::ListTemplatePage {
                server,
                cursor,
                reply,
            } => {
                let outcome = resource_reply(
                    &runtime,
                    &mut servers,
                    &server,
                    auth_dir,
                    |client, timeout| {
                        let page = runtime.block_on(
                            client.resources_templates_list_page(cursor.as_deref(), timeout),
                        )?;
                        Ok(serde_json::to_value(page).unwrap_or(Json::Null))
                    },
                );
                let _ = reply.send(outcome);
            }
            Request::ListTemplates { server, reply } => {
                let outcome = resource_reply(
                    &runtime,
                    &mut servers,
                    &server,
                    auth_dir,
                    |client, timeout| {
                        let items = runtime.block_on(client.resources_templates_list(timeout))?;
                        Ok(serde_json::to_value(items).unwrap_or(Json::Null))
                    },
                );
                let _ = reply.send(outcome);
            }
            Request::ReadResource { server, uri, reply } => {
                let outcome = resource_reply(
                    &runtime,
                    &mut servers,
                    &server,
                    auth_dir,
                    |client, timeout| {
                        let result = runtime.block_on(client.resource_read(&uri, timeout))?;
                        Ok(serde_json::to_value(result).unwrap_or(Json::Null))
                    },
                );
                let _ = reply.send(outcome);
            }
            Request::Drop { name } => {
                drop_entry(&runtime, &mut servers, &name);
                timeouts
                    .lock()
                    .expect("mcp timeouts mutex poisoned")
                    .remove(&name);
            }
            Request::ServersWithResources { reply } => {
                let names = servers
                    .iter()
                    .filter(|(_, entry)| {
                        matches!(
                            &entry.state,
                            ServerState::Connected {
                                has_resources: true,
                                ..
                            }
                        )
                    })
                    .map(|(name, _)| name.clone())
                    .collect();
                let _ = reply.send(names);
            }
            Request::DrainNotifications { reply } => {
                // The try_recv loop in `collect_tool_changes` is the port's
                // notification listener (`runtime.ts:382-384`): it never
                // blocks, and the changes are published to the shared queue
                // before the ack is sent.
                collect_tool_changes(&runtime, &mut servers, tool_changes);
                let _ = reply.send(());
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

/// The connect error text recorded by `connectFailed` (`runtime.ts:411-420`):
/// the stderr-augmented text stored by [`connect_with_spawn`], or `fallback`
/// when no Failed entry was recorded.
fn failed_error_text(servers: &HashMap<String, ServerEntry>, name: &str, fallback: &str) -> String {
    servers
        .get(name)
        .and_then(|entry| match &entry.state {
            ServerState::Failed { error, .. } => Some(error.clone()),
            ServerState::Connected { .. } => None,
        })
        .unwrap_or_else(|| fallback.to_string())
}

/// Drains server-initiated messages and re-lists tools on
/// `notifications/tools/list_changed` (`runtime.ts:382-384`, `:432-442`),
/// publishing the results for the host to re-register.
fn collect_tool_changes(
    runtime: &tokio::runtime::Runtime,
    servers: &mut HashMap<String, ServerEntry>,
    tool_changes: &Arc<Mutex<Vec<ToolChange>>>,
) {
    let names: Vec<String> = servers
        .iter()
        .filter_map(|(name, entry)| {
            let ServerState::Connected { client, .. } = &entry.state else {
                return None;
            };
            let mut changed = false;
            while let Some(message) = client.try_recv_inbound() {
                if let InboundMessage::Notification(notification) = &message
                    && notification.method == "notifications/tools/list_changed"
                {
                    changed = true;
                }
                // `notifications/message` is still discarded here. Upstream
                // forwards it to the `mcp.log` file in the agent directory
                // (`extensions/mcp/log.ts:1-69`, `:43-60`), but the MCP worker
                // has no log-event host seam: the only log sink in this crate
                // is the JS host's `ui_log` (engine.rs:313), out of this
                // module's reach. Wiring it needs a host op or log event in
                // runner.rs/glue.rs (outside this slice's ownership).
            }
            changed.then(|| name.clone())
        })
        .collect();
    if names.is_empty() {
        return;
    }
    let mut changes = Vec::new();
    for name in names {
        let Some(entry) = servers.get_mut(&name) else {
            continue;
        };
        let ServerState::Connected {
            client,
            tools,
            timeout,
            ..
        } = &mut entry.state
        else {
            continue;
        };
        let timeout = *timeout;
        match runtime.block_on(client.tools_list_with_timeout(timeout)) {
            Ok(list) => {
                *tools = list.tools.iter().map(mcp_tool_definition_json).collect();
                changes.push(ToolChange {
                    server: name,
                    tools: tools.clone(),
                    error: None,
                });
            }
            Err(error) => {
                // Upstream `refreshTools`'s error text, verbatim
                // (`runtime.ts:439`).
                changes.push(ToolChange {
                    server: name,
                    tools: Vec::new(),
                    error: Some(format!("Failed to refresh tools: {error}")),
                });
            }
        }
    }
    if !changes.is_empty() {
        tool_changes
            .lock()
            .expect("mcp tool changes mutex poisoned")
            .extend(changes);
    }
}

/// The last `STDERR_TAIL_CHARS` characters of a failed stdio server's
/// stderr (`sliceTail(transport.stderr.trim(), STDERR_TAIL_CHARS)`,
/// `runtime.ts:403`). The UTF-16 code-unit boundary from upstream's
/// `sliceTail` is approximated with a character boundary (stderr is ASCII in
/// practice).
fn stdio_stderr_tail(spawn: &McpServerConfig) -> Option<String> {
    let McpTransportConfig::Stdio { env, .. } = &spawn.transport else {
        return None;
    };
    let path = env.get(STDERR_PATH_ENV)?;
    let text = std::fs::read_to_string(path).ok()?;
    Some(slice_tail_chars(text.trim(), STDERR_TAIL_CHARS))
}

/// Removes the stdio stderr sink once its tail has been read (or the
/// connection succeeded); the port's stand-in for upstream dropping the
/// transport's stderr buffer.
fn clear_stderr_sink(spawn: &McpServerConfig) {
    if let McpTransportConfig::Stdio { env, .. } = &spawn.transport
        && let Some(path) = env.get(STDERR_PATH_ENV)
    {
        let _ = std::fs::remove_file(path);
    }
}

/// Upstream's `sliceTail` (`core/tools/truncate.ts:288-294`) by characters.
fn slice_tail_chars(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    chars[chars.len() - max..].iter().collect()
}

/// Connects (or reconnects) one server, replacing any existing entry, with
/// the URL-server connect retries (`open`, `runtime.ts:358-377`: URL
/// servers retry the two transient-failure delays).
fn connect_with_retries(
    runtime: &tokio::runtime::Runtime,
    servers: &mut HashMap<String, ServerEntry>,
    name: &str,
    spawn: &McpServerConfig,
    timeout: Duration,
    auth_dir: Option<&std::path::Path>,
) -> Result<ConnectOutcome, McpError> {
    let retries: &[u64] = match spawn.transport {
        McpTransportConfig::Http { .. } => &CONNECT_RETRY_DELAYS_MS,
        McpTransportConfig::Stdio { .. } => &[],
    };
    for attempt in 0.. {
        match connect_with_spawn(runtime, servers, name, spawn.clone(), timeout, auth_dir) {
            Ok(outcome) => return Ok(outcome),
            Err(error) => {
                let Some(delay_ms) = retries.get(attempt) else {
                    return Err(error);
                };
                if !error_is_transient(&error) {
                    return Err(error);
                }
                // The worker is serial, so the wait cannot overlap a
                // shutdown request (upstream sleeps with its shutdown
                // signal, `runtime.ts:377-379`).
                std::thread::sleep(Duration::from_millis(*delay_ms));
            }
        }
    }
    unreachable!("retry loop returns on every path");
}

/// Whether a connect failure is worth another attempt
/// (`isTransientError`, `runtime.ts:55-62`): 408, 429, 5xx-but-not-501, or
/// a network failure. (The stream-resumption classifier in pi-mcp has no
/// 501 exclusion; this is the connection-level one.)
fn error_is_transient(error: &McpError) -> bool {
    match error {
        McpError::Http { status, .. } => {
            *status == 408 || *status == 429 || (*status >= 500 && *status != 501)
        }
        McpError::Network(_) => true,
        _ => false,
    }
}

/// Attach the `mcp-auth.json` credential provider to an HTTP server
/// (`createMcpAuthProvider`, `auth-provider.ts:55-134`). Stored OAuth
/// states are read per server; a missing/corrupt file simply yields a
/// provider with no token, which the transport treats as unauthenticated.
fn http_auth_provider(
    name: &str,
    spawn: &McpServerConfig,
    auth_dir: &std::path::Path,
) -> Option<Arc<dyn AuthProvider>> {
    let McpTransportConfig::Http { url, .. } = &spawn.transport else {
        return None;
    };
    let store = McpOAuthCredentialStore::with_path(auth_dir.join(MCP_AUTH_FILE_NAME));
    let server_store = store.for_server(name, url).ok()?;
    Some(Arc::new(McpAuthProvider::new(McpAuthProviderOptions {
        server_url: url.clone(),
        store: Arc::new(server_store),
        settings: Arc::new(McpOAuthSettings::default),
        client_name: "pi".to_string(),
        client_metadata_documents: None,
        on_challenge: Arc::new(|_| {}),
        redirect_url: None,
    })))
}

/// Connects (or reconnects) one server, replacing any existing entry
/// (`connectOnce`, `runtime.ts:381-409`).
fn connect_with_spawn(
    runtime: &tokio::runtime::Runtime,
    servers: &mut HashMap<String, ServerEntry>,
    name: &str,
    spawn: McpServerConfig,
    timeout: Duration,
    auth_dir: Option<&std::path::Path>,
) -> Result<ConnectOutcome, McpError> {
    let auth = auth_dir.and_then(|dir| http_auth_provider(name, &spawn, dir));
    let attempt = runtime.block_on(async {
        let client = McpClient::connect_with_auth(&spawn, auth, timeout).await?;
        // Servers without the tools capability (prompts or resources only)
        // do not answer tools/list (`runtime.ts:387-388`).
        let tools = if client.capabilities.tools.is_some() {
            client.tools_list_with_timeout(timeout).await?.tools
        } else {
            Vec::new()
        };
        let tools = tools
            .iter()
            .map(mcp_tool_definition_json)
            .collect::<Vec<_>>();
        let has_resources = client.capabilities.resources.is_some();
        let instructions = client.instructions.clone();
        Ok::<_, McpError>((client, tools, has_resources, instructions))
    });
    let (client, tools, has_resources, instructions) = match attempt {
        Ok(attempt) => attempt,
        Err(error) => {
            // Upstream `connectOnce` captures the stdio transport's stderr
            // tail before `connectFailed` composes the error
            // (`runtime.ts:400-405`, `:411-420`).
            let error_text = match stdio_stderr_tail(&spawn) {
                Some(tail) if !tail.is_empty() => format!("{error}\n{tail}"),
                _ => error.to_string(),
            };
            clear_stderr_sink(&spawn);
            servers.insert(
                name.to_string(),
                ServerEntry {
                    state: ServerState::Failed {
                        spawn,
                        timeout,
                        error: error_text,
                    },
                },
            );
            return Err(error);
        }
    };
    clear_stderr_sink(&spawn);
    let previous = servers.insert(
        name.to_string(),
        ServerEntry {
            state: ServerState::Connected {
                client,
                tools: tools.clone(),
                has_resources,
                timeout,
                spawn,
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
        has_resources,
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

/// One tool call with the execution-time guards, then the wire call
/// (`withClient`, `runtime.ts:276-303`).
/// Run one read-only resource request against a connected server. A failed
/// server is lazily reconnected first (`withClient(…, true)`,
/// `runtime.ts:280-303`); reads are idempotent, so one reconnect is enough.
fn resource_reply(
    runtime: &tokio::runtime::Runtime,
    servers: &mut HashMap<String, ServerEntry>,
    server: &str,
    auth_dir: Option<&std::path::Path>,
    op: impl FnOnce(&McpClient, Duration) -> Result<Json, McpError>,
) -> Result<Json, String> {
    let reconnect = match servers.get(server).map(|entry| &entry.state) {
        Some(ServerState::Failed { spawn, timeout, .. }) => Some((spawn.clone(), *timeout)),
        Some(ServerState::Connected { .. }) => None,
        None => return Err(format!("MCP server \"{server}\" is not connected")),
    };
    if let Some((spawn, timeout)) = reconnect
        && let Err(error) = connect_with_spawn(runtime, servers, server, spawn, timeout, auth_dir)
    {
        // Same stderr-augmented `connectFailed` text as the other
        // reconnect paths (`runtime.ts:411-420`).
        return Err(failed_error_text(servers, server, &error.to_string()));
    }
    let entry = servers
        .get(server)
        .ok_or_else(|| format!("MCP server \"{server}\" is not connected"))?;
    let ServerState::Connected {
        client, timeout, ..
    } = &entry.state
    else {
        return Err(format!("MCP server \"{server}\" is disabled"));
    };
    op(client, *timeout).map_err(|error| error.to_string())
}

fn call_tool_on(
    runtime: &tokio::runtime::Runtime,
    servers: &mut HashMap<String, ServerEntry>,
    server: &str,
    tool: &str,
    args: Json,
    auth_dir: Option<&std::path::Path>,
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
        if connect_with_spawn(runtime, servers, server, spawn, timeout, auth_dir).is_err()
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
    let ServerState::Connected { tools, .. } = &entry.state else {
        return Err(format!("MCP server \"{server}\" is disabled."));
    };
    // A tool the server no longer offers is no longer available
    // (`index.ts:433-435`); the guard runs once, before the call.
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
    let mut session_retry = true;
    loop {
        let ServerState::Connected {
            client, timeout, ..
        } = &servers[server].state
        else {
            return Err(format!("MCP server \"{server}\" is disabled."));
        };
        let result = runtime.block_on(client.tools_call(tool, arguments.clone(), *timeout));
        match result {
            Ok(result) => return Ok(convert_mcp_result(server, tool, &result)),
            Err(McpError::SessionExpired { .. }) if session_retry => {
                // The server no longer knows the session (restart, deploy),
                // so it did not run the request; retry once on a new
                // session (`runtime.ts:289-295`). Upstream detaches the old
                // client without closing it, so concurrent in-flight calls
                // survive; the worker is serial, so the reconnect replaces
                // the entry (closing the old transport).
                session_retry = false;
                let (spawn, timeout) = match &servers[server].state {
                    ServerState::Connected { spawn, timeout, .. } => (spawn.clone(), *timeout),
                    ServerState::Failed { .. } => {
                        return Err(format!("MCP server \"{server}\" is disabled."));
                    }
                };
                if let Err(error) =
                    connect_with_spawn(runtime, servers, server, spawn, timeout, auth_dir)
                {
                    return Err(failed_error_text(servers, server, &error.to_string()));
                }
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// The spawn config from a server entry (`createDefaultTransport`,
/// `runtime.ts:103-124`): the HTTP arm for a `url`, the stdio arm
/// otherwise.
fn spawn_config(name: &str, config: &Json, cwd: &str) -> Result<McpServerConfig, String> {
    if let Some(url) = config.get("url").and_then(Json::as_str) {
        // `resolveHeadersOrThrow` (`resolve-config-value.ts:271-282`):
        // every header value resolves like every config value, with the
        // per-key description.
        let mut headers = HashMap::new();
        if let Some(record) = config.get("headers").and_then(Json::as_object) {
            for (key, value) in record {
                let Some(raw) = value.as_str() else {
                    continue;
                };
                let resolved = resolve_config_value_or_throw(
                    raw,
                    &format!("MCP server \"{name}\" header \"{key}\""),
                )?;
                headers.insert(key.clone(), resolved);
            }
        }
        return Ok(McpServerConfig {
            name: name.to_string(),
            transport: McpTransportConfig::Http {
                url: url.to_string(),
                headers,
            },
        });
    }
    let Some(command) = config.get("command").and_then(Json::as_str) else {
        return Err(format!("MCP server \"{name}\": config has no \"command\""));
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
    // The stdio transport discards stderr; route it to a private sink so a
    // failed connect can include its tail like upstream's `stderr: "pipe"`
    // (`runtime.ts:122`, `:400-405`).
    let stderr_path = std::env::temp_dir().join(format!("pi-mcp-stderr-{:016x}", unique_suffix()));
    env.insert(
        STDERR_PATH_ENV.to_string(),
        stderr_path.to_string_lossy().into_owned(),
    );
    let (command, args) = wrap_stdio_stderr(expand_home(command), args);
    Ok(McpServerConfig {
        name: name.to_string(),
        transport: McpTransportConfig::Stdio {
            command,
            args,
            env,
            working_dir: Some(working_dir),
        },
    })
}

/// Wraps a stdio command so its stderr goes to `$PI_MCP_STDERR_PATH` while
/// stdin/stdout stay untouched. `exec` keeps the server as the direct child
/// (signals and exit codes are unchanged). On non-unix targets stderr stays
/// discarded, like the unmodified transport.
#[cfg(unix)]
fn wrap_stdio_stderr(command: String, args: Vec<String>) -> (String, Vec<String>) {
    let mut wrapped = vec![
        "-c".to_string(),
        "exec \"$@\" 2>\"$PI_MCP_STDERR_PATH\"".to_string(),
        "sh".to_string(),
        command,
    ];
    wrapped.extend(args);
    ("/bin/sh".to_string(), wrapped)
}

#[cfg(not(unix))]
fn wrap_stdio_stderr(command: String, args: Vec<String>) -> (String, Vec<String>) {
    (command, args)
}

/// The home directory used by [`expand_home`]. Unix keeps `HOME` verbatim;
/// native Windows also accepts `USERPROFILE` (Git Bash sets both, plain
/// `cmd.exe` usually only the latter).
fn home_env() -> String {
    #[cfg(windows)]
    {
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME").unwrap_or_default()
    }
}

/// `expandHome` (`runtime.ts:92-98`): `~` and `~/…` name the home directory.
///
/// Unix uses `HOME`; native Windows falls back to `USERPROFILE` when `HOME` is
/// absent. The pure expansion semantics are otherwise identical.
fn expand_home(value: &str) -> String {
    let home = home_env();
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

/// The default shell for `!...` config values, mirroring Node's
/// `child_process.exec` default shell: `/bin/sh -c` on Unix (including
/// macOS) and `cmd.exe /C` on Windows. Other targets get `/bin/sh` as a
/// documented placeholder; they cannot spawn a shell but the call still
/// fails cleanly as a `None` result.
fn config_value_shell() -> (&'static str, &'static str) {
    #[cfg(windows)]
    {
        ("cmd.exe", "/C")
    }
    #[cfg(not(windows))]
    {
        ("/bin/sh", "-c")
    }
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
    let (shell, flag) = config_value_shell();
    let mut child = std::process::Command::new(shell)
        .arg(flag)
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

/// Windows uses `taskkill /F /T` (force, including descendants), the OS
/// equivalent of SIGKILL for a process group. No-op on targets without either
/// mechanism so the 10 s cap still reports a timeout rather than hanging.
#[cfg(windows)]
fn kill_process(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F", "/T"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(any(unix, windows)))]
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
    // The script result is the whole `CallToolResult` minus its top-level
    // `_meta` (`tools.ts:228-232`). Block-level `_meta`/`annotations` are part
    // of each decoded `ContentBlock` and survive serialization here.
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
/// `server` feeds the `read_mcp_resource` suffix of resource links; that
/// tool is registered once a connected server advertises resources, so the
/// hint is always available (`tools.ts:178-188`).
fn block_to_content(server: &str, block: &ContentBlock) -> Vec<Json> {
    match block {
        ContentBlock::Text { text, .. } => vec![json!({ "type": "text", "text": text })],
        ContentBlock::Image {
            data, mime_type, ..
        } => {
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
            ..
        } => {
            // `blockToContent`'s resource_link arm (`tools.ts:178-188`),
            // including the `read_mcp_resource` suffix when resources can be
            // read (upstream's `options.readableResources`).
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
            let read = format!(". Read it with {READ_MCP_RESOURCE_TOOL} (server \"{server}\")");
            let label = title.clone().unwrap_or_else(|| name.clone());
            vec![json!({
                "type": "text",
                "text": format!("[Resource {uri} \"{label}\"{details}{description}{read}]"),
            })]
        }
        ContentBlock::Resource { resource, .. } => {
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

/// The boolean annotation hints upstream forwards to a tool declaration
/// (`ANNOTATION_HINTS` / `toToolAnnotations`, `tools.ts:237-247`).
const ANNOTATION_HINTS: [&str; 4] = [
    "readOnlyHint",
    "destructiveHint",
    "idempotentHint",
    "openWorldHint",
];

/// `relocateSchema` (`tools.ts:111-129`): rewrite a subschema's local `$ref`s
/// so they keep resolving to the same nodes once nested under `pointer`.
fn relocate_schema(schema: &Json, pointer: &str) -> Json {
    match schema {
        Json::Array(items) => items
            .iter()
            .map(|item| relocate_schema(item, pointer))
            .collect(),
        Json::Object(map) => {
            let mut relocated = serde_json::Map::new();
            for (key, value) in map {
                let rewritten = match (key.as_str(), value) {
                    ("$ref", Json::String(reference))
                        if reference == "#" || reference.starts_with("#/") =>
                    {
                        json!(format!("{pointer}{}", &reference[1..]))
                    }
                    ("const" | "enum" | "default" | "examples", _) => value.clone(),
                    (
                        "properties" | "patternProperties" | "dependentSchemas" | "dependencies"
                        | "$defs" | "definitions",
                        Json::Object(names),
                    ) => Json::Object(
                        names
                            .iter()
                            .map(|(name, subschema)| {
                                (name.clone(), relocate_schema(subschema, pointer))
                            })
                            .collect(),
                    ),
                    _ => relocate_schema(value, pointer),
                };
                relocated.insert(key.clone(), rewritten);
            }
            Json::Object(relocated)
        }
        other => other.clone(),
    }
}

/// `createMcpResultSchema` (`tools.ts:135-150`): the declaration schema of the
/// `CallToolResult` scripts receive, with the server's own output schema under
/// `structuredContent` and its local references relocated.
fn create_mcp_result_schema(structured_content: Option<&Json>) -> Json {
    json!({
        "type": "object",
        "properties": {
            "content": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "type": { "enum": ["text", "image", "audio", "resource_link", "resource"] },
                        "text": { "type": "string" },
                        "data": { "type": "string" },
                        "mimeType": { "type": "string" }
                    },
                    "required": ["type"],
                    "additionalProperties": true
                }
            },
            "structuredContent": structured_content.map_or_else(
                || json!({}),
                |schema| relocate_schema(schema, "#/properties/structuredContent"),
            ),
            "isError": { "type": "boolean" }
        },
        "required": ["content"],
        "additionalProperties": true
    })
}

/// The published declaration of one `tools/list` tool, adding the fields
/// upstream's `createMcpToolDefinition` computes (`tools.ts:261-274`): the
/// `CallToolResult`-shaped `outputSchema`, the boolean annotation hints, and
/// the `title` fallback from `annotations.title`.
#[must_use]
fn mcp_tool_definition_json(tool: &McpTool) -> Json {
    let mut value = serde_json::to_value(tool).unwrap_or(Json::Null);
    value["outputSchema"] = create_mcp_result_schema(tool.output_schema.as_ref());
    let mut hints = serde_json::Map::new();
    if let Some(annotations) = tool.annotations.as_ref().and_then(Json::as_object) {
        for hint in ANNOTATION_HINTS {
            if let Some(flag) = annotations.get(hint).and_then(Json::as_bool) {
                hints.insert(hint.to_string(), json!(flag));
            }
        }
        if tool.title.is_none()
            && let Some(title) = annotations.get("title").and_then(Json::as_str)
        {
            value["title"] = json!(title);
        }
    }
    if let Some(object) = value.as_object_mut() {
        if hints.is_empty() {
            object.remove("annotations");
        } else {
            object.insert("annotations".to_string(), Json::Object(hints));
        }
    }
    value
}

/// Exposure of one tool of a server (`getMcpToolExposure`,
/// `core/mcp-servers.ts:169-178`): an exact `toolExposure` name wins, then
/// the first `*`-pattern in insertion order, then the server's `exposure`.
///
/// This is the faithful counterpart for re-registered tools from
/// [`ToolChange`]: pattern matching delegates to
/// [`pi_mcp::types::tool_name_pattern_matches`], the port of upstream's
/// escaped-regex `toolPatternRegExp` (`core/mcp-servers.ts:17-34`) rather
/// than the in-order literal scan used by the original registration path.
#[must_use]
pub fn tool_exposure(config: &Json, tool_name: &str) -> String {
    if let Some(overrides) = config.get("toolExposure").and_then(Json::as_object) {
        if let Some(exposure) = overrides.get(tool_name).and_then(Json::as_str) {
            return exposure.to_string();
        }
        for (pattern, exposure) in overrides {
            if pattern.contains('*')
                && pi_mcp::types::tool_name_pattern_matches(pattern, tool_name)
                && let Some(exposure) = exposure.as_str()
            {
                return exposure.to_string();
            }
        }
    }
    config
        .get("exposure")
        .and_then(Json::as_str)
        .unwrap_or("codemode")
        .to_string()
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

/// A synthetic server that sleeps before its handshake, so a connection can
/// settle after the startup bound has elapsed (`index.ts:594-608`).
#[cfg(test)]
pub(crate) const SLOW_SERVER_SCRIPT: &str = r#"#!/bin/sh
sleep 1
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"slow","version":"1.0.0"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"late","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
  esac
done
"#;

/// A synthetic server that emits `list_changed` after a delay, so the
/// notification lands between two host ticks (`runtime.ts:382-384`).
#[cfg(test)]
pub(crate) const LATE_NOTIFICATION_SERVER_SCRIPT: &str = r#"#!/bin/sh
lists=0
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"later","version":"1.0.0"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      lists=$((lists + 1))
      if [ "$lists" -eq 1 ]; then
        printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"old","inputSchema":{"type":"object"}}]}}\n' "$id"
        ( sleep 0.3; printf '{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}\n' ) &
      else
        printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"new","inputSchema":{"type":"object"}}]}}\n' "$id"
      fi ;;
  esac
done
"#;

/// Writes the slow fixture server to `directory` as an executable script.
#[cfg(test)]
pub(crate) fn write_late_server(
    directory: &std::path::Path,
) -> std::io::Result<std::path::PathBuf> {
    let path = directory.join("slow-server.sh");
    std::fs::write(&path, SLOW_SERVER_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(path)
}

/// Writes the delayed-notification fixture server to `directory`.
#[cfg(test)]
pub(crate) fn write_late_notification_server(
    directory: &std::path::Path,
) -> std::io::Result<std::path::PathBuf> {
    let path = directory.join("later-server.sh");
    std::fs::write(&path, LATE_NOTIFICATION_SERVER_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(path)
}

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
        let home = home_env();
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
            "[Resource file:///a.txt \"a\" (text/plain, 30B). Read it with read_mcp_resource (server \"synth\")]"
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

    #[test]
    fn http_auth_provider_attaches_only_to_http_servers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let http = McpServerConfig {
            name: "web".to_string(),
            transport: McpTransportConfig::Http {
                url: "https://example.test/mcp".to_string(),
                headers: std::collections::HashMap::new(),
            },
        };
        let provider = http_auth_provider("web", &http, dir.path());
        assert!(provider.is_some(), "HTTP servers get an OAuth provider");

        // A stdio server has no URL to hold credentials for.
        let stdio = McpServerConfig {
            name: "local".to_string(),
            transport: McpTransportConfig::Stdio {
                command: "x".to_string(),
                args: Vec::new(),
                env: std::collections::HashMap::new(),
                working_dir: None,
            },
        };
        assert!(http_auth_provider("local", &stdio, dir.path()).is_none());
    }

    /// Writes an executable synthetic server script into `dir`.
    fn write_script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod script");
        }
        path
    }

    /// Answers the first `tools/list` with `old` and a notification, then
    /// the second with `new` (a synthetic stand-in for a live server that
    /// changed its tools, `runtime.ts:382-384`).
    const CHANGING_SERVER_SCRIPT: &str = r#"#!/bin/sh
lists=0
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"changing","version":"1.0.0"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      lists=$((lists + 1))
      if [ "$lists" -eq 1 ]; then
        printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"old","inputSchema":{"type":"object"}}]}}\n' "$id"
        printf '{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}\n'
      else
        printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"new","inputSchema":{"type":"object"}}]}}\n' "$id"
      fi ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"ok"}]}}\n' "$id" ;;
  esac
done
"#;

    /// As above, but the refresh fails with an RPC error so the worker
    /// records upstream's `Failed to refresh tools: …` text.
    const FAILING_REFRESH_SCRIPT: &str = r#"#!/bin/sh
lists=0
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"flaky","version":"1.0.0"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      lists=$((lists + 1))
      if [ "$lists" -eq 1 ]; then
        printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"old","inputSchema":{"type":"object"}}]}}\n' "$id"
        printf '{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}\n'
      else
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32603,"message":"refresh boom"}}\n' "$id"
      fi ;;
  esac
done
"#;

    fn wait_for_tool_changes(manager: &McpManager) -> Vec<ToolChange> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut changes = manager.take_tool_changes();
        while changes.is_empty() && Instant::now() < deadline {
            manager.poll_notifications();
            changes = manager.take_tool_changes();
            if changes.is_empty() {
                // Let the manager worker thread run instead of busy-yielding,
                // which can starve it under a parallel test load.
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        changes
    }

    #[test]
    fn connect_failure_appends_the_stderr_tail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = write_script(
            dir.path(),
            "failing.sh",
            "#!/bin/sh\nprintf 'boom-on-stderr\\n' >&2\nexit 1\n",
        );
        let config = json!({"command": script.to_string_lossy(), "timeout": 2});
        let manager = McpManager::default();
        let error = manager
            .connect("bad", &config, dir.path().to_str().expect("cwd"))
            .expect_err("the server never answers initialize");
        assert!(error.contains("\nboom-on-stderr"), "{error}");
    }

    #[test]
    fn stderr_tail_keeps_only_the_last_2000_characters() {
        let long = "x".repeat(STDERR_TAIL_CHARS + 500);
        let tail = slice_tail_chars(long.trim(), STDERR_TAIL_CHARS);
        assert_eq!(tail.len(), STDERR_TAIL_CHARS);
        assert_eq!(tail, "x".repeat(STDERR_TAIL_CHARS));
        assert_eq!(slice_tail_chars("short", STDERR_TAIL_CHARS), "short");
    }

    #[test]
    fn list_changed_notification_re_registers_the_new_tools() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = write_script(dir.path(), "changing.sh", CHANGING_SERVER_SCRIPT);
        let config = json!({"command": script.to_string_lossy(), "timeout": 5});
        let manager = McpManager::default();
        let initial = manager
            .connect("changing", &config, dir.path().to_str().expect("cwd"))
            .expect("connect");
        assert_eq!(initial.tools.len(), 1);
        assert_eq!(initial.tools[0]["name"], "old");

        let changes = wait_for_tool_changes(&manager);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].server, "changing");
        assert!(changes[0].error.is_none(), "{changes:?}");
        assert_eq!(changes[0].tools.len(), 1);
        assert_eq!(changes[0].tools[0]["name"], "new");
        manager.shutdown_all();
    }

    #[test]
    fn failed_tool_refresh_records_the_upstream_error_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = write_script(dir.path(), "flaky.sh", FAILING_REFRESH_SCRIPT);
        let config = json!({"command": script.to_string_lossy(), "timeout": 5});
        let manager = McpManager::default();
        manager
            .connect("flaky", &config, dir.path().to_str().expect("cwd"))
            .expect("connect");

        let changes = wait_for_tool_changes(&manager);
        assert_eq!(changes.len(), 1, "{changes:?}");
        let error = changes[0].error.as_deref().expect("refresh error");
        assert!(error.starts_with("Failed to refresh tools: "), "{error}");
        assert_eq!(changes[0].tools, Vec::<Json>::new());
        manager.shutdown_all();
    }

    #[test]
    fn startup_wait_is_bounded_and_one_shot() {
        let now = Instant::now();
        let settled = Arc::new(Mutex::new(Vec::new()));
        let mut wait = StartupWait::new_with_start(100, now);
        wait.pending.push(PendingConnect {
            name: "slow".to_string(),
            settled: settled.clone(),
        });
        assert_eq!(
            wait.tick(now),
            StartupTick::Waiting {
                remaining: Duration::from_millis(100)
            }
        );
        assert_eq!(wait.pending(), 1);
        assert_eq!(
            wait.tick(now + Duration::from_millis(99)),
            StartupTick::Waiting {
                remaining: Duration::from_millis(1)
            }
        );
        assert_eq!(
            wait.tick(now + Duration::from_millis(100)),
            StartupTick::TimedOut
        );
        assert!(wait.waited());
        assert_eq!(
            wait.tick(now + Duration::from_millis(500)),
            StartupTick::Ready
        );
        // The bound is one-shot, but the outcome is not owned by the wait:
        // settling late leaves it in the manager's queue for the host.
        settled.lock().expect("settled lock").push((
            "slow".to_string(),
            Ok(ConnectOutcome {
                tools: Vec::new(),
                instructions: None,
                has_resources: false,
            }),
        ));
        drop(wait);
        assert_eq!(settled.lock().expect("settled lock").len(), 1);
    }

    #[test]
    fn late_connect_is_delivered_after_the_startup_bound_elapsed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = write_late_server(dir.path()).expect("fixture");
        let config = json!({"command": script.to_string_lossy(), "timeout": 5});
        let manager = McpManager::default();
        let mut wait = StartupWait::new(20);
        wait.add("slow", &config, dir.path().to_str().expect("cwd"), &manager);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match wait.tick(Instant::now()) {
                StartupTick::TimedOut => break,
                StartupTick::Waiting { .. } => {
                    assert!(Instant::now() < deadline, "startup wait never elapsed");
                    std::thread::sleep(Duration::from_millis(2));
                }
                StartupTick::Ready => panic!("the server is still sleeping"),
            }
        }
        assert!(wait.waited());
        assert_eq!(wait.pending(), 1);
        // The host drops the one-shot wait; the manager still owns the
        // in-flight connection and its eventual outcome.
        drop(wait);
        let outcome = loop {
            if let Some((name, outcome)) = manager.take_settled_connects().into_iter().next() {
                assert_eq!(name, "slow");
                break outcome;
            }
            assert!(Instant::now() < deadline, "late connect was lost");
            std::thread::sleep(Duration::from_millis(5));
        };
        let outcome = outcome.expect("late connect succeeded");
        assert_eq!(outcome.tools.len(), 1);
        assert_eq!(outcome.tools[0]["name"], "late");
        manager.shutdown_all();
    }

    #[test]
    fn notification_arriving_between_ticks_is_drained_without_a_barrier() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = write_late_notification_server(dir.path()).expect("fixture");
        let config = json!({"command": script.to_string_lossy(), "timeout": 5});
        let manager = McpManager::default();
        let initial = manager
            .connect("later", &config, dir.path().to_str().expect("cwd"))
            .expect("connect");
        assert_eq!(initial.tools[0]["name"], "old");

        // Before the notification arrives a drain publishes nothing.
        manager.try_drain_notifications();
        assert_eq!(manager.take_tool_changes().len(), 0);
        // Let the notification land between two host ticks.
        std::thread::sleep(Duration::from_millis(500));
        let deadline = Instant::now() + Duration::from_secs(5);
        let changes = loop {
            manager.try_drain_notifications();
            let changes = manager.take_tool_changes();
            if !changes.is_empty() {
                break changes;
            }
            assert!(Instant::now() < deadline, "notification was never drained");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].server, "later");
        assert!(changes[0].error.is_none(), "{changes:?}");
        assert_eq!(changes[0].tools[0]["name"], "new");
        manager.shutdown_all();
    }

    #[test]
    fn startup_wait_settles_against_the_fixture_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = write_fixture_server(dir.path()).expect("fixture");
        let config = json!({"command": script.to_string_lossy(), "timeout": 5});
        let manager = McpManager::default();
        let mut wait = StartupWait::new(5_000);
        wait.add(
            "synth",
            &config,
            dir.path().to_str().expect("cwd"),
            &manager,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match wait.tick(Instant::now()) {
                StartupTick::Ready | StartupTick::TimedOut => break,
                StartupTick::Waiting { .. } => {
                    assert!(Instant::now() < deadline, "startup wait did not settle");
                    std::thread::yield_now();
                }
            }
        }
        let settled = wait.drain_settled();
        assert_eq!(settled.len(), 1, "{settled:?}");
        assert_eq!(settled[0].0, "synth");
        assert!(settled[0].1.is_ok(), "{:?}", settled[0].1);
        manager.shutdown_all();
    }

    #[test]
    fn converted_results_keep_block_meta_and_drop_top_level_meta() {
        let result: CallToolResult = serde_json::from_value(json!({
            "_meta": {"secret": true},
            "content": [{
                "type": "text", "text": "hello",
                "_meta": {"trace": 1}, "annotations": {"priority": 0.5}
            }],
            "structuredContent": {"ok": true},
        }))
        .expect("decode");
        let mapped = convert_mcp_result("synth", "echo", &result);
        let script = &mapped["structuredContent"];
        assert_eq!(script["content"][0]["text"], "hello");
        assert_eq!(script["content"][0]["_meta"], json!({"trace": 1}));
        assert_eq!(
            script["content"][0]["annotations"],
            json!({"priority": 0.5})
        );
        assert!(script.get("_meta").is_none(), "top-level _meta dropped");
        assert_eq!(script["structuredContent"], json!({"ok": true}));
    }

    #[test]
    fn published_tool_definitions_carry_output_schema_and_boolean_hints() {
        let tool: McpTool = serde_json::from_value(json!({
            "name": "search",
            "description": "Search docs",
            "annotations": {
                "title": "Search title",
                "readOnlyHint": true,
                "destructiveHint": false,
                "priority": 0.9
            },
            "inputSchema": {"type": "object"},
            "outputSchema": {
                "type": "object",
                "properties": {"item": {"$ref": "#/$defs/Item"}},
                "$defs": {"Item": {"type": "string"}}
            }
        }))
        .expect("tool");
        let definition = mcp_tool_definition_json(&tool);
        // Upstream `createMcpResultSchema` (`tools.ts:135-150`).
        assert_eq!(definition["outputSchema"]["type"], json!("object"));
        assert_eq!(definition["outputSchema"]["required"], json!(["content"]));
        assert_eq!(
            definition["outputSchema"]["additionalProperties"],
            json!(true)
        );
        assert_eq!(
            definition["outputSchema"]["properties"]["structuredContent"]["properties"]["item"]["$ref"],
            json!("#/properties/structuredContent/$defs/Item")
        );
        // Only boolean hints survive (`tools.ts:237-247`).
        assert_eq!(
            definition["annotations"],
            json!({"readOnlyHint": true, "destructiveHint": false})
        );
        // `title` falls back to `annotations.title` (`tools.ts:261`).
        assert_eq!(definition["title"], json!("Search title"));
    }

    #[test]
    fn tool_exposure_uses_the_faithful_glob_matcher() {
        let config = json!({
            "exposure": "codemode",
            "toolExposure": {
                "read.*": "hidden",
                "write*": "direct",
                "exact": "deferred"
            }
        });
        assert_eq!(tool_exposure(&config, "exact"), "deferred");
        assert_eq!(tool_exposure(&config, "read.foo"), "hidden");
        // Upstream escapes the `.`, so it is not a regex wildcard.
        assert_eq!(tool_exposure(&config, "readXfoo"), "codemode");
        assert_eq!(tool_exposure(&config, "writeThing"), "direct");
        assert_eq!(tool_exposure(&config, "other"), "codemode");
    }

    #[test]
    fn plain_call_tool_regression_still_works() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = write_fixture_server(dir.path()).expect("fixture");
        let config = json!({"command": script.to_string_lossy(), "timeout": 5});
        let manager = McpManager::default();
        manager
            .connect("synth", &config, dir.path().to_str().expect("cwd"))
            .expect("connect");
        let result = manager.call_tool("synth", "echo", json!({"text": "hi"}));
        assert_eq!(result["result"]["content"][0]["text"], "echo-ok");
        manager.shutdown_all();
    }
}
