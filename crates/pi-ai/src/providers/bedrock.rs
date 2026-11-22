//! AWS Bedrock Converse Stream adapter (port of TS
//! `packages/ai/src/api/bedrock-converse-stream.ts`, ~1466 lines and its lazy
//! loader `bedrock-converse-stream.lazy.ts`).
//!
//! Ground-truth ranges used here:
//! - stream loop / event dispatch: `142-404` and `341-374`;
//! - error formatting and diagnostics: `406-499`;
//! - header injection / user agent: `501-580`;
//! - `streamSimple` thinking setup: `582-631`;
//! - content-block start/delta/stop and metadata: `633-795`;
//! - model capability probes: `797-956`;
//! - request construction (`buildSystemPrompt`, messages, tools): `958-1232`;
//! - stop-reason / region / credential resolution: `1234-1299`;
//! - additional model fields and image helpers: `1301-1466`.
//!
//! Unlike the TypeScript port this module talks to Bedrock directly with
//! `reqwest`, implements AWS Signature V4 itself (see `bedrock_sigv4`) and
//! decodes the binary AWS event-stream framing instead of relying on the
//! Node-only AWS SDK. The event sequence, block conversion, usage and error
//! mapping follow the upstream semantics exactly.

use crate::cost::calculate_cost;
use crate::event_stream::{AssistantMessageEventStream, EventSender};
use crate::providers::bedrock_sigv4::{
    AwsCredentials, endpoint_region, model_path, region_from_arn, sign_request,
};
use crate::providers::common::{
    apply_payload_hook, apply_response_hook, apply_stream_event_hook, finalize, initial_output,
    response_metadata, system_text, tools_at_point,
};
use crate::streaming_json::parse_tool_call_arguments;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, InputContent,
    Message, Model, StopReason, StreamOptions, ThinkingContent, ThinkingLevel, ToolCall,
    TranscriptContext, UserContent,
};
use base64::Engine;
use futures::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

const EMPTY_TEXT_PLACEHOLDER: &str = "<empty>";
const REDACTED_THINKING_PLACEHOLDER: &str = "[Reasoning redacted]";
const BEDROCK_DATA_RETENTION_DOCS_URL: &str =
    "https://docs.aws.amazon.com/bedrock/latest/userguide/data-retention.html";
const SERVICE: &str = "bedrock";

/// Streamed state of one open Bedrock content block (TS lines `122-133`).
/// Kept off the content block so it never reaches partial messages.
struct OpenBlock {
    content_index: usize,
    /// Accumulated raw tool-call argument text.
    arguments_raw: Option<String>,
    /// Encrypted reasoning chunks, joined into `thinkingSignature` at close.
    redacted_chunks: Option<Vec<u8>>,
}

type OpenBlocks = HashMap<u64, OpenBlock>;

/// Authentication mode chosen for a request (upstream lines `198-259`).
enum Auth {
    Bearer(String),
    SigV4(AwsCredentials),
}

/// Everything endpoint- and credential-related resolved before a request.
struct Resolved {
    base_url: String,
    host: String,
    path_prefix: String,
    region: String,
    auth: Auth,
}

type StreamResult<T> = Result<T, String>;

/// Entry point: spawn the request and return the event stream.
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
    let mut open_blocks = OpenBlocks::new();
    let result = request_and_stream(
        model,
        context,
        options,
        &mut output,
        &mut open_blocks,
        &sender,
    )
    .await;
    if result.is_err() {
        // Upstream's catch block always flushes open blocks (TS lines 391-392).
        flush_open_blocks(&mut open_blocks, &mut output);
    }
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
            if !matches!(output.stop_reason, StopReason::Aborted) {
                output.stop_reason = StopReason::Error;
            }
            output.error_message = Some(message);
            let _ = sender
                .send(AssistantMessageEvent::Error {
                    reason: output.stop_reason,
                    error: output,
                })
                .await;
        }
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Resolve the endpoint, region and authentication (TS lines `177-259`,
/// `1251-1299`). Region precedence: ARN > `AWS_REGION`/`AWS_DEFAULT_REGION` >
/// endpoint host > `us-east-1`.
fn resolve_config(model: &Model) -> StreamResult<Resolved> {
    let region = region_from_arn(&model.id)
        .or_else(|| env("AWS_REGION").or_else(|| env("AWS_DEFAULT_REGION")))
        .or_else(|| endpoint_region(&model.base_url))
        .unwrap_or_else(|| "us-east-1".to_string());

    let base_url = if model.base_url.trim().is_empty() {
        format!("https://bedrock-runtime.{region}.amazonaws.com")
    } else {
        model.base_url.trim_end_matches('/').to_string()
    };
    let (host, path_prefix) = split_url(&base_url);

    let skip_auth = env("AWS_BEDROCK_SKIP_AUTH").as_deref() == Some("1");
    let bearer = env("AWS_BEARER_TOKEN_BEDROCK").filter(|_| !skip_auth);
    let auth = if let Some(token) = bearer {
        Auth::Bearer(token)
    } else if skip_auth {
        Auth::SigV4(AwsCredentials {
            access_key_id: "dummy-access-key".to_string(),
            secret_access_key: "dummy-secret-key".to_string(),
            session_token: None,
        })
    } else {
        let access_key_id = env("AWS_ACCESS_KEY_ID").ok_or_else(|| {
            "Bedrock credentials not set (set AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY or AWS_BEARER_TOKEN_BEDROCK)"
                .to_string()
        })?;
        let secret_access_key = env("AWS_SECRET_ACCESS_KEY").ok_or_else(|| {
            "Bedrock credentials not set (AWS_SECRET_ACCESS_KEY missing)".to_string()
        })?;
        Auth::SigV4(AwsCredentials {
            access_key_id,
            secret_access_key,
            session_token: env("AWS_SESSION_TOKEN"),
        })
    };

    Ok(Resolved {
        base_url,
        host,
        path_prefix,
        region,
        auth,
    })
}

/// Split a base URL into its `Host` header value and its path prefix.
fn split_url(url: &str) -> (String, String) {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let (authority, rest) = after_scheme.split_once('/').unwrap_or((after_scheme, ""));
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .to_string();
    let prefix = if rest.is_empty() {
        String::new()
    } else {
        format!("/{}", rest.trim_start_matches('/').trim_end_matches('/'))
    };
    (host, prefix)
}

/// Bearer/SigV4 headers are reserved and cannot be overridden by caller headers
/// (TS `isReservedHeader`, lines `501-512`).
fn is_reserved_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "authorization" || lower == "host" || lower.starts_with("x-amz-")
}

