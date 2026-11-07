//! Unified multi-provider LLM API with streaming.
//!
//! Rust port of `@earendil-works/pi-ai` (TS source: `packages/ai/src`).
//!
//! Key TS surfaces to port (P1+):
//! - `types.ts`: `Message`, content blocks (text/image/toolCall/toolResult),
//!   `Model`, `Usage`, stop reasons, `AssistantMessageEventStream`
//! - `providers/`: ~60 providers, mostly thin configs over a handful of API
//!   shapes (openai-completions/responses, anthropic-messages, google-generative-ai,
//!   bedrock…) + `compat-schema.ts` for OpenAI-compatible relays
//! - `models.generated.ts` + catalog overlay from pi.dev
//! - `auth/`: OAuth flows and api-key resolution
//!
//! Contract inherited from `pi-agent-core`'s `StreamFn`: streaming must not
//! throw on request/model/runtime failures; failures are encoded as protocol
//! events and a final assistant message with stop reason `error`/`aborted`.

/// Unified assistant stop reasons (mirrors TS `StopReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}
