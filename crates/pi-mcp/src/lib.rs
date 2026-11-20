//! Model Context Protocol client support.
//!
//! Rust port of `@earendil-works/pi-mcp` (TS source: `packages/mcp/src`).
//! The stdio transport and client handshake follow the MCP specification
//! directly (protocol version [`PROTOCOL_VERSION`]); the streamable HTTP
//! transport ports `packages/mcp/src/transports/streamable-http.ts` (the
//! OAuth provider port lands with the OAuth slice).
//!
//! * [`types`] — JSON-RPC 2.0 envelope + MCP message types, plus the
//!   input-schema normalizer.
//! * [`transport`] — subprocess framing with request/response correlation.
//! * [`http`] — the streamable HTTP transport (SSE parser, session
//!   capture, GET stream, resumption).
//! * [`client`] — `initialize` handshake, tool discovery and calls.
//! * [`error`] — the error surface.

/// The MCP client: `initialize` handshake, tool discovery and calls.
pub mod client;
/// The error surface ([`McpError`]).
pub mod error;
/// The streamable HTTP transport (`streamable-http.ts`).
pub mod http;
/// Subprocess stdio framing with request/response correlation.
pub mod transport;
/// JSON-RPC 2.0 envelope + MCP message types.
pub mod types;

/// MCP protocol version this port sends in the `initialize` handshake
/// (upstream sends `LATEST_PROTOCOL_VERSION`, "2025-11-25"; the port pins
/// its own — a recorded deviation — and accepts whatever version the
/// server picks from [`SUPPORTED_PROTOCOL_VERSIONS`]).
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Versions the client accepts from a server
/// (`SUPPORTED_PROTOCOL_VERSIONS`, upstream
/// `protocol/types.ts:9` — servers answer with their own latest when they
/// do not support the requested one).
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 4] =
    ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Client identity sent in the `initialize` handshake.
pub const CLIENT_NAME: &str = "pi-rs";

/// Client version sent in the `initialize` handshake.
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub use client::{McpClient, McpServerConfig, McpTransportConfig};
pub use error::McpError;
pub use http::{AuthProvider, StreamableHttpTransport};
pub use types::{
    CallToolResult, ContentBlock, McpTool, ToolsListResult, normalize_mcp_input_schema,
};