#[allow(clippy::too_many_lines)] // mirrors the TS request-and-consume loop; split when extending
async fn request_and_stream(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
    output: &mut AssistantMessage,
    open_blocks: &mut OpenBlocks,
    sender: &EventSender,
) -> StreamResult<()> {
    let config = resolve_config(model)?;
    let body = build_request_body(model, context, options);
    let body = apply_payload_hook(&options.on_payload, body, model).await?;
    let payload =
        serde_json::to_vec(&body).map_err(|e| format!("could not serialize request: {e}"))?;
    let path = format!("{}{}", config.path_prefix, model_path(&model.id));
    let base = &config.base_url;
    let url = format!("{base}{path}");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| format!("client build failed: {e}"))?;
    let mut request = client
        .post(&url)
        .header("content-type", "application/json")
        .header("host", &config.host)
        .body(payload.clone());
    match &config.auth {
        Auth::Bearer(token) => {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        Auth::SigV4(credentials) => {
            let (amz_date, date_stamp) = crate::providers::bedrock_sigv4::amz_timestamp();
            let signed = sign_request(
                credentials,
                &config.region,
                SERVICE,
                &config.host,
                &path,
                &payload,
                &amz_date,
                &date_stamp,
            );
            request = request
                .header("authorization", signed.authorization)
                .header("x-amz-date", signed.x_amz_date)
                .header("x-amz-content-sha256", signed.x_amz_content_sha256);
            if let Some(token) = signed.x_amz_security_token {
                request = request.header("x-amz-security-token", token);
            }
        }
    }
    for (name, value) in &options.headers {
        if let Some(value) = value
            && !is_reserved_header(name)
        {
            request = request.header(name, value);
        }
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    apply_response_hook(&options.on_response, response_metadata(&response), model).await?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        return Err(format_http_error(status, &text));
    }

    let mut decoder = EventStreamDecoder::new();
    let mut byte_stream = response.bytes_stream();
    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk.map_err(|e| format!("stream read failed: {e}"))?;
        for event in decoder.feed(&chunk)? {
            process_event(event, model, options, output, open_blocks, sender).await?;
        }
    }
    for event in decoder.finish()? {
        process_event(event, model, options, output, open_blocks, sender).await?;
    }

    if output.stop_reason == StopReason::Pending {
        return Err("Bedrock stream ended without a stop reason".to_string());
    }
    if matches!(output.stop_reason, StopReason::Error | StopReason::Aborted) {
        return Err(output
            .error_message
            .clone()
            .unwrap_or_else(|| "An unknown error occurred".to_string()));
    }
    Ok(())
}

/// Dispatch one decoded provider event (TS lines `341-374`).
async fn process_event(
    event: DecodedEvent,
    model: &Model,
    options: &StreamOptions,
    output: &mut AssistantMessage,
    open_blocks: &mut OpenBlocks,
    sender: &EventSender,
) -> StreamResult<()> {
    let item = match event {
        DecodedEvent::Item(item) => item,
        DecodedEvent::Error(message) => return Err(message),
    };
    apply_stream_event_hook(&options.on_provider_stream_event, item.clone(), model).await?;

    if let Some(value) = item.get("messageStart") {
        handle_message_start(value, output, sender).await?;
    } else if let Some(value) = item.get("contentBlockStart") {
        handle_content_block_start(value, open_blocks, output, sender).await;
    } else if let Some(value) = item.get("contentBlockDelta") {
        handle_content_block_delta(value, open_blocks, output, sender).await;
    } else if let Some(value) = item.get("contentBlockStop") {
        handle_content_block_stop(value, open_blocks, output, sender).await;
    } else if let Some(value) = item.get("messageStop") {
        handle_message_stop(value, output);
    } else if let Some(value) = item.get("metadata") {
        handle_metadata(value, model, output);
    } else {
        for (name, pascal) in [
            ("internalServerException", "InternalServerException"),
            ("modelStreamErrorException", "ModelStreamErrorException"),
            ("validationException", "ValidationException"),
            ("throttlingException", "ThrottlingException"),
            ("serviceUnavailableException", "ServiceUnavailableException"),
        ] {
            if let Some(value) = item.get(name) {
                let message = value
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                return Err(format_bedrock_error(Some(pascal), message));
            }
        }
    }
    Ok(())
}

async fn handle_message_start(
    event: &Value,
    output: &AssistantMessage,
    sender: &EventSender,
) -> StreamResult<()> {
    if event.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err(
            "Unexpected assistant message start but got user message start instead".to_string(),
        );
    }
    let _ = sender
        .send(AssistantMessageEvent::Start {
            partial: output.clone(),
        })
        .await;
    Ok(())
}

fn handle_message_stop(event: &Value, output: &mut AssistantMessage) {
    let reason = event
        .get("stopReason")
        .and_then(Value::as_str)
        .unwrap_or("");
    output.raw_stop_reason = Some(reason.to_string());
    let (stop, error) = map_stop_reason(reason);
    output.stop_reason = stop;
    if let Some(error) = error {
        output.error_message = Some(error);
    }
}

/// TS `mapStopReason`, lines `1234-1249`.
fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "end_turn" | "stop_sequence" => (StopReason::Stop, None),
        "max_tokens" | "model_context_window_exceeded" => (StopReason::Length, None),
        "tool_use" => (StopReason::ToolUse, None),
        "" => (StopReason::Error, None),
        other => (
            StopReason::Error,
            Some(format!("Provider stopped with: {other}")),
        ),
    }
}

