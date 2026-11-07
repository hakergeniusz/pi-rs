//! Core wire types of pi-ai: messages, content blocks, usage, models, events.
//!
//! Rust port of the type surface in TS `packages/ai/src/types.ts`. Serde field
//! names mirror the TypeScript wire format (camelCase, `"type"`/`"role"` tags)
//! so session JSONL and RPC payloads stay compatible with the TS implementation.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// JSON value (mirrors TS `JsonValue`).
pub type JsonValue = serde_json::Value;
/// JSON object (mirrors TS `JsonObject`).
pub type JsonObject = serde_json::Map<String, JsonValue>;

/// Current unix time in milliseconds (TS uses `Date.now()`).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Unified assistant stop reasons (TS `StopReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Pending,
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
    Deferred,
}

/// pi thinking levels accepted by `streamSimple` (TS `ThinkingLevel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThinkingLevel {
    Minimal,
    Off,
    Low,
    Medium,
    High,
}

/// Model thinking levels recorded on assistant messages (TS `ModelThinkingLevel`).
pub type ModelThinkingLevel = ThinkingLevel;

/// Known chat API identifiers (TS `KnownApi`). Custom APIs are free-form strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KnownApi {
    AnthropicMessages,
    OpenAiCompletions,
    OpenAiResponses,
    AzureOpenAiResponses,
    OpenAiCodexResponses,
    GoogleGenerativeAi,
    GoogleVertex,
    MistralConversations,
    BedrockConverseStream,
    PiMessages,
}

impl KnownApi {
    /// Wire name (TS string literal).
    pub fn as_str(self) -> &'static str {
        match self {
            KnownApi::AnthropicMessages => "anthropic-messages",
            KnownApi::OpenAiCompletions => "openai-completions",
            KnownApi::OpenAiResponses => "openai-responses",
            KnownApi::AzureOpenAiResponses => "azure-openai-responses",
            KnownApi::OpenAiCodexResponses => "openai-codex-responses",
            KnownApi::GoogleGenerativeAi => "google-generative-ai",
            KnownApi::GoogleVertex => "google-vertex",
            KnownApi::MistralConversations => "mistral-conversations",
            KnownApi::BedrockConverseStream => "bedrock-converse-stream",
            KnownApi::PiMessages => "pi-messages",
        }
    }
}

/// Text content block (TS `TextContent`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
    pub text: String,
    /// e.g. for `OpenAI` responses, message metadata (legacy id string or `TextSignatureV1` JSON)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<String>,
}

/// Thinking content block (TS `ThinkingContent`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
    pub thinking: String,
    /// Provider-specific opaque or serialized reasoning replay data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    /// True when the thinking content was redacted by safety filters.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
}

/// Image content block; `data` is base64 (TS `ImageContent`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    pub data: String,
    /// e.g. "image/jpeg", "image/png"
    pub mime_type: String,
}

/// Tool call content block (TS `ToolCall`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: JsonObject,
    /// Google-specific: opaque signature for reusing thought context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
    /// `OpenAI` Responses namespace for namespaced/dynamic tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Set when the model's arguments text was not valid JSON; `arguments` is then `{}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_error: Option<String>,
}

/// Content blocks appearing in user and tool-result messages (TS `TextContent | ImageContent`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum InputContent {
    Text(TextContent),
    Image(ImageContent),
}

/// Content blocks appearing in assistant messages
/// (TS `TextContent | ThinkingContent | ToolCall`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AssistantContent {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCall),
}

/// Any content block (loose union, mirroring all TS block types).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    Text(TextContent),
    Thinking(ThinkingContent),
    Image(ImageContent),
    ToolCall(ToolCall),
}

impl From<InputContent> for ContentBlock {
    fn from(c: InputContent) -> Self {
        match c {
            InputContent::Text(b) => ContentBlock::Text(b),
            InputContent::Image(b) => ContentBlock::Image(b),
        }
    }
}

impl From<AssistantContent> for ContentBlock {
    fn from(c: AssistantContent) -> Self {
        match c {
            AssistantContent::Text(b) => ContentBlock::Text(b),
            AssistantContent::Thinking(b) => ContentBlock::Thinking(b),
            AssistantContent::ToolCall(b) => ContentBlock::ToolCall(b),
        }
    }
}

/// Token usage with catalog-priced cost breakdown (TS `Usage`).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Subset of `cache_write` written with 1h retention. Only Anthropic reports this split.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<u64>,
    /// Reasoning/thinking tokens when the provider reports them (subset of `output`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    #[serde(default)]
    pub total_tokens: u64,
    pub cost: UsageCost,
}

