//! Meta Model API OAuth (`packages/ai/src/auth/oauth/meta.ts`).
//!
//! Ground truth: constants/header + JSON helpers (`:20-60`), device
//! authorization (`:62-89`), identity-token poll (`:91-132`), the Muse Code
//! key-mint (`:134-163`), login (`:165-183`) and `metaOAuth` (`:185-197`).
//!
//! Deviations: the 30 s request timeout is per reqwest request instead of an
//! `AbortSignal.timeout`; the mint and device URLs live in `EndpointConfig` so
//! tests can override them.

use std::time::Duration;

use serde_json::Value;

use super::device_code::{
    DeviceCodePollOptions, DeviceCodePollResult, poll_oauth_device_code_flow,
};
use super::http::{form_encode, send};
use super::trusted_url::trusted_http_url;
use super::{AuthError, AuthEvent, CancelToken, FlowConfig, Interaction, OAuthCredential, now_ms};

const CLIENT_ID: &str = "1031625952748946";
#[cfg(test)]
const DEFAULT_DEVICE_AUTHORIZATION_URL: &str = "https://auth.meta.com/oidc/device/authorization/";
#[cfg(test)]
const DEFAULT_DEVICE_TOKEN_URL: &str = "https://auth.meta.com/oidc/device/token/";
const DEFAULT_MINT_URL: &str = "https://api.meta.ai/muse-code/key";
const API_KEY_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn read_json(text: &str) -> Option<Value> {
    serde_json::from_str::<Value>(text)
        .ok()
        .filter(Value::is_object)
}

/// `errorDetail` (`:50-56`) - the first non-blank error-ish field.
fn error_detail(json: Option<&Value>) -> String {
    ["error_description", "detail", "message", "error"]
        .into_iter()
        .find_map(|key| {
            json.and_then(|json| json.get(key))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .map_or_else(String::new, |value| format!(": {value}"))
}

fn positive_number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite() && *number > 0.0)
}

struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval_seconds: Option<f64>,
    expires_in_seconds: Option<u64>,
}

