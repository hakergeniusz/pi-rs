//! Transcript replay helpers over raw JSON messages
//! (`packages/ai/src/utils/transcript.ts`): the system-message folding
//! the `context` transform needs to reattach the prompt and tool state
//! after a handler rewrote the conversation.

use serde_json::{Value, json};

/// `contentText` (`utils/text.ts:6-12`): string content passes through,
/// otherwise the text blocks join with `\n`.
#[must_use]
pub fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn is_system_message(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("system")
}

/// `getCurrentTools` (`transcript.ts:63-73`): the tool set after applying
/// every system message's `toolsRemoved`/`toolsAdded` deltas in order.
#[must_use]
pub fn get_current_tools(messages: &[Value]) -> Vec<Value> {
    let mut tools: Vec<(String, Value)> = Vec::new();
    for message in messages {
        if !is_system_message(message) {
            continue;
        }
        if let Some(removed) = message.get("toolsRemoved").and_then(Value::as_array) {
            for tool in removed {
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    continue;
                };
                tools.retain(|(existing, _)| existing != name);
            }
        }
        if let Some(added) = message.get("toolsAdded").and_then(Value::as_array) {
            for tool in added {
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    continue;
                };
                tools.retain(|(existing, _)| existing != name);
                tools.push((name.to_string(), tool.clone()));
            }
        }
    }
    tools.into_iter().map(|(_, tool)| tool).collect()
}

/// `getCurrentSystemMessage` (`transcript.ts:78-95`): replay every system
/// message into one leading message holding the current prompt and
/// tools — later content appends, `sections` patch by name (null
/// deletes), tools resolve through [`get_current_tools`].
#[must_use]
pub fn get_current_system_message(messages: &[Value]) -> Option<Value> {
    let mut content: Vec<String> = Vec::new();
    let mut sections: Vec<(String, Value)> = Vec::new();
    let mut timestamp: Option<Value> = None;
    for message in messages {
        if !is_system_message(message) {
            continue;
        }
        if timestamp.is_none() {
            timestamp = message.get("timestamp").cloned();
        }
        let text = content_text(message.get("content").unwrap_or(&Value::Null));
        if !text.is_empty() {
            content.push(text);
        }
        if let Some(patch) = message.get("sections").and_then(Value::as_object) {
            for (name, value) in patch {
                if value.is_null() {
                    sections.retain(|(existing, _)| existing != name);
                } else {
                    sections.retain(|(existing, _)| existing != name);
                    sections.push((name.clone(), value.clone()));
                }
            }
        }
    }
    let tools = get_current_tools(messages);
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }
    let mut replayed = json!({
        "role": "system",
        "content": content.join("\n\n"),
    });
    if !sections.is_empty() {
        let map = sections
            .into_iter()
            .collect::<serde_json::Map<String, Value>>();
        replayed["sections"] = Value::Object(map);
    }
    if !tools.is_empty() {
        replayed["toolsAdded"] = json!(tools);
    }
    replayed["timestamp"] = timestamp.unwrap_or_else(|| json!(0));
    Some(replayed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replay_folds_content_sections_and_tool_deltas() {
        let messages = vec![
            json!({ "role": "system", "content": "base", "toolsAdded": [{ "name": "read" }], "timestamp": 5 }),
            json!({ "role": "user", "content": "hi" }),
            json!({ "role": "system", "content": "extra", "sections": { "a": "one" } }),
            json!({ "role": "system", "content": "", "sections": { "a": null, "b": "two" }, "toolsRemoved": [{ "name": "read" }], "toolsAdded": [{ "name": "bash" }] }),
        ];
        let replayed = get_current_system_message(&messages).expect("replayed");
        assert_eq!(replayed["content"], "base\n\nextra");
        assert_eq!(replayed["sections"], json!({ "b": "two" }));
        assert_eq!(replayed["toolsAdded"], json!([{ "name": "bash" }]));
        assert_eq!(replayed["timestamp"], 5);
    }

    #[test]
    fn replay_without_system_messages_is_none() {
        let messages = vec![json!({ "role": "user", "content": "hi" })];
        assert!(get_current_system_message(&messages).is_none());
    }

    #[test]
    fn content_text_joins_text_blocks() {
        assert_eq!(content_text(&json!("raw")), "raw");
        assert_eq!(
            content_text(&json!([
                { "type": "text", "text": "a" },
                { "type": "image", "url": "x" },
                { "type": "text", "text": "b" },
            ])),
            "a\nb"
        );
    }
}
