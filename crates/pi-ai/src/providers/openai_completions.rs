//! `OpenAI` Chat Completions adapter (port of TS `api/openai-completions.ts`,
//! core streaming path). Covers the `OpenAI` wire format shared by the many
//! compatible providers (llama.cpp, vLLM, `OpenRouter`, Groq, …).
//!
//! P1 scope: bearer auth, request building (messages/tools/temperature/
//! `max_tokens`/`stream_options`), SSE streaming with `reasoning_content` support,
//! tool-call delta accumulation, finish-reason and usage mapping.
//! The `onPayload`/`onResponse`/`onProviderStreamEvent` bridges mirror
//! `api/openai-completions.ts:375-378,392,545`.
//! The resolved compat matrix ports `detectCompat`/`getCompat` and the payload
//! flags `buildParams` applies (`openai-completions.ts:761-970,1588-1739`):
//! `maxTokensField`, `supportsDeveloperRole`, `supportsStore` and the main
//! `thinkingFormat` variants. Catalog `model.headers` are merged into requests
//! (`openai-completions.ts:751,773-776`).
//! Deferred: grammar tools, prompt-cache keys, strict tool schemas.
//! `OpenAI`-wire-compatible providers: llama.cpp, vLLM, `OpenRouter`, Groq, …

use crate::event_stream::{AssistantMessageEventStream, EventSender};
use crate::providers::common::{
    apply_headers, apply_payload_hook, apply_response_hook, apply_stream_event_hook, finalize,
    initial_output, is_leading_system, request_headers, response_metadata, send_with_retry,
    system_text, system_update_text, tools_at_point,
};
use crate::sse::{SseError, read_sse_stream};
use crate::streaming_json::parse_tool_call_arguments;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, InputContent, JsonObject, Message,
    Model, StopReason, StreamOptions, ThinkingLevel, ToolCall, TranscriptContext, UserContent,
    now_ms,
};
use futures::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;

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
    let started = output.timestamp;

    let result = request_and_stream(model, context, options, &mut output, &sender).await;
    finalize(&mut output, model);
    output.duration_ms = Some(u64::try_from(now_ms() - started).unwrap_or(0));

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

type StreamResult<T> = Result<T, String>;

/// Wire field chosen for a request's output cap (`openai-completions.ts:834-841`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
}

/// The payload-shaping subset of upstream `ResolvedOpenAICompletionsCompat`
/// (`openai-completions.ts:1644-1691`), with explicit `model.compat` overrides
/// layered on top (`openai-completions.ts:1698-1739`).
#[derive(Debug, Clone)]
struct OpenAiCompat {
    max_tokens_field: MaxTokensField,
    supports_developer_role: bool,
    supports_store: bool,
    supports_reasoning_effort: bool,
    thinking_format: String,
}

fn compat_bool(compat: Option<&JsonObject>, key: &str) -> Option<bool> {
    compat?.get(key).and_then(Value::as_bool)
}

fn compat_str(compat: Option<&JsonObject>, key: &str) -> Option<String> {
    compat?.get(key).and_then(Value::as_str).map(str::to_string)
}

