//! Shared helpers for the two Google streaming adapters (port of TS
//! `packages/ai/src/api/google-shared.ts:1-574`; the streaming event loop is the
//! identical body of `google-generative-ai.ts:102-293` and
//! `google-vertex.ts:118-308`).
//!
//! This module owns the thinking-level resolution, tool/message conversion,
//! finish-reason and usage mapping, and the SSE chunk -> `AssistantMessageEvent`
//! state machine shared by both adapters. Wire-shape notes: the `@google/genai`
//! SDK maps `GenerateContentConfig.systemInstruction`/`tools`/`toolConfig` to
//! top-level request fields and `temperature`/`maxOutputTokens`/`thinkingConfig`
//! into `generationConfig` (`_models_converters.ts:2241-2322`).

use crate::constrained_sampling::{ConstrainedSampling, StrictMode};
use crate::cost::calculate_cost;
use crate::event_stream::EventSender;
use crate::providers::common::{
    apply_stream_event_hook, system_text, system_update_text, tools_at_point,
};
use crate::sse::{SseError, SseMessage};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, InputContent, Message, Model,
    StopReason, StreamOptions, TextContent, ThinkingContent, ThinkingLevel, Tool, ToolCall,
    ToolChoice, TranscriptContext, UserContent, now_ms,
};
use futures::StreamExt;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic suffix for generated tool-call IDs (TS module-level
/// `toolCallCounter` in both adapters).
static NEXT_TOOL_CALL_ID: AtomicU64 = AtomicU64::new(0);

const THINKING_LEVEL_ORDER: [&str; 5] = ["off", "minimal", "low", "medium", "high"];

/// A resolved Google thinking level (TS `ResolvedGoogleThinkingLevel`,
/// `google-shared.ts:37`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolvedThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
}

impl ResolvedThinkingLevel {
    /// Lowercase key used by `ThinkingBudgets` lookups.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Wire key for a pi thinking level (TS uses the enum's lowercase value).
fn thinking_level_key(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
    }
}

fn thinking_level_from_str(key: &str) -> Option<ThinkingLevel> {
    match key {
        "off" => Some(ThinkingLevel::Off),
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        _ => None,
    }
}

/// TS `clampThinkingLevel` (`models.ts:1315-1334`) restricted to the levels the
/// Rust `ThinkingLevel` enum can express.
pub(crate) fn clamp_thinking_level(model: &Model, level: ThinkingLevel) -> ThinkingLevel {
    let supported = crate::types::get_supported_thinking_levels(model);
    let key = thinking_level_key(level);
    if supported.contains(&key) {
        return level;
    }
    let requested = THINKING_LEVEL_ORDER
        .iter()
        .position(|candidate| *candidate == key)
        .unwrap_or(0);
    for candidate in THINKING_LEVEL_ORDER
        .iter()
        .skip(requested)
        .chain(THINKING_LEVEL_ORDER.iter().take(requested).rev())
    {
        if supported.contains(candidate)
            && let Some(resolved) = thinking_level_from_str(candidate)
        {
            return resolved;
        }
    }
    ThinkingLevel::Off
}

/// TS `resolveGoogleThinkingLevel` (`google-shared.ts:48-65`): a model's
/// `thinkingLevelMap` may remap a pi level before it is validated.
pub(crate) fn resolve_google_thinking_level(
    model: &Model,
    level: ThinkingLevel,
) -> Result<ResolvedThinkingLevel, String> {
    let key = thinking_level_key(level);
    let mapped = model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(key))
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    let resolved = mapped.as_deref().unwrap_or(key);
    match resolved {
        "minimal" => Ok(ResolvedThinkingLevel::Minimal),
        "low" => Ok(ResolvedThinkingLevel::Low),
        "medium" => Ok(ResolvedThinkingLevel::Medium),
        "high" => Ok(ResolvedThinkingLevel::High),
        _ => Err(format!(
            "Unsupported Google thinking level mapping for {}/{}: {key} -> {resolved}",
            model.provider, model.id
        )),
    }
}

/// TS `usesGoogleThinkingLevel` (`google-shared.ts:72-85`). Implemented as a
/// small scanner instead of a regex so no extra crate is needed.
pub(crate) fn uses_google_thinking_level(model: &Model) -> bool {
    let id = model.id.to_ascii_lowercase();
    if id == "gemini-flash-latest" || id == "gemini-flash-lite-latest" {
        return true;
    }
    if id.contains("gemma-4") || id.contains("gemma4") {
        return true;
    }
    let mut offset = 0;
    while let Some(relative) = id[offset..].find("gemini-") {
        let start = offset + relative;
        let rest = &id[start + "gemini-".len()..];
        let rest = rest.strip_prefix("live-").unwrap_or(rest);
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty()
            && !digits.starts_with('0')
            && let Ok(major) = digits.parse::<u64>()
        {
            let after_major = &rest[digits.len()..];
            let after_minor = if let Some(after_dot) = after_major.strip_prefix('.') {
                let minor: String = after_dot.chars().take_while(char::is_ascii_digit).collect();
                if minor.is_empty() {
                    after_major
                } else {
                    &after_dot[minor.len()..]
                }
            } else {
                after_major
            };
            let gemini_3_pro_flash = major == 3
                && (after_minor.starts_with("-pro") || after_minor.starts_with("-flash"));
            if gemini_3_pro_flash || major >= 4 {
                return true;
            }
        }
        offset = start + "gemini-".len();
    }
    false
}

/// TS `toGoogleThinkingLevel` / the SDK enum value (`google-shared.ts:87-102`).
pub(crate) fn to_api_thinking_level(level: ResolvedThinkingLevel) -> &'static str {
    match level {
        ResolvedThinkingLevel::Minimal => "MINIMAL",
        ResolvedThinkingLevel::Low => "LOW",
        ResolvedThinkingLevel::Medium => "MEDIUM",
        ResolvedThinkingLevel::High => "HIGH",
    }
}

