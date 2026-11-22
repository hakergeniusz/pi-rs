//! OpenAI Codex / ChatGPT browser and device-code OAuth
//! (`packages/ai/src/auth/oauth/openai-codex.ts`).
//!
//! Ground truth: constants (`:22-37`), state/random bytes (`:62-67`),
//! `parseAuthorizationInput` (`:69-97`), JWT account id (`:99-109`),
//! token response validation (`:122-185`), device-code start/poll
//! (`:187-287`), authorization URL (`:289-308`), credential conversion
//! (`:310-339`), device/browser login (`:341-400`), refresh (`:405-438`),
//! and `openaiCodexOAuth` (`:409-438`).

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};

use super::callback::CallbackResult;
use super::callback::{
    CallbackOptions, CallbackServer, ManualPrompt, wait_for_callback_or_manual_input,
};
use super::device_code::{
    DeviceCodePollOptions, DeviceCodePollResult, poll_oauth_device_code_flow,
};
use super::http::{form_encode, parse_authorization_input, send};
use super::pkce;
use super::{
    AuthError, AuthEvent, AuthPrompt, AuthPromptOption, CancelToken, EndpointConfig, FlowConfig,
    Interaction, OAuthCredential, now_ms,
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const SCOPE: &str = "openid profile email offline_access";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const DEVICE_CODE_TIMEOUT_SECONDS: u64 = 15 * 60;

#[derive(Debug, Clone, Copy)]
enum TokenOperation {
    Exchange,
    Refresh,
}

impl std::fmt::Display for TokenOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Exchange => "exchange",
            Self::Refresh => "refresh",
        })
    }
}

/// `readTokenResponse` (`openai-codex.ts:122-143`).
fn read_token_response(
    value: &Value,
    operation: TokenOperation,
) -> Result<OAuthCredential, AuthError> {
    let access = value.get("access_token").and_then(Value::as_str);
    let refresh = value.get("refresh_token").and_then(Value::as_str);
    let expires_in = value.get("expires_in").and_then(Value::as_i64);
    if access.is_none() || refresh.is_none() || expires_in.is_none() {
        return Err(AuthError::Message(format!(
            "OpenAI Codex token {operation} response missing fields: {value}"
        )));
    }
    Ok(OAuthCredential {
        access: access.expect("access").to_string(),
        refresh: refresh.expect("refresh").to_string(),
        expires: now_ms().saturating_add(expires_in.expect("expires").saturating_mul(1000)),
        account_id: None,
        enterprise_url: None,
        available_model_ids: Vec::new(),
    })
}

async fn response_error(response: reqwest::Response, operation: TokenOperation) -> AuthError {
    let status = response.status();
    let code = status.as_u16();
    let reason = status.canonical_reason().unwrap_or_default().to_string();
    let text = response.text().await.unwrap_or_default();
    let detail = if text.is_empty() { reason } else { text };
    AuthError::Message(format!(
        "OpenAI Codex token {operation} failed ({code}): {detail}"
    ))
}

async fn token_response(
    response: reqwest::Response,
    operation: TokenOperation,
) -> Result<OAuthCredential, AuthError> {
    if !response.status().is_success() {
        return Err(response_error(response, operation).await);
    }
    let value = response
        .json::<Value>()
        .await
        .map_err(|error| AuthError::Message(error.to_string()))?;
    read_token_response(&value, operation)
}

async fn exchange_authorization_code(
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    endpoints: &EndpointConfig,
    client: &reqwest::Client,
    cancel: &CancelToken,
) -> Result<OAuthCredential, AuthError> {
    let body = form_encode(&[
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
    ]);
    let response = send(
        client
            .post(&endpoints.token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body),
        cancel,
    )
    .await?;
    token_response(response, TokenOperation::Exchange).await
}

async fn refresh_access_token(
    refresh_token: &str,
    endpoints: &EndpointConfig,
    client: &reqwest::Client,
    cancel: &CancelToken,
) -> Result<OAuthCredential, AuthError> {
    let body = form_encode(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
    ]);
    let response = send(
        client
            .post(&endpoints.token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body),
        cancel,
    )
    .await
    .map_err(|error| AuthError::Message(format!("OpenAI Codex token refresh error: {error}")))?;
    token_response(response, TokenOperation::Refresh).await
}

