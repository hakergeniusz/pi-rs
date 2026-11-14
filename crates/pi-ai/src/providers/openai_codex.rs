//! `OpenAI` Codex (`ChatGPT` backend) adapter — port of TS
//! `api/openai-codex-responses.ts` + `api/openai-responses-shared.ts`.
//!
//! Scope: the HTTP SSE transport (`POST {baseUrl}/codex/responses`,
//! `stream: true`) with the full `ResponseStreamEvent` →
//! [`AssistantMessageEvent`] mapping: reasoning summaries, text, function and
//! custom (grammar) tool calls, usage/stop-reason mapping, reasoning-replay
//! signatures, and `store: false` multi-turn replay state.
//!
//! Documented deviations from upstream:
//! - Transport is SSE only. Upstream prefers a WebSocket transport and falls
//!   back to this same SSE endpoint on failure; events are identical.
//! - No zstd request compression (an optional bandwidth optimization).
//! - No retry/backoff loop: upstream's default is `maxRetries: 0` too.
//! - Mid-transcript tool *additions* (`additional_tools` / synthetic tool
//!   search items) are not re-declared; pi's print/RPC paths only declare
//!   tools in the leading system message, so the transcript never carries them.
//! - `sanitizeSurrogates` is a no-op: Rust strings are always valid UTF-8.
//! - Grammar-tool replay uses the default `input` property and raw text
//!   accumulation instead of upstream's JSON-delta stitching buffer.
//! - No service-tier pricing (`serviceTier` is not on `StreamOptions`).

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL;
use futures::StreamExt;
use serde_json::{Value, json};

use crate::event_stream::{AssistantMessageEventStream, EventSender};
use crate::providers::common::{
    finalize, initial_output, is_leading_system, system_text, system_update_text, tools_at_point,
};
use crate::sse::{SseError, read_sse_stream};
use crate::streaming_json::parse_tool_call_arguments;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, InputContent, Message, Model,
    StopReason, StreamOptions, ThinkingLevel, ToolCall, TranscriptContext, UserContent,
};

/// JWT claim path holding the `ChatGPT` account id (`JWT_CLAIM_PATH`).
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
/// Fallback instructions (`buildRequestBody`).
const DEFAULT_INSTRUCTIONS: &str = "You are a helpful assistant.";
/// Providers allowed to replay `callId|itemId` tool-call ids verbatim
/// (`CODEX_TOOL_CALL_PROVIDERS`).
const TOOL_CALL_PROVIDERS: [&str; 3] = ["openai", "openai-codex", "opencode"];
/// Upstream's SSE-beta marker (`buildSSEHeaders`).
const OPENAI_BETA: &str = "responses=experimental";

type StreamResult<T> = Result<T, String>;

/// Send the request and stream protocol events (`StreamFn` contract: failures
/// surface as `error` events).
pub fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
) -> AssistantMessageEventStream {
    let (stream_out, sender) = AssistantMessageEventStream::channel();
    let model = model.clone();
    let context = context.clone();
    let options = options.clone();
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
    finalize(&mut output, model);
    match result {
        Ok(()) => {
            if matches!(output.stop_reason, StopReason::Error | StopReason::Aborted) {
                let _ = sender
                    .send(AssistantMessageEvent::Error {
                        reason: output.stop_reason,
                        error: output,
                    })
                    .await;
            } else {
                let _ = sender
                    .send(AssistantMessageEvent::Done {
                        reason: output.stop_reason,
                        message: output,
                    })
                    .await;
            }
        }
        Err(message) => {
            output.stop_reason = StopReason::Error;
            output.error_message = Some(message);
            let _ = sender
                .send(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: output,
                })
                .await;
        }
    }
}

/// Extract the `ChatGPT` account id from the OAuth access token's JWT payload
/// (`extractAccountId`): any failure is upstream's single error message.
fn extract_account_id(token: &str) -> StreamResult<String> {
    let failure = || "Failed to extract accountId from token".to_string();
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(failure());
    }
    let payload = BASE64_URL.decode(parts[1]).map_err(|_| failure())?;
    let payload: Value = serde_json::from_slice(&payload).map_err(|_| failure())?;
    payload
        .get(JWT_CLAIM_PATH)
        .and_then(|claim| claim.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(failure)
}

