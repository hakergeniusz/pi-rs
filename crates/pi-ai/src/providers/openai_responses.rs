//! `OpenAI` Responses and Azure `OpenAI` Responses adapters — port of TS
//! `api/openai-responses.ts:1-455`, `api/azure-openai-responses.ts:1-256`,
//! `api/openai-responses-shared.ts:1-891`, `api/azure-openai-config.ts:1-107`,
//! and the four-line lazy entry modules (`openai-responses.lazy.ts`,
//! `azure-openai-responses.lazy.ts`).
//!
//! Both adapters share one request/event pipeline: `POST {baseUrl}/responses`
//! (`{base}/responses?api-version=…` on Azure), SSE `ResponseStreamEvent`s
//! mapped to `AssistantMessageEvent`s by `shared::ResponsesStreamState`.
//! Upstream's SDK `withResponse()`,
//! `onPayload`/`onResponse`/`onProviderStreamEvent` hooks, usage/stop mapping,
//! reasoning replay signatures and Azure encrypted-content backfill are
//! preserved. Failures surface as terminal `error` events, never panics.
//!
//! Documented deviations from upstream (see `WIRING.md`):
//! - Provider-specific options (`azureResourceName`, `azureDeploymentName`,
//!   `azureBaseUrl`, `azureApiVersion`, `reasoningSummary`, `serviceTier`,
//!   `reasoningEffort`) ride in `StreamOptions::metadata` because the shared
//!   `StreamOptions` cannot be extended; `reasoningEffort` also has the
//!   provider-neutral `StreamOptions::reasoning` path.
//! - No `ClientIdentity`/scoped `ProviderEnv` on `StreamOptions`: identity
//!   headers reduce to `User-Agent: pi`, and env lookups use `std::env`.
//! - The OpenAI SDK's retry/backoff is omitted (upstream sets `maxRetries: 0`
//!   on the request anyway).
//! - `[DONE]` is tolerated as a defensive terminator; the Responses protocol
//!   normally ends with `response.completed` instead.

// 1:1 port of the upstream TS adapter: the TS-shaped control flow, literal
// tables and helper structs trigger pedantic lints; keep the mirror exact.
#![allow(
    clippy::assert_is_empty,
    clippy::assigning_clones,
    clippy::collapsible_if,
    clippy::default_trait_access,
    clippy::doc_markdown,
    clippy::field_reassign_with_default,
    clippy::float_cmp,
    clippy::format_push_string,
    clippy::if_not_else,
    clippy::if_same_then_else,
    clippy::manual_assert_eq,
    clippy::manual_is_multiple_of,
    clippy::manual_strip,
    clippy::map_unwrap_or,
    clippy::needless_pass_by_value,
    clippy::redundant_closure_for_method_calls,
    clippy::result_large_err,
    clippy::single_match_else,
    clippy::struct_excessive_bools,
    clippy::too_many_arguments,
    clippy::unnecessary_get_then_check,
    clippy::unnecessary_map_or,
    clippy::unnecessary_wraps,
    clippy::useless_concat,
    dead_code
)]

#[path = "azure_openai_config.rs"]
mod azure_config;
#[path = "openai_responses_shared.rs"]
mod shared;


use std::collections::BTreeMap;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{Value, json};

use crate::event_stream::{AssistantMessageEventStream, EventSender};
use crate::providers::common::{
    apply_payload_hook, apply_response_hook, apply_stream_event_hook, initial_output,
    response_metadata, tools_at_point,
};
use crate::sse::{SseError, read_sse_stream};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, Model, StopReason,
    StreamOptions, now_ms,
};

use shared::{
    ResponsesCompat, ResponsesStreamState, clamp_prompt_cache_key, convert_responses_messages,
    convert_responses_tools, effort_for_level,
};

/// `OPENAI_RESPONSES_MIN_OUTPUT_TOKENS` (`openai-responses.ts:38`).
const MIN_OUTPUT_TOKENS: u64 = 16;
/// `CHATGPT_USAGE_URL` (`openai-responses.ts:39`).
const CHATGPT_USAGE_URL: &str = "https://chatgpt.com/settings/usage";
/// `CHATGPT_WORKSPACE_LIMIT_HINT` (`openai-responses.ts:40-41`). The arrow is
/// verbatim from upstream's user-facing message.
#[allow(clippy::non_ascii_literal)]
const CHATGPT_WORKSPACE_LIMIT_HINT: &str = "You've reached your ChatGPT workspace's usage limit. Ask your workspace admin to increase it at admin.openai.com → Workspace → Usage limits.";
/// `CONFIGURATION_UPDATE_REQUEST_EFFORT` (`openai-responses.ts:43`).
const CONFIGURATION_UPDATE_REQUEST_EFFORT: &str = "medium";

type StreamResult<T> = Result<T, String>;