/// `detectCompat` + `getCompat` (`openai-completions.ts:1593-1739`).
fn resolve_compat(model: &Model) -> OpenAiCompat {
    let provider = model.provider.as_str();
    let base = model.base_url.as_str();
    let has = |needle: &str| base.contains(needle);

    let is_zai = provider == "zai"
        || provider == "zai-coding-cn"
        || has("api.z.ai")
        || has("open.bigmodel.cn");
    let is_together = provider == "together" || has("api.together.ai") || has("api.together.xyz");
    let is_moonshot =
        provider == "moonshotai" || provider == "moonshotai-cn" || has("api.moonshot.");
    let is_openrouter = provider == "openrouter" || has("openrouter.ai");
    let is_cloudflare_workers = provider == "cloudflare-workers-ai" || has("api.cloudflare.com");
    let is_cloudflare_gateway =
        provider == "cloudflare-ai-gateway" || has("gateway.ai.cloudflare.com");
    let is_nvidia = provider == "nvidia" || has("integrate.api.nvidia.com");
    let is_ant_ling = provider == "ant-ling" || has("api.ant-ling.com");
    let is_cerebras = provider == "cerebras" || has("cerebras.ai");
    let is_deepseek = provider == "deepseek" || base.to_ascii_lowercase().contains("deepseek.com");
    let is_grok = provider == "xai" || has("api.x.ai");

    let is_non_standard = is_nvidia
        || is_cerebras
        || is_grok
        || is_together
        || has("chutes.ai")
        || is_deepseek
        || is_zai
        || is_moonshot
        || provider == "opencode"
        || has("opencode.ai")
        || is_cloudflare_workers
        || is_cloudflare_gateway
        || is_ant_ling;
    let use_max_tokens = has("chutes.ai")
        || is_deepseek
        || is_moonshot
        || is_cloudflare_gateway
        || is_together
        || is_nvidia
        || is_ant_ling
        || is_zai;
    let is_openrouter_dev =
        is_openrouter && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/"));

    let detected_thinking = if is_deepseek {
        "deepseek"
    } else if is_zai {
        "zai"
    } else if is_together {
        "together"
    } else if is_ant_ling {
        "ant-ling"
    } else if is_openrouter {
        "openrouter"
    } else {
        "openai"
    };

    let max_tokens_field = match compat_str(model.compat.as_ref(), "maxTokensField").as_deref() {
        Some("max_tokens") => MaxTokensField::MaxTokens,
        None if use_max_tokens => MaxTokensField::MaxTokens,
        Some(_) | None => MaxTokensField::MaxCompletionTokens,
    };

    OpenAiCompat {
        max_tokens_field,
        supports_developer_role: compat_bool(model.compat.as_ref(), "supportsDeveloperRole")
            .unwrap_or(is_openrouter_dev || (!is_non_standard && !is_openrouter)),
        supports_store: compat_bool(model.compat.as_ref(), "supportsStore")
            .unwrap_or(!is_non_standard),
        supports_reasoning_effort: compat_bool(model.compat.as_ref(), "supportsReasoningEffort")
            .unwrap_or(
                !is_grok
                    && !is_zai
                    && !is_moonshot
                    && !is_together
                    && !is_cloudflare_gateway
                    && !is_nvidia
                    && !is_ant_ling,
            ),
        thinking_format: compat_str(model.compat.as_ref(), "thinkingFormat")
            .unwrap_or_else(|| detected_thinking.to_string()),
    }
}

fn level_key(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Off => "off",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
    }
}

/// `model.thinkingLevelMap[level] ?? level` (`openai-completions.ts:934,951`).
fn mapped_effort(model: &Model, level: ThinkingLevel) -> String {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(level_key(level)))
        .and_then(Value::as_str)
        .unwrap_or(level_key(level))
        .to_string()
}

/// Streaming state for text/thinking/tool-call blocks.
#[derive(Default)]
struct StreamState {
    thinking_open: Option<usize>,
    text_open: Option<usize>,
    tool_indices: std::collections::HashMap<u64, usize>,
    tool_args_raw: std::collections::HashMap<usize, String>,
}

