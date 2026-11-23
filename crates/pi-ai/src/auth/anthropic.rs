//! Anthropic Claude Pro/Max browser/copy-code OAuth
//! (`packages/ai/src/auth/oauth/anthropic.ts`).
//!
//! Ground truth: constants and scopes (`:13-27`), `parseAuthorizationInput`
//! (`:29-57`), `formatErrorDetails`/`postJson` (`:59-94`), token exchange
//! (`:96-138`), browser login (`:140-198`), copy-code login (`:200-235`),
//! refresh (`:240-276`) and `anthropicOAuth` (`:278-307`).

use std::time::Duration;

use serde_json::{Value, json};

use super::callback::{
    CallbackOptions, CallbackResult, CallbackServer, ManualPrompt,
    wait_for_callback_or_manual_input,
};
use super::http::{form_encode, parse_authorization_input};
use super::pkce;
use super::{
    AuthError, AuthEvent, AuthPrompt, AuthPromptOption, CancelToken, EndpointConfig, FlowConfig,
    Interaction, OAuthCredential, now_ms,
};

/// `CLIENT_ID` (`anthropic.ts:14`, base64 in upstream source).
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
#[cfg(test)]
const CLIENT_ID_ENCODED: &str = "OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl";
const COPY_CODE_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const FIVE_MINUTES_MS: i64 = 5 * 60 * 1000;

fn format_error_details(error: &AuthError) -> String {
    format!("Error: {error}")
}

async fn post_json(client: &reqwest::Client, url: &str, body: Value) -> Result<String, AuthError> {
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        client
            .post(url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&body)
            .send(),
    )
    .await
    .map_err(|error| AuthError::Message(error.to_string()))?
    .map_err(|error| AuthError::Message(error.to_string()))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|error| AuthError::Message(error.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(AuthError::Message(format!(
            "HTTP request failed. status={status}; url={url}; body={text}"
        )));
    }
    Ok(text)
}

fn required_string(value: &Value, field: &str) -> Result<String, AuthError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| AuthError::Message(format!("Token response missing {field}")))
}

async fn exchange_authorization_code(
    code: &str,
    state: &str,
    verifier: &str,
    redirect_uri: &str,
    endpoints: &EndpointConfig,
    client: &reqwest::Client,
) -> Result<OAuthCredential, AuthError> {
    let url = &endpoints.token_url;
    let body = json!({
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "code": code,
        "state": state,
        "redirect_uri": redirect_uri,
        "code_verifier": verifier,
    });
    let response_body = post_json(client, url, body).await.map_err(|error| {
        AuthError::Message(format!(
            "Token exchange request failed. url={url}; redirect_uri={redirect_uri}; response_type=authorization_code; details={}",
            format_error_details(&error)
        ))
    })?;
    let data = serde_json::from_str::<Value>(&response_body).map_err(|error| {
        AuthError::Message(format!(
            "Token exchange returned invalid JSON. url={url}; body={response_body}; details={}",
            format_error_details(&AuthError::Message(error.to_string()))
        ))
    })?;
    let expires_in = data
        .get("expires_in")
        .and_then(Value::as_i64)
        .ok_or_else(|| AuthError::Message("Token response missing expires_in".to_string()))?;
    Ok(OAuthCredential {
        access: required_string(&data, "access_token")?,
        refresh: required_string(&data, "refresh_token")?,
        expires: now_ms()
            .saturating_add(expires_in.saturating_mul(1000))
            .saturating_sub(FIVE_MINUTES_MS),
        account_id: None,
        enterprise_url: None,
        available_model_ids: Vec::new(),
        client_id: None,
        scopes: Vec::new(),
    })
}

fn authorization_url(
    endpoints: &EndpointConfig,
    challenge: &str,
    verifier: &str,
    redirect_uri: &str,
) -> String {
    let query = form_encode(&[
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPES),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", verifier),
    ]);
    format!("{}?{query}", endpoints.authorize_url)
}

fn start_callback(
    endpoints: &EndpointConfig,
    verifier: &str,
    cancel: &CancelToken,
) -> Option<CallbackServer> {
    let options = |port: u16| CallbackOptions {
        provider_name: "Anthropic",
        host: &endpoints.callback_host,
        port,
        path: &endpoints.callback_path,
        redirect_host: Some("localhost"),
        state: Some(verifier),
        cancel: cancel.clone(),
        timeout: None,
        return_query: false,
    };
    CallbackServer::start(&options(endpoints.callback_port))
        .or_else(|_| CallbackServer::start(&options(0)))
        .ok()
}

