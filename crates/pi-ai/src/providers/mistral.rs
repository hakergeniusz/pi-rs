//! Mistral native Chat Completions adapter: port of TS
//! `packages/ai/src/api/mistral-conversations.ts` (lines 1-924), with the lazy
//! registration stub at `mistral-conversations.lazy.ts:1-4`.
//!
//! Ported areas:
//!
//! - stream lifecycle and terminal checks (lines 126-179): `start` is emitted
//!   only after the HTTP response succeeds; a missing finish reason, or an
//!   `error`/`aborted` stop reason, becomes a terminal `error` event.
//! - header/URL/payload construction (lines 290-447, 519-546, 895-906):
//!   `/v1/chat/completions`, bearer auth, `x-affinity` prompt caching, and the
//!   `camelCase` → `snake_case` wire remapping.
//! - SSE framing (lines 449-517): tolerant blank-line boundaries, multi-line
//!   `data:` fields, and the `[DONE]` sentinel that ends the read loop.
//! - stream consumption (lines 568-747): text/thinking block switching with
//!   `text_end`/`thinking_end`, tool-call accumulation keyed by `index`, usage
//!   with cached-prompt-token extraction, and `toolcall_end` after the loop.
//! - stop-reason mapping (lines 908-924) and error formatting (lines 267-288).
//! - message/tool replay (lines 749-893) and Mistral's 9-char tool-call ids
//!   (lines 30-31, 235-265).
//!
//! Deliberate deviations are listed in `WIRING.md` at the worktree root.

// 1:1 port of the upstream TS adapter: the TS-shaped control flow, literal
// tables and helper structs trigger pedantic lints; keep the mirror exact.
#![allow(
    clippy::assert_is_empty,
    clippy::assigning_clones,
    clippy::collapsible_if,
    clippy::default_trait_access,
    clippy::doc_markdown,
    clippy::field_reassign_with_default,
    clippy::float_cmp,
    clippy::format_push_string,
    clippy::if_not_else,
    clippy::if_same_then_else,
    clippy::manual_assert_eq,
    clippy::manual_is_multiple_of,
    clippy::manual_strip,
    clippy::map_unwrap_or,
    clippy::needless_pass_by_value,
    clippy::redundant_closure_for_method_calls,
    clippy::result_large_err,
    clippy::single_match_else,
    clippy::struct_excessive_bools,
    clippy::too_many_arguments,
    clippy::unnecessary_get_then_check,
    clippy::unnecessary_map_or,
    clippy::unnecessary_wraps,
    clippy::useless_concat,
    dead_code
)]

use crate::cost::calculate_cost;
use crate::event_stream::{AssistantMessageEventStream, EventSender};
use crate::providers::common::{
    apply_payload_hook, apply_response_hook, apply_stream_event_hook, initial_output,
    response_metadata, system_text, system_update_text, tools_at_point,
};
use crate::sse::{SseError, SseMessage, read_sse_stream};
use crate::streaming_json::parse_tool_call_arguments;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Message, Model, ModelInputModality,
    StopReason, StreamOptions, ToolCall, TranscriptContext, now_ms,
};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

const MISTRAL_TOOL_CALL_ID_LENGTH: usize = 9;
const MAX_MISTRAL_ERROR_BODY_CHARS: usize = 4000;

/// One streamed Mistral completion chunk (TS lines 89-121).
#[derive(Debug, Clone, Deserialize)]
struct MistralChunk {
    id: Option<String>,
    usage: Option<Value>,
    choices: Vec<MistralChoice>,
}

#[derive(Debug, Clone, Deserialize)]
struct MistralChoice {
    finish_reason: Option<String>,
    delta: MistralDelta,
}

#[derive(Debug, Clone, Deserialize)]
struct MistralDelta {
    content: Option<Value>,
    tool_calls: Option<Vec<MistralStreamToolCall>>,
}

#[derive(Debug, Clone, Deserialize)]
struct MistralStreamToolCall {
    id: Option<String>,
    index: Option<u64>,
    function: MistralStreamFunction,
}

#[derive(Debug, Clone, Deserialize)]
struct MistralStreamFunction {
    name: Option<String>,
    /// A string of incremental JSON, or an already-decoded JSON object.
    arguments: Option<Value>,
}

/// Which content block is currently open (TS lines 575-580).
#[derive(Debug, Clone, Copy)]
enum BlockKind {
    Text(usize),
    Thinking(usize),
}

/// Per-tool-call accumulator; deliberately kept off the content blocks so
/// stream state never reaches `partial` (TS lines 578-579).
#[derive(Debug)]
struct OpenToolCall {
    content_index: usize,
    arguments_text: String,
}

#[derive(Debug, Default)]
struct StreamState {
    current: Option<BlockKind>,
    tools: HashMap<String, OpenToolCall>,
    /// JS `Map` preserves insertion order; keep the keys so `toolcall_end`
    /// events come out in the same order as `toolcall_start` (TS lines 737-746).
    tool_order: Vec<String>,
}

/// Fast deterministic hash used for derived tool-call ids (TS `utils/hash.ts`,
/// ported from the same cyrb-style routine used elsewhere in this crate).
fn short_hash(input: &str) -> String {
    let mut h1: u32 = 0xdead_beef;
    let mut h2: u32 = 0x41c6_ce57;
    for ch in input.chars() {
        let code = u32::from(ch);
        h1 = (h1 ^ code).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ code).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    format!("{}{}", to_base36(h2), to_base36(h1))
}

fn to_base36(mut value: u32) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_string();
    }
    let mut digits = Vec::new();
    while value > 0 {
        digits.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    digits.reverse();
    String::from_utf8(digits).unwrap_or_default()
}

/// `deriveMistralToolCallId` (TS lines 257-265): Mistral accepts only 9
/// alphanumeric characters; hashed ids are used when the sanitized id is not
/// already exactly 9 chars.
fn derive_tool_call_id(id: &str, attempt: u32) -> String {
    let normalized: String = id.chars().filter(|ch| ch.is_ascii_alphanumeric()).collect();
    if attempt == 0 && normalized.len() == MISTRAL_TOOL_CALL_ID_LENGTH {
        return normalized;
    }
    let seed_base = if normalized.is_empty() {
        id
    } else {
        &normalized
    };
    let seed = if attempt == 0 {
        seed_base.to_string()
    } else {
        format!("{seed_base}:{attempt}")
    };
    short_hash(&seed)
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(MISTRAL_TOOL_CALL_ID_LENGTH)
        .collect()
}

/// `createMistralToolCallIdNormalizer` (TS lines 235-255): stable, collision-
/// free id mapping with a reverse map so the same original id always maps to
/// the same 9-char id.
#[derive(Debug, Default)]
struct ToolCallIdNormalizer {
    id_map: HashMap<String, String>,
    reverse_map: HashMap<String, String>,
}