/// `stream` for the `openai-responses` API (`openai-responses.ts:155-273`).
pub fn stream(
    model: &Model,
    context: &crate::types::TranscriptContext,
    options: &StreamOptions,
) -> AssistantMessageEventStream {
    spawn(model, context, options, false)
}

/// `stream` for the `azure-openai-responses` API
/// (`azure-openai-responses.ts:40-129`).
pub fn azure_stream(
    model: &Model,
    context: &crate::types::TranscriptContext,
    options: &StreamOptions,
) -> AssistantMessageEventStream {
    spawn(model, context, options, true)
}

fn spawn(
    model: &Model,
    context: &crate::types::TranscriptContext,
    options: &StreamOptions,
    azure: bool,
) -> AssistantMessageEventStream {
    let (stream_out, sender) = AssistantMessageEventStream::channel();
    let (model, context, options) = (model.clone(), context.clone(), options.clone());
    tokio::spawn(async move {
        run(&model, &context, &options, azure, sender).await;
    });
    stream_out
}

async fn run(
    model: &Model,
    context: &crate::types::TranscriptContext,
    options: &StreamOptions,
    azure: bool,
    sender: EventSender,
) {
    let mut output = initial_output(model);
    let started = output.timestamp;
    let result = request_and_stream(model, context, options, azure, &mut output, &sender).await;
    output.duration_ms = Some(u64::try_from(now_ms() - started).unwrap_or(0));
    match result {
        Ok(()) => {
            if matches!(output.stop_reason, StopReason::Error | StopReason::Aborted) {
                let _ = sender
                    .send(AssistantMessageEvent::Error {
                        reason: output.stop_reason,
                        error: output,
                    })
                    .await;
            } else {
                let _ = sender
                    .send(AssistantMessageEvent::Done {
                        reason: output.stop_reason,
                        message: output,
                    })
                    .await;
            }
        }
        Err(message) => {
            output.stop_reason = StopReason::Error;
            output.error_message = Some(if azure {
                message
            } else {
                format_openai_error(&message)
            });
            let _ = sender
                .send(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: output,
                })
                .await;
        }
    }
}

/// Read a provider-specific option from `StreamOptions::metadata`.
fn option<'a>(options: &'a StreamOptions, key: &str) -> Option<&'a str> {
    options
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(key))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// `getClientApiKey` (`openai-responses.ts:67-71`): an explicit key wins, then
/// an OAuth access token, then headers that already carry authorization.
fn get_client_api_key(model: &Model, options: &StreamOptions) -> StreamResult<String> {
    if let Some(key) = options.api_key.as_deref().filter(|key| !key.is_empty()) {
        return Ok(key.to_string());
    }
    if let Some(token) = options
        .access_token
        .as_deref()
        .filter(|token| !token.is_empty())
    {
        return Ok(token.to_string());
    }
    let has_authorization = options.headers.iter().any(|(name, value)| {
        value
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            && (name.eq_ignore_ascii_case("authorization")
                || name.eq_ignore_ascii_case("cf-aig-authorization"))
    });
    if has_authorization {
        return Ok("unused".to_string());
    }
    Err(format!("No API key for provider: {}", model.provider))
}

/// `isChatGPTSignIn` (`openai-responses.ts:49-56`). A non-`sk-` credential sent
/// straight to `api.openai.com` is a Sign in with ChatGPT access token.
fn is_chat_gpt_sign_in(model: &Model, api_key: &str) -> bool {
    model.provider == "openai"
        && model.base_url == "https://api.openai.com/v1"
        && !api_key.starts_with("sk-")
}

/// `resolveCacheRetention` (`openai-responses.ts:81-89`). The port has no
/// scoped env overrides, so `PI_CACHE_RETENTION` is read from the process.
fn resolve_cache_retention(options: &StreamOptions) -> CacheRetention {
    if let Some(retention) = options.cache_retention {
        retention
    } else if std::env::var("PI_CACHE_RETENTION").ok().as_deref() == Some("long") {
        CacheRetention::Long
    } else {
        CacheRetention::Short
    }
}

/// `getConfigurationUpdateEffort` (`openai-responses.ts:109-123`).
fn configuration_update_effort(
    model: &Model,
    options: &StreamOptions,
    compat: &ResponsesCompat,
    api_key: &str,
) -> Option<String> {
    if !compat.supports_mid_convo_effort || is_chat_gpt_sign_in(model, api_key) {
        return None;
    }
    options
        .reasoning
        .map(|level| effort_for_level(model, level))
}

/// `applyServiceTierPricing` special-case prefixed error hints in
/// `openai-responses.ts:260-266`: upstream appends ChatGPT usage guidance to
/// the formatted error body.
fn format_openai_error(message: &str) -> String {
    if message.contains("subscription_sharing_usage_limit_exceeded") {
        format!("{message}\nCheck your ChatGPT usage: {CHATGPT_USAGE_URL}")
    } else if message.contains("subscription_sharing_workplace_individual_usage_limit_exceeded") {
        format!("{message}\n{CHATGPT_WORKSPACE_LIMIT_HINT}")
    } else {
        message.to_string()
    }
}

