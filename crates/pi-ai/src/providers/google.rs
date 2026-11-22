//! Google Generative AI and Google Vertex streaming adapters (port of TS
//! `packages/ai/src/api/google-generative-ai.ts:1-466` and
//! `google-vertex.ts:1-561`; shared helpers in `google-shared.rs`).
//!
//! Both adapters call the same SDK (`@google/genai` `generateContentStream`)
//! and normalize its `GenerateContentResponse` chunks with the identical loop,
//! so only credential resolution and endpoint construction differ. Errors are
//! encoded in the returned event stream, never thrown (`StreamFn` contract).
//!
//! Deviations from upstream (documented in `WIRING.md`): the Rust
//! `StreamOptions` has no `env`, `signal`, `project` or `location` fields, so
//! project/location and extra Google knobs are read from `metadata` plus the
//! standard environment variables; service-account ADC token minting requires
//! an OAuth signer and is delegated to the caller via `access_token`.

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

use crate::event_stream::{AssistantMessageEventStream, EventSender};
use crate::providers::common::{
    apply_payload_hook, apply_response_hook, finalize, initial_output, response_metadata,
};
use crate::providers::google_shared::{build_request_body, consume_stream};
use crate::sse::read_sse_stream;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Model, StopReason, StreamOptions, TranscriptContext,
    now_ms,
};
use serde_json::Value;
use std::time::Duration;

/// Which of the two Google APIs a stream targets (TS `api` discriminator).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    GenerativeAi,
    Vertex,
}

/// `StreamFn` for models with `api: "google-generative-ai"` (TS
/// `google-generative-ai.ts:59-297`).
pub fn stream_google_generative_ai(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
) -> AssistantMessageEventStream {
    launch(Flavor::GenerativeAi, model, context, options)
}

/// `StreamFn` for models with `api: "google-vertex"` (TS
/// `google-vertex.ts:68-312`).
pub fn stream_google_vertex(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
) -> AssistantMessageEventStream {
    launch(Flavor::Vertex, model, context, options)
}

fn launch(
    flavor: Flavor,
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
) -> AssistantMessageEventStream {
    let (stream, sender) = AssistantMessageEventStream::channel();
    let model = model.clone();
    let context = context.clone();
    let options = options.clone();
    tokio::spawn(async move {
        run(flavor, &model, &context, &options, sender).await;
    });
    stream
}

async fn run(
    flavor: Flavor,
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
    sender: EventSender,
) {
    let mut output = initial_output(model);
    let started = output.timestamp;

    let result = request_and_stream(flavor, model, context, options, &mut output, &sender).await;
    finalize(&mut output, model);
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
            output.error_message = Some(message);
            let _ = sender
                .send(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: output,
                })
                .await;
        }
    }
}

async fn request_and_stream(
    flavor: Flavor,
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
    output: &mut AssistantMessage,
    sender: &EventSender,
) -> Result<(), String> {
    let url = endpoint_url(flavor, model, options)?;
    let auth = auth_headers(flavor, model, options)?;
    let body = build_request_body(model, context, options)?;
    let body = apply_payload_hook(&options.on_payload, body, model).await?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|error| format!("client build failed: {error}"))?;
    let mut request = client.post(&url).header("content-type", "application/json");
    for (name, value) in &auth {
        request = request.header(name.as_str(), value.as_str());
    }
    for (name, value) in &options.headers {
        if let Some(value) = value {
            request = request.header(name.as_str(), value.as_str());
        }
    }
    let response = request
        .json(&body)
        .send()
        .await
        .map_err(|error| format!("request failed: {error}"))?;

    // Upstream's SDK throws on non-2xx before the stream is returned, so the
    // `onResponse` observer only sees successful responses.
    if response.status().is_success() {
        apply_response_hook(&options.on_response, response_metadata(&response), model).await?;
    }
    let sse = read_sse_stream(response)
        .await
        .map_err(|error| error.to_string())?;
    let _ = sender
        .send(AssistantMessageEvent::Start {
            partial: output.clone(),
        })
        .await;
    consume_stream(sse, model, options, output, sender).await
}