/// `decodeJwt` (`openai-codex.ts:99-109`).
fn decode_jwt(token: &str) -> Option<Value> {
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    serde_json::from_slice::<Value>(&decoded).ok()
}

/// `getAccountId` + `credentialsFromToken` (`openai-codex.ts:310-330`).
fn credentials_from_token(mut credential: OAuthCredential) -> Result<OAuthCredential, AuthError> {
    let account_id = decode_jwt(&credential.access)
        .and_then(|payload| payload.get(JWT_CLAIM_PATH).cloned())
        .and_then(|claim| claim.get("chatgpt_account_id").cloned())
        .and_then(|claim| claim.as_str().map(str::to_string))
        .filter(|account| !account.is_empty())
        .ok_or_else(|| AuthError::Message("Failed to extract accountId from token".to_string()))?;
    credential.account_id = Some(account_id);
    Ok(credential)
}

#[derive(Debug, Clone, PartialEq)]
struct DeviceAuthInfo {
    device_auth_id: String,
    user_code: String,
    interval_seconds: f64,
}

/// `startOpenAICodexDeviceAuth` (`openai-codex.ts:187-229`).
async fn start_device_auth(
    endpoints: &EndpointConfig,
    client: &reqwest::Client,
    cancel: &CancelToken,
) -> Result<DeviceAuthInfo, AuthError> {
    let response = send(
        client
            .post(&endpoints.device_user_code_url)
            .header("Content-Type", "application/json")
            .json(&json!({ "client_id": CLIENT_ID })),
        cancel,
    )
    .await?;
    if response.status().as_u16() == 404 {
        return Err(AuthError::Message(
            "OpenAI Codex device code login is not enabled for this server. Use browser login or verify the server URL.".to_string(),
        ));
    }
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        let suffix = if body.is_empty() {
            String::new()
        } else {
            format!(": {body}")
        };
        return Err(AuthError::Message(format!(
            "OpenAI Codex device code request failed with status {status}{suffix}"
        )));
    }
    let value = response
        .json::<Value>()
        .await
        .map_err(|error| AuthError::Message(error.to_string()))?;
    let device_auth_id = value.get("device_auth_id").and_then(Value::as_str);
    let user_code = value.get("user_code").and_then(Value::as_str);
    let interval = value.get("interval").and_then(|interval| match interval {
        Value::String(text) => text.trim().parse::<f64>().ok(),
        Value::Number(number) => number.as_f64(),
        _ => None,
    });
    if device_auth_id.is_none()
        || user_code.is_none()
        || !interval.is_some_and(|seconds| seconds.is_finite() && seconds >= 0.0)
    {
        return Err(AuthError::Message(format!(
            "Invalid OpenAI Codex device code response: {value}"
        )));
    }
    Ok(DeviceAuthInfo {
        device_auth_id: device_auth_id.expect("device id").to_string(),
        user_code: user_code.expect("user code").to_string(),
        interval_seconds: interval.expect("interval"),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeviceTokenSuccess {
    authorization_code: String,
    code_verifier: String,
}

/// `pollOpenAICodexDeviceAuth` (`openai-codex.ts:231-287`).
async fn poll_device_auth(
    device: &DeviceAuthInfo,
    endpoints: &EndpointConfig,
    client: &reqwest::Client,
    cancel: &CancelToken,
) -> Result<DeviceTokenSuccess, AuthError> {
    let mut options = DeviceCodePollOptions::new(cancel, || async {
        let response = send(
            client
                .post(&endpoints.device_token_url)
                .header("Content-Type", "application/json")
                .json(&json!({
                    "device_auth_id": device.device_auth_id,
                    "user_code": device.user_code,
                })),
            cancel,
        )
        .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                return DeviceCodePollResult::Failed(error.to_string());
            }
        };
        if response.status().is_success() {
            let value = match response.json::<Value>().await {
                Ok(value) => value,
                Err(error) => {
                    return DeviceCodePollResult::Failed(error.to_string());
                }
            };
            let authorization_code = value.get("authorization_code").and_then(Value::as_str);
            let code_verifier = value.get("code_verifier").and_then(Value::as_str);
            return match (authorization_code, code_verifier) {
                (Some(code), Some(verifier)) => {
                    DeviceCodePollResult::Complete(DeviceTokenSuccess {
                        authorization_code: code.to_string(),
                        code_verifier: verifier.to_string(),
                    })
                }
                _ => DeviceCodePollResult::Failed(format!(
                    "Invalid OpenAI Codex device auth token response: {value}"
                )),
            };
        }
        let status = response.status().as_u16();
        if status == 403 || status == 404 {
            return DeviceCodePollResult::Pending;
        }
        let body = response.text().await.unwrap_or_default();
        let error_code = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| value.get("error").cloned())
            .and_then(|error| match error {
                Value::Object(object) => object.get("code").cloned(),
                other => Some(other),
            })
            .and_then(|value| value.as_str().map(str::to_string));
        match error_code.as_deref() {
            Some("deviceauth_authorization_pending") => DeviceCodePollResult::Pending,
            Some("slow_down") => DeviceCodePollResult::SlowDown {
                interval_seconds: None,
            },
            _ => {
                let suffix = if body.is_empty() {
                    String::new()
                } else {
                    format!(": {body}")
                };
                DeviceCodePollResult::Failed(format!(
                    "OpenAI Codex device auth failed with status {status}{suffix}"
                ))
            }
        }
    });
    options.interval_seconds = Some(device.interval_seconds);
    options.expires_in_seconds = Some(DEVICE_CODE_TIMEOUT_SECONDS);
    poll_oauth_device_code_flow(options).await
}