/// Default OpenAI request headers (`createClient`, `openai-responses.ts:295-338`).
/// Model and request headers are applied last so callers can override or
/// delete (`None`) any default.
fn build_openai_headers(
    model: &Model,
    options: &StreamOptions,
    api_key: &str,
    session_id: Option<&str>,
    compat: &ResponsesCompat,
) -> BTreeMap<String, Option<String>> {
    let mut headers = BTreeMap::new();
    headers.insert(
        "authorization".to_string(),
        Some(format!("Bearer {api_key}")),
    );
    headers.insert(
        "content-type".to_string(),
        Some("application/json".to_string()),
    );
    headers.insert("accept".to_string(), Some("text/event-stream".to_string()));
    // `getIdentityHeaders` with the port's short user agent.
    headers.insert("user-agent".to_string(), Some("pi".to_string()));
    if let Some(session_id) = session_id {
        if compat.session_affinity_format == "openrouter" {
            headers.insert("x-session-id".to_string(), Some(session_id.to_string()));
        } else {
            if compat.session_affinity_format == "openai" {
                headers.insert("session-id".to_string(), Some(session_id.to_string()));
            }
            headers.insert(
                "x-client-request-id".to_string(),
                Some(session_id.to_string()),
            );
        }
    }
    for (name, value) in model.headers.iter().flatten() {
        headers.insert(name.clone(), Some(value.clone()));
    }
    for (name, value) in &options.headers {
        headers.insert(name.clone(), value.clone());
    }
    headers
}

/// Azure request headers (`azure-openai-responses.ts:154-171`). Upstream's SDK
/// sends the key as `api-key`; an `accessToken` additionally produces a bearer
/// `authorization` header so Entra-style auth tokens work without editing the
/// shared options type.
fn build_azure_headers(
    model: &Model,
    options: &StreamOptions,
    api_key: &str,
) -> BTreeMap<String, Option<String>> {
    let mut headers = BTreeMap::new();
    headers.insert("api-key".to_string(), Some(api_key.to_string()));
    if let Some(token) = options
        .access_token
        .as_deref()
        .filter(|token| !token.is_empty())
    {
        headers.insert("authorization".to_string(), Some(format!("Bearer {token}")));
    }
    headers.insert(
        "content-type".to_string(),
        Some("application/json".to_string()),
    );
    headers.insert("accept".to_string(), Some("text/event-stream".to_string()));
    headers.insert("user-agent".to_string(), Some("pi".to_string()));
    for (name, value) in model.headers.iter().flatten() {
        headers.insert(name.clone(), Some(value.clone()));
    }
    for (name, value) in &options.headers {
        headers.insert(name.clone(), value.clone());
    }
    headers
}