#[allow(clippy::too_many_lines)] // mirrors the TS event loop shape; split when extending
async fn request_and_stream(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
    output: &mut AssistantMessage,
    sender: &EventSender,
) -> StreamResult<()> {
    // OpenAI-style key auth is `Authorization: Bearer`; a resolved bearer
    // token (kimi-coding's OAuth `toAuth.headers`) takes precedence, then the
    // key, then the ambient env fallback.
    let bearer = options
        .access_token
        .clone()
        .or_else(|| options.api_key.clone())
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .ok_or_else(|| "OpenAI API key not set (set OPENAI_API_KEY)".to_string())?;

    // `onPayload` runs on the fully built payload, before the request is sent;
    // a replacement payload (if any) is what actually goes over the wire.
    let body = build_request_body(model, context, options)?;
    let body = apply_payload_hook(&options.on_payload, body, model).await?;
    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| format!("client build failed: {e}"))?;
    // Upstream's createClient merges identity headers, `model.headers` and
    // options headers before handing them to the SDK
    // (`openai-completions.ts:751,773-776`). Catalog headers supply Copilot's
    // User-Agent/Editor-* and NVIDIA's NVCF-POLL-SECONDS.
    let auth = format!("Bearer {bearer}");
    let headers = request_headers(
        &[
            ("content-type", "application/json"),
            ("authorization", &auth),
        ],
        model,
        options,
    );
    let response = send_with_retry(
        || apply_headers(client.post(&url).json(&body), headers.clone()),
        options,
    )
    .await?;

    // `onResponse` sees the raw status/headers before the body is consumed
    // (and before the `start` event). Upstream's SDK throws on non-2xx before
    // `withResponse()` returns, so the hook is only reached on success here.
    if response.status().is_success() {
        apply_response_hook(&options.on_response, response_metadata(&response), model).await?;
    }

    let sse = read_sse_stream(response).await.map_err(|e| e.to_string())?;
    let _ = sender
        .send(AssistantMessageEvent::Start {
            partial: output.clone(),
        })
        .await;

    let mut state = StreamState::default();
    let mut has_finish_reason = false;

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
        // The OpenAI SDK yields a `[DONE]` sentinel to its iterator before Pi
        // normalizes chunks; the observer sees it too (TS line 545).
        if data == "[DONE]" {
            apply_stream_event_hook(
                &options.on_provider_stream_event,
                Value::String("[DONE]".to_string()),
                model,
            )
            .await?;
            break;
        }
        let chunk: Value = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(error) => return Err(format!("could not parse OpenAI SSE chunk: {error}")),
        };
        apply_stream_event_hook(&options.on_provider_stream_event, chunk.clone(), model).await?;

        if let Some(id) = chunk.get("id").and_then(Value::as_str)
            && output.response_id.is_none()
        {
            output.response_id = Some(id.to_string());
        }
        if let Some(chunk_model) = chunk.get("model").and_then(Value::as_str)
            && !chunk_model.is_empty()
            && chunk_model != model.id
            && output.response_model.is_none()
        {
            output.response_model = Some(chunk_model.to_string());
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            apply_chunk_usage(usage, output);
        }

        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            continue;
        };
        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
            output.raw_stop_reason = Some(finish.to_string());
            let (stop, error_message) = map_finish_reason(finish);
            output.stop_reason = stop;
            if let Some(error_message) = error_message {
                output.error_message = Some(error_message);
            }
            has_finish_reason = true;
        }

        let Some(delta) = choice.get("delta") else {
            continue;
        };

        // Reasoning first: proxies may aggregate reasoning and answer in one delta.
        for field in ["reasoning_content", "reasoning", "reasoning_text"] {
            if let Some(text) = delta.get(field).and_then(Value::as_str) {
                if text.is_empty() {
                    continue;
                }
                let index = if let Some(index) = state.thinking_open {
                    index
                } else {
                    output.content.push(AssistantContent::Thinking(
                        crate::types::ThinkingContent {
                            thinking: String::new(),
                            thinking_signature: Some(field.to_string()),
                            redacted: false,
                        },
                    ));
                    let index = output.content.len() - 1;
                    state.thinking_open = Some(index);
                    let _ = sender
                        .send(AssistantMessageEvent::ThinkingStart {
                            content_index: index,
                            partial: output.clone(),
                        })
                        .await;
                    index
                };
                if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(index) {
                    block.thinking += text;
                    let _ = sender
                        .send(AssistantMessageEvent::ThinkingDelta {
                            content_index: index,
                            delta: text.to_string(),
                            partial: output.clone(),
                        })
                        .await;
                }
                break;
            }
        }

        if let Some(text) = delta.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            let index = if let Some(index) = state.text_open {
                index
            } else {
                output
                    .content
                    .push(AssistantContent::Text(crate::types::TextContent {
                        text: String::new(),
                        text_signature: None,
                    }));
                let index = output.content.len() - 1;
                state.text_open = Some(index);
                let _ = sender
                    .send(AssistantMessageEvent::TextStart {
                        content_index: index,
                        partial: output.clone(),
                    })
                    .await;
                index
            };
            if let Some(AssistantContent::Text(block)) = output.content.get_mut(index) {
                block.text += text;
                let _ = sender
                    .send(AssistantMessageEvent::TextDelta {
                        content_index: index,
                        delta: text.to_string(),
                        partial: output.clone(),
                    })
                    .await;
            }
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call_delta in tool_calls {
                let wire_index = call_delta.get("index").and_then(Value::as_u64).unwrap_or(0);
                let content_index = if let Some(index) = state.tool_indices.get(&wire_index) {
                    *index
                } else {
                    let id = call_delta
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let name = call_delta
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    output.content.push(AssistantContent::ToolCall(ToolCall {
                        id: id.to_string(),
                        name: name.to_string(),
                        arguments: serde_json::Map::new(),
                        thought_signature: None,
                        namespace: None,
                        arguments_error: None,
                    }));
                    let index = output.content.len() - 1;
                    state.tool_indices.insert(wire_index, index);
                    state.tool_args_raw.insert(index, String::new());
                    let _ = sender
                        .send(AssistantMessageEvent::ToolcallStart {
                            content_index: index,
                            partial: output.clone(),
                        })
                        .await;
                    index
                };
                // Later chunks may still carry id/name before arguments start.
                if let Some(AssistantContent::ToolCall(block)) =
                    output.content.get_mut(content_index)
                {
                    if block.id.is_empty()
                        && let Some(id) = call_delta.get("id").and_then(Value::as_str)
                    {
                        block.id = id.to_string();
                    }
                    if block.name.is_empty()
                        && let Some(name) = call_delta
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str)
                    {
                        block.name = name.to_string();
                    }
                }
                if let Some(arguments) = call_delta
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    && !arguments.is_empty()
                {
                    if let Some(raw) = state.tool_args_raw.get_mut(&content_index) {
                        raw.push_str(arguments);
                        if let Some(AssistantContent::ToolCall(block)) =
                            output.content.get_mut(content_index)
                        {
                            block.arguments = parse_tool_call_arguments(raw).unwrap_or_default();
                        }
                    }
                    let _ = sender
                        .send(AssistantMessageEvent::ToolcallDelta {
                            content_index,
                            delta: arguments.to_string(),
                            partial: output.clone(),
                        })
                        .await;
                }
            }
        }
    }

    // Close open blocks with their terminal events.
    if let Some(index) = state.thinking_open
        && let Some(AssistantContent::Thinking(block)) = output.content.get(index)
    {
        let _ = sender
            .send(AssistantMessageEvent::ThinkingEnd {
                content_index: index,
                content: block.thinking.clone(),
                partial: output.clone(),
            })
            .await;
    }
    if let Some(index) = state.text_open
        && let Some(AssistantContent::Text(block)) = output.content.get(index)
    {
        let _ = sender
            .send(AssistantMessageEvent::TextEnd {
                content_index: index,
                content: block.text.clone(),
                partial: output.clone(),
            })
            .await;
    }
    for index in state.tool_args_raw.keys().copied().collect::<Vec<_>>() {
        let raw = state.tool_args_raw.get(&index).cloned().unwrap_or_default();
        if let Some(AssistantContent::ToolCall(block)) = output.content.get_mut(index) {
            match parse_tool_call_arguments(&raw) {
                Ok(arguments) => block.arguments = arguments,
                Err(error) => {
                    block.arguments = serde_json::Map::new();
                    block.arguments_error = Some(error);
                }
            }
            let final_block = block.clone();
            let _ = sender
                .send(AssistantMessageEvent::ToolcallEnd {
                    content_index: index,
                    tool_call: final_block,
                    partial: output.clone(),
                })
                .await;
        }
    }

    let _ = has_finish_reason;
    if output.stop_reason == StopReason::Pending {
        return Err("OpenAI stream ended without a finish reason".to_string());
    }
    if matches!(output.stop_reason, StopReason::Error | StopReason::Aborted) {
        return Err(output
            .error_message
            .clone()
            .unwrap_or_else(|| "An unknown error occurred".to_string()));
    }
    Ok(())
}