impl ToolCallIdNormalizer {
    fn normalize(&mut self, id: &str) -> String {
        if let Some(existing) = self.id_map.get(id) {
            return existing.clone();
        }
        let mut attempt = 0;
        loop {
            let candidate = derive_tool_call_id(id, attempt);
            let owner = self.reverse_map.get(&candidate);
            if owner.is_none_or(|owner| owner == id) {
                self.id_map.insert(id.to_string(), candidate.clone());
                self.reverse_map.insert(candidate.clone(), id.to_string());
                return candidate;
            }
            attempt += 1;
        }
    }
}

/// `mapChatStopReason` (TS lines 908-924).
fn map_chat_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" => (StopReason::Stop, None),
        "length" | "model_length" => (StopReason::Length, None),
        "tool_calls" => (StopReason::ToolUse, None),
        "error" => (
            StopReason::Error,
            Some("Provider stopped with: error (server error)".to_string()),
        ),
        other => (
            StopReason::Error,
            Some(format!("Provider stopped with: {other}")),
        ),
    }
}

/// Extract cached prompt tokens from any of the spellings Mistral-compatible
/// relays use (TS lines 547-566).
fn cached_prompt_tokens(usage: &Value, prompt_tokens: u64) -> u64 {
    let paths: [&[&str]; 6] = [
        &["promptTokensDetails", "cachedTokens"],
        &["prompt_tokens_details", "cached_tokens"],
        &["promptTokenDetails", "cachedTokens"],
        &["prompt_token_details", "cached_tokens"],
        &["numCachedTokens"],
        &["num_cached_tokens"],
    ];
    let null = Value::Null;
    for path in paths {
        let mut current = usage;
        for segment in path {
            match current.get(*segment) {
                Some(next) => current = next,
                None => {
                    current = &null;
                    break;
                }
            }
        }
        if let Some(number) = current.as_u64() {
            return number.min(prompt_tokens);
        }
    }
    0
}

/// Apply `chunk.usage` (TS lines 609-621).
fn apply_usage(usage: &Value, output: &mut AssistantMessage, model: &Model) {
    let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    let prompt_tokens = field("prompt_tokens");
    let cached = cached_prompt_tokens(usage, prompt_tokens);
    output.usage.input = prompt_tokens.saturating_sub(cached);
    output.usage.output = field("completion_tokens");
    output.usage.cache_read = cached;
    output.usage.cache_write = 0;
    output.usage.total_tokens = match usage.get("total_tokens").and_then(Value::as_u64) {
        Some(total) if total > 0 => total,
        _ => output.usage.input + output.usage.output + output.usage.cache_read,
    };
    calculate_cost(&mut output.usage, &model.cost);
}

/// Send an event, mapping a closed channel to a stream error.
async fn emit(sender: &EventSender, event: AssistantMessageEvent) -> Result<(), String> {
    sender
        .send(event)
        .await
        .map_err(|_| "event stream closed".to_string())
}

/// Emit the terminal `text_end` / `thinking_end` for the open block (TS lines
/// 581-600).
async fn finish_current_block(
    current: &mut Option<BlockKind>,
    output: &AssistantMessage,
    sender: &EventSender,
) -> Result<(), String> {
    match current.take() {
        Some(BlockKind::Text(index)) => {
            let content = output
                .content
                .get(index)
                .and_then(AssistantContent::as_text)
                .unwrap_or_default()
                .to_string();
            emit(
                sender,
                AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content,
                    partial: output.clone(),
                },
            )
            .await
        }
        Some(BlockKind::Thinking(index)) => {
            let content = match output.content.get(index) {
                Some(AssistantContent::Thinking(block)) => block.thinking.clone(),
                _ => String::new(),
            };
            emit(
                sender,
                AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content,
                    partial: output.clone(),
                },
            )
            .await
        }
        None => Ok(()),
    }
}

/// Open (or reuse) a text block (TS lines 644-649, 686-691).
async fn ensure_text_block(
    current: &mut Option<BlockKind>,
    output: &mut AssistantMessage,
    sender: &EventSender,
) -> Result<usize, String> {
    if let Some(BlockKind::Text(index)) = *current {
        return Ok(index);
    }
    finish_current_block(current, output, sender).await?;
    output
        .content
        .push(AssistantContent::Text(crate::types::TextContent {
            text: String::new(),
            text_signature: None,
        }));
    let index = output.content.len() - 1;
    *current = Some(BlockKind::Text(index));
    emit(
        sender,
        AssistantMessageEvent::TextStart {
            content_index: index,
            partial: output.clone(),
        },
    )
    .await?;
    Ok(index)
}

/// Open (or reuse) a thinking block (TS lines 667-672).
async fn ensure_thinking_block(
    current: &mut Option<BlockKind>,
    output: &mut AssistantMessage,
    sender: &EventSender,
) -> Result<usize, String> {
    if let Some(BlockKind::Thinking(index)) = *current {
        return Ok(index);
    }
    finish_current_block(current, output, sender).await?;
    output
        .content
        .push(AssistantContent::Thinking(crate::types::ThinkingContent {
            thinking: String::new(),
            thinking_signature: None,
            redacted: false,
        }));
    let index = output.content.len() - 1;
    *current = Some(BlockKind::Thinking(index));
    emit(
        sender,
        AssistantMessageEvent::ThinkingStart {
            content_index: index,
            partial: output.clone(),
        },
    )
    .await?;
    Ok(index)
}

/// Apply one content item from a chunk's `delta.content` (TS lines 636-701).
/// Rust strings are valid UTF-8, so the TS `sanitizeSurrogates` pass is a
/// no-op here (see WIRING.md).
#[allow(clippy::too_many_lines)] // mirrors the TS item loop; split when extending
async fn apply_content_item(
    item: &Value,
    current: &mut Option<BlockKind>,
    output: &mut AssistantMessage,
    sender: &EventSender,
) -> Result<(), String> {
    if let Some(text) = item.as_str() {
        if text.is_empty() {
            return Ok(());
        }
        let index = ensure_text_block(current, output, sender).await?;
        if let Some(AssistantContent::Text(block)) = output.content.get_mut(index) {
            block.text.push_str(text);
        }
        return emit(
            sender,
            AssistantMessageEvent::TextDelta {
                content_index: index,
                delta: text.to_string(),
                partial: output.clone(),
            },
        )
        .await;
    }
    match item.get("type").and_then(Value::as_str) {
        Some("thinking") => {
            let delta: String = item
                .get("thinking")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect();
            if delta.is_empty() {
                return Ok(());
            }
            let index = ensure_thinking_block(current, output, sender).await?;
            if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(index) {
                block.thinking.push_str(&delta);
            }
            emit(
                sender,
                AssistantMessageEvent::ThinkingDelta {
                    content_index: index,
                    delta,
                    partial: output.clone(),
                },
            )
            .await
        }
        Some("text") => {
            let delta = item.get("text").and_then(Value::as_str).unwrap_or_default();
            if delta.is_empty() {
                return Ok(());
            }
            let index = ensure_text_block(current, output, sender).await?;
            if let Some(AssistantContent::Text(block)) = output.content.get_mut(index) {
                block.text.push_str(delta);
            }
            emit(
                sender,
                AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta: delta.to_string(),
                    partial: output.clone(),
                },
            )
            .await
        }
        _ => Ok(()),
    }
}

