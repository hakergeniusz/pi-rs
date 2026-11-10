//! Serde for the agent's own value types, in pi's wire shapes.
//!
//! Ground truth: `packages/agent/src/types.ts` (`AgentToolResult`) and
//! `packages/ai/src/types.ts` (`AssistantMessageEvent`).
//!
//! `pi-ai`'s types are serde-derived in pi's wire shapes (`snake_case` event
//! discriminants, `camelCase` fields, pi's key order), so this module only adds
//! the entry points hosts need and keeps wire-shape regression tests.

use pi_ai::types::AssistantMessageEvent;
use serde_json::Value;

/// An `AssistantMessageEvent` in pi's discriminated JSON shape (`type` +
/// camelCase fields; streaming events carry `partial`, `done`/`error` do not).
pub fn assistant_message_event(event: &AssistantMessageEvent) -> Value {
    serde_json::to_value(event).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::empty_assistant_message;
    use pi_ai::types::{ContentBlock, StopReason, TextContent, Usage};

    #[test]
    fn a_tool_result_keeps_pis_field_names_and_omits_unset_fields() {
        let result = super::super::AgentToolResult {
            content: vec![ContentBlock::Text(TextContent {
                text: "ok".into(),
                text_signature: None,
            })],
            details: serde_json::json!({"rows": 2}),
            structured_content: None,
            usage: None,
            is_error: false,
            terminate: false,
        };
        let value = serde_json::to_value(&result).expect("tool result serialises");
        assert_eq!(
            value,
            serde_json::json!({
                "content": [{"type": "text", "text": "ok"}],
                "details": {"rows": 2},
            })
        );
        let keys: Vec<&String> = value.as_object().expect("object").keys().collect();
        assert_eq!(
            keys,
            ["content", "details"],
            "pi's key order is content, details"
        );
    }

    #[test]
    fn a_failed_tool_result_carries_the_optional_fields() {
        let result = super::super::AgentToolResult {
            content: Vec::new(),
            details: Value::Null,
            structured_content: Some(serde_json::json!({"code": 7})),
            usage: Some(Usage::default()),
            is_error: true,
            terminate: true,
        };
        let value = serde_json::to_value(&result).expect("tool result serialises");
        assert_eq!(value["structuredContent"], serde_json::json!({"code": 7}));
        assert_eq!(value["isError"], true);
        assert_eq!(value["terminate"], true);
        assert!(value.get("usage").is_some(), "usage is written when set");
    }

    #[test]
    fn an_assistant_event_is_pis_discriminated_shape() {
        let message = empty_assistant_message();
        let event = AssistantMessageEvent::TextDelta {
            content_index: 2,
            delta: "hi".to_string(),
            partial: message,
        };
        let value = assistant_message_event(&event);
        assert_eq!(value["type"], "text_delta");
        assert_eq!(value["contentIndex"], 2);
        assert_eq!(value["delta"], "hi");
        assert!(
            value["partial"].is_object(),
            "the extension event keeps the partial"
        );
    }

    #[test]
    fn done_and_error_use_pis_message_fields() {
        let message = empty_assistant_message();
        let done = assistant_message_event(&AssistantMessageEvent::Done {
            reason: StopReason::ToolUse,
            message: message.clone(),
        });
        assert_eq!(done["type"], "done");
        assert_eq!(done["reason"], "toolUse");
        assert!(done["message"].is_object());
        assert!(done.get("partial").is_none());

        let failed = assistant_message_event(&AssistantMessageEvent::Error {
            reason: StopReason::Error,
            error: message,
        });
        assert_eq!(failed["type"], "error");
        assert!(failed["error"].is_object());
    }
}
