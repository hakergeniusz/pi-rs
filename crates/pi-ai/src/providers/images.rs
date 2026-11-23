//! Image-generation wire path (port of TS `api/openrouter-images.ts`,
//! `providers/images/register-builtins.ts`, `images-api-registry.ts`,
//! `images.ts` and the image types in `types.ts:78-80,345-353,382,412-416,
//! 672-687,900-903`).
//!
//! `OpenRouter` generates images through the ordinary chat-completions endpoint:
//! `POST {baseUrl}/chat/completions` with one user message whose content parts
//! are `text` / `image_url` blocks and `modalities: ["image", "text"]` (or
//! `["image"]` for image-only models). The response is a chat completion whose
//! first choice carries optional text plus `images[].image_url` data URLs.
//!
//! Public surface: [`generate_images`] dispatches on `ImageModel.api` through
//! the built-in registry (only `openrouter-images` is built in upstream);
//! [`build_params`], [`parse_usage`] and [`parse_response`] expose the typed
//! request/response path without a live server.
//!
//! NOT ported (deliberate scope tails):
//! - Upstream's `openai` SDK client, `onPayload`/`onResponse` hooks, injected
//!   `fetch`, abort signals and `sanitizeSurrogates`. Requests are built as
//!   plain JSON and sent with reqwest; `metadata` is accepted in options but
//!   not forwarded (upstream providers ignore unknown metadata fields too).
//! - `createProvider({ images })` / `registerImagesApiProvider` extension
//!   registration: Rust callers dispatch by `model.api` string instead. Custom
//!   image APIs therefore return an error result rather than being pluggable.
//! - No other image API is built in upstream 1.5.2, so no other provider arm
//!   exists here.

use crate::cost::calculate_cost;
use crate::providers::common::{JsonPostOptions, post_json_with_retry};
use crate::types::{
    ImageContent, ImageModel, InputContent, JsonObject, TextContent, Usage, now_ms,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const LABEL: &str = "OpenRouter Images";

/// One input block: text or image (TS `ImagesInputContent`, `types.ts:672`).
pub type ImagesInputContent = InputContent;
/// One output block: text or image (TS `ImagesOutputContent`, `types.ts:673`).
pub type ImagesOutputContent = InputContent;

/// Generation input (TS `ImagesContext`, `types.ts:675-677`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImagesContext {
    pub input: Vec<ImagesInputContent>,
}

/// Image-generation stop reasons (TS `ImagesStopReason`, `types.ts:679`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ImagesStopReason {
    Stop,
    Error,
    Aborted,
}

impl ImagesStopReason {
    /// Wire name (TS string literal).
    pub fn as_str(self) -> &'static str {
        match self {
            ImagesStopReason::Stop => "stop",
            ImagesStopReason::Error => "error",
            ImagesStopReason::Aborted => "aborted",
        }
    }
}

/// The normalized generation result (TS `AssistantImages`, `types.ts:678-687`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantImages {
    pub api: String,
    pub provider: String,
    pub model: String,
    pub output: Vec<ImagesOutputContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub stop_reason: ImagesStopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: i64,
}

impl AssistantImages {
    /// Error result used when generation could not start or failed
    /// (TS `imageErrorResult`, `utils/model-operations.ts:55-65`).
    pub fn error_result(model: &ImageModel, message: impl Into<String>, aborted: bool) -> Self {
        Self {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            output: Vec::new(),
            response_id: None,
            usage: None,
            stop_reason: if aborted {
                ImagesStopReason::Aborted
            } else {
                ImagesStopReason::Error
            },
            error_message: Some(message.into()),
            timestamp: now_ms(),
        }
    }
}

/// Per-call image options (TS `ImagesOptions`, `types.ts:374-382`).
#[derive(Debug, Clone, Default)]
pub struct ImagesOptions {
    pub api_key: Option<String>,
    /// Upstream image generation passes 0 by default.
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub headers: BTreeMap<String, Option<String>>,
    pub metadata: Option<JsonObject>,
}

/// Dispatch image generation by `ImageModel.api` (TS `generateImages`,
/// `images.ts:11-22`). Unsupported APIs return an error result.
pub async fn generate_images(
    model: &ImageModel,
    context: &ImagesContext,
    options: &ImagesOptions,
) -> AssistantImages {
    match model.api.as_str() {
        "openrouter-images" => generate_images_openrouter(model, context, options).await,
        other => AssistantImages::error_result(
            model,
            format!("No API provider registered for api: {other}"),
            false,
        ),
    }
}

/// Build the chat-completions request body (`openrouter-images.ts:133-158`).
pub fn build_params(model: &ImageModel, context: &ImagesContext) -> Value {
    let parts: Vec<Value> = context
        .input
        .iter()
        .map(|item| match item {
            InputContent::Text(text) => json!({ "type": "text", "text": text.text }),
            InputContent::Image(image) => json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{};base64,{}", image.mime_type, image.data) },
            }),
        })
        .collect();
    let modalities: Vec<&str> = if model.output.iter().any(|output| output.as_str() == "text") {
        vec!["image", "text"]
    } else {
        vec!["image"]
    };
    json!({
        "model": model.id,
        "messages": [{ "role": "user", "content": parts }],
        "stream": false,
        "modalities": modalities,
    })
}

