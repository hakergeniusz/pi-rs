//! MCP client: one connected server, after the `initialize` handshake
//! (upstream `client.ts:204-250`, over either transport).
//!
//! Server-initiated messages land on the transport's `inbound` channel — the
//! port's stand-in for upstream's `client.onNotification` listeners
//! (`extensions/mcp/runtime.ts:369`, `:382-384`). [`McpClient::try_recv_inbound`]
//! lets the host drain them without a background task; the host re-lists
//! tools on `notifications/tools/list_changed`.
//! [`McpClient::drain_server_notifications`] is the typed form, forwarding
//! log messages, progress and cancellation (`client.ts:525-530`,
//! `tools.ts:281-286`, `log.ts:43-60`). Server-initiated `ping` and
//! `roots/list` requests get replies through [`McpClient::respond_to_request`]
//! (`client.ts:176-182`).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::json;

use crate::auth_provider::AuthProvider;
use crate::error::McpError;
use crate::http::{StreamableHttpOptions, StreamableHttpTransport};
use crate::resources::{
    ListResourceTemplatesResult, ListResourcesResult, ReadResourceResult, Resource,
    ResourceTemplate,
};
use crate::transport::{SpawnOptions, StdioTransport};
use crate::types::{
    CallToolParams, CallToolResult, ClientCapabilities, Implementation, InboundMessage,
    InitializeParams, InitializeResult, JsonRpcError, JsonRpcId, JsonRpcRequest, JsonRpcResponse,
    JsonRpcVersion, Root, RootsCapability, ServerNotification, ToolsListResult,
    json_rpc_error_codes, parse_server_notification,
};

/// How to reach one MCP server (`mcp-servers.json` entry shape): a stdio
/// subprocess or a streamable HTTP endpoint. The config values are already
/// resolved by the caller (`createDefaultTransport`, upstream
/// `runtime.ts:103-124`).
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransportConfig,
}

#[derive(Debug, Clone, PartialEq)]
pub enum McpTransportConfig {
    /// Launch a subprocess and speak newline-delimited JSON-RPC on its
    /// stdio (`runtime.ts:114-123`).
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        working_dir: Option<std::path::PathBuf>,
    },
    /// POST JSON-RPC to a streamable HTTP URL (`runtime.ts:107-112`).
    Http {
        url: String,
        headers: HashMap<String, String>,
    },
}

/// Default request timeout for protocol calls.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// One live connection, over either transport. The stdio arm correlates
/// replies on a pending map; the HTTP arm correlates on the POST itself.
enum ActiveTransport {
    Stdio(StdioTransport),
    Http(StreamableHttpTransport),
}

impl ActiveTransport {
    /// Builds the configured transport (`createDefaultTransport`,
    /// `runtime.ts:103-124`) with an optional HTTP credential provider. The
    /// provider is stored on the transport so every request (and the 401
    /// recovery) sees it.
    fn build_with_auth(
        config: &McpTransportConfig,
        auth: Option<Arc<dyn AuthProvider>>,
    ) -> Result<Self, McpError> {
        match config {
            McpTransportConfig::Stdio {
                command,
                args,
                env,
                working_dir,
            } => Ok(Self::Stdio(StdioTransport::spawn(&SpawnOptions {
                command: command.clone(),
                args: args.clone(),
                env: env.clone(),
                working_dir: working_dir.clone(),
            })?)),
            McpTransportConfig::Http { url, headers } => Ok(Self::Http(
                StreamableHttpTransport::new(StreamableHttpOptions {
                    url: url.clone(),
                    headers: headers.clone(),
                    auth,
                    ..StreamableHttpOptions::default()
                })?,
            )),
        }
    }

    /// The stdio transport is live from `spawn`; the HTTP transport starts
    /// explicitly (`transport.start()`, upstream `client.ts:216`).
    fn start(&self) -> Result<(), McpError> {
        match self {
            Self::Stdio(_) => Ok(()),
            Self::Http(transport) => transport.start(),
        }
    }

    fn set_protocol_version(&self, version: &str) {
        match self {
            Self::Stdio(_) => {}
            Self::Http(transport) => transport.set_protocol_version(version),
        }
    }