/// `resolveCodexUrl`: normalize the base URL onto the `/codex/responses` path.
fn resolve_codex_url(base_url: &str) -> String {
    let raw = if base_url.trim().is_empty() {
        "https://chatgpt.com/backend-api"
    } else {
        base_url
    };
    let normalized = raw.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_string()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

/// Read a boolean `model.compat` flag with the given default.
fn compat_flag(model: &Model, key: &str, default: bool) -> bool {
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

/// `normalizeIdPart`: sanitize to `[a-zA-Z0-9_-]`, cap at 64, strip trailing `_`.
fn normalize_id_part(part: &str) -> String {
    let sanitized: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut normalized: String = sanitized.chars().take(64).collect();
    while normalized.ends_with('_') {
        normalized.pop();
    }
    normalized
}

/// Stable short hash standing in for upstream's `shortHash` (FNV-1a 64, hex).
/// The provider treats these ids as opaque.
fn short_hash(input: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// `clampOpenAIPromptCacheKey`: sanitize a session id into a prompt-cache key.
fn clamp_prompt_cache_key(session_id: &str) -> String {
    normalize_id_part(session_id)
}

/// `normalizeToolCallId` for replayed assistant tool calls.
fn normalize_tool_call_id(id: &str, model: &Model, source: &AssistantMessage) -> String {
    if !TOOL_CALL_PROVIDERS.contains(&model.provider.as_str()) {
        return normalize_id_part(id);
    }
    if !id.contains('|') {
        return normalize_id_part(id);
    }
    let (call_id, item_id) = id.split_once('|').unwrap_or((id, ""));
    let normalized_call_id = normalize_id_part(call_id);
    let is_foreign = source.provider != model.provider || source.api != model.api;
    let mut normalized_item_id = if is_foreign {
        format!("fc_{}", short_hash(item_id))
    } else {
        normalize_id_part(item_id)
    };
    // The Responses API requires the item id to start with "fc".
    if !normalized_item_id.starts_with("fc_") {
        normalized_item_id = normalize_id_part(&format!("fc_{normalized_item_id}"));
    }
    format!("{normalized_call_id}|{normalized_item_id}")
}

/// `parseTextSignature`: `{v:1,id,phase?}` JSON, or a legacy plain id string.
fn parse_text_signature(signature: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(signature) = signature else {
        return (None, None);
    };
    if signature.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<Value>(signature) {
            if parsed.get("v").and_then(Value::as_u64) == Some(1) {
                if let Some(id) = parsed.get("id").and_then(Value::as_str) {
                    let phase = parsed.get("phase").and_then(Value::as_str);
                    let phase = phase
                        .filter(|p| *p == "commentary" || *p == "final_answer")
                        .map(str::to_string);
                    return (Some(id.to_string()), phase);
                }
            }
        }
    }
    (Some(signature.to_string()), None)
}

/// `encodeTextSignatureV1`.
fn encode_text_signature(id: &str, phase: Option<&str>) -> String {
    let mut payload = json!({ "v": 1, "id": id });
    if let Some(phase) = phase {
        payload["phase"] = json!(phase);
    }
    payload.to_string()
}

/// The tool-input property grammar tools stream into (`customInput` default).
const GRAMMAR_INPUT_PROPERTY: &str = "input";

/// Grammar-tool opt-in for a tool, when the model supports grammar tools.
fn grammar_definition(model: &Model, tool: &crate::types::Tool) -> Option<(&'static str, String)> {
    if !compat_flag(model, "supportsOpenAIGrammarTools", false) {
        return None;
    }
    match tool.constrained_sampling.as_ref()? {
        crate::constrained_sampling::ConstrainedSampling::Grammar { variants } => variants
            .openai_lark
            .as_ref()
            .map(|definition| ("lark", definition.clone())),
        _ => None,
    }
}

/// `convertResponsesTools`: function tools (`strict: null` by default, like
/// the codex path) plus grammar opt-ins as `custom` tools.
fn convert_responses_tools(model: &Model, tools: &[crate::types::Tool]) -> Vec<Value> {
    let supports_strict = compat_flag(model, "supportsStrictMode", true);
    tools
        .iter()
        .map(|tool| {
            if let Some((syntax, definition)) = grammar_definition(model, tool) {
                return json!({
                    "type": "custom",
                    "name": tool.name,
                    "description": tool.description,
                    "format": { "type": "grammar", "syntax": syntax, "definition": definition },
                });
            }
            let strict = match tool.constrained_sampling {
                Some(crate::constrained_sampling::ConstrainedSampling::JsonSchema {
                    strict: crate::constrained_sampling::StrictMode::Require,
                }) if supports_strict => json!(true),
                _ => json!(null),
            };
            let mut function = json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            });
            if supports_strict {
                function["strict"] = strict;
            }
            function
        })
        .collect()
}

/// `convertResponsesMessages` (codex options: leading system becomes
/// `instructions`, reasoning models use the `developer` role).
#[allow(clippy::too_many_lines)] // mirrors the TS message-conversion shape
fn convert_responses_messages(model: &Model, context: &TranscriptContext) -> Vec<Value> {
    let messages = &context.messages;
    let instruction_role = if model.reasoning && compat_flag(model, "supportsDeveloperRole", true) {
        "developer"
    } else {
        "system"
    };
    let model_input_images = model
        .input
        .iter()
        .any(|modality| matches!(modality, crate::types::ModelInputModality::Image));

    let mut input: Vec<Value> = Vec::new();
    let mut msg_index = 0usize;
    let mut text_block_counts: HashMap<usize, usize> = HashMap::new();
    for (i, msg) in messages.iter().enumerate() {
        let leading_system = is_leading_system(messages, i);
        match msg {
            Message::System(system) => {
                // The leading system message rides in `instructions`
                // (`includeSystemPrompt: false`).
                if leading_system {
                    continue;
                }
                let text = system_update_text(system);
                if !text.is_empty() {
                    input.push(json!({ "role": instruction_role, "content": text }));
                }
            }
            Message::User(user) => match &user.content {
                UserContent::Text(text) => {
                    if !text.trim().is_empty() {
                        input.push(json!({
                            "role": "user",
                            "content": [{ "type": "input_text", "text": text }],
                        }));
                    }
                }
                UserContent::Blocks(blocks) => {
                    let blocks_json: Vec<Value> = blocks
                        .iter()
                        .map(|entry| match entry {
                            InputContent::Text(text) => json!({
                                "type": "input_text",
                                "text": text.text,
                            }),
                            InputContent::Image(image) => json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!("data:{};base64,{}", image.mime_type, image.data),
                            }),
                        })
                        .collect();
                    if blocks_json.is_empty() {
                        continue;
                    }
                    input.push(json!({ "role": "user", "content": blocks_json }));
                }
            },
            Message::Assistant(assistant) => {
                let is_same_provider_and_api =
                    assistant.provider == model.provider && assistant.api == model.api;
                let is_same_model = is_same_provider_and_api && assistant.model == model.id;
                let is_different_model = is_same_provider_and_api && assistant.model != model.id;
                let mut output: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Thinking(thinking) => {
                            // Replay the serialized `ResponseReasoningItem` verbatim.
                            if let Some(signature) = &thinking.thinking_signature {
                                if let Ok(item) = serde_json::from_str::<Value>(signature) {
                                    output.push(item);
                                }
                            }
                        }
                        AssistantContent::Text(text) => {
                            let count = text_block_counts.entry(msg_index).or_default();
                            let fallback = if *count == 0 {
                                format!("msg_pi_{msg_index}")
                            } else {
                                format!("msg_pi_{msg_index}_{count}")
                            };
                            *count += 1;
                            let (signature_id, phase) =
                                parse_text_signature(text.text_signature.as_deref());
                            let mut id = signature_id.unwrap_or(fallback);
                            if id.chars().count() > 64 {
                                id = format!("msg_{}", short_hash(&id));
                            }
                            let mut item = json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{ "type": "output_text", "text": text.text, "annotations": [] }],
                                "status": "completed",
                                "id": id,
                            });
                            if let Some(phase) = &phase {
                                item["phase"] = json!(phase);
                            }
                            output.push(item);
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            let call_id = tool_call.id.split('|').next().unwrap_or("");
                            let normalized =
                                normalize_tool_call_id(&tool_call.id, model, assistant);
                            let item_id =
                                normalized.split_once('|').map(|(_, item)| item.to_string());
                            let prefix = "fc_";
                            // Different-model messages drop the item id to avoid
                            // pairing validation; ids must match the replayed type.
                            let id = if is_different_model
                                || !item_id.as_deref().unwrap_or("").starts_with(prefix)
                            {
                                None
                            } else {
                                item_id.clone()
                            };
                            let mut item = json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": tool_call.name,
                                "arguments": serde_json::to_string(&tool_call.arguments)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            });
                            if let Some(id) = id {
                                item["id"] = json!(id);
                            }
                            if is_same_model {
                                if let Some(namespace) = &tool_call.namespace {
                                    item["namespace"] = json!(namespace);
                                }
                            }
                            output.push(item);
                        }
                    }
                }
                if output.is_empty() {
                    continue;
                }
                input.extend(output);
            }
            Message::ToolResult(result) => {
                let call_id = result
                    .tool_call_id
                    .split('|')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let text: String = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        InputContent::Text(text) => Some(text.text.as_str()),
                        InputContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let images: Vec<&crate::types::ImageContent> = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        InputContent::Image(image) => Some(image),
                        InputContent::Text(_) => None,
                    })
                    .collect();
                let has_text = !text.trim().is_empty();
                let output = if images.is_empty() || !model_input_images {
                    json!(if has_text {
                        text
                    } else if !images.is_empty() {
                        "(see attached image)".to_string()
                    } else {
                        "(no tool output)".to_string()
                    })
                } else {
                    let mut parts: Vec<Value> = Vec::new();
                    if has_text {
                        parts.push(json!({ "type": "input_text", "text": text }));
                    }
                    for image in images {
                        parts.push(json!({
                            "type": "input_image",
                            "detail": "auto",
                            "image_url": format!("data:{};base64,{}", image.mime_type, image.data),
                        }));
                    }
                    json!(parts)
                };
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                }));
            }
        }
        if !leading_system {
            msg_index += 1;
        }
    }
    input
}

