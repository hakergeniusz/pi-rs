//! Provider-API adapters: shared dispatch over known APIs.
//!
//! TS equivalent: `api/` modules selected per `Model.api`, wrapped by
//! `models.ts`. Each adapter exposes `stream()` (see `StreamFn` contract:
//! errors are encoded in the event stream, never thrown).

/// Anthropic Messages API adapter (TS `api/anthropic-messages.ts`).
pub mod anthropic;
/// AWS Bedrock Converse Stream adapter (TS `api/bedrock-converse-stream.ts`).
pub mod bedrock;
pub mod bedrock_sigv4;
/// Shared adapter helpers: message assembly, finalization, abort.
pub mod common;
/// Google Generative AI + Google Vertex adapters (TS `api/google-*.ts`).
pub mod google;
pub mod google_shared;
/// OpenAI Codex (ChatGPT backend) adapter (TS `api/openai-codex-responses.ts`).
pub mod openai_codex;
/// OpenAI Chat Completions adapter, shared by the compatible relays.
pub mod openai_completions;
/// OpenAI Responses + Azure OpenAI Responses adapters (TS `api/openai-responses.ts`).
pub mod openai_responses;
/// Mistral native Chat Completions adapter (TS `api/mistral-conversations.ts`).
pub mod mistral;
/// pi's serialized assistant-message event protocol (TS `api/pi-messages.ts`).
pub mod pi_messages;

use crate::event_stream::AssistantMessageEventStream;
use crate::types::{Model, StreamOptions, TranscriptContext};

/// The uniform stream function of an API implementation (TS `StreamFunction`).
pub type StreamFn = fn(&Model, &TranscriptContext, &StreamOptions) -> AssistantMessageEventStream;

/// Resolve the streaming function for a model's `api` identifier.
pub fn stream_fn_for(api: &str) -> Option<StreamFn> {
    match api {
        "anthropic-messages" => Some(anthropic::stream),
        "bedrock-converse-stream" => Some(bedrock::stream),
        "mistral-conversations" => Some(mistral::stream),
        "pi-messages" => Some(pi_messages::stream),
        "openai-responses" => Some(openai_responses::stream),
        "azure-openai-responses" => Some(openai_responses::azure_stream),
        "openai-completions" => Some(openai_completions::stream),
        "openai-codex-responses" => Some(openai_codex::stream),
        "google-generative-ai" => Some(google::stream_google_generative_ai),
        "google-vertex" => Some(google::stream_google_vertex),
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