async fn handle_content_block_start(
    event: &Value,
    open_blocks: &mut OpenBlocks,
    output: &mut AssistantMessage,
    sender: &EventSender,
) {
    let Some(tool_use) = event.get("start").and_then(|s| s.get("toolUse")) else {
        return;
    };
    let index = event
        .get("contentBlockIndex")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    output.content.push(AssistantContent::ToolCall(ToolCall {
        id: tool_use
            .get("toolUseId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        name: tool_use
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        arguments: serde_json::Map::new(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    }));
    let content_index = output.content.len() - 1;
    open_blocks.insert(
        index,
        OpenBlock {
            content_index,
            arguments_raw: Some(String::new()),
            redacted_chunks: None,
        },
    );
    let _ = sender
        .send(AssistantMessageEvent::ToolcallStart {
            content_index,
            partial: output.clone(),
        })
        .await;
}

#[allow(clippy::too_many_lines)] // mirrors TS handleContentBlockDelta (lines 654-731)
async fn handle_content_block_delta(
    event: &Value,
    open_blocks: &mut OpenBlocks,
    output: &mut AssistantMessage,
    sender: &EventSender,
) {
    let index = event
        .get("contentBlockIndex")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let Some(delta) = event.get("delta") else {
        return;
    };

    if let Some(text) = delta.get("text").and_then(Value::as_str) {
        // Text blocks have no `contentBlockStart`; create one on first delta.
        let content_index = if let Some(open) = open_blocks.get(&index) {
            open.content_index
        } else {
            output
                .content
                .push(AssistantContent::Text(crate::types::TextContent {
                    text: String::new(),
                    text_signature: None,
                }));
            let content_index = output.content.len() - 1;
            open_blocks.insert(
                index,
                OpenBlock {
                    content_index,
                    arguments_raw: None,
                    redacted_chunks: None,
                },
            );
            let _ = sender
                .send(AssistantMessageEvent::TextStart {
                    content_index,
                    partial: output.clone(),
                })
                .await;
            content_index
        };
        if let Some(AssistantContent::Text(block)) = output.content.get_mut(content_index) {
            block.text.push_str(text);
        }
        let _ = sender
            .send(AssistantMessageEvent::TextDelta {
                content_index,
                delta: text.to_string(),
                partial: output.clone(),
            })
            .await;
    } else if let Some(tool_use) = delta.get("toolUse") {
        let Some(content_index) = open_blocks.get(&index).map(|open| open.content_index) else {
            return;
        };
        let is_tool = matches!(
            output.content.get(content_index),
            Some(AssistantContent::ToolCall(_))
        );
        if !is_tool
            || open_blocks
                .get(&index)
                .is_none_or(|open| open.arguments_raw.is_none())
        {
            return;
        }
        let input = tool_use.get("input").and_then(Value::as_str).unwrap_or("");
        if let Some(raw) = open_blocks
            .get_mut(&index)
            .and_then(|open| open.arguments_raw.as_mut())
        {
            raw.push_str(input);
        }
        let raw = open_blocks
            .get(&index)
            .and_then(|open| open.arguments_raw.clone())
            .unwrap_or_default();
        if let Some(AssistantContent::ToolCall(block)) = output.content.get_mut(content_index) {
            block.arguments = parse_tool_call_arguments(&raw).unwrap_or_default();
        }
        let _ = sender
            .send(AssistantMessageEvent::ToolcallDelta {
                content_index,
                delta: input.to_string(),
                partial: output.clone(),
            })
            .await;
    } else if let Some(reasoning) = delta.get("reasoningContent") {
        let content_index = if let Some(open) = open_blocks.get(&index) {
            open.content_index
        } else {
            output
                .content
                .push(AssistantContent::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: Some(String::new()),
                    redacted: false,
                }));
            let content_index = output.content.len() - 1;
            open_blocks.insert(
                index,
                OpenBlock {
                    content_index,
                    arguments_raw: None,
                    redacted_chunks: None,
                },
            );
            let _ = sender
                .send(AssistantMessageEvent::ThinkingStart {
                    content_index,
                    partial: output.clone(),
                })
                .await;
            content_index
        };

        if let Some(text) = reasoning.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(content_index) {
                block.thinking.push_str(text);
            }
            let _ = sender
                .send(AssistantMessageEvent::ThinkingDelta {
                    content_index,
                    delta: text.to_string(),
                    partial: output.clone(),
                })
                .await;
        }

        if let Some(signature) = reasoning.get("signature").and_then(Value::as_str) {
            if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(content_index)
                && !block.redacted
            {
                let mut next = block.thinking_signature.clone().unwrap_or_default();
                next.push_str(signature);
                block.thinking_signature = Some(next);
            }
        }

        if let Some(redacted) = reasoning.get("redactedContent") {
            let bytes = decode_blob(redacted);
            if !bytes.is_empty() {
                let mut first_redacted = false;
                if let Some(AssistantContent::Thinking(block)) =
                    output.content.get_mut(content_index)
                    && !block.redacted
                {
                    block.redacted = true;
                    block.thinking_signature = Some(String::new());
                    block.thinking.push_str(REDACTED_THINKING_PLACEHOLDER);
                    first_redacted = true;
                }
                if first_redacted {
                    let _ = sender
                        .send(AssistantMessageEvent::ThinkingDelta {
                            content_index,
                            delta: REDACTED_THINKING_PLACEHOLDER.to_string(),
                            partial: output.clone(),
                        })
                        .await;
                }
                if let Some(open) = open_blocks.get_mut(&index) {
                    open.redacted_chunks
                        .get_or_insert_with(Vec::new)
                        .extend_from_slice(&bytes);
                }
            }
        }
    }
}

async fn handle_content_block_stop(
    event: &Value,
    open_blocks: &mut OpenBlocks,
    output: &mut AssistantMessage,
    sender: &EventSender,
) {
    let index = event
        .get("contentBlockIndex")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let Some(mut open) = open_blocks.remove(&index) else {
        return;
    };
    let content_index = open.content_index;
    let kind = match output.content.get(content_index) {
        Some(AssistantContent::Text(_)) => 0u8,
        Some(AssistantContent::Thinking(_)) => 1,
        Some(AssistantContent::ToolCall(_)) => 2,
        None => return,
    };
    match kind {
        0 => {
            let content = output
                .content
                .get(content_index)
                .and_then(|block| block.as_text())
                .unwrap_or_default()
                .to_string();
            let _ = sender
                .send(AssistantMessageEvent::TextEnd {
                    content_index,
                    content,
                    partial: output.clone(),
                })
                .await;
        }
        1 => {
            flush_redacted(&mut open, output);
            let content = output
                .content
                .get(content_index)
                .and_then(|block| match block {
                    AssistantContent::Thinking(thinking) => Some(thinking.thinking.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let _ = sender
                .send(AssistantMessageEvent::ThinkingEnd {
                    content_index,
                    content,
                    partial: output.clone(),
                })
                .await;
        }
        2 => {
            if let Some(raw) = &open.arguments_raw
                && let Some(AssistantContent::ToolCall(block)) =
                    output.content.get_mut(content_index)
            {
                match parse_tool_call_arguments(raw) {
                    Ok(arguments) => {
                        block.arguments = arguments;
                    }
                    Err(error) => {
                        block.arguments = serde_json::Map::new();
                        block.arguments_error = Some(error);
                    }
                }
            }
            let tool_call = output
                .content
                .get(content_index)
                .and_then(|block| match block {
                    AssistantContent::ToolCall(call) => Some(call.clone()),
                    _ => None,
                });
            if let Some(tool_call) = tool_call {
                let _ = sender
                    .send(AssistantMessageEvent::ToolcallEnd {
                        content_index,
                        tool_call,
                        partial: output.clone(),
                    })
                    .await;
            }
        }
        _ => {}
    }
}

/// Encode buffered encrypted reasoning into `thinkingSignature` (TS lines
/// `733-749`). The chunks stay in stream state, never on the content block.
fn flush_redacted(open: &mut OpenBlock, output: &mut AssistantMessage) {
    let Some(chunks) = open.redacted_chunks.take() else {
        return;
    };
    if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(open.content_index) {
        block.thinking_signature = Some(STANDARD.encode(chunks));
    }
}

fn flush_open_blocks(open_blocks: &mut OpenBlocks, output: &mut AssistantMessage) {
    for open in open_blocks.values_mut() {
        flush_redacted(open, output);
    }
    open_blocks.clear();
}

/// Decode an AWS blob carried in JSON: a base64 string or an array of bytes.
fn decode_blob(value: &Value) -> Vec<u8> {
    if let Some(encoded) = value.as_str() {
        return STANDARD.decode(encoded).unwrap_or_default();
    }
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_u64)
                .map(|byte| byte as u8)
                .collect()
        })
        .unwrap_or_default()
}

