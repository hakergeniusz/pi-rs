//! Shared provider-adapter helpers: message assembly, finalization, abort, and
//! the `onPayload`/`onResponse`/`onProviderStreamEvent` hook bridges used by
//! every built-in adapter (TS `packages/ai/src/types.ts:161-232`,
//! `api/simple-options.ts:49-80`).

use crate::cost::calculate_cost;
use crate::retry::{
    ProviderError, ProviderRetryOptions, is_retryable_provider_error, provider_retry_delay_ms,
    retry_provider_request,
};
use crate::types::{
    AssistantMessage, JsonValue, Message, Model, PayloadHook, ProviderHook, ProviderResponse,
    ResponseHook, StopReason, StreamEventHook, StreamOptions, SystemMessage, Usage, now_ms,
};
use std::collections::BTreeMap;
use std::time::Duration;

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
    for (name, value) in response.headers() {
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

/// Merge adapter defaults, catalog `model.headers` and per-call option headers
/// into one lowercased map (TS `mergeHeaders(getIdentityHeaders(...), ...,
/// model.headers, optionsHeaders)`: `openai-completions.ts:751,773-776`,
/// `anthropic-messages.ts:1278-1287,1323-1332`). Catalog headers can replace a
/// default (GitHub Copilot's `User-Agent`/`Editor-*`); option headers win over
/// both. A `None` option value removes the header entirely.
pub fn request_headers(
    defaults: &[(&str, &str)],
    model: &Model,
    options: &StreamOptions,
) -> BTreeMap<String, Option<String>> {
    let mut headers = BTreeMap::new();
    for (name, value) in defaults {
        headers.insert(name.to_ascii_lowercase(), Some((*value).to_string()));
    }
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            headers.insert(name.to_ascii_lowercase(), Some(value.clone()));
        }
    }
    for (name, value) in &options.headers {
        headers.insert(name.to_ascii_lowercase(), value.clone());
    }
    headers
}

/// Send a provider request with the SDK-compatible retry wrapper
/// (`utils/provider-retry.ts:99-128`): transient 408/409/429/5xx responses and
/// transport failures are retried according to `StreamOptions.max_retries`,
/// honouring `retry-after-ms`/`retry-after` and the delay cap. Non-2xx bodies
/// are consumed so the error is the same one `read_sse_stream` would report.
pub async fn send_with_retry(
    build: impl Fn() -> reqwest::RequestBuilder,
    options: &StreamOptions,
) -> Result<reqwest::Response, String> {
    let policy = ProviderRetryOptions {
        max_retries: options.max_retries,
        max_retry_delay_ms: options.max_retry_delay_ms,
        ..Default::default()
    };
    retry_provider_request(
        || async {
            let response = build().send().await.map_err(|error| ProviderError {
                status: None,
                headers: BTreeMap::new(),
                message: format!("request failed: {error}"),
            })?;
            let status = response.status();
            let headers: BTreeMap<String, String> = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_ascii_lowercase(),
                        value.to_str().unwrap_or_default().to_string(),
                    )
                })
                .collect();
            if status.is_success() {
                return Ok(response);
            }
            let body = response.text().await.unwrap_or_default();
            Err(ProviderError {
                status: Some(status.as_u16()),
                headers,
                message: format!("HTTP {status}: {body}"),
            })
        },
        &policy,
    )
    .await
}

/// Apply a merged header map to a request builder. `None` removes a header so
/// callers can suppress a default through `options.headers`.
pub fn apply_headers(
    builder: reqwest::RequestBuilder,
    headers: BTreeMap<String, Option<String>>,
) -> reqwest::RequestBuilder {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        if let Ok(name) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) {
            match value {
                Some(value) => {
                    if let Ok(value) = value.parse() {
                        map.insert(name, value);
                    }
                }
                None => {
                    map.remove(&name);
                }
            }
        }
    }
    builder.headers(map)
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

/// Shared options of the non-streaming classifier/image JSON POST
/// (`packages/ai/src/types.ts:365-382` `ClassifierOptions`/`ImagesOptions`,
/// `api/classifier-shared.ts:47-98`). Hooks and custom fetch are not exposed
/// by the Rust port; auth is resolved by the caller.
#[derive(Debug, Clone, Default)]
pub struct JsonPostOptions {
    pub api_key: Option<String>,
    /// Upstream classifier default is 2; image generation passes through 0.
    pub max_retries: u32,
    pub max_retry_delay_ms: Option<u64>,
    pub headers: BTreeMap<String, Option<String>>,
    /// Statuses that fail at once even though they are normally retried
    /// (`api/openai-decisions.ts:108` retries no 504s).
    pub no_retry_statuses: Vec<u16>,
}

