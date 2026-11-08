//! Shared provider-adapter helpers: message assembly, finalization, abort.

use crate::cost::calculate_cost;
use crate::types::{AssistantMessage, Message, Model, StopReason, SystemMessage, Usage, now_ms};

/// Base assistant message for stream accumulation.
pub fn initial_output(model: &Model) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        thinking_level: None,
        diagnostics: Vec::new(),
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

/// Fill cost (from catalog prices) and duration on a finished message.
pub fn finalize(output: &mut AssistantMessage, model: &Model) {
    calculate_cost(&mut output.usage, &model.cost);
    output.duration_ms = Some(u64::try_from(now_ms() - output.timestamp).unwrap_or(0));
}

/// Render a system message's full text: content plus named section values
/// (TS `getSystemMessageText`).
pub fn system_text(message: &SystemMessage) -> String {
    let mut parts: Vec<String> = Vec::new();
    match &message.content {
        crate::types::StringOrTextBlocks::Text(s) => {
            if !s.is_empty() {
                parts.push(s.clone());
            }
        }
        crate::types::StringOrTextBlocks::Blocks(blocks) => {
            let joined = blocks
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("");
            if !joined.is_empty() {
                parts.push(joined);
            }
        }
    }
    for text in message
        .sections
        .iter()
        .flatten()
        .filter_map(|(_, v)| v.as_deref())
    {
        if !text.is_empty() {
            parts.push(text.to_string());
        }
    }
    parts.join("\n\n")
}

/// Render a later (mid-conversation) system message for APIs that accept
/// them natively (TS `renderSystemMessageUpdate`).
pub fn system_update_text(message: &SystemMessage) -> String {
    let mut parts: Vec<String> = Vec::new();
    match &message.content {
        crate::types::StringOrTextBlocks::Text(s) => {
            if !s.is_empty() {
                parts.push(s.clone());
            }
        }
        crate::types::StringOrTextBlocks::Blocks(blocks) => {
            let joined = blocks
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("");
            if !joined.is_empty() {
                parts.push(joined);
            }
        }
    }
    for (name, value) in message.sections.iter().flatten() {
        parts.push(match value {
            None => format!("Removed system prompt section \"{name}\"."),
            Some(text) => format!("Updated system prompt section \"{name}\":\n\n{text}"),
        });
    }
    parts.join("\n\n")
}

/// Merge tools declared by leading system messages (TS `getCurrentTools`):
/// later definitions replace earlier ones under the same name, and
/// `tools_removed` entries drop them again.
pub fn tools_at_point(messages: &[Message]) -> Vec<crate::types::Tool> {
    let mut tools: Vec<crate::types::Tool> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    for msg in messages {
        let Message::System(system) = msg else {
            continue;
        };
        for tool in system.tools_added.iter().flatten() {
            removed.retain(|name| name != &tool.name);
            if let Some(existing) = tools.iter_mut().find(|t| t.name == tool.name) {
                *existing = tool.clone();
            } else {
                tools.push(tool.clone());
            }
        }
        for reference in system.tools_removed.iter().flatten() {
            removed.push(reference.name.clone());
            tools.retain(|t| t.name != reference.name);
        }
    }
    let _ = removed;
    tools
}

/// True when all messages before `index` are `System` (i.e. `index` is the
/// first non-system message).
pub fn is_leading_system(messages: &[Message], index: usize) -> bool {
    messages[..index]
        .iter()
        .all(|m| matches!(m, Message::System(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Tool, ToolReference};

    fn system(content: &str, tools_added: Vec<Tool>, tools_removed: Vec<&str>) -> Message {
        Message::System(SystemMessage {
            content: crate::types::StringOrTextBlocks::Text(content.into()),
            sections: None,
            tools_added: Some(tools_added),
            tools_removed: Some(
                tools_removed
                    .into_iter()
                    .map(|name| ToolReference { name: name.into() })
                    .collect(),
            ),
            timestamp: 1,
        })
    }

    fn tool(name: &str) -> Tool {
        Tool {
            name: name.into(),
            description: "d".into(),
            parameters: serde_json::json!({}),
            constrained_sampling: None,
        }
    }

    #[test]
    fn tools_merge_and_remove() {
        let msgs = vec![
            system("s1", vec![tool("a"), tool("b")], vec![]),
            system("", vec![tool("c")], vec!["a"]),
        ];
        let tools = tools_at_point(&msgs);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["b", "c"]);
    }

    #[test]
    fn system_text_joins_sections() {
        let msg = SystemMessage {
            content: crate::types::StringOrTextBlocks::Text("base".into()),
            sections: Some(
                [
                    ("env".to_string(), Some("ctx".to_string())),
                    ("gone".to_string(), None),
                ]
                .into_iter()
                .collect(),
            ),
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
        };
        assert_eq!(system_text(&msg), "base\n\nctx");
        let update = system_update_text(&msg);
        assert!(
            update.contains("Updated system prompt section \"env\""),
            "{update}"
        );
        assert!(
            update.contains("Removed system prompt section \"gone\""),
            "{update}"
        );
    }
}