fn handle_metadata(event: &Value, model: &Model, output: &mut AssistantMessage) {
    let Some(usage) = event.get("usage").filter(|usage| usage.is_object()) else {
        return;
    };
    let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    output.usage.input = field("inputTokens");
    output.usage.output = field("outputTokens");
    output.usage.cache_read = field("cacheReadInputTokens");
    output.usage.cache_write = field("cacheWriteInputTokens");
    output.usage.cache_write_1h =
        usage
            .get("cacheDetails")
            .and_then(Value::as_array)
            .map(|details| {
                details
                    .iter()
                    .filter(|detail| detail.get("ttl").and_then(Value::as_str) == Some("1h"))
                    .filter_map(|detail| detail.get("inputTokens").and_then(Value::as_u64))
                    .sum()
            });
    output.usage.total_tokens = {
        let total = field("totalTokens");
        if total == 0 {
            output.usage.input + output.usage.output
        } else {
            total
        }
    };
    calculate_cost(&mut output.usage, &model.cost);
}

/// Human-readable Bedrock error prefix (TS lines `406-451`).
fn format_bedrock_error(name: Option<&str>, core: &str) -> String {
    let prefix = name.and_then(|name| match name {
        "InternalServerException" => Some("Internal server error"),
        "ModelStreamErrorException" => Some("Model stream error"),
        "ValidationException" => Some("Validation error"),
        "ThrottlingException" => Some("Throttling error"),
        "ServiceUnavailableException" => Some("Service unavailable"),
        _ => None,
    });
    let hint = if core.to_ascii_lowercase().contains("data retention mode") {
        format!(" See {BEDROCK_DATA_RETENTION_DOCS_URL} for supported data retention modes.")
    } else {
        String::new()
    };
    match prefix {
        Some(prefix) => format!("{prefix}: {core}{hint}"),
        None => format!("{core}{hint}"),
    }
}

/// Map a non-success HTTP response into the SDK-style human-readable error
/// (TS `normalizeProviderError` + `formatBedrockError`, lines `434-451`).
fn format_http_error(status: u16, body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let name = parsed
        .as_ref()
        .and_then(|value| {
            value
                .get("__type")
                .or_else(|| value.get("code"))
                .or_else(|| value.get("name"))
        })
        .and_then(Value::as_str)
        .and_then(|value| value.rsplit('#').next())
        .map(str::to_string);
    let message = parsed
        .as_ref()
        .and_then(|value| value.get("message").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| format!("{status}: {body}"));
    format_bedrock_error(name.as_deref(), &message)
}

/// Build the Converse Stream request payload (TS lines `284-302`,
/// `958-1232`).
#[allow(clippy::too_many_lines)] // mirrors the TS request assembly; split when extending
fn build_request_body(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
) -> Value {
    let system = system_prompt_text(&context.messages);
    let cache = resolve_cache_retention(options);
    let messages = convert_messages(context, model, cache);
    let tool_config = convert_tool_config(model, context, options);

    let mut inference = serde_json::Map::new();
    let max_tokens = options
        .max_tokens
        .or_else(|| is_claude_model(model).then_some(model.max_tokens))
        .unwrap_or(0);
    if max_tokens > 0 {
        inference.insert("maxTokens".to_string(), json!(max_tokens));
    }
    if let Some(temperature) = options.temperature {
        inference.insert("temperature".to_string(), json!(temperature));
    }

    let mut body = json!({
        "modelId": model.id,
        "messages": messages,
        "inferenceConfig": inference,
    });
    if let Some(system) = build_system_prompt(&system, model, cache) {
        body["system"] = system;
    }
    if let Some(tools) = tool_config {
        body["toolConfig"] = tools;
    }
    if let Some(fields) = additional_model_fields(model, options) {
        body["additionalModelRequestFields"] = fields;
    }
    if let Some(metadata) = &options.metadata {
        body["requestMetadata"] = Value::Object(metadata.clone());
    }
    body
}

