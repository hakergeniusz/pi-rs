//! Event payloads and results for the E1 dispatch surface.
//!
//! Payload constructors shape the JSON exactly as the upstream event
//! interfaces do (`core/extensions/types.ts`); results are the parsed
//! handler returns the runner's `emit*` methods consume. Payloads and
//! results are JSON end to end (§6.4 of `docs/EXTENSIONS.md`); these types
//! exist so the runner reads typed fields instead of string keys.

use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

/// `{type:"session_start", reason, previousSessionFile?}`.
pub fn session_start(reason: &str) -> Json {
    json!({ "type": "session_start", "reason": reason })
}

/// `{type:"session_shutdown", reason, targetSessionFile?}`.
pub fn session_shutdown(reason: &str) -> Json {
    json!({ "type": "session_shutdown", "reason": reason })
}

/// `{type:"agent_start"}`.
pub fn agent_start() -> Json {
    json!({ "type": "agent_start" })
}

/// `{type:"agent_end", messages}`.
pub fn agent_end(messages: &Json) -> Json {
    json!({ "type": "agent_end", "messages": messages })
}

/// `{type:"tool_call", toolCallId, toolName, input}` — `input` is mutable
/// in place by handlers (`types.ts:1214-1285`).
pub fn tool_call(tool_call_id: &str, tool_name: &str, input: &Json) -> Json {
    json!({ "type": "tool_call", "toolCallId": tool_call_id, "toolName": tool_name, "input": input })
}

/// `{type:"tool_result", toolCallId, toolName, input, content,
/// structuredContent?, isError, usage?, details?}`
/// (`types.ts:1288-1305`).
pub fn tool_result(
    tool_call_id: &str,
    tool_name: &str,
    input: &Json,
    content: &Json,
    is_error: bool,
    details: Option<Json>,
) -> Json {
    let mut event = json!({
        "type": "tool_result",
        "toolCallId": tool_call_id,
        "toolName": tool_name,
        "input": input,
        "content": content,
        "isError": is_error,
    });
    let object = event.as_object_mut().expect("literal object");
    if let Some(details) = details {
        object.insert("details".to_string(), details);
    }
    event
}

/// `{type:"input", text, images?, source, streamingBehavior?}`
/// (`types.ts:1189-1197`).
pub fn input(text: &str, source: &str, streaming_behavior: Option<&str>) -> Json {
    let mut event = json!({ "type": "input", "text": text, "source": source });
    let object = event.as_object_mut().expect("literal object");
    if let Some(behavior) = streaming_behavior {
        object.insert("streamingBehavior".to_string(), json!(behavior));
    }
    event
}

/// `{type:"session_before_switch", reason, targetSessionFile?}`.
pub fn session_before_switch(reason: &str, target_session_file: Option<&str>) -> Json {
    let mut event = json!({ "type": "session_before_switch", "reason": reason });
    if let Some(target) = target_session_file {
        event["targetSessionFile"] = json!(target);
    }
    event
}

/// `{type:"session_before_fork", entryId, position}`.
pub fn session_before_fork(entry_id: &str, position: &str) -> Json {
    json!({ "type": "session_before_fork", "entryId": entry_id, "position": position })
}

/// `{type:"user_bash", command, excludeFromContext, cwd}`
/// (`types.ts:1172-1182`) — fired when the user executes a bash command
/// via the `!`/`!!` prefix.
pub fn user_bash(command: &str, exclude_from_context: bool, cwd: &str) -> Json {
    json!({
        "type": "user_bash",
        "command": command,
        "excludeFromContext": exclude_from_context,
        "cwd": cwd,
    })
}

/// `{type:"ui_prompt_start", reason:"ui_prompt", kind, title?}`
/// (`types.ts:1063-1069`) — fired when the runner starts waiting on a
/// blocking extension UI prompt.
pub fn ui_prompt_start(kind: &str, title: Option<&str>) -> Json {
    ui_prompt_event("ui_prompt_start", kind, title)
}

/// `{type:"ui_prompt_end", reason:"ui_prompt", kind, title?}`
/// (`types.ts:1070-1076`).
pub fn ui_prompt_end(kind: &str, title: Option<&str>) -> Json {
    ui_prompt_event("ui_prompt_end", kind, title)
}

fn ui_prompt_event(event_type: &str, kind: &str, title: Option<&str>) -> Json {
    let mut event = json!({ "type": event_type, "reason": "ui_prompt", "kind": kind });
    if let Some(title) = title {
        event["title"] = json!(title);
    }
    event
}

/// `{type:"project_trust", cwd}` (`types.ts`, `ProjectTrustEvent`).
pub fn project_trust(cwd: &str) -> Json {
    json!({ "type": "project_trust", "cwd": cwd })
}

