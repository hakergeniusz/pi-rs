//! Azure OpenAI endpoint resolution — port of TS
//! `packages/ai/src/api/azure-openai-config.ts:1-107` (plus the API-key lookup
//! in `azure-openai-responses.ts:70-75,132-139`).
//!
//! Azure models ship without a `baseUrl`, so the resource, deployment, API
//! version and base URL are resolved per request from model/options/env. The
//! port has no `AzureEndpointOptions` on `StreamOptions` (it cannot edit the
//! shared type), so the four `azure*` fields ride in `StreamOptions::metadata`
//! under their upstream TS names; environment variables are consulted exactly
//! as upstream's `getProviderEnvValue` does, minus its scoped `env` override
//! (the port has no `ProviderEnv` field).

use crate::types::{Model, StreamOptions};
use serde_json::Value;
use std::collections::BTreeMap;

/// Upstream `DEFAULT_AZURE_API_VERSION` (`azure-openai-config.ts:4`).
pub(super) const DEFAULT_AZURE_API_VERSION: &str = "v1";

/// Read a provider-specific string option from `StreamOptions::metadata`, or
/// from the process environment under the upstream env-var name.
fn option<'a>(options: &'a StreamOptions, key: &str) -> Option<&'a str> {
    options
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(key))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn option_or_env(options: &StreamOptions, key: &str) -> Option<String> {
    option(options, key)
        .map(str::to_string)
        .or_else(|| env_value(key))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// `parseDeploymentNameMap` (`azure-openai-config.ts:14-25`): comma-separated
/// `modelId=deployment` entries.
fn parse_deployment_name_map(value: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for entry in value.split(',') {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Upstream uses `split("=", 2)` (JS limit), so a third `=` stays in
        // the deployment name.
        let mut parts = trimmed.split('=');
        let (Some(model_id), Some(deployment)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (model_id, deployment) = (model_id.trim(), deployment.trim());
        if model_id.is_empty() || deployment.is_empty() {
            continue;
        }
        map.insert(model_id.to_string(), deployment.to_string());
    }
    map
}

/// `resolveDeploymentName` (`azure-openai-config.ts:27-35`).
pub(super) fn resolve_deployment_name(model: &Model, options: &StreamOptions) -> String {
    if let Some(name) = option_or_env(options, "azureDeploymentName") {
        return name;
    }
    let map = parse_deployment_name_map(
        &option_or_env(options, "AZURE_OPENAI_DEPLOYMENT_NAME_MAP").unwrap_or_default(),
    );
    map.get(&model.id)
        .cloned()
        .unwrap_or_else(|| model.id.clone())
}

/// `buildDefaultBaseUrl` (`azure-openai-config.ts:68-70`).
fn build_default_base_url(resource_name: &str) -> String {
    format!("https://{resource_name}.openai.azure.com/openai/v1")
}

/// `normalizeAzureBaseUrl` (`azure-openai-config.ts:37-66`). The port has no
/// `url` dependency, so the small subset of WHATWG URL normalization pi needs
/// is implemented directly (scheme/host/path split, trailing-slash stripping,
/// and the Azure-host `/openai/v1` base-path fix-up). Query strings and
/// fragments on non-Azure hosts are preserved verbatim.
fn normalize_azure_base_url(base_url: &str) -> Result<String, String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let invalid = || format!("Invalid Azure OpenAI base URL: {base_url}");
    let (scheme, rest) = trimmed.split_once("://").ok_or_else(invalid)?;
    if scheme.is_empty() || rest.is_empty() {
        return Err(invalid());
    }
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..host_end];
    if host.is_empty() {
        return Err(invalid());
    }
    let suffix = &rest[host_end..];
    let host_lower = host.to_ascii_lowercase();
    let is_azure_host = host_lower.ends_with(".openai.azure.com")
        || host_lower.ends_with(".cognitiveservices.azure.com")
        || host_lower.ends_with(".ai.azure.com");
    let normalized_path = suffix
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/');
    if is_azure_host
        && matches!(
            normalized_path,
            "" | "/" | "/openai" | "/openai/v1/responses"
        )
    {
        return Ok(format!("{scheme}://{host}/openai/v1"));
    }
    Ok(trimmed.to_string())
}