/// Upstream `OpenAI` SDK appends `/chat/completions` to `baseUrl`.
pub fn images_url(model: &ImageModel) -> String {
    format!("{}/chat/completions", model.base_url.trim_end_matches('/'))
}

/// Parse a `data:<mime>;base64,<data>` URL (the upstream regex at
/// `openrouter-images.ts:89-92`). Non-data URLs are ignored.
fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (mime, data) = rest.split_once(";base64,")?;
    (!mime.is_empty() && !data.is_empty()).then(|| (mime.to_string(), data.to_string()))
}

fn number_or_zero(value: Option<&Value>) -> u64 {
    value
        .and_then(Value::as_f64)
        .map_or(0, |number| number as u64)
}

/// Usage from the chat-completions response (`openrouter-images.ts:162-195`):
/// `OpenRouter` reports cached and cache-write tokens inside `prompt_tokens`,
/// so both are split out before pricing at the catalog rate.
pub fn parse_usage(raw: Option<&Value>, model: &ImageModel) -> Option<Usage> {
    let raw = raw?;
    if !raw.is_object() {
        return None;
    }
    let details = raw.get("prompt_tokens_details");
    let prompt_tokens = number_or_zero(raw.get("prompt_tokens"));
    let reported_cached = number_or_zero(details.and_then(|value| value.get("cached_tokens")));
    let cache_write = number_or_zero(details.and_then(|value| value.get("cache_write_tokens")));
    let cache_read = if cache_write > 0 {
        reported_cached.saturating_sub(cache_write)
    } else {
        reported_cached
    };
    let input = prompt_tokens
        .saturating_sub(cache_read)
        .saturating_sub(cache_write);
    let output = number_or_zero(raw.get("completion_tokens"));
    let mut usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: input + output + cache_read + cache_write,
        ..Default::default()
    };
    calculate_cost(&mut usage, &model.cost);
    usage.cost.total_from_provider = Some(false);
    Some(usage)
}

/// Extract the response id, usage and output blocks from a chat completion
/// (`openrouter-images.ts:80-103`). Answer arrays may be absent; a bad image
/// URL is skipped like upstream.
pub fn parse_response(
    value: &Value,
    model: &ImageModel,
) -> (Option<String>, Option<Usage>, Vec<ImagesOutputContent>) {
    let response_id = value.get("id").and_then(Value::as_str).map(str::to_string);
    let usage = parse_usage(value.get("usage"), model);
    let mut output = Vec::new();
    if let Some(choice) = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    {
        let message = choice.get("message");
        if let Some(text) = message
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            && !text.is_empty()
        {
            output.push(InputContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            }));
        }
        if let Some(images) = message
            .and_then(|message| message.get("images"))
            .and_then(Value::as_array)
        {
            for image in images {
                let image_url = match image.get("image_url") {
                    Some(Value::String(url)) => Some(url.as_str()),
                    Some(Value::Object(object)) => object.get("url").and_then(Value::as_str),
                    _ => None,
                };
                let Some((mime_type, data)) = image_url.and_then(parse_data_url) else {
                    continue;
                };
                output.push(InputContent::Image(ImageContent { data, mime_type }));
            }
        }
    }
    (response_id, usage, output)
}