/// Resolve credential headers (TS `createClient` in both adapters and
/// `resolveApiKey`/`buildGoogleAuthOptions` in `google-vertex.ts:431-446`).
fn auth_headers(
    flavor: Flavor,
    model: &Model,
    options: &StreamOptions,
) -> Result<Vec<(String, String)>, String> {
    match flavor {
        Flavor::GenerativeAi => {
            let key = options
                .api_key
                .clone()
                .or_else(|| std::env::var("GEMINI_API_KEY").ok())
                .or_else(|| std::env::var("GOOGLE_API_KEY").ok())
                .ok_or_else(|| format!("No API key for provider: {}", model.provider))?;
            Ok(vec![("x-goog-api-key".to_string(), key)])
        }
        Flavor::Vertex => {
            let api_key = options.api_key.as_deref().map(str::trim).filter(|key| {
                !key.is_empty() && !is_placeholder(key) && *key != "gcp-vertex-credentials"
            });
            if let Some(key) = api_key {
                return Ok(vec![("x-goog-api-key".to_string(), key.to_string())]);
            }
            if let Some(token) = options.access_token.as_deref()
                && !token.is_empty()
            {
                return Ok(vec![(
                    "authorization".to_string(),
                    format!("Bearer {token}"),
                )]);
            }
            Err(
                "Vertex AI requires credentials: set an API key or an OAuth access token"
                    .to_string(),
            )
        }
    }
}

fn is_placeholder(value: &str) -> bool {
    value.len() >= 2 && value.starts_with('<') && value.ends_with('>')
}

/// Read a Vertex setting from `StreamOptions.metadata` first, then the standard
/// environment variables (TS `options.project`/`options.location` plus
/// `getProviderEnvValue`). The lookup closure makes the precedence testable
/// without depending on the host environment.
fn meta_or_env_with(
    options: &StreamOptions,
    key: &str,
    envs: &[&str],
    lookup: &impl Fn(&str) -> Option<String>,
) -> Option<String> {
    options
        .metadata
        .as_ref()
        .and_then(|meta| meta.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| envs.iter().find_map(|name| lookup(name)))
}

/// Build the full `streamGenerateContent` URL, matching the SDK's `tModel` and
/// `ApiClient.constructUrl` (`_transformers.ts`, `_api_client.ts`).
fn endpoint_url(flavor: Flavor, model: &Model, options: &StreamOptions) -> Result<String, String> {
    endpoint_url_with(flavor, model, options, |name| std::env::var(name).ok())
}

fn endpoint_url_with(
    flavor: Flavor,
    model: &Model,
    options: &StreamOptions,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    match flavor {
        Flavor::GenerativeAi => {
            let base = if model.base_url.trim().is_empty() {
                "https://generativelanguage.googleapis.com/v1beta".to_string()
            } else {
                model.base_url.trim_end_matches('/').to_string()
            };
            Ok(format!(
                "{base}/{}:streamGenerateContent?alt=sse",
                t_model_generative_ai(&model.id)
            ))
        }
        Flavor::Vertex => {
            let project = meta_or_env_with(
                options,
                "project",
                &["GOOGLE_CLOUD_PROJECT", "GCLOUD_PROJECT"],
                &lookup,
            )
            .ok_or_else(|| {
                "Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT or pass project in options.".to_string()
            })?;
            let location = meta_or_env_with(
                options,
                "location",
                &["GOOGLE_CLOUD_LOCATION"],
                &lookup,
            )
            .ok_or_else(|| {
                "Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION or pass location in options.".to_string()
            })?;
            let base = vertex_base(model, &location);
            Ok(format!(
                "{base}/projects/{project}/locations/{location}/{}:streamGenerateContent?alt=sse",
                t_model_vertex(&model.id)
            ))
        }
    }
}

fn vertex_base(model: &Model, location: &str) -> String {
    let custom = model.base_url.trim();
    if !custom.is_empty() && !custom.contains("{location}") {
        let base = custom.trim_end_matches('/').to_string();
        return if has_api_version(&base) {
            base
        } else {
            format!("{base}/v1")
        };
    }
    let host = match location {
        "global" => "https://aiplatform.googleapis.com".to_string(),
        "us" | "eu" => format!("https://aiplatform.{location}.rep.googleapis.com"),
        _ => format!("https://{location}-aiplatform.googleapis.com"),
    };
    format!("{host}/v1")
}

fn has_api_version(base: &str) -> bool {
    base.split('/').any(|segment| {
        let Some(rest) = segment.strip_prefix('v') else {
            return false;
        };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return false;
        }
        let tail = &rest[digits.len()..];
        tail.is_empty()
            || tail
                .strip_prefix("beta")
                .is_some_and(|beta| beta.bytes().all(|byte| byte.is_ascii_digit()))
    })
}