/// TS `getDisabledGoogleThinkingConfig` (`google-shared.ts:104-113`).
pub(crate) fn get_disabled_thinking_config(model: &Model) -> Value {
    if !uses_google_thinking_level(model) {
        return json!({ "thinkingBudget": 0 });
    }
    let fallback = clamp_thinking_level(model, ThinkingLevel::Off);
    if fallback == ThinkingLevel::Off {
        return json!({ "thinkingBudget": 0 });
    }
    match resolve_google_thinking_level(model, fallback) {
        Ok(level) => json!({ "thinkingLevel": to_api_thinking_level(level) }),
        Err(_) => json!({ "thinkingBudget": 0 }),
    }
}

/// TS `getGoogleBudget` (`google-generative-ai.ts:426-466`,
/// `google-vertex.ts:531-561`), with the flash-lite branch ported from the
/// former (harmless on Vertex, where that model family is absent).
pub(crate) fn get_budget(
    model: &Model,
    level: ResolvedThinkingLevel,
    custom: Option<&Map<String, Value>>,
) -> i64 {
    if let Some(value) = custom
        .and_then(|budgets| budgets.get(level.as_str()))
        .and_then(Value::as_i64)
    {
        return value;
    }
    let budgets = if model.id.contains("2.5-pro") {
        Some([128, 2048, 8192, 32_768])
    } else if model.id.contains("2.5-flash-lite") {
        Some([512, 2048, 8192, 24_576])
    } else if model.id.contains("2.5-flash") {
        Some([128, 2048, 8192, 24_576])
    } else {
        None
    };
    budgets.map_or(-1, |values| match level {
        ResolvedThinkingLevel::Minimal => values[0],
        ResolvedThinkingLevel::Low => values[1],
        ResolvedThinkingLevel::Medium => values[2],
        ResolvedThinkingLevel::High => values[3],
    })
}

/// Assemble `config.thinkingConfig` (TS `buildParams` in both adapters,
/// lines 398-408 and 503-513). `options.reasoning` stands in for the TS
/// `GoogleOptions.thinking`; `metadata.thinkingBudgets` and
/// `metadata.thinkingLevel` are additive override seams for callers that need
/// the exact upstream budget/level knobs.
pub(crate) fn thinking_config(
    model: &Model,
    options: &StreamOptions,
) -> Result<Option<Value>, String> {
    if !model.reasoning {
        return Ok(None);
    }
    let Some(requested) = options.reasoning else {
        return Ok(Some(get_disabled_thinking_config(model)));
    };
    let clamped = clamp_thinking_level(model, requested);
    if clamped == ThinkingLevel::Off {
        return Ok(Some(get_disabled_thinking_config(model)));
    }
    let resolved = resolve_google_thinking_level(model, clamped)?;
    let mut config = json!({ "includeThoughts": true });
    if uses_google_thinking_level(model) {
        let level = options
            .metadata
            .as_ref()
            .and_then(|meta| meta.get("thinkingLevel"))
            .and_then(Value::as_str)
            .map(str::to_ascii_uppercase)
            .unwrap_or_else(|| to_api_thinking_level(resolved).to_string());
        config["thinkingLevel"] = json!(level);
    } else {
        let budgets = options
            .metadata
            .as_ref()
            .and_then(|meta| meta.get("thinkingBudgets"))
            .and_then(Value::as_object);
        config["thinkingBudget"] = json!(get_budget(model, resolved, budgets));
    }
    Ok(Some(config))
}

/// TS `isThinkingPart` (`google-shared.ts:130-132`). Only `thought: true`
/// marks thinking; `thoughtSignature` may appear on any part.
pub(crate) fn is_thinking_part(part: &Value) -> bool {
    part.get("thought").and_then(Value::as_bool) == Some(true)
}

/// TS `retainThoughtSignature` (`google-shared.ts:143-146`): a later delta that
/// omits the signature must not clear one seen earlier in the same block.
pub(crate) fn retain_thought_signature(
    existing: Option<&str>,
    incoming: Option<&str>,
) -> Option<String> {
    match incoming {
        Some(signature) if !signature.is_empty() => Some(signature.to_string()),
        _ => existing.map(str::to_string),
    }
}

fn is_valid_thought_signature(signature: &str) -> bool {
    if signature.is_empty() || signature.len() % 4 != 0 {
        return false;
    }
    let mut padding = 0;
    for byte in signature.bytes() {
        if byte == b'=' {
            padding += 1;
            if padding > 2 {
                return false;
            }
        } else if padding > 0 {
            return false;
        } else if !(byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/') {
            return false;
        }
    }
    true
}

fn resolve_thought_signature(
    is_same_provider_and_model: bool,
    signature: Option<&str>,
) -> Option<String> {
    if is_same_provider_and_model && signature.is_some_and(is_valid_thought_signature) {
        signature.map(str::to_string)
    } else {
        None
    }
}

fn gemini_major_version(model_id: &str) -> Option<u64> {
    let lower = model_id.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("gemini-live-")
        .or_else(|| lower.strip_prefix("gemini-"))?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// TS `requiresToolCallId` (`google-shared.ts:167-174`).
pub(crate) fn requires_tool_call_id(model_id: &str) -> bool {
    model_id.starts_with("claude-")
        || model_id.starts_with("gpt-oss-")
        || gemini_major_version(model_id).is_some_and(|major| major >= 3)
}

fn supports_multimodal_function_response(model_id: &str) -> bool {
    gemini_major_version(model_id).map_or(true, |major| major >= 3)
}

/// TS `supportsGoogleStrictToolSampling` (`google-shared.ts:463-466`).
pub(crate) fn supports_google_strict_tool_sampling(model_id: &str) -> bool {
    gemini_major_version(model_id).is_some_and(|major| major >= 3)
}

/// TS `mapStopReason` (`google-shared.ts:500-528`). Safety and every other
/// non-STOP/MAX_TOKENS reason map to `error`, which the adapter turns into a
/// terminal `error` event.
pub(crate) fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "STOP" => StopReason::Stop,
        "MAX_TOKENS" => StopReason::Length,
        _ => StopReason::Error,
    }
}