    async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
        timeout: Duration,
    ) -> Result<serde_json::Value, McpError> {
        match self {
            Self::Stdio(transport) => transport.request(method, params, timeout).await,
            Self::Http(transport) => transport.request(method, params, timeout).await,
        }
    }

    async fn notify(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), McpError> {
        match self {
            Self::Stdio(transport) => transport.notify(method, params).await,
            Self::Http(transport) => transport.notify(method, params).await,
        }
    }

    /// Replies to a server-initiated request (`roots/list`, `ping`).
    async fn respond(&self, response: JsonRpcResponse) -> Result<(), McpError> {
        match self {
            Self::Stdio(transport) => transport.respond(response).await,
            Self::Http(transport) => transport.respond(response).await,
        }
    }

    async fn shutdown(self) -> Result<(), McpError> {
        match self {
            Self::Stdio(transport) => transport.shutdown().await,
            Self::Http(transport) => transport.shutdown().await,
        }
    }
}

/// A connected, initialized MCP server.
pub struct McpClient {
    transport: ActiveTransport,
    pub server_info: Implementation,
    pub capabilities: crate::types::ServerCapabilities,
    /// Server instructions from `initialize` (`InitializeResult.instructions`).
    pub instructions: Option<String>,
    /// The roots served to the server through `roots/list`
    /// (`runtime.ts:363`, `client.ts:177-181`).
    pub roots: Vec<Root>,
    /// Source of `_meta.progressToken` values for tool calls
    /// (`client.ts:404-410`).
    progress_tokens: AtomicU64,
}

impl McpClient {
    /// Spawns the server and performs the `initialize` handshake
    /// (`initialize` request, then the `notifications/initialized` ack)
    /// with the default request timeout.
    ///
    /// # Errors
    /// Spawn failures, handshake errors, or a protocol version the server
    /// refuses.
    pub async fn connect(config: &McpServerConfig) -> Result<Self, McpError> {
        Self::connect_with_timeout(config, DEFAULT_TIMEOUT).await
    }

    /// [`McpClient::connect`] with an explicit handshake timeout (the
    /// connection's `requestTimeoutMs` upstream: pi passes
    /// `(config.timeout ?? 60) * 1000`, `runtime.ts:212-214`).
    ///
    /// # Errors
    /// Spawn failures, handshake errors, or a protocol version the server
    /// refuses.
    pub async fn connect_with_timeout(
        config: &McpServerConfig,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        Self::connect_with_auth(config, None, timeout).await
    }

    /// [`McpClient::connect_with_timeout`] with an HTTP credential provider
    /// (`createDefaultTransport`'s `authProvider`, `runtime.ts:100-124`). A
    /// `None` provider keeps the unauthenticated transport behavior.
    ///
    /// # Errors
    /// Spawn failures, handshake errors, or a protocol version the server
    /// refuses.
    pub async fn connect_with_auth(
        config: &McpServerConfig,
        auth: Option<Arc<dyn AuthProvider>>,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        Self::connect_with_auth_and_roots(config, auth, Vec::new(), timeout).await
    }

    /// [`McpClient::connect_with_auth`] with the roots the host serves
    /// through `roots/list` (upstream's `client.ts:177-181`, `:217-218`,
    /// fed the session cwd at `runtime.ts:363`).
    ///
    /// # Errors
    /// Spawn failures, handshake errors, or a protocol version the server
    /// refuses.
    pub async fn connect_with_auth_and_roots(
        config: &McpServerConfig,
        auth: Option<Arc<dyn AuthProvider>>,
        roots: Vec<Root>,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        let transport = ActiveTransport::build_with_auth(&config.transport, auth)?;
        match handshake(&transport, timeout, &roots).await {
            Ok(initialized) => {
                // The negotiated version rides `MCP-Protocol-Version` on
                // HTTP servers (`client.ts:242`, `streamable-http.ts:281`).
                transport.set_protocol_version(&initialized.protocol_version);
                transport.notify("notifications/initialized", None).await?;
                Ok(Self {
                    server_info: initialized.server_info,
                    capabilities: initialized.capabilities,
                    instructions: initialized.instructions,
                    transport,
                    roots,
                    progress_tokens: AtomicU64::new(1),
                })
            }
            Err(error) => {
                // A failed handshake closes the transport
                // (`client.ts:246-249`).
                let _ = transport.shutdown().await;
                Err(error)
            }
        }
    }