/// Apply the tool-call deltas of one chunk (TS lines 703-733).
async fn apply_tool_call_deltas(
    calls: &[MistralStreamToolCall],
    state: &mut StreamState,
    output: &mut AssistantMessage,
    sender: &EventSender,
) -> Result<(), String> {
    for call in calls {
        if state.current.is_some() {
            finish_current_block(&mut state.current, output, sender).await?;
        }
        let index = call.index.unwrap_or(0);
        let call_id = match call.id.as_deref() {
            Some(id) if id != "null" && !id.is_empty() => id.to_string(),
            _ => derive_tool_call_id(&format!("toolcall:{index}"), 0),
        };
        let key = if call.index.is_some() {
            format!("idx:{index}")
        } else {
            format!("id:{call_id}")
        };
        if !state.tools.contains_key(&key) {
            output.content.push(AssistantContent::ToolCall(ToolCall {
                id: call_id,
                name: call.function.name.clone().unwrap_or_default(),
                arguments: Map::new(),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            }));
            let content_index = output.content.len() - 1;
            state.tools.insert(
                key.clone(),
                OpenToolCall {
                    content_index,
                    arguments_text: String::new(),
                },
            );
            state.tool_order.push(key.clone());
            emit(
                sender,
                AssistantMessageEvent::ToolcallStart {
                    content_index,
                    partial: output.clone(),
                },
            )
            .await?;
        }
        let arguments_delta = match call.function.arguments.as_ref() {
            Some(Value::String(text)) => text.clone(),
            Some(value) => serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string()),
            None => "{}".to_string(),
        };
        if let Some(open) = state.tools.get_mut(&key) {
            open.arguments_text.push_str(&arguments_delta);
            if let Some(AssistantContent::ToolCall(block)) =
                output.content.get_mut(open.content_index)
                && let Ok(arguments) = parse_tool_call_arguments(&open.arguments_text)
            {
                block.arguments = arguments;
            }
        }
        let content_index = state.tools.get(&key).map_or(0, |open| open.content_index);
        emit(
            sender,
            AssistantMessageEvent::ToolcallDelta {
                content_index,
                delta: arguments_delta,
                partial: output.clone(),
            },
        )
        .await?;
    }
    Ok(())
}

/// Apply one parsed chunk to the accumulating message (TS lines 602-734).
#[allow(clippy::too_many_lines)] // mirrors the TS chunk loop; split when extending
async fn apply_chunk(
    chunk: &MistralChunk,
    output: &mut AssistantMessage,
    state: &mut StreamState,
    sender: &EventSender,
) -> Result<(), String> {
    if output.response_id.is_none()
        && let Some(id) = chunk.id.as_deref()
        && !id.is_empty()
    {
        output.response_id = Some(id.to_string());
    }
    let Some(choice) = chunk.choices.first() else {
        return Ok(());
    };
    if let Some(reason) = choice.finish_reason.as_deref() {
        output.raw_stop_reason = Some(reason.to_string());
        let (stop, error_message) = map_chat_stop_reason(reason);
        output.stop_reason = stop;
        if let Some(error_message) = error_message {
            output.error_message = Some(error_message);
        }
    }
    if let Some(content) = choice.delta.content.as_ref() {
        match content {
            Value::Array(items) => {
                for item in items {
                    apply_content_item(item, &mut state.current, output, sender).await?;
                }
            }
            Value::String(_) => {
                apply_content_item(content, &mut state.current, output, sender).await?;
            }
            _ => {}
        }
    }
    if let Some(calls) = choice.delta.tool_calls.as_deref() {
        apply_tool_call_deltas(calls, state, output, sender).await?;
    }
    Ok(())
}

/// Consume the whole SSE chunk stream and close all open blocks (TS lines
/// 568-747). `model` is used for usage cost calculation.
async fn consume_chat_stream<S>(
    mut sse: S,
    model: &Model,
    options: &StreamOptions,
    output: &mut AssistantMessage,
    sender: &EventSender,
) -> Result<(), String>
where
    S: Stream<Item = Result<SseMessage, SseError>> + Unpin,
{
    let mut state = StreamState::default();
    while let Some(item) = sse.next().await {
        let message = match item {
            Ok(message) => message,
            Err(SseError::Status { status, body }) => return Err(format!("HTTP {status}: {body}")),
            Err(SseError::Http(message)) => return Err(message),
        };
        let data = message.data.trim();
        if data.is_empty() {
            continue;
        }
        // Mistral terminates with `[DONE]`; unlike pi-messages this is a normal
        // end-of-stream marker, not an error (TS lines 502-517).
        if data == "[DONE]" {
            break;
        }
        let raw: Value = serde_json::from_str(data)
            .map_err(|e| format!("could not parse Mistral SSE chunk: {e}"))?;
        apply_stream_event_hook(&options.on_provider_stream_event, raw.clone(), model).await?;
        let chunk: MistralChunk = serde_json::from_value(raw)
            .map_err(|e| format!("invalid Mistral streaming event: {e}"))?;
        if let Some(usage) = &chunk.usage {
            apply_usage(usage, output, model);
        }
        apply_chunk(&chunk, output, &mut state, sender).await?;
    }

    finish_current_block(&mut state.current, output, sender).await?;
    for key in state.tool_order {
        let Some(open) = state.tools.remove(&key) else {
            continue;
        };
        let Some(AssistantContent::ToolCall(block)) = output.content.get_mut(open.content_index)
        else {
            continue;
        };
        match parse_tool_call_arguments(&open.arguments_text) {
            Ok(arguments) => {
                block.arguments = arguments;
                block.arguments_error = None;
            }
            Err(error) => {
                block.arguments = Map::new();
                block.arguments_error = Some(error);
            }
        }
        let tool_call = block.clone();
        emit(
            sender,
            AssistantMessageEvent::ToolcallEnd {
                content_index: open.content_index,
                tool_call,
                partial: output.clone(),
            },
        )
        .await?;
    }
    Ok(())
}