/// `mapStopReason`: status (+ incomplete reason) → pi stop reason.
fn map_stop_reason(
    status: Option<&str>,
    incomplete_reason: Option<&str>,
) -> (StopReason, Option<String>) {
    match status {
        None | Some("completed" | "in_progress" | "queued") => (StopReason::Stop, None),
        Some("incomplete") => match incomplete_reason {
            Some("max_output_tokens") => (StopReason::Length, None),
            other => (
                StopReason::Error,
                Some(match other {
                    Some(reason) => format!("Response incomplete: {reason}"),
                    None => "Response incomplete without a provider reason".to_string(),
                }),
            ),
        },
        Some("failed" | "cancelled") => (StopReason::Error, None),
        Some(other) => (
            StopReason::Error,
            Some(format!("Unhandled stop reason: {other}")),
        ),
    }
}

/// Streaming slots keyed by `output_index`
/// (`ResponsesOutputSlot` + argument buffers).
enum Slot {
    Thinking { index: usize },
    Text { index: usize },
    ToolCall { index: usize, raw_args: String },
}

/// Stream state (`processResponsesStream` locals).
struct StreamState {
    slots: HashMap<u64, Slot>,
    /// Content indices of tool calls whose `output_item.done` has not arrived.
    unfinished: Vec<usize>,
    saw_terminal: bool,
}

impl StreamState {
    fn new() -> Self {
        Self {
            slots: HashMap::new(),
            unfinished: Vec::new(),
            saw_terminal: false,
        }
    }

    /// `applyMessagePhaseStopReason`.
    fn apply_message_phase_stop_reason(item: &Value, output: &mut AssistantMessage) {
        if item.get("type").and_then(Value::as_str) == Some("message")
            && item.get("phase").and_then(Value::as_str) == Some("final_answer")
        {
            output.stop_reason = StopReason::Stop;
        }
    }