/// TS `mapToolChoice` / `resolveGoogleFunctionCallingMode`
/// (`google-shared.ts:469-495`). `ToolChoice::Auto` maps to `AUTO`; the
/// `metadata.toolChoice` seam additionally exposes `any`.
pub(crate) fn resolve_function_calling_mode(
    tools: &[Tool],
    options: &StreamOptions,
    supports_strict: bool,
) -> Result<Option<&'static str>, String> {
    let choice = match options.tool_choice {
        Some(ToolChoice::None) => Some("NONE"),
        Some(ToolChoice::Auto) => Some("AUTO"),
        None => options
            .metadata
            .as_ref()
            .and_then(|meta| meta.get("toolChoice"))
            .and_then(Value::as_str)
            .map(|value| match value {
                "none" => "NONE",
                "any" => "ANY",
                _ => "AUTO",
            }),
    };
    if matches!(choice, Some("NONE") | Some("ANY")) {
        return Ok(choice);
    }
    let use_strict = tools
        .iter()
        .any(|tool| resolve_json_schema_strict(tool, supports_strict).unwrap_or(false));
    if use_strict {
        return Ok(Some("VALIDATED"));
    }
    Ok(choice)
}

const JSON_SCHEMA_META_DECLARATIONS: [&str; 7] = [
    "$schema",
    "$id",
    "$anchor",
    "$dynamicAnchor",
    "$vocabulary",
    "$comment",
    "$defs",
];

const JSON_SCHEMA_NAME_MAPS: [&str; 6] = [
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
    "dependencies",
];

const JSON_SCHEMA_SUBSCHEMAS: [&str; 20] = [
    "additionalProperties",
    "unevaluatedProperties",
    "propertyNames",
    "items",
    "prefixItems",
    "additionalItems",
    "unevaluatedItems",
    "contains",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
    "contentSchema",
    "dependentSchemas",
    "dependencies",
    "patternProperties",
    "definitions",
];

const UNSUPPORTED_STRICT_SCHEMA_KEYS: [&str; 17] = [
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
    "nullable",
];

/// TS `sanitizeForOpenApi` (`google-shared.ts:361-372`).
fn sanitize_for_open_api(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut result = Map::new();
            for (key, value) in map {
                if !JSON_SCHEMA_META_DECLARATIONS.contains(&key.as_str()) {
                    result.insert(key.clone(), sanitize_for_open_api(value));
                }
            }
            Value::Object(result)
        }
        other => other.clone(),
    }
}

/// TS `rewriteConstAsEnum` (`google-shared.ts:408-428`): Gemini ignores
/// `const` in `parametersJsonSchema` but honors a single-value `enum`.
fn rewrite_const_as_enum(schema: &mut Value, is_name_map: bool) {
    match schema {
        Value::Array(items) => {
            for item in items {
                rewrite_const_as_enum(item, false);
            }
        }
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if JSON_SCHEMA_NAME_MAPS.contains(&key.as_str()) {
                    rewrite_const_as_enum(child, true);
                } else if JSON_SCHEMA_SUBSCHEMAS.contains(&key.as_str()) || is_name_map {
                    rewrite_const_as_enum(child, false);
                }
            }
            if !is_name_map && !map.contains_key("enum") {
                let as_enum = map
                    .get("const")
                    .is_some_and(|value| value.is_string() || value.is_number());
                if as_enum && let Some(value) = map.remove("const") {
                    map.insert("enum".to_string(), json!([value]));
                }
            }
        }
        _ => {}
    }
}

/// The strict JSON-schema subset Google's `VALIDATED` mode expects, mirroring
/// `makeStrictJsonSchema` (`constrained-sampling.ts:126-146`) for object roots.
fn make_strict_json_schema(schema: &Value) -> Result<Value, String> {
    fn node(schema: &Value) -> Result<Value, String> {
        let Value::Object(map) = schema else {
            return Err("boolean schemas are unsupported".to_string());
        };
        for key in UNSUPPORTED_STRICT_SCHEMA_KEYS {
            if map.contains_key(key) {
                return Err(format!("{key} schemas are unsupported"));
            }
        }
        let mut result = map.clone();
        if let Some(items) = result.get("items") {
            let strict_items = node(items)?;
            result.insert("items".to_string(), strict_items);
        }
        if let Some(any_of) = result.get("anyOf").and_then(Value::as_array) {
            let mut variants = Vec::with_capacity(any_of.len());
            for variant in any_of {
                variants.push(node(variant)?);
            }
            result.insert("anyOf".to_string(), Value::Array(variants));
        }
        if result.get("type").and_then(Value::as_str) == Some("object") {
            let properties = result
                .get("properties")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let mut names = Vec::with_capacity(properties.len());
            let mut strict_properties = Map::new();
            for (name, property) in &properties {
                names.push(Value::String(name.clone()));
                strict_properties.insert(name.clone(), node(property)?);
            }
            result.insert("properties".to_string(), Value::Object(strict_properties));
            result.insert("required".to_string(), Value::Array(names));
            result.insert("additionalProperties".to_string(), json!(false));
        }
        Ok(Value::Object(result))
    }
    let strict = node(schema)?;
    if strict.get("type").and_then(Value::as_str) != Some("object") {
        return Err("root schema must have type object".to_string());
    }
    Ok(strict)
}

/// TS `resolveJsonSchemaStrictSampling` (`constrained-sampling.ts:246-270`),
/// reduced to the decision the Google adapters consume.
fn resolve_json_schema_strict(tool: &Tool, supports_strict: bool) -> Result<Option<bool>, String> {
    let Some(ConstrainedSampling::JsonSchema { strict }) = tool.constrained_sampling.as_ref()
    else {
        return Ok(None);
    };
    let requires = *strict == StrictMode::Require;
    if !supports_strict {
        if requires {
            return Err(format!(
                "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported",
                tool.name
            ));
        }
        return Ok(None);
    }
    match make_strict_json_schema(&tool.parameters) {
        Ok(_) => Ok(Some(true)),
        Err(error) if requires => Err(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but {error}",
            tool.name
        )),
        Err(_) => Ok(None),
    }
}