/// `OpenRouter`'s chat-completions image generation arm.
pub async fn generate_images_openrouter(
    model: &ImageModel,
    context: &ImagesContext,
    options: &ImagesOptions,
) -> AssistantImages {
    let mut output = AssistantImages {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        output: Vec::new(),
        response_id: None,
        usage: None,
        stop_reason: ImagesStopReason::Stop,
        error_message: None,
        timestamp: now_ms(),
    };
    let result = async {
        let api_key = options
            .api_key
            .as_deref()
            .ok_or_else(|| format!("No API key for provider: {}", model.provider))?;
        let body = build_params(model, context);
        let post_options = JsonPostOptions {
            api_key: Some(api_key.to_string()),
            max_retries: options.max_retries.unwrap_or(0),
            max_retry_delay_ms: options.max_retry_delay_ms,
            headers: options.headers.clone(),
            no_retry_statuses: Vec::new(),
        };
        let response = post_json_with_retry(
            LABEL,
            &images_url(model),
            model.headers.as_ref(),
            &post_options,
            &body,
        )
        .await?;
        Ok::<(Option<String>, Option<Usage>, Vec<ImagesOutputContent>), String>(parse_response(
            &response, model,
        ))
    }
    .await;
    match result {
        Ok((response_id, usage, blocks)) => {
            output.response_id = response_id;
            output.usage = usage;
            output.output = blocks;
        }
        Err(message) => {
            output.stop_reason = ImagesStopReason::Error;
            output.error_message = Some(message);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelCost, ModelInputModality, ModelOutputModality};

    fn model(output: Vec<ModelOutputModality>) -> ImageModel {
        ImageModel {
            id: "google/gemini-2.5-flash-image".into(),
            name: "Gemini 2.5 Flash Image".into(),
            api: "openrouter-images".into(),
            provider: "openrouter".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            input: vec![ModelInputModality::Text, ModelInputModality::Image],
            input_limits: None,
            cost: ModelCost {
                input: 0.015,
                output: 0.03,
                ..Default::default()
            },
            headers: Some(BTreeMap::from([(
                "HTTP-Referer".into(),
                "https://example.com".into(),
            )])),
            r#type: "image".into(),
            output,
        }
    }

    fn context() -> ImagesContext {
        ImagesContext {
            input: vec![InputContent::Text(TextContent {
                text: "Generate a dog".into(),
                text_signature: None,
            })],
        }
    }

    #[test]
    fn request_wire_shape_and_url() {
        let params = build_params(
            &model(vec![ModelOutputModality::Image, ModelOutputModality::Text]),
            &context(),
        );
        assert_eq!(
            params,
            json!({
                "model": "google/gemini-2.5-flash-image",
                "messages": [{"role": "user", "content": [{"type": "text", "text": "Generate a dog"}]}],
                "stream": false,
                "modalities": ["image", "text"],
            })
        );
        assert_eq!(
            images_url(&model(vec![ModelOutputModality::Image])),
            "https://openrouter.ai/api/v1/chat/completions"
        );

        let mut context = context();
        context.input.push(InputContent::Image(ImageContent {
            data: "aW1hZ2U=".into(),
            mime_type: "image/png".into(),
        }));
        let params = build_params(&model(vec![ModelOutputModality::Image]), &context);
        assert_eq!(
            params.pointer("/messages/0/content/1").unwrap(),
            &json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,aW1hZ2U="}})
        );
        assert_eq!(params.pointer("/modalities").unwrap(), &json!(["image"]));
    }

    #[test]
    fn parses_text_images_usage_and_response_id() {
        let response: Value = serde_json::from_str(
            r#"{
                "id": "img-1",
                "usage": {"prompt_tokens":12,"completion_tokens":34,
                          "prompt_tokens_details":{"cached_tokens":2,"cache_write_tokens":5}},
                "choices": [{"message":{
                    "content":"Here is your image.",
                    "images":[{"image_url":"data:image/png;base64,ZmFrZS1wbmc="},
                              {"image_url":{"url":"data:image/jpeg;base64,ZmFrZS1qcGVn"}},
                              {"image_url":"https://example.com/not-a-data-url"}]
                }}]
            }"#,
        )
        .unwrap();
        let model = model(vec![ModelOutputModality::Image, ModelOutputModality::Text]);
        let (response_id, usage, output) = parse_response(&response, &model);
        assert_eq!(response_id.as_deref(), Some("img-1"));
        assert_eq!(
            output,
            vec![
                InputContent::Text(TextContent {
                    text: "Here is your image.".into(),
                    text_signature: None,
                }),
                InputContent::Image(ImageContent {
                    mime_type: "image/png".into(),
                    data: "ZmFrZS1wbmc=".into(),
                }),
                InputContent::Image(ImageContent {
                    mime_type: "image/jpeg".into(),
                    data: "ZmFrZS1qcGVn".into(),
                }),
            ]
        );
        let usage = usage.unwrap();
        // 12 prompt tokens: 5 cache writes, 2 cached (so 0 cache reads), 7 input.
        assert_eq!(usage.input, 7);
        assert_eq!(usage.cache_write, 5);
        assert_eq!(usage.cache_read, 0);
        assert_eq!(usage.output, 34);
        assert_eq!(usage.total_tokens, 46);
        assert!((usage.cost.input - 0.015 / 1_000_000.0 * 7.0).abs() < 1e-15);
        assert_eq!(usage.cost.total_from_provider, Some(false));
    }

    #[tokio::test]
    async fn rejects_unknown_api_and_missing_key_without_http() {
        let other = ImageModel {
            api: "unknown-images".into(),
            ..model(vec![ModelOutputModality::Image])
        };
        let result = generate_images(&other, &context(), &ImagesOptions::default()).await;
        assert_eq!(result.stop_reason, ImagesStopReason::Error);
        assert!(
            result
                .error_message
                .unwrap()
                .contains("No API provider registered")
        );

        let result = generate_images_openrouter(
            &model(vec![ModelOutputModality::Image]),
            &context(),
            &ImagesOptions::default(),
        )
        .await;
        assert_eq!(result.stop_reason, ImagesStopReason::Error);
        assert!(
            result
                .error_message
                .unwrap()
                .contains("No API key for provider")
        );
    }
}
