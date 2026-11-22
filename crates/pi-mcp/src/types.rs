//! MCP wire types: JSON-RPC 2.0 envelope plus the protocol messages the
//! client needs (spec: `modelcontextprotocol.io`, protocol version
//! [`crate::PROTOCOL_VERSION`]).
//!
//! Extension points beyond pi's immediate needs — `_meta` and block
//! `annotations` on tool results, and the `*` tool-name patterns of
//! `packages/coding-agent/src/core/mcp-servers.ts:17-34` — are kept verbatim
//! so the host can forward them (upstream `extensions/mcp/tools.ts:228-232`,
//! `:237-273`). Preserving them here is what lets `convertMcpResult` return
//! the whole `CallToolResult` minus the top-level `_meta`.

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

/// A JSON-RPC error object: numeric `code`, human-readable `message`, optional
/// `data`.
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

/// JSON-RPC error codes (`JSON_RPC_ERROR_CODES`, upstream
/// `protocol/jsonrpc.ts:37-43`).
pub mod json_rpc_error_codes {
    /// `-32700`.
    pub const PARSE_ERROR: i64 = -32700;
    /// `-32600`.
    pub const INVALID_REQUEST: i64 = -32600;
    /// `-32601`.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// `-32602`.
    pub const INVALID_PARAMS: i64 = -32602;
    /// `-32603`.
    pub const INTERNAL_ERROR: i64 = -32603;
}

/// Classifies one parsed inbound value (`isJsonRpcRequest` /
/// `isJsonRpcNotification` / `isJsonRpcResponse`, upstream
/// `protocol/jsonrpc.ts:93-108`): a message carries `method` with (`id` =
/// request) or without (notification) an `id`, or `id` with `result` xor a
/// well-formed `error` (response).
#[must_use]
pub fn classify_value(value: &Value, raw: &str) -> InboundMessage {
    let has_id = value.get("id").is_some_and(|id| !id.is_null());
    let has_method = value.get("method").is_some();
    match (has_method, has_id) {
        (true, true) => serde_json::from_value::<JsonRpcRequest>(value.clone()).map_or(
            InboundMessage::Malformed(raw.to_string()),
            InboundMessage::Request,
        ),
        (true, false) => serde_json::from_value::<JsonRpcNotification>(value.clone()).map_or(
            InboundMessage::Malformed(raw.to_string()),
            InboundMessage::Notification,
        ),
        (false, true) => serde_json::from_value::<JsonRpcResponse>(value.clone()).map_or(
            InboundMessage::Malformed(raw.to_string()),
            InboundMessage::Response,
        ),
        (false, false) => InboundMessage::Malformed(raw.to_string()),
    }
}

/// Classifies one inbound line (`transport.rs` reader loop).
#[must_use]
pub fn classify_line(line: &str) -> InboundMessage {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return InboundMessage::Malformed(line.to_string());
    };
    classify_value(&value, line)
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

/// The capabilities the client declares in `initialize`. The port declares
/// none.
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
    /// Server instructions describing its tools as a group
    /// (`client.ts` surfaces it as `instructions`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// The capabilities the server reports in `initialize`'s result.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServerCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Value>,
}