    /// Lists the tools the server exposes.
    ///
    /// # Errors
    /// Transport or RPC failures.
    pub async fn tools_list(&self) -> Result<ToolsListResult, McpError> {
        self.tools_list_with_timeout(DEFAULT_TIMEOUT).await
    }

    /// [`McpClient::tools_list`] with an explicit request timeout.
    ///
    /// # Errors
    /// Transport or RPC failures.
    pub async fn tools_list_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<ToolsListResult, McpError> {
        let result = self.transport.request("tools/list", None, timeout).await?;
        serde_json::from_value(result)
            .map_err(|error| McpError::Malformed(format!("bad tools/list result: {error}")))
    }

    /// Calls one tool.
    ///
    /// # Errors
    /// Transport failures; protocol-level tool errors come back as
    /// [`CallToolResult::is_error`], not [`McpError`].
    pub async fn tools_call(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
        timeout: Duration,
    ) -> Result<CallToolResult, McpError> {
        let params = CallToolParams {
            name: name.to_string(),
            arguments,
            meta: None,
        };
        let result = self
            .transport
            .request(
                "tools/call",
                Some(serde_json::to_value(params).map_err(|error| serde_error(&error))?),
                timeout,
            )
            .await?;
        serde_json::from_value(result)
            .map_err(|error| McpError::Malformed(format!("bad tools/call result: {error}")))
    }

    /// Allocates a token for one call's `notifications/progress` stream
    /// (`client.ts:404-410` uses the request id; a monotonic client token is
    /// equally unique on the wire and survives request-id rewrites by HTTP
    /// transports).
    #[must_use]
    pub fn allocate_progress_token(&self) -> JsonRpcId {
        JsonRpcId::Number(
            i64::try_from(self.progress_tokens.fetch_add(1, Ordering::Relaxed)).unwrap_or(i64::MAX),
        )
    }

    /// Calls one tool asking the server for progress notifications via
    /// `_meta.progressToken` (`client.ts:404-410`, `tools.ts:280-286`). The
    /// notifications themselves are delivered by
    /// [`Self::drain_server_notifications`].
    ///
    /// # Errors
    /// Transport failures; protocol-level tool errors come back as
    /// [`CallToolResult::is_error`].
    pub async fn tools_call_with_progress(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
        progress_token: &JsonRpcId,
        timeout: Duration,
    ) -> Result<CallToolResult, McpError> {
        // `_meta` is a sibling of `arguments` on the request params, as
        // upstream builds it (`client.ts:407-409`).
        let params = CallToolParams {
            name: name.to_string(),
            arguments,
            meta: Some(json!({ "progressToken": progress_token })),
        };
        let result = self
            .transport
            .request(
                "tools/call",
                Some(serde_json::to_value(params).map_err(|error| serde_error(&error))?),
                timeout,
            )
            .await?;
        serde_json::from_value(result)
            .map_err(|error| McpError::Malformed(format!("bad tools/call result: {error}")))
    }

    /// One `resources/list` page (`client.ts:306-309`).
    ///
    /// # Errors
    /// Transport failures or a malformed page.
    pub async fn resources_list_page(
        &self,
        cursor: Option<&str>,
        timeout: Duration,
    ) -> Result<ListResourcesResult, McpError> {
        let params = cursor.map(|cursor| json!({ "cursor": cursor }));
        let value = self
            .transport
            .request("resources/list", params, timeout)
            .await?;
        crate::resources::parse_list_resources_page(&value)
    }

    /// Every resource, following `nextCursor` (`client.ts:301-303`, `:357-376`).
    ///
    /// # Errors
    /// Transport failures, malformed pages, duplicate cursors, or a page
    /// loop past [`crate::resources::MAX_LIST_PAGES`].
    pub async fn resources_list(&self, timeout: Duration) -> Result<Vec<Resource>, McpError> {
        let mut items = Vec::new();
        let mut cursors = std::collections::HashSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..crate::resources::MAX_LIST_PAGES {
            let page = self.resources_list_page(cursor.as_deref(), timeout).await?;
            items.extend(page.resources);
            let Some(next) = page.next_cursor else {
                return Ok(items);
            };
            if !cursors.insert(next.clone()) {
                return Err(McpError::Malformed(format!(
                    "MCP resources/list returned duplicate cursor: {next}"
                )));
            }
            cursor = Some(next);
        }
        Err(McpError::Malformed(format!(
            "MCP resources/list exceeded {} pages",
            crate::resources::MAX_LIST_PAGES
        )))
    }

