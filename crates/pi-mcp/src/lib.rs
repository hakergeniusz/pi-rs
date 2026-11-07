//! Model Context Protocol client support.
//!
//! Rust port of `@earendil-works/pi-mcp` (TS source: `packages/mcp/src`).
//!
//! Scope (P5): MCP client over stdio and HTTP transports, tool/resource
//! discovery surfaced into the agent's tool registry, server lifecycle
//! management driven by `~/.pi/agent/mcp-servers.json` (see
//! `coding-agent/src/core/mcp-servers.ts` and docs/mcp.md).

/// MCP protocol version this port targets.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
