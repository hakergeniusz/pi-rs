//! `OpenRouter` PKCE OAuth (`packages/ai/src/auth/oauth/openrouter.ts`).
//!
//! Ground truth: constants/URL helpers (`:19-26`), input + error parsing
//! (`:30-56`), the JSON token exchange (`:58-111`), login (`:113-157`) and
//! `openRouterOAuth` (`:159-169`).
//!
//! Deviations: the loopback server is [`crate::auth::callback::CallbackServer`]
//! (a blocking task, same route/query contract), `crypto.randomUUID()` is
//! [`pkce::uuid_v4`], and the key exchange's 30 s timeout is a reqwest timeout.

use std::time::Duration;

use serde_json::Value;

use super::callback::{
    CallbackOptions, CallbackResult, CallbackServer, ManualPrompt,
    wait_for_callback_or_manual_input,
};
use super::http::parse_query;
use super::pkce::{generate, uuid_v4};
use super::{AuthError, AuthEvent, CancelToken, FlowConfig, Interaction, OAuthCredential};

const LOGIN_TIMEOUT: Duration = Duration::from_mins(5);
const TOKEN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// `parseAuthorizationInput` (`:30-45`): a URL's `code`, a query string's
/// `code`, or the bare code.
fn parse_authorization_input(input: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    if value.starts_with("http://") || value.starts_with("https://") {
        let query = value.split_once('?').map_or("", |(_, query)| query);
        return parse_query(query).get("code").cloned();
    }
    if value.contains("code=") {
        return parse_query(value).get("code").cloned();
    }
    Some(value.to_string())
}

/// `errorDetail` (`:47-56`).
fn error_detail(body: &Value) -> Option<String> {
    if let Some(value) = body.get("error_description").and_then(Value::as_str) {
        return Some(value.to_string());
    }
    if let Some(value) = body.get("message").and_then(Value::as_str) {
        return Some(value.to_string());
    }
    if let Some(value) = body.get("error").and_then(Value::as_str) {
        return Some(value.to_string());
    }
    body.get("error")
        .and_then(Value::as_object)
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// `exchangeAuthorizationCode` (`:58-111`).
async fn exchange_authorization_code(
    code: &str,
    verifier: &str,
    config: &FlowConfig,
    cancel: &CancelToken,
) -> Result<OAuthCredential, AuthError> {
    let response = super::http::send(
        config
            .client
            .post(&config.openrouter.token_url)
            .timeout(TOKEN_EXCHANGE_TIMEOUT)
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "code": code,
                "code_verifier": verifier,
                "code_challenge_method": "S256",
            })),
        cancel,
    )
    .await
    .map_err(|error| {
        if cancel.is_cancelled() {
            AuthError::Message(super::CANCEL_MESSAGE.to_string())
        } else {
            error
        }
    })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let body = serde_json::from_str::<Value>(&text)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_default();
    if status >= 400 {
        let detail = error_detail(&body).map_or_else(String::new, |detail| format!(": {detail}"));
        return Err(AuthError::Message(format!(
            "OpenRouter OAuth key exchange failed (HTTP {status}){detail}"
        )));
    }
    if text.is_empty() || !body.is_object() {
        return Err(AuthError::Message(
            "OpenRouter OAuth returned invalid JSON".to_string(),
        ));
    }
    let key = body
        .get("key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            AuthError::Message("OpenRouter OAuth response carries no \"key\"".to_string())
        })?;
    Ok(OAuthCredential {
        access: key,
        refresh: String::new(),
        expires: MAX_SAFE_INTEGER,
        account_id: None,
        enterprise_url: None,
        available_model_ids: Vec::new(),
        client_id: None,
        scopes: Vec::new(),
    })
}

/// `loginOpenRouter` (`:113-157`).
pub async fn login(
    interaction: &Interaction,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let pkce = generate()?;
    let cancel = interaction.cancellation();
    let path = format!(
        "{}/{}",
        config.openrouter.callback_path.trim_end_matches('/'),
        uuid_v4()
    );
    let mut callback = Some(CallbackServer::start(&CallbackOptions {
        provider_name: "OpenRouter",
        host: &config.openrouter.callback_host,
        port: config.openrouter.callback_port,
        path: &path,
        redirect_host: None,
        state: None,
        cancel: cancel.clone(),
        timeout: Some(LOGIN_TIMEOUT),
        return_query: false,
    })?);
    let redirect_uri = callback
        .as_ref()
        .expect("callback")
        .redirect_uri()
        .to_string();
    let authorize_url = format!(
        "{}?{}",
        config.openrouter.authorize_url,
        super::http::form_encode(&[
            ("callback_url", &redirect_uri),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
        ])
    );
    interaction.notify(AuthEvent::Progress {
        message: format!("Listening for OpenRouter OAuth callback on {redirect_uri}"),
    });
    interaction.notify(AuthEvent::AuthUrl {
        url: authorize_url,
        instructions: Some(
            "Complete sign-in in your browser. If the browser is on another machine, paste the final redirect URL here.".to_string(),
        ),
    });
    let result = async {
        let result = wait_for_callback_or_manual_input(
            interaction,
            &mut callback,
            &ManualPrompt {
                message: "Complete sign-in in your browser, or paste the authorization code / redirect URL here:".to_string(),
                placeholder: redirect_uri.clone(),
            },
        )
        .await?;
        match result {
            CallbackResult::Callback(code) => {
                exchange_authorization_code(&code, &pkce.verifier, config, &cancel).await
            }
            CallbackResult::Manual(input) => {
                let code = parse_authorization_input(&input)
                    .ok_or_else(|| AuthError::Message("Missing authorization code".to_string()))?;
                interaction.notify(AuthEvent::Progress {
                    message: "Exchanging authorization code for an API key...".to_string(),
                });
                exchange_authorization_code(&code, &pkce.verifier, config, &cancel).await
            }
        }
    }
    .await;
    if let Some(callback) = callback.as_mut() {
        callback.close();
    }
    result
}

/// `openRouterOAuth.refresh` (`:163-165`): the permanent key needs no refresh.
#[must_use]
pub fn refresh(access: &str) -> OAuthCredential {
    OAuthCredential {
        access: access.to_string(),
        refresh: String::new(),
        expires: MAX_SAFE_INTEGER,
        account_id: None,
        enterprise_url: None,
        available_model_ids: Vec::new(),
        client_id: None,
        scopes: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_endpoints_match_upstream() {
        let config = FlowConfig::default();
        assert_eq!(
            config.openrouter.authorize_url,
            "https://openrouter.ai/auth"
        );
        assert_eq!(
            config.openrouter.token_url,
            "https://openrouter.ai/api/v1/auth/keys"
        );
    }

    #[test]
    fn authorization_input_accepts_urls_queries_and_bare_codes() {
        assert_eq!(
            parse_authorization_input("https://openrouter.ai/cb?code=abc&state=x"),
            Some("abc".to_string())
        );
        assert_eq!(
            parse_authorization_input("code=abc&code_challenge=x"),
            Some("abc".to_string())
        );
        assert_eq!(
            parse_authorization_input(" bare "),
            Some("bare".to_string())
        );
        assert_eq!(parse_authorization_input("  "), None);
    }

    #[test]
    fn error_detail_follows_the_upstream_field_order() {
        let json = serde_json::json!({"error": "e", "message": "m", "error_description": "d"});
        assert_eq!(error_detail(&json).as_deref(), Some("d"));
        let nested = serde_json::json!({"error": {"message": "nested"}});
        assert_eq!(error_detail(&nested).as_deref(), Some("nested"));
    }
}
