//! Sign in with `ChatGPT` (`packages/ai/src/auth/oauth/openai-chatgpt.ts`).
//!
//! Ground truth: constants (`:16-29`), callback parsing (`:49-78`), token
//! response validation (`:134-223`), the dynamic-client login (`:233-298`)
//! and `openaiChatGPTOAuth` (`:300-309`).
//!
//! Deviations: upstream obtains a stable device UUID from
//! `LoginOptions.getDeviceId()` (`interactive-mode.ts:6538`); the port has no
//! settings manager, so [`FlowConfig::openai_chatgpt_device_id`] carries it and
//! a fresh UUID v4 is synthesized when unset. The loopback server is the
//! shared callback server with `return_query` enabled because `OpenAI` returns
//! the issued `client_id` in the callback query.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use super::callback::{
    CallbackOptions, CallbackResult, CallbackServer, ManualPrompt,
    wait_for_callback_or_manual_input,
};
use super::http::{form_encode, parse_query, send};
use super::pkce::{base64url_encode, generate, random_bytes, uuid_v4};
use super::{AuthError, AuthEvent, CancelToken, FlowConfig, Interaction, OAuthCredential, now_ms};

const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
const AGENT_NAME_HINT: &str = "Pi";
const RESOURCE: &str = "https://api.openai.com/v1";
const DIRECT_TOKEN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const EXPIRY_MARGIN_MS: i64 = 3 * 60 * 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn random_value() -> String {
    base64url_encode(&random_bytes(32))
}

fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23].iter().all(|index| bytes[*index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
}

/// `agentHostId` (`:225-231`), with the port's synthesized UUID fallback.
fn agent_host_id(config: &FlowConfig) -> String {
    let device_id = config
        .openai_chatgpt_device_id
        .as_deref()
        .filter(|value| is_uuid(value))
        .map_or_else(uuid_v4, str::to_string);
    format!("urn:uuid:{}", device_id.to_lowercase())
}

/// `authorizationResultFromCallback` (`:53-62`).
fn authorization_result_from_callback(
    query: &BTreeMap<String, String>,
    expected_state: &str,
) -> Result<(String, String), AuthError> {
    let code = query
        .get("code")
        .filter(|code| !code.is_empty())
        .ok_or_else(|| AuthError::Message("Missing authorization code".to_string()))?;
    let state = query
        .get("state")
        .ok_or_else(|| AuthError::Message("Missing OAuth state".to_string()))?;
    if state != expected_state {
        return Err(AuthError::Message("OAuth state mismatch".to_string()));
    }
    let client_id = query
        .get("client_id")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AuthError::Message(
                "OpenAI OAuth registration callback did not contain an issued client ID"
                    .to_string(),
            )
        })?;
    Ok((code.clone(), client_id.to_string()))
}

/// `authorizationResultFromManualInput` (`:64-78`).
fn authorization_result_from_manual_input(
    input: &str,
    expected_state: &str,
    redirect_uri: &str,
) -> Result<(String, String), AuthError> {
    let value = input.trim();
    let (origin, path, query) = if let Some((scheme, rest)) = value.split_once("://") {
        if !matches!(scheme, "http" | "https") {
            return Err(AuthError::Message(
                "Paste the full callback URL from the browser".to_string(),
            ));
        }
        let (authority, rest) = rest.split_once('/').unwrap_or((rest, ""));
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        (format!("{scheme}://{authority}"), format!("/{path}"), query)
    } else {
        return Err(AuthError::Message(
            "Paste the full callback URL from the browser".to_string(),
        ));
    };
    let expected_origin_and_path = redirect_uri
        .split_once("://")
        .map(|(_, rest)| rest.split_once('/').unwrap_or((rest, "")));
    let matches_redirect = expected_origin_and_path.is_some_and(|(authority, tail)| {
        let (expected_path, _) = tail.split_once('?').unwrap_or((tail, ""));
        origin.ends_with(authority) && path == format!("/{expected_path}")
    });
    if !matches_redirect {
        return Err(AuthError::Message(format!(
            "The pasted callback URL must start with {redirect_uri}"
        )));
    }
    let query_map = parse_query(query);
    if let Some(error) = query_map.get("error") {
        return Err(AuthError::Message(format!(
            "ChatGPT authorization failed: {error}"
        )));
    }
    authorization_result_from_callback(&query_map, expected_state)
}