    /// `createSlot`/`getOrCreateSlot`: register a block for `output_index`,
    /// emitting its start event.
    async fn create_slot(
        slots: &mut HashMap<u64, Slot>,
        unfinished: &mut Vec<usize>,
        output: &mut AssistantMessage,
        output_index: u64,
        item: &Value,
        sender: &EventSender,
    ) {
        let item_type = item.get("type").and_then(Value::as_str);
        match item_type {
            Some("reasoning") => {
                output
                    .content
                    .push(AssistantContent::Thinking(crate::types::ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: false,
                    }));
                let index = output.content.len() - 1;
                slots.insert(output_index, Slot::Thinking { index });
                let _ = sender
                    .send(AssistantMessageEvent::ThinkingStart {
                        content_index: index,
                        partial: output.clone(),
                    })
                    .await;
            }
            Some("message") => {
                Self::apply_message_phase_stop_reason(item, output);
                output
                    .content
                    .push(AssistantContent::Text(crate::types::TextContent {
                        text: String::new(),
                        text_signature: None,
                    }));
                let index = output.content.len() - 1;
                slots.insert(output_index, Slot::Text { index });
                let _ = sender
                    .send(AssistantMessageEvent::TextStart {
                        content_index: index,
                        partial: output.clone(),
                    })
                    .await;
            }
            Some("function_call" | "custom_tool_call") => {
                let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
                let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                let initial = if item_type == Some("custom_tool_call") {
                    item.get("input").and_then(Value::as_str).unwrap_or("")
                } else {
                    item.get("arguments").and_then(Value::as_str).unwrap_or("")
                };
                let mut arguments = parse_tool_call_arguments(initial).unwrap_or_default();
                if item_type == Some("custom_tool_call") {
                    arguments = serde_json::Map::from_iter([(
                        GRAMMAR_INPUT_PROPERTY.to_string(),
                        json!(initial),
                    )]);
                }
                let block = ToolCall {
                    id: format!("{call_id}|{item_id}"),
                    name: name.to_string(),
                    arguments,
                    thought_signature: None,
                    namespace: item
                        .get("namespace")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    arguments_error: None,
                };
                output.content.push(AssistantContent::ToolCall(block));
                let index = output.content.len() - 1;
                unfinished.push(index);
                slots.insert(
                    output_index,
                    Slot::ToolCall {
                        index,
                        raw_args: initial.to_string(),
                    },
                );
                let _ = sender
                    .send(AssistantMessageEvent::ToolcallStart {
                        content_index: index,
                        partial: output.clone(),
                    })
                    .await;
            }
            _ => {}
        }
    }

    fn slot_mut(slots: &mut HashMap<u64, Slot>, output_index: u64) -> Option<&mut Slot> {
        slots.get_mut(&output_index)
    }

    /// Apply one `ResponseStreamEvent` JSON payload.
    #[allow(clippy::too_many_lines)] // mirrors the TS event-loop shape; split when extending
    async fn apply(
        &mut self,
        event: &Value,
        output: &mut AssistantMessage,
        sender: &EventSender,
    ) -> StreamResult<()> {
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            "response.created" => {
                if let Some(id) = event.pointer("/response/id").and_then(Value::as_str) {
                    output.response_id = Some(id.to_string());
                }
            }
            "response.output_item.added" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if let Some(item) = event.get("item") {
                    Self::create_slot(
                        &mut self.slots,
                        &mut self.unfinished,
                        output,
                        output_index,
                        item,
                        sender,
                    )
                    .await;
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if let Some(Slot::Thinking { index }) =
                    Self::slot_mut(&mut self.slots, output_index)
                {
                    let index = *index;
                    if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                        if let Some(AssistantContent::Thinking(thinking)) =
                            output.content.get_mut(index)
                        {
                            thinking.thinking.push_str(delta);
                        }
                        let _ = sender
                            .send(AssistantMessageEvent::ThinkingDelta {
                                content_index: index,
                                delta: delta.to_string(),
                                partial: output.clone(),
                            })
                            .await;
                    }
                }
            }
            "response.reasoning_summary_part.done" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if let Some(Slot::Thinking { index }) =
                    Self::slot_mut(&mut self.slots, output_index)
                {
                    let index = *index;
                    if let Some(AssistantContent::Thinking(thinking)) =
                        output.content.get_mut(index)
                    {
                        thinking.thinking.push_str("\n\n");
                    }
                    let _ = sender
                        .send(AssistantMessageEvent::ThinkingDelta {
                            content_index: index,
                            delta: "\n\n".to_string(),
                            partial: output.clone(),
                        })
                        .await;
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if let Some(Slot::Text { index }) = Self::slot_mut(&mut self.slots, output_index) {
                    let index = *index;
                    if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                        if let Some(AssistantContent::Text(text)) = output.content.get_mut(index) {
                            text.text.push_str(delta);
                        }
                        let _ = sender
                            .send(AssistantMessageEvent::TextDelta {
                                content_index: index,
                                delta: delta.to_string(),
                                partial: output.clone(),
                            })
                            .await;
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if let Some(Slot::ToolCall { index, raw_args }) =
                    Self::slot_mut(&mut self.slots, output_index)
                {
                    let index = *index;
                    if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                        raw_args.push_str(delta);
                        if let Some(AssistantContent::ToolCall(block)) =
                            output.content.get_mut(index)
                        {
                            block.arguments =
                                parse_tool_call_arguments(raw_args).unwrap_or_default();
                        }
                        let _ = sender
                            .send(AssistantMessageEvent::ToolcallDelta {
                                content_index: index,
                                delta: delta.to_string(),
                                partial: output.clone(),
                            })
                            .await;
                    }
                }
            }
            "response.function_call_arguments.done" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if let Some(Slot::ToolCall { index, raw_args }) =
                    Self::slot_mut(&mut self.slots, output_index)
                {
                    let index = *index;
                    if let Some(arguments) = event.get("arguments").and_then(Value::as_str) {
                        // Push whatever the done event carries beyond the
                        // accumulated deltas (`replace(...).delta`).
                        let unsent = if arguments.starts_with(raw_args.as_str()) {
                            &arguments[raw_args.len()..]
                        } else {
                            arguments
                        };
                        if !unsent.is_empty() {
                            let _ = sender
                                .send(AssistantMessageEvent::ToolcallDelta {
                                    content_index: index,
                                    delta: unsent.to_string(),
                                    partial: output.clone(),
                                })
                                .await;
                        }
                        *raw_args = arguments.to_string();
                        if let Some(AssistantContent::ToolCall(block)) =
                            output.content.get_mut(index)
                        {
                            block.arguments =
                                parse_tool_call_arguments(arguments).unwrap_or_default();
                        }
                    }
                }
            }
            "response.output_item.done" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let Some(item) = event.get("item") else {
                    return Ok(());
                };
                Self::apply_message_phase_stop_reason(item, output);
                if !self.slots.contains_key(&output_index) {
                    Self::create_slot(
                        &mut self.slots,
                        &mut self.unfinished,
                        output,
                        output_index,
                        item,
                        sender,
                    )
                    .await;
                }
                let item_type = item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                // Copy the slot descriptor out (the map is mutated below).
                let slot_info = self.slots.get(&output_index).map(|slot| match slot {
                    Slot::Thinking { index } => ("reasoning", *index),
                    Slot::Text { index } => ("message", *index),
                    Slot::ToolCall { index, .. } => ("toolCall", *index),
                });
                let mut raw_args = self
                    .slots
                    .get(&output_index)
                    .map(|slot| match slot {
                        Slot::ToolCall { raw_args, .. } => raw_args.clone(),
                        _ => String::new(),
                    })
                    .unwrap_or_default();
                match (item_type.as_str(), slot_info) {
                    ("reasoning", Some(("reasoning", index))) => {
                        let summary: String = item
                            .get("summary")
                            .and_then(Value::as_array)
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                                    .collect::<Vec<_>>()
                                    .join("\n\n")
                            })
                            .unwrap_or_default();
                        let content_text: String = item
                            .get("content")
                            .and_then(Value::as_array)
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                                    .collect::<Vec<_>>()
                                    .join("\n\n")
                            })
                            .unwrap_or_default();
                        let (final_thinking, signature) = {
                            let Some(AssistantContent::Thinking(thinking)) =
                                output.content.get_mut(index)
                            else {
                                return Ok(());
                            };
                            if !summary.is_empty() {
                                thinking.thinking = summary;
                            } else if !content_text.is_empty() {
                                thinking.thinking = content_text;
                            }
                            (thinking.thinking.clone(), item.to_string())
                        };
                        if let Some(AssistantContent::Thinking(thinking)) =
                            output.content.get_mut(index)
                        {
                            thinking.thinking_signature = Some(signature);
                        }
                        let _ = sender
                            .send(AssistantMessageEvent::ThinkingEnd {
                                content_index: index,
                                content: final_thinking,
                                partial: output.clone(),
                            })
                            .await;
                        self.slots.remove(&output_index);
                    }
                    ("message", Some(("message", index))) => {
                        let final_text: String = item
                            .get("content")
                            .and_then(Value::as_array)
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|part| {
                                        match part.get("type").and_then(Value::as_str) {
                                            Some("output_text") => {
                                                part.get("text").and_then(Value::as_str)
                                            }
                                            _ => part.get("refusal").and_then(Value::as_str),
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .join("")
                            })
                            .unwrap_or_default();
                        let signature = encode_text_signature(
                            item.get("id").and_then(Value::as_str).unwrap_or(""),
                            item.get("phase").and_then(Value::as_str),
                        );
                        if let Some(AssistantContent::Text(text)) = output.content.get_mut(index) {
                            text.text.clone_from(&final_text);
                            text.text_signature = Some(signature);
                        }
                        let _ = sender
                            .send(AssistantMessageEvent::TextEnd {
                                content_index: index,
                                content: final_text,
                                partial: output.clone(),
                            })
                            .await;
                        self.slots.remove(&output_index);
                    }
                    ("function_call", Some(("toolCall", index))) => {
                        if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                            let unsent = if arguments.starts_with(raw_args.as_str()) {
                                &arguments[raw_args.len()..]
                            } else {
                                arguments
                            };
                            if !unsent.is_empty() {
                                let _ = sender
                                    .send(AssistantMessageEvent::ToolcallDelta {
                                        content_index: index,
                                        delta: unsent.to_string(),
                                        partial: output.clone(),
                                    })
                                    .await;
                            }
                            raw_args = arguments.to_string();
                        }
                        let namespace = item
                            .get("namespace")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        let block = match output.content.get_mut(index) {
                            Some(AssistantContent::ToolCall(block)) => {
                                block.arguments =
                                    parse_tool_call_arguments(&raw_args).unwrap_or_default();
                                if block.arguments.is_empty() && !raw_args.trim().is_empty() {
                                    block.arguments_error = Some(format!(
                                        "Invalid JSON in tool call arguments: {raw_args}"
                                    ));
                                }
                                if let Some(namespace) = &namespace {
                                    block.namespace = Some(namespace.clone());
                                }
                                block.clone()
                            }
                            _ => return Ok(()),
                        };
                        self.unfinished.retain(|open| *open != index);
                        let _ = sender
                            .send(AssistantMessageEvent::ToolcallEnd {
                                content_index: index,
                                tool_call: block,
                                partial: output.clone(),
                            })
                            .await;
                        self.slots.remove(&output_index);
                    }
                    ("custom_tool_call", Some(("toolCall", index))) => {
                        if let Some(input) = item.get("input").and_then(Value::as_str) {
                            raw_args = input.to_string();
                        }
                        let namespace = item
                            .get("namespace")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        let block = match output.content.get_mut(index) {
                            Some(AssistantContent::ToolCall(block)) => {
                                block.arguments = serde_json::Map::from_iter([(
                                    GRAMMAR_INPUT_PROPERTY.to_string(),
                                    json!(raw_args.clone()),
                                )]);
                                if let Some(namespace) = &namespace {
                                    block.namespace = Some(namespace.clone());
                                }
                                block.clone()
                            }
                            _ => return Ok(()),
                        };
                        self.unfinished.retain(|open| *open != index);
                        let _ = sender
                            .send(AssistantMessageEvent::ToolcallEnd {
                                content_index: index,
                                tool_call: block,
                                partial: output.clone(),
                            })
                            .await;
                        self.slots.remove(&output_index);
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                self.saw_terminal = true;
                let response = event.get("response");
                if let Some(id) = response.and_then(|r| r.get("id")).and_then(Value::as_str) {
                    output.response_id = Some(id.to_string());
                }
                if let Some(usage) = response
                    .and_then(|r| r.get("usage"))
                    .filter(|u| u.is_object())
                {
                    let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
                    let input_tokens = field("input_tokens");
                    let output_tokens = field("output_tokens");
                    let details = usage.get("input_tokens_details");
                    let cached = details
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let cache_write = details
                        .and_then(|d| d.get("cache_write_tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let reasoning = usage
                        .get("output_tokens_details")
                        .and_then(|d| d.get("reasoning_tokens"))
                        .and_then(Value::as_u64);
                    // OpenAI includes cached and cache-write tokens in input_tokens.
                    output.usage.input = input_tokens
                        .saturating_sub(cached)
                        .saturating_sub(cache_write);
                    output.usage.output = output_tokens;
                    output.usage.cache_read = cached;
                    output.usage.cache_write = cache_write;
                    output.usage.reasoning = reasoning;
                    output.usage.total_tokens = field("total_tokens");
                }
                let status = response
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_str);
                let incomplete_reason = response
                    .and_then(|r| r.get("incomplete_details"))
                    .and_then(|d| d.get("reason"))
                    .and_then(Value::as_str);
                output.raw_stop_reason = Some(match (status, incomplete_reason) {
                    (Some(status), Some(reason)) => format!("{status}.{reason}"),
                    (Some(status), None) => status.to_string(),
                    (None, _) => String::new(),
                });
                let (stop, error_message) = map_stop_reason(status, incomplete_reason);
                output.stop_reason = stop;
                output.error_message = error_message;
                if output
                    .content
                    .iter()
                    .any(|block| matches!(block, AssistantContent::ToolCall(_)))
                    && output.stop_reason == StopReason::Stop
                {
                    output.stop_reason = StopReason::ToolUse;
                }
            }
            "error" => {
                let code = event.get("code").and_then(Value::as_str).unwrap_or("");
                let message = event.get("message").and_then(Value::as_str).unwrap_or("");
                return Err(format!("Error Code {code}: {message}"));
            }
            "response.failed" => {
                self.saw_terminal = true;
                let response = event.get("response");
                output.raw_stop_reason = response
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let message = match response.and_then(|r| r.get("error")) {
                    Some(error) if error.is_object() => format!(
                        "{}: {}",
                        error
                            .get("code")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown"),
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("no message")
                    ),
                    _ => match response
                        .and_then(|r| r.get("incomplete_details"))
                        .and_then(|d| d.get("reason"))
                        .and_then(Value::as_str)
                    {
                        Some(reason) => format!("incomplete: {reason}"),
                        None => "Unknown error (no error details in response)".to_string(),
                    },
                };
                return Err(message);
            }
            _ => {}
        }
        Ok(())
    }
}

