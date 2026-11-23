//! xAI device-code OAuth (`packages/ai/src/auth/oauth/xai.ts`).
//!
//! Ground truth: constants (`:8-14`), field/URI validation (`:33-62`), form
//! POST + error messages (`:64-106`), device-code parsing (`:108-126`),
//! credential construction (`:128-143`), device request + poll (`:145-199`),
//! login (`:201-211`), refresh (`:213-227`) and `xaiOAuth` (`:229-239`).
//!
//! Deviations: the RFC 8628 poller is the shared [`poll_oauth_device_code_flow`]
//! and `AbortSignal` is [`CancelToken`].

use serde_json::Value;

use super::device_code::{
    DeviceCodePollOptions, DeviceCodePollResult, poll_oauth_device_code_flow,
};
use super::http::{form_encode, send};
use super::{AuthError, AuthEvent, CancelToken, FlowConfig, Interaction, OAuthCredential, now_ms};

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;
const DEFAULT_TOKEN_LIFETIME_SECONDS: f64 = 3600.0;

fn required_string(body: &Value, field: &str) -> Result<String, AuthError> {
    body.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| AuthError::Message(format!("Invalid xAI OAuth response field: {field}")))
}

fn positive_number(body: &Value, field: &str) -> Result<f64, AuthError> {
    body.get(field)
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite() && *number > 0.0)
        .ok_or_else(|| AuthError::Message(format!("Invalid xAI OAuth response field: {field}")))
}

/// `validateVerificationUri` (`:51-62`): only https URLs are trusted.
fn validate_verification_uri(raw: &str) -> Result<String, AuthError> {
    let rest = raw.strip_prefix("https://").ok_or_else(|| {
        AuthError::Message("Untrusted verification URI in xAI OAuth response".to_string())
    })?;
    if rest.is_empty() || rest.starts_with('/') || rest.chars().any(char::is_whitespace) {
        return Err(AuthError::Message(
            "Untrusted verification URI in xAI OAuth response".to_string(),
        ));
    }
    Ok(raw.to_string())
}

/// `postForm` (`:64-98`): `(ok, status, body)`.
async fn post_form(
    url: &str,
    fields: &[(&str, &str)],
    config: &FlowConfig,
    cancel: &CancelToken,
) -> Result<(bool, u16, Value), AuthError> {
    let response = send(
        config
            .client
            .post(url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form_encode(fields)),
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
    let text = response.text().await.map_err(|error| {
        if cancel.is_cancelled() {
            AuthError::Message(super::CANCEL_MESSAGE.to_string())
        } else {
            AuthError::Message(error.to_string())
        }
    })?;
    let body = serde_json::from_str::<Value>(&text)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_default();
    if serde_json::from_str::<Value>(&text).is_err() {
        return Err(AuthError::Message(format!(
            "xAI OAuth returned invalid JSON (HTTP {status})"
        )));
    }
    Ok((status < 400, status, body))
}

fn request_failure(action: &str, status: u16, body: &Value) -> AuthError {
    let error = body.get("error").and_then(Value::as_str);
    let description = body.get("error_description").and_then(Value::as_str);
    let detail = [error, description]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(": ");
    AuthError::Message(if detail.is_empty() {
        format!("xAI OAuth {action} failed (HTTP {status})")
    } else {
        format!("xAI OAuth {action} failed (HTTP {status}): {detail}")
    })
}

struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    interval_seconds: Option<f64>,
    expires_in_seconds: u64,
}

/// `parseDeviceCode` (`:108-126`).
fn parse_device_code(body: &Value) -> Result<DeviceAuthorization, AuthError> {
    let verification_uri_complete = body
        .get("verification_uri_complete")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(validate_verification_uri)
        .transpose()?;
    Ok(DeviceAuthorization {
        device_code: required_string(body, "device_code")?,
        user_code: required_string(body, "user_code")?,
        verification_uri: validate_verification_uri(&required_string(body, "verification_uri")?)?,
        verification_uri_complete,
        interval_seconds: body
            .get("interval")
            .and_then(Value::as_f64)
            .filter(|number| number.is_finite() && *number > 0.0),
        expires_in_seconds: positive_number(body, "expires_in")? as u64,
    })
}