fn apply_headers(
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

/// Azure endpoint (`createClient` + `resolveAzureConfig`): v1 base URLs put the
/// deployment in the body; older base URLs use the SDK's deployments path.
fn azure_url(model: &Model, options: &StreamOptions, deployment: &str) -> StreamResult<String> {
    let (base_url, api_version) = azure_config::resolve_azure_config(model, options)?;
    let base_url = base_url.trim_end_matches('/');
    let path = if base_url.ends_with("/openai/v1") {
        format!("{base_url}/responses")
    } else {
        format!("{base_url}/openai/deployments/{deployment}/responses")
    };
    Ok(format!("{path}?api-version={api_version}"))
}

/// `buildParams` (`openai-responses.ts:340-425`).
#[allow(clippy::too_many_lines)] // mirrors the TS request assembly; split when extending
fn build_openai_body(
    model: &Model,
    context: &crate::types::TranscriptContext,
    options: &StreamOptions,
    compat: &ResponsesCompat,
    configuration_update_effort: Option<&str>,
    api_key: &str,
) -> Value {
    let messages = convert_responses_messages(model, context, compat, configuration_update_effort);
    let cache_retention = resolve_cache_retention(options);
    let omit_unsupported = is_chat_gpt_sign_in(model, api_key);
    let session_id = options
        .session_id
        .as_deref()
        .filter(|session_id| !session_id.is_empty());

    let mut body = json!({
        "model": model.id,
        "input": messages,
        "stream": true,
        "store": false,
    });
    if let Some(session_id) = session_id
        && compat.prompt_cache_key_mode != "disabled"
    {
        body["prompt_cache_key"] = json!(clamp_prompt_cache_key(session_id));
    }
    if !omit_unsupported {
        if cache_retention == CacheRetention::Long
            && compat.supports_long_cache_retention
            && !compat.supports_explicit_prompt_cache_mode
        {
            body["prompt_cache_retention"] = json!("24h");
        }
        if compat.supports_explicit_prompt_cache_mode {
            match cache_retention {
                CacheRetention::Short => {
                    body["prompt_cache_options"] = json!({ "mode": "explicit" });
                }
                CacheRetention::Long if compat.supports_long_cache_retention => {
                    body["prompt_cache_options"] = json!({ "ttl": "30m" });
                }
                CacheRetention::Long => {}
            }
        }
    }
    if let Some(max_tokens) = options.max_tokens
        && compat.supports_max_output_tokens
        && !omit_unsupported
    {
        body["max_output_tokens"] = json!(max_tokens.max(MIN_OUTPUT_TOKENS));
    }
    if let Some(temperature) = options.temperature
        && !omit_unsupported
    {
        body["temperature"] = json!(temperature);
    }
    if let Some(service_tier) = option(options, "serviceTier") {
        body["service_tier"] = json!(service_tier);
    }
    let tools = tools_at_point(&context.messages);
    if !tools.is_empty() {
        body["tools"] = json!(convert_responses_tools(
            model,
            &tools,
            compat.supports_strict_mode
        ));
    }
    if let Some(tool_choice) = options.tool_choice {
        body["tool_choice"] = json!(match tool_choice {
            crate::types::ToolChoice::Auto => "auto",
            crate::types::ToolChoice::None => "none",
        });
    }
    apply_reasoning(&mut body, model, options, configuration_update_effort, true);
    if let Some(sampling) = &options.sampling_params {
        for (key, value) in sampling {
            body[key] = value.clone();
        }
    }
    body
}

/// `buildParams` (`azure-openai-responses.ts:173-256`).
#[allow(clippy::too_many_lines)] // mirrors the TS request assembly; split when extending
fn build_azure_body(
    model: &Model,
    context: &crate::types::TranscriptContext,
    options: &StreamOptions,
    deployment: &str,
) -> Value {
    let compat = ResponsesCompat::from_model(model);
    let messages = convert_responses_messages(model, context, &compat, None);
    let mut body = json!({
        "model": deployment,
        "input": messages,
        "stream": true,
        "store": false,
    });
    let session_id = options
        .session_id
        .as_deref()
        .filter(|session_id| !session_id.is_empty());
    if let Some(session_id) = session_id
        && compat.prompt_cache_key_mode != "disabled"
    {
        body["prompt_cache_key"] = json!(clamp_prompt_cache_key(session_id));
    }
    if let Some(max_tokens) = options.max_tokens {
        body["max_output_tokens"] = json!(max_tokens.max(MIN_OUTPUT_TOKENS));
    }
    if let Some(temperature) = options.temperature {
        body["temperature"] = json!(temperature);
    }
    let tools = tools_at_point(&context.messages);
    if !tools.is_empty() {
        // Azure's `supportsStrictMode` default is `true` (unlike OpenAI's).
        body["tools"] = json!(convert_responses_tools(model, &tools, true));
    }
    if let Some(tool_choice) = options.tool_choice {
        body["tool_choice"] = json!(match tool_choice {
            crate::types::ToolChoice::Auto => "auto",
            crate::types::ToolChoice::None => "none",
        });
    }
    apply_reasoning(&mut body, model, options, None, false);
    if let Some(sampling) = &options.sampling_params {
        for (key, value) in sampling {
            body[key] = value.clone();
        }
    }
    body
}

/// Shared `params.reasoning` assembly of both `buildParams` functions.
fn apply_reasoning(
    body: &mut Value,
    model: &Model,
    options: &StreamOptions,
    configuration_update_effort: Option<&str>,
    openai: bool,
) {
    let summary = option(options, "reasoningSummary")
        .filter(|summary| matches!(*summary, "auto" | "detailed" | "concise"));
    let requested = options
        .reasoning
        .map(|level| effort_for_level(model, level));
    let reasoning_effort = requested.or_else(|| summary.map(|_| "medium".to_string()));
    if model.reasoning {
        if let Some(effort) = &reasoning_effort {
            let effective = if configuration_update_effort.is_some() {
                CONFIGURATION_UPDATE_REQUEST_EFFORT
            } else {
                effort.as_str()
            };
            body["reasoning"] = json!({
                "effort": effective,
                "summary": summary.unwrap_or("auto"),
            });
            body["include"] = json!(["reasoning.encrypted_content"]);
        } else if (!openai || model.provider != "github-copilot")
            && model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get("off"))
                .is_none_or(|value| !value.is_null())
        {
            let off = model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get("off"))
                .and_then(Value::as_str)
                .unwrap_or("none");
            body["reasoning"] = json!({ "effort": off });
        }
        if model.provider == "xai" {
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
    }
}

