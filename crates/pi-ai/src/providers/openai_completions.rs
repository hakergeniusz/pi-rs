//! `OpenAI` Chat Completions adapter (port of TS `api/openai-completions.ts`,
//! core streaming path). Covers the `OpenAI` wire format shared by the many
//! compatible providers (llama.cpp, vLLM, `OpenRouter`, Groq, …).
//!
//! P1 scope: bearer auth, request building (messages/tools/temperature/
//! `max_tokens`/`stream_options`), SSE streaming with `reasoning_content` support,
//! tool-call delta accumulation, finish-reason and usage mapping.
//! Deferred: compat autodetection matrix, grammar tools, prompt-cache keys.
//! `OpenAI`-wire-compatible providers: llama.cpp, vLLM, `OpenRouter`, Groq, …

use crate::event_stream::{AssistantMessageEventStream, EventSender};
use crate::providers::common::{
    finalize, initial_output, is_leading_system, system_text, system_update_text, tools_at_point,
};
use crate::sse::{SseError, read_sse_stream};
use crate::streaming_json::parse_tool_call_arguments;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, InputContent, Message, Model,
    StopReason, StreamOptions, ToolCall, TranscriptContext, UserContent, now_ms,
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

    let body = build_request_body(model, context, options)?;
    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| format!("client build failed: {e}"))?;
    let response = client
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
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
        if data == "[DONE]" {
            break;
        }
        if data.is_empty() {
            continue;
        }
        let chunk: Value = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(error) => return Err(format!("could not parse OpenAI SSE chunk: {error}")),
        };

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
                params.push(json!({ "role": "system", "content": text }));
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
    if let Some(max_tokens) = options.max_tokens {
        body["max_tokens"] = json!(max_tokens.min(model.max_tokens));
    }
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
}
