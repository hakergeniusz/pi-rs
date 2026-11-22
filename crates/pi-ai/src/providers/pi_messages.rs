//! `pi-messages` API adapter: port of TS `packages/ai/src/api/pi-messages.ts`
//! (lines 1-465), with the lazy registration stub at `pi-messages.lazy.ts:1-4`.
//!
//! This is pi's own serialized assistant-message event protocol, spoken by the
//! local llama.cpp / llama-swap bridge and the Radius gateway. The request is
//! one `POST <baseUrl>/messages` of `{ model, context, options }`; the response
//! is an SSE stream of serialized `AssistantMessageEvent`s plus a terminal
//! `done`/`error` event. Ported areas:
//!
//! - wire events `PiMessagesEvent` / `PiMessagesRewriteImpact` (lines 45-91)
//! - error body parsing, formatting and diagnostics (lines 93-160, 341-363)
//! - the event converter with `contentIndex` validation (lines 184-292)
//! - SSE framing: CR/LF normalization, first `data:` line, `[DONE]` skip,
//!   trailing-buffer flush (lines 294-339)
//! - the terminal-event check that turns a bare `[DONE]`/EOF into an `error`
//!   event instead of a silently truncated stream (lines 435-447)
//! - request construction, hooks and key resolution (lines 365-451)
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

use crate::event_stream::{AssistantMessageEventStream, EventSender};
use crate::providers::common::{
    apply_payload_hook, apply_response_hook, apply_stream_event_hook, initial_output,
    response_metadata,
};
use crate::sse::{SseError, SseMessage, read_sse_stream};
use crate::streaming_json::parse_tool_call_arguments;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, JsonValue, Model,
    StopReason, StreamOptions, ToolCall, TranscriptContext, Usage, now_ms,
};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::time::Duration;

/// Impact summary of a server-side message rewrite (TS lines 45-53).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiMessagesRewriteImpact {
    pub policy_id: String,
    pub policy_version: i64,
    pub changed: bool,
    pub token_count_change: i64,
    pub message_count_change: i64,
    pub system_prompt_changed: bool,
}

/// A serialized assistant-message event on the wire (TS lines 55-91).
/// Field names mirror the `camelCase` TS discriminated union.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PiMessagesEvent {
    Start,
    TextStart {
        content_index: i64,
    },
    TextDelta {
        content_index: i64,
        delta: String,
    },
    TextEnd {
        content_index: i64,
        content: String,
        content_signature: Option<String>,
    },
    ThinkingStart {
        content_index: i64,
    },
    ThinkingDelta {
        content_index: i64,
        delta: String,
    },
    ThinkingEnd {
        content_index: i64,
        content: String,
        content_signature: Option<String>,
        redacted: Option<bool>,
    },
    ToolcallStart {
        content_index: i64,
        id: String,
        tool_name: String,
    },
    ToolcallDelta {
        content_index: i64,
        delta: String,
    },
    ToolcallEnd {
        content_index: i64,
        tool_call: ToolCall,
    },
    Done {
        reason: StopReason,
        usage: JsonValue,
        response_id: Option<String>,
        response_model: Option<String>,
        provider_thinking_level: Option<String>,
        rewrite: Option<PiMessagesRewriteImpact>,
    },
    Error {
        reason: StopReason,
        usage: JsonValue,
        error_message: Option<String>,
        response_id: Option<String>,
        response_model: Option<String>,
        provider_thinking_level: Option<String>,
        rewrite: Option<PiMessagesRewriteImpact>,
    },
}