/// Fold every system message into one prompt (TS `collapseSystemMessages`,
/// lines `148-149` and `280-281`).
fn system_prompt_text(messages: &[Message]) -> String {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::System(system) => Some(system_text(system)),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn resolve_cache_retention(options: &StreamOptions) -> CacheRetention {
    if options.cache_retention == Some(CacheRetention::Long)
        || env("PI_CACHE_RETENTION").as_deref() == Some("long")
    {
        CacheRetention::Long
    } else {
        CacheRetention::Short
    }
}

fn model_match_candidates(model: &Model) -> Vec<String> {
    [model.id.as_str(), model.name.as_str()]
        .into_iter()
        .filter(|value| !value.is_empty())
        .flat_map(|value| {
            let lower = value.to_ascii_lowercase();
            let normalized: String = lower
                .chars()
                .map(|ch| {
                    if matches!(ch, ' ' | '_' | '.' | ':') {
                        '-'
                    } else {
                        ch
                    }
                })
                .collect();
            [lower, normalized]
        })
        .collect()
}

fn is_claude_model(model: &Model) -> bool {
    let id = model.id.to_ascii_lowercase();
    let name = model.name.to_ascii_lowercase();
    id.contains("anthropic.claude")
        || id.contains("anthropic/claude")
        || name.contains("anthropic.claude")
        || name.contains("anthropic/claude")
        || name.contains("claude")
}

fn supports_prompt_caching(model: &Model) -> bool {
    let candidates = model_match_candidates(model);
    if !candidates
        .iter()
        .any(|candidate| candidate.contains("claude"))
    {
        return env("AWS_BEDROCK_FORCE_CACHE").as_deref() == Some("1");
    }
    candidates.iter().any(|candidate| {
        [
            "fable-5",
            "opus-5",
            "sonnet-5",
            "haiku-5",
            "-4-",
            "claude-3-7-sonnet",
            "claude-3-5-haiku",
        ]
        .iter()
        .any(|needle| candidate.contains(needle))
    })
}

fn cache_point(retention: CacheRetention) -> Value {
    let mut point = json!({ "type": "default" });
    if retention == CacheRetention::Long {
        point["ttl"] = json!("1h");
    }
    json!({ "cachePoint": point })
}

fn build_system_prompt(text: &str, model: &Model, retention: CacheRetention) -> Option<Value> {
    if text.is_empty() {
        return None;
    }
    let mut blocks = vec![json!({ "text": text })];
    if supports_prompt_caching(model) {
        blocks.push(cache_point(retention));
    }
    Some(Value::Array(blocks))
}

fn normalize_tool_call_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    sanitized.chars().take(64).collect()
}

fn text_block(text: &str) -> Option<Value> {
    if text.trim().is_empty() {
        None
    } else {
        Some(json!({ "text": text }))
    }
}

fn image_block(content: &InputContent) -> StreamResult<Value> {
    let InputContent::Image(image) = content else {
        return Err("not an image".to_string());
    };
    let format = match image.mime_type.as_str() {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        other => return Err(format!("Unknown image type: {other}")),
    };
    Ok(json!({
        "image": {
            "format": format,
            "source": { "bytes": image.data }
        }
    }))
}

/// TS `convertMessages`, lines `1020-1189`.
#[allow(clippy::too_many_lines)] // mirrors the TS message conversion; split when extending
fn convert_messages(
    context: &TranscriptContext,
    model: &Model,
    retention: CacheRetention,
) -> Vec<Value> {
    let mut result: Vec<Value> = Vec::new();
    let mut index = 0;
    let messages = &context.messages;
    while index < messages.len() {
        match &messages[index] {
            Message::System(_) => {
                index += 1;
            }
            Message::User(user) => {
                let mut content: Vec<Value> = Vec::new();
                match &user.content {
                    UserContent::Text(text) => {
                        content.push(
                            text_block(text)
                                .unwrap_or_else(|| json!({ "text": EMPTY_TEXT_PLACEHOLDER })),
                        );
                    }
                    UserContent::Blocks(blocks) => {
                        for block in blocks {
                            match block {
                                InputContent::Text(text) => {
                                    if let Some(block) = text_block(&text.text) {
                                        content.push(block);
                                    }
                                }
                                InputContent::Image(_) => {
                                    if let Ok(block) = image_block(block) {
                                        content.push(block);
                                    }
                                }
                            }
                        }
                        if content.is_empty() {
                            content.push(json!({ "text": EMPTY_TEXT_PLACEHOLDER }));
                        }
                    }
                }
                result.push(json!({ "role": "user", "content": content }));
                index += 1;
            }
            Message::Assistant(assistant) => {
                let mut content: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => {
                            if let Some(block) = text_block(&text.text) {
                                content.push(block);
                            }
                        }
                        AssistantContent::ToolCall(call) => {
                            content.push(json!({
                                "toolUse": {
                                    "toolUseId": normalize_tool_call_id(&call.id),
                                    "name": call.name,
                                    "input": sanitize_document(&Value::Object(call.arguments.clone())),
                                }
                            }));
                        }
                        AssistantContent::Thinking(thinking) => {
                            content.extend(thinking_content_block(thinking, model));
                        }
                    }
                }
                if !content.is_empty() {
                    result.push(json!({ "role": "assistant", "content": content }));
                }
                index += 1;
            }
            Message::ToolResult(_) => {
                let mut blocks: Vec<Value> = Vec::new();
                while index < messages.len() {
                    let Message::ToolResult(result_message) = &messages[index] else {
                        break;
                    };
                    let mut content: Vec<Value> = Vec::new();
                    for block in &result_message.content {
                        match block {
                            InputContent::Text(text) => {
                                if let Some(block) = text_block(&text.text) {
                                    content.push(block);
                                }
                            }
                            InputContent::Image(_) => {
                                if let Ok(block) = image_block(block) {
                                    content.push(block);
                                }
                            }
                        }
                    }
                    if content.is_empty() {
                        content.push(json!({ "text": EMPTY_TEXT_PLACEHOLDER }));
                    }
                    blocks.push(json!({
                        "toolResult": {
                            "toolUseId": normalize_tool_call_id(&result_message.tool_call_id),
                            "content": content,
                            "status": if result_message.is_error { "error" } else { "success" },
                        }
                    }));
                    index += 1;
                }
                result.push(json!({ "role": "user", "content": blocks }));
            }
        }
    }

    if supports_prompt_caching(model)
        && let Some(last) = result.last_mut()
    {
        if last.get("role").and_then(Value::as_str) == Some("user") {
            if let Some(content) = last.get_mut("content").and_then(Value::as_array_mut) {
                content.push(cache_point(retention));
            }
        }
    }
    result
}

/// Replay one thinking block (TS lines `1082-1121`).
fn thinking_content_block(thinking: &ThinkingContent, model: &Model) -> Vec<Value> {
    if thinking.redacted {
        let decoded = thinking
            .thinking_signature
            .as_deref()
            .and_then(|signature| STANDARD.decode(signature).ok())
            .unwrap_or_default();
        if !decoded.is_empty() {
            return vec![json!({
                "reasoningContent": { "redactedContent": STANDARD.encode(decoded) }
            })];
        }
        return Vec::new();
    }
    if thinking.thinking.trim().is_empty() {
        return Vec::new();
    }
    if !is_claude_model(model) {
        return vec![json!({
            "reasoningContent": { "reasoningText": { "text": thinking.thinking } }
        })];
    }
    match thinking.thinking_signature.as_deref() {
        Some(signature) if !signature.trim().is_empty() => vec![json!({
            "reasoningContent": {
                "reasoningText": { "text": thinking.thinking, "signature": signature }
            }
        })],
        _ => vec![json!({ "text": thinking.thinking })],
    }
}

