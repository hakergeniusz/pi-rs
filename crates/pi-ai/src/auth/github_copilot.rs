//! GitHub Copilot device-code OAuth (`packages/ai/src/auth/oauth/github-copilot.ts`).
//!
//! Ground truth: client id and Copilot headers (`:11-20`), domain/URL/token
//! helpers (`:42-88`), model catalogue parsing (`:90-134`), device flow
//! (`:207-305`), Copilot token refresh (`:307-361`), model enabling
//! (`:363-426`), login (`:428-479`) and `githubCopilotOAuth` (`:487-501`).
//!
//! Deviations: the 429 back-off and best-effort policy-enable POSTs are not
//! ported (they only widen an already-authenticated model catalogue); the
//! catalogue is fetched once with the same headers and parsed with the same
//! picker/policy rules. Enterprise URLs are only synthesized for non-default
//! domains; tests substitute the loopback endpoint table for `github.com`.

use serde_json::{Value, json};

use super::CANCEL_MESSAGE;
use super::device_code::{
    DeviceCodePollOptions, DeviceCodePollResult, poll_oauth_device_code_flow,
};
use super::http::{form_encode, normalize_domain, send};
use super::trusted_url::trusted_http_url;
use super::{
    AuthError, AuthEvent, AuthPrompt, CancelToken, EndpointConfig, FlowConfig, Interaction,
    OAuthCredential,
};

const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const CLIENT_ID_ENCODED: &str = "SXYxLmI1MDdhMDhjODdlY2ZlOTg=";
const COPILOT_API_VERSION: &str = "2026-06-01";
const USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
const FIVE_MINUTES_MS: i64 = 5 * 60 * 1000;
const DEFAULT_BASE_URL: &str = "https://api.individual.githubcopilot.com";

/// `getUrls` (`github-copilot.ts:53-63`).
fn get_urls(domain: &str, endpoints: &EndpointConfig) -> (String, String, String) {
    if domain == "github.com" {
        return (
            endpoints.device_user_code_url.clone(),
            endpoints.device_token_url.clone(),
            endpoints.token_url.clone(),
        );
    }
    (
        format!("https://{domain}/login/device/code"),
        format!("https://{domain}/login/oauth/access_token"),
        format!("https://api.{domain}/copilot_internal/v2/token"),
    )
}

/// `getBaseUrlFromToken` + `getGitHubCopilotBaseUrl` (`:70-88`).
#[must_use]
pub fn base_url_from_token(token: &str, enterprise_domain: Option<&str>) -> Option<String> {
    let proxy = token
        .split(';')
        .find_map(|part| part.strip_prefix("proxy-ep="))
        .filter(|host| !host.is_empty());
    if let Some(proxy) = proxy {
        let api_host = proxy
            .strip_prefix("proxy.")
            .map_or_else(|| proxy.to_string(), |rest| format!("api.{rest}"));
        return Some(format!("https://{api_host}"));
    }
    enterprise_domain.map_or_else(
        || Some(DEFAULT_BASE_URL.to_string()),
        |domain| Some(format!("https://copilot-api.{domain}")),
    )
}

#[must_use]
fn base_url(token: &str, enterprise_domain: Option<&str>) -> String {
    base_url_from_token(token, enterprise_domain).unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

/// `fetchJson`'s non-ok branch (`:198-205`).
fn http_error(status: reqwest::StatusCode, text: String) -> AuthError {
    let reason = status.canonical_reason().unwrap_or_default();
    AuthError::Message(format!("{} {reason}: {text}", status.as_u16()))
}

async fn get_json(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
    cancel: &CancelToken,
) -> Result<Value, AuthError> {
    let mut request = client.get(url).header("Accept", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = send(request, cancel).await?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| AuthError::Message(error.to_string()))?;
    if !status.is_success() {
        return Err(http_error(status, text));
    }
    serde_json::from_str::<Value>(&text).map_err(|error| AuthError::Message(error.to_string()))
}

#[derive(Debug, Clone, PartialEq)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval_seconds: Option<f64>,
    expires_in_seconds: u64,
}