/// `shouldUsePromptCaching` (TS lines 543-545). The Rust `CacheRetention`
/// enum has no `none` variant; a raw `metadata.cacheRetention == "none"`
/// disables caching, matching the TS opt-out.
fn should_use_prompt_caching(options: &StreamOptions) -> bool {
    let disabled = options
        .metadata
        .as_ref()
        .and_then(|m| m.get("cacheRetention"))
        .and_then(Value::as_str)
        == Some("none");
    !disabled && options.session_id.is_some()
}

/// Remap a `camelCase` payload key to its Mistral wire spelling (TS lines 439-443).
fn remap(record: &mut Map<String, Value>, source: &str, target: &str) {
    if let Some(value) = record.remove(source) {
        record.insert(target.to_string(), value);
    }
}

/// `toMistralWirePayload` (TS lines 378-412).
fn to_mistral_wire_payload(payload: &Value) -> Value {
    let mut wire = payload.clone();
    let Some(record) = wire.as_object_mut() else {
        return wire;
    };
    for (source, target) in [
        ("topP", "top_p"),
        ("maxTokens", "max_tokens"),
        ("randomSeed", "random_seed"),
        ("responseFormat", "response_format"),
        ("toolChoice", "tool_choice"),
        ("presencePenalty", "presence_penalty"),
        ("frequencyPenalty", "frequency_penalty"),
        ("parallelToolCalls", "parallel_tool_calls"),
        ("reasoningEffort", "reasoning_effort"),
        ("promptMode", "prompt_mode"),
        ("promptCacheKey", "prompt_cache_key"),
        ("safePrompt", "safe_prompt"),
    ] {
        remap(record, source, target);
    }
    if let Some(messages) = record.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            to_mistral_wire_message(message);
        }
    }
    if let Some(response_format) = record
        .get_mut("response_format")
        .and_then(Value::as_object_mut)
    {
        remap(response_format, "jsonSchema", "json_schema");
        if let Some(schema) = response_format
            .get_mut("json_schema")
            .and_then(Value::as_object_mut)
        {
            remap(schema, "schemaDefinition", "schema");
        }
    }
    wire
}

fn to_mistral_wire_message(message: &mut Value) {
    let Some(record) = message.as_object_mut() else {
        return;
    };
    remap(record, "toolCalls", "tool_calls");
    remap(record, "toolCallId", "tool_call_id");
    if let Some(Value::Array(content)) = record.get_mut("content") {
        for chunk in content {
            let Some(chunk) = chunk.as_object_mut() else {
                continue;
            };
            for (source, target) in [
                ("imageUrl", "image_url"),
                ("documentUrl", "document_url"),
                ("documentName", "document_name"),
                ("fileId", "file_id"),
                ("referenceIds", "reference_ids"),
                ("inputAudio", "input_audio"),
            ] {
                remap(chunk, source, target);
            }
        }
    }
}

/// `buildToolResultText` (TS lines 874-893).
fn build_tool_result_text(
    text: &str,
    has_images: bool,
    supports_images: bool,
    is_error: bool,
) -> String {
    let trimmed = text.trim();
    let error_prefix = if is_error { "[tool error] " } else { "" };
    if !trimmed.is_empty() {
        let suffix = if has_images && !supports_images {
            "\n[tool image omitted: model does not support images]"
        } else {
            ""
        };
        return format!("{error_prefix}{trimmed}{suffix}");
    }
    if has_images {
        if supports_images {
            return if is_error {
                "[tool error] (see attached image)".to_string()
            } else {
                "(see attached image)".to_string()
            };
        }
        return if is_error {
            "[tool error] (image omitted: model does not support images)".to_string()
        } else {
            "(image omitted: model does not support images)".to_string()
        };
    }
    if is_error {
        "[tool error] (no tool output)".to_string()
    } else {
        "(no tool output)".to_string()
    }
}

/// `transformMessages` (TS `api/transform-messages.ts:91-256`), reduced to the
/// behaviors Mistral needs: image downgrade, foreign-thinking drop, tool-call
/// id normalization, skipping errored turns and synthesizing orphan results.
#[allow(clippy::too_many_lines)] // mirrors the TS two-pass transform; split when extending
fn transform_messages(messages: &[Message], model: &Model) -> Vec<Message> {
    let supports_images = model.input.contains(&ModelInputModality::Image);
    let mut normalizer = ToolCallIdNormalizer::default();
    let mut id_map: HashMap<String, String> = HashMap::new();
    let mut first: Vec<Message> = Vec::new();
    for message in messages {
        match message {
            Message::System(system) => first.push(Message::System(system.clone())),
            Message::User(user) => {
                let content = match &user.content {
                    crate::types::UserContent::Text(_) => user.content.clone(),
                    crate::types::UserContent::Blocks(blocks) => {
                        let replaced: Vec<_> = blocks
                            .iter()
                            .map(|block| match block {
                                crate::types::InputContent::Image(image) if !supports_images => {
                                    crate::types::InputContent::Text(crate::types::TextContent {
                                        text: "(image omitted: model does not support images)"
                                            .to_string(),
                                        text_signature: None,
                                    })
                                }
                                other => other.clone(),
                            })
                            .collect();
                        crate::types::UserContent::Blocks(replaced)
                    }
                };
                first.push(Message::User(crate::types::UserMessage {
                    content,
                    timestamp: user.timestamp,
                }));
            }
            Message::Assistant(assistant) => {
                if matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    continue;
                }
                let same_model = assistant.provider == model.provider
                    && assistant.api == model.api
                    && assistant.model == model.id;
                let mut content = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Thinking(thinking) => {
                            if !same_model {
                                continue;
                            }
                            if thinking.redacted || thinking.thinking_signature.is_some() {
                                content.push(block.clone());
                            } else if !thinking.thinking.trim().is_empty() {
                                content.push(block.clone());
                            }
                        }
                        AssistantContent::Text(_) => content.push(block.clone()),
                        AssistantContent::ToolCall(call) => {
                            let mut call = call.clone();
                            if !same_model {
                                let new_id = normalizer.normalize(&call.id);
                                if new_id != call.id {
                                    id_map.insert(call.id.clone(), new_id.clone());
                                    call.id = new_id;
                                }
                            }
                            content.push(AssistantContent::ToolCall(call));
                        }
                    }
                }
                let mut transformed = assistant.clone();
                transformed.content = content;
                first.push(Message::Assistant(transformed));
            }
            Message::ToolResult(result) => {
                let mut transformed = result.clone();
                if let Some(id) = id_map.get(&transformed.tool_call_id) {
                    transformed.tool_call_id = id.clone();
                }
                transformed.content = transformed
                    .content
                    .iter()
                    .map(|block| match block {
                        crate::types::InputContent::Image(image) if !supports_images => {
                            crate::types::InputContent::Text(crate::types::TextContent {
                                text: "(tool image omitted: model does not support images)"
                                    .to_string(),
                                text_signature: None,
                            })
                        }
                        other => other.clone(),
                    })
                    .collect();
                first.push(Message::ToolResult(transformed));
            }
        }
    }

    // Second pass: insert synthetic tool results for calls never answered.
    let mut result = Vec::new();
    let mut pending: Vec<ToolCall> = Vec::new();
    let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
    for message in first {
        match message {
            Message::Assistant(assistant) => {
                append_synthetic_results(&mut result, &pending, &answered);
                pending.clear();
                answered.clear();
                let calls: Vec<ToolCall> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::ToolCall(call) => Some(call.clone()),
                        _ => None,
                    })
                    .collect();
                if !calls.is_empty() {
                    pending = calls;
                }
                result.push(Message::Assistant(assistant));
            }
            Message::ToolResult(ref tool_result) => {
                answered.insert(tool_result.tool_call_id.clone());
                result.push(message);
            }
            Message::User(_) => {
                append_synthetic_results(&mut result, &pending, &answered);
                pending.clear();
                answered.clear();
                result.push(message);
            }
            Message::System(_) => result.push(message),
        }
    }
    append_synthetic_results(&mut result, &pending, &answered);
    result
}