/// Drop object keys with empty names (TS `sanitizeBedrockDocument`, lines
/// `992-1004`).
fn sanitize_document(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sanitize_document).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| !key.is_empty())
                .map(|(key, value)| (key.clone(), sanitize_document(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn convert_tool_config(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
) -> Option<Value> {
    if options.tool_choice == Some(crate::types::ToolChoice::None) {
        return None;
    }
    let tools = tools_at_point(&context.messages);
    if tools.is_empty() {
        return None;
    }
    let supports_strict = model
        .compat
        .as_ref()
        .and_then(|compat| compat.get("supportsStrictMode"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let bedrock_tools: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let strict = match &tool.constrained_sampling {
                Some(crate::constrained_sampling::ConstrainedSampling::JsonSchema {
                    strict: crate::constrained_sampling::StrictMode::Require,
                }) => true,
                Some(crate::constrained_sampling::ConstrainedSampling::JsonSchema {
                    strict: crate::constrained_sampling::StrictMode::Prefer,
                }) => supports_strict,
                _ => false,
            };
            let mut spec = json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": { "json": tool.parameters },
            });
            if strict {
                spec["strict"] = json!(true);
            }
            json!({ "toolSpec": spec })
        })
        .collect();
    let mut config = json!({ "tools": bedrock_tools });
    if options.tool_choice == Some(crate::types::ToolChoice::Auto) {
        config["toolChoice"] = json!({ "auto": {} });
    }
    Some(config)
}

/// Thinking / reasoning overrides (TS `buildAdditionalModelRequestFields`,
/// lines `1311-1382`).
fn additional_model_fields(model: &Model, options: &StreamOptions) -> Option<Value> {
    let level = options.reasoning?;
    if !model.reasoning || level == ThinkingLevel::Off {
        return None;
    }
    let candidates = model_match_candidates(model);
    let adaptive = [
        "opus-4-6",
        "opus-4-7",
        "opus-4-8",
        "opus-5",
        "sonnet-4-6",
        "sonnet-5",
        "haiku-5",
        "fable-5",
    ]
    .iter()
    .any(|needle| {
        candidates
            .iter()
            .any(|candidate| candidate.contains(needle))
    });

    if is_claude_model(model) {
        let display = json!("summarized");
        if adaptive {
            let mut thinking = json!({ "type": "adaptive", "display": display });
            if [
                "opus-4-7", "opus-4-8", "opus-5", "sonnet-5", "haiku-5", "fable-5",
            ]
            .iter()
            .any(|needle| {
                candidates
                    .iter()
                    .any(|candidate| candidate.contains(needle))
            }) {
                thinking["block_binding"] = json!({ "prefix_mismatch_behavior": "drop_block" });
            }
            return Some(json!({
                "thinking": thinking,
                "output_config": { "effort": effort_for(level) },
            }));
        }
        let budget = match level {
            ThinkingLevel::Minimal => 1024,
            ThinkingLevel::Low => 2048,
            ThinkingLevel::Medium => 8192,
            ThinkingLevel::High | ThinkingLevel::Off => 16_384,
        };
        return Some(json!({
            "thinking": { "type": "enabled", "budget_tokens": budget, "display": display },
            "anthropic_beta": ["interleaved-thinking-2025-05-14"],
        }));
    }

    if candidates
        .iter()
        .any(|candidate| candidate.contains("gpt-oss"))
    {
        let effort = match level {
            ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High | ThinkingLevel::Off => "high",
        };
        return Some(json!({ "reasoning_effort": effort }));
    }
    if candidates
        .iter()
        .any(|candidate| candidate.contains("gpt-"))
    {
        let effort = match level {
            ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High | ThinkingLevel::Off => "high",
        };
        return Some(json!({ "reasoning": { "effort": effort } }));
    }
    None
}

fn effort_for(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High | ThinkingLevel::Off => "high",
    }
}

/// One decoded AWS event-stream message.
#[derive(Debug, Clone, PartialEq)]
enum DecodedEvent {
    /// A modeled event or exception, keyed by its event type (mirrors the TS
    /// SDK item shape, e.g. `{ "contentBlockDelta": { … } }`).
    Item(Value),
    /// A low-level error frame.
    Error(String),
}

/// Incremental decoder for the binary AWS event stream (content type
/// `application/vnd.amazon.eventstream`). Frames may be split arbitrarily
/// across HTTP chunks.
#[derive(Default)]
struct EventStreamDecoder {
    buffer: Vec<u8>,
}

impl EventStreamDecoder {
    fn new() -> Self {
        Self::default()
    }

    fn feed(&mut self, chunk: &[u8]) -> StreamResult<Vec<DecodedEvent>> {
        self.buffer.extend_from_slice(chunk);
        self.drain_frames()
    }

    fn finish(&mut self) -> StreamResult<Vec<DecodedEvent>> {
        if self.buffer.is_empty() {
            Ok(Vec::new())
        } else {
            Err("Bedrock event stream ended with an incomplete frame".to_string())
        }
    }

    fn drain_frames(&mut self) -> StreamResult<Vec<DecodedEvent>> {
        let mut events = Vec::new();
        loop {
            if self.buffer.len() < 16 {
                break;
            }
            let total = u32::from_be_bytes(self.buffer[0..4].try_into().unwrap_or([0; 4])) as usize;
            if total < 16 {
                return Err("invalid Bedrock event stream frame length".to_string());
            }
            if self.buffer.len() < total {
                break;
            }
            let frame: Vec<u8> = self.buffer[..total].to_vec();
            self.buffer.drain(..total);
            events.push(decode_frame(&frame)?);
        }
        Ok(events)
    }
}

/// `CRC-32` (IEEE) used by the event-stream prelude and message checksums.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn take<'a>(data: &'a [u8], cursor: &mut usize, length: usize) -> StreamResult<&'a [u8]> {
    let end = cursor
        .checked_add(length)
        .ok_or("malformed event stream header")?;
    let slice = data
        .get(*cursor..end)
        .ok_or("malformed event stream header")?;
    *cursor = end;
    Ok(slice)
}