/// `startDeviceFlow` (`github-copilot.ts:207-255`).
async fn start_device_flow(
    domain: &str,
    endpoints: &EndpointConfig,
    client: &reqwest::Client,
    cancel: &CancelToken,
) -> Result<DeviceCodeResponse, AuthError> {
    let (device_code_url, _, _) = get_urls(domain, endpoints);
    let body = form_encode(&[("client_id", CLIENT_ID), ("scope", "read:user")]);
    let response = send(
        client
            .post(&device_code_url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", USER_AGENT)
            .body(body),
        cancel,
    )
    .await?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| AuthError::Message(error.to_string()))?;
    if !status.is_success() {
        return Err(http_error(status, text));
    }
    let value = serde_json::from_str::<Value>(&text)
        .map_err(|_| AuthError::Message("Invalid device code response".to_string()))?;
    let device_code = value.get("device_code").and_then(Value::as_str);
    let user_code = value.get("user_code").and_then(Value::as_str);
    let verification_uri = value.get("verification_uri").and_then(Value::as_str);
    let interval = value.get("interval").and_then(Value::as_f64);
    let expires_in = value.get("expires_in").and_then(Value::as_u64);
    if device_code.is_none()
        || user_code.is_none()
        || verification_uri.is_none()
        || expires_in.is_none()
    {
        return Err(AuthError::Message(
            "Invalid device code response fields".to_string(),
        ));
    }
    let verification_uri = verification_uri.expect("verification uri");
    let trusted = trusted_http_url(verification_uri).ok_or_else(|| {
        AuthError::Message("Untrusted verification_uri in device code response".to_string())
    })?;
    Ok(DeviceCodeResponse {
        device_code: device_code.expect("device code").to_string(),
        user_code: user_code.expect("user code").to_string(),
        verification_uri: trusted,
        interval_seconds: interval,
        expires_in_seconds: expires_in.expect("expires in"),
    })
}

/// `pollForGitHubAccessToken` (`github-copilot.ts:257-305`).
async fn poll_for_access_token(
    domain: &str,
    device: &DeviceCodeResponse,
    endpoints: &EndpointConfig,
    client: &reqwest::Client,
    cancel: &CancelToken,
) -> Result<String, AuthError> {
    let (_, access_token_url, _) = get_urls(domain, endpoints);
    let mut options = DeviceCodePollOptions::new(cancel, || async {
        let body = form_encode(&[
            ("client_id", CLIENT_ID),
            ("device_code", &device.device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ]);
        let response = send(
            client
                .post(&access_token_url)
                .header("Accept", "application/json")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("User-Agent", USER_AGENT)
                .body(body),
            cancel,
        )
        .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => return DeviceCodePollResult::Failed(error.to_string()),
        };
        let text = match response.text().await {
            Ok(text) => text,
            Err(error) => return DeviceCodePollResult::Failed(error.to_string()),
        };
        let value = match serde_json::from_str::<Value>(&text) {
            Ok(value) => value,
            Err(error) => return DeviceCodePollResult::Failed(error.to_string()),
        };
        if let Some(access_token) = value.get("access_token").and_then(Value::as_str) {
            return DeviceCodePollResult::Complete(access_token.to_string());
        }
        let Some(error) = value.get("error").and_then(Value::as_str) else {
            return DeviceCodePollResult::Failed("Invalid device token response".to_string());
        };
        let description = value.get("error_description").and_then(Value::as_str);
        let interval = value.get("interval").and_then(Value::as_f64);
        match error {
            "authorization_pending" => DeviceCodePollResult::Pending,
            "slow_down" => DeviceCodePollResult::SlowDown {
                interval_seconds: interval,
            },
            other => {
                let suffix = description.map_or_else(String::new, |text| format!(": {text}"));
                DeviceCodePollResult::Failed(format!("Device flow failed: {other}{suffix}"))
            }
        }
    });
    options.interval_seconds = device.interval_seconds;
    options.expires_in_seconds = Some(device.expires_in_seconds);
    options.wait_before_first_poll = true;
    poll_oauth_device_code_flow(options).await
}