/// Catalog-priced cost breakdown (TS `Usage["cost"]`).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    /// Provider-reported billed cost when supported, otherwise the catalog estimate.
    pub total: f64,
    /// True when `total` is the billed cost reported by the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_from_provider: Option<bool>,
}

/// A tool the model may call; `parameters` is its JSON Schema (TS `Tool`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: JsonValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constrained_sampling: Option<JsonValue>,
}

/// Reference to a tool being removed mid-transcript (TS `ToolReference`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolReference {
    pub name: String,
}

/// Prompt cache retention tiers (TS `CacheRetention`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CacheRetention {
    Short,
    Long,
}

/// Model cost per million tokens (TS `ModelCost`).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
    #[serde(default)]
    pub cache_write_1h: Option<f64>,
}

/// Input modalities a model accepts (TS `ModelInputModality`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ModelInputModality {
    Text,
    Image,
    Pdf,
    Audio,
    Video,
}

/// Provider input limits (TS `ModelInputLimits`); all fields optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInputLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_images_per_request: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_image_size_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pdf_size_mb: Option<u64>,
}

/// Prompt cache lifetimes per retention tier (TS `ModelPromptCache`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPromptCache {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long: Option<u64>,
}

/// A chat model from the catalog (TS `Model`; image/classifier variants omitted for now).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
    /// API identifier: a `KnownApi` wire name or a custom string.
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub input: Vec<ModelInputModality>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<ModelInputLimits>,
    #[serde(default)]
    pub cost: ModelCost,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// Chat is the default model type; absent for chat models on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    pub reasoning: bool,
    /// Maps pi thinking levels to provider/model-specific values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<JsonObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<ModelPromptCache>,
    pub context_window: u64,
    pub max_tokens: u64,
    /// Default sampling parameters for this model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<JsonObject>,
    /// Sampling parameter overrides selected by the effective thinking level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params_by_thinking_level: Option<JsonObject>,
    /// Compatibility overrides for OpenAI-compatible APIs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<JsonObject>,
}

/// Handle for providers with server-side continuation (TS `DeferredHandle`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredHandle {
    pub provider: String,
    pub model_id: String,
    pub api: String,
    /// Provider token, such as a response id or batch id plus row id.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<JsonValue>,
}

/// System instructions and tool declarations at one point in the transcript
/// (TS `SystemMessage`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemMessage {
    /// On the leading message this is the base prompt; later, additional instructions.
    pub content: StringOrTextBlocks,
    /// Named, ordered prompt sections rendered after `content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<BTreeMap<String, Option<String>>>,
    /// Complete definitions of tools that become available at this point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_added: Option<Vec<Tool>>,
    /// Tools that stop being available at this point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_removed: Option<Vec<ToolReference>>,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// `string | TextContent[]` (TS `SystemMessage["content"]`, `UserMessage["content"]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StringOrTextBlocks {
    Text(String),
    Blocks(Vec<TextContent>),
}

/// User message (TS `UserMessage`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub content: UserContent,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// `string | (TextContent | ImageContent)[]` (TS `UserMessage["content"]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<InputContent>),
}