/// Only string headers are interesting for event dispatch; other types are
/// consumed to advance the cursor and then ignored.
fn parse_headers(data: &[u8]) -> StreamResult<HashMap<String, String>> {
    let mut headers = HashMap::new();
    let mut cursor = 0;
    while cursor < data.len() {
        let name_length = usize::from(take(data, &mut cursor, 1)?[0]);
        let name = std::str::from_utf8(take(data, &mut cursor, name_length)?)
            .map_err(|e| format!("invalid event stream header name: {e}"))?
            .to_string();
        let kind = take(data, &mut cursor, 1)?[0];
        match kind {
            0 | 1 => {}
            2 => {
                take(data, &mut cursor, 1)?;
            }
            3 => {
                take(data, &mut cursor, 2)?;
            }
            4 => {
                take(data, &mut cursor, 4)?;
            }
            5 | 8 => {
                take(data, &mut cursor, 8)?;
            }
            6 | 7 => {
                let length_bytes = take(data, &mut cursor, 2)?;
                let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
                let bytes = take(data, &mut cursor, length)?;
                if kind == 7 {
                    let value = std::str::from_utf8(bytes)
                        .map_err(|e| format!("invalid event stream header value: {e}"))?
                        .to_string();
                    headers.insert(name, value);
                }
            }
            9 => {
                take(data, &mut cursor, 16)?;
            }
            _ => return Err(format!("unknown event stream header type: {kind}")),
        }
    }
    Ok(headers)
}

