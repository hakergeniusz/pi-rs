//! Agent runtime: loop, tool calling, state management, steering.
//!
//! Rust port of `@earendil-works/pi-agent-core` (TS source:
//! `packages/agent/src`: `agent-loop.ts`, `agent.ts`, `proxy.ts`,
//! `stream-fn.ts`, `types.ts`).
//!
//! Core semantics to preserve (P2):
//! - one turn = model request → streamed assistant response → tool execution
//!   (sequential or parallel mode) → results → next request if needed
//! - steering messages enter after the current assistant turn; follow-ups
//!   after pending work finishes; abort returns queued messages
//! - hooks: `beforeToolCall` (can block with reason, hint terminate),
//!   `afterToolCall` (field-wise merge overrides), transforms
//! - `StreamFn` failures are encoded in-band, never thrown

/// How tool calls from a single assistant message execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    Sequential,
    Parallel,
}
