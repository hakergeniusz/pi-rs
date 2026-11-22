//! Shared OpenAI Responses protocol — port of TS
//! `packages/ai/src/api/openai-responses-shared.ts:55-891` (utilities,
//! message/tool conversion and `processResponsesStream`). Used by both the
//! `openai-responses` and `azure-openai-responses` adapters.
//!
//! The event loop mirrors upstream's `ResponseStreamEvent` handling: output
//! slots keyed by `output_index`, reasoning/text/tool-call start/delta/end
//! events, terminal `response.completed`/`response.incomplete` usage and stop
//! mapping, Azure reasoning-signature backfill, and the unfinished-tool-call
//! guard. Documented deviations (as in the already-ported `openai_codex`
//! adapter):
//! - `sanitizeSurrogates` is a no-op: Rust strings are always valid UTF-8.
//! - Grammar (`custom_tool_call`) input uses the default `input` property and
//!   raw text accumulation instead of upstream's JSON-delta stitching buffer.
//! - Mid-transcript tool additions/removals (`additional_tools`, synthetic
//!   tool search) are not re-declared; the port's transcripts declare tools in
//!   the leading system message only.
//! - Service-tier pricing is available but only the OpenAI adapter enables it
//!   (matching upstream, where only `openai-responses.ts` passes the hook).

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

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::cost::calculate_cost;
use crate::event_stream::EventSender;
use crate::providers::common::{is_leading_system, system_text, system_update_text};
use crate::streaming_json::parse_tool_call_arguments;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Message, Model, StopReason,
    ThinkingLevel, ToolCall, Usage, UserContent,
};

/// Read a boolean `model.compat` flag with a default (upstream `getCompat`).
pub(super) fn compat_flag(model: &Model, key: &str, default: bool) -> bool {
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

/// Read a string `model.compat` value with a default (upstream `getCompat`).
fn compat_string(model: &Model, key: &str, default: String) -> String {
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.get(key))
        .and_then(Value::as_str)
        .unwrap_or(&default)
        .to_string()
}

/// Upstream `getCompat(model)` (`openai-responses.ts:91-106`).
pub(super) struct ResponsesCompat {
    pub(super) supports_developer_role: bool,
    // Retained from upstream's compat matrix for future mid-conversation tool
    // change support; the port's transcripts declare tools in the leading
    // system message only.
    #[allow(dead_code)]
    pub(super) supports_mid_convo_system_messages: bool,
    pub(super) session_affinity_format: String,
    pub(super) prompt_cache_key_mode: String,
    pub(super) supports_long_cache_retention: bool,
    pub(super) supports_strict_mode: bool,
    pub(super) supports_openai_grammar_tools: bool,
    #[allow(dead_code)]
    pub(super) supports_additional_tools: bool,
    #[allow(dead_code)]
    pub(super) supports_tool_search: bool,
    pub(super) supports_explicit_prompt_cache_mode: bool,
    pub(super) supports_max_output_tokens: bool,
    pub(super) supports_mid_convo_effort: bool,
}

impl ResponsesCompat {
    pub(super) fn from_model(model: &Model) -> Self {
        let session_affinity_format = compat_string(
            model,
            "sessionAffinityFormat",
            detect_session_affinity_format(model),
        );
        Self {
            supports_developer_role: compat_flag(model, "supportsDeveloperRole", true),
            supports_mid_convo_system_messages: compat_flag(
                model,
                "supportsMidConvoSystemMessages",
                false,
            ),
            session_affinity_format,
            prompt_cache_key_mode: compat_string(model, "promptCacheKeyMode", "auto".to_string()),
            supports_long_cache_retention: compat_flag(model, "supportsLongCacheRetention", true),
            supports_strict_mode: compat_flag(model, "supportsStrictMode", false),
            supports_openai_grammar_tools: compat_flag(model, "supportsOpenAIGrammarTools", false),
            supports_additional_tools: compat_flag(model, "supportsAdditionalTools", false),
            supports_tool_search: compat_flag(model, "supportsToolSearch", false),
            supports_explicit_prompt_cache_mode: compat_flag(
                model,
                "supportsExplicitPromptCacheMode",
                false,
            ),
            supports_max_output_tokens: compat_flag(model, "supportsMaxOutputTokens", true),
            supports_mid_convo_effort: compat_flag(model, "supportsMidConvoEffort", false),
        }
    }
}

