// `@earendil-works/pi-mcp` virtual module — the naming/config helpers the
// bundled MCP extension imports at runtime
// (`extensions/mcp/{index,config,runtime}.ts`). Ported verbatim from
// `packages/mcp/src/naming.ts:7-36` and `packages/mcp/src/config.ts:241-243`;
// validation delegates to the host's Rust MCP registry so the error strings
// stay identical.

import { createHash } from "node:crypto";

const ops = globalThis.__piOps;

/** Provider tool names are limited to 64 characters of `[A-Za-z0-9_-]`. */
const MAX_TOOL_NAME_LENGTH = 64;

export function createMcpToolName(server, tool, isTaken = () => false) {
  const name = `mcp__${server}__${tool}`.replace(/[^A-Za-z0-9_]/g, "_");
  if (name.length <= MAX_TOOL_NAME_LENGTH && !isTaken(name)) return name;
  const hash = createHash("sha256")
    .update(`${server}\0${tool}`)
    .digest("hex")
    .slice(0, 8);
  return `${name.slice(0, MAX_TOOL_NAME_LENGTH - hash.length - 1)}_${hash}`;
}

export function mcpNamespace(server) {
  return `mcp__${String(server).replace(/-/g, "_")}`;
}

export function isMcpServerOverride(entry) {
  return entry.command === undefined && entry.url === undefined && entry.type === undefined;
}

export function validateMcpServerConfig(name, config) {
  const error = ops.validateMcpServer(String(name), JSON.stringify(config));
  if (error) throw new Error(error);
  return config;
}

export function limitMcpContent(content, maxBytes) {
  // The native MCP manager owns content limiting (`mcp_manager.rs`); the
  // shape here is the pure-JS fallback used by tool wrappers.
  const text = typeof content === "string" ? content : JSON.stringify(content);
  const limit = maxBytes ?? 100000;
  if (text.length <= limit) return content;
  return `${text.slice(0, limit)}\n[truncated]`;
}

export const MCP_OUTPUT_MAX_BYTES = 100000;

export default {
  createMcpToolName,
  mcpNamespace,
  isMcpServerOverride,
  validateMcpServerConfig,
  limitMcpContent,
  MCP_OUTPUT_MAX_BYTES,
};
