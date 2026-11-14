//! Anthropic Messages API adapter (port of TS `api/anthropic-messages.ts`,
//! core streaming path).
//!
//! P1 scope: api-key auth, request building (system/messages/tools/thinking
//! budgets), SSE streaming mapped to the unified protocol, stop-reason and
//! usage mapping, catalog-priced cost. Deferred: OAuth, Claude Code name
//! mapping, model fallbacks, threads, retries, `OpenRouter` billing.

use crate::event_stream::{AssistantMessageEventStream, EventSender};
use crate::providers::common::{
    finalize, initial_output, is_leading_system, system_text, system_update_text,
};
use crate::sse::{SseError, read_sse_stream};
use crate::streaming_json::parse_tool_call_arguments;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, ImageContent, InputContent, Message,
    Model, StopReason, StreamOptions, ThinkingLevel, ToolCall, TranscriptContext, now_ms,
};
use futures::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;

/// Send the request and stream protocol events. Never panics on provider
/// errors; failures surface as `error` events (`StreamFn` contract).
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

#[allow(clippy::too_many_lines)] // mirrors the TS event loop shape; split when extending
async fn request_and_stream(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
    output: &mut AssistantMessage,
    sender: &EventSender,
) -> StreamResult<()> {
    // Credential (TS `providers/anthropic.ts:30-45` and the OAuth flows'
    // `toAuth`): the resolved key goes out as `x-api-key`; a bearer token
    // (upstream's `ANTHROPIC_AUTH_TOKEN` path, `providers/anthropic.ts:38`)
    // rides as a plain `Authorization: Bearer` header. The key wins when
    // both are set, like upstream's stored-credential-first ladder. Env
    // resolution belongs to the caller's auth ladder, not this adapter.
    let api_key = options.api_key.clone();
    let access_token = if api_key.is_some() {
        None
    } else {
        options.access_token.clone()
    };
    if api_key.is_none() && access_token.is_none() {
        return Err("Anthropic API key not set (set ANTHROPIC_API_KEY)".to_string());
    }

    let body = build_request_body(model, context, options)?;
    let url = format!("{}/v1/messages", model.base_url.trim_end_matches('/'));

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| format!("client build failed: {e}"))?;
    let mut request = client
        .post(&url)
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01");
    if let Some(token) = &access_token {
        request = request.header("authorization", format!("Bearer {token}"));
    } else if let Some(key) = &api_key {
        request = request.header("x-api-key", key);
    }
    let response = request
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let sse = read_sse_stream(response).await.map_err(|e| e.to_string())?;
    let _ = sender
        .send(AssistantMessageEvent::Start {
            partial: output.clone(),
        })
        .await;

    // Per-block streaming state, keyed by Anthropic block index.
    let mut open_blocks: std::collections::HashMap<u64, BlockState> =
        std::collections::HashMap::new();

    tokio::pin!(sse);
    while let Some(item) = sse.next().await {
        let message = match item {
            Ok(message) => message,
            Err(SseError::Status { status, body }) => return Err(format!("HTTP {status}: {body}")),
            Err(SseError::Http(message)) => return Err(message),
        };
        if message.event.as_deref() == Some("error") {
            return Err(format!("provider error event: {}", message.data));
        }
        let event: Value = match serde_json::from_str(&message.data) {
            Ok(event) => event,
            Err(error) => return Err(format!("could not parse Anthropic SSE event: {error}")),
        };
        match event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "message_start" => {
                if let Some(message_obj) = event.get("message") {
                    if let Some(id) = message_obj.get("id").and_then(Value::as_str) {
                        output.response_id = Some(id.to_string());
                    }
                    if let Some(response_model) = message_obj.get("model").and_then(Value::as_str) {
                        if response_model != model.id {
                            output.response_model = Some(response_model.to_string());
                        }
                    }
                    if let Some(usage) = message_obj.get("usage") {
                        apply_usage_field(usage, output, true);
                    }
                }
            }
            "content_block_start" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block = event.get("content_block").cloned().unwrap_or(Value::Null);
                let block_type = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match block_type {
                    "text" => {
                        let text = block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        output
                            .content
                            .push(AssistantContent::Text(crate::types::TextContent {
                                text,
                                text_signature: None,
                            }));
                        open_blocks.insert(
                            index,
                            BlockState {
                                content_index: output.content.len() - 1,
                                args_raw: None,
                            },
                        );
                        let _ = sender
                            .send(AssistantMessageEvent::TextStart {
                                content_index: output.content.len() - 1,
                                partial: output.clone(),
                            })
                            .await;
                    }
                    "thinking" | "redacted_thinking" => {
                        let redacted = block_type == "redacted_thinking";
                        let thinking = if redacted {
                            "[Reasoning redacted]".to_string()
                        } else {
                            block
                                .get("thinking")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string()
                        };
                        let signature = if redacted {
                            block
                                .get("data")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string()
                        } else {
                            block
                                .get("signature")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string()
                        };
                        output.content.push(AssistantContent::Thinking(
                            crate::types::ThinkingContent {
                                thinking,
                                thinking_signature: Some(signature),
                                redacted,
                            },
                        ));
                        open_blocks.insert(
                            index,
                            BlockState {
                                content_index: output.content.len() - 1,
                                args_raw: None,
                            },
                        );
                        let _ = sender
                            .send(AssistantMessageEvent::ThinkingStart {
                                content_index: output.content.len() - 1,
                                partial: output.clone(),
                            })
                            .await;
                    }
                    "tool_use" => {
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let arguments = block
                            .get("input")
                            .cloned()
                            .and_then(|v| v.as_object().cloned())
                            .unwrap_or_default();
                        output.content.push(AssistantContent::ToolCall(ToolCall {
                            id,
                            name,
                            arguments,
                            thought_signature: None,
                            namespace: None,
                            arguments_error: None,
                        }));
                        open_blocks.insert(
                            index,
                            BlockState {
                                content_index: output.content.len() - 1,
                                args_raw: Some(String::new()),
                            },
                        );
                        let _ = sender
                            .send(AssistantMessageEvent::ToolcallStart {
                                content_index: output.content.len() - 1,
                                partial: output.clone(),
                            })
                            .await;
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let delta = event.get("delta").cloned().unwrap_or(Value::Null);
                let delta_type = delta
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let Some(state) = open_blocks.get(&index) else {
                    continue;
                };
                let content_index = state.content_index;
                match delta_type {
                    "text_delta" => {
                        let text = delta
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let Some(AssistantContent::Text(block)) =
                            output.content.get_mut(content_index)
                        {
                            block.text += text;
                            let _ = sender
                                .send(AssistantMessageEvent::TextDelta {
                                    content_index,
                                    delta: text.to_string(),
                                    partial: output.clone(),
                                })
                                .await;
                        }
                    }
                    "thinking_delta" => {
                        let text = delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let Some(AssistantContent::Thinking(block)) =
                            output.content.get_mut(content_index)
                        {
                            block.thinking += text;
                            let _ = sender
                                .send(AssistantMessageEvent::ThinkingDelta {
                                    content_index,
                                    delta: text.to_string(),
                                    partial: output.clone(),
                                })
                                .await;
                        }
                    }
                    "signature_delta" => {
                        let signature = delta
                            .get("signature")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let Some(AssistantContent::Thinking(block)) =
                            output.content.get_mut(content_index)
                        {
                            let sig = block.thinking_signature.get_or_insert_with(String::new);
                            sig.push_str(signature);
                        }
                    }
                    "input_json_delta" => {
                        let partial_json = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let Some(state) = open_blocks.get_mut(&index) {
                            if let Some(raw) = state.args_raw.as_mut() {
                                raw.push_str(partial_json);
                                if let Some(AssistantContent::ToolCall(block)) =
                                    output.content.get_mut(state.content_index)
                                {
                                    block.arguments =
                                        parse_tool_call_arguments(raw).unwrap_or_default();
                                }
                                let _ = sender
                                    .send(AssistantMessageEvent::ToolcallDelta {
                                        content_index: state.content_index,
                                        delta: partial_json.to_string(),
                                        partial: output.clone(),
                                    })
                                    .await;
                            }
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(state) = open_blocks.remove(&index) {
                    let content_index = state.content_index;
                    match output.content.get(content_index).cloned() {
                        Some(AssistantContent::Text(block)) => {
                            let _ = sender
                                .send(AssistantMessageEvent::TextEnd {
                                    content_index,
                                    content: block.text,
                                    partial: output.clone(),
                                })
                                .await;
                        }
                        Some(AssistantContent::Thinking(block)) => {
                            let _ = sender
                                .send(AssistantMessageEvent::ThinkingEnd {
                                    content_index,
                                    content: block.thinking,
                                    partial: output.clone(),
                                })
                                .await;
                        }
                        Some(AssistantContent::ToolCall(_)) => {
                            // Final strict parse: invalid arguments land in arguments_error.
                            if let Some(raw) = state.args_raw {
                                let block = output
                                    .content
                                    .get_mut(content_index)
                                    .and_then(|c| match c {
                                        AssistantContent::ToolCall(t) => Some(t),
                                        _ => None,
                                    })
                                    .expect("toolCall block exists");
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
                                        content_index,
                                        tool_call: final_block,
                                        partial: output.clone(),
                                    })
                                    .await;
                            }
                        }
                        _ => {}
                    }
                }
            }
            "message_delta" => {
                if let Some(delta) = event.get("delta") {
                    if let Some(reason) = delta.get("stop_reason").and_then(Value::as_str) {
                        output.raw_stop_reason = Some(reason.to_string());
                        match map_stop_reason(reason) {
                            Ok((stop, None)) => output.stop_reason = stop,
                            Ok((stop, Some(error_message))) => {
                                output.stop_reason = stop;
                                output.error_message = Some(error_message);
                            }
                            Err(error_message) => {
                                output.stop_reason = StopReason::Error;
                                output.error_message = Some(error_message);
                            }
                        }
                    }
                }
                if let Some(usage) = event.get("usage") {
                    apply_usage_field(usage, output, false);
                }
            }
            "message_stop" => break,
            _ => {}
        }
    }

    if output.stop_reason == StopReason::Pending {
        return Err("Anthropic stream ended without a stop reason".to_string());
    }
    if matches!(output.stop_reason, StopReason::Error | StopReason::Aborted) {
        return Err(output
            .error_message
            .clone()
            .unwrap_or_else(|| "An unknown error occurred".to_string()));
    }
    Ok(())
}