/// `detectSessionAffinityFormat` (`openai-responses.ts:73-75`).
fn detect_session_affinity_format(model: &Model) -> String {
    if model.provider == "openrouter" || model.base_url.contains("openrouter.ai") {
        "openrouter".to_string()
    } else {
        "openai".to_string()
    }
}

/// `normalizeIdPart` (`openai-responses-shared.ts:201-205`): sanitize to
/// `[a-zA-Z0-9_-]`, cap at 64, strip trailing underscores.
pub(super) fn normalize_id_part(part: &str) -> String {
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
/// Providers treat these ids as opaque.
pub(super) fn short_hash(input: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// `clampOpenAIPromptCacheKey`: sanitize a session id into a prompt-cache key.
pub(super) fn clamp_prompt_cache_key(session_id: &str) -> String {
    normalize_id_part(session_id)
}

/// `normalizeToolCallId` (`openai-responses-shared.ts:212-224`) for replayed
/// assistant tool calls.
fn normalize_tool_call_id(id: &str, model: &Model, source: &AssistantMessage) -> String {
    let allowed = ["openai", "openai-codex", "opencode", "azure"];
    if !allowed.contains(&model.provider.as_str()) || !id.contains('|') {
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

/// `parseTextSignature` (`openai-responses-shared.ts:61-79`): `{v:1,id,phase?}`
/// JSON, or a legacy plain id string. Returns `(id, phase)`.
fn parse_text_signature(signature: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(signature) = signature else {
        return (None, None);
    };
    if signature.starts_with('{')
        && let Ok(parsed) = serde_json::from_str::<Value>(signature)
        && parsed.get("v").and_then(Value::as_u64) == Some(1)
        && let Some(id) = parsed.get("id").and_then(Value::as_str)
    {
        let phase = parsed
            .get("phase")
            .and_then(Value::as_str)
            .filter(|phase| *phase == "commentary" || *phase == "final_answer")
            .map(str::to_string);
        return (Some(id.to_string()), phase);
    }
    (Some(signature.to_string()), None)
}

/// `encodeTextSignatureV1` (`openai-responses-shared.ts:55-59`).
pub(super) fn encode_text_signature(id: &str, phase: Option<&str>) -> String {
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

/// `convertResponsesTools` (`openai-responses-shared.ts:439-476`). `default_strict`
/// is upstream's `options.supportsStrictMode ?? ...`: `false` for the OpenAI
/// adapter's compat matrix, `true` for Azure (and Codex).
pub(super) fn convert_responses_tools(
    model: &Model,
    tools: &[crate::types::Tool],
    default_strict: bool,
) -> Vec<Value> {
    let supports_strict = compat_flag(model, "supportsStrictMode", default_strict);
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

/// `convertResponsesMessages` (`openai-responses-shared.ts:192-433`), minus the
/// mid-transcript tool-change items documented in the module docs. Unlike the
/// Codex adapter, later system messages go into `input` (the Responses API has
/// no `instructions` split), and a `configuration_update` item can precede a
/// reply and close the request.
#[allow(clippy::too_many_lines)] // mirrors the TS message-conversion shape; split when extending
pub(super) fn convert_responses_messages(
    model: &Model,
    context: &crate::types::TranscriptContext,
    compat: &ResponsesCompat,
    configuration_update_effort: Option<&str>,
) -> Vec<Value> {
    let messages = &context.messages;
    let instruction_role = if model.reasoning && compat.supports_developer_role {
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
                // Responses carries the system prompt in `input`; Codex uses
                // `instructions` instead.
                let text = if leading_system {
                    system_text(system)
                } else {
                    system_update_text(system)
                };
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
                            crate::types::InputContent::Text(text) => json!({
                                "type": "input_text",
                                "text": text.text,
                            }),
                            crate::types::InputContent::Image(image) => json!({
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
                            if let Some(signature) = &thinking.thinking_signature
                                && let Ok(item) = serde_json::from_str::<Value>(signature)
                            {
                                output.push(item);
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
                            // Different-model messages drop the item id to avoid
                            // pairing validation; `function_call` ids must be `fc_*`.
                            let id = if is_different_model
                                || !item_id.as_deref().unwrap_or("").starts_with("fc_")
                            {
                                None
                            } else {
                                item_id
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
                            if is_same_model && let Some(namespace) = &tool_call.namespace {
                                item["namespace"] = json!(namespace);
                            }
                            output.push(item);
                        }
                    }
                }
                if output.is_empty() {
                    continue;
                }
                // `configuration_update` before a reply records the thinking
                // level used from that point on, keeping earlier input stable.
                if configuration_update_effort.is_some()
                    && is_same_model
                    && let Some(level) = &assistant.provider_thinking_level
                {
                    input.push(json!({
                        "type": "configuration_update",
                        "reasoning": { "effort": level },
                    }));
                }
                input.extend(output);
            }
            Message::ToolResult(result) => {
                let call_id = result.tool_call_id.split('|').next().unwrap_or("");
                let text: String = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        crate::types::InputContent::Text(text) => Some(text.text.as_str()),
                        crate::types::InputContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let images: Vec<&crate::types::ImageContent> = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        crate::types::InputContent::Image(image) => Some(image),
                        crate::types::InputContent::Text(_) => None,
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
    if let Some(effort) = configuration_update_effort {
        input.push(json!({
            "type": "configuration_update",
            "reasoning": { "effort": effort },
        }));
    }
    input
}

/// `mapStopReason` (`openai-responses-shared.ts:861-891`): status (+ incomplete
/// reason) → pi stop reason and optional error message.
pub(super) fn map_stop_reason(
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

/// Thinking effort for a pi level via `model.thinkingLevelMap`
/// (`thinkingLevelMap?.[level] ?? level`).
pub(super) fn effort_for_level(model: &Model, level: ThinkingLevel) -> String {
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

/// `getServiceTierCostMultiplier` + `applyServiceTierPricing`
/// (`openai-responses.ts:427-455`).
pub(super) fn apply_service_tier_pricing(usage: &mut Usage, tier: Option<&str>, model_id: &str) {
    let multiplier = match tier {
        Some("flex") => 0.5,
        Some("priority" | "fast") => {
            if model_id == "gpt-5.5" {
                2.5
            } else {
                2.0
            }
        }
        _ => 1.0,
    };
    if multiplier == 1.0 {
        return;
    }
    usage.cost.input *= multiplier;
    usage.cost.output *= multiplier;
    usage.cost.cache_read *= multiplier;
    usage.cost.cache_write *= multiplier;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
}

/// Streaming slot keyed by `output_index` (`ResponsesOutputSlot`).
enum Slot {
    Thinking {
        index: usize,
    },
    Text {
        index: usize,
    },
    ToolCall {
        index: usize,
        raw_args: String,
        /// True for `custom_tool_call` (grammar) input; false for JSON args.
        custom: bool,
    },
}

/// `processResponsesStream` state (`openai-responses-shared.ts:503-828`).
pub(super) struct ResponsesStreamState {
    slots: HashMap<u64, Slot>,
    /// Tool calls whose `output_item.done` has not arrived yet.
    unfinished: Vec<usize>,
    /// `reasoningBlocksById`, used for Azure encrypted-content backfill.
    reasoning_blocks: HashMap<String, usize>,
    saw_terminal: bool,
    /// Only the OpenAI adapter applies service-tier pricing (upstream's hook).
    service_tier_pricing: bool,
}

impl ResponsesStreamState {
    pub(super) fn new(service_tier_pricing: bool) -> Self {
        Self {
            slots: HashMap::new(),
            unfinished: Vec::new(),
            reasoning_blocks: HashMap::new(),
            saw_terminal: false,
            service_tier_pricing,
        }
    }

    pub(super) fn saw_terminal(&self) -> bool {
        self.saw_terminal
    }

    pub(super) fn unfinished(&self) -> &[usize] {
        &self.unfinished
    }

    /// `applyMessagePhaseStopReason` (`openai-responses-shared.ts:515-519`).
    fn apply_message_phase_stop_reason(item: &Value, output: &mut AssistantMessage) {
        if item.get("type").and_then(Value::as_str) == Some("message")
            && item.get("phase").and_then(Value::as_str) == Some("final_answer")
        {
            output.stop_reason = StopReason::Stop;
        }
    }

    /// `createSlot`/`getOrCreateSlot` (`openai-responses-shared.ts:536-605`).
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
                let custom = item_type == Some("custom_tool_call");
                let initial = if custom {
                    item.get("input").and_then(Value::as_str).unwrap_or("")
                } else {
                    item.get("arguments").and_then(Value::as_str).unwrap_or("")
                };
                let arguments = if custom {
                    serde_json::Map::from_iter([(
                        GRAMMAR_INPUT_PROPERTY.to_string(),
                        json!(initial),
                    )])
                } else {
                    parse_tool_call_arguments(initial).unwrap_or_default()
                };
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
                        custom,
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

    /// `backfillReasoningSignatures` (`openai-responses-shared.ts:610-623`):
    /// Azure can omit `encrypted_content` from `output_item.done`, providing it
    /// only in the terminal `response.output`.
    fn backfill_reasoning_signatures(&self, response: &Value, output: &mut AssistantMessage) {
        let Some(items) = response.get("output").and_then(Value::as_array) else {
            return;
        };
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some("reasoning") {
                continue;
            }
            let Some(encrypted) = item.get("encrypted_content").and_then(Value::as_str) else {
                continue;
            };
            let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
            let Some(&index) = self.reasoning_blocks.get(item_id) else {
                continue;
            };
            let Some(AssistantContent::Thinking(block)) = output.content.get_mut(index) else {
                continue;
            };
            let Some(signature) = block.thinking_signature.clone() else {
                continue;
            };
            let Ok(mut stored) = serde_json::from_str::<Value>(&signature) else {
                continue;
            };
            if stored
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some()
            {
                continue;
            }
            stored["encrypted_content"] = json!(encrypted);
            block.thinking_signature = Some(stored.to_string());
        }
    }

    /// `finalizeResponse` (`openai-responses-shared.ts:624-669`).
    fn finalize_response(
        &mut self,
        response: Option<&Value>,
        output: &mut AssistantMessage,
        model: &Model,
    ) {
        self.saw_terminal = true;
        let Some(response) = response else {
            calculate_cost(&mut output.usage, &model.cost);
            output.stop_reason = StopReason::Stop;
            output.error_message = None;
            return;
        };
        self.backfill_reasoning_signatures(response, output);
        if let Some(id) = response.get("id").and_then(Value::as_str) {
            output.response_id = Some(id.to_string());
        }
        if let Some(usage) = response.get("usage").filter(|usage| usage.is_object()) {
            let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
            let input_tokens = field("input_tokens");
            let output_tokens = field("output_tokens");
            let details = usage.get("input_tokens_details");
            let cached = details
                .and_then(|details| details.get("cached_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cache_write = details
                .and_then(|details| details.get("cache_write_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let reasoning = usage
                .get("output_tokens_details")
                .and_then(|details| details.get("reasoning_tokens"))
                .and_then(Value::as_u64);
            // OpenAI includes cached and cache-write tokens in `input_tokens`.
            output.usage.input = input_tokens
                .saturating_sub(cached)
                .saturating_sub(cache_write);
            output.usage.output = output_tokens;
            output.usage.cache_read = cached;
            output.usage.cache_write = cache_write;
            output.usage.reasoning = reasoning;
            output.usage.total_tokens = field("total_tokens");
        }
        calculate_cost(&mut output.usage, &model.cost);
        if self.service_tier_pricing {
            let tier = response.get("service_tier").and_then(Value::as_str);
            apply_service_tier_pricing(&mut output.usage, tier, &model.id);
        }
        let status = response.get("status").and_then(Value::as_str);
        let incomplete_reason = response
            .get("incomplete_details")
            .and_then(|details| details.get("reason"))
            .and_then(Value::as_str);
        output.raw_stop_reason = match (status, incomplete_reason) {
            (Some(status), Some(reason)) => Some(format!("{status}.{reason}")),
            (Some(status), None) => Some(status.to_string()),
            (None, _) => None,
        };
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

    /// Apply one `ResponseStreamEvent` JSON payload (`processResponsesStream`).
    #[allow(clippy::too_many_lines)] // mirrors the TS event-loop shape; split when extending
    pub(super) async fn apply(
        &mut self,
        event: &Value,
        output: &mut AssistantMessage,
        model: &Model,
        sender: &EventSender,
    ) -> Result<(), String> {
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            "response.created" => {
                if let Some(id) = event.pointer("/response/id").and_then(Value::as_str) {
                    output.response_id = Some(id.to_string());
                }
            }
            "response.output_item.added" => {
                let output_index = output_index(event);
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
                let output_index = output_index(event);
                if let Some(Slot::Thinking { index }) = self.slots.get(&output_index) {
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
                let output_index = output_index(event);
                if let Some(Slot::Thinking { index }) = self.slots.get(&output_index) {
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
                let output_index = output_index(event);
                if let Some(Slot::Text { index }) = self.slots.get(&output_index) {
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
                let output_index = output_index(event);
                if let Some(Slot::ToolCall {
                    index,
                    raw_args,
                    custom: false,
                }) = self.slots.get_mut(&output_index)
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
                let output_index = output_index(event);
                if let Some(Slot::ToolCall {
                    index,
                    raw_args,
                    custom: false,
                }) = self.slots.get_mut(&output_index)
                {
                    let index = *index;
                    if let Some(arguments) = event.get("arguments").and_then(Value::as_str) {
                        let unsent = unsent_tail(arguments, raw_args);
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
            "response.custom_tool_call_input.delta" => {
                let output_index = output_index(event);
                if let Some(Slot::ToolCall {
                    index,
                    raw_args,
                    custom: true,
                }) = self.slots.get_mut(&output_index)
                {
                    let index = *index;
                    if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                        raw_args.push_str(delta);
                        if let Some(AssistantContent::ToolCall(block)) =
                            output.content.get_mut(index)
                        {
                            block.arguments = serde_json::Map::from_iter([(
                                GRAMMAR_INPUT_PROPERTY.to_string(),
                                json!(raw_args.as_str()),
                            )]);
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
            "response.custom_tool_call_input.done" => {
                let output_index = output_index(event);
                if let Some(Slot::ToolCall {
                    index,
                    raw_args,
                    custom: true,
                }) = self.slots.get_mut(&output_index)
                {
                    let index = *index;
                    if let Some(input) = event.get("input").and_then(Value::as_str) {
                        let unsent = unsent_tail(input, raw_args);
                        if !unsent.is_empty() {
                            let _ = sender
                                .send(AssistantMessageEvent::ToolcallDelta {
                                    content_index: index,
                                    delta: unsent.to_string(),
                                    partial: output.clone(),
                                })
                                .await;
                        }
                        *raw_args = input.to_string();
                    }
                    if let Some(AssistantContent::ToolCall(block)) = output.content.get_mut(index) {
                        block.arguments = serde_json::Map::from_iter([(
                            GRAMMAR_INPUT_PROPERTY.to_string(),
                            json!(raw_args.as_str()),
                        )]);
                    }
                }
            }
            "response.output_item.done" => {
                let output_index = output_index(event);
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
                        let summary = text_field(item, "summary");
                        let content_text = text_field(item, "content");
                        let final_thinking = {
                            let Some(AssistantContent::Thinking(thinking)) =
                                output.content.get_mut(index)
                            else {
                                return Ok(());
                            };
                            if !summary.is_empty() {
                                thinking.thinking.clone_from(&summary);
                            } else if !content_text.is_empty() {
                                thinking.thinking.clone_from(&content_text);
                            }
                            thinking.thinking.clone()
                        };
                        let signature = item.to_string();
                        if let Some(AssistantContent::Thinking(thinking)) =
                            output.content.get_mut(index)
                        {
                            thinking.thinking_signature = Some(signature);
                        }
                        if let Some(id) = item.get("id").and_then(Value::as_str) {
                            self.reasoning_blocks.insert(id.to_string(), index);
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
                        let final_text = item
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
                            let unsent = unsent_tail(arguments, &raw_args);
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
                            let unsent = unsent_tail(input, &raw_args);
                            if !unsent.is_empty() {
                                let _ = sender
                                    .send(AssistantMessageEvent::ToolcallDelta {
                                        content_index: index,
                                        delta: unsent.to_string(),
                                        partial: output.clone(),
                                    })
                                    .await;
                            }
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
                                    json!(raw_args.as_str()),
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
                self.finalize_response(event.get("response"), output, model);
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
                    .and_then(|response| response.get("status"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let message = match response.and_then(|response| response.get("error")) {
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
                        .and_then(|response| response.get("incomplete_details"))
                        .and_then(|details| details.get("reason"))
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

/// `output_index` of a stream event, defaulting to 0 (matching TS's numeric
/// `output_index` field).
fn output_index(event: &Value) -> u64 {
    event
        .get("output_index")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Port of `ToolCallArgumentsStream.replace(...).delta`: emit only the part of
/// the final arguments not already delivered through deltas.
fn unsent_tail<'a>(final_text: &'a str, accumulated: &str) -> &'a str {
    if final_text.starts_with(accumulated) {
        &final_text[accumulated.len()..]
    } else {
        final_text
    }
}

/// Join a reasoning item's `summary`/`content` text parts with `\n\n`
/// (`response.output_item.done` reasoning handling).
fn text_field(item: &Value, name: &str) -> String {
    item.get(name)
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_and_ids_round_trip() {
        let (id, phase) =
            parse_text_signature(Some(r#"{"v":1,"id":"msg_9","phase":"final_answer"}"#));
        assert_eq!(id.as_deref(), Some("msg_9"));
        assert_eq!(phase.as_deref(), Some("final_answer"));
        assert_eq!(
            encode_text_signature("msg_9", Some("final_answer")),
            r#"{"v":1,"id":"msg_9","phase":"final_answer"}"#
        );
        assert_eq!(
            parse_text_signature(Some("legacy")).0.as_deref(),
            Some("legacy")
        );
        assert_eq!(normalize_id_part("a b/c"), "a_b_c");
        assert_eq!(short_hash("abc").len(), 16);
    }

    #[test]
    fn stop_reason_mapping_matches_ts() {
        assert_eq!(map_stop_reason(None, None).0, StopReason::Stop);
        assert_eq!(map_stop_reason(Some("completed"), None).0, StopReason::Stop);
        assert_eq!(map_stop_reason(Some("queued"), None).0, StopReason::Stop);
        assert_eq!(
            map_stop_reason(Some("incomplete"), Some("max_output_tokens")).0,
            StopReason::Length
        );
        let (stop, message) = map_stop_reason(Some("incomplete"), Some("content_filter"));
        assert_eq!(stop, StopReason::Error);
        assert_eq!(
            message.as_deref(),
            Some("Response incomplete: content_filter")
        );
        assert_eq!(map_stop_reason(Some("failed"), None).0, StopReason::Error);
    }

    #[test]
    fn service_tier_pricing_matches_ts() {
        let mut usage = Usage {
            input: 100,
            output: 100,
            cache_read: 100,
            cache_write: 100,
            ..Usage::default()
        };
        usage.cost.input = 1.0;
        usage.cost.output = 1.0;
        usage.cost.cache_read = 1.0;
        usage.cost.cache_write = 1.0;
        usage.cost.total = 4.0;
        apply_service_tier_pricing(&mut usage, Some("flex"), "gpt-5");
        assert!((usage.cost.total - 2.0).abs() < 1e-9);
        apply_service_tier_pricing(&mut usage, Some("priority"), "gpt-5.5");
        assert!((usage.cost.input - 1.25).abs() < 1e-9);
        apply_service_tier_pricing(&mut usage, None, "gpt-5");
        assert!((usage.cost.input - 1.25).abs() < 1e-9);
    }
}