    /// One `resources/templates/list` page. A server that does not implement
    /// the method answers as empty (`withoutTemplates`, `runtime.ts:126-138`).
    ///
    /// # Errors
    /// Transport failures other than "method not found", or a malformed page.
    pub async fn resources_templates_list_page(
        &self,
        cursor: Option<&str>,
        timeout: Duration,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let params = cursor.map(|cursor| json!({ "cursor": cursor }));
        let value = match self
            .transport
            .request("resources/templates/list", params, timeout)
            .await
        {
            Ok(value) => value,
            Err(McpError::Rpc { code, .. })
                if code == crate::types::json_rpc_error_codes::METHOD_NOT_FOUND =>
            {
                return Ok(ListResourceTemplatesResult {
                    resource_templates: Vec::new(),
                    next_cursor: None,
                    meta: None,
                });
            }
            Err(error) => return Err(error),
        };
        crate::resources::parse_list_resource_templates_page(&value)
    }

    /// Every resource template, following `nextCursor`.
    ///
    /// # Errors
    /// As [`Self::resources_templates_list_page`], plus cursor loop errors.
    pub async fn resources_templates_list(
        &self,
        timeout: Duration,
    ) -> Result<Vec<ResourceTemplate>, McpError> {
        let mut items = Vec::new();
        let mut cursors = std::collections::HashSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..crate::resources::MAX_LIST_PAGES {
            let page = self
                .resources_templates_list_page(cursor.as_deref(), timeout)
                .await?;
            items.extend(page.resource_templates);
            let Some(next) = page.next_cursor else {
                return Ok(items);
            };
            if !cursors.insert(next.clone()) {
                return Err(McpError::Malformed(format!(
                    "MCP resources/templates/list returned duplicate cursor: {next}"
                )));
            }
            cursor = Some(next);
        }
        Err(McpError::Malformed(format!(
            "MCP resources/templates/list exceeded {} pages",
            crate::resources::MAX_LIST_PAGES
        )))
    }

    /// `resources/read` (`client.ts:337-339`).
    ///
    /// # Errors
    /// Transport failures or a malformed result.
    pub async fn resource_read(
        &self,
        uri: &str,
        timeout: Duration,
    ) -> Result<ReadResourceResult, McpError> {
        let value = self
            .transport
            .request("resources/read", Some(json!({ "uri": uri })), timeout)
            .await?;
        crate::resources::parse_read_resource_result(&value)
    }

    /// Drains one server-initiated message, if any, without waiting
    /// (upstream's notification listeners, `runtime.ts:369`, `:382-384`).
    /// The host polls this on its event loop; there is no internal timer.
    pub fn try_recv_inbound(&self) -> Option<InboundMessage> {
        // `Receiver::try_recv` takes `&mut`, so the guarded receivers are
        // locked for the non-async drain.
        match &self.transport {
            ActiveTransport::Stdio(transport) => transport
                .inbound
                .lock()
                .expect("inbound mutex poisoned")
                .try_recv()
                .ok(),
            ActiveTransport::Http(transport) => transport
                .inbound
                .lock()
                .expect("inbound mutex poisoned")
                .try_recv()
                .ok(),
        }
    }

    /// Drains every queued server-initiated notification as a typed value
    /// (`client.ts:525-530` and the notification listeners of
    /// `runtime.ts:369`, `:382-384`). Non-blocking like
    /// [`Self::try_recv_inbound`]; malformed lines are skipped.
    #[must_use]
    pub fn drain_server_notifications(&self) -> Vec<ServerNotification> {
        let mut output = Vec::new();
        while let Some(message) = self.try_recv_inbound() {
            if let InboundMessage::Notification(notification) = message {
                output.push(parse_server_notification(&notification));
            }
        }
        output
    }