/// Streaming state of one Anthropic content block.
struct BlockState {
    content_index: usize,
    /// Raw `input_json_delta` accumulation for tool calls.
    args_raw: Option<String>,
}

/// Apply usage numbers from `message_start`/`message_delta` events. Fields
/// only overwrite when present (proxies may omit them in deltas).
fn apply_usage_field(usage: &Value, output: &mut AssistantMessage, initial: bool) {
    let field = |name: &str| usage.get(name).and_then(Value::as_u64);
    if let Some(input) = field("input_tokens") {
        output.usage.input = input;
    } else if initial {
        output.usage.input = 0;
    }
    if let Some(out) = field("output_tokens") {
        output.usage.output = out;
    }
    if let Some(cache_read) = field("cache_read_input_tokens") {
        output.usage.cache_read = cache_read;
    }
    if let Some(cache_write) = field("cache_creation_input_tokens") {
        output.usage.cache_write = cache_write;
    }
    if let Some(cache_1h) = usage
        .get("cache_creation")
        .and_then(|c| c.get("ephemeral_1h_input_tokens"))
        .and_then(Value::as_u64)
    {
        output.usage.cache_write_1h = Some(cache_1h);
    }
    if let Some(thinking) = usage
        .get("output_tokens_details")
        .and_then(|d| d.get("thinking_tokens"))
        .and_then(Value::as_u64)
    {
        output.usage.reasoning = Some(thinking);
    }
    output.usage.total_tokens = output.usage.input
        + output.usage.output
        + output.usage.cache_read
        + output.usage.cache_write;
}