/// `createAuthorizationFlow` (`openai-codex.ts:289-308`).
fn authorization_flow(endpoints: &EndpointConfig, originator: &str) -> (String, String, String) {
    let generated = pkce::generate().expect("pkce");
    let state = pkce::state_hex();
    let redirect_uri = format!(
        "http://localhost:{}{}",
        endpoints.callback_port, endpoints.callback_path
    );
    let query = form_encode(&[
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", &redirect_uri),
        ("scope", SCOPE),
        ("code_challenge", &generated.challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", originator),
    ]);
    (
        generated.verifier,
        state,
        format!("{}?{query}", endpoints.authorize_url),
    )
}

/// `loginOpenAICodexDeviceCode` (`openai-codex.ts:341-357`).
async fn login_device_code(
    interaction: &Interaction,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let device = start_device_auth(
        &config.openai_codex,
        &config.client,
        &interaction.cancellation(),
    )
    .await?;
    interaction.notify(AuthEvent::DeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: config.openai_codex.verification_uri.clone(),
        interval_seconds: Some(device.interval_seconds as u64),
        expires_in_seconds: Some(DEVICE_CODE_TIMEOUT_SECONDS),
    });
    let code = poll_device_auth(
        &device,
        &config.openai_codex,
        &config.client,
        &interaction.cancellation(),
    )
    .await?;
    let credential = exchange_authorization_code(
        &code.authorization_code,
        &code.code_verifier,
        &config.openai_codex.device_redirect_uri,
        &config.openai_codex,
        &config.client,
        &interaction.cancellation(),
    )
    .await?;
    credentials_from_token(credential)
}

/// `loginOpenAICodex` (`openai-codex.ts:359-400`).
#[allow(clippy::too_many_lines)] // mirrors the TS browser-login sequence; split when extending
async fn login_browser(
    interaction: &Interaction,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let (verifier, state, url) = authorization_flow(&config.openai_codex, "pi");
    let redirect_uri = format!(
        "http://localhost:{}{}",
        config.openai_codex.callback_port, config.openai_codex.callback_path
    );
    let mut callback = CallbackServer::start(&CallbackOptions {
        provider_name: "OpenAI",
        host: &config.openai_codex.callback_host,
        port: config.openai_codex.callback_port,
        path: &config.openai_codex.callback_path,
        // The redirect URI says localhost (`openai-codex.ts:26`).
        redirect_host: Some("localhost"),
        state: Some(&state),
        cancel: interaction.cancellation(),
        timeout: None,
    })
    .ok();
    interaction.notify(AuthEvent::AuthUrl {
        url,
        instructions: Some("A browser window should open. Complete login to finish.".to_string()),
    });
    let prompt = ManualPrompt {
        message:
            "Complete login in your browser, or paste the authorization code / redirect URL here:"
                .to_string(),
        placeholder: redirect_uri.clone(),
    };
    let result = wait_for_callback_or_manual_input(interaction, &mut callback, &prompt).await;
    if let Some(server) = callback.as_ref() {
        server.close();
    }
    let result = result?;
    let code = match result {
        CallbackResult::Callback(code) => Some(code),
        CallbackResult::Manual(input) => {
            let (code, parsed_state) = parse_authorization_input(&input, &state);
            if parsed_state
                .as_deref()
                .is_some_and(|parsed| parsed != state)
            {
                return Err(AuthError::Message("State mismatch".to_string()));
            }
            code
        }
    };
    let Some(code) = code else {
        return Err(AuthError::Message("Missing authorization code".to_string()));
    };
    let credential = exchange_authorization_code(
        &code,
        &verifier,
        &redirect_uri,
        &config.openai_codex,
        &config.client,
        &interaction.cancellation(),
    )
    .await?;
    credentials_from_token(credential)
}

