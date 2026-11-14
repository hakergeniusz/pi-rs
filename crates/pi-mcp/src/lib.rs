//! Model Context Protocol client support.
//!
//! Rust port of `@earendil-works/pi-mcp` (TS source: `packages/mcp/src`).
//! The stdio transport and client handshake follow the MCP specification
//! directly (protocol version [`PROTOCOL_VERSION`]); HTTP transport and the
//! `mcp-servers.json` server-manager wiring land later in P5.
//!
//! * [`types`] — JSON-RPC 2.0 envelope + MCP message types.
//! * [`transport`] — subprocess framing with request/response correlation.
//! * [`client`] — `initialize` handshake, tool discovery and calls.
//! * [`error`] — the error surface.

pub mod client;
pub mod error;
pub mod transport;
pub mod types;

/// MCP protocol version this port targets.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Client identity sent in the `initialize` handshake.
pub const CLIENT_NAME: &str = "pi-rs";

/// Client version sent in the `initialize` handshake.
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub use client::{McpClient, McpServerConfig};
pub use error::McpError;
pub use types::{CallToolResult, ContentBlock, McpTool, ToolsListResult};