/// TS `mapStopReason`: anthropic stop reasons to pi stop reasons. `Err` is an
/// unhandled reason (error message); `Ok((reason, Some(msg)))` is a refusal-style stop.
fn map_stop_reason(reason: &str) -> Result<(StopReason, Option<String>), String> {
    match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => Ok((StopReason::Stop, None)),
        "max_tokens" => Ok((StopReason::Length, None)),
        "tool_use" => Ok((StopReason::ToolUse, None)),
        "refusal" => Ok((
            StopReason::Error,
            Some("The model refused to complete the request".to_string()),
        )),
        "sensitive" => Ok((
            StopReason::Error,
            Some("Provider stopped with: sensitive".to_string()),
        )),
        other => Err(format!("Unhandled stop reason: {other}")),
    }
}

/// Build the Anthropic `POST /v1/messages` JSON body (P1 core path).
#[allow(clippy::too_many_lines)] // request assembly; split if it grows further
pub fn build_request_body(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
) -> StreamResult<Value> {
    let messages = &context.messages;
    let mut system_blocks: Vec<Value> = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    // Tools are declared by system messages inside the transcript
    // (TS resolveTranscript folds Context.tools into the leading system message).
    let tools = crate::providers::common::tools_at_point(messages);

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
                if is_leading_system(messages, i) {
                    system_blocks.push(json!({ "type": "text", "text": text }));
                } else {
                    params.push(json!({
                        "role": "system",
                        "content": [{ "type": "text", "text": text }],
                    }));
                }
            }
            Message::User(user) => match &user.content {
                crate::types::UserContent::Text(text) => {
                    if !text.trim().is_empty() {
                        params.push(json!({ "role": "user", "content": text }));
                    }
                }
                crate::types::UserContent::Blocks(blocks) => {
                    let converted: Vec<Value> = blocks.iter().map(input_block).collect();
                    let converted: Vec<Value> = converted
                        .into_iter()
                        .filter(|b| {
                            b.get("type").and_then(Value::as_str) != Some("text") || {
                                b.get("text")
                                    .and_then(Value::as_str)
                                    .is_some_and(|t| !t.trim().is_empty())
                            }
                        })
                        .collect();
                    if !converted.is_empty() {
                        params.push(json!({ "role": "user", "content": converted }));
                    }
                }
            },
            Message::Assistant(assistant) => {
                let mut blocks: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => {
                            if text.text.trim().is_empty() {
                                continue;
                            }
                            blocks.push(json!({ "type": "text", "text": text.text }));
                        }
                        AssistantContent::Thinking(thinking) => {
                            if thinking.redacted {
                                blocks.push(json!({
                                    "type": "redacted_thinking",
                                    "data": thinking.thinking_signature.clone().unwrap_or_default(),
                                }));
                            } else {
                                blocks.push(json!({
                                    "type": "thinking",
                                    "thinking": thinking.thinking,
                                    "signature": thinking.thinking_signature.clone().unwrap_or_default(),
                                }));
                            }
                        }
                        AssistantContent::ToolCall(call) => {
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": call.id,
                                "name": call.name,
                                "input": call.arguments,
                            }));
                        }
                    }
                }
                if !blocks.is_empty() {
                    params.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
            Message::ToolResult(result) => {
                let result_blocks: Vec<Value> = result
                    .content
                    .iter()
                    .map(|block| match block {
                        InputContent::Text(text) => json!({ "type": "text", "text": text.text }),
                        InputContent::Image(image) => image_block(image),
                    })
                    .collect();
                params.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": result.tool_call_id,
                        "content": result_blocks,
                        "is_error": result.is_error,
                    }],
                }));
            }
        }
    }

    if system_blocks.is_empty() {
        return Err("Anthropic requires a system prompt".to_string());
    }

    let max_tokens = options
        .max_tokens
        .unwrap_or(model.max_tokens)
        .min(model.max_tokens);
    let mut body = json!({
        "model": model.id,
        "max_tokens": max_tokens,
        "stream": true,
        "system": system_blocks,
        "messages": params,
    });

    if !tools.is_empty() {
        let converted: Vec<Value> = tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                })
            })
            .collect();
        body["tools"] = Value::Array(converted);
        if options.tool_choice == Some(crate::types::ToolChoice::None) {
            body["tool_choice"] = json!({ "type": "none" });
        }
    }

    if let Some(temperature) = options.temperature {
        body["temperature"] = json!(temperature);
    }

    // Thinking budgets: token-based via model.thinkingLevelMap when present,
    // otherwise level defaults capped to half the max tokens.
    if let Some(level) = options.reasoning {
        if model.reasoning {
            let default_budgets: [(ThinkingLevel, u64); 5] = [
                (ThinkingLevel::Minimal, 1024),
                (ThinkingLevel::Off, 0),
                (ThinkingLevel::Low, 2048),
                (ThinkingLevel::Medium, 4096),
                (ThinkingLevel::High, 8192),
            ];
            let mapped = model
                .thinking_level_map
                .as_ref()
                .and_then(|m| m.get(map_key(level)))
                .cloned();
            let budget = mapped
                .as_ref()
                .and_then(Value::as_u64)
                .or_else(|| {
                    default_budgets
                        .iter()
                        .find(|(l, _)| *l == level)
                        .map(|(_, b)| *b)
                })
                .unwrap_or(0);
            if budget > 0 {
                body["thinking"] = json!({
                    "type": "enabled",
                    "budget_tokens": budget.min(max_tokens / 2).max(1024),
                });
            }
        }
    }

    Ok(body)
}