/// `parseGitHubCopilotModelCatalog` (`:94-134`) — only the picker-enabled
/// ids; enabled-by-policy models come from the policy endpoint upstream.
fn model_ids(value: &Value) -> Result<Vec<String>, AuthError> {
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| AuthError::Message("Invalid Copilot models response".to_string()))?;
    Ok(data
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?;
            if item
                .get("capabilities")
                .and_then(|capabilities| capabilities.get("supports"))
                .and_then(|supports| supports.get("tool_calls"))
                .and_then(Value::as_bool)
                == Some(false)
            {
                return None;
            }
            let picker_enabled =
                item.get("model_picker_enabled").and_then(Value::as_bool) == Some(true);
            let disabled = item
                .get("policy")
                .and_then(|policy| policy.get("state"))
                .and_then(Value::as_str)
                == Some("disabled");
            (picker_enabled && !disabled).then(|| id.to_string())
        })
        .collect())
}

/// `fetchGitHubCopilotModels` (`github-copilot.ts:169-196`) with the
/// retry policy omitted (see module docs).
async fn fetch_models(
    token: &str,
    enterprise_domain: Option<&str>,
    endpoints: &EndpointConfig,
    client: &reqwest::Client,
    cancel: &CancelToken,
) -> Result<Vec<String>, AuthError> {
    let url = format!("{}/models", base_url(token, enterprise_domain));
    // Only the `github.com` default endpoint is overridable in tests.
    let url = if base_url(token, enterprise_domain) == DEFAULT_BASE_URL {
        endpoints
            .token_url
            .strip_suffix("/copilot_internal/v2/token")
            .map_or(url.clone(), |prefix| format!("{prefix}models"))
    } else {
        url
    };
    let value = get_json(
        client,
        &url,
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("User-Agent", USER_AGENT),
            ("Editor-Version", "vscode/1.107.0"),
            ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
            ("Copilot-Integration-Id", "vscode-chat"),
            ("X-GitHub-Api-Version", COPILOT_API_VERSION),
        ],
        cancel,
    )
    .await?;
    model_ids(&value)
}

/// `refreshGitHubCopilotAccessToken` (`github-copilot.ts:307-342`).
async fn refresh_access_token(
    refresh_token: &str,
    enterprise_domain: Option<&str>,
    endpoints: &EndpointConfig,
    client: &reqwest::Client,
    cancel: &CancelToken,
) -> Result<OAuthCredential, AuthError> {
    let (_, _, token_url) = get_urls("github.com", endpoints);
    let value = get_json(
        client,
        &token_url,
        &[
            ("Authorization", &format!("Bearer {refresh_token}")),
            ("User-Agent", USER_AGENT),
            ("Editor-Version", "vscode/1.107.0"),
            ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
            ("Copilot-Integration-Id", "vscode-chat"),
        ],
        cancel,
    )
    .await?;
    let token = value.get("token").and_then(Value::as_str);
    let expires_at = value.get("expires_at").and_then(Value::as_f64);
    if token.is_none() || expires_at.is_none() {
        return Err(AuthError::Message(
            "Invalid Copilot token response fields".to_string(),
        ));
    }
    let expires = (expires_at.expect("expires at") * 1000.0) as i64 - FIVE_MINUTES_MS;
    Ok(OAuthCredential {
        access: token.expect("token").to_string(),
        refresh: refresh_token.to_string(),
        expires,
        account_id: None,
        enterprise_url: enterprise_domain.map(str::to_string),
        available_model_ids: Vec::new(),
    })
}

/// `refreshGitHubCopilotToken` (`:347-361`).
///
/// # Errors
/// HTTP/JSON errors from the token and model endpoints.
pub async fn refresh(
    refresh_token: &str,
    enterprise_domain: Option<&str>,
    cancel: &CancelToken,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let mut credential = refresh_access_token(
        refresh_token,
        enterprise_domain,
        &config.github_copilot,
        &config.client,
        cancel,
    )
    .await?;
    credential.available_model_ids = fetch_models(
        &credential.access,
        enterprise_domain,
        &config.github_copilot,
        &config.client,
        cancel,
    )
    .await?;
    Ok(credential)
}