/// TS `convertTools` (`google-shared.ts:439-460`).
pub(crate) fn convert_tools(
    tools: &[Tool],
    use_parameters: bool,
    supports_strict: bool,
) -> Result<Option<Value>, String> {
    if tools.is_empty() {
        return Ok(None);
    }
    let mut declarations = Vec::with_capacity(tools.len());
    for tool in tools {
        let strict = resolve_json_schema_strict(tool, supports_strict)? == Some(true);
        let mut parameters = if strict {
            make_strict_json_schema(&tool.parameters)?
        } else {
            tool.parameters.clone()
        };
        rewrite_const_as_enum(&mut parameters, false);
        let parameters = if use_parameters {
            sanitize_for_open_api(&parameters)
        } else {
            parameters
        };
        let mut declaration = Map::new();
        declaration.insert("name".to_string(), json!(tool.name));
        declaration.insert("description".to_string(), json!(tool.description));
        declaration.insert(
            if use_parameters {
                "parameters".to_string()
            } else {
                "parametersJsonSchema".to_string()
            },
            parameters,
        );
        declarations.push(Value::Object(declaration));
    }
    Ok(Some(json!([{ "functionDeclarations": declarations }])))
}

fn normalize_tool_call_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn image_part(image: &crate::types::ImageContent) -> Value {
    json!({ "inlineData": { "mimeType": image.mime_type, "data": image.data } })
}

fn flush_pending_tool_calls(
    contents: &mut Vec<Value>,
    pending: &mut Vec<ToolCall>,
    answered: &mut std::collections::HashSet<String>,
) {
    for call in pending.drain(..) {
        if answered.contains(&call.id) {
            continue;
        }
        let response = json!({
            "functionResponse": {
                "name": call.name,
                "response": { "error": "No result provided" },
            }
        });
        contents.push(json!({ "role": "user", "parts": [response] }));
    }
    answered.clear();
}

/// TS `convertMessages` (`google-shared.ts:193-345`), with the synthetic
/// orphan-tool-result pass of `transformMessages`
/// (`transform-messages.ts:179-255`) folded into the same loop.
#[allow(clippy::too_many_lines)] // mirrors the TS converter; split when extending
pub(crate) fn convert_messages(model: &Model, context: &TranscriptContext) -> Vec<Value> {
    let mut contents: Vec<Value> = Vec::new();
    let mut id_map: HashMap<String, String> = HashMap::new();
    let mut pending: Vec<ToolCall> = Vec::new();
    let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
    let multimodal = model
        .input
        .iter()
        .any(|input| matches!(input, crate::types::ModelInputModality::Image));

    for message in &context.messages {
        match message {
            Message::System(_) => {}
            Message::User(user) => {
                flush_pending_tool_calls(&mut contents, &mut pending, &mut answered);
                let parts: Vec<Value> = match &user.content {
                    UserContent::Text(text) => vec![json!({ "text": text })],
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .map(|block| match block {
                            InputContent::Text(text) => json!({ "text": text.text }),
                            InputContent::Image(image) => image_part(image),
                        })
                        .collect(),
                };
                if parts.is_empty() {
                    continue;
                }
                contents.push(json!({ "role": "user", "parts": parts }));
            }
            Message::Assistant(assistant) => {
                flush_pending_tool_calls(&mut contents, &mut pending, &mut answered);
                if matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    continue;
                }
                let is_same = assistant.provider == model.provider
                    && assistant.api == model.api
                    && assistant.model == model.id;
                let mut parts: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => {
                            let signature =
                                resolve_thought_signature(is_same, text.text_signature.as_deref());
                            if text.text.trim().is_empty() && signature.is_none() {
                                continue;
                            }
                            let mut part = json!({ "text": text.text });
                            if let Some(signature) = signature {
                                part["thoughtSignature"] = json!(signature);
                            }
                            parts.push(part);
                        }
                        AssistantContent::Thinking(thinking) => {
                            let signature = resolve_thought_signature(
                                is_same,
                                thinking.thinking_signature.as_deref(),
                            );
                            if thinking.thinking.trim().is_empty() && signature.is_none() {
                                continue;
                            }
                            let mut part = if is_same {
                                json!({ "thought": true, "text": thinking.thinking })
                            } else {
                                json!({ "text": thinking.thinking })
                            };
                            if let Some(signature) = signature {
                                part["thoughtSignature"] = json!(signature);
                            }
                            parts.push(part);
                        }
                        AssistantContent::ToolCall(call) => {
                            let id = if requires_tool_call_id(&model.id) {
                                let normalized = normalize_tool_call_id(&call.id);
                                id_map.insert(call.id.clone(), normalized.clone());
                                normalized
                            } else {
                                call.id.clone()
                            };
                            let mut function_call = json!({
                                "name": call.name,
                                "args": call.arguments,
                            });
                            if requires_tool_call_id(&model.id) {
                                function_call["id"] = json!(id);
                            }
                            let mut part = json!({ "functionCall": function_call });
                            if is_same
                                && let Some(signature) = resolve_thought_signature(
                                    true,
                                    call.thought_signature.as_deref(),
                                )
                            {
                                part["thoughtSignature"] = json!(signature);
                            }
                            parts.push(part);
                            pending.push(ToolCall {
                                id,
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                                thought_signature: None,
                                namespace: None,
                                arguments_error: None,
                            });
                        }
                    }
                }
                if parts.is_empty() {
                    continue;
                }
                contents.push(json!({ "role": "model", "parts": parts }));
            }
            Message::ToolResult(result) => {
                let tool_call_id = id_map
                    .get(&result.tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| result.tool_call_id.clone());
                answered.insert(tool_call_id.clone());
                let text = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        InputContent::Text(text) => Some(text.text.as_str()),
                        InputContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let images: Vec<&crate::types::ImageContent> = if multimodal {
                    result
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            InputContent::Image(image) => Some(image),
                            InputContent::Text(_) => None,
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let response_value = if !text.trim().is_empty() {
                    text
                } else if !images.is_empty() {
                    "(see attached image)".to_string()
                } else {
                    String::new()
                };
                let mut function_response = json!({
                    "name": result.tool_name,
                    "response": if result.is_error {
                        json!({ "error": response_value })
                    } else {
                        json!({ "output": response_value })
                    },
                });
                let image_parts: Vec<Value> =
                    images.iter().map(|image| image_part(image)).collect();
                if !image_parts.is_empty() && supports_multimodal_function_response(&model.id) {
                    function_response["parts"] = json!(image_parts);
                }
                if requires_tool_call_id(&model.id) {
                    function_response["id"] = json!(tool_call_id);
                }
                let part = json!({ "functionResponse": function_response });
                let merge = contents
                    .last_mut()
                    .and_then(Value::as_object_mut)
                    .is_some_and(|last| {
                        last.get("role").and_then(Value::as_str) == Some("user")
                            && last
                                .get("parts")
                                .and_then(Value::as_array)
                                .is_some_and(|parts| {
                                    parts
                                        .iter()
                                        .any(|part| part.get("functionResponse").is_some())
                                })
                    });
                if merge {
                    if let Some(parts) = contents
                        .last_mut()
                        .and_then(Value::as_object_mut)
                        .and_then(|last| last.get_mut("parts"))
                        .and_then(Value::as_array_mut)
                    {
                        parts.push(part);
                    }
                } else {
                    contents.push(json!({ "role": "user", "parts": [part] }));
                }
                if !image_parts.is_empty() && !supports_multimodal_function_response(&model.id) {
                    let mut parts = vec![json!({ "text": "Tool result image:" })];
                    parts.extend(image_parts);
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
        }
    }
    flush_pending_tool_calls(&mut contents, &mut pending, &mut answered);
    contents
}