/// `resolveAzureBaseUrl` (`azure-openai-config.ts:72-94`).
pub(super) fn resolve_azure_base_url(
    model: &Model,
    options: &StreamOptions,
) -> Result<String, String> {
    let base_url = option_or_env(options, "azureBaseUrl")
        .or_else(|| option_or_env(options, "AZURE_OPENAI_BASE_URL"));
    let resource_name = option_or_env(options, "azureResourceName")
        .or_else(|| option_or_env(options, "AZURE_OPENAI_RESOURCE_NAME"));
    let resolved = base_url
        .or_else(|| resource_name.map(|resource| build_default_base_url(&resource)))
        .or_else(|| {
            let model_base = model.base_url.trim();
            (!model_base.is_empty()).then(|| model_base.to_string())
        })
        .ok_or_else(|| {
            "Azure OpenAI base URL is required. Set AZURE_OPENAI_BASE_URL or \
             AZURE_OPENAI_RESOURCE_NAME, or pass azureBaseUrl, azureResourceName, or model.baseUrl."
                .to_string()
        })?;
    normalize_azure_base_url(&resolved)
}

/// `resolveAzureConfig` (`azure-openai-config.ts:96-107`): `(baseUrl, apiVersion)`.
pub(super) fn resolve_azure_config(
    model: &Model,
    options: &StreamOptions,
) -> Result<(String, String), String> {
    let base_url = resolve_azure_base_url(model, options)?;
    let api_version = option_or_env(options, "azureApiVersion")
        .or_else(|| option_or_env(options, "AZURE_OPENAI_API_VERSION"))
        .unwrap_or_else(|| DEFAULT_AZURE_API_VERSION.to_string());
    Ok((base_url, api_version))
}

/// Azure API key (`azure-openai-responses.ts:70-75,132-139`): options first,
/// then `AZURE_OPENAI_API_KEY`. `accessToken` is accepted as a stand-in for a
/// caller-supplied `apiKey` so the shared `StreamOptions` can carry either
/// credential shape.
pub(super) fn resolve_azure_api_key(
    model: &Model,
    options: &StreamOptions,
) -> Result<String, String> {
    options
        .api_key
        .clone()
        .or_else(|| options.access_token.clone())
        .or_else(|| option_or_env(options, "azureApiKey"))
        .ok_or_else(|| format!("No API key for provider: {}", model.provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn azure_model() -> Model {
        Model {
            id: "gpt-test".into(),
            api: "azure-openai-responses".into(),
            provider: "azure".into(),
            ..Model::default()
        }
    }

    #[test]
    fn deployment_name_precedence_and_env_map() {
        let model = azure_model();
        let mut options = StreamOptions::default();
        options.metadata = Some(
            serde_json::from_str(
                r#"{"AZURE_OPENAI_DEPLOYMENT_NAME_MAP":"gpt-test=deploy-a, other=deploy-b"}"#,
            )
            .unwrap(),
        );
        assert_eq!(resolve_deployment_name(&model, &options), "deploy-a");
        options.metadata.as_mut().unwrap().insert(
            "azureDeploymentName".to_string(),
            Value::String("explicit".to_string()),
        );
        assert_eq!(resolve_deployment_name(&model, &options), "explicit");
    }

    #[test]
    fn base_url_normalization() {
        assert_eq!(
            normalize_azure_base_url("https://myres.openai.azure.com/").unwrap(),
            "https://myres.openai.azure.com/openai/v1"
        );
        assert_eq!(
            normalize_azure_base_url("https://myres.openai.azure.com/openai/v1/responses").unwrap(),
            "https://myres.openai.azure.com/openai/v1"
        );
        assert_eq!(
            normalize_azure_base_url("https://custom.example/v1").unwrap(),
            "https://custom.example/v1"
        );
        assert!(normalize_azure_base_url("not a url").is_err());
    }

    #[test]
    fn config_resolves_resource_name_and_api_version() {
        let mut model = azure_model();
        model.base_url.clear();
        let mut options = StreamOptions::default();
        options.metadata = Some(
            serde_json::from_str(r#"{"azureResourceName":"myres","azureApiVersion":"2024-10-21"}"#)
                .unwrap(),
        );
        let (base, version) = resolve_azure_config(&model, &options).unwrap();
        assert_eq!(base, "https://myres.openai.azure.com/openai/v1");
        assert_eq!(version, "2024-10-21");

        options.metadata.as_mut().unwrap().remove("azureApiVersion");
        let (_, version) = resolve_azure_config(&model, &options).unwrap();
        assert_eq!(version, DEFAULT_AZURE_API_VERSION);
    }

    #[test]
    fn config_falls_back_to_model_base_url() {
        let mut model = azure_model();
        model.base_url = "https://myres.openai.azure.com".into();
        let options = StreamOptions::default();
        assert_eq!(
            resolve_azure_base_url(&model, &options).unwrap(),
            "https://myres.openai.azure.com/openai/v1"
        );
        model.base_url.clear();
        assert!(resolve_azure_base_url(&model, &options).is_err());
    }
}