/// Reasoning `effort` for a pi thinking level via `model.thinkingLevelMap`
/// (`thinkingLevelMap?.[level] ?? level`).
fn effort_for_level(model: &Model, level: ThinkingLevel) -> String {
    let ts_name = match level {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Off => "off",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
    };
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(ts_name))
        .and_then(Value::as_str)
        .unwrap_or(ts_name)
        .to_string()
}

/// Build the Codex Responses request body (`buildRequestBody`).
#[allow(clippy::too_many_lines)] // request assembly
fn build_request_body(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
) -> Value {
    let messages = &context.messages;
    let instructions = messages
        .first()
        .filter(|msg| matches!(msg, Message::System(_)))
        .and_then(|msg| match msg {
            Message::System(system) => Some(system_text(system)),
            _ => None,
        })
        .unwrap_or_default();

    let mut body = json!({
        "model": model.id,
        "store": false,
        "stream": true,
        "instructions": if instructions.is_empty() { DEFAULT_INSTRUCTIONS } else { &instructions },
        "input": convert_responses_messages(model, context),
        "text": { "verbosity": "low" },
        "include": ["reasoning.encrypted_content"],
        "tool_choice": match options.tool_choice {
            Some(crate::types::ToolChoice::None) => "none",
            _ => "auto",
        },
        "parallel_tool_calls": true,
    });

    if let Some(session_id) = &options.session_id {
        // `cacheRetention === "none"` skips the cache key; our retention enum
        // has no "none", so the key rides whenever a session id is present.
        body["prompt_cache_key"] = json!(clamp_prompt_cache_key(session_id));
    }

    if let Some(temperature) = options.temperature {
        body["temperature"] = json!(temperature);
    }

    let tools = tools_at_point(messages);
    if !tools.is_empty() {
        body["tools"] = json!(convert_responses_tools(model, &tools));
    }

    let effort = match options.reasoning {
        Some(level) => Some(effort_for_level(model, level)),
        None if model.reasoning => {
            // No requested level: the model's `off` mapping, else "none".
            Some(
                model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|map| map.get("off"))
                    .and_then(Value::as_str)
                    .unwrap_or("none")
                    .to_string(),
            )
        }
        None => None,
    };
    if let Some(effort) = effort {
        body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
    }

    body
}