/// `requestToken` (`:134-153`).
async fn request_token(
    fields: &[(&str, &str)],
    config: &FlowConfig,
    cancel: &CancelToken,
) -> Result<Value, AuthError> {
    let response = send(
        config
            .client
            .post(&config.openai_chatgpt.token_url)
            .timeout(REQUEST_TIMEOUT)
            .header("accept", "application/json")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form_encode(fields)),
        cancel,
    )
    .await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let reason = status.canonical_reason().unwrap_or("");
        let detail = if text.is_empty() {
            reason.to_string()
        } else {
            text.clone()
        };
        return Err(AuthError::Message(format!(
            "OpenAI OAuth token request failed ({}): {detail}",
            status.as_u16()
        )));
    }
    serde_json::from_str::<Value>(&text)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| {
            AuthError::Message("OpenAI OAuth token response must be an object".to_string())
        })
}

/// `requireTokenString` (`:155-160`).
fn require_token_string(value: Option<&Value>, field: &str) -> Result<String, AuthError> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            AuthError::Message(format!("OpenAI OAuth token response has invalid {field}"))
        })
}

/// `credentialFromTokenResponse` (`:162-181`).
fn credential_from_token_response(
    token: &Value,
    client_id: &str,
) -> Result<OAuthCredential, AuthError> {
    let access = require_token_string(token.get("access_token"), "access_token")?;
    let refresh = require_token_string(token.get("refresh_token"), "refresh_token")?;
    let scope = require_token_string(token.get("scope"), "scope")?;
    let expires_in = token
        .get("expires_in")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| {
            AuthError::Message("OpenAI OAuth token response has invalid expires_in".to_string())
        })?;
    let scopes: Vec<String> = scope
        .split_whitespace()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect();
    if !scopes.iter().any(|scope| scope == DIRECT_TOKEN_SCOPE) {
        return Err(AuthError::Message(format!(
            "OpenAI OAuth grant did not include {DIRECT_TOKEN_SCOPE}"
        )));
    }
    Ok(OAuthCredential {
        access,
        refresh,
        expires: now_ms()
            .saturating_add((expires_in * 1000.0) as i64)
            .saturating_sub(EXPIRY_MARGIN_MS),
        account_id: None,
        enterprise_url: None,
        available_model_ids: Vec::new(),
        client_id: Some(client_id.to_string()),
        scopes,
    })
}

/// `exchangeAuthorizationCode` (`:183-206`).
async fn exchange_authorization_code(
    code: &str,
    verifier: &str,
    client_id: &str,
    redirect_uri: &str,
    config: &FlowConfig,
    cancel: &CancelToken,
) -> Result<OAuthCredential, AuthError> {
    let token = request_token(
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
            ("resource", RESOURCE),
        ],
        config,
        cancel,
    )
    .await?;
    let id_token = token.get("id_token").and_then(Value::as_str);
    if id_token.is_none_or(|value| value.trim().is_empty()) {
        return Err(AuthError::Message(
            "OpenAI OAuth token response did not contain an ID token".to_string(),
        ));
    }
    credential_from_token_response(&token, client_id)
}

/// `refreshAccessToken` (`:208-223`).
pub async fn refresh(
    client_id: Option<&str>,
    refresh_token: &str,
    cancel: &CancelToken,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let client_id = client_id
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            AuthError::Message(
                "Stored OpenAI OAuth credential does not contain an issued client ID; reconnect ChatGPT"
                    .to_string(),
            )
        })?;
    let token = request_token(
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
            ("resource", RESOURCE),
        ],
        config,
        cancel,
    )
    .await?;
    credential_from_token_response(&token, client_id)
}