fn t_model_generative_ai(id: &str) -> String {
    if id.starts_with("models/") || id.starts_with("tunedModels/") {
        id.to_string()
    } else {
        format!("models/{id}")
    }
}

fn t_model_vertex(id: &str) -> String {
    if id.starts_with("publishers/") || id.starts_with("projects/") || id.starts_with("models/") {
        return id.to_string();
    }
    if let Some((publisher, model)) = id.split_once('/') {
        return format!("publishers/{publisher}/models/{model}");
    }
    format!("publishers/google/models/{id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{StreamOptions, ThinkingLevel};
    use serde_json::json;

    fn model() -> Model {
        Model {
            id: "gemini-2.5-flash".into(),
            api: "google-generative-ai".into(),
            provider: "google".into(),
            ..Default::default()
        }
    }

    fn vertex_options() -> StreamOptions {
        let mut metadata = serde_json::Map::new();
        metadata.insert("project".to_string(), json!("proj-1"));
        metadata.insert("location".to_string(), json!("us-central1"));
        StreamOptions {
            metadata: Some(metadata),
            ..Default::default()
        }
    }

    #[test]
    fn generative_ai_endpoint_defaults_and_custom_base_url() {
        let mut model = model();
        let options = StreamOptions::default();
        assert_eq!(
            endpoint_url(Flavor::GenerativeAi, &model, &options).unwrap(),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
        model.base_url = "https://relay.example/v1beta/".into();
        assert_eq!(
            endpoint_url(Flavor::GenerativeAi, &model, &options).unwrap(),
            "https://relay.example/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn vertex_endpoint_and_model_path() {
        let mut model = model();
        model.api = "google-vertex".into();
        assert_eq!(
            endpoint_url(Flavor::Vertex, &model, &vertex_options()).unwrap(),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/proj-1/locations/us-central1/publishers/google/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
        model.id = "meta/llama-3.1".into();
        assert_eq!(
            t_model_vertex(&model.id),
            "publishers/meta/models/llama-3.1"
        );
        model.base_url = "https://discovery.example/v1".into();
        assert!(
            endpoint_url(Flavor::Vertex, &model, &vertex_options())
                .unwrap()
                .starts_with("https://discovery.example/v1/projects/")
        );
    }

    #[test]
    fn vertex_requires_project_and_location() {
        let model = Model {
            api: "google-vertex".into(),
            ..Default::default()
        };
        let error = endpoint_url_with(Flavor::Vertex, &model, &StreamOptions::default(), |_| None)
            .unwrap_err();
        assert!(error.contains("project ID"), "{error}");
        let mut metadata = serde_json::Map::new();
        metadata.insert("project".to_string(), json!("proj-1"));
        let options = StreamOptions {
            metadata: Some(metadata),
            ..Default::default()
        };
        let error = endpoint_url_with(Flavor::Vertex, &model, &options, |_| None).unwrap_err();
        assert!(error.contains("location"), "{error}");
    }

    #[test]
    fn auth_header_precedence() {
        let model = model();
        let options = StreamOptions {
            api_key: Some("ga-key".into()),
            access_token: Some("oauth".into()),
            reasoning: Some(ThinkingLevel::Low),
            ..Default::default()
        };
        assert_eq!(
            auth_headers(Flavor::GenerativeAi, &model, &options).unwrap(),
            vec![("x-goog-api-key".to_string(), "ga-key".to_string())]
        );
        assert_eq!(
            auth_headers(Flavor::Vertex, &model, &options).unwrap(),
            vec![("x-goog-api-key".to_string(), "ga-key".to_string())]
        );

        let options = StreamOptions {
            api_key: Some("<placeholder>".into()),
            access_token: Some("oauth-token".into()),
            ..Default::default()
        };
        assert_eq!(
            auth_headers(Flavor::Vertex, &model, &options).unwrap(),
            vec![(
                "authorization".to_string(),
                "Bearer oauth-token".to_string()
            )]
        );
        let options = StreamOptions {
            api_key: Some("gcp-vertex-credentials".into()),
            ..Default::default()
        };
        assert_eq!(
            auth_headers(Flavor::Vertex, &model, &options).unwrap_err(),
            "Vertex AI requires credentials: set an API key or an OAuth access token"
        );
    }

    #[test]
    fn has_api_version_detection() {
        assert!(has_api_version("https://host/v1"));
        assert!(has_api_version("https://host/v1beta"));
        assert!(has_api_version("https://host/v1beta2/"));
        assert!(!has_api_version("https://host/vertex-ai"));
    }
}