/// The `before_agent_start` trigger: the user event
/// (`{messageType:"prompt", prompt, images?}`) or a custom message
/// (`{messageType, message}`) — `BeforeAgentStartTrigger`,
/// `runner.ts:169-171`.
pub enum BeforeAgentStartTrigger {
    /// A user prompt.
    User {
        /// The prompt text.
        prompt: String,
        /// Attached images, if any.
        images: Option<Json>,
    },
    /// A custom message enqueued by an extension.
    Custom {
        /// The custom message type.
        message_type: String,
        /// The message payload.
        message: Json,
    },
}

impl BeforeAgentStartTrigger {
    /// The event fields this trigger contributes.
    pub fn to_json(&self) -> Json {
        match self {
            Self::User { prompt, images } => {
                let mut event = json!({ "messageType": "prompt", "prompt": prompt });
                if let Some(images) = images {
                    event["images"] = images.clone();
                }
                event
            }
            Self::Custom {
                message_type,
                message,
            } => {
                json!({ "messageType": message_type, "message": message })
            }
        }
    }
}

/// One handler's `{message?, systemPrompt?}` result
/// (`BeforeAgentStartEventResult`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BeforeAgentStartResult {
    /// A message to inject into the run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Json>,
    /// Replaces the complete system prompt for this turn.
    #[serde(rename = "systemPrompt", skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

/// The combined `before_agent_start` outcome
/// (`BeforeAgentStartCombinedResult`, `runner.ts:174-177`).
#[derive(Debug, Clone, Default)]
pub struct BeforeAgentStartCombined {
    /// Messages from every handler, in dispatch order.
    pub messages: Vec<Json>,
    /// The system-prompt options after the `forceSystemPrompt` chain.
    pub system_prompt_options: Json,
}

/// One `tool_call` handler's `{block?, reason?, terminate?}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolCallResult {
    /// Block tool execution.
    #[serde(default)]
    pub block: bool,
    /// Why the call was blocked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Hint that the agent should stop after the current tool batch.
    #[serde(default)]
    pub terminate: bool,
}

/// The outcome of the `input` transform chain (`InputEventResult`).
#[derive(Debug, Clone)]
pub enum InputResult {
    /// No handler changed the input.
    Continue,
    /// A handler transformed the text and/or images.
    Transform {
        /// The transformed text.
        text: String,
        /// The transformed images, if a handler supplied any.
        images: Option<Json>,
    },
    /// A handler consumed the input.
    Handled {
        /// The full handler result.
        result: Json,
    },
}

/// The `session_before_*` cancel result (`{cancel?}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionBeforeResult {
    /// Cancel the session operation.
    #[serde(default)]
    pub cancel: bool,
}

/// The `project_trust` decision (`ProjectTrustEventResult`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectTrustResult {
    /// `"yes"`, `"no"`, or `"undecided"` (undecided falls through).
    pub trusted: String,
    /// Remember the decision.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remember: bool,
}

/// The boundary dispatch outcome (`BoundaryDispatchResult`,
/// `runner.ts:229-234`).
#[derive(Debug, Clone)]
pub struct BoundaryDispatchResult {
    /// The accumulated entry drafts.
    pub entries: Json,
    /// Whether the agent should continue.
    pub continue_run: bool,
    /// The last valid context preview.
    pub context: Json,
    /// False when a preview rebuild failed: everything is discarded.
    pub valid: bool,
}

/// The `tool_result` middleware patch one handler returned
/// (`ToolResultEventResult`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolResultPatch {
    /// Replacement content.
    #[serde(default)]
    pub content: Option<Json>,
    /// Replacement details.
    #[serde(default)]
    pub details: Option<Json>,
    /// Replacement structured content.
    #[serde(rename = "structuredContent", default)]
    pub structured_content: Option<Json>,
    /// Replacement error flag.
    #[serde(rename = "isError", default)]
    pub is_error: Option<bool>,
    /// Replacement usage.
    #[serde(default)]
    pub usage: Option<Json>,
}

/// The session result slot the engine fills (`{done, error?, value?}`).
#[derive(Debug, Clone, Deserialize)]
pub struct SessionOutcome {
    /// Whether the handler settled.
    pub done: bool,
    /// The handler error, if it threw.
    #[serde(default)]
    pub error: Option<HandlerErrorJson>,
    /// The handler result, if it returned one.
    #[serde(default)]
    pub value: Option<Json>,
}

/// `{message, stack}` as the glue reports handler errors.
#[derive(Debug, Clone, Deserialize)]
pub struct HandlerErrorJson {
    /// The error message.
    pub message: String,
    /// The error stack, when the throw was an `Error`.
    pub stack: Option<String>,
}

/// A bind-time provider-registration failure (the `emitError` report at
/// the bindCore flush, `runner.ts:470-521`): the extension loaded, the
/// registration did not apply.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderBindError {
    /// The extension that requested the registration.
    pub extension_path: String,
    /// `"register_provider"` or `"register_virtual_model"` (upstream's
    /// emitError event names; native registrations report as
    /// `"register_provider"`).
    pub event: String,
    /// The validation message.
    pub error: String,
}

/// Parses one session result slot.
pub fn parse_session_outcome(raw: &str) -> Result<SessionOutcome, serde_json::Error> {
    serde_json::from_str(raw)
}