    /// Sends `notifications/cancelled` for one of our in-flight requests
    /// (`client.ts:562-572`, `cancelPending`). Reason is optional, as in the
    /// spec (`protocol/types.ts:59-62`).
    ///
    /// # Errors
    /// Transport failures.
    pub async fn notify_cancelled(
        &self,
        request_id: &JsonRpcId,
        reason: Option<&str>,
    ) -> Result<(), McpError> {
        let mut params = json!({ "requestId": request_id });
        if let Some(reason) = reason {
            params["reason"] = json!(reason);
        }
        self.transport
            .notify("notifications/cancelled", Some(params))
            .await
    }

    /// Replies to a server-initiated request. `ping` always answers `{}`;
    /// `roots/list` answers with the roots this client was connected with;
    /// anything else gets `-32601` (`client.ts:176-182`, `:510-520`).
    ///
    /// # Errors
    /// Transport failures.
    pub async fn respond_to_request(&self, request: &JsonRpcRequest) -> Result<(), McpError> {
        let result = match request.method.as_str() {
            "ping" => json!({}),
            "roots/list" => json!({ "roots": self.roots }),
            _ => {
                return self
                    .transport
                    .respond(JsonRpcResponse {
                        jsonrpc: JsonRpcVersion,
                        id: request.id.clone(),
                        result: None,
                        error: Some(JsonRpcError {
                            code: json_rpc_error_codes::METHOD_NOT_FOUND,
                            message: format!("Method not found: {}", request.method),
                            data: None,
                        }),
                    })
                    .await;
            }
        };
        self.transport
            .respond(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: request.id.clone(),
                result: Some(result),
                error: None,
            })
            .await
    }

    /// Protocol liveness probe.
    ///
    /// # Errors
    /// Transport failures.
    pub async fn ping(&self) -> Result<(), McpError> {
        self.transport
            .request("ping", Some(json!({})), DEFAULT_TIMEOUT)
            .await
            .map(|_| ())
    }

    /// Kills a stdio server process, or closes an HTTP session
    /// (`transport.close()`, upstream `client.ts:388-394`).
    ///
    /// # Errors
    /// Propagates transport-task join failures.
    pub async fn shutdown(self) -> Result<(), McpError> {
        self.transport.shutdown().await
    }
}

/// The `initialize` exchange through either transport (`client.ts:216-243`).
///
/// # Errors
/// Transport failures, a bad result shape, or a protocol version outside
/// [`crate::SUPPORTED_PROTOCOL_VERSIONS`].
async fn handshake(
    transport: &ActiveTransport,
    timeout: Duration,
    roots: &[Root],
) -> Result<InitializeResult, McpError> {
    transport.start()?;
    // `client.ts:217-218`: supplying roots declares the `roots` capability
    // even when the caller passed an empty capability object.
    let capabilities = ClientCapabilities {
        roots: (!roots.is_empty()).then(RootsCapability::default),
    };
    let params = InitializeParams {
        protocol_version: crate::PROTOCOL_VERSION.to_string(),
        capabilities,
        client_info: Implementation {
            name: crate::CLIENT_NAME.to_string(),
            version: Some(crate::CLIENT_VERSION.to_string()),
        },
    };
    let result = transport
        .request(
            "initialize",
            Some(serde_json::to_value(params).map_err(|error| serde_error(&error))?),
            timeout,
        )
        .await?;
    let initialized: InitializeResult = serde_json::from_value(result)
        .map_err(|error| McpError::Handshake(format!("bad initialize result: {error}")))?;
    // Servers that do not support the requested version answer with their
    // own latest one, so older versions stay accepted for servers built on
    // older SDKs (`SUPPORTED_PROTOCOL_VERSIONS`, upstream `client.ts:235-237`).
    if !crate::SUPPORTED_PROTOCOL_VERSIONS.contains(&initialized.protocol_version.as_str()) {
        return Err(McpError::Handshake(format!(
            "MCP server selected unsupported protocol version {}",
            initialized.protocol_version
        )));
    }
    Ok(initialized)
}

fn serde_error(error: &serde_json::Error) -> McpError {
    McpError::Malformed(format!("request serialization failed: {error}"))
}