/// `startDeviceAuthorization` (`:62-89`).
async fn start_device_authorization(
    url: &str,
    config: &FlowConfig,
    cancel: &CancelToken,
) -> Result<DeviceAuthorization, AuthError> {
    let response = send(
        config
            .client
            .post(url)
            .timeout(REQUEST_TIMEOUT)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(form_encode(&[("client_id", CLIENT_ID)])),
        cancel,
    )
    .await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let json = read_json(&text);
    if !status.is_success() {
        return Err(AuthError::Message(format!(
            "Meta device authorization failed with status {}{}",
            status.as_u16(),
            error_detail(json.as_ref())
        )));
    }
    let string = |name: &str| {
        json.as_ref()
            .and_then(|json| json.get(name))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let device_code = string("device_code");
    let user_code = string("user_code");
    let verification_uri = json
        .as_ref()
        .and_then(|json| json.get("verification_uri_complete"))
        .and_then(Value::as_str)
        .and_then(trusted_http_url)
        .or_else(|| {
            json.as_ref()
                .and_then(|json| json.get("verification_uri"))
                .and_then(Value::as_str)
                .and_then(trusted_http_url)
        });
    match (device_code, user_code, verification_uri) {
        (Some(device_code), Some(user_code), Some(verification_uri)) => Ok(DeviceAuthorization {
            device_code,
            user_code,
            verification_uri,
            interval_seconds: positive_number(json.as_ref().and_then(|json| json.get("interval"))),
            expires_in_seconds: positive_number(
                json.as_ref().and_then(|json| json.get("expires_in")),
            )
            .map(|seconds| seconds as u64),
        }),
        _ => Err(AuthError::Message(format!(
            "Invalid Meta device authorization response: {text}"
        ))),
    }
}

/// `pollForIdentityToken` (`:91-132`).
async fn poll_for_identity_token(
    url: &str,
    device: &DeviceAuthorization,
    config: &FlowConfig,
    cancel: &CancelToken,
) -> Result<String, AuthError> {
    let mut options = DeviceCodePollOptions::new(cancel, || async {
        let response = send(
            config
                .client
                .post(url)
                .timeout(REQUEST_TIMEOUT)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("Accept", "application/json")
                .body(form_encode(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("device_code", &device.device_code),
                    ("client_id", CLIENT_ID),
                ])),
            cancel,
        )
        .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => return DeviceCodePollResult::Failed(error.to_string()),
        };
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let json = read_json(&text);
        let access = json
            .as_ref()
            .and_then(|json| json.get("access_token"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if status.is_success()
            && let Some(access) = access
        {
            return DeviceCodePollResult::Complete(access);
        }
        let error = json
            .as_ref()
            .and_then(|json| json.get("error"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        match error {
            "authorization_pending" => DeviceCodePollResult::Pending,
            "slow_down" => DeviceCodePollResult::SlowDown {
                interval_seconds: positive_number(
                    json.as_ref().and_then(|json| json.get("interval")),
                ),
            },
            "access_denied" => DeviceCodePollResult::Failed("Meta login was denied.".to_string()),
            "expired_token" => DeviceCodePollResult::Failed(
                "Meta device authorization expired. Please restart login.".to_string(),
            ),
            _ => DeviceCodePollResult::Failed(format!(
                "Meta device token request failed with status {}{}",
                status.as_u16(),
                error_detail(json.as_ref())
            )),
        }
    });
    options.interval_seconds = device.interval_seconds;
    options.expires_in_seconds = device.expires_in_seconds;
    options.wait_before_first_poll = true;
    poll_oauth_device_code_flow(options).await
}

/// `mintApiKey` (`:134-163`). The identity token rides as `refresh` so the
/// standard OAuth scheduler re-mints the short-lived key.
async fn mint_api_key(
    identity_token: &str,
    url: &str,
    config: &FlowConfig,
    cancel: &CancelToken,
) -> Result<OAuthCredential, AuthError> {
    let response = send(
        config
            .client
            .post(url)
            .timeout(REQUEST_TIMEOUT)
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {identity_token}"))
            .header("Content-Type", "application/json")
            .header("x-api-version", "1.0.0")
            .body("{}"),
        cancel,
    )
    .await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let json = read_json(&text);
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(AuthError::Message(format!(
            "Meta session expired (status {}). Run `/login meta` to sign in again.{}",
            status.as_u16(),
            error_detail(json.as_ref())
        )));
    }
    if !status.is_success() {
        return Err(AuthError::Message(format!(
            "Meta API key mint failed with status {}{}",
            status.as_u16(),
            error_detail(json.as_ref())
        )));
    }
    let api_key = json
        .as_ref()
        .and_then(|json| json.get("api_key"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if let Some(api_key) = api_key {
        return Ok(OAuthCredential {
            access: api_key,
            refresh: identity_token.to_string(),
            expires: now_ms().saturating_add(API_KEY_LIFETIME_MS),
            account_id: None,
            enterprise_url: None,
            available_model_ids: Vec::new(),
            client_id: None,
            scopes: Vec::new(),
        });
    }
    let action_url = json
        .as_ref()
        .and_then(|json| json.get("action_url"))
        .and_then(Value::as_str)
        .and_then(trusted_http_url);
    Err(AuthError::Message(format!(
        "Meta did not issue an API key.{}",
        action_url.map_or_else(String::new, |url| format!(" Complete setup at {url}"))
    )))
}

/// `loginMeta` (`:165-183`).
pub async fn login(
    interaction: &Interaction,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let cancel = interaction.cancellation();
    let result = async {
        let device =
            start_device_authorization(&config.meta.device_user_code_url, config, &cancel).await?;
        interaction.notify(AuthEvent::DeviceCode {
            user_code: device.user_code.clone(),
            verification_uri: device.verification_uri.clone(),
            interval_seconds: device.interval_seconds.map(|seconds| seconds as u64),
            expires_in_seconds: device.expires_in_seconds,
        });
        let identity_token =
            poll_for_identity_token(&config.meta.device_token_url, &device, config, &cancel)
                .await?;
        interaction.notify(AuthEvent::Progress {
            message: "Enabling Meta Model API access...".to_string(),
        });
        mint_api_key(&identity_token, &config.meta.token_url, config, &cancel).await
    }
    .await;
    match result {
        Err(_error) if cancel.is_cancelled() => {
            Err(AuthError::Message(super::CANCEL_MESSAGE.to_string()))
        }
        other => other,
    }
}

/// `metaOAuth.refresh` (`:192`): re-mint from the stored identity token.
pub async fn refresh(
    identity_token: &str,
    cancel: &CancelToken,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let url = if config.meta.token_url.is_empty() {
        DEFAULT_MINT_URL
    } else {
        &config.meta.token_url
    };
    mint_api_key(identity_token, url, config, cancel).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_endpoints_match_upstream() {
        let config = FlowConfig::default();
        assert_eq!(
            config.meta.device_user_code_url,
            DEFAULT_DEVICE_AUTHORIZATION_URL
        );
        assert_eq!(config.meta.device_token_url, DEFAULT_DEVICE_TOKEN_URL);
        assert_eq!(config.meta.token_url, DEFAULT_MINT_URL);
    }

    #[test]
    fn error_detail_checks_fields_in_upstream_order() {
        let json = serde_json::json!({"error": "generic", "message": " 具体 "});
        assert_eq!(error_detail(Some(&json)), ": 具体");
        assert_eq!(error_detail(None), "");
    }
}