/// `loginOpenAIChatGPT` (`:233-298`).
pub async fn login(
    interaction: &Interaction,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let host_id = agent_host_id(config);
    let pkce = generate()?;
    let state = random_value();
    let nonce = random_value();
    let cancel = interaction.cancellation();

    let mut callback = Some(
        CallbackServer::start(&CallbackOptions {
            provider_name: "OpenAI",
            host: &config.openai_chatgpt.callback_host,
            port: config.openai_chatgpt.callback_port,
            path: &config.openai_chatgpt.callback_path,
            redirect_host: Some("127.0.0.1"),
            state: Some(&state),
            cancel: cancel.clone(),
            timeout: None,
            return_query: true,
        })
        .map_err(|error| {
            if error.to_string().contains("Address already in use") {
                AuthError::Message(format!(
                    "Port {} is in use, probably by an unfinished login in another pi session or by the Codex CLI. Cancel that login and try again.",
                    config.openai_chatgpt.callback_port
                ))
            } else {
                error
            }
        })?,
    );
    let redirect_uri = callback
        .as_ref()
        .expect("callback")
        .redirect_uri()
        .to_string();
    let authorize_url = format!(
        "{}?{}",
        config.openai_chatgpt.authorize_url,
        form_encode(&[
            ("client_id", DYNAMIC_CLIENT_ID),
            ("agent_name_hint", AGENT_NAME_HINT),
            ("ext_agent_host_id", &host_id),
            ("response_type", "code"),
            ("redirect_uri", &redirect_uri),
            ("resource", RESOURCE),
            ("scope", SCOPE),
            ("state", &state),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("nonce", &nonce),
        ])
    );
    interaction.notify(AuthEvent::AuthUrl {
        url: authorize_url,
        instructions: Some(
            "Complete sign-in in your browser. If the callback does not complete, paste the final redirect URL here.".to_string(),
        ),
    });

    let result = async {
        let received = wait_for_callback_or_manual_input(
            interaction,
            &mut callback,
            &ManualPrompt {
                message: "Complete login in your browser, or paste the final redirect URL here:"
                    .to_string(),
                placeholder: redirect_uri.clone(),
            },
        )
        .await?;
        let (code, client_id) = match received {
            CallbackResult::Callback(query) => {
                authorization_result_from_callback(&parse_query(&query), &state)?
            }
            CallbackResult::Manual(input) => {
                authorization_result_from_manual_input(&input, &state, &redirect_uri)?
            }
        };
        interaction.notify(AuthEvent::Progress {
            message: "Exchanging authorization code for tokens...".to_string(),
        });
        exchange_authorization_code(
            &code,
            &pkce.verifier,
            &client_id,
            &redirect_uri,
            config,
            &cancel,
        )
        .await
    }
    .await;
    if let Some(callback) = callback.as_mut() {
        callback.close();
    }
    match result {
        Err(_error) if cancel.is_cancelled() => {
            Err(AuthError::Message(super::CANCEL_MESSAGE.to_string()))
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_endpoints_match_upstream() {
        let config = FlowConfig::default();
        assert_eq!(
            config.openai_chatgpt.authorize_url,
            "https://auth.openai.com/api/accounts/authorize"
        );
        assert_eq!(
            config.openai_chatgpt.token_url,
            "https://auth.openai.com/api/accounts/oauth/token"
        );
        assert_eq!(config.openai_chatgpt.callback_port, 1455);
        assert_eq!(config.openai_chatgpt.callback_path, "/auth/callback");
    }

    #[test]
    fn callback_parsing_checks_state_and_requires_an_issued_client() {
        let mut query = BTreeMap::new();
        query.insert("code".to_string(), "abc".to_string());
        query.insert("state".to_string(), "s".to_string());
        assert_eq!(
            authorization_result_from_callback(&query, "s")
                .unwrap_err()
                .to_string(),
            "OpenAI OAuth registration callback did not contain an issued client ID"
        );
        query.insert("client_id".to_string(), " app-1 ".to_string());
        assert_eq!(
            authorization_result_from_callback(&query, "s").unwrap(),
            ("abc".to_string(), "app-1".to_string())
        );
        assert_eq!(
            authorization_result_from_callback(&query, "other")
                .unwrap_err()
                .to_string(),
            "OAuth state mismatch"
        );
    }

    #[test]
    fn manual_input_must_be_the_expected_redirect() {
        let error = authorization_result_from_manual_input(
            "https://evil.example/cb?code=a&state=s&client_id=c",
            "s",
            "http://127.0.0.1:1455/auth/callback",
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "The pasted callback URL must start with http://127.0.0.1:1455/auth/callback"
        );
        assert_eq!(
            authorization_result_from_manual_input(
                "not a url",
                "s",
                "http://127.0.0.1:1455/auth/callback"
            )
            .unwrap_err()
            .to_string(),
            "Paste the full callback URL from the browser"
        );
    }

    #[test]
    fn token_response_requires_the_direct_token_scope() {
        let token = serde_json::json!({
            "access_token": "a", "refresh_token": "r", "scope": "openid profile",
            "expires_in": 3600, "id_token": "id"
        });
        assert_eq!(
            credential_from_token_response(&token, "client-1")
                .unwrap_err()
                .to_string(),
            "OpenAI OAuth grant did not include chatgpt.tokens.use.direct"
        );
        let token = serde_json::json!({
            "access_token": "a", "refresh_token": "r",
            "scope": "openid chatgpt.tokens.use.direct",
            "expires_in": 3600, "id_token": "id"
        });
        let credential = credential_from_token_response(&token, "client-1").expect("credential");
        assert_eq!(credential.client_id.as_deref(), Some("client-1"));
        assert!(
            credential
                .scopes
                .iter()
                .any(|scope| scope == DIRECT_TOKEN_SCOPE)
        );
        assert!(credential.expires < now_ms().saturating_add(3600 * 1000));
    }

    #[test]
    fn device_id_is_a_uuid_urn() {
        let config = FlowConfig::default();
        let host = agent_host_id(&config);
        assert!(host.starts_with("urn:uuid:"));
        let uuid = &host["urn:uuid:".len()..];
        assert!(is_uuid(uuid));
    }
}
