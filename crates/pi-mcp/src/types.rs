//! MCP wire types: JSON-RPC 2.0 envelope plus the protocol messages the
//! client needs (spec: `modelcontextprotocol.io`, protocol version
//! [`crate::PROTOCOL_VERSION`]).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 2.0 request going to a server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcRequest {
    pub jsonrpc: JsonRpcVersion,
    pub id: i64,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 notification (no id; no reply expected).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcNotification {
    pub jsonrpc: JsonRpcVersion,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 response from a server (result xor error).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcResponse {
    pub jsonrpc: JsonRpcVersion,
    pub id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// `"2.0"` — newtype so serde writes the literal version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonRpcVersion;

impl Serialize for JsonRpcVersion {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text == "2.0" {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported jsonrpc version: {text}"
            )))
        }
    }
}

/// One inbound line: response, request (server-initiated), or notification.
#[derive(Debug, Clone, PartialEq)]
pub enum InboundMessage {
    Response(JsonRpcResponse),
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
    /// Line was not valid JSON-RPC; surfaced for diagnostics.
    Malformed(String),
}

/// Classifies one inbound line (`transport.rs` reader loop).
#[must_use]
pub fn classify_line(line: &str) -> InboundMessage {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return InboundMessage::Malformed(line.to_string());
    };
    let has_id = value.get("id").is_some_and(|id| !id.is_null());
    let has_method = value.get("method").is_some();
    match (has_method, has_id) {
        (true, true) => serde_json::from_value::<JsonRpcRequest>(value).map_or(
            InboundMessage::Malformed(line.to_string()),
            InboundMessage::Request,
        ),
        (true, false) => serde_json::from_value::<JsonRpcNotification>(value).map_or(
            InboundMessage::Malformed(line.to_string()),
            InboundMessage::Notification,
        ),
        (false, true) => serde_json::from_value::<JsonRpcResponse>(value).map_or(
            InboundMessage::Malformed(line.to_string()),
            InboundMessage::Response,
        ),
        (false, false) => InboundMessage::Malformed(line.to_string()),
    }
}

/// `initialize` request params.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: ClientCapabilities,
    #[serde(rename = "clientInfo")]
    pub client_info: Implementation,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ClientCapabilities {}

/// `implementation` info block (name + version).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Implementation {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// `initialize` result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: ServerCapabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: Implementation,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServerCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolsCapability {
    #[serde(
        default,
        rename = "listChanged",
        skip_serializing_if = "Option::is_none"
    )]
    pub list_changed: Option<bool>,
}

/// A tool a server exposes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the arguments.
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// `tools/list` result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolsListResult {
    #[serde(default)]
    pub tools: Vec<McpTool>,
    #[serde(
        default,
        rename = "nextCursor",
        skip_serializing_if = "Option::is_none"
    )]
    pub next_cursor: Option<String>,
}

/// `tools/call` params.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CallToolParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}

/// `tools/call` result: content blocks plus the protocol-level error flag.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

/// Content blocks a tool result may carry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    #[serde(rename = "resource")]
    Resource { resource: Value },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_response_request_notification_and_garbage() {
        assert!(matches!(
            classify_line(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#),
            InboundMessage::Response(_)
        ));
        assert!(matches!(
            classify_line(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#),
            InboundMessage::Request(_)
        ));
        assert!(matches!(
            classify_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            InboundMessage::Notification(_)
        ));
        assert!(matches!(
            classify_line("not json at all"),
            InboundMessage::Malformed(_)
        ));
        // A bare string with no id and no method is malformed, not a
        // notification.
        assert!(matches!(
            classify_line(r#"{"hello":"world"}"#),
            InboundMessage::Malformed(_)
        ));
    }

    #[test]
    fn requests_serialize_with_jsonrpc_2_0() {
        let request = JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: 1,
            method: "tools/list".to_string(),
            params: None,
        };
        let line = serde_json::to_string(&request).expect("serialize");
        assert!(line.contains(r#""jsonrpc":"2.0""#), "{line}");
        assert!(line.contains(r#""method":"tools/list""#), "{line}");
    }

    #[test]
    fn tool_results_decode_pi_relevant_shapes() {
        let result: CallToolResult = serde_json::from_value(
            json!({"content": [{"type": "text", "text": "hi"}], "isError": false}),
        )
        .expect("decode");
        assert_eq!(result.content.len(), 1);
        assert!(!result.is_error);
    }
}