/// TS `buildParams` (`google-generative-ai.ts:364-424`,
/// `google-vertex.ts:469-529`) assembled into the SDK's REST request body.
pub(crate) fn build_request_body(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
) -> Result<Value, String> {
    let tools = tools_at_point(&context.messages);
    let supports_strict = supports_google_strict_tool_sampling(&model.id);
    let system_instruction = context
        .messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| match message {
            Message::System(system) => {
                let text = if index == 0 {
                    system_text(system)
                } else {
                    system_update_text(system)
                };
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut generation_config = Map::new();
    if let Some(temperature) = options.temperature {
        generation_config.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(max_tokens) = options.max_tokens {
        generation_config.insert("maxOutputTokens".to_string(), json!(max_tokens));
    }
    if let Some(thinking) = thinking_config(model, options)? {
        generation_config.insert("thinkingConfig".to_string(), thinking);
    }

    let mut body = json!({
        "contents": convert_messages(model, context),
        "generationConfig": Value::Object(generation_config),
    });
    if !system_instruction.is_empty() {
        body["systemInstruction"] = json!({
            "role": "user",
            "parts": [{ "text": system_instruction }],
        });
    }
    if !tools.is_empty() {
        body["tools"] = convert_tools(&tools, false, supports_strict)?.unwrap_or_else(|| json!([]));
    }
    if let Some(mode) = resolve_function_calling_mode(&tools, options, supports_strict)? {
        body["toolConfig"] = json!({ "functionCallingConfig": { "mode": mode } });
    }
    // Additive seam: upstream's adapters do not set safety settings, but callers
    // can pass the SDK-shaped array through `metadata.safetySettings`.
    if let Some(safety) = options
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("safetySettings"))
    {
        body["safetySettings"] = safety.clone();
    }
    Ok(body)
}

/// TS `chunk.usageMetadata` mapping (both adapters, lines 232-251 / 247-266).
pub(crate) fn apply_chunk_usage(metadata: &Value, output: &mut AssistantMessage) {
    let field = |name: &str| metadata.get(name).and_then(Value::as_u64).unwrap_or(0);
    let cached = field("cachedContentTokenCount");
    let thoughts = field("thoughtsTokenCount");
    output.usage.input = field("promptTokenCount").saturating_sub(cached);
    output.usage.output = field("candidatesTokenCount").saturating_add(thoughts);
    output.usage.cache_read = cached;
    output.usage.cache_write = 0;
    output.usage.reasoning = Some(thoughts);
    output.usage.total_tokens = field("totalTokenCount");
}

/// Extract the human-readable message from a Google REST error body (TS
/// `normalizeProviderError` / `formatProviderError`, `error-body.ts:40-57,157`).
fn google_error_message(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body)
        && let Some(message) = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
    {
        return message.to_string();
    }
    body.to_string()
}

/// Emit the matching `text_end` / `thinking_end` for a currently open block.
async fn close_block(output: &AssistantMessage, sender: &EventSender, block: (bool, usize)) {
    let (is_thinking, index) = block;
    let event = if is_thinking {
        let content = match output.content.get(index) {
            Some(AssistantContent::Thinking(thinking)) => thinking.thinking.clone(),
            _ => String::new(),
        };
        AssistantMessageEvent::ThinkingEnd {
            content_index: index,
            content,
            partial: output.clone(),
        }
    } else {
        let content = match output.content.get(index) {
            Some(AssistantContent::Text(text)) => text.text.clone(),
            _ => String::new(),
        };
        AssistantMessageEvent::TextEnd {
            content_index: index,
            content,
            partial: output.clone(),
        }
    };
    let _ = sender.send(event).await;
}

/// Consume the SSE stream and emit Pi events (TS loops at
/// `google-generative-ai.ts:106-252` and `google-vertex.ts:122-267`). Both
/// adapters share this body byte-for-byte upstream.
#[allow(clippy::too_many_lines)] // mirrors the TS event loop; split when extending
pub(crate) async fn consume_stream<S>(
    mut sse: S,
    model: &Model,
    options: &StreamOptions,
    output: &mut AssistantMessage,
    sender: &EventSender,
) -> Result<(), String>
where
    S: futures::Stream<Item = Result<SseMessage, SseError>> + Send + Unpin,
{
    let mut current: Option<(bool, usize)> = None;
    while let Some(item) = sse.next().await {
        let message = match item {
            Ok(message) => message,
            Err(SseError::Status { status, body }) => {
                return Err(format!("HTTP {status}: {}", google_error_message(&body)));
            }
            Err(SseError::Http(message)) => return Err(message),
        };
        let data = message.data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let chunk: Value = serde_json::from_str(data)
            .map_err(|error| format!("could not parse Google SSE chunk: {error}"))?;
        apply_stream_event_hook(&options.on_provider_stream_event, chunk.clone(), model).await?;
        // Some relays wrap each response in a one-element array.
        let chunk = if chunk.is_array() {
            chunk.first().cloned().unwrap_or(chunk)
        } else {
            chunk
        };

        // The SDK throws on a streamed error chunk (`_api_client.ts`
        // `throwIfStreamErrorChunk`); upstream surfaces it through the catch.
        if let Some(error) = chunk.get("error")
            && let Some(code) = error.get("code").and_then(Value::as_i64)
            && (400..600).contains(&code)
        {
            let status = error
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("UNKNOWN");
            return Err(format!("got status: {status}. {chunk}"));
        }

        if let Some(id) = chunk.get("responseId").and_then(Value::as_str)
            && !id.is_empty()
            && output.response_id.is_none()
        {
            output.response_id = Some(id.to_string());
        }
        let candidate = chunk
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first());

        if let Some(parts) = candidate
            .and_then(|entry| entry.get("content"))
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    let is_thinking = is_thinking_part(part);
                    let needs_new = match current {
                        None => true,
                        Some((open_thinking, _)) => open_thinking != is_thinking,
                    };
                    if needs_new {
                        if let Some(block) = current.take() {
                            close_block(output, sender, block).await;
                        }
                        if is_thinking {
                            output
                                .content
                                .push(AssistantContent::Thinking(ThinkingContent {
                                    thinking: String::new(),
                                    thinking_signature: None,
                                    redacted: false,
                                }));
                        } else {
                            output.content.push(AssistantContent::Text(TextContent {
                                text: String::new(),
                                text_signature: None,
                            }));
                        }
                        let index = output.content.len() - 1;
                        current = Some((is_thinking, index));
                        let event = if is_thinking {
                            AssistantMessageEvent::ThinkingStart {
                                content_index: index,
                                partial: output.clone(),
                            }
                        } else {
                            AssistantMessageEvent::TextStart {
                                content_index: index,
                                partial: output.clone(),
                            }
                        };
                        let _ = sender.send(event).await;
                    }
                    let index = current.map_or(0, |(_, index)| index);
                    let signature = part.get("thoughtSignature").and_then(Value::as_str);
                    if is_thinking {
                        if let Some(AssistantContent::Thinking(block)) =
                            output.content.get_mut(index)
                        {
                            block.thinking.push_str(text);
                            block.thinking_signature = retain_thought_signature(
                                block.thinking_signature.as_deref(),
                                signature,
                            );
                        }
                        let _ = sender
                            .send(AssistantMessageEvent::ThinkingDelta {
                                content_index: index,
                                delta: text.to_string(),
                                partial: output.clone(),
                            })
                            .await;
                    } else {
                        if let Some(AssistantContent::Text(block)) = output.content.get_mut(index) {
                            block.text.push_str(text);
                            block.text_signature = retain_thought_signature(
                                block.text_signature.as_deref(),
                                signature,
                            );
                        }
                        let _ = sender
                            .send(AssistantMessageEvent::TextDelta {
                                content_index: index,
                                delta: text.to_string(),
                                partial: output.clone(),
                            })
                            .await;
                    }
                }

                if let Some(function_call) =
                    part.get("functionCall").filter(|value| value.is_object())
                {
                    if let Some(block) = current.take() {
                        close_block(output, sender, block).await;
                    }
                    let name = function_call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let provided_id = function_call.get("id").and_then(Value::as_str);
                    let duplicate = provided_id.is_some_and(|id| {
                        output.content.iter().any(|block| {
                            matches!(block, AssistantContent::ToolCall(call) if call.id == id)
                        })
                    });
                    let id = match provided_id {
                        Some(id) if !id.is_empty() && !duplicate => id.to_string(),
                        _ => {
                            let suffix = NEXT_TOOL_CALL_ID.fetch_add(1, Ordering::Relaxed) + 1;
                            format!("{name}_{}_{suffix}", now_ms())
                        }
                    };
                    let arguments = function_call
                        .get("args")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    let thought_signature = part
                        .get("thoughtSignature")
                        .and_then(Value::as_str)
                        .filter(|signature| !signature.is_empty())
                        .map(str::to_string);
                    let tool_call = ToolCall {
                        id,
                        name: name.to_string(),
                        arguments,
                        thought_signature,
                        namespace: None,
                        arguments_error: None,
                    };
                    let index = output.content.len();
                    output
                        .content
                        .push(AssistantContent::ToolCall(tool_call.clone()));
                    let _ = sender
                        .send(AssistantMessageEvent::ToolcallStart {
                            content_index: index,
                            partial: output.clone(),
                        })
                        .await;
                    let delta = serde_json::to_string(&tool_call.arguments)
                        .unwrap_or_else(|_| "{}".to_string());
                    let _ = sender
                        .send(AssistantMessageEvent::ToolcallDelta {
                            content_index: index,
                            delta,
                            partial: output.clone(),
                        })
                        .await;
                    let _ = sender
                        .send(AssistantMessageEvent::ToolcallEnd {
                            content_index: index,
                            tool_call,
                            partial: output.clone(),
                        })
                        .await;
                }
            }
        }

        if let Some(reason) = candidate
            .and_then(|entry| entry.get("finishReason"))
            .and_then(Value::as_str)
            && !reason.is_empty()
        {
            output.raw_stop_reason = Some(reason.to_string());
            output.stop_reason = map_stop_reason(reason);
            if output.stop_reason == StopReason::Stop
                && output
                    .content
                    .iter()
                    .any(|block| matches!(block, AssistantContent::ToolCall(_)))
            {
                output.stop_reason = StopReason::ToolUse;
            }
        }
        if let Some(usage) = chunk.get("usageMetadata").filter(|value| value.is_object()) {
            apply_chunk_usage(usage, output);
            // `finalize` fills catalog prices after the loop; do it here too
            // so partial snapshots carry cost like upstream's `calculateCost`.
            calculate_cost(&mut output.usage, &model.cost);
        }
    }

    if let Some(block) = current.take() {
        close_block(output, sender, block).await;
    }
    if output.stop_reason == StopReason::Pending {
        return Err("Google stream ended without a finish reason".to_string());
    }
    if matches!(output.stop_reason, StopReason::Error | StopReason::Aborted) {
        return Err(output.raw_stop_reason.as_ref().map_or_else(
            || "An unknown error occurred".to_string(),
            |reason| format!("Provider stopped with: {reason}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_stream::AssistantMessageEventStream;
    use crate::sse::SseParser;
    use crate::types::{
        ModelCost, ModelInputModality, StringOrTextBlocks, SystemMessage, UserMessage,
    };
    use serde_json::json;

    fn test_model() -> Model {
        Model {
            id: "gemini-2.5-flash".into(),
            name: "Gemini 2.5 Flash".into(),
            api: "google-generative-ai".into(),
            provider: "google".into(),
            base_url: String::new(),
            input: vec![ModelInputModality::Text],
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                ..Default::default()
            },
            context_window: 1_000_000,
            max_tokens: 8192,
            ..Default::default()
        }
    }

    fn sse_events(body: &str) -> Vec<Result<SseMessage, SseError>> {
        let mut parser = SseParser::default();
        let mut messages = parser.feed(body.as_bytes());
        messages.extend(parser.finish());
        messages.into_iter().map(Ok).collect()
    }

    async fn drive(body: &str) -> (AssistantMessage, Vec<AssistantMessageEvent>) {
        let (stream, sender) = AssistantMessageEventStream::channel();
        let model = test_model();
        let mut output = crate::providers::common::initial_output(&model);
        let result = consume_stream(
            tokio_stream::iter(sse_events(body)),
            &model,
            &StreamOptions::default(),
            &mut output,
            &sender,
        )
        .await;
        drop(sender);
        let mut events = Vec::new();
        let mut stream = stream;
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        if let Err(error) = result {
            output.error_message = Some(error);
            output.stop_reason = StopReason::Error;
        }
        (output, events)
    }

    fn event_kinds(events: &[AssistantMessageEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|event| match event {
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
            })
            .collect()
    }

    #[tokio::test]
    async fn text_only_sequence_and_usage() {
        let body = concat!(
            "data: {\"responseId\":\"resp-1\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello \"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"world\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":12,\"cachedContentTokenCount\":4,\"candidatesTokenCount\":5,\"thoughtsTokenCount\":3,\"totalTokenCount\":20}}\n\n",
        );
        let (output, events) = drive(body).await;
        assert_eq!(
            event_kinds(&events),
            vec!["text_start", "text_delta", "text_delta", "text_end",]
        );
        assert_eq!(output.stop_reason, StopReason::Stop);
        assert_eq!(output.response_id.as_deref(), Some("resp-1"));
        assert_eq!(output.content.len(), 1);
        assert_eq!(output.content[0].as_text(), Some("Hello world"));
        assert_eq!(output.usage.input, 8);
        assert_eq!(output.usage.output, 8);
        assert_eq!(output.usage.cache_read, 4);
        assert_eq!(output.usage.reasoning, Some(3));
        assert_eq!(output.usage.total_tokens, 20);
    }

    #[tokio::test]
    async fn thinking_then_text_with_signature_retention() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"thought\":true,\"text\":\"reason\",\"thoughtSignature\":\"YWJjZA==\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"thought\":true,\"text\":\"ing\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"answer\"}]}}],\"usageMetadata\":{}}\n\n",
            "data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n",
        );
        let (output, events) = drive(body).await;
        assert_eq!(
            event_kinds(&events),
            vec![
                "thinking_start",
                "thinking_delta",
                "thinking_delta",
                "thinking_end",
                "text_start",
                "text_delta",
                "text_end",
            ]
        );
        let Some(AssistantContent::Thinking(thinking)) = output.content.first() else {
            panic!("expected thinking block")
        };
        assert_eq!(thinking.thinking, "reasoning");
        assert_eq!(thinking.thinking_signature.as_deref(), Some("YWJjZA=="));
    }

    #[tokio::test]
    async fn tool_call_sequence_and_tool_use_stop() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"one moment\"},{\"functionCall\":{\"name\":\"bash\",\"args\":{\"cmd\":\"ls\"}}}]}}]}\n\n",
            "data: {\"candidates\":[{\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":1,\"totalTokenCount\":4}}\n\n",
        );
        let (output, events) = drive(body).await;
        assert_eq!(
            event_kinds(&events),
            vec![
                "text_start",
                "text_delta",
                "text_end",
                "toolcall_start",
                "toolcall_delta",
                "toolcall_end",
            ]
        );
        assert_eq!(output.stop_reason, StopReason::ToolUse);
        let Some(AssistantContent::ToolCall(call)) = output.content.get(1) else {
            panic!("expected tool call")
        };
        assert_eq!(call.name, "bash");
        assert_eq!(
            call.arguments.get("cmd").and_then(Value::as_str),
            Some("ls")
        );
        assert!(call.id.starts_with("bash_"));
    }

    #[tokio::test]
    async fn error_mapping_from_stream_and_finish_reason() {
        let stream_error = concat!(
            "data: {\"error\":{\"code\":429,\"status\":\"RESOURCE_EXHAUSTED\",\"message\":\"quota\"}}\n\n",
        );
        let (output, _) = drive(stream_error).await;
        assert_eq!(output.stop_reason, StopReason::Error);
        assert!(
            output
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("RESOURCE_EXHAUSTED")),
            "{:?}",
            output.error_message
        );
        assert_eq!(map_stop_reason("SAFETY"), StopReason::Error);
        assert_eq!(map_stop_reason("MAX_TOKENS"), StopReason::Length);

        let safety = "data: {\"candidates\":[{\"finishReason\":\"SAFETY\"}]}\n\n";
        let (output, _) = drive(safety).await;
        assert_eq!(
            output.error_message.as_deref(),
            Some("Provider stopped with: SAFETY")
        );
    }

    #[tokio::test]
    async fn missing_finish_reason_is_an_error() {
        let (output, _) =
            drive("data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\n\n")
                .await;
        assert_eq!(output.stop_reason, StopReason::Error);
        assert_eq!(
            output.error_message.as_deref(),
            Some("Google stream ended without a finish reason")
        );
    }

    #[test]
    fn thinking_level_helpers() {
        let mut model = test_model();
        model.reasoning = true;
        assert_eq!(
            clamp_thinking_level(&model, ThinkingLevel::High),
            ThinkingLevel::High
        );
        assert_eq!(
            resolve_google_thinking_level(&model, ThinkingLevel::Medium).unwrap(),
            ResolvedThinkingLevel::Medium
        );
        assert!(!uses_google_thinking_level(&model));
        assert_eq!(
            get_budget(&model, ResolvedThinkingLevel::High, None),
            24_576
        );
        assert_eq!(
            get_disabled_thinking_config(&model),
            json!({ "thinkingBudget": 0 })
        );

        model.id = "gemini-3-flash-preview".into();
        model.thinking_level_map = Some(json!({ "high": "MEDIUM" }).as_object().cloned().unwrap());
        assert!(uses_google_thinking_level(&model));
        assert_eq!(
            resolve_google_thinking_level(&model, ThinkingLevel::High).unwrap(),
            ResolvedThinkingLevel::Medium
        );
        assert_eq!(get_budget(&model, ResolvedThinkingLevel::High, None), -1);
    }

    #[test]
    fn request_body_shape() {
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
            temperature: Some(0.5),
            max_tokens: Some(128),
            reasoning: Some(ThinkingLevel::Low),
            ..Default::default()
        };
        let body = build_request_body(&model, &context, &options).unwrap();
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Be terse.");
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["generationConfig"]["temperature"], 0.5);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 128);
    }

    #[test]
    fn convert_messages_preserves_tool_results_and_thought_signatures() {
        let model = test_model();
        let assistant = crate::types::AssistantMessage {
            content: vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "plan".into(),
                    thinking_signature: Some("YWJjZA==".into()),
                    redacted: false,
                }),
                AssistantContent::ToolCall(ToolCall {
                    id: "call|1".into(),
                    name: "bash".into(),
                    arguments: json!({"cmd":"ls"}).as_object().cloned().unwrap(),
                    thought_signature: None,
                    namespace: None,
                    arguments_error: None,
                }),
            ],
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: Vec::new(),
            usage: Default::default(),
            stop_reason: StopReason::ToolUse,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
            duration_ms: None,
        };
        let context = TranscriptContext {
            messages: vec![
                Message::Assistant(assistant),
                Message::ToolResult(crate::types::ToolResultMessage {
                    tool_call_id: "call|1".into(),
                    tool_name: "bash".into(),
                    content: vec![InputContent::Text(TextContent {
                        text: "file.txt".into(),
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
        let contents = convert_messages(&model, &context);
        assert_eq!(contents[0]["role"], "model");
        assert_eq!(contents[0]["parts"][0]["thought"], true);
        assert_eq!(contents[0]["parts"][0]["thoughtSignature"], "YWJjZA==");
        assert_eq!(contents[1]["role"], "user");
        assert_eq!(
            contents[1]["parts"][0]["functionResponse"]["response"]["output"],
            "file.txt"
        );
    }

    #[test]
    fn convert_tools_rewrites_const_as_enum() {
        let tool = Tool {
            name: "pick".into(),
            description: "pick one".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "const": "a" },
                    "count": { "type": "number", "const": 3 }
                }
            }),
            constrained_sampling: None,
        };
        let tools = convert_tools(std::slice::from_ref(&tool), false, false)
            .unwrap()
            .unwrap();
        let properties = &tools[0]["functionDeclarations"][0]["parametersJsonSchema"]["properties"];
        assert_eq!(properties["kind"]["enum"][0], "a");
        assert_eq!(properties["count"]["enum"][0], 3);
        assert!(properties["kind"].get("const").is_none());
    }

    #[test]
    fn grounding_metadata_is_left_to_the_provider_hook() {
        // Upstream does not normalize grounding metadata into content; the
        // observer receives the whole chunk before the loop maps it.
        let chunk = json!({
            "candidates": [{
                "content": { "parts": [{ "text": "hi" }] },
                "groundingMetadata": { "groundingChunks": [] },
                "finishReason": "STOP"
            }]
        });
        let model = test_model();
        let (stream, sender) = AssistantMessageEventStream::channel();
        let mut output = crate::providers::common::initial_output(&model);
        let sse = tokio_stream::iter(vec![Ok(SseMessage {
            event: None,
            data: chunk.to_string(),
            fields: HashMap::new(),
        })]);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            consume_stream(sse, &model, &StreamOptions::default(), &mut output, &sender)
                .await
                .unwrap();
        });
        drop(sender);
        let mut stream = stream;
        rt.block_on(async {
            assert!(matches!(
                stream.next().await,
                Some(AssistantMessageEvent::TextStart { .. })
            ));
            assert!(matches!(
                stream.next().await,
                Some(AssistantMessageEvent::TextDelta { .. })
            ));
            assert!(matches!(
                stream.next().await,
                Some(AssistantMessageEvent::TextEnd { .. })
            ));
        });
        assert_eq!(output.content.len(), 1);
    }
}