impl PiMessagesEvent {
    /// The TS `type` discriminant, used in `contentIndex` diagnostics.
    fn kind(&self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::TextStart { .. } => "text_start",
            Self::TextDelta { .. } => "text_delta",
            Self::TextEnd { .. } => "text_end",
            Self::ThinkingStart { .. } => "thinking_start",
            Self::ThinkingDelta { .. } => "thinking_delta",
            Self::ThinkingEnd { .. } => "thinking_end",
            Self::ToolcallStart { .. } => "toolcall_start",
            Self::ToolcallDelta { .. } => "toolcall_delta",
            Self::ToolcallEnd { .. } => "toolcall_end",
            Self::Done { .. } => "done",
            Self::Error { .. } => "error",
        }
    }

    /// The wire `contentIndex`, if this event carries one.
    fn content_index(&self) -> Option<i64> {
        match self {
            Self::Start | Self::Done { .. } | Self::Error { .. } => None,
            Self::TextStart { content_index, .. }
            | Self::TextDelta { content_index, .. }
            | Self::TextEnd { content_index, .. }
            | Self::ThinkingStart { content_index, .. }
            | Self::ThinkingDelta { content_index, .. }
            | Self::ThinkingEnd { content_index, .. }
            | Self::ToolcallStart { content_index, .. }
            | Self::ToolcallDelta { content_index, .. }
            | Self::ToolcallEnd { content_index, .. } => Some(*content_index),
        }
    }
}

/// Lenient `Usage` decode: a backend may omit any numeric field. Mirrors the
/// TS `Object.assign(partial, { usage: event.usage })` verbatim-assignment
/// semantics while keeping Rust's typed `Usage` total (TS lines 208-221).
fn parse_usage(value: &JsonValue) -> Usage {
    let field = |name: &str| value.get(name).and_then(Value::as_u64).unwrap_or(0);
    let cost_value = value.get("cost");
    let cost_field = |name: &str| {
        cost_value
            .and_then(|c| c.get(name))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
    };
    Usage {
        input: field("input"),
        output: field("output"),
        cache_read: field("cacheRead"),
        cache_write: field("cacheWrite"),
        cache_write_1h: value
            .get("cacheWrite1h")
            .and_then(Value::as_u64)
            .or_else(|| value.get("cacheWrite1H").and_then(Value::as_u64)),
        reasoning: value.get("reasoning").and_then(Value::as_u64),
        total_tokens: field("totalTokens"),
        cost: crate::types::UsageCost {
            input: cost_field("input"),
            output: cost_field("output"),
            cache_read: cost_field("cacheRead"),
            cache_write: cost_field("cacheWrite"),
            total: cost_field("total"),
            total_from_provider: cost_value
                .and_then(|c| c.get("totalFromProvider"))
                .and_then(Value::as_bool),
        },
    }
}

/// Append a diagnostic to an assistant message (TS `utils/diagnostics.ts:66-68`).
fn append_diagnostic(message: &mut AssistantMessage, diagnostic: JsonValue) {
    message.diagnostics.push(diagnostic);
}

/// Stateful wire-event → `AssistantMessageEvent` converter (TS lines 184-292).
struct Converter {
    partial: AssistantMessage,
    tool_arguments: HashMap<usize, String>,
}

impl Converter {
    fn new(model: &Model) -> Self {
        Self {
            partial: initial_output(model),
            tool_arguments: HashMap::new(),
        }
    }