/// TS `mapStopReason` for Chat Completions finish reasons.
fn map_finish_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "function_call" | "tool_calls" => (StopReason::ToolUse, None),
        "content_filter" => (
            StopReason::Error,
            Some("Provider finish_reason: content_filter".into()),
        ),
        "network_error" => (
            StopReason::Error,
            Some("Provider finish_reason: network_error".into()),
        ),
        other => (
            StopReason::Error,
            Some(format!("Provider finish_reason: {other}")),
        ),
    }
}

/// Apply `chunk.usage` (`OpenAI` numbering; `cache_read` from cached prompt tokens).
fn apply_chunk_usage(usage: &Value, output: &mut AssistantMessage) {
    let field = |name: &str| usage.get(name).and_then(Value::as_u64);
    if let Some(input) = field("prompt_tokens") {
        output.usage.input = input;
    }
    if let Some(out) = field("completion_tokens") {
        output.usage.output = out;
    }
    if let Some(total) = field("total_tokens") {
        output.usage.total_tokens = total;
    } else {
        output.usage.total_tokens = output.usage.input + output.usage.output;
    }
    if let Some(cached) = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
    {
        output.usage.cache_read = cached;
    }
    if let Some(reasoning) = usage
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
    {
        output.usage.reasoning = Some(reasoning);
    }
}

