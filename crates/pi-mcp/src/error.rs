//! MCP client error type.

use crate::oauth::errors::OAuthError;

/// Errors the client surfaces to the caller.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The server process could not be spawned.
    #[error("failed to spawn MCP server {command:?}: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    /// The transport died (server exited, pipe closed).
    #[error("MCP server connection closed: {0}")]
    ConnectionClosed(String),
    /// The server replied with a JSON-RPC error.
    #[error("MCP server error {code}: {message}")]
    Rpc { code: i64, message: String },
    /// The reply line was not valid JSON-RPC.
    #[error("MCP server sent malformed data: {0}")]
    Malformed(String),
    /// A request timed out waiting for its response.
    #[error("MCP request timed out after {timeout_secs}s (method {method})")]
    Timeout { method: String, timeout_secs: u64 },
    /// The `initialize` handshake failed or negotiated a bad version.
    #[error("MCP handshake failed: {0}")]
    Handshake(String),
    /// I/O on the server pipes.
    #[error("MCP transport I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A network-level HTTP failure (DNS, connect, connection reset
    /// mid-body) — retryable like upstream's `TypeError` on `fetch`.
    #[error("MCP network error: {0}")]
    Network(String),
    /// An HTTP status the transport cannot answer (`McpHttpError`); `body`
    /// carries the first [`crate::http::MAX_ERROR_BODY_BYTES`] bytes.
    #[error("{message}")]
    Http {
        status: u16,
        message: String,
        body: String,
    },
    /// The server requires authentication: 401, or a 403 bearer challenge
    /// asking for `insufficient_scope` (`McpAuthRequiredError`).
    #[error("MCP server requires authentication")]
    AuthRequired {
        /// The response's `www-authenticate` header.
        www_authenticate: Option<String>,
        body: String,
    },
    /// The server no longer knows the session (404 after a session id was
    /// captured — restart or deploy; `McpSessionExpiredError`).
    #[error("MCP session expired")]
    SessionExpired { body: String },
    /// An OAuth flow failure (`oauth::OAuthError`; the
    /// [`OAuthError::AuthorizationRequired`] variant reaches the transport
    /// through the auth provider's recovery hook).
    #[error("{0}")]
    OAuth(#[from] OAuthError),
}

impl McpError {
    /// Whether this failure means the user must sign in
    /// (`needsSignIn`, `extensions/mcp/runtime.ts:320-322`): a 401/insufficient
    /// -scope challenge or an OAuth flow that needs user interaction.
    #[must_use]
    pub fn needs_auth(&self) -> bool {
        matches!(self, Self::AuthRequired { .. })
            || matches!(self, Self::OAuth(OAuthError::AuthorizationRequired))
    }
}

/// `Clone` so concurrent requests joining one in-flight OAuth refresh all
/// see the same failure (upstream: every waiter of the shared promise gets
/// the same error object). `std::io::Error` is not `Clone`, so the two
/// spawn/I/O variants rebuild one from the kind and the message.
impl Clone for McpError {
    fn clone(&self) -> Self {
        let io_clone =
            |error: &std::io::Error| std::io::Error::new(error.kind(), error.to_string());
        match self {
            McpError::Spawn { command, source } => McpError::Spawn {
                command: command.clone(),
                source: io_clone(source),
            },
            McpError::Io(source) => McpError::Io(io_clone(source)),
            McpError::ConnectionClosed(message) => McpError::ConnectionClosed(message.clone()),
            McpError::Rpc { code, message } => McpError::Rpc {
                code: *code,
                message: message.clone(),
            },
            McpError::Malformed(message) => McpError::Malformed(message.clone()),
            McpError::Timeout {
                method,
                timeout_secs,
            } => McpError::Timeout {
                method: method.clone(),
                timeout_secs: *timeout_secs,
            },
            McpError::Handshake(message) => McpError::Handshake(message.clone()),
            McpError::Network(message) => McpError::Network(message.clone()),
            McpError::Http {
                status,
                message,
                body,
            } => McpError::Http {
                status: *status,
                message: message.clone(),
                body: body.clone(),
            },
            McpError::AuthRequired {
                www_authenticate,
                body,
            } => McpError::AuthRequired {
                www_authenticate: www_authenticate.clone(),
                body: body.clone(),
            },
            McpError::SessionExpired { body } => McpError::SessionExpired { body: body.clone() },
            McpError::OAuth(error) => McpError::OAuth(error.clone()),
        }
    }
}