    /// Convert one wire event; `Err` is the stream's terminal error, matching
    /// an exception thrown inside the TS converter.
    #[allow(clippy::too_many_lines)] // mirrors the TS switch statement; split when extending
    fn apply(&mut self, event: PiMessagesEvent) -> Result<AssistantMessageEvent, String> {
        if let Some(index) = event.content_index() {
            // TS lines 198-206: starts append a new block, other events must
            // address the last existing one. Without this check a hostile
            // backend could index a sparse array of billions of holes.
            let length = i64::try_from(self.partial.content.len()).unwrap_or(i64::MAX);
            let limit = if event.kind().ends_with("_start") {
                length
            } else {
                length - 1
            };
            if index < 0 || index > limit {
                return Err(format!(
                    "Invalid contentIndex {index} in pi-messages {} event",
                    event.kind()
                ));
            }
        }

        match event {
            PiMessagesEvent::Start => Ok(AssistantMessageEvent::Start {
                partial: self.partial.clone(),
            }),
            PiMessagesEvent::TextStart { content_index } => {
                let index = content_index as usize;
                self.partial
                    .content
                    .push(AssistantContent::Text(crate::types::TextContent {
                        text: String::new(),
                        text_signature: None,
                    }));
                Ok(AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial: self.partial.clone(),
                })
            }
            PiMessagesEvent::TextDelta {
                content_index,
                delta,
            } => {
                let index = content_index as usize;
                if let Some(AssistantContent::Text(block)) = self.partial.content.get_mut(index) {
                    block.text.push_str(&delta);
                }
                Ok(AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta,
                    partial: self.partial.clone(),
                })
            }
            PiMessagesEvent::TextEnd {
                content_index,
                content,
                content_signature,
            } => {
                let index = content_index as usize;
                if let Some(AssistantContent::Text(block)) = self.partial.content.get_mut(index) {
                    block.text = content.clone();
                    block.text_signature = content_signature;
                }
                Ok(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content,
                    partial: self.partial.clone(),
                })
            }
            PiMessagesEvent::ThinkingStart { content_index } => {
                let index = content_index as usize;
                self.partial.content.push(AssistantContent::Thinking(
                    crate::types::ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: false,
                    },
                ));
                Ok(AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial: self.partial.clone(),
                })
            }
            PiMessagesEvent::ThinkingDelta {
                content_index,
                delta,
            } => {
                let index = content_index as usize;
                if let Some(AssistantContent::Thinking(block)) = self.partial.content.get_mut(index)
                {
                    block.thinking.push_str(&delta);
                }
                Ok(AssistantMessageEvent::ThinkingDelta {
                    content_index: index,
                    delta,
                    partial: self.partial.clone(),
                })
            }
            PiMessagesEvent::ThinkingEnd {
                content_index,
                content,
                content_signature,
                redacted,
            } => {
                let index = content_index as usize;
                if let Some(AssistantContent::Thinking(block)) = self.partial.content.get_mut(index)
                {
                    block.thinking = content.clone();
                    block.thinking_signature = content_signature;
                    block.redacted = redacted.unwrap_or(false);
                }
                Ok(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content,
                    partial: self.partial.clone(),
                })
            }
            PiMessagesEvent::ToolcallStart {
                content_index,
                id,
                tool_name,
            } => {
                let index = content_index as usize;
                self.partial
                    .content
                    .push(AssistantContent::ToolCall(ToolCall {
                        id,
                        name: tool_name,
                        arguments: Map::new(),
                        thought_signature: None,
                        namespace: None,
                        arguments_error: None,
                    }));
                self.tool_arguments.insert(index, String::new());
                Ok(AssistantMessageEvent::ToolcallStart {
                    content_index: index,
                    partial: self.partial.clone(),
                })
            }
            PiMessagesEvent::ToolcallDelta {
                content_index,
                delta,
            } => {
                let index = content_index as usize;
                if let Some(raw) = self.tool_arguments.get_mut(&index) {
                    raw.push_str(&delta);
                }
                if let (Some(raw), Some(AssistantContent::ToolCall(block))) = (
                    self.tool_arguments.get(&index),
                    self.partial.content.get_mut(index),
                ) && let Ok(arguments) = parse_tool_call_arguments(raw)
                {
                    block.arguments = arguments;
                }
                Ok(AssistantMessageEvent::ToolcallDelta {
                    content_index: index,
                    delta,
                    partial: self.partial.clone(),
                })
            }
            PiMessagesEvent::ToolcallEnd {
                content_index,
                tool_call,
            } => {
                let index = content_index as usize;
                if let Some(block) = self.partial.content.get_mut(index) {
                    *block = AssistantContent::ToolCall(tool_call.clone());
                }
                self.tool_arguments.remove(&index);
                Ok(AssistantMessageEvent::ToolcallEnd {
                    content_index: index,
                    tool_call,
                    partial: self.partial.clone(),
                })
            }
            PiMessagesEvent::Done {
                reason,
                usage,
                response_id,
                response_model,
                provider_thinking_level,
                rewrite,
            } => {
                self.partial.stop_reason = reason;
                self.partial.usage = parse_usage(&usage);
                self.partial.response_id = response_id;
                if let Some(model_name) = response_model {
                    self.partial.response_model = Some(model_name);
                }
                if let Some(level) = provider_thinking_level {
                    self.partial.provider_thinking_level = Some(level);
                }
                if let Some(rewrite) = rewrite {
                    append_diagnostic(
                        &mut self.partial,
                        json!({
                            "type": "pi_messages_rewrite",
                            "timestamp": now_ms(),
                            "details": rewrite,
                        }),
                    );
                }
                Ok(AssistantMessageEvent::Done {
                    reason,
                    message: self.partial.clone(),
                })
            }
            PiMessagesEvent::Error {
                reason,
                usage,
                error_message,
                response_id,
                response_model,
                provider_thinking_level,
                rewrite,
            } => {
                self.partial.stop_reason = reason;
                self.partial.usage = parse_usage(&usage);
                self.partial.error_message = error_message;
                self.partial.response_id = response_id;
                if let Some(model_name) = response_model {
                    self.partial.response_model = Some(model_name);
                }
                if let Some(level) = provider_thinking_level {
                    self.partial.provider_thinking_level = Some(level);
                }
                if let Some(rewrite) = rewrite {
                    append_diagnostic(
                        &mut self.partial,
                        json!({
                            "type": "pi_messages_rewrite",
                            "timestamp": now_ms(),
                            "details": rewrite,
                        }),
                    );
                }
                Ok(AssistantMessageEvent::Error {
                    reason,
                    error: self.partial.clone(),
                })
            }
        }
    }
}