/// Build the Chat Completions request body (P1 core path).
#[allow(clippy::too_many_lines)] // request assembly; split if it grows further
pub fn build_request_body(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
) -> StreamResult<Value> {
    let messages = &context.messages;
    let mut params: Vec<Value> = Vec::new();
    // Tools are declared by system messages inside the transcript.
    let tools = tools_at_point(messages);
    let compat = resolve_compat(model);
    // Reasoning models with developer-role support use `developer` instead of
    // `system` for instructions (`openai-completions.ts:1226,1254`).
    let instruction_role = if model.reasoning && compat.supports_developer_role {
        "developer"
    } else {
        "system"
    };

    for (i, msg) in messages.iter().enumerate() {
        match msg {
            Message::System(system) => {
                let text = if is_leading_system(messages, i) {
                    system_text(system)
                } else {
                    system_update_text(system)
                };
                if text.is_empty() {
                    continue;
                }
                params.push(json!({ "role": instruction_role, "content": text }));
            }
            Message::User(user) => match &user.content {
                UserContent::Text(text) => {
                    if !text.trim().is_empty() {
                        params.push(json!({ "role": "user", "content": text }));
                    }
                }
                UserContent::Blocks(blocks) => {
                    let converted: Vec<Value> = blocks.iter().map(input_block).collect();
                    if !converted.is_empty() {
                        params.push(json!({ "role": "user", "content": converted }));
                    }
                }
            },
            Message::Assistant(assistant) => {
                let mut text_parts: Vec<String> = Vec::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => text_parts.push(text.text.clone()),
                        AssistantContent::Thinking(_) => {
                            // Chat Completions has no thinking replay; skipped (P1).
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
                            }));
                        }
                    }
                }
                if text_parts.is_empty() && tool_calls.is_empty() {
                    continue;
                }
                let mut message = json!({
                    "role": "assistant",
                    "content": text_parts.join(""),
                });
                if !tool_calls.is_empty() {
                    message["tool_calls"] = Value::Array(tool_calls);
                }
                params.push(message);
            }
            Message::ToolResult(result) => {
                let text = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        InputContent::Text(text) => Some(text.text.clone()),
                        InputContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                params.push(json!({
                    "role": "tool",
                    "tool_call_id": result.tool_call_id,
                    "content": text,
                }));
            }
        }
    }

    if params.is_empty() {
        return Err("OpenAI requires at least one message".to_string());
    }

    let mut body = json!({
        "model": model.id,
        "messages": params,
        "stream": true,
        "stream_options": { "include_usage": true },
    });

    // Non-standard endpoints reject `store`; opt out when supported
    // (`openai-completions.ts:830-832`).
    if compat.supports_store {
        body["store"] = json!(false);
    }

    if !tools.is_empty() {
        let converted: Vec<Value> = tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    },
                })
            })
            .collect();
        body["tools"] = Value::Array(converted);
        if options.tool_choice == Some(crate::types::ToolChoice::None) {
            body["tool_choice"] = json!("none");
        }
    }

    if let Some(temperature) = options.temperature {
        body["temperature"] = json!(temperature);
    }

    // `maxTokensField` (`openai-completions.ts:834-841`): most compatible
    // endpoints only understand the legacy `max_tokens` name.
    if let Some(max_tokens) = options.max_tokens {
        let capped = max_tokens.min(model.max_tokens);
        match compat.max_tokens_field {
            MaxTokensField::MaxTokens => {
                body["max_tokens"] = json!(capped);
            }
            MaxTokensField::MaxCompletionTokens => {
                body["max_completion_tokens"] = json!(capped);
            }
        }
    }

    // `thinkingFormat` family (`openai-completions.ts:868-968`).
    let reasoning = options
        .reasoning
        .filter(|level| *level != ThinkingLevel::Off);
    let effort_value = reasoning.map(|level| mapped_effort(model, level));
    let off_value = model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get("off"))
        .and_then(Value::as_str)
        .map(str::to_string);
    if model.reasoning {
        match compat.thinking_format.as_str() {
            "deepseek" => {
                if reasoning.is_some() {
                    body["thinking"] = json!({ "type": "enabled" });
                } else if !matches!(
                    model.thinking_level_map.as_ref().and_then(|m| m.get("off")),
                    Some(Value::Null)
                ) {
                    body["thinking"] = json!({ "type": "disabled" });
                }
                if compat.supports_reasoning_effort
                    && let Some(effort) = &effort_value
                {
                    body["reasoning_effort"] = json!(effort);
                }
            }
            "qwen" => {
                body["enable_thinking"] = json!(reasoning.is_some());
                if compat.supports_reasoning_effort
                    && let Some(effort) = &effort_value
                {
                    body["reasoning_effort"] = json!(effort);
                }
            }
            "openrouter" => {
                if let Some(effort) = &effort_value {
                    body["reasoning"] = json!({ "effort": effort });
                } else {
                    body["reasoning"] = json!({ "effort": off_value.as_deref().unwrap_or("none") });
                }
            }
            "together" => {
                body["reasoning"] = json!({ "enabled": reasoning.is_some() });
                if compat.supports_reasoning_effort
                    && let Some(effort) = &effort_value
                {
                    body["reasoning_effort"] = json!(effort);
                }
            }
            "zai" => {
                body["thinking"] = json!({
                    "type": if reasoning.is_some() { "enabled" } else { "disabled" },
                    "clear_thinking": false,
                });
                if compat.supports_reasoning_effort
                    && let Some(effort) = &effort_value
                {
                    body["reasoning_effort"] = json!(effort);
                }
            }
            "ant-ling" => {
                if let Some(mapped) = reasoning.and_then(|level| {
                    model
                        .thinking_level_map
                        .as_ref()
                        .and_then(|map| map.get(level_key(level)))
                        .and_then(Value::as_str)
                }) {
                    body["reasoning"] = json!({ "effort": mapped });
                }
            }
            // Default OpenAI-style `reasoning_effort` (`openai-completions.ts:960-968`).
            _ => {
                if compat.supports_reasoning_effort {
                    if let Some(effort) = &effort_value {
                        body["reasoning_effort"] = json!(effort);
                    } else if let Some(off) = &off_value {
                        body["reasoning_effort"] = json!(off);
                    }
                }
            }
        }
    }

    // Sampling overrides named fields last (`openai-completions.ts:994-998`).
    if let Some(sampling) = &options.sampling_params {
        for (key, value) in sampling {
            body[key] = value.clone();
        }
    }

    Ok(body)
}

