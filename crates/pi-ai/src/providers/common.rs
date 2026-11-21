//! Shared provider-adapter helpers: message assembly, finalization, abort, and
//! the `onPayload`/`onResponse`/`onProviderStreamEvent` hook bridges used by
//! every built-in adapter (TS `packages/ai/src/types.ts:161-232`,
//! `api/simple-options.ts:49-80`).

use crate::cost::calculate_cost;
use crate::types::{
    AssistantMessage, JsonValue, Message, Model, PayloadHook, ProviderHook, ProviderResponse,
    ResponseHook, StopReason, StreamEventHook, SystemMessage, Usage, now_ms,
};
use std::collections::BTreeMap;

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

/// Capture the status/headers of a received response for `onResponse`
/// (TS `headersToRecord`, `packages/ai/src/utils/headers.ts:3-9`).
pub fn response_metadata(response: &reqwest::Response) -> ProviderResponse {
    let mut headers = BTreeMap::new();
    for (name, value) in response.headers().iter() {
        headers.insert(
            name.as_str().to_ascii_lowercase(),
            value.to_str().map(str::to_string).unwrap_or_default(),
        );
    }
    ProviderResponse {
        status: response.status().as_u16(),
        headers,
    }
}

/// Run `onPayload` before the request is sent (TS `openai-completions.ts:375-378`,
/// `anthropic-messages.ts:905-908`, `openai-codex-responses.ts:287-290`): a
/// `None` return keeps the built payload; an `Err` is the stream's terminal
/// error, like a rejected upstream promise.
pub async fn apply_payload_hook(
    hook: &ProviderHook<PayloadHook>,
    payload: JsonValue,
    model: &Model,
) -> Result<JsonValue, String> {
    let Some(callback) = hook.get() else {
        return Ok(payload);
    };
    let original = payload.clone();
    match callback(payload, model.clone()).await? {
        Some(replacement) => Ok(replacement),
        None => Ok(original),
    }
}

/// Run `onResponse` after the HTTP headers arrive and before the body stream
/// is consumed (TS `openai-completions.ts:392`, `anthropic-messages.ts:943`,
/// `openai-codex-responses.ts:433-436`).
pub async fn apply_response_hook(
    hook: &ProviderHook<ResponseHook>,
    response: ProviderResponse,
    model: &Model,
) -> Result<(), String> {
    if let Some(callback) = hook.get() {
        callback(response, model.clone()).await?;
    }
    Ok(())
}

/// Run `onProviderStreamEvent` for each parsed provider event before Pi
/// normalizes it (TS `openai-completions.ts:545`, `anthropic-messages.ts:954`,
/// `openai-codex-responses.ts:772`).
pub async fn apply_stream_event_hook(
    hook: &ProviderHook<StreamEventHook>,
    event: JsonValue,
    model: &Model,
) -> Result<(), String> {
    if let Some(callback) = hook.get() {
        callback(event, model.clone()).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Tool, ToolReference};
    use std::sync::Arc;

    #[tokio::test]
    async fn hook_slots_default_to_noop() {
        let payload = serde_json::json!({"model": "original"});
        let model = Model::default();
        assert_eq!(
            apply_payload_hook(&ProviderHook::default(), payload.clone(), &model)
                .await
                .unwrap(),
            payload
        );
        apply_response_hook(
            &ProviderHook::default(),
            ProviderResponse {
                status: 200,
                headers: BTreeMap::new(),
            },
            &model,
        )
        .await
        .unwrap();
        apply_stream_event_hook(&ProviderHook::default(), payload, &model)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn payload_hook_replaces_and_hook_errors_propagate() {
        let model = Model::default();
        let payload = serde_json::json!({"model": "original"});

        let replacing: Arc<PayloadHook> = Arc::new(|payload, _model| {
            Box::pin(async move {
                let mut next = payload;
                next["model"] = serde_json::json!("replacement");
                Ok(Some(next))
            })
        });
        let slot = ProviderHook::new(replacing);
        assert_eq!(
            apply_payload_hook(&slot, payload, &model).await.unwrap()["model"],
            "replacement"
        );

        let failing: Arc<ResponseHook> =
            Arc::new(|_response, _model| Box::pin(async move { Err("rejected".to_string()) }));
        let slot = ProviderHook::new(failing);
        let error = apply_response_hook(
            &slot,
            ProviderResponse {
                status: 200,
                headers: BTreeMap::new(),
            },
            &model,
        )
        .await
        .unwrap_err();
        assert_eq!(error, "rejected");
    }

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