/// A backend HTTP error with its parsed `error` payload (TS lines 93-160).
#[derive(Debug, Clone)]
struct PiMessagesResponseError {
    message: String,
    code: Option<String>,
    diagnostic_details: JsonValue,
}

fn truncate_diagnostic_string(value: &str) -> String {
    const MAX_LENGTH: usize = 8192;
    if value.chars().count() > MAX_LENGTH {
        let truncated: String = value.chars().take(MAX_LENGTH).collect();
        format!("{truncated}…")
    } else {
        value.to_string()
    }
}

fn create_response_error(
    model: &Model,
    url: &str,
    status: u16,
    status_text: &str,
    body: &str,
) -> PiMessagesResponseError {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let error_object = parsed
        .as_ref()
        .and_then(|value| value.get("error"))
        .filter(|value| value.is_object());
    let message = error_object
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str);
    let code = error_object
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str);
    let suffix = message.unwrap_or(body);
    let code_suffix = code.map_or_else(String::new, |code| format!(" ({code})"));
    let message = format!("{status} {status_text}: {suffix}{code_suffix}");

    let mut details = json!({
        "version": 1,
        "provider": model.provider,
        "model": model.id,
        "url": url,
        "status": status,
        "statusText": status_text,
        "timestampMs": now_ms(),
    });
    if let Some(map) = details.as_object_mut() {
        if let Some(error) = error_object {
            map.insert("error".to_string(), error.clone());
        } else {
            map.insert("body".to_string(), json!(truncate_diagnostic_string(body)));
        }
    }
    PiMessagesResponseError {
        message,
        code: code.map(str::to_string),
        diagnostic_details: details,
    }
}

/// Terminal error event for request/transport failures (TS lines 341-363).
fn create_error_event(
    model: &Model,
    message: &str,
    response_error: Option<&PiMessagesResponseError>,
) -> AssistantMessageEvent {
    let mut error_message = initial_output(model);
    error_message.stop_reason = StopReason::Error;
    error_message.error_message = Some(message.to_string());
    if let Some(response_error) = response_error {
        append_diagnostic(
            &mut error_message,
            json!({
                "type": "pi_messages_response_failure",
                "timestamp": now_ms(),
                "error": {
                    "name": "PiMessagesResponseError",
                    "message": response_error.message,
                    "code": response_error.code,
                },
                "details": response_error.diagnostic_details,
            }),
        );
    }
    AssistantMessageEvent::Error {
        reason: StopReason::Error,
        error: error_message,
    }
}

/// `resolveCacheRetention` (TS lines 365-371): explicit tier wins; otherwise
/// the legacy `PI_CACHE_RETENTION=long` env opt-in is mapped.
fn resolve_cache_retention(options: &StreamOptions) -> Option<CacheRetention> {
    if let Some(retention) = options.cache_retention {
        return Some(retention);
    }
    (std::env::var("PI_CACHE_RETENTION").as_deref() == Ok("long")).then_some(CacheRetention::Long)
}

