//! `AgentMessage`: the loop's transcript message — LLM messages plus the
//! session-level roles.
//!
//! Ground truth: `packages/agent/src/types.ts` (`AgentMessage = Message |
//! CustomAgentMessages[...]`) and the message payloads the coding agent's
//! session writes (`packages/coding-agent/src/core/agent-session.ts`,
//! `BashExecutionMessage`/`CustomMessage`/`BranchSummaryMessage`/
//! `CompactionSummaryMessage`).
//!
//! The four LLM roles wrap `pi-ai`'s wire types unchanged; the extra roles are
//! app-level: they ride in the transcript and are turned into LLM messages
//! (or dropped) by `ConvertToLlm` at the provider boundary. `Custom` is the
//! Rust stand-in for TS's declaration-merged custom messages: the original
//! `role` string and payload are kept verbatim.
//!
//! Serialization mirrors the session wire format exactly: the `role` tag plus
//! the variant's own fields (camelCase, matching TS key order).

use pi_ai::types::{
    AssistantMessage, JsonObject, JsonValue, SystemMessage, ToolResultMessage, UserContent,
    UserMessage,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A `bashExecution` message (`agent-session.ts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionMessage {
    pub command: String,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_from_context: Option<bool>,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// A `custom` message: app-declared, rendered but not sent to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    /// The app's own message type name (TS's declaration-merged role key).
    pub custom_type: String,
    #[serde(default)]
    pub content: UserContent,
    #[serde(default)]
    pub display: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// A `branchSummary` message; synthesized from session entries on load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryMessage {
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_id: Option<String>,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// A `compactionSummary` message; synthesized from compaction entries on load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryMessage {
    pub summary: String,
    #[serde(default, rename = "tokensBefore")]
    pub tokens_before: f64,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// Transcript message union: the four LLM roles plus the session-level ones.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentMessage {
    User(UserMessage),
    System(SystemMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    BashExecution(BashExecutionMessage),
    Custom(CustomMessage),
    BranchSummary(BranchSummaryMessage),
    CompactionSummary(CompactionSummaryMessage),
    /// A role we do not model (extension-declared), kept verbatim.
    Unknown {
        role: String,
        raw: JsonObject,
    },
}

impl AgentMessage {
    /// The wire `role` string, identical to pi's discriminants.
    pub fn role(&self) -> &str {
        match self {
            AgentMessage::User(_) => "user",
            AgentMessage::System(_) => "system",
            AgentMessage::Assistant(_) => "assistant",
            AgentMessage::ToolResult(_) => "toolResult",
            AgentMessage::BashExecution(_) => "bashExecution",
            AgentMessage::Custom(_) => "custom",
            AgentMessage::BranchSummary(_) => "branchSummary",
            AgentMessage::CompactionSummary(_) => "compactionSummary",
            AgentMessage::Unknown { role, .. } => role,
        }
    }

    /// `_hasConversation`: a session file is only created once a user or
    /// assistant message exists.
    pub fn is_conversation(&self) -> bool {
        matches!(self, AgentMessage::User(_) | AgentMessage::Assistant(_))
    }

    /// The usage this message contributes to context accounting, if any.
    pub fn usage(&self) -> Option<&pi_ai::types::Usage> {
        match self {
            AgentMessage::Assistant(message) => Some(&message.usage),
            AgentMessage::ToolResult(message) => message.usage.as_ref(),
            _ => None,
        }
    }

    /// Lossless wire encoding: the `role` tag plus the variant's fields.
    pub fn to_value(&self) -> JsonValue {
        let (role, value) = match self {
            AgentMessage::User(message) => ("user", serde_json::to_value(message)),
            AgentMessage::System(message) => ("system", serde_json::to_value(message)),
            AgentMessage::Assistant(message) => ("assistant", serde_json::to_value(message)),
            AgentMessage::ToolResult(message) => ("toolResult", serde_json::to_value(message)),
            AgentMessage::BashExecution(message) => {
                ("bashExecution", serde_json::to_value(message))
            }
            AgentMessage::Custom(message) => ("custom", serde_json::to_value(message)),
            AgentMessage::BranchSummary(message) => {
                ("branchSummary", serde_json::to_value(message))
            }
            AgentMessage::CompactionSummary(message) => {
                ("compactionSummary", serde_json::to_value(message))
            }
            AgentMessage::Unknown { role, raw } => {
                (role.as_str(), Ok(JsonValue::Object(raw.clone())))
            }
        };
        match value {
            Ok(JsonValue::Object(fields)) => {
                let mut object = JsonObject::new();
                object.insert("role".to_string(), JsonValue::String(role.to_string()));
                for (key, value) in fields {
                    object.insert(key, value);
                }
                JsonValue::Object(object)
            }
            _ => JsonValue::Null,
        }
    }

    /// Decode a wire message by its `role` tag; unknown roles are preserved.
    pub fn from_value(value: &JsonValue) -> Result<AgentMessage, String> {
        let Some(object) = value.as_object() else {
            return Err("message is not an object".to_string());
        };
        let role = object.get("role").and_then(JsonValue::as_str).unwrap_or("");
        let typed: Result<AgentMessage, serde_json::Error> = match role {
            "user" => serde_json::from_value(value.clone()).map(AgentMessage::User),
            "system" => serde_json::from_value(value.clone()).map(AgentMessage::System),
            "assistant" => serde_json::from_value(value.clone()).map(AgentMessage::Assistant),
            "toolResult" => serde_json::from_value(value.clone()).map(AgentMessage::ToolResult),
            "bashExecution" => {
                serde_json::from_value(value.clone()).map(AgentMessage::BashExecution)
            }
            "custom" => serde_json::from_value(value.clone()).map(AgentMessage::Custom),
            "branchSummary" => {
                serde_json::from_value(value.clone()).map(AgentMessage::BranchSummary)
            }
            "compactionSummary" => {
                serde_json::from_value(value.clone()).map(AgentMessage::CompactionSummary)
            }
            _ => Ok(AgentMessage::Unknown {
                role: role.to_string(),
                raw: object.clone(),
            }),
        };
        typed.map_err(|error| format!("message `{role}` does not match its payload: {error}"))
    }

    /// The LLM message this variant wraps, if it is one of the four LLM roles.
    pub fn to_llm_message(&self) -> Option<pi_ai::types::Message> {
        match self {
            AgentMessage::User(m) => Some(pi_ai::types::Message::User(m.clone())),
            AgentMessage::System(m) => Some(pi_ai::types::Message::System(m.clone())),
            AgentMessage::Assistant(m) => Some(pi_ai::types::Message::Assistant(m.clone())),
            AgentMessage::ToolResult(m) => Some(pi_ai::types::Message::ToolResult(m.clone())),
            _ => None,
        }
    }
}

impl From<pi_ai::types::Message> for AgentMessage {
    fn from(message: pi_ai::types::Message) -> AgentMessage {
        match message {
            pi_ai::types::Message::System(m) => AgentMessage::System(m),
            pi_ai::types::Message::User(m) => AgentMessage::User(m),
            pi_ai::types::Message::Assistant(m) => AgentMessage::Assistant(m),
            pi_ai::types::Message::ToolResult(m) => AgentMessage::ToolResult(m),
        }
    }
}

impl Serialize for AgentMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AgentMessage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        AgentMessage::from_value(&JsonValue::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}
