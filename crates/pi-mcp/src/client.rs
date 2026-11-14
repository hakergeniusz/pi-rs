//! MCP client: one connected server, after the `initialize` handshake.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;

use crate::error::McpError;
use crate::transport::{SpawnOptions, StdioTransport};
use crate::types::{
    CallToolParams, CallToolResult, ClientCapabilities, Implementation, InitializeParams,
    InitializeResult, ToolsListResult,
};

/// How to launch one MCP server (`mcp-servers.json` entry shape).
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub working_dir: Option<std::path::PathBuf>,
}

/// Default request timeout for protocol calls.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// A connected, initialized MCP server.
pub struct McpClient {
    transport: StdioTransport,
    pub server_info: Implementation,
    pub capabilities: crate::types::ServerCapabilities,
}

impl McpClient {
    /// Spawns the server and performs the `initialize` handshake
    /// (`initialize` request, then the `notifications/initialized` ack).
    ///
    /// # Errors
    /// Spawn failures, handshake errors, or a protocol version the server
    /// refuses.
    pub async fn connect(config: &McpServerConfig) -> Result<Self, McpError> {
        let transport = StdioTransport::spawn(&SpawnOptions {
            command: config.command.clone(),
            args: config.args.clone(),
            env: config.env.clone(),
            working_dir: config.working_dir.clone(),
        })?;

        let params = InitializeParams {
            protocol_version: crate::PROTOCOL_VERSION.to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: Implementation {
                name: crate::CLIENT_NAME.to_string(),
                version: Some(crate::CLIENT_VERSION.to_string()),
            },
        };
        let result = transport
            .request(
                "initialize",
                Some(serde_json::to_value(params).map_err(|error| serde_error(&error))?),
                DEFAULT_TIMEOUT,
            )
            .await?;
        let initialized: InitializeResult = serde_json::from_value(result)
            .map_err(|error| McpError::Handshake(format!("bad initialize result: {error}")))?;
        if initialized.protocol_version != crate::PROTOCOL_VERSION {
            return Err(McpError::Handshake(format!(
                "server negotiated protocol version {} but client speaks {}",
                initialized.protocol_version,
                crate::PROTOCOL_VERSION
            )));
        }
        transport.notify("notifications/initialized", None).await?;

        Ok(Self {
            server_info: initialized.server_info,
            capabilities: initialized.capabilities,
            transport,
        })
    }

    /// Lists the tools the server exposes.
    ///
    /// # Errors
    /// Transport or RPC failures.
    pub async fn tools_list(&self) -> Result<ToolsListResult, McpError> {
        let result = self
            .transport
            .request("tools/list", None, DEFAULT_TIMEOUT)
            .await?;
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
    ) -> Result<CallToolResult, McpError> {
        let params = CallToolParams {
            name: name.to_string(),
            arguments,
        };
        let result = self
            .transport
            .request(
                "tools/call",
                Some(serde_json::to_value(params).map_err(|error| serde_error(&error))?),
                DEFAULT_TIMEOUT,
            )
            .await?;
        serde_json::from_value(result)
            .map_err(|error| McpError::Malformed(format!("bad tools/call result: {error}")))
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

    /// Kills the server process.
    ///
    /// # Errors
    /// Propagates transport-task join failures.
    pub async fn shutdown(self) -> Result<(), McpError> {
        self.transport.shutdown().await
    }
}

fn serde_error(error: &serde_json::Error) -> McpError {
    McpError::Malformed(format!("request serialization failed: {error}"))
}