/// `credentialsFromTokenResponse` (`:128-143`).
fn credentials_from_token(
    body: &Value,
    previous_refresh_token: Option<&str>,
) -> Result<OAuthCredential, AuthError> {
    let access = required_string(body, "access_token")?;
    let refresh = match body.get("refresh_token") {
        None => previous_refresh_token.map(str::to_string).ok_or_else(|| {
            AuthError::Message("Invalid xAI OAuth response field: refresh_token".to_string())
        })?,
        Some(_) => required_string(body, "refresh_token")?,
    };
    let expires_in = body
        .get("expires_in")
        .map(|_| positive_number(body, "expires_in"))
        .transpose()?
        .unwrap_or(DEFAULT_TOKEN_LIFETIME_SECONDS);
    Ok(OAuthCredential {
        access,
        refresh,
        expires: now_ms()
            .saturating_add((expires_in * 1000.0) as i64)
            .saturating_sub(REFRESH_SKEW_MS),
        account_id: None,
        enterprise_url: None,
        available_model_ids: Vec::new(),
        client_id: None,
        scopes: Vec::new(),
    })
}

/// `requestDeviceCode` (`:145-159`).
async fn request_device_code(
    config: &FlowConfig,
    cancel: &CancelToken,
) -> Result<DeviceAuthorization, AuthError> {
    let (ok, status, body) = post_form(
        &config.xai.device_user_code_url,
        &[
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
            ("referrer", "pi"),
        ],
        config,
        cancel,
    )
    .await?;
    if !ok {
        return Err(request_failure("device authorization", status, &body));
    }
    parse_device_code(&body)
}

/// `pollForTokens` (`:161-199`).
async fn poll_for_tokens(
    device: &DeviceAuthorization,
    config: &FlowConfig,
    cancel: &CancelToken,
) -> Result<OAuthCredential, AuthError> {
    let mut options = DeviceCodePollOptions::new(cancel, || async {
        let result = post_form(
            &config.xai.device_token_url,
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", CLIENT_ID),
                ("device_code", &device.device_code),
            ],
            config,
            cancel,
        )
        .await;
        let (ok, status, body) = match result {
            Ok(values) => values,
            Err(error) => return DeviceCodePollResult::Failed(error.to_string()),
        };
        if ok {
            return match credentials_from_token(&body, None) {
                Ok(credential) => DeviceCodePollResult::Complete(credential),
                Err(error) => DeviceCodePollResult::Failed(error.to_string()),
            };
        }
        match body.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => DeviceCodePollResult::Pending,
            Some("slow_down") => DeviceCodePollResult::SlowDown {
                interval_seconds: body.get("interval").and_then(Value::as_f64),
            },
            Some("access_denied" | "authorization_denied") => {
                DeviceCodePollResult::Failed("xAI device authorization was denied".to_string())
            }
            Some("expired_token") => {
                DeviceCodePollResult::Failed("xAI device code expired".to_string())
            }
            _ => DeviceCodePollResult::Failed(
                request_failure("device token polling", status, &body).to_string(),
            ),
        }
    });
    options.interval_seconds = device.interval_seconds;
    options.expires_in_seconds = Some(device.expires_in_seconds);
    options.wait_before_first_poll = true;
    poll_oauth_device_code_flow(options).await
}

/// `loginXai` (`:201-211`).
pub async fn login(
    interaction: &Interaction,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let cancel = interaction.cancellation();
    let device = request_device_code(config, &cancel).await?;
    interaction.notify(AuthEvent::DeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: device
            .verification_uri_complete
            .clone()
            .unwrap_or_else(|| device.verification_uri.clone()),
        interval_seconds: device.interval_seconds.map(|seconds| seconds as u64),
        expires_in_seconds: Some(device.expires_in_seconds),
    });
    poll_for_tokens(&device, config, &cancel).await
}

/// `refreshXaiToken` (`:213-227`).
pub async fn refresh(
    refresh_token: &str,
    cancel: &CancelToken,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let (ok, status, body) = post_form(
        &config.xai.device_token_url,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ],
        config,
        cancel,
    )
    .await?;
    if !ok {
        return Err(request_failure("token refresh", status, &body));
    }
    credentials_from_token(&body, Some(refresh_token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_endpoints_match_upstream() {
        let config = FlowConfig::default();
        assert_eq!(
            config.xai.device_user_code_url,
            "https://auth.x.ai/oauth2/device/code"
        );
        assert_eq!(
            config.xai.device_token_url,
            "https://auth.x.ai/oauth2/token"
        );
    }

    #[test]
    fn verification_uri_requires_https() {
        assert_eq!(
            validate_verification_uri("https://auth.x.ai/device").unwrap(),
            "https://auth.x.ai/device"
        );
        assert_eq!(
            validate_verification_uri("http://auth.x.ai/device")
                .unwrap_err()
                .to_string(),
            "Untrusted verification URI in xAI OAuth response"
        );
    }

    #[test]
    fn refresh_keeps_the_old_token_when_none_is_issued() {
        let body = serde_json::json!({"access_token": "new", "expires_in": 3600});
        let credential = credentials_from_token(&body, Some("old-refresh")).expect("credential");
        assert_eq!(credential.refresh, "old-refresh");
        assert!(credential.expires < now_ms().saturating_add(3600 * 1000));
    }
}