/// Assistant message (TS `AssistantMessage`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<AssistantContent>,
    pub api: String,
    pub provider: String,
    pub model: String,
    /// Concrete model reported by the provider, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    /// Provider-specific response/message identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Exact provider-native effort level used for this response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_thinking_level: Option<String>,
    /// Pi thinking level the agent loop requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ModelThinkingLevel>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<JsonValue>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredHandle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_stop_reason: Option<String>,
    /// Provider indication of whether the model explicitly ended its turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<bool>,
    /// Unix timestamp in milliseconds when the request started.
    pub timestamp: i64,
    /// Monotonic milliseconds from `timestamp` until the response ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// Record of a nested tool call (TS `NestedToolCallRecord`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolCallRecord {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<JsonObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_bytes: Option<u64>,
    pub status: NestedToolCallStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Nested tool call status (TS `NestedToolCallRecord["status"]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NestedToolCallStatus {
    Ok,
    Error,
    /// The call was still running when the calling tool finished.
    Unfinished,
}

/// Bounded record of nested calls a tool made (TS `NestedToolCalls`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolCalls {
    pub calls: Vec<NestedToolCallRecord>,
    /// False when calls were dropped, arguments omitted, or calls had not finished.
    pub complete: bool,
}

/// Tool result message (TS `ToolResultMessage`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<InputContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nested_calls: Option<NestedToolCalls>,
    pub is_error: bool,
    /// Unix timestamp in milliseconds when the result was created.
    pub timestamp: i64,
    /// Monotonic milliseconds the tool's execution took.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// Transcript message (TS `Message`); tagged by `role`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    System(SystemMessage),
    User(UserMessage),
    Assistant(AssistantMessage),
    #[serde(rename = "toolResult")]
    ToolResult(ToolResultMessage),
}

/// Streaming event protocol (TS `AssistantMessageEvent`).
///
/// Successful streams emit `Start` before partial updates and terminate with
/// `Done`. Failures terminate with `Error`, carrying an assistant message with
/// stop reason `error`/`aborted`. `partial` is the live response-so-far.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AssistantMessageEvent {
    Start {
        partial: AssistantMessage,
    },
    TextStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    TextDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    TextEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    ThinkingStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    ToolcallStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    ToolcallDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    ToolcallEnd {
        content_index: usize,
        tool_call: ToolCall,
        partial: AssistantMessage,
    },
    /// Successful termination; `reason` is stop/length/toolUse/deferred.
    Done {
        reason: StopReason,
        message: AssistantMessage,
    },
    /// Failure termination; `error` carries stop reason aborted/error and a message.
    Error {
        reason: StopReason,
        error: AssistantMessage,
    },
}

/// Request context accepted by stream entry points (TS `Context`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Context {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
}

/// Normalized request context passed to providers (TS `TranscriptContext`):
/// system prompt and tools live in the leading system message.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TranscriptContext {
    pub messages: Vec<Message>,
}

/// Streaming options shared by all providers (subset of TS `StreamOptions`
/// needed for P1; provider-specific extras extend via `extra`).
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    pub api_key: Option<String>,
    pub temperature: Option<f64>,
    /// Arbitrary sampling parameters merged into the request body as-is.
    pub sampling_params: Option<JsonObject>,
    pub max_tokens: Option<u64>,
    pub headers: BTreeMap<String, Option<String>>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub disable_continuation: bool,
    /// Thinking level for simple streams (TS `SimpleStreamOptions.reasoning`).
    pub reasoning: Option<ThinkingLevel>,
    /// Provider-neutral tool selection (TS `SimpleStreamOptions.toolChoice`).
    pub tool_choice: Option<ToolChoice>,
    pub metadata: Option<JsonObject>,
}

/// Provider-neutral tool selection (TS `ToolChoice`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    None,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wire-format compatibility: a TS-shaped message JSONL line deserializes.
    #[test]
    fn parses_ts_session_message() {
        let user = r#"{"role":"user","content":"hello","timestamp":1730000000000}"#;
        let msg: Message = serde_json::from_str(user).unwrap();
        assert_eq!(
            msg,
            Message::User(UserMessage {
                content: UserContent::Text("hello".into()),
                timestamp: 1_730_000_000_000
            })
        );

        let assistant = r#"{
            "role":"assistant",
            "content":[{"type":"text","text":"hi"},{"type":"toolCall","id":"t1","name":"bash","arguments":{"cmd":"ls"}}],
            "api":"anthropic-messages","provider":"anthropic","model":"claude-x",
            "usage":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0,
                     "cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},
            "stopReason":"toolUse","timestamp":1730000000001
        }"#;
        let msg: Message = serde_json::from_str(assistant).unwrap();
        let Message::Assistant(a) = msg else {
            panic!("expected assistant")
        };
        assert_eq!(a.stop_reason, StopReason::ToolUse);
        assert_eq!(a.content.len(), 2);
        assert_eq!(a.usage.output, 2);
    }

    /// Round-trip: serialized fields use TS camelCase names.
    #[test]
    fn serializes_camel_case() {
        let msg = Message::ToolResult(ToolResultMessage {
            tool_call_id: "t1".into(),
            tool_name: "bash".into(),
            content: vec![InputContent::Text(TextContent {
                text: "ok".into(),
                text_signature: None,
            })],
            details: None,
            usage: None,
            nested_calls: None,
            is_error: false,
            timestamp: 1,
            duration_ms: Some(12),
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"toolCallId\":\"t1\""), "{json}");
        assert!(json.contains("\"isError\":false"), "{json}");
        assert!(json.contains("\"durationMs\":12"), "{json}");
    }

    /// Event protocol tag names match TS.
    #[test]
    fn event_tag_names() {
        let e = AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "x".into(),
            partial: minimal_assistant(),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.starts_with("{\"type\":\"textDelta\""), "{json}");
    }

    fn minimal_assistant() -> AssistantMessage {
        AssistantMessage {
            content: vec![],
            api: "openai-completions".into(),
            provider: "test".into(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: vec![],
            usage: Usage::default(),
            stop_reason: StopReason::Pending,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: now_ms(),
            duration_ms: None,
        }
    }
}