/// Emit synthetic `No result provided` tool results for unanswered calls.
fn append_synthetic_results(
    result: &mut Vec<Message>,
    pending: &[ToolCall],
    answered: &std::collections::HashSet<String>,
) {
    for call in pending {
        if answered.contains(&call.id) {
            continue;
        }
        result.push(Message::ToolResult(crate::types::ToolResultMessage {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: vec![crate::types::InputContent::Text(
                crate::types::TextContent {
                    text: "No result provided".to_string(),
                    text_signature: None,
                },
            )],
            details: None,
            usage: None,
            nested_calls: None,
            is_error: true,
            timestamp: now_ms(),
            duration_ms: None,
        }));
    }
}

/// `toChatMessages` (TS lines 780-872).
#[allow(clippy::too_many_lines)] // mirrors the TS role switch; split when extending
fn to_chat_messages(messages: &[Message], supports_images: bool) -> Vec<Value> {
    let mut result = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        match message {
            Message::System(system) => {
                let text = if index == 0 {
                    system_text(system)
                } else {
                    system_update_text(system)
                };
                if !text.is_empty() {
                    result.push(json!({ "role": "system", "content": text }));
                }
            }
            Message::User(user) => match &user.content {
                crate::types::UserContent::Text(text) => {
                    result.push(json!({ "role": "user", "content": text }));
                }
                crate::types::UserContent::Blocks(blocks) => {
                    let had_images = blocks
                        .iter()
                        .any(|block| matches!(block, crate::types::InputContent::Image(_)));
                    let content: Vec<Value> = blocks
                        .iter()
                        .filter_map(|block| match block {
                            crate::types::InputContent::Text(text) => Some(json!({
                                "type": "text",
                                "text": text.text,
                            })),
                            crate::types::InputContent::Image(image) if supports_images => {
                                Some(json!({
                                    "type": "image_url",
                                    "imageUrl": format!("data:{};base64,{}", image.mime_type, image.data),
                                }))
                            }
                            crate::types::InputContent::Image(_) => None,
                        })
                        .collect();
                    if !content.is_empty() {
                        result.push(json!({ "role": "user", "content": content }));
                    } else if had_images {
                        result.push(json!({
                            "role": "user",
                            "content": "(image omitted: model does not support images)",
                        }));
                    }
                }
            },
            Message::Assistant(assistant) => {
                let mut content_parts = Vec::new();
                let mut tool_calls = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => {
                            if !text.text.trim().is_empty() {
                                content_parts.push(json!({"type": "text", "text": text.text}));
                            }
                        }
                        AssistantContent::Thinking(thinking) => {
                            if !thinking.thinking.trim().is_empty() {
                                content_parts.push(json!({
                                    "type": "thinking",
                                    "thinking": [{"type": "text", "text": thinking.thinking}],
                                }));
                            }
                        }
                        AssistantContent::ToolCall(call) => {
                            tool_calls.push(json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": serde_json::to_string(&call.arguments)
                                        .unwrap_or_else(|_| "{}".to_string()),
                                },
                                "index": 0,
                            }));
                        }
                    }
                }
                let mut message = json!({ "role": "assistant", "prefix": false });
                if !content_parts.is_empty() {
                    message["content"] = Value::Array(content_parts);
                }
                if !tool_calls.is_empty() {
                    message["toolCalls"] = Value::Array(tool_calls);
                }
                if message.get("content").is_some() || message.get("toolCalls").is_some() {
                    result.push(message);
                }
            }
            Message::ToolResult(tool_result) => {
                let text = tool_result
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        crate::types::InputContent::Text(text) => Some(text.text.clone()),
                        crate::types::InputContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let has_images = tool_result
                    .content
                    .iter()
                    .any(|part| matches!(part, crate::types::InputContent::Image(_)));
                let tool_text = build_tool_result_text(
                    &text,
                    has_images,
                    supports_images,
                    tool_result.is_error,
                );
                let mut content = vec![json!({ "type": "text", "text": tool_text })];
                if supports_images {
                    for part in &tool_result.content {
                        if let crate::types::InputContent::Image(image) = part {
                            content.push(json!({
                                "type": "image_url",
                                "imageUrl": format!("data:{};base64,{}", image.mime_type, image.data),
                            }));
                        }
                    }
                }
                result.push(json!({
                    "role": "tool",
                    "toolCallId": tool_result.tool_call_id,
                    "name": tool_result.tool_name,
                    "content": content,
                }));
            }
        }
    }
    result
}

/// Strict-sampling resolution (TS `api/constrained-sampling.ts:246-270`).
/// The Rust port's `ConstrainedSampling` has no schema transformer, so strict
/// mode is only advertised when the schema is a plain object without the
/// unsupported keywords; the schema is passed through unchanged.
fn resolve_strict_sampling(tool: &crate::types::Tool) -> Option<bool> {
    let config = tool.constrained_sampling.as_ref()?;
    if config.type_name() != "json_schema" {
        return None;
    }
    let unsupported = [
        "$ref",
        "$defs",
        "definitions",
        "allOf",
        "oneOf",
        "patternProperties",
        "dependentSchemas",
        "dependencies",
        "unevaluatedProperties",
        "propertyNames",
        "contains",
        "prefixItems",
        "not",
        "if",
        "then",
        "else",
    ];
    let compatible = tool
        .parameters
        .as_object()
        .is_some_and(|schema| schema.get("type").and_then(Value::as_str) == Some("object"))
        && !unsupported
            .iter()
            .any(|key| tool.parameters.get(key).is_some());
    if compatible { Some(true) } else { None }
}