/// The server's `tools` capability: whether it offers tools and reports
/// `listChanged`.
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
    /// Human-facing title (`Tool.title`), the description fallback for the
    /// tool bridge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// MCP annotations (`readOnlyHint`, `title`, …) kept as raw JSON; the
    /// bridge forwards the boolean hints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
    /// JSON Schema for the arguments.
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    /// JSON Schema for `structuredContent` (`Tool.outputSchema`).
    #[serde(
        default,
        rename = "outputSchema",
        skip_serializing_if = "Option::is_none"
    )]
    pub output_schema: Option<Value>,
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
    /// Structured output (`CallToolResult.structuredContent`); any JSON.
    #[serde(
        default,
        rename = "structuredContent",
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_content: Option<Value>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
    /// Protocol metadata (`CallToolResult._meta`). `convertMcpResult` drops
    /// this from the script result (`tools.ts:228`), but block-level `_meta`
    /// survives because it is part of each [`ContentBlock`].
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// Content blocks a tool result may carry.
///
/// Every variant also keeps the spec's optional `_meta` and `annotations`
/// fields so a decoded result serializes back exactly as the server sent it
/// (upstream `tools.ts:228-232` forwards the whole result to scripts).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Value>,
    },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Value>,
    },
    #[serde(rename = "audio")]
    Audio {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Value>,
    },
    #[serde(rename = "resource_link")]
    ResourceLink {
        uri: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, rename = "mimeType", skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<f64>,
        #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Value>,
    },
    #[serde(rename = "resource")]
    Resource {
        resource: Value,
        #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Value>,
    },
}

/// An MCP tool's input schema in the shape LLM tool APIs accept
/// (`input-schema.ts:7-13`): tool parameters must be objects, servers may
/// omit `type`, and some providers reject object schemas without
/// `properties`. Other keywords are kept.
#[must_use]
pub fn normalize_mcp_input_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(fields) => {
            let mut out = fields.clone();
            out.entry("type")
                .or_insert_with(|| Value::String("object".to_string()));
            out.entry("properties")
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            Value::Object(out)
        }
        _ => json_schema_object(),
    }
}

fn json_schema_object() -> Value {
    serde_json::json!({ "type": "object", "properties": {} })
}

/// Whether `name` matches an MCP `*`-pattern (`toolPatternRegExp`,
/// `packages/coding-agent/src/core/mcp-servers.ts:17-34`, used by
/// `createToolNameMatcher` and `getMcpToolExposure`).
///
/// Upstream builds `^` + each `*`-separated literal (regex-escaped) joined
/// by `.*` + `$`. This is the same language: literal parts are matched
/// literally (so `.`, `+`, `?`, … never act as regex operators), `*` matches
/// any run of characters except JavaScript line terminators (a regex `.`
/// does not cross `\n`, `\r`, U+2028 or U+2029), and the whole name must be
/// consumed. Matching is by backtracking, so a later literal is not forced
/// to use the earliest possible position of an earlier one.
#[must_use]
pub fn tool_name_pattern_matches(pattern: &str, name: &str) -> bool {
    let parts: Vec<Vec<char>> = pattern
        .split('*')
        .map(|part| part.chars().collect())
        .collect();
    let chars: Vec<char> = name.chars().collect();
    match_parts(&parts, 0, &chars, 0)
}

/// Backtracking core of [`tool_name_pattern_matches`]: `parts[index]` is the
/// literal that must follow the `.*` begun at `pos`. `index == 0` is pinned
/// to `pos` by the regex's leading `^`; the final part must end at the name's
/// end by the trailing `$`.
fn match_parts(parts: &[Vec<char>], index: usize, chars: &[char], pos: usize) -> bool {
    let Some(part) = parts.get(index) else {
        return pos == chars.len();
    };
    let last = index + 1 == parts.len();
    let mut start = pos;
    loop {
        if index == 0 && start != pos {
            // The leading `^` pins the first literal to `pos`.
            return false;
        }
        if let Some(end) = literal_at(part, chars, start)
            && (!last || end == chars.len())
            && match_parts(parts, index + 1, chars, end)
        {
            return true;
        }
        // A regex `.` stops at a JS line terminator, so the wildcard run
        // cannot advance past one either.
        if start == chars.len() || matches!(chars[start], '\n' | '\r' | '\u{2028}' | '\u{2029}') {
            return false;
        }
        start += 1;
    }
}

