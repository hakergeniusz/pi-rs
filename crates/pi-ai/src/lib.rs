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

/// Built-in per-provider OAuth login and refresh flows (`packages/ai/src/auth/`).
pub mod auth;
/// The embedded model catalog: generated data, overlay, lookup.
pub mod catalog;
/// The tool-declared constrained-sampling config (`ConstrainedSamplingConfig`).
pub mod constrained_sampling;
/// Cost calculation from usage and catalog prices (TS `calculateCost`).
pub mod cost;
/// The async [`AssistantMessageEventStream`] over provider events.
pub mod event_stream;
/// Context-overflow classifiers (`packages/ai/src/utils/overflow.ts`).
pub mod overflow;
/// Provider-API adapters, dispatched per `Model.api`.
pub mod providers;
/// Retry classification and policy (`packages/ai/src/utils/retry.ts`).
pub mod retry;
/// Server-Sent Events parsing over a reqwest byte stream.
pub mod sse;
/// Tolerant JSON parsing for streamed tool-call arguments.
pub mod streaming_json;
/// Core wire types: messages, content blocks, usage, models, events.
pub mod types;

pub use constrained_sampling::{ConstrainedSampling, GrammarFormat, GrammarVariants, StrictMode};
pub use event_stream::AssistantMessageEventStream;
pub use providers::classifier::{
    ClassifierAnswer, ClassifierBoolCriteria, ClassifierContext, ClassifierOptions,
    ClassifierQuestion, ClassifierResult, ClassifierStopReason,
};
pub use providers::images::{AssistantImages, ImagesContext, ImagesOptions, ImagesStopReason};
/// Non-chat provider arms: classifier and image generation wire paths.
pub use providers::{classify, generate_images};
pub use types::get_supported_thinking_levels;
pub use types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, ClassifierModel, ContentBlock,
    Context, HookFuture, ImageModel, InputContent, KnownApi, Message, Model, ModelOutputModality,
    ModelType, PayloadHook, ProviderHook, ProviderResponse, ResponseHook, StopReason,
    StreamEventHook, StreamOptions, SystemMessage, ThinkingLevel, Tool, ToolCall, ToolChoice,
    ToolResultMessage, TranscriptContext, Usage, UserContent, UserMessage,
};