fn map_key(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Off => "off",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
    }
}

fn input_block(block: &InputContent) -> Value {
    match block {
        InputContent::Text(text) => json!({ "type": "text", "text": text.text }),
        InputContent::Image(image) => image_block(image),
    }
}

fn image_block(image: &ImageContent) -> Value {
    json!({
        "type": "image",
        "source": { "type": "base64", "media_type": image.mime_type, "data": image.data },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelCost, ModelInputModality, SystemMessage, UserContent, UserMessage};

    fn test_model() -> Model {
        Model {
            id: "claude-test".into(),
            name: "Claude Test".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            input: vec![ModelInputModality::Text],
            input_limits: None,
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                ..Default::default()
            },
            headers: None,
            r#type: None,
            reasoning: true,
            thinking_level_map: None,
            prompt_cache: None,
            context_window: 200_000,
            max_tokens: 8192,
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
        };
        let body = build_request_body(&model, &context, &StreamOptions::default()).unwrap();
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["max_tokens"], 8192);
        assert_eq!(body["stream"], true);
        assert_eq!(body["system"][0]["text"], "Be terse.");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hi");
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn request_body_with_tools_and_images() {
        let model = test_model();
        let tool = crate::types::Tool {
            name: "bash".into(),
            description: "run".into(),
            parameters: serde_json::json!({"type":"object"}),
            constrained_sampling: None,
        };
        let context = TranscriptContext {
            messages: vec![
                Message::System(SystemMessage {
                    content: crate::types::StringOrTextBlocks::Text("s".into()),
                    sections: None,
                    tools_added: Some(vec![tool]),
                    tools_removed: None,
                    timestamp: 1,
                }),
                Message::User(UserMessage {
                    content: UserContent::Blocks(vec![
                        InputContent::Text(crate::types::TextContent {
                            text: "see:".into(),
                            text_signature: None,
                        }),
                        InputContent::Image(ImageContent {
                            data: "aGk=".into(),
                            mime_type: "image/png".into(),
                        }),
                    ]),
                    timestamp: 2,
                }),
            ],
        };
        let body = build_request_body(&model, &context, &StreamOptions::default()).unwrap();
        assert_eq!(body["tools"][0]["name"], "bash");
        let content_arr = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(
            content_arr.len(),
            2,
            "text and image blocks: {content_arr:?}"
        );
        assert_eq!(content_arr[0]["type"], "text");
        assert_eq!(content_arr[1]["type"], "image");
        assert_eq!(content_arr[1]["source"]["media_type"], "image/png");
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(map_stop_reason("end_turn").unwrap().0, StopReason::Stop);
        assert_eq!(map_stop_reason("max_tokens").unwrap().0, StopReason::Length);
        assert_eq!(map_stop_reason("tool_use").unwrap().0, StopReason::ToolUse);
        assert!(map_stop_reason("refusal").unwrap().1.is_some());
        assert!(map_stop_reason("future_thing").is_err());
    }

    #[test]
    fn usage_field_leniency() {
        let mut output = initial_output(&test_model());
        apply_usage_field(&json!({"output_tokens": 7}), &mut output, false);
        assert_eq!(output.usage.output, 7);
        assert_eq!(
            output.usage.input, 0,
            "missing input preserved from message_start"
        );
    }
}
