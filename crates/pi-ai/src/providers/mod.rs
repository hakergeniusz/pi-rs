//! Provider-API adapters: shared dispatch over known APIs.
//!
//! TS equivalent: `api/` modules selected per `Model.api`, wrapped by
//! `models.ts`. Each adapter exposes `stream()` (see `StreamFn` contract:
//! errors are encoded in the event stream, never thrown).

/// Anthropic Messages API adapter (TS `api/anthropic-messages.ts`).
pub mod anthropic;
/// Shared adapter helpers: message assembly, finalization, abort.
pub mod common;
/// OpenAI Codex (ChatGPT backend) adapter (TS `api/openai-codex-responses.ts`).
pub mod openai_codex;
/// OpenAI Chat Completions adapter, shared by the compatible relays.
pub mod openai_completions;

use crate::event_stream::AssistantMessageEventStream;
use crate::types::{Model, StreamOptions, TranscriptContext};

/// The uniform stream function of an API implementation (TS `StreamFunction`).
pub type StreamFn = fn(&Model, &TranscriptContext, &StreamOptions) -> AssistantMessageEventStream;

/// Resolve the streaming function for a model's `api` identifier.
pub fn stream_fn_for(api: &str) -> Option<StreamFn> {
    match api {
        "anthropic-messages" => Some(anthropic::stream),
        "openai-completions" => Some(openai_completions::stream),
        "openai-codex-responses" => Some(openai_codex::stream),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_known_apis() {
        assert!(stream_fn_for("anthropic-messages").is_some());
        assert!(stream_fn_for("openai-completions").is_some());
        assert!(stream_fn_for("openai-codex-responses").is_some());
        assert!(stream_fn_for("unknown-api").is_none());
    }
}