fn decode_frame(frame: &[u8]) -> StreamResult<DecodedEvent> {
    let headers_len = u32::from_be_bytes(frame[4..8].try_into().unwrap_or([0; 4])) as usize;
    let prelude_crc = u32::from_be_bytes(frame[8..12].try_into().unwrap_or([0; 4]));
    if crc32(&frame[..8]) != prelude_crc {
        return Err("Bedrock event stream prelude checksum mismatch".to_string());
    }
    if headers_len + 16 > frame.len() {
        return Err("invalid Bedrock event stream header length".to_string());
    }
    let payload_end = frame.len() - 4;
    let message_crc = u32::from_be_bytes(frame[payload_end..].try_into().unwrap_or([0; 4]));
    if crc32(&frame[..payload_end]) != message_crc {
        return Err("Bedrock event stream message checksum mismatch".to_string());
    }
    let headers = parse_headers(&frame[12..12 + headers_len])?;
    let message_type = headers
        .get(":message-type")
        .map(String::as_str)
        .unwrap_or("event");
    let raw_type = headers.get(":event-type").map(String::as_str).unwrap_or("");
    // Upstream keys are camelCase (`messageStart`, `throttlingException`). Some
    // SDKs emit the PascalCase shape name in `:event-type`; normalize it here.
    let event_type = match raw_type.chars().next() {
        Some(first) if first.is_ascii_uppercase() => {
            let mut chars = raw_type.chars();
            let lowered = chars.next().unwrap_or(first).to_ascii_lowercase();
            format!("{lowered}{}", chars.as_str())
        }
        _ => raw_type.to_string(),
    };
    let payload: Value = serde_json::from_slice(&frame[12 + headers_len..payload_end])
        .map_err(|e| format!("could not parse Bedrock event payload: {e}"))?;

    match message_type {
        "event" | "exception" => {
            let key = if event_type.is_empty() {
                payload
                    .as_object()
                    .and_then(|object| object.keys().next())
                    .cloned()
                    .unwrap_or_default()
            } else {
                event_type
            };
            if key.is_empty() {
                return Err("Bedrock event stream event has no type".to_string());
            }
            let value = if payload.is_object() && payload.get(&key).is_some() {
                payload
            } else {
                json!({ key: payload })
            };
            Ok(DecodedEvent::Item(value))
        }
        "error" => {
            let message = payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(DecodedEvent::Error(message))
        }
        other => Err(format!(
            "unknown Bedrock event stream message type: {other}"
        )),
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
            id: "anthropic.claude-3-5-sonnet-20241022-v2:0".into(),
            name: "Claude Sonnet".into(),
            api: "bedrock-converse-stream".into(),
            provider: "amazon-bedrock".into(),
            base_url: String::new(),
            input: vec![ModelInputModality::Text],
            input_limits: None,
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                ..Default::default()
            },
            headers: None,
            r#type: None,
            reasoning: false,
            thinking_level_map: None,
            prompt_cache: None,
            context_window: 200_000,
            max_tokens: 4096,
            sampling_params: None,
            sampling_params_by_thinking_level: None,
            compat: None,
        }
    }

    fn context(messages: Vec<Message>) -> TranscriptContext {
        TranscriptContext { messages }
    }

    fn system(text: &str) -> Message {
        Message::System(SystemMessage {
            content: StringOrTextBlocks::Text(text.into()),
            sections: None,
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
        })
    }

    fn user(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp: 2,
        })
    }

    /// Encode one AWS event-stream frame with valid checksums for the tests.
    fn encode_frame(message_type: &str, event_type: &str, payload: &Value) -> Vec<u8> {
        let mut headers = Vec::new();
        for (name, value) in [(":message-type", message_type), (":event-type", event_type)] {
            headers.push(name.len() as u8);
            headers.extend_from_slice(name.as_bytes());
            headers.push(7);
            headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        }
        let body = serde_json::to_vec(payload).unwrap();
        let total = 16 + headers.len() + body.len();
        let mut frame = Vec::with_capacity(total);
        frame.extend_from_slice(&(total as u32).to_be_bytes());
        frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        frame.extend_from_slice(&crc32(&frame));
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(&body);
        frame.extend_from_slice(&crc32(&frame));
        frame
    }

    fn decode_all(frames: Vec<Vec<u8>>) -> Vec<DecodedEvent> {
        let mut decoder = EventStreamDecoder::new();
        let mut all = Vec::new();
        for frame in frames {
            all.extend(decoder.feed(&frame).unwrap());
        }
        all.extend(decoder.finish().unwrap());
        all
    }

    #[test]
    fn request_payload_shape() {
        let model = test_model();
        let ctx = context(vec![system("Be terse."), user("hi")]);
        let body = build_request_body(&model, &ctx, &StreamOptions::default());
        assert_eq!(body["modelId"], model.id);
        assert_eq!(body["system"][0]["text"], "Be terse.");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
        assert_eq!(body["inferenceConfig"]["maxTokens"], 4096);
    }

    #[test]
    fn thinking_block_replay() {
        let model = test_model();
        let thinking = ThinkingContent {
            thinking: "reasoning".into(),
            thinking_signature: Some("sig".into()),
            redacted: false,
        };
        let block = thinking_content_block(&thinking, &model);
        assert_eq!(
            block[0]["reasoningContent"]["reasoningText"]["signature"],
            "sig"
        );

        let redacted = ThinkingContent {
            thinking: "[Reasoning redacted]".into(),
            thinking_signature: Some(STANDARD.encode(b"secret")),
            redacted: true,
        };
        let block = thinking_content_block(&redacted, &model);
        assert_eq!(
            block[0]["reasoningContent"]["redactedContent"],
            STANDARD.encode(b"secret")
        );
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(map_stop_reason("end_turn").0, StopReason::Stop);
        assert_eq!(map_stop_reason("tool_use").0, StopReason::ToolUse);
        assert_eq!(map_stop_reason("max_tokens").0, StopReason::Length);
        assert_eq!(map_stop_reason("mystery").0, StopReason::Error);
        assert_eq!(
            map_stop_reason("mystery").1.as_deref(),
            Some("Provider stopped with: mystery")
        );
    }

    #[test]
    fn error_prefix_mapping() {
        assert_eq!(
            format_bedrock_error(Some("ThrottlingException"), "slow down"),
            "Throttling error: slow down"
        );
        assert_eq!(format_bedrock_error(None, "boom"), "boom");
        assert!(
            format_bedrock_error(None, "data retention mode 'default' unavailable")
                .contains(BEDROCK_DATA_RETENTION_DOCS_URL)
        );
        assert_eq!(
            format_http_error(403, r#"{"__type":"ValidationException","message":"bad"}"#),
            "Validation error: bad"
        );
    }

    #[tokio::test]
    async fn decodes_text_thinking_and_toolcall_events() {
        let frames = vec![
            encode_frame("event", "messageStart", &json!({"role":"assistant"})),
            encode_frame(
                "event",
                "contentBlockDelta",
                &json!({"contentBlockIndex":0,"delta":{"text":"hel"}}),
            ),
            encode_frame(
                "event",
                "contentBlockDelta",
                &json!({"contentBlockIndex":0,"delta":{"text":"lo"}}),
            ),
            encode_frame("event", "contentBlockStop", &json!({"contentBlockIndex":0})),
            encode_frame(
                "event",
                "contentBlockDelta",
                &json!({"contentBlockIndex":1,"delta":{"reasoningContent":{"text":"hmm","signature":"sig"}}}),
            ),
            encode_frame("event", "contentBlockStop", &json!({"contentBlockIndex":1})),
            encode_frame(
                "event",
                "contentBlockStart",
                &json!({"contentBlockIndex":2,"start":{"toolUse":{"toolUseId":"t1","name":"bash"}}}),
            ),
            encode_frame(
                "event",
                "contentBlockDelta",
                &json!({"contentBlockIndex":2,"delta":{"toolUse":{"input":"{\"cmd\":"}}}),
            ),
            encode_frame(
                "event",
                "contentBlockDelta",
                &json!({"contentBlockIndex":2,"delta":{"toolUse":{"input":"\"ls\"}"}}}),
            ),
            encode_frame("event", "contentBlockStop", &json!({"contentBlockIndex":2})),
            encode_frame(
                "event",
                "metadata",
                &json!({"usage":{"inputTokens":10,"outputTokens":5,"totalTokens":15}}),
            ),
            encode_frame("event", "messageStop", &json!({"stopReason":"tool_use"})),
        ];
        let events = decode_all(frames);
        assert_eq!(events.len(), 12);
        assert!(
            events[0].clone() == DecodedEvent::Item(json!({"messageStart":{"role":"assistant"}}))
        );

        let mut output = initial_output(&test_model());
        let mut open = OpenBlocks::new();
        let (_, tx) = AssistantMessageEventStream::channel();
        let model = test_model();
        let options = StreamOptions::default();
        for event in events {
            process_event(event, &model, &options, &mut output, &mut open, &tx)
                .await
                .unwrap();
        }
        assert_eq!(output.content.len(), 3);
        assert_eq!(output.content[0].as_text(), Some("hello"));
        assert!(
            matches!(&output.content[1], AssistantContent::Thinking(t) if t.thinking == "hmm" && t.thinking_signature.as_deref() == Some("sig"))
        );
        assert!(
            matches!(&output.content[2], AssistantContent::ToolCall(c) if c.name == "bash" && c.arguments.get("cmd").and_then(Value::as_str) == Some("ls"))
        );
        assert_eq!(output.usage.input, 10);
        assert_eq!(output.usage.output, 5);
        assert_eq!(output.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn exception_and_error_frames() {
        let events = decode_all(vec![
            encode_frame(
                "exception",
                "throttlingException",
                &json!({"message":"slow down"}),
            ),
            encode_frame("error", "", &json!({"message":"boom"})),
        ]);
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[0], DecodedEvent::Item(value) if value.get("throttlingException").is_some())
        );
        assert!(matches!(&events[1], DecodedEvent::Error(message) if message == "boom"));
    }

    #[test]
    fn frame_checksums_are_validated() {
        let mut frame = encode_frame("event", "messageStart", &json!({"role":"assistant"}));
        frame[20] ^= 0xff;
        let mut decoder = EventStreamDecoder::new();
        assert!(decoder.feed(&frame).is_err());
    }

    #[tokio::test]
    async fn redacted_reasoning_flushes_to_signature() {
        let frames = vec![
            encode_frame(
                "event",
                "contentBlockDelta",
                &json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"redactedContent":"c2VjcmV0"}}}),
            ),
            encode_frame("event", "contentBlockStop", &json!({"contentBlockIndex":0})),
        ];
        let events = decode_all(frames);
        let mut output = initial_output(&test_model());
        let mut open = OpenBlocks::new();
        let (_, tx) = AssistantMessageEventStream::channel();
        let model = test_model();
        let options = StreamOptions::default();
        for event in events {
            process_event(event, &model, &options, &mut output, &mut open, &tx)
                .await
                .unwrap();
        }
        assert!(
            matches!(&output.content[0], AssistantContent::Thinking(thinking) if thinking.redacted && thinking.thinking_signature.as_deref() == Some("c2VjcmV0"))
        );
    }

    #[test]
    fn decoder_handles_frames_split_across_chunks() {
        let frame = encode_frame("event", "messageStart", &json!({"role":"assistant"}));
        let (left, right) = frame.split_at(7);
        let mut decoder = EventStreamDecoder::new();
        assert!(decoder.feed(left).unwrap().is_empty());
        assert_eq!(
            decoder.feed(right).unwrap(),
            vec![DecodedEvent::Item(
                json!({"messageStart":{"role":"assistant"}})
            )]
        );
    }

    #[test]
    fn normalizes_pascal_case_event_types() {
        let events = decode_all(vec![encode_frame(
            "event",
            "MessageStart",
            &json!({"role":"assistant"}),
        )]);
        assert_eq!(
            events,
            vec![DecodedEvent::Item(
                json!({"messageStart":{"role":"assistant"}})
            )]
        );
    }
}
