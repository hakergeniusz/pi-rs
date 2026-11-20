//! MCP client error type.

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
}
