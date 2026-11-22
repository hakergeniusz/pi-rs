// `@earendil-works/pi-agent-core` virtual module. Upstream bundles the whole
// agent runtime (`core/extensions/virtual-modules.ts:1,22,34`); the port's
// agent loop is Rust-owned, so this shim supplies only what extensions can
// meaningfully construct in-process: type-only surfaces (erased at runtime)
// and the `AgentToolResult` / content data shapes
// (`packages/agent/src/types.ts:424-440`). The live `Agent`, `runToolCall`
// and loop exports are intentionally absent — see WIRING.md.

/** A text content block (`pi-ai` TextContent shape). */
export function textContent(text) {
  return { type: "text", text: String(text ?? "") };
}

/** An image content block (`pi-ai` ImageContent shape). */
export function imageContent(data, mimeType) {
  return { type: "image", data: String(data ?? ""), mimeType: String(mimeType ?? "image/png") };
}

/**
 * Builds an `AgentToolResult` (`types.ts:424`): `{content, details,
 * structuredContent?, usage?, isError?}`. Unknown options pass through so
 * extensions can set fields added in newer upstream versions.
 */
export function toolResult(content, details, options = {}) {
  const blocks = typeof content === "string" ? [textContent(content)] : (content ?? []);
  return {
    content: Array.isArray(blocks) ? blocks : [blocks],
    details: details ?? {},
    ...options,
  };
}

/** Convenience: a single-text tool result. */
export function textToolResult(text, details = {}, options = {}) {
  return toolResult([textContent(text)], details, options);
}

/** Error tool result — `isError: true` with the message as model-facing text. */
export function errorToolResult(message, details = {}, options = {}) {
  return toolResult([textContent(message)], details, { ...options, isError: true });
}

export function isToolResult(value) {
  return !!value && Array.isArray(value.content) && "details" in value;
}

export function isErrorToolResult(value) {
  return isToolResult(value) && value.isError === true;
}

/**
 * Wraps a finalized tool call into a `ToolResultMessage` (`agent-loop.ts:508`).
 * No execution happens here — the caller supplies the already-finalized
 * `AgentToolCallOutcome`.
 */
export function createToolResultMessage(toolCall, result) {
  return {
    role: "toolResult",
    toolCallId: toolCall?.id ?? toolCall?.toolCallId,
    toolName: toolCall?.name ?? toolCall?.toolName,
    content: result?.content ?? [],
    details: result?.details ?? {},
    isError: result?.isError === true,
    timestamp: Date.now(),
  };
}

/** Upstream's `setDefaultStreamFn` is a process-global setter; keep the value
 * local so imports are harmless, but never wire a live stream into QuickJS. */
let defaultStreamFn;
export function setDefaultStreamFn(fn) {
  defaultStreamFn = fn;
}
export function getDefaultStreamFn() {
  return defaultStreamFn;
}

export default {
  textContent,
  imageContent,
  toolResult,
  textToolResult,
  errorToolResult,
  isToolResult,
  isErrorToolResult,
  createToolResultMessage,
  setDefaultStreamFn,
  getDefaultStreamFn,
};