/// `loginAnthropic` (`anthropic.ts:140-198`).
#[allow(clippy::too_many_lines)] // mirrors the TS browser-login sequence; split when extending
async fn login_browser(
    interaction: &Interaction,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let generated = pkce::generate()?;
    let mut callback = start_callback(
        &config.anthropic,
        &generated.verifier,
        &interaction.cancellation(),
    );
    // The fallback URI uses the upstream fixed path and port (`:22`).
    let fallback_uri = format!(
        "http://localhost:{}/callback",
        config.anthropic.callback_port
    );
    let redirect_uri = callback.as_ref().map_or_else(
        || fallback_uri.clone(),
        |server| server.redirect_uri().to_string(),
    );
    let url = authorization_url(
        &config.anthropic,
        &generated.challenge,
        &generated.verifier,
        &redirect_uri,
    );
    interaction.notify(AuthEvent::AuthUrl {
        url,
        instructions: Some(
            "Complete login in your browser. If the browser is on another machine, paste the final redirect URL here."
                .to_string(),
        ),
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
    let (code, state) = match result {
        CallbackResult::Callback(code) => (Some(code), generated.verifier.clone()),
        CallbackResult::Manual(input) => {
            let (code, state) = parse_authorization_input(&input, &generated.verifier);
            if state
                .as_deref()
                .is_some_and(|state| state != generated.verifier)
            {
                return Err(AuthError::Message("OAuth state mismatch".to_string()));
            }
            (code, state.unwrap_or_else(|| generated.verifier.clone()))
        }
    };
    let Some(code) = code else {
        return Err(AuthError::Message("Missing authorization code".to_string()));
    };
    interaction.notify(AuthEvent::Progress {
        message: "Exchanging authorization code for tokens...".to_string(),
    });
    exchange_authorization_code(
        &code,
        &state,
        &generated.verifier,
        &redirect_uri,
        &config.anthropic,
        &config.client,
    )
    .await
}

/// `loginAnthropicCopyCode` (`anthropic.ts:200-235`).
async fn login_copy_code(
    interaction: &Interaction,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let generated = pkce::generate()?;
    let url = authorization_url(
        &config.anthropic,
        &generated.challenge,
        &generated.verifier,
        COPY_CODE_REDIRECT_URI,
    );
    interaction.notify(AuthEvent::AuthUrl {
        url,
        instructions: Some(
            "Complete login in your browser, then copy the code Anthropic shows and paste it here."
                .to_string(),
        ),
    });
    let input = interaction
        .prompt(AuthPrompt::ManualCode {
            message: "Paste the code Anthropic shows after you sign in:".to_string(),
            placeholder: Some("code#state".to_string()),
        })
        .await?;
    let (code, state) = parse_authorization_input(&input, &generated.verifier);
    if state
        .as_deref()
        .is_some_and(|state| state != generated.verifier)
    {
        return Err(AuthError::Message("OAuth state mismatch".to_string()));
    }
    let Some(code) = code else {
        return Err(AuthError::Message("Missing authorization code".to_string()));
    };
    interaction.notify(AuthEvent::Progress {
        message: "Exchanging authorization code for tokens...".to_string(),
    });
    exchange_authorization_code(
        &code,
        &state.unwrap_or_else(|| generated.verifier.clone()),
        &generated.verifier,
        COPY_CODE_REDIRECT_URI,
        &config.anthropic,
        &config.client,
    )
    .await
}

/// `anthropicOAuth.login` (`anthropic.ts:282-300`).
///
/// # Errors
/// Unknown methods, state/code failures and token exchange errors.
pub async fn login(
    interaction: &Interaction,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let method = interaction
        .prompt(AuthPrompt::Select {
            message: "Select Anthropic login method:".to_string(),
            options: vec![
                AuthPromptOption {
                    id: "browser".to_string(),
                    label: "Browser login (default)".to_string(),
                },
                AuthPromptOption {
                    id: "copy_code".to_string(),
                    label: "Copy code login (headless)".to_string(),
                },
            ],
        })
        .await?;
    match method.as_str() {
        "copy_code" => login_copy_code(interaction, config).await,
        "browser" => login_browser(interaction, config).await,
        other => Err(AuthError::Message(format!(
            "Unknown Anthropic login method: {other}"
        ))),
    }
}

/// `refreshAnthropicToken` (`anthropic.ts:240-276`).
///
/// # Errors
/// Request/JSON failures with the upstream error strings.
pub async fn refresh(
    refresh_token: &str,
    _cancel: &CancelToken,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let url = &config.anthropic.token_url;
    let body = json!({
        "grant_type": "refresh_token",
        "client_id": CLIENT_ID,
        "refresh_token": refresh_token,
    });
    let response_body = post_json(&config.client, url, body)
        .await
        .map_err(|error| {
            AuthError::Message(format!(
                "Anthropic token refresh request failed. url={url}; details={}",
                format_error_details(&error)
            ))
        })?;
    let data = serde_json::from_str::<Value>(&response_body).map_err(|error| {
        AuthError::Message(format!(
            "Anthropic token refresh returned invalid JSON. url={url}; body={response_body}; details={}",
            format_error_details(&AuthError::Message(error.to_string()))
        ))
    })?;
    let expires_in = data
        .get("expires_in")
        .and_then(Value::as_i64)
        .ok_or_else(|| AuthError::Message("Token response missing expires_in".to_string()))?;
    Ok(OAuthCredential {
        access: required_string(&data, "access_token")?,
        refresh: required_string(&data, "refresh_token")?,
        expires: now_ms()
            .saturating_add(expires_in.saturating_mul(1000))
            .saturating_sub(FIVE_MINUTES_MS),
        account_id: None,
        enterprise_url: None,
        available_model_ids: Vec::new(),
        client_id: None,
        scopes: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::http::parse_query;
    use crate::auth::test_support::FakeServer;
    use crate::auth::{CANCEL_MESSAGE, InteractionMessage};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::sync::mpsc::Receiver;
    use std::time::Instant;

    fn token_response() -> (u16, String) {
        (
            200,
            r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600}"#
                .to_string(),
        )
    }

    fn next_message(messages: &Receiver<InteractionMessage>) -> InteractionMessage {
        messages
            .recv_timeout(Duration::from_secs(5))
            .expect("interaction message")
    }

    fn next_prompt(messages: &Receiver<InteractionMessage>) -> (u64, AuthPrompt) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match next_message(messages) {
                InteractionMessage::Prompt { id, prompt } => return (id, prompt),
                InteractionMessage::Notify(_) | InteractionMessage::PromptClosed { .. } => {
                    assert!(Instant::now() < deadline, "prompt never arrived");
                }
            }
        }
    }

    fn next_auth_url(messages: &Receiver<InteractionMessage>) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match next_message(messages) {
                InteractionMessage::Notify(AuthEvent::AuthUrl { url, .. }) => return url,
                _ => assert!(Instant::now() < deadline, "auth url never arrived"),
            }
        }
    }

    fn callback_get(uri: &str, query: &str) -> String {
        let without_scheme = uri.strip_prefix("http://").expect("http callback");
        let (authority, path) = without_scheme
            .split_once('/')
            .map_or((without_scheme, ""), |(authority, path)| (authority, path));
        let authority = authority.replace("localhost", "127.0.0.1");
        let mut stream = TcpStream::connect(&authority).expect("connect callback");
        let request = format!("GET /{path}?{query} HTTP/1.1\r\nHost: {authority}\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .expect("callback request");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }

    #[test]
    fn client_id_matches_the_upstream_encoded_constant() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD;
        let decoded = STANDARD.decode(CLIENT_ID_ENCODED).expect("client id");
        assert_eq!(String::from_utf8(decoded).expect("utf8"), CLIENT_ID);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn copy_code_login_exchanges_tokens_with_five_minute_skew() {
        let server = FakeServer::start(|_| token_response());
        let (interaction, messages) = Interaction::new();
        let interaction = Arc::new(interaction);
        let mut config = FlowConfig::default();
        config.anthropic.token_url = server.url("/v1/oauth/token");
        let flow = {
            let interaction = Arc::clone(&interaction);
            tokio::spawn(async move { login(&interaction, &config).await })
        };

        let (id, prompt) = next_prompt(&messages);
        assert_eq!(
            prompt,
            AuthPrompt::Select {
                message: "Select Anthropic login method:".to_string(),
                options: vec![
                    AuthPromptOption {
                        id: "browser".to_string(),
                        label: "Browser login (default)".to_string(),
                    },
                    AuthPromptOption {
                        id: "copy_code".to_string(),
                        label: "Copy code login (headless)".to_string(),
                    },
                ],
            }
        );
        assert!(interaction.answer(id, Ok("copy_code".to_string())));
        let url = next_auth_url(&messages);
        let params = parse_query(url.split_once('?').expect("query").1);
        let state = params.get("state").expect("state").clone();
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some(COPY_CODE_REDIRECT_URI)
        );
        let (id, _) = next_prompt(&messages);
        assert!(interaction.answer(id, Ok(format!("code-1#{state}"))));

        let before = now_ms();
        let credential = flow.await.expect("flow").expect("credential");
        assert_eq!(credential.access, "access-1");
        assert_eq!(credential.refresh, "refresh-1");
        assert!(credential.expires >= before + 3_600_000 - FIVE_MINUTES_MS);
        assert!(credential.expires <= now_ms() + 3_600_000 - FIVE_MINUTES_MS);
        let requests = server.requests();
        assert!(requests[0].contains("POST /v1/oauth/token"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn browser_login_uses_the_callback_server() {
        let server = FakeServer::start(|_| token_response());
        let (interaction, messages) = Interaction::new();
        let interaction = Arc::new(interaction);
        let mut config = FlowConfig::default();
        config.anthropic.callback_port = 0;
        config.anthropic.token_url = server.url("/v1/oauth/token");
        let flow = {
            let interaction = Arc::clone(&interaction);
            tokio::spawn(async move { login(&interaction, &config).await })
        };

        let (id, _) = next_prompt(&messages);
        interaction.answer(id, Ok("browser".to_string()));
        let url = next_auth_url(&messages);
        let params = parse_query(url.split_once('?').expect("query").1);
        let redirect = params.get("redirect_uri").expect("redirect").clone();
        let state = params.get("state").expect("state").clone();
        let response = callback_get(&redirect, &format!("code=cb-code&state={state}"));
        assert!(response.contains("Signed in to Anthropic"), "{response}");
        let credential = flow.await.expect("flow").expect("credential");
        assert_eq!(credential.access, "access-1");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn state_mismatch_is_reported() {
        let server = FakeServer::start(|_| token_response());
        let (interaction, messages) = Interaction::new();
        let interaction = Arc::new(interaction);
        let mut config = FlowConfig::default();
        config.anthropic.token_url = server.url("/v1/oauth/token");
        let flow = {
            let interaction = Arc::clone(&interaction);
            tokio::spawn(async move { login(&interaction, &config).await })
        };
        let (id, _) = next_prompt(&messages);
        interaction.answer(id, Ok("copy_code".to_string()));
        next_auth_url(&messages);
        let (id, _) = next_prompt(&messages);
        interaction.answer(id, Ok("wrong#state".to_string()));
        assert_eq!(
            flow.await.expect("flow").unwrap_err().to_string(),
            "OAuth state mismatch"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_method_is_reported_verbatim() {
        let (interaction, messages) = Interaction::new();
        let interaction = Arc::new(interaction);
        let config = FlowConfig::default();
        let flow = {
            let interaction = Arc::clone(&interaction);
            tokio::spawn(async move { login(&interaction, &config).await })
        };
        let (id, _) = next_prompt(&messages);
        interaction.answer(id, Ok("other".to_string()));
        assert_eq!(
            flow.await.expect("flow").unwrap_err().to_string(),
            "Unknown Anthropic login method: other"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_login_reports_login_cancelled() {
        let (interaction, messages) = Interaction::new();
        let interaction = Arc::new(interaction);
        let cancel = interaction.cancellation();
        let config = FlowConfig::default();
        let flow = {
            let interaction = Arc::clone(&interaction);
            tokio::spawn(async move { login(&interaction, &config).await })
        };
        let (id, _) = next_prompt(&messages);
        cancel.cancel();
        interaction.answer(id, Ok("browser".to_string()));
        assert_eq!(
            flow.await.expect("flow").unwrap_err().to_string(),
            CANCEL_MESSAGE
        );
    }

    #[tokio::test]
    async fn refresh_applies_the_five_minute_skew() {
        let server = FakeServer::start(|_| token_response());
        let mut config = FlowConfig::default();
        config.anthropic.token_url = server.url("/v1/oauth/token");
        let before = now_ms();
        let credential = refresh("refresh-1", &CancelToken::new(), &config)
            .await
            .expect("refresh");
        assert_eq!(credential.access, "access-1");
        assert_eq!(credential.refresh, "refresh-1");
        assert!(credential.expires >= before + 3_600_000 - FIVE_MINUTES_MS);
        assert!(credential.expires <= now_ms() + 3_600_000 - FIVE_MINUTES_MS);
        assert!(server.requests()[0].contains("POST /v1/oauth/token"));
    }

    #[tokio::test]
    async fn refresh_http_error_keeps_the_upstream_shape() {
        let server = FakeServer::start(|_| (400, "bad refresh".to_string()));
        let mut config = FlowConfig::default();
        config.anthropic.token_url = server.url("/v1/oauth/token");
        let error = refresh("r", &CancelToken::new(), &config)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("Anthropic token refresh request failed. url="),
            "{error}"
        );
        assert!(error.contains("status=400"), "{error}");
        assert!(error.contains("body=bad refresh"), "{error}");
    }

    #[tokio::test]
    async fn refresh_invalid_json_error_is_verbatim() {
        let server = FakeServer::start(|_| (200, "not json".to_string()));
        let mut config = FlowConfig::default();
        config.anthropic.token_url = server.url("/v1/oauth/token");
        let error = refresh("r", &CancelToken::new(), &config)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("Anthropic token refresh returned invalid JSON. url="),
            "{error}"
        );
        assert!(error.contains("body=not json"), "{error}");
    }
}