fn to_function_tools(tools: &[crate::types::Tool]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let strict = resolve_strict_sampling(tool).unwrap_or(false);
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                    "strict": strict,
                },
            })
        })
        .collect()
}

fn tool_choice(options: &StreamOptions) -> Option<Value> {
    if let Some(raw) = options.metadata.as_ref().and_then(|m| m.get("toolChoice")) {
        return Some(raw.clone());
    }
    match options.tool_choice {
        Some(crate::types::ToolChoice::Auto) => Some(json!("auto")),
        Some(crate::types::ToolChoice::None) => Some(json!("none")),
        None => None,
    }
}

/// `buildChatPayload` (TS lines 519-541) plus the reasoning mapping from
/// `streamSimple` (TS lines 198-212).
#[allow(clippy::too_many_lines)] // mirrors the TS option list; split when extending
fn build_chat_payload(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
) -> Value {
    let transformed = transform_messages(&context.messages, model);
    let messages = to_chat_messages(
        &transformed,
        model.input.contains(&ModelInputModality::Image),
    );
    let mut payload = json!({
        "model": model.id,
        "stream": true,
        "messages": messages,
    });
    let tools = tools_at_point(&context.messages);
    if !tools.is_empty() {
        payload["tools"] = Value::Array(to_function_tools(&tools));
    }
    if let Some(temperature) = options.temperature {
        payload["temperature"] = json!(temperature);
    }
    if let Some(max_tokens) = options.max_tokens {
        payload["maxTokens"] = json!(max_tokens.min(model.max_tokens));
    }
    if let Some(choice) = tool_choice(options) {
        payload["toolChoice"] = choice;
    }
    if let Some(raw) = options
        .metadata
        .as_ref()
        .and_then(|m| m.get("responseFormat"))
    {
        payload["responseFormat"] = raw.clone();
    }
    if let Some(raw) = options
        .metadata
        .as_ref()
        .and_then(|m| m.get("reasoningEffort"))
    {
        payload["reasoningEffort"] = raw.clone();
    } else if let Some(level) = options.reasoning
        && let Some(map) = &model.thinking_level_map
    {
        let key = match level {
            crate::types::ThinkingLevel::Off => "off",
            crate::types::ThinkingLevel::Minimal => "minimal",
            crate::types::ThinkingLevel::Low => "low",
            crate::types::ThinkingLevel::Medium => "medium",
            crate::types::ThinkingLevel::High => "high",
        };
        if let Some(effort) = map.get(key).filter(|value| !value.is_null()) {
            payload["reasoningEffort"] = effort.clone();
        } else if let Some(effort) = map.get("off").filter(|value| !value.is_null()) {
            payload["reasoningEffort"] = effort.clone();
        }
    }
    if let Some(raw) = options.metadata.as_ref().and_then(|m| m.get("promptMode")) {
        payload["promptMode"] = raw.clone();
    } else if model.reasoning
        && model.thinking_level_map.is_none()
        && options
            .reasoning
            .is_some_and(|level| level != crate::types::ThinkingLevel::Off)
    {
        payload["promptMode"] = json!("reasoning");
    }
    if should_use_prompt_caching(options)
        && let Some(session_id) = &options.session_id
    {
        payload["promptCacheKey"] = json!(session_id);
    }
    payload
}

/// `new URL("v1/chat/completions", baseUrl)` (TS lines 296-298).
fn completion_url(base_url: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(base_url).map_err(|e| format!("invalid base URL: {e}"))?;
    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(&format!("{path}/"));
    url.join("v1/chat/completions")
        .map(|url| url.to_string())
        .map_err(|e| format!("invalid base URL: {e}"))
}

fn has_header_override(headers: &BTreeMap<String, Option<String>>, target: &str) -> bool {
    headers.keys().any(|name| name.eq_ignore_ascii_case(target))
}

fn has_model_header_override(headers: Option<&BTreeMap<String, String>>, target: &str) -> bool {
    headers.is_some_and(|headers| headers.keys().any(|name| name.eq_ignore_ascii_case(target)))
}

/// `buildMistralHeaders` (TS lines 347-376).
fn build_request(
    client: &reqwest::Client,
    url: &str,
    model: &Model,
    options: &StreamOptions,
    api_key: &str,
    body: &Value,
) -> reqwest::RequestBuilder {
    let mut request = client
        .post(url)
        .header("accept", "text/event-stream")
        .header("authorization", format!("Bearer {api_key}"))
        .header("content-type", "application/json")
        .json(body);
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            request = request.header(name, value);
        }
    }
    for (name, value) in &options.headers {
        if let Some(value) = value {
            request = request.header(name, value);
        }
    }
    if should_use_prompt_caching(options)
        && let Some(session_id) = &options.session_id
        && !has_model_header_override(model.headers.as_ref(), "x-affinity")
        && !has_header_override(&options.headers, "x-affinity")
    {
        request = request.header("x-affinity", session_id);
    }
    request
}

/// API key resolution (TS line 138, `getApiKey("API_KEY", provider)`).
fn api_key(model: &Model, options: &StreamOptions) -> Option<String> {
    options
        .api_key
        .clone()
        .or_else(|| options.access_token.clone())
        .or_else(|| {
            let name = format!(
                "{}_API_KEY",
                model
                    .provider
                    .to_uppercase()
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                    .collect::<String>()
            );
            std::env::var(name).ok()
        })
}

/// `formatMistralError` (TS lines 267-288) for already-formatted string errors.
fn format_mistral_error(message: &str) -> String {
    if message.is_empty() {
        "An unknown error occurred".to_string()
    } else {
        message.to_string()
    }
}

/// Public stream entry point (TS lines 126-179).
pub fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
) -> AssistantMessageEventStream {
    let (stream_out, sender) = AssistantMessageEventStream::channel();
    let (model, context, options) = (model.clone(), context.clone(), options.clone());
    tokio::spawn(async move {
        run(&model, &context, &options, sender).await;
    });
    stream_out
}

async fn run(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
    sender: EventSender,
) {
    let mut output = initial_output(model);
    let result = request_and_stream(model, context, options, &mut output, &sender).await;
    if let Err(message) = result {
        output.stop_reason = StopReason::Error;
        output.error_message = Some(format_mistral_error(&message));
        let _ = emit(
            &sender,
            AssistantMessageEvent::Error {
                reason: StopReason::Error,
                error: output,
            },
        )
        .await;
    }
}