/// POST the request and map the SSE stream onto protocol events.
#[allow(clippy::too_many_lines)] // transport + loop; matches the TS stream entry
async fn request_and_stream(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
    output: &mut AssistantMessage,
    sender: &EventSender,
) -> StreamResult<()> {
    // The ChatGPT OAuth access token arrives as either an API key (upstream
    // `toAuth().apiKey`) or a resolved bearer token.
    let token = options
        .access_token
        .clone()
        .or_else(|| options.api_key.clone())
        .ok_or_else(|| format!("No API key for provider: {}", model.provider))?;
    let account_id = extract_account_id(&token)?;

    let body = build_request_body(model, context, options);
    let url = resolve_codex_url(&model.base_url);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| format!("client build failed: {e}"))?;

    // Defaults first so model and caller headers can override them
    // (`buildSSEHeaders`); a `None` caller value deletes the header.
    let mut headers = reqwest::header::HeaderMap::new();
    let mut insert = |name: &'static str, value: String| {
        if let Ok(value) = value.parse() {
            headers.insert(name, value);
        }
    };
    insert("authorization", format!("Bearer {token}"));
    insert("chatgpt-account-id", account_id.clone());
    insert("openai-beta", OPENAI_BETA.to_string());
    insert("accept", "text/event-stream".to_string());
    insert("content-type", "application/json".to_string());
    // Codex expects an originator on every request (`identityHeaders`).
    insert("originator", "pi".to_string());
    if let Some(session_id) = &options.session_id {
        insert("session-id", session_id.clone());
        insert("x-client-request-id", session_id.clone());
    }
    for (key, value) in model.headers.iter().flatten() {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(key.as_bytes()),
            value.parse(),
        ) {
            headers.insert(name, value);
        }
    }
    for (key, value) in &options.headers {
        match value {
            Some(value) => {
                if let (Ok(name), Ok(value)) = (
                    reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                    value.parse(),
                ) {
                    headers.insert(name, value);
                }
            }
            None => {
                if let Ok(name) = reqwest::header::HeaderName::from_bytes(key.as_bytes()) {
                    headers.remove(name);
                }
            }
        }
    }

    let response = client
        .post(&url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let sse = read_sse_stream(response).await.map_err(|e| match e {
        SseError::Status { status, body } => format!("HTTP {status}: {body}"),
        SseError::Http(message) => message,
    })?;
    let _ = sender
        .send(AssistantMessageEvent::Start {
            partial: output.clone(),
        })
        .await;

    let mut state = StreamState::new();
    tokio::pin!(sse);
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
        let event: Value = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(error) => return Err(format!("could not parse Codex SSE event: {error}")),
        };
        state.apply(&event, output, sender).await?;
    }

    if !state.saw_terminal {
        return Err("OpenAI Responses stream ended before a terminal response event".to_string());
    }
    if output.stop_reason == StopReason::ToolUse {
        if let Some(&index) = state.unfinished.first() {
            let name = match output.content.get(index) {
                Some(AssistantContent::ToolCall(block)) => block.name.clone(),
                _ => String::new(),
            };
            let id = match output.content.get(index) {
                Some(AssistantContent::ToolCall(block)) => block.id.clone(),
                _ => String::new(),
            };
            return Err(format!(
                "OpenAI Responses stream completed with an unfinished tool call: {name} ({id})"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // `assert!(v.is_empty())` stays: the `assert_eq!(v, [])` alternative is
    // noisier for these fixture assertions.
    #![allow(clippy::assert_is_empty)]
    use super::*;

    fn codex_model() -> Model {
        Model {
            id: "gpt-6-luna".into(),
            name: "GPT-6 Luna".into(),
            api: "openai-codex-responses".into(),
            provider: "openai-codex".into(),
            base_url: "https://chatgpt.com/backend-api".into(),
            reasoning: true,
            thinking_level_map: Some(
                serde_json::from_str(
                    r#"{"off":"none","minimal":"low","low":"low","medium":"medium","high":"high"}"#,
                )
                .unwrap(),
            ),
            ..Model::default()
        }
    }

    #[test]
    fn resolves_codex_url_variants() {
        assert_eq!(
            resolve_codex_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url("https://example.com/codex/"),
            "https://example.com/codex/responses"
        );
        assert_eq!(
            resolve_codex_url("https://example.com/codex/responses"),
            "https://example.com/codex/responses"
        );
    }

    #[test]
    fn extracts_account_id_from_jwt() {
        // header.payload.signature with a chatgpt_account_id claim
        let header = BASE64_URL.encode(r#"{"alg":"none"}"#);
        let payload = BASE64_URL
            .encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc_123"}}"#);
        let token = format!("{header}.{payload}.sig");
        assert_eq!(extract_account_id(&token).unwrap(), "acc_123");
        assert!(extract_account_id("not-a-jwt").is_err());
    }

    #[test]
    fn body_carries_codex_defaults() {
        let model = codex_model();
        let context = TranscriptContext {
            messages: vec![
                Message::System(crate::types::SystemMessage {
                    content: crate::types::StringOrTextBlocks::Text("Be terse.".into()),
                    sections: None,
                    tools_added: None,
                    tools_removed: None,
                    timestamp: 0,
                }),
                Message::User(crate::types::UserMessage {
                    content: UserContent::Text("hi".into()),
                    timestamp: 1,
                }),
            ],
        };
        let body = build_request_body(&model, &context, &StreamOptions::default());
        assert_eq!(body["model"], "gpt-6-luna");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "Be terse.");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["reasoning"]["effort"], "none");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["text"]["verbosity"], "low");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
    }

    #[test]
    fn replayed_assistant_becomes_response_items() {
        let model = codex_model();
        let assistant = AssistantMessage {
            content: vec![
                AssistantContent::Thinking(crate::types::ThinkingContent {
                    thinking: "hmm".into(),
                    thinking_signature: Some(r#"{"id":"rs_1","summary":[]}"#.into()),
                    redacted: false,
                }),
                AssistantContent::Text(crate::types::TextContent {
                    text: "done".into(),
                    text_signature: Some(r#"{"v":1,"id":"msg_9","phase":"final_answer"}"#.into()),
                }),
                AssistantContent::ToolCall(ToolCall {
                    id: "call_1|fc_item2".into(),
                    name: "bash".into(),
                    arguments: serde_json::from_str(r#"{"cmd":"ls"}"#).unwrap(),
                    thought_signature: None,
                    namespace: None,
                    arguments_error: None,
                }),
            ],
            api: "openai-codex-responses".into(),
            provider: "openai-codex".into(),
            model: "gpt-6-luna".into(),
            ..initial_output(&codex_model())
        };
        let context = TranscriptContext {
            messages: vec![
                Message::User(crate::types::UserMessage {
                    content: UserContent::Text("go".into()),
                    timestamp: 0,
                }),
                Message::Assistant(assistant),
                Message::ToolResult(crate::types::ToolResultMessage {
                    tool_call_id: "call_1|fc_item2".into(),
                    tool_name: "bash".into(),
                    content: vec![InputContent::Text(crate::types::TextContent {
                        text: "out".into(),
                        text_signature: None,
                    })],
                    details: None,
                    usage: None,
                    nested_calls: None,
                    is_error: false,
                    timestamp: 2,
                    duration_ms: None,
                }),
            ],
        };
        let input = convert_responses_messages(&model, &context);
        assert_eq!(input.len(), 5);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["id"], "rs_1"); // reasoning item passthrough
        assert_eq!(input[2]["type"], "message");
        assert_eq!(input[2]["id"], "msg_9");
        assert_eq!(input[2]["phase"], "final_answer");
        assert_eq!(input[3]["type"], "function_call");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[3]["id"], "fc_item2");
        assert_eq!(input[4]["type"], "function_call_output");
        assert_eq!(input[4]["call_id"], "call_1");
        assert_eq!(input[4]["output"], "out");
    }

    #[tokio::test]
    async fn event_stream_maps_blocks_and_usage() {
        let model = codex_model();
        let mut output = initial_output(&model);
        let (stream, sender) = AssistantMessageEventStream::channel();
        let mut state = StreamState::new();
        let events = vec![
            json!({"type":"response.created","response":{"id":"resp_1"}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}),
            json!({"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"thinking"}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"text":"thinking hard"}]}}),
            json!({"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1"}}),
            json!({"type":"response.output_text.delta","output_index":1,"delta":"PONG"}),
            json!({"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"PONG"}]}}),
            json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":10},"output_tokens_details":{"reasoning_tokens":5}}}}),
        ];
        for event in &events {
            state.apply(event, &mut output, &sender).await.unwrap();
        }
        drop(sender);
        let received: Vec<AssistantMessageEvent> = futures::StreamExt::collect(stream).await;
        // `Start` is emitted by the transport layer, not by `apply`.
        assert!(matches!(
            received[0],
            AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                ..
            }
        ));
        assert!(
            matches!(received[1], AssistantMessageEvent::ThinkingDelta { ref delta, .. } if delta == "thinking")
        );
        assert!(
            matches!(received[2], AssistantMessageEvent::ThinkingEnd { ref content, .. } if content == "thinking hard")
        );
        assert!(matches!(
            received[3],
            AssistantMessageEvent::TextStart {
                content_index: 1,
                ..
            }
        ));
        assert!(
            matches!(received[4], AssistantMessageEvent::TextDelta { ref delta, .. } if delta == "PONG")
        );
        assert!(
            matches!(received[5], AssistantMessageEvent::TextEnd { ref content, .. } if content == "PONG")
        );
        assert_eq!(output.response_id.as_deref(), Some("resp_1"));
        assert_eq!(output.usage.input, 90);
        assert_eq!(output.usage.cache_read, 10);
        assert_eq!(output.usage.reasoning, Some(5));
        assert_eq!(output.stop_reason, StopReason::Stop);
        assert_eq!(output.raw_stop_reason.as_deref(), Some("completed"));
        let signature = match &output.content[0] {
            AssistantContent::Thinking(thinking) => thinking.thinking_signature.clone().unwrap(),
            other => panic!("expected thinking, got {other:?}"),
        };
        assert!(signature.contains("\"id\":\"rs_1\""), "{signature}");
        assert_eq!(
            match &output.content[1] {
                AssistantContent::Text(text) => text.text_signature.clone().unwrap(),
                other => panic!("expected text, got {other:?}"),
            },
            r#"{"v":1,"id":"msg_1"}"#
        );
    }

    #[tokio::test]
    async fn tool_call_events_map_and_tool_use_wins() {
        let model = codex_model();
        let mut output = initial_output(&model);
        let (_, sender) = AssistantMessageEventStream::channel();
        let mut state = StreamState::new();
        state
            .apply(
                &json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","id":"fc_1","name":"bash","arguments":""}}),
                &mut output,
                &sender,
            )
            .await
            .unwrap();
        state
            .apply(
                &json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"cm"}),
                &mut output,
                &sender,
            )
            .await
            .unwrap();
        state
            .apply(
                &json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"cmd\":\"ls\"}"}),
                &mut output,
                &sender,
            )
            .await
            .unwrap();
        state
            .apply(
                &json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_1","id":"fc_1","name":"bash","arguments":"{\"cmd\":\"ls\"}"}}),
                &mut output,
                &sender,
            )
            .await
            .unwrap();
        state
            .apply(
                &json!({"type":"response.completed","response":{"id":"r","status":"completed","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
                &mut output,
                &sender,
            )
            .await
            .unwrap();
        assert_eq!(output.stop_reason, StopReason::ToolUse);
        match &output.content[0] {
            AssistantContent::ToolCall(block) => {
                assert_eq!(block.id, "call_1|fc_1");
                assert_eq!(block.name, "bash");
                assert_eq!(block.arguments["cmd"], "ls");
                assert!(block.arguments_error.is_none());
            }
            other => panic!("expected tool call, got {other:?}"),
        }
        assert!(state.unfinished.is_empty());
    }

    #[tokio::test]
    async fn incomplete_and_failed_events_error() {
        let model = codex_model();
        let (_, sender) = AssistantMessageEventStream::channel();
        let mut state = StreamState::new();
        let mut output = initial_output(&model);
        state
            .apply(
                &json!({"type":"response.incomplete","response":{"id":"r","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}),
                &mut output,
                &sender,
            )
            .await
            .unwrap();
        assert_eq!(output.stop_reason, StopReason::Length);
        assert_eq!(
            output.raw_stop_reason.as_deref(),
            Some("incomplete.max_output_tokens")
        );

        let mut output = initial_output(&model);
        let error = state
            .apply(
                &json!({"type":"response.failed","response":{"status":"failed","error":{"code":"oops","message":"boom"}}}),
                &mut output,
                &sender,
            )
            .await
            .unwrap_err();
        assert_eq!(error, "oops: boom");
    }

    #[test]
    fn stop_reason_mapping_matches_ts() {
        let (stop, _) = map_stop_reason(Some("completed"), None);
        assert_eq!(stop, StopReason::Stop);
        let (stop, _) = map_stop_reason(Some("queued"), None);
        assert_eq!(stop, StopReason::Stop);
        let (stop, message) = map_stop_reason(Some("incomplete"), Some("content_filter"));
        assert_eq!(stop, StopReason::Error);
        assert_eq!(
            message.as_deref(),
            Some("Response incomplete: content_filter")
        );
        let (stop, _) = map_stop_reason(Some("incomplete"), Some("max_output_tokens"));
        assert_eq!(stop, StopReason::Length);
        let (stop, _) = map_stop_reason(Some("cancelled"), None);
        assert_eq!(stop, StopReason::Error);
    }
}