/// POST one JSON document and parse the JSON response, with the SDK retry
/// policy (`utils/provider-retry.ts:99-128`). Returns the provider error text
/// (including the HTTP status and body) rather than throwing, so classifier
/// and image adapters can package it into their result types.
fn insert_header(headers: &mut reqwest::header::HeaderMap, name: &str, value: &str) {
    if let Ok(name) = reqwest::header::HeaderName::from_bytes(name.as_bytes())
        && let Ok(value) = reqwest::header::HeaderValue::from_str(value)
    {
        headers.insert(name, value);
    }
}

pub(crate) async fn post_json_with_retry(
    label: &str,
    url: &str,
    model_headers: Option<&BTreeMap<String, String>>,
    options: &JsonPostOptions,
    body: &JsonValue,
) -> Result<JsonValue, String> {
    let client = reqwest::Client::new();
    let mut remaining = options.max_retries;
    let mut retry_index = 0u32;
    loop {
        let result: Result<JsonValue, ProviderError> = async {
            let mut headers = reqwest::header::HeaderMap::new();
            insert_header(&mut headers, "content-type", "application/json");
            if let Some(key) = options.api_key.as_deref() {
                insert_header(&mut headers, "authorization", &format!("Bearer {key}"));
            }
            if let Some(model_headers) = model_headers {
                for (name, value) in model_headers {
                    insert_header(&mut headers, name, value);
                }
            }
            for (name, value) in &options.headers {
                if let Ok(header_name) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) {
                    match value {
                        Some(value) => insert_header(&mut headers, name, value),
                        None => {
                            headers.remove(header_name);
                        }
                    }
                }
            }
            let builder = client.post(url).headers(headers).json(body);
            let response = builder.send().await.map_err(|error| ProviderError {
                status: None,
                headers: BTreeMap::new(),
                message: format!("{label} error: request failed: {error}"),
            })?;
            let status = response.status();
            let headers: BTreeMap<String, String> = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_ascii_lowercase(),
                        value.to_str().unwrap_or_default().to_string(),
                    )
                })
                .collect();
            if status.is_success() {
                return response.json::<JsonValue>().await.map_err(|error| ProviderError {
                    status: Some(status.as_u16()),
                    headers,
                    message: format!("{label} error: invalid JSON response: {error}"),
                });
            }
            let error_body = response.text().await.unwrap_or_default();
            Err(ProviderError {
                status: Some(status.as_u16()),
                headers,
                message: if status.as_u16() == 504 {
                    format!(
                        "{label} error (504): the request timed out at the gateway. Very large inputs (above roughly 600K tokens) currently exceed its time limit."
                    )
                } else {
                    format!("{label} error ({}): {}", status.as_u16(), error_body)
                },
            })
        }
        .await;
        match result {
            Ok(value) => return Ok(value),
            Err(error) => {
                let message = error.message.clone();
                if remaining == 0
                    || !is_retryable_provider_error(&error)
                    || error
                        .status
                        .is_some_and(|status| options.no_retry_statuses.contains(&status))
                {
                    return Err(message);
                }
                let retry = retry_index;
                remaining -= 1;
                retry_index += 1;
                let delay = provider_retry_delay_ms(&error, retry, options.max_retry_delay_ms)?;
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }
    }
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

    #[test]
    fn catalog_headers_override_defaults_and_options_win() {
        let model = Model {
            headers: Some(
                [
                    (
                        "User-Agent".to_string(),
                        "GitHubCopilotChat/0.35.0".to_string(),
                    ),
                    ("Editor-Version".to_string(), "vscode/1.107.0".to_string()),
                    ("x-nvcF-poll-seconds".to_string(), "30".to_string()),
                ]
                .into_iter()
                .collect(),
            ),
            ..Model::default()
        };
        let mut options = StreamOptions::default();
        options
            .headers
            .insert("USER-AGENT".to_string(), Some("custom/1".to_string()));
        options.headers.insert("editor-version".to_string(), None);

        let headers = request_headers(
            &[("content-type", "application/json"), ("user-agent", "pi")],
            &model,
            &options,
        );
        assert_eq!(headers["content-type"].as_deref(), Some("application/json"));
        // Catalog overrides the default; options override the catalog.
        assert_eq!(headers["user-agent"].as_deref(), Some("custom/1"));
        assert_eq!(headers.get("editor-version"), Some(&None));
        assert_eq!(headers["x-nvcf-poll-seconds"].as_deref(), Some("30"));
        assert_eq!(headers.len(), 4, "case variants must dedupe: {headers:?}");
    }
}