/// Tool choice on the wire. `ToolChoice` only models `auto`/`none`; a raw
/// `metadata.toolChoice` value overrides it for `required`/named functions.
fn tool_choice(options: &StreamOptions) -> Option<JsonValue> {
    if let Some(raw) = options.metadata.as_ref().and_then(|m| m.get("toolChoice")) {
        return Some(raw.clone());
    }
    match options.tool_choice {
        Some(crate::types::ToolChoice::Auto) => Some(json!("auto")),
        Some(crate::types::ToolChoice::None) => Some(json!("none")),
        None => None,
    }
}

/// `<baseUrl>/messages`, with the `debug=1` query flag (TS lines 388-391).
fn request_url(model: &Model, debug: bool) -> String {
    let base = model.base_url.trim_end_matches('/');
    let mut url = format!("{base}/messages");
    if debug {
        url.push_str("?debug=1");
    }
    url
}

/// Request body `{ model, context, options }` (TS lines 393-408).
#[allow(clippy::too_many_lines)] // mirrors the TS options object field-by-field
fn build_request_body(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
) -> Result<JsonValue, String> {
    let mut request_options = Map::new();
    if let Some(temperature) = options.temperature {
        request_options.insert(
            "temperature".to_string(),
            serde_json::to_value(temperature).map_err(|e| e.to_string())?,
        );
    }
    if let Some(max_tokens) = options.max_tokens {
        request_options.insert("maxTokens".to_string(), json!(max_tokens));
    }
    if let Some(reasoning) = options.reasoning {
        request_options.insert(
            "reasoning".to_string(),
            serde_json::to_value(reasoning).map_err(|e| e.to_string())?,
        );
    }
    if let Some(retention) = resolve_cache_retention(options) {
        let value = match retention {
            CacheRetention::Short => "short",
            CacheRetention::Long => "long",
        };
        request_options.insert("cacheRetention".to_string(), json!(value));
    }
    if let Some(session_id) = &options.session_id {
        request_options.insert("sessionId".to_string(), json!(session_id));
    }
    if let Some(choice) = tool_choice(options) {
        request_options.insert("toolChoice".to_string(), choice);
    }
    // `TranscriptContext` is not `Serialize`; the wire shape is just its
    // messages (TS sends `{ messages, tools? }`).
    Ok(json!({
        "model": model.id,
        "context": { "messages": context.messages },
        "options": request_options,
    }))
}

/// Provider-scoped API key (TS line 383, `getApiKey("API_KEY", provider)`).
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

/// Consume an SSE stream of wire events, returning whether a terminal event
/// was seen. A bare `[DONE]` or EOF is *not* terminal (TS lines 435-444).
async fn consume_sse<S>(
    mut sse: S,
    model: &Model,
    options: &StreamOptions,
    output: &mut AssistantMessage,
    sender: &EventSender,
) -> Result<bool, String>
where
    S: Stream<Item = Result<SseMessage, SseError>> + Unpin,
{
    let mut converter = Converter::new(model);
    let mut terminal = false;
    while let Some(item) = sse.next().await {
        let message = match item {
            Ok(message) => message,
            Err(SseError::Status { status, body }) => return Err(format!("HTTP {status}: {body}")),
            Err(SseError::Http(message)) => return Err(message),
        };
        let data = message.data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let raw: JsonValue = serde_json::from_str(data)
            .map_err(|e| format!("could not parse pi-messages event: {e}"))?;
        apply_stream_event_hook(&options.on_provider_stream_event, raw.clone(), model).await?;
        let event: PiMessagesEvent =
            serde_json::from_value(raw).map_err(|e| format!("invalid pi-messages event: {e}"))?;
        let outcome = converter.apply(event)?;
        *output = converter.partial.clone();
        let is_terminal = AssistantMessageEventStream::is_terminal(&outcome);
        if sender.send(outcome).await.is_err() {
            break;
        }
        if is_terminal {
            terminal = true;
            break;
        }
    }
    Ok(terminal)
}