/// `Some(end)` when `part` occurs at `chars[start..]`, compared character by
/// character. Literal matching is what upstream's regex escaping achieves.
fn literal_at(part: &[char], chars: &[char], start: usize) -> Option<usize> {
    if part.len() > chars.len().saturating_sub(start) {
        return None;
    }
    (part.iter().enumerate())
        .all(|(offset, expected)| chars[start + offset] == *expected)
        .then_some(start + part.len())
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

    #[test]
    fn tool_results_keep_block_meta_and_annotations() {
        let result: CallToolResult = serde_json::from_value(json!({
            "_meta": {"progressToken": 7},
            "content": [
                {"type": "text", "text": "hi",
                 "_meta": {"trace": "abc"},
                 "annotations": {"audience": ["assistant"], "priority": 0.9}},
                {"type": "image", "data": "AAAA", "mimeType": "image/png",
                 "annotations": {"readOnlyHint": true}},
            ],
            "isError": false,
        }))
        .expect("decode");
        assert_eq!(result.meta, Some(json!({"progressToken": 7})));
        let ContentBlock::Text {
            meta, annotations, ..
        } = &result.content[0]
        else {
            panic!("text block")
        };
        assert_eq!(*meta, Some(json!({"trace": "abc"})));
        assert_eq!(
            *annotations,
            Some(json!({"audience": ["assistant"], "priority": 0.9}))
        );
        let ContentBlock::Image { annotations, .. } = &result.content[1] else {
            panic!("image block")
        };
        assert_eq!(*annotations, Some(json!({"readOnlyHint": true})));
        // Serializing the blocks round-trips the extension fields; the
        // top-level `_meta` stays on the result for the caller to drop.
        let blocks = serde_json::to_value(&result.content).expect("serialize");
        assert_eq!(blocks[0]["_meta"], json!({"trace": "abc"}));
        assert_eq!(blocks[1]["annotations"], json!({"readOnlyHint": true}));
        let full = serde_json::to_value(&result).expect("serialize result");
        assert_eq!(full["_meta"], json!({"progressToken": 7}));
    }

    #[test]
    fn tool_name_patterns_match_upstream_glob_semantics() {
        // Anchored at both ends, `*` crosses arbitrary substrings.
        assert!(tool_name_pattern_matches("foo*", "foobar"));
        assert!(tool_name_pattern_matches("*bar", "foobar"));
        assert!(tool_name_pattern_matches("foo*bar", "foobazbar"));
        assert!(!tool_name_pattern_matches("foo*bar", "foobarx"));
        assert!(!tool_name_pattern_matches("foo*bar", "xfoobar"));
        // An empty pattern part (leading, trailing, doubled `*`).
        assert!(tool_name_pattern_matches("*", "anything"));
        assert!(tool_name_pattern_matches("**", "anything"));
        assert!(tool_name_pattern_matches("*foo*", "afoo b"));
        // Literal regex operators are escaped upstream, so they match as
        // ordinary characters here rather than as regex syntax.
        assert!(tool_name_pattern_matches("a.b", "a.b"));
        assert!(!tool_name_pattern_matches("a.b", "axb"));
        assert!(tool_name_pattern_matches("a+b*c", "a+bXXc"));
        assert!(!tool_name_pattern_matches("a+b*c", "abXXc"));
        assert!(tool_name_pattern_matches("a[b]", "a[b]"));
        assert!(!tool_name_pattern_matches("a[b]", "ab"));
        // Backtracking: an earlier literal need not take the earliest slot.
        assert!(tool_name_pattern_matches("a*ba*b", "abab"));
        assert!(tool_name_pattern_matches("aba*aba", "abaXaba"));
        assert!(!tool_name_pattern_matches("aba*aba", "ababa"));
        // A regex `.` does not cross JS line terminators.
        assert!(!tool_name_pattern_matches("a*b", "a\nb"));
        assert!(!tool_name_pattern_matches("a*b", "a\u{2028}b"));
        assert!(tool_name_pattern_matches("a*b", "a\tb"));
        // Non-star patterns still behave as exact matches.
        assert!(tool_name_pattern_matches("echo", "echo"));
        assert!(!tool_name_pattern_matches("echo", "echo2"));
    }
}