fn input_block(block: &InputContent) -> Value {
    match block {
        InputContent::Text(text) => json!({ "type": "text", "text": text.text }),
        InputContent::Image(image) => json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{};base64,{}", image.mime_type, image.data) },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        ModelCost, ModelInputModality, StringOrTextBlocks, SystemMessage, UserMessage,
    };

    fn test_model() -> Model {
        Model {
            id: "gpt-test".into(),
            name: "GPT Test".into(),
            api: "openai-completions".into(),
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            input: vec![ModelInputModality::Text],
            input_limits: None,
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                ..Default::default()
            },
            headers: None,
            r#type: None,
            reasoning: false,
            thinking_level_map: None,
            prompt_cache: None,
            context_window: 128_000,
            max_tokens: 4096,
            sampling_params: None,
            sampling_params_by_thinking_level: None,
            compat: None,
        }
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
        let body = build_request_body(&model, &context, &StreamOptions::default()).unwrap();
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hi");
        assert!(body["stream_options"]["include_usage"].as_bool().unwrap());
    }

    #[test]
    fn tool_calls_round_trip_in_messages() {
        let model = test_model();
        let assistant = AssistantMessage {
            content: vec![
                AssistantContent::Text(crate::types::TextContent {
                    text: "calling".into(),
                    text_signature: None,
                }),
                AssistantContent::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"cmd":"ls"})
                        .as_object()
                        .cloned()
                        .unwrap(),
                    thought_signature: None,
                    namespace: None,
                    arguments_error: None,
                }),
            ],
            api: "openai-completions".into(),
            provider: "openai".into(),
            model: "gpt-test".into(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: vec![],
            usage: crate::types::Usage::default(),
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
                Message::User(UserMessage {
                    content: UserContent::Text("run ls".into()),
                    timestamp: 1,
                }),
                Message::Assistant(assistant),
                Message::ToolResult(crate::types::ToolResultMessage {
                    tool_call_id: "call_1".into(),
                    tool_name: "bash".into(),
                    content: vec![InputContent::Text(crate::types::TextContent {
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
        let body = build_request_body(&model, &context, &StreamOptions::default()).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[1]["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["arguments"],
            "{\"cmd\":\"ls\"}"
        );
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["content"], "file.txt");
    }

    #[test]
    fn finish_reason_mapping() {
        assert_eq!(map_finish_reason("stop").0, StopReason::Stop);
        assert_eq!(map_finish_reason("end").0, StopReason::Stop);
        assert_eq!(map_finish_reason("tool_calls").0, StopReason::ToolUse);
        assert_eq!(map_finish_reason("length").0, StopReason::Length);
        assert_eq!(map_finish_reason("content_filter").0, StopReason::Error);
        assert_eq!(map_finish_reason("weird").0, StopReason::Error);
    }

    #[test]
    fn usage_numbering() {
        let mut output = initial_output(&test_model());
        apply_chunk_usage(
            &json!({
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
                "prompt_tokens_details": { "cached_tokens": 4 },
            }),
            &mut output,
        );
        assert_eq!(output.usage.input, 10);
        assert_eq!(output.usage.output, 5);
        assert_eq!(output.usage.total_tokens, 15);
        assert_eq!(output.usage.cache_read, 4);
    }

    fn with_compat(mut model: Model, compat: Value) -> Model {
        model.compat = serde_json::from_value(compat).ok();
        model
    }

    fn system_user_context() -> TranscriptContext {
        TranscriptContext {
            messages: vec![
                Message::System(SystemMessage {
                    content: crate::types::StringOrTextBlocks::Text("Be terse.".into()),
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
        }
    }

    #[test]
    fn compat_selects_max_tokens_field() {
        let context = system_user_context();
        let options = StreamOptions {
            max_tokens: Some(1000),
            ..Default::default()
        };

        // Explicit catalog override: legacy max_tokens.
        let legacy = with_compat(test_model(), json!({ "maxTokensField": "max_tokens" }));
        let body = build_request_body(&legacy, &context, &options).unwrap();
        assert_eq!(body["max_tokens"], 1000);
        assert!(body.get("max_completion_tokens").is_none());

        // Default OpenAI behavior: max_completion_tokens.
        let body = build_request_body(&test_model(), &context, &options).unwrap();
        assert_eq!(body["max_completion_tokens"], 1000);
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn compat_switches_system_to_developer_role() {
        let context = system_user_context();
        let mut model = test_model();
        model.reasoning = true;
        let with_role = with_compat(model.clone(), json!({ "supportsDeveloperRole": true }));
        let body = build_request_body(&with_role, &context, &StreamOptions::default()).unwrap();
        assert_eq!(body["messages"][0]["role"], "developer");

        let no_role = with_compat(model, json!({ "supportsDeveloperRole": false }));
        let body = build_request_body(&no_role, &context, &StreamOptions::default()).unwrap();
        assert_eq!(body["messages"][0]["role"], "system");
    }

    #[test]
    fn compat_thinking_formats_shape_the_payload() {
        let context = system_user_context();
        let options = StreamOptions {
            reasoning: Some(ThinkingLevel::Medium),
            ..Default::default()
        };
        let mut base = test_model();
        base.reasoning = true;

        let deepseek = with_compat(base.clone(), json!({ "thinkingFormat": "deepseek" }));
        let body = build_request_body(&deepseek, &context, &options).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "medium");

        let qwen = with_compat(base.clone(), json!({ "thinkingFormat": "qwen" }));
        let body = build_request_body(&qwen, &context, &options).unwrap();
        assert_eq!(body["enable_thinking"], true);
        assert_eq!(body["reasoning_effort"], "medium");

        let openrouter = with_compat(base.clone(), json!({ "thinkingFormat": "openrouter" }));
        let body = build_request_body(&openrouter, &context, &options).unwrap();
        assert_eq!(body["reasoning"]["effort"], "medium");

        let openai = with_compat(base, json!({ "thinkingFormat": "openai" }));
        let body = build_request_body(&openai, &context, &options).unwrap();
        assert_eq!(body["reasoning_effort"], "medium");
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn supports_store_adds_store_false() {
        let context = system_user_context();
        let stored = with_compat(test_model(), json!({ "supportsStore": true }));
        let body = build_request_body(&stored, &context, &StreamOptions::default()).unwrap();
        assert_eq!(body["store"], false);

        let unstored = with_compat(test_model(), json!({ "supportsStore": false }));
        let body = build_request_body(&unstored, &context, &StreamOptions::default()).unwrap();
        assert!(body.get("store").is_none());
    }

    #[test]
    fn catalog_headers_reach_request_headers() {
        // End-to-end: models.json -> Model.headers -> merged request headers.
        let model = crate::catalog::catalog()
            .get("github-copilot", "claude-fable-5")
            .unwrap();
        let headers = request_headers(
            &[("content-type", "application/json")],
            model,
            &StreamOptions::default(),
        );
        assert_eq!(
            headers["user-agent"].as_deref(),
            Some("GitHubCopilotChat/0.35.0")
        );
        assert_eq!(headers["editor-version"].as_deref(), Some("vscode/1.107.0"));
        assert_eq!(
            headers["copilot-integration-id"].as_deref(),
            Some("vscode-chat")
        );

        let nvidia = crate::catalog::catalog()
            .get("nvidia", "deepseek-ai/deepseek-v4.1-flash")
            .unwrap();
        let headers = request_headers(&[], nvidia, &StreamOptions::default());
        assert_eq!(headers["nvcf-poll-seconds"].as_deref(), Some("3600"));
    }
}