/// `loginGitHubCopilot` (`github-copilot.ts:428-479`).
///
/// # Errors
/// Invalid enterprise domains, untrusted verification URLs, device flow
/// failures and Copilot token/model errors.
pub async fn login(
    interaction: &Interaction,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let input = interaction
        .prompt(AuthPrompt::Text {
            message: "GitHub Enterprise URL/domain (blank for github.com)".to_string(),
            placeholder: Some("company.ghe.com".to_string()),
        })
        .await?;
    if interaction.cancellation().is_cancelled() {
        return Err(AuthError::Message(CANCEL_MESSAGE.to_string()));
    }
    let trimmed = input.trim();
    let enterprise_domain = normalize_domain(&input);
    if !trimmed.is_empty() && enterprise_domain.is_none() {
        return Err(AuthError::Message(
            "Invalid GitHub Enterprise URL/domain".to_string(),
        ));
    }
    let domain = enterprise_domain.as_deref().unwrap_or("github.com");
    let device = start_device_flow(
        domain,
        &config.github_copilot,
        &config.client,
        &interaction.cancellation(),
    )
    .await?;
    interaction.notify(AuthEvent::DeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
        interval_seconds: device.interval_seconds.map(|seconds| seconds as u64),
        expires_in_seconds: Some(device.expires_in_seconds),
    });
    let github_access_token = poll_for_access_token(
        domain,
        &device,
        &config.github_copilot,
        &config.client,
        &interaction.cancellation(),
    )
    .await?;
    let mut credential = refresh_access_token(
        &github_access_token,
        enterprise_domain.as_deref(),
        &config.github_copilot,
        &config.client,
        &interaction.cancellation(),
    )
    .await?;
    credential.available_model_ids = fetch_models(
        &credential.access,
        enterprise_domain.as_deref(),
        &config.github_copilot,
        &config.client,
        &interaction.cancellation(),
    )
    .await?;
    Ok(credential)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InteractionMessage;
    use crate::auth::test_support::FakeServer;
    use std::sync::mpsc::Receiver;
    use std::time::{Duration, Instant};

    fn next_prompt(messages: &Receiver<InteractionMessage>) -> (u64, AuthPrompt) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match messages
                .recv_timeout(Duration::from_secs(5))
                .expect("interaction message")
            {
                InteractionMessage::Prompt { id, prompt } => return (id, prompt),
                _ => assert!(Instant::now() < deadline, "prompt never arrived"),
            }
        }
    }

    fn token_and_models(request: &str) -> (u16, String) {
        if request.contains("GET /models") {
            (
                200,
                json!({
                    "data": [
                        {
                            "id": "gpt-5",
                            "model_picker_enabled": true,
                            "capabilities": { "supports": { "tool_calls": true } },
                            "policy": { "state": "enabled" }
                        },
                        {
                            "id": "disabled-model",
                            "model_picker_enabled": true,
                            "policy": { "state": "disabled" }
                        },
                        {
                            "id": "no-tools",
                            "model_picker_enabled": true,
                            "capabilities": { "supports": { "tool_calls": false } }
                        }
                    ]
                })
                .to_string(),
            )
        } else if request.contains("POST /usercode") {
            (
                200,
                r#"{"device_code":"device-1","user_code":"ABCD","verification_uri":"https://github.com/login/device","interval":0,"expires_in":60}"#.to_string(),
            )
        } else if request.contains("POST /access-token") {
            (200, r#"{"access_token":"gh-access"}"#.to_string())
        } else {
            (
                200,
                r#"{"token":"copilot-token;proxy-ep=proxy.individual.githubcopilot.com;","expires_at":4102444800}"#.to_string(),
            )
        }
    }

    fn configured(server: &FakeServer) -> FlowConfig {
        let mut config = FlowConfig::default();
        config.github_copilot.token_url = server.url("/copilot_internal/v2/token");
        config.github_copilot.device_user_code_url = server.url("/usercode");
        config.github_copilot.device_token_url = server.url("/access-token");
        config
    }

    #[test]
    fn base_url_comes_from_the_token_proxy_endpoint() {
        assert_eq!(
            base_url_from_token("tid=1;proxy-ep=proxy.individual.githubcopilot.com;", None),
            Some("https://api.individual.githubcopilot.com".to_string())
        );
        assert_eq!(
            base_url_from_token("no proxy", Some("ghe.company.com")),
            Some("https://copilot-api.ghe.company.com".to_string())
        );
        assert_eq!(
            base_url_from_token("no proxy", None),
            Some(DEFAULT_BASE_URL.to_string())
        );
    }

    #[test]
    fn client_id_matches_the_upstream_encoded_constant() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD;
        assert_eq!(
            String::from_utf8(STANDARD.decode(CLIENT_ID_ENCODED).expect("client id"))
                .expect("utf8"),
            CLIENT_ID
        );
    }

    #[tokio::test]
    async fn device_login_refreshes_and_filters_models() {
        let server = FakeServer::start(token_and_models);
        let config = configured(&server);
        let (interaction, messages) = Interaction::new();
        let flow = tokio::spawn(async move { login(&interaction, &config).await });
        let (id, prompt) = next_prompt(&messages);
        assert_eq!(
            prompt,
            AuthPrompt::Text {
                message: "GitHub Enterprise URL/domain (blank for github.com)".to_string(),
                placeholder: Some("company.ghe.com".to_string()),
            }
        );
        interaction.answer(id, Ok(String::new()));
        let credential = tokio::time::timeout(Duration::from_secs(10), flow)
            .await
            .expect("flow")
            .expect("credential");
        assert!(credential.access.starts_with("copilot-token"));
        assert_eq!(credential.expires, 4102444800_000 - FIVE_MINUTES_MS);
        assert_eq!(credential.available_model_ids, vec!["gpt-5".to_string()]);
        assert!(
            server
                .requests()
                .iter()
                .any(|request| request.contains("POST /usercode"))
        );
    }

    #[tokio::test]
    async fn refresh_applies_the_five_minute_skew() {
        let server = FakeServer::start(token_and_models);
        let config = configured(&server);
        let credential = refresh("gh-token", None, &CancelToken::new(), &config)
            .await
            .expect("refresh");
        assert_eq!(credential.refresh, "gh-token");
        assert_eq!(credential.expires, 4102444800_000 - FIVE_MINUTES_MS);
        assert_eq!(credential.available_model_ids, vec!["gpt-5".to_string()]);
    }

    #[tokio::test]
    async fn untrusted_verification_uri_is_rejected() {
        let server = FakeServer::start(|request| {
            if request.contains("POST /usercode") {
                (
                    200,
                    r#"{"device_code":"d","user_code":"u","verification_uri":"file:///etc/passwd","expires_in":60}"#.to_string(),
                )
            } else {
                (200, String::new())
            }
        });
        let config = configured(&server);
        let (interaction, messages) = Interaction::new();
        let flow = tokio::spawn(async move { login(&interaction, &config).await });
        let (id, _) = next_prompt(&messages);
        interaction.answer(id, Ok(String::new()));
        assert_eq!(
            flow.await.expect("flow").unwrap_err().to_string(),
            "Untrusted verification_uri in device code response"
        );
    }

    #[tokio::test]
    async fn invalid_enterprise_domain_is_reported() {
        let server = FakeServer::start(token_and_models);
        let config = configured(&server);
        let (interaction, messages) = Interaction::new();
        let flow = tokio::spawn(async move { login(&interaction, &config).await });
        let (id, _) = next_prompt(&messages);
        interaction.answer(id, Ok("bad domain??".to_string()));
        assert_eq!(
            flow.await.expect("flow").unwrap_err().to_string(),
            "Invalid GitHub Enterprise URL/domain"
        );
    }

    #[tokio::test]
    async fn cancelled_login_reports_login_cancelled() {
        let server = FakeServer::start(token_and_models);
        let config = configured(&server);
        let (interaction, messages) = Interaction::new();
        let cancel = interaction.cancellation();
        let flow = tokio::spawn(async move { login(&interaction, &config).await });
        let (id, _) = next_prompt(&messages);
        cancel.cancel();
        let _ = interaction.answer(id, Ok(String::new()));
        assert_eq!(
            flow.await.expect("flow").unwrap_err(),
            AuthError::Message(CANCEL_MESSAGE.to_string())
        );
    }
}