/// Send the request and drive the SSE loop (`stream` + `processResponsesStream`).
#[allow(clippy::too_many_lines)] // transport + loop; matches the TS stream entry
async fn request_and_stream(
    model: &Model,
    context: &crate::types::TranscriptContext,
    options: &StreamOptions,
    azure: bool,
    output: &mut AssistantMessage,
    sender: &EventSender,
) -> StreamResult<()> {
    let label = if azure {
        "Azure OpenAI Responses"
    } else {
        "OpenAI Responses"
    };
    let compat = ResponsesCompat::from_model(model);
    let api_key = if azure {
        azure_config::resolve_azure_api_key(model, options)?
    } else {
        get_client_api_key(model, options)?
    };
    let session_id = options
        .session_id
        .as_deref()
        .filter(|session_id| !session_id.is_empty());
    let body = if azure {
        let deployment = azure_config::resolve_deployment_name(model, options);
        build_azure_body(model, context, options, &deployment)
    } else {
        let update_effort = configuration_update_effort(model, options, &compat, &api_key);
        if let Some(effort) = &update_effort {
            output.provider_thinking_level = Some(effort.clone());
        }
        build_openai_body(
            model,
            context,
            options,
            &compat,
            update_effort.as_deref(),
            &api_key,
        )
    };
    let body = apply_payload_hook(&options.on_payload, body, model).await?;

    let url = if azure {
        let deployment = azure_config::resolve_deployment_name(model, options);
        azure_url(model, options, &deployment)?
    } else {
        format!("{}/responses", model.base_url.trim_end_matches('/'))
    };
    let headers = if azure {
        build_azure_headers(model, options, &api_key)
    } else {
        build_openai_headers(model, options, &api_key, session_id, &compat)
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|error| format!("client build failed: {error}"))?;
    let response = apply_headers(client.post(&url).json(&body), headers)
        .send()
        .await
        .map_err(|error| format!("request failed: {error}"))?;
    // Upstream's SDK throws before `withResponse()` returns on non-2xx, so the
    // `onResponse` hook is only reached on success here.
    if response.status().is_success() {
        apply_response_hook(&options.on_response, response_metadata(&response), model).await?;
    }
    let sse = read_sse_stream(response)
        .await
        .map_err(|error| match error {
            SseError::Status { status, body } => format!("HTTP {status}: {body}"),
            SseError::Http(message) => message,
        })?;
    let _ = sender
        .send(AssistantMessageEvent::Start {
            partial: output.clone(),
        })
        .await;

    let mut state = ResponsesStreamState::new(!azure);
    tokio::pin!(sse);
    while let Some(item) = sse.next().await {
        let message = match item {
            Ok(message) => message,
            Err(SseError::Status { status, body }) => return Err(format!("HTTP {status}: {body}")),
            Err(SseError::Http(message)) => return Err(message),
        };
        let data = message.data.trim();
        if data.is_empty() {
            continue;
        }
        // The Responses protocol terminates with `response.completed`; accept
        // the Chat-Completions-style `[DONE]` sentinel defensively too.
        if data == "[DONE]" {
            apply_stream_event_hook(
                &options.on_provider_stream_event,
                Value::String("[DONE]".to_string()),
                model,
            )
            .await?;
            break;
        }
        let event: Value = serde_json::from_str(data)
            .map_err(|error| format!("could not parse Responses SSE event: {error}"))?;
        apply_stream_event_hook(&options.on_provider_stream_event, event.clone(), model).await?;
        state.apply(&event, output, model, sender).await?;
    }

    if !state.saw_terminal() {
        return Err(format!(
            "{label} stream ended before a terminal response event"
        ));
    }
    if output.stop_reason == StopReason::ToolUse
        && let Some(&index) = state.unfinished().first()
    {
        let (name, id) = match output.content.get(index) {
            Some(AssistantContent::ToolCall(block)) => (block.name.clone(), block.id.clone()),
            _ => (String::new(), String::new()),
        };
        return Err(format!(
            "{label} stream completed with an unfinished tool call: {name} ({id})"
        ));
    }
    if output.stop_reason == StopReason::Pending {
        return Err(format!("{label} stream ended without a stop reason"));
    }
    if matches!(output.stop_reason, StopReason::Error | StopReason::Aborted) {
        return Err(output
            .error_message
            .clone()
            .unwrap_or_else(|| "An unknown error occurred".to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseParser;
    use crate::types::{
        AssistantMessageEvent as Event, ModelCost, SystemMessage, ToolResultMessage, UserContent,
        UserMessage,
    };

    fn model() -> Model {
        Model {
            id: "gpt-5".into(),
            name: "GPT 5".into(),
            api: "openai-responses".into(),
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: true,
            thinking_level_map: Some(
                serde_json::from_str(
                    r#"{"off":"none","minimal":"low","low":"low","medium":"medium","high":"high"}"#,
                )
                .unwrap(),
            ),
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                ..ModelCost::default()
            },
            ..Model::default()
        }
    }

    fn azure_model() -> Model {
        let mut model = model();
        model.id = "gpt-5".into();
        model.api = "azure-openai-responses".into();
        model.provider = "azure".into();
        model.base_url.clear();
        model
    }

    fn context() -> crate::types::TranscriptContext {
        crate::types::TranscriptContext {
            messages: vec![
                crate::types::Message::System(SystemMessage {
                    content: crate::types::StringOrTextBlocks::Text("Be terse.".into()),
                    sections: None,
                    tools_added: None,
                    tools_removed: None,
                    timestamp: 0,
                }),
                crate::types::Message::User(UserMessage {
                    content: UserContent::Text("hi".into()),
                    timestamp: 1,
                }),
            ],
        }
    }

    /// Drive a canned SSE byte stream through the exact parser and event state
    /// used by the adapter's transport (without an HTTP server).
    async fn drive_sse(
        sse: &str,
        model: &Model,
    ) -> (AssistantMessage, Vec<Event>, Result<(), String>) {
        let mut parser = SseParser::default();
        let mut messages = parser.feed(sse.as_bytes());
        messages.extend(parser.finish());
        let mut output = initial_output(model);
        let (stream, sender) = AssistantMessageEventStream::channel();
        let mut state = ResponsesStreamState::new(false);
        let mut result = Ok(());
        for message in messages {
            let data = message.data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let event: Value = serde_json::from_str(data).unwrap();
            if let Err(error) = state.apply(&event, &mut output, model, &sender).await {
                result = Err(error);
                break;
            }
        }
        if result.is_ok() {
            if !state.saw_terminal() {
                result = Err(
                    "OpenAI Responses stream ended before a terminal response event".to_string(),
                );
            } else if output.stop_reason == StopReason::ToolUse
                && let Some(&index) = state.unfinished().first()
            {
                let (name, id) = match output.content.get(index) {
                    Some(AssistantContent::ToolCall(block)) => {
                        (block.name.clone(), block.id.clone())
                    }
                    _ => (String::new(), String::new()),
                };
                result = Err(format!(
                    "OpenAI Responses stream completed with an unfinished tool call: {name} ({id})"
                ));
            }
        }
        drop(sender);
        let events: Vec<Event> = futures::StreamExt::collect(stream).await;
        (output, events, result)
    }

    #[tokio::test]
    async fn text_stream_emits_delta_end_usage_and_done() {
        let model = model();
        let sse = concat!(
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"Hello\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\" world\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello world\"}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":100,\"output_tokens\":20,\"total_tokens\":120,\"input_tokens_details\":{\"cached_tokens\":10}}}}\n\n",
            "data: [DONE]\n\n",
        );
        let (output, events, result) = drive_sse(sse, &model).await;
        result.unwrap();
        assert!(matches!(
            events[0],
            Event::TextStart {
                content_index: 0,
                ..
            }
        ));
        assert!(matches!(events[1], Event::TextDelta { ref delta, .. } if delta == "Hello"));
        assert!(matches!(events[2], Event::TextDelta { ref delta, .. } if delta == " world"));
        assert!(
            matches!(events[3], Event::TextEnd { ref content, .. } if content == "Hello world")
        );
        assert_eq!(events.len(), 4);
        assert_eq!(output.response_id.as_deref(), Some("resp_1"));
        assert_eq!(output.usage.input, 90);
        assert_eq!(output.usage.cache_read, 10);
        assert_eq!(output.stop_reason, StopReason::Stop);
        let signature = match &output.content[0] {
            AssistantContent::Text(text) => text.text_signature.clone().unwrap(),
            other => panic!("expected text, got {other:?}"),
        };
        assert_eq!(signature, r#"{"v":1,"id":"msg_1"}"#);
    }

    #[tokio::test]
    async fn reasoning_stream_backfills_encrypted_content_from_terminal_response() {
        let model = model();
        let sse = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\"}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":0,\"delta\":\"thinking\"}\n\n",
            "data: {\"type\":\"response.reasoning_summary_part.done\",\"output_index\":0}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[{\"text\":\"thinking hard\"}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"enc_1\"}],\"usage\":{\"input_tokens\":5,\"output_tokens\":3,\"total_tokens\":8}}}\n\n",
        );
        let (output, events, result) = drive_sse(sse, &model).await;
        result.unwrap();
        assert!(matches!(
            events[0],
            Event::ThinkingStart {
                content_index: 0,
                ..
            }
        ));
        assert!(matches!(events[1], Event::ThinkingDelta { ref delta, .. } if delta == "thinking"));
        assert!(matches!(events[2], Event::ThinkingDelta { ref delta, .. } if delta == "\n\n"));
        assert!(
            matches!(events[3], Event::ThinkingEnd { ref content, .. } if content == "thinking hard")
        );
        let signature = match &output.content[0] {
            AssistantContent::Thinking(thinking) => thinking.thinking_signature.clone().unwrap(),
            other => panic!("expected thinking, got {other:?}"),
        };
        assert!(signature.contains("\"id\":\"rs_1\""), "{signature}");
        assert!(
            signature.contains("\"encrypted_content\":\"enc_1\""),
            "{signature}"
        );
    }

    #[tokio::test]
    async fn tool_call_stream_emits_toolcall_events_and_tool_use() {
        let model = model();
        let sse = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"id\":\"fc_1\",\"name\":\"bash\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"cmd\\\":\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"\\\"ls\\\"}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"id\":\"fc_1\",\"name\":\"bash\",\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
        );
        let (output, events, result) = drive_sse(sse, &model).await;
        result.unwrap();
        assert!(matches!(
            events[0],
            Event::ToolcallStart {
                content_index: 0,
                ..
            }
        ));
        assert!(
            matches!(events[1], Event::ToolcallDelta { ref delta, .. } if delta == "{\"cmd\":")
        );
        assert!(matches!(events[2], Event::ToolcallDelta { ref delta, .. } if delta == "\"ls\"}"));
        assert!(matches!(events[3], Event::ToolcallEnd { .. }));
        assert_eq!(events.len(), 4);
        assert_eq!(output.stop_reason, StopReason::ToolUse);
        match &output.content[0] {
            AssistantContent::ToolCall(block) => {
                assert_eq!(block.id, "call_1|fc_1");
                assert_eq!(block.name, "bash");
                assert_eq!(block.arguments["cmd"], "ls");
                assert!(block.arguments_error.is_none());
            }
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn errors_and_missing_terminal_event_are_rejected() {
        let model = model();
        let (_, _, result) = drive_sse(
            "data: {\"type\":\"error\",\"code\":\"rate_limit_exceeded\",\"message\":\"slow down\"}\n\n",
            &model,
        )
        .await;
        assert_eq!(
            result.unwrap_err(),
            "Error Code rate_limit_exceeded: slow down"
        );

        let (_, _, result) = drive_sse(
            "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"oops\",\"message\":\"boom\"}}}\n\n",
            &model,
        )
        .await;
        assert_eq!(result.unwrap_err(), "oops: boom");

        let (_, _, result) = drive_sse(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\"}}\n\n",
            &model,
        )
        .await;
        assert_eq!(
            result.unwrap_err(),
            "OpenAI Responses stream ended before a terminal response event"
        );
    }

    #[tokio::test]
    async fn unfinished_tool_call_is_rejected() {
        let model = model();
        let sse = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_9\",\"id\":\"fc_9\",\"name\":\"bash\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
        );
        let (_, _, result) = drive_sse(sse, &model).await;
        let error = result.unwrap_err();
        assert!(
            error.contains("unfinished tool call: bash (call_9|fc_9)"),
            "{error}"
        );
    }

    #[test]
    fn openai_request_body_shape() {
        let model = model();
        let mut options = StreamOptions::default();
        options.api_key = Some("sk-test".into());
        options.session_id = Some("session 1".into());
        options.max_tokens = Some(4);
        options.temperature = Some(0.3);
        options.reasoning = Some(crate::types::ThinkingLevel::High);
        options.metadata = Some(
            serde_json::from_str(r#"{"serviceTier":"flex","reasoningSummary":"detailed"}"#)
                .unwrap(),
        );
        let compat = ResponsesCompat::from_model(&model);
        let body = build_openai_body(&model, &context(), &options, &compat, None, "sk-test");
        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["max_output_tokens"], MIN_OUTPUT_TOKENS);
        assert_eq!(body["temperature"], 0.3);
        assert_eq!(body["service_tier"], "flex");
        assert_eq!(body["prompt_cache_key"], "session_1");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "detailed");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[1]["content"][0]["type"], "input_text");
    }

    #[test]
    fn azure_request_body_uses_deployment_and_defaults() {
        let model = azure_model();
        let options = StreamOptions::default();
        let body = build_azure_body(&model, &context(), &options, "deploy-gpt5");
        assert_eq!(body["model"], "deploy-gpt5");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        // No reasoning effort requested: the model's `off` mapping is used.
        assert_eq!(body["reasoning"]["effort"], "none");
    }

    #[test]
    fn openai_headers_carry_bearer_and_session_affinity() {
        let model = model();
        let options = StreamOptions::default();
        let compat = ResponsesCompat::from_model(&model);
        let headers = build_openai_headers(&model, &options, "sk-test", Some("session 1"), &compat);
        assert_eq!(headers["authorization"].as_deref(), Some("Bearer sk-test"));
        assert_eq!(headers["session-id"].as_deref(), Some("session 1"));
        assert_eq!(headers["x-client-request-id"].as_deref(), Some("session 1"));

        let mut options = StreamOptions::default();
        options.headers.insert("authorization".to_string(), None);
        let headers = build_openai_headers(&model, &options, "sk-test", None, &compat);
        assert!(headers["authorization"].is_none());
    }

    #[test]
    fn azure_headers_carry_api_key_or_bearer_token() {
        let model = azure_model();
        let options = StreamOptions::default();
        let headers = build_azure_headers(&model, &options, "key-123");
        assert_eq!(headers["api-key"].as_deref(), Some("key-123"));
        assert!(headers.get("authorization").is_none());

        let mut options = StreamOptions::default();
        options.access_token = Some("token-456".into());
        let headers = build_azure_headers(&model, &options, "key-123");
        assert_eq!(headers["api-key"].as_deref(), Some("key-123"));
        assert_eq!(
            headers["authorization"].as_deref(),
            Some("Bearer token-456")
        );

        let mut options = StreamOptions::default();
        options
            .headers
            .insert("api-key".to_string(), Some("custom-key".to_string()));
        let headers = build_azure_headers(&model, &options, "key-123");
        assert_eq!(headers["api-key"].as_deref(), Some("custom-key"));
    }

    #[test]
    fn azure_url_uses_v1_path_and_api_version() {
        let mut model = azure_model();
        let mut options = StreamOptions::default();
        options.metadata = Some(
            serde_json::from_str(r#"{"azureResourceName":"myres","azureApiVersion":"2024-10-21"}"#)
                .unwrap(),
        );
        assert_eq!(
            azure_url(&model, &options, "gpt-5").unwrap(),
            "https://myres.openai.azure.com/openai/v1/responses?api-version=2024-10-21"
        );
        model.base_url = "https://proxy.example/openai".into();
        let options = StreamOptions::default();
        assert_eq!(
            azure_url(&model, &options, "gpt-5").unwrap(),
            "https://proxy.example/openai/openai/deployments/gpt-5/responses?api-version=v1"
        );
    }

    #[test]
    fn api_key_resolution_and_chat_gpt_hints() {
        let model = model();
        let mut options = StreamOptions::default();
        assert_eq!(
            get_client_api_key(&model, &options).unwrap_err(),
            "No API key for provider: openai"
        );
        options
            .headers
            .insert("cf-aig-authorization".to_string(), Some("Bearer x".into()));
        assert_eq!(get_client_api_key(&model, &options).unwrap(), "unused");
        assert!(is_chat_gpt_sign_in(&model, "ey-token"));
        assert!(!is_chat_gpt_sign_in(&model, "sk-test"));
        let error = format_openai_error("subscription_sharing_usage_limit_exceeded");
        assert!(error.contains(CHATGPT_USAGE_URL));
        let error =
            format_openai_error("subscription_sharing_workplace_individual_usage_limit_exceeded");
        assert!(error.contains(CHATGPT_WORKSPACE_LIMIT_HINT));
    }

    #[test]
    fn replayed_assistant_and_tool_result_convert_to_response_items() {
        let model = model();
        let assistant = crate::types::AssistantMessage {
            content: vec![
                AssistantContent::Thinking(crate::types::ThinkingContent {
                    thinking: "hmm".into(),
                    thinking_signature: Some(r#"{"id":"rs_1","summary":[]}"#.into()),
                    redacted: false,
                }),
                AssistantContent::ToolCall(crate::types::ToolCall {
                    id: "call_1|fc_item2".into(),
                    name: "bash".into(),
                    arguments: serde_json::from_str(r#"{"cmd":"ls"}"#).unwrap(),
                    thought_signature: None,
                    namespace: None,
                    arguments_error: None,
                }),
            ],
            api: "openai-responses".into(),
            provider: "openai".into(),
            model: "gpt-5".into(),
            ..initial_output(&model)
        };
        let mut context = context();
        context
            .messages
            .push(crate::types::Message::Assistant(assistant));
        context
            .messages
            .push(crate::types::Message::ToolResult(ToolResultMessage {
                tool_call_id: "call_1|fc_item2".into(),
                tool_name: "bash".into(),
                content: vec![crate::types::InputContent::Text(
                    crate::types::TextContent {
                        text: "out".into(),
                        text_signature: None,
                    },
                )],
                details: None,
                usage: None,
                nested_calls: None,
                is_error: false,
                timestamp: 2,
                duration_ms: None,
            }));
        let compat = ResponsesCompat::from_model(&model);
        let input = convert_responses_messages(&model, &context, &compat, None);
        assert_eq!(input.len(), 5);
        assert_eq!(input[2]["id"], "rs_1");
        assert_eq!(input[3]["type"], "function_call");
        assert_eq!(input[3]["id"], "fc_item2");
        assert_eq!(input[4]["type"], "function_call_output");
        assert_eq!(input[4]["output"], "out");
    }
}