/// Public stream entry point (TS lines 373-451). Failures are always encoded
/// as an `error` event, never thrown.
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
    if let Err((message, response_error)) = result {
        let _ = sender
            .send(create_error_event(model, &message, response_error.as_ref()))
            .await;
    }
}

type StreamError = (String, Option<PiMessagesResponseError>);

async fn request_and_stream(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
    output: &mut AssistantMessage,
    sender: &EventSender,
) -> Result<(), StreamError> {
    let key = api_key(model, options).ok_or_else(|| {
        (
            format!("No API key provided for provider \"{}\"", model.provider),
            None,
        )
    })?;
    let debug = options
        .metadata
        .as_ref()
        .and_then(|m| m.get("debug"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let url = request_url(model, debug);

    let body = build_request_body(model, context, options).map_err(|e| (e, None))?;
    let body = apply_payload_hook(&options.on_payload, body, model)
        .await
        .map_err(|e| (e, None))?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| (format!("client build failed: {e}"), None))?;
    let mut request = client
        .post(&url)
        .header("authorization", format!("Bearer {key}"))
        .header("accept", "text/event-stream")
        .header("content-type", "application/json")
        .json(&body);
    for (name, value) in &options.headers {
        if let Some(value) = value {
            request = request.header(name, value);
        }
    }
    let response = request
        .send()
        .await
        .map_err(|e| (format!("request failed: {e}"), None))?;

    apply_response_hook(&options.on_response, response_metadata(&response), model)
        .await
        .map_err(|e| (e, None))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let error = create_response_error(
            model,
            &url,
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            &body,
        );
        return Err((error.message.clone(), Some(error)));
    }

    let sse = read_sse_stream(response)
        .await
        .map_err(|e| (e.to_string(), None))?;
    let terminal = consume_sse(sse, model, options, output, sender)
        .await
        .map_err(|e| (e, None))?;
    if !terminal {
        return Err((
            format!("{} stream ended without a terminal event", model.provider),
            None,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        Message, ModelCost, ModelInputModality, SystemMessage, ThinkingLevel, UserContent,
        UserMessage,
    };

    fn test_model() -> Model {
        Model {
            id: "local-model".into(),
            name: "Local Model".into(),
            api: "pi-messages".into(),
            provider: "llama-swap".into(),
            base_url: "http://127.0.0.1:8080/".into(),
            input: vec![ModelInputModality::Text],
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                ..Default::default()
            },
            context_window: 8192,
            max_tokens: 1024,
            ..Model::default()
        }
    }

    fn wire(line: &str) -> PiMessagesEvent {
        serde_json::from_str(line).unwrap()
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

    /// Drive a canned SSE stream through the real `consume_sse` path plus the
    /// terminal check from `request_and_stream`.
    async fn drive(chunks: &[&[u8]]) -> Vec<AssistantMessageEvent> {
        let model = test_model();
        let options = StreamOptions::default();
        let (stream, sender) = AssistantMessageEventStream::channel();
        let mut output = initial_output(&model);
        let messages = sse_messages(chunks);
        let sse = futures::stream::iter(messages.into_iter().map(Ok::<_, SseError>));
        let terminal = consume_sse(sse, &model, &options, &mut output, &sender)
            .await
            .unwrap();
        if !terminal {
            let _ = sender
                .send(create_error_event(
                    &model,
                    &format!("{} stream ended without a terminal event", model.provider),
                    None,
                ))
                .await;
        }
        drop(sender);
        futures::StreamExt::collect(stream).await
    }

    #[test]
    fn request_body_carries_context_and_options() {
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
        let options = StreamOptions {
            temperature: Some(0.4),
            max_tokens: Some(64),
            reasoning: Some(ThinkingLevel::Medium),
            session_id: Some("sess-1".into()),
            tool_choice: Some(crate::types::ToolChoice::None),
            ..StreamOptions::default()
        };
        let body = build_request_body(&model, &context, &options).unwrap();
        assert_eq!(body["model"], "local-model");
        assert_eq!(body["context"]["messages"][1]["content"], "hi");
        assert_eq!(body["options"]["temperature"], 0.4);
        assert_eq!(body["options"]["maxTokens"], 64);
        assert_eq!(body["options"]["reasoning"], "medium");
        assert_eq!(body["options"]["sessionId"], "sess-1");
        assert_eq!(body["options"]["toolChoice"], "none");
        assert_eq!(request_url(&model, false), "http://127.0.0.1:8080/messages");
        assert_eq!(
            request_url(&model, true),
            "http://127.0.0.1:8080/messages?debug=1"
        );
    }

    #[tokio::test]
    async fn canned_text_stream_emits_upstream_sequence() {
        let events = drive(&[
            b"data: {\"type\":\"start\"}\n\n",
            b"data: {\"type\":\"text_start\",\"contentIndex\":0}\n\n",
            b"data: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"Hel\"}\n\ndata: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"lo\"}\n\n",
            b"data: {\"type\":\"text_end\",\"contentIndex\":0,\"content\":\"Hello\",\"contentSignature\":\"sig\"}\n\n",
            b"data: {\"type\":\"done\",\"reason\":\"stop\",\"usage\":{\"input\":11,\"output\":8,\"totalTokens\":19},\"responseId\":\"r1\",\"responseModel\":\"local-model\"}\n\n",
        ])
        .await;
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| match event {
                AssistantMessageEvent::Start { .. } => "start",
                AssistantMessageEvent::TextStart { .. } => "text_start",
                AssistantMessageEvent::TextDelta { .. } => "text_delta",
                AssistantMessageEvent::TextEnd { .. } => "text_end",
                AssistantMessageEvent::Done { .. } => "done",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "start",
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
        assert_eq!(message.content.len(), 1);
        assert_eq!(message.content[0].as_text(), Some("Hello"));
        assert_eq!(message.response_id.as_deref(), Some("r1"));
        assert_eq!(message.usage.input, 11);
        assert_eq!(message.usage.total_tokens, 19);
        let AssistantMessageEvent::TextEnd { partial, .. } = &events[4] else {
            panic!("expected text_end")
        };
        assert_eq!(partial.content[0].as_text(), Some("Hello"));
    }

    #[tokio::test]
    async fn canned_thinking_and_tool_call_stream() {
        let events = drive(&[
            b"data: {\"type\":\"thinking_start\",\"contentIndex\":0}\n\n",
            b"data: {\"type\":\"thinking_delta\",\"contentIndex\":0,\"delta\":\"hm\"}\n\n",
            b"data: {\"type\":\"thinking_end\",\"contentIndex\":0,\"content\":\"hmm\",\"redacted\":true}\n\n",
            b"data: {\"type\":\"toolcall_start\",\"contentIndex\":1,\"id\":\"tc_1\",\"toolName\":\"bash\"}\n\n",
            b"data: {\"type\":\"toolcall_delta\",\"contentIndex\":1,\"delta\":\"{\\\"cmd\\\":\"}\n\n",
            b"data: {\"type\":\"toolcall_delta\",\"contentIndex\":1,\"delta\":\"\\\"ls\\\"}\"}\n\n",
            b"data: {\"type\":\"toolcall_end\",\"contentIndex\":1,\"toolCall\":{\"id\":\"tc_1\",\"name\":\"bash\",\"arguments\":{\"cmd\":\"ls\"}}}\n\n",
            b"data: {\"type\":\"done\",\"reason\":\"toolUse\",\"usage\":{}}\n\n",
        ])
        .await;
        let AssistantMessageEvent::Done { reason, message } = events.last().unwrap() else {
            panic!("expected done")
        };
        assert_eq!(*reason, StopReason::ToolUse);
        assert_eq!(message.content.len(), 2);
        let AssistantContent::Thinking(block) = &message.content[0] else {
            panic!("expected thinking")
        };
        assert_eq!(block.thinking, "hmm");
        assert!(block.redacted);
        let AssistantContent::ToolCall(call) = &message.content[1] else {
            panic!("expected tool call")
        };
        assert_eq!(call.id, "tc_1");
        assert_eq!(call.arguments["cmd"], "ls");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AssistantMessageEvent::ToolcallEnd { .. }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AssistantMessageEvent::ThinkingEnd { .. }))
        );
    }

    #[tokio::test]
    async fn done_sentinel_and_eof_without_terminal_become_error() {
        let events = drive(&[
            b"data: {\"type\":\"start\"}\n\n",
            b"data: {\"type\":\"text_start\",\"contentIndex\":0}\n\n",
            b"data: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"partial\"}\n\n",
            b"data: [DONE]\n\n",
        ])
        .await;
        let AssistantMessageEvent::Error { reason, error } = events.last().unwrap() else {
            panic!("expected terminal error")
        };
        assert_eq!(*reason, StopReason::Error);
        assert_eq!(
            error.error_message.as_deref(),
            Some("llama-swap stream ended without a terminal event")
        );
        // Upstream builds a fresh message for transport errors; no partial
        // content leaks into the error message.
        assert!(error.content.is_empty());
    }

    #[tokio::test]
    async fn wire_error_event_is_forwarded_and_stops_the_stream() {
        let events = drive(&[
            b"data: {\"type\":\"error\",\"reason\":\"aborted\",\"usage\":{\"input\":0,\"output\":0,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":0,\"cost\":{\"input\":0,\"output\":0,\"cacheRead\":0,\"cacheWrite\":0,\"total\":0}},\"errorMessage\":\"cancelled\"}\n\n",
            b"data: {\"type\":\"text_start\",\"contentIndex\":0}\n\n",
        ])
        .await;
        assert_eq!(events.len(), 1);
        let AssistantMessageEvent::Error { reason, error } = &events[0] else {
            panic!("expected error")
        };
        assert_eq!(*reason, StopReason::Aborted);
        assert_eq!(error.error_message.as_deref(), Some("cancelled"));
    }

    #[test]
    fn content_index_is_validated() {
        let mut converter = Converter::new(&test_model());
        assert!(
            converter
                .apply(wire(
                    r#"{"type":"text_delta","contentIndex":0,"delta":"x"}"#
                ))
                .is_err()
        );
        converter
            .apply(wire(r#"{"type":"text_start","contentIndex":0}"#))
            .unwrap();
        assert!(
            converter
                .apply(wire(
                    r#"{"type":"text_delta","contentIndex":-1,"delta":"x"}"#
                ))
                .is_err()
        );
        assert!(
            converter
                .apply(wire(
                    r#"{"type":"text_delta","contentIndex":2,"delta":"x"}"#
                ))
                .is_err()
        );
        converter
            .apply(wire(
                r#"{"type":"text_delta","contentIndex":0,"delta":"x"}"#,
            ))
            .unwrap();
    }

    #[test]
    fn response_error_formats_status_and_code() {
        let error = create_response_error(
            &test_model(),
            "http://127.0.0.1:8080/messages",
            429,
            "Too Many Requests",
            r#"{"error":{"message":"slow down","code":"rate_limit"}}"#,
        );
        assert_eq!(
            error.message,
            "429 Too Many Requests: slow down (rate_limit)"
        );
        assert_eq!(error.code.as_deref(), Some("rate_limit"));
        assert_eq!(error.diagnostic_details["status"], 429);
        let plain = create_response_error(&test_model(), "u", 500, "Server Error", "boom");
        assert_eq!(plain.message, "500 Server Error: boom");
        assert_eq!(plain.diagnostic_details["body"], "boom");
    }

    #[test]
    fn rewrite_impact_is_attached_as_diagnostic() {
        let event = wire(
            r#"{"type":"done","reason":"stop","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"rewrite":{"policyId":"p","policyVersion":2,"changed":true,"tokenCountChange":-1,"messageCountChange":0,"systemPromptChanged":false}}"#,
        );
        let converted = Converter::new(&test_model()).apply(event).unwrap();
        let AssistantMessageEvent::Done { message, .. } = converted else {
            panic!("expected done")
        };
        assert_eq!(message.diagnostics.len(), 1);
        assert_eq!(message.diagnostics[0]["type"], "pi_messages_rewrite");
        assert_eq!(message.diagnostics[0]["details"]["policyId"], "p");
    }
}