async fn request_and_stream(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
    output: &mut AssistantMessage,
    sender: &EventSender,
) -> Result<(), String> {
    let key = api_key(model, options)
        .ok_or_else(|| format!("No API key for provider: {}", model.provider))?;
    let payload = build_chat_payload(model, context, options);
    let payload = apply_payload_hook(&options.on_payload, payload, model).await?;
    let wire_payload = to_mistral_wire_payload(&payload);
    let url = completion_url(&model.base_url)?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| format!("client build failed: {e}"))?;
    let response = build_request(&client, &url, model, options, &key, &wire_payload)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    apply_response_hook(&options.on_response, response_metadata(&response), model).await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let trimmed = body.trim();
        let excerpt: String = trimmed.chars().take(MAX_MISTRAL_ERROR_BODY_CHARS).collect();
        let message = if !excerpt.is_empty() {
            format!("Mistral API error ({}): {excerpt}", status.as_u16())
        } else {
            format!(
                "Mistral API error ({}): {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("Request failed")
            )
        };
        return Err(message);
    }

    let sse = read_sse_stream(response).await.map_err(|e| e.to_string())?;
    emit(
        sender,
        AssistantMessageEvent::Start {
            partial: output.clone(),
        },
    )
    .await?;
    consume_chat_stream(sse, model, options, output, sender).await?;

    if output.stop_reason == StopReason::Pending {
        return Err("Mistral stream ended without a finish reason".to_string());
    }
    if matches!(output.stop_reason, StopReason::Error | StopReason::Aborted) {
        return Err(output
            .error_message
            .clone()
            .unwrap_or_else(|| "An unknown error occurred".to_string()));
    }
    emit(
        sender,
        AssistantMessageEvent::Done {
            reason: output.stop_reason,
            message: output.clone(),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelCost, StringOrTextBlocks, SystemMessage, UserContent, UserMessage};

    fn test_model() -> Model {
        Model {
            id: "mistral-test".into(),
            name: "Mistral Test".into(),
            api: "mistral-conversations".into(),
            provider: "mistral".into(),
            base_url: "https://api.mistral.ai".into(),
            input: vec![ModelInputModality::Text],
            cost: ModelCost {
                input: 1.0,
                output: 3.0,
                ..Default::default()
            },
            context_window: 32_768,
            max_tokens: 4096,
            ..Model::default()
        }
    }

    fn sse_messages(chunks: &[&[u8]]) -> Vec<SseMessage> {
        let mut parser = crate::sse::SseParser::default();
        let mut messages = Vec::new();
        for chunk in chunks {
            messages.extend(parser.feed(chunk));
        }
        messages.extend(parser.finish());
        messages
    }

    /// Drive a canned SSE stream through the same post-processing as `run`.
    async fn drive(chunks: &[&[u8]]) -> Vec<AssistantMessageEvent> {
        let model = test_model();
        let options = StreamOptions::default();
        let (stream, sender) = AssistantMessageEventStream::channel();
        let mut output = initial_output(&model);
        let messages = sse_messages(chunks);
        let sse = futures::stream::iter(messages.into_iter().map(Ok::<_, SseError>));
        emit(
            &sender,
            AssistantMessageEvent::Start {
                partial: output.clone(),
            },
        )
        .await
        .unwrap();
        let result = consume_chat_stream(sse, &model, &options, &mut output, &sender).await;
        let failure = match result {
            Err(error) => Some(error),
            Ok(()) if output.stop_reason == StopReason::Pending => {
                Some("Mistral stream ended without a finish reason".to_string())
            }
            Ok(()) if matches!(output.stop_reason, StopReason::Error | StopReason::Aborted) => {
                Some(
                    output
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "An unknown error occurred".to_string()),
                )
            }
            Ok(()) => None,
        };
        if let Some(message) = failure {
            output.stop_reason = StopReason::Error;
            output.error_message = Some(message);
            let _ = emit(
                &sender,
                AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: output,
                },
            )
            .await;
        } else {
            let _ = emit(
                &sender,
                AssistantMessageEvent::Done {
                    reason: output.stop_reason,
                    message: output,
                },
            )
            .await;
        }
        drop(sender);
        futures::StreamExt::collect(stream).await
    }

    fn kind(event: &AssistantMessageEvent) -> &'static str {
        match event {
            AssistantMessageEvent::Start { .. } => "start",
            AssistantMessageEvent::TextStart { .. } => "text_start",
            AssistantMessageEvent::TextDelta { .. } => "text_delta",
            AssistantMessageEvent::TextEnd { .. } => "text_end",
            AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
            AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
            AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
            AssistantMessageEvent::ToolcallStart { .. } => "toolcall_start",
            AssistantMessageEvent::ToolcallDelta { .. } => "toolcall_delta",
            AssistantMessageEvent::ToolcallEnd { .. } => "toolcall_end",
            AssistantMessageEvent::Done { .. } => "done",
            AssistantMessageEvent::Error { .. } => "error",
        }
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(map_chat_stop_reason("stop").0, StopReason::Stop);
        assert_eq!(map_chat_stop_reason("length").0, StopReason::Length);
        assert_eq!(map_chat_stop_reason("model_length").0, StopReason::Length);
        assert_eq!(map_chat_stop_reason("tool_calls").0, StopReason::ToolUse);
        assert_eq!(map_chat_stop_reason("error").0, StopReason::Error);
        assert_eq!(
            map_chat_stop_reason("error").1.as_deref(),
            Some("Provider stopped with: error (server error)")
        );
        assert_eq!(
            map_chat_stop_reason("other").1.as_deref(),
            Some("Provider stopped with: other")
        );
    }

    #[test]
    fn tool_call_id_is_nine_alphanumeric_chars() {
        let mut normalizer = ToolCallIdNormalizer::default();
        let id = normalizer.normalize("call-with-dashes-and-underscores");
        assert!(!id.is_empty() && id.len() <= 9, "id: {id}");
        assert!(id.chars().all(char::is_alphanumeric));
        assert_eq!(normalizer.normalize("call-with-dashes-and-underscores"), id);
        assert_eq!(normalizer.normalize("abcdefghi"), "abcdefghi");
        // A collision on the first candidate bumps the attempt suffix.
        let other = normalizer.normalize("abcdefghi-2");
        assert_ne!(other, "abcdefghi");
    }

    #[test]
    fn payload_remaps_to_mistral_wire_keys() {
        let model = test_model();
        let context = TranscriptContext {
            messages: vec![
                Message::System(SystemMessage {
                    content: StringOrTextBlocks::Text("Be terse.".into()),
                    sections: None,
                    tools_added: None,
                    tools_removed: None,
                    timestamp: 1,
                }),
                Message::User(UserMessage {
                    content: UserContent::Text("hi".into()),
                    timestamp: 2,
                }),
            ],
        };
        let options = StreamOptions {
            max_tokens: Some(128),
            session_id: Some("session-1".into()),
            ..StreamOptions::default()
        };
        let payload = build_chat_payload(&model, &context, &options);
        assert_eq!(payload["maxTokens"], 128);
        assert_eq!(payload["promptCacheKey"], "session-1");
        let wire = to_mistral_wire_payload(&payload);
        assert_eq!(wire["max_tokens"], 128);
        assert!(wire.get("maxTokens").is_none());
        assert_eq!(wire["prompt_cache_key"], "session-1");
        assert_eq!(wire["messages"][0]["role"], "system");
        assert_eq!(wire["messages"][1]["content"], "hi");
        assert_eq!(
            completion_url(&model.base_url).unwrap(),
            "https://api.mistral.ai/v1/chat/completions"
        );
    }

    #[test]
    fn assistant_replay_uses_thinking_and_tool_call_shape() {
        let model = test_model();
        let mut assistant = initial_output(&model);
        assistant.content = vec![
            AssistantContent::Thinking(crate::types::ThinkingContent {
                thinking: "hmm".into(),
                thinking_signature: None,
                redacted: false,
            }),
            AssistantContent::ToolCall(ToolCall {
                id: "abcdefghi".into(),
                name: "bash".into(),
                arguments: json!({"cmd": "ls"}).as_object().cloned().unwrap(),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            }),
        ];
        let messages = vec![Message::Assistant(assistant)];
        let wire = to_mistral_wire_payload(&json!({
            "messages": to_chat_messages(&messages, false),
        }));
        assert_eq!(wire["messages"][0]["content"][0]["type"], "thinking");
        assert_eq!(
            wire["messages"][0]["content"][0]["thinking"][0]["text"],
            "hmm"
        );
        assert_eq!(wire["messages"][0]["tool_calls"][0]["id"], "abcdefghi");
        let arguments = wire["messages"][0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        assert_eq!(arguments, "{\"cmd\":\"ls\"}");
    }

    #[tokio::test]
    async fn canned_text_and_thinking_stream() {
        let events = drive(&[
            b"data: {\"id\":\"resp-1\",\"choices\":[{\"delta\":{\"content\":[{\"type\":\"thinking\",\"thinking\":[{\"text\":\"why\"}]}]}}]}\n\n",
            b"data: {\"choices\":[{\"delta\":{\"content\":[{\"type\":\"text\",\"text\":\"Hello\"}]}}]}\n\n",
            b"data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3,\"total_tokens\":15,\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n",
            b"data: [DONE]\n\n",
        ])
        .await;
        let kinds: Vec<&str> = events.iter().map(kind).collect();
        assert_eq!(
            kinds,
            vec![
                "start",
                "thinking_start",
                "thinking_delta",
                "thinking_end",
                "text_start",
                "text_delta",
                "text_delta",
                "text_end",
                "done"
            ]
        );
        let AssistantMessageEvent::Done { reason, message } = events.last().unwrap() else {
            panic!("expected done")
        };
        assert_eq!(*reason, StopReason::Stop);
        assert_eq!(message.response_id.as_deref(), Some("resp-1"));
        assert_eq!(message.content.len(), 2);
        assert_eq!(message.content[0].as_text(), None);
        assert_eq!(message.content[1].as_text(), Some("Hello world"));
        assert_eq!(message.usage.input, 8);
        assert_eq!(message.usage.cache_read, 4);
        assert_eq!(message.usage.output, 3);
        assert_eq!(message.usage.total_tokens, 15);
    }

    #[tokio::test]
    async fn canned_tool_call_stream() {
        let events = drive(&[
            b"data: {\"choices\":[{\"delta\":{\"content\":null,\"tool_calls\":[{\"index\":0,\"id\":\"tc_1\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"cmd\\\":\"}}]}}]}\n\n",
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ls\\\"}\"}}]}}]}\n\n",
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            b"data: [DONE]\n\n",
        ])
        .await;
        let kinds: Vec<&str> = events.iter().map(kind).collect();
        assert_eq!(
            kinds,
            vec![
                "start",
                "toolcall_start",
                "toolcall_delta",
                "toolcall_delta",
                "toolcall_end",
                "done"
            ]
        );
        let AssistantMessageEvent::Done { reason, message } = events.last().unwrap() else {
            panic!("expected done")
        };
        assert_eq!(*reason, StopReason::ToolUse);
        let AssistantContent::ToolCall(call) = &message.content[0] else {
            panic!("expected tool call")
        };
        assert_eq!(call.id, "tc_1");
        assert_eq!(call.name, "bash");
        assert_eq!(call.arguments["cmd"], "ls");
        assert!(call.arguments_error.is_none());
    }

    #[tokio::test]
    async fn done_sentinel_stops_reading_and_missing_finish_reason_errors() {
        let events = drive(&[
            b"data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n",
            b"data: [DONE]\n\n",
            b"data: {\"choices\":[{\"delta\":{\"content\":\"ignored\"},\"finish_reason\":\"stop\"}]}\n\n",
        ])
        .await;
        let AssistantMessageEvent::Error { error, .. } = events.last().unwrap() else {
            panic!("expected error")
        };
        assert_eq!(
            error.error_message.as_deref(),
            Some("Mistral stream ended without a finish reason")
        );
        assert_eq!(error.content.len(), 1);
        assert_eq!(error.content[0].as_text(), Some("first"));
    }

    #[tokio::test]
    async fn provider_error_finish_reason_becomes_error_event() {
        let events =
            drive(&[b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"error\"}]}\n\n"]).await;
        let AssistantMessageEvent::Error { error, .. } = events.last().unwrap() else {
            panic!("expected error")
        };
        assert_eq!(
            error.error_message.as_deref(),
            Some("Provider stopped with: error (server error)")
        );
    }

    #[test]
    fn cached_prompt_tokens_are_clamped_and_usage_is_priced() {
        let mut output = initial_output(&test_model());
        apply_usage(
            &json!({
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 0,
                "prompt_tokens_details": {"cached_tokens": 99},
            }),
            &mut output,
            &test_model(),
        );
        assert_eq!(output.usage.input, 0);
        assert_eq!(output.usage.cache_read, 10);
        assert_eq!(output.usage.output, 5);
        assert_eq!(output.usage.total_tokens, 15);
        assert!(output.usage.cost.total > 0.0);
    }

    #[test]
    fn tool_result_text_variants() {
        assert_eq!(build_tool_result_text(" ok ", false, false, false), "ok");
        assert_eq!(
            build_tool_result_text("", false, false, true),
            "[tool error] (no tool output)"
        );
        assert_eq!(
            build_tool_result_text("", true, false, false),
            "(image omitted: model does not support images)"
        );
        assert_eq!(
            build_tool_result_text("x", true, false, true),
            "[tool error] x\n[tool image omitted: model does not support images]"
        );
    }
}