/// `openaiCodexOAuth.login` (`openai-codex.ts:413-431`).
///
/// # Errors
/// Unknown methods plus the browser/device flow errors.
pub async fn login(
    interaction: &Interaction,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let method = interaction
        .prompt(AuthPrompt::Select {
            message: "Select OpenAI Codex login method:".to_string(),
            options: vec![
                AuthPromptOption {
                    id: "browser".to_string(),
                    label: "Browser login (default)".to_string(),
                },
                AuthPromptOption {
                    id: "device_code".to_string(),
                    label: "Device code login (headless)".to_string(),
                },
            ],
        })
        .await?;
    match method.as_str() {
        "device_code" => login_device_code(interaction, config).await,
        "browser" => login_browser(interaction, config).await,
        other => Err(AuthError::Message(format!(
            "Unknown OpenAI Codex login method: {other}"
        ))),
    }
}

/// `refreshOpenAICodexToken` (`openai-codex.ts:405-407`).
///
/// # Errors
/// Refresh request failures and missing JWT account id.
pub async fn refresh(
    refresh_token: &str,
    cancel: &CancelToken,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let credential =
        refresh_access_token(refresh_token, &config.openai_codex, &config.client, cancel).await?;
    credentials_from_token(credential)
}

/// `getBaseUrlFromToken`/`getGitHubCopilotBaseUrl` are Copilot-only, but the
/// JWT claim path constant is shared with `mod.rs`'s `to_auth`.
#[must_use]
pub fn jwt_claim_path() -> &'static str {
    JWT_CLAIM_PATH
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::http::form_decode;
    use crate::auth::test_support::FakeServer;
    use crate::auth::{CANCEL_MESSAGE, InteractionMessage};
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

    fn next_event(messages: &Receiver<InteractionMessage>) -> AuthEvent {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match messages
                .recv_timeout(Duration::from_secs(5))
                .expect("interaction message")
            {
                InteractionMessage::Notify(event) => return event,
                _ => assert!(Instant::now() < deadline, "event never arrived"),
            }
        }
    }

    fn jwt(account: &str) -> String {
        let payload = json!({ JWT_CLAIM_PATH: { "chatgpt_account_id": account } }).to_string();
        format!("h.{}.s", URL_SAFE_NO_PAD.encode(payload.as_bytes()))
    }

    fn token_response() -> (u16, String) {
        (
            200,
            format!(
                r#"{{"access_token":"{}","refresh_token":"refresh-1","expires_in":3600}}"#,
                jwt("acct-1")
            ),
        )
    }

    #[test]
    fn jwt_account_id_is_read_from_the_openai_claim() {
        let credential = credentials_from_token(OAuthCredential {
            access: jwt("acct-1"),
            refresh: "r".to_string(),
            expires: 0,
            account_id: None,
            enterprise_url: None,
            available_model_ids: Vec::new(),
        })
        .expect("credential");
        assert_eq!(credential.account_id.as_deref(), Some("acct-1"));
        assert_eq!(
            credentials_from_token(OAuthCredential {
                access: "not-a-jwt".to_string(),
                refresh: "r".to_string(),
                expires: 0,
                account_id: None,
                enterprise_url: None,
                available_model_ids: Vec::new(),
            })
            .unwrap_err()
            .to_string(),
            "Failed to extract accountId from token"
        );
    }

    #[test]
    fn device_code_404_is_verbatim() {
        let server = FakeServer::start(|_| (404, String::new()));
        let mut config = FlowConfig::default();
        config.openai_codex.device_user_code_url = server.url("/usercode");
        let error = futures::executor::block_on(start_device_auth(
            &config.openai_codex,
            &config.client,
            &CancelToken::new(),
        ))
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "OpenAI Codex device code login is not enabled for this server. Use browser login or verify the server URL."
        );
    }

    #[tokio::test]
    async fn device_code_login_exchanges_the_authorization_code() {
        let server = FakeServer::start(|request| {
            if request.contains("POST /usercode") {
                (
                    200,
                    r#"{"device_auth_id":"device-1","user_code":"ABCD","interval":0}"#.to_string(),
                )
            } else if request.contains("POST /device-token") {
                (
                    200,
                    r#"{"authorization_code":"auth-1","code_verifier":"verifier-1"}"#.to_string(),
                )
            } else {
                token_response()
            }
        });
        let (interaction, messages) = Interaction::new();
        let mut config = FlowConfig::default();
        config.openai_codex.device_user_code_url = server.url("/usercode");
        config.openai_codex.device_token_url = server.url("/device-token");
        config.openai_codex.token_url = server.url("/token");
        let flow = tokio::spawn(async move { login(&interaction, &config).await });
        let (id, _) = next_prompt(&messages);
        interaction.answer(id, Ok("device_code".to_string()));
        match next_event(&messages) {
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                expires_in_seconds,
                ..
            } => {
                assert_eq!(user_code, "ABCD");
                assert!(verification_uri.contains("auth.openai.com"));
                assert_eq!(expires_in_seconds, Some(DEVICE_CODE_TIMEOUT_SECONDS));
            }
            other => panic!("expected device_code event, got {other:?}"),
        }
        let credential = flow.await.expect("flow").expect("credential");
        assert_eq!(credential.account_id.as_deref(), Some("acct-1"));
        assert_eq!(credential.refresh, "refresh-1");
        assert!(
            server
                .requests()
                .iter()
                .any(|request| request.contains("POST /token"))
        );
    }

    #[tokio::test]
    async fn browser_login_rejects_a_state_mismatch() {
        let (interaction, messages) = Interaction::new();
        let config = FlowConfig::default();
        let flow = tokio::spawn(async move { login(&interaction, &config).await });
        let (id, _) = next_prompt(&messages);
        interaction.answer(id, Ok("browser".to_string()));
        // Consume the auth_url notification.
        next_event(&messages);
        loop {
            match messages
                .recv_timeout(Duration::from_secs(5))
                .expect("interaction message")
            {
                InteractionMessage::Prompt { id, .. } => {
                    interaction.answer(id, Ok("code=x&state=wrong".to_string()));
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(
            flow.await.expect("flow").unwrap_err().to_string(),
            "State mismatch"
        );
    }

    #[tokio::test]
    async fn refresh_missing_fields_is_verbatim() {
        let server = FakeServer::start(|_| (200, r#"{"access_token":"a"}"#.to_string()));
        let mut config = FlowConfig::default();
        config.openai_codex.token_url = server.url("/token");
        let error = refresh("r", &CancelToken::new(), &config)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("OpenAI Codex token refresh response missing fields:"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn refresh_http_failure_keeps_status_and_body() {
        let server = FakeServer::start(|_| (401, "invalid_grant".to_string()));
        let mut config = FlowConfig::default();
        config.openai_codex.token_url = server.url("/token");
        let error = refresh("r", &CancelToken::new(), &config)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "OpenAI Codex token refresh failed (401): invalid_grant"
        );
    }

    #[tokio::test]
    async fn cancelled_login_reports_login_cancelled() {
        let (interaction, messages) = Interaction::new();
        let cancel = interaction.cancellation();
        let config = FlowConfig::default();
        let flow = tokio::spawn(async move { login(&interaction, &config).await });
        let (id, _) = next_prompt(&messages);
        cancel.cancel();
        let _ = interaction.answer(id, Ok("browser".to_string()));
        assert_eq!(
            flow.await.expect("flow").unwrap_err(),
            AuthError::Message(CANCEL_MESSAGE.to_string())
        );
    }

    #[test]
    fn form_decode_round_trips_the_state_query() {
        assert_eq!(form_decode("a%2Bb"), "a+b");
    }
}
