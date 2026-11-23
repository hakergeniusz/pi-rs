//! Kimi Code subscription OAuth (`packages/ai/src/auth/oauth/kimi-coding.ts`).
//!
//! Ground truth: constants (`:15-20`), env host override + JSON helpers
//! (`:37-57`), device authorization (`:59-100`), token parsing (`:102-122`),
//! the device poll (`:124-190`), the bounded refresh retry (`:192-247`),
//! login (`:249-261`) and `kimiCodingOAuth` (`:263-278`).
//!
//! Deviations: `AbortSignal.timeout` is a per-request reqwest timeout, and the
//! env host override is resolved inside `endpoints()` so tests can point the
//! same endpoint table at a loopback server.

use std::time::Duration;

use serde_json::Value;

use super::device_code::{
    DeviceCodePollOptions, DeviceCodePollResult, poll_oauth_device_code_flow,
};
use super::http::{form_encode, send};
use super::trusted_url::trusted_http_url;
use super::{AuthError, AuthEvent, CancelToken, FlowConfig, Interaction, OAuthCredential, now_ms};

const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
#[cfg(test)]
const DEFAULT_OAUTH_HOST: &str = "https://auth.kimi.com";
const DEVICE_CODE_TIMEOUT_SECONDS: u64 = 15 * 60;
const DEFAULT_POLL_INTERVAL_SECONDS: f64 = 5.0;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const REFRESH_MAX_RETRIES: usize = 3;

/// `getOauthHost` (`:37-40`), with the env override checked first like
/// upstream's `getProviderEnvValue("KIMI_CODE_OAUTH_HOST") || ...`.
fn env_host() -> Option<String> {
    ["KIMI_CODE_OAUTH_HOST", "KIMI_OAUTH_HOST"]
        .into_iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .map(|value| value.trim_end_matches('/').to_string())
}

fn endpoints(config: &FlowConfig) -> (String, String) {
    match env_host() {
        Some(host) => (
            format!("{host}/api/oauth/device_authorization"),
            format!("{host}/api/oauth/token"),
        ),
        None => (
            config.kimi_coding.device_user_code_url.clone(),
            config.kimi_coding.device_token_url.clone(),
        ),
    }
}

/// `readJson` (`:50-57`): `null` on a non-object or parse failure.
fn read_json(text: &str) -> Option<Value> {
    serde_json::from_str::<Value>(text)
        .ok()
        .filter(Value::is_object)
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
    interval_seconds: f64,
    expires_in_seconds: u64,
}

/// `startDeviceAuthorization` (`:59-100`).
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
    if !status.is_success() {
        let suffix = if text.is_empty() {
            String::new()
        } else {
            format!(": {text}")
        };
        return Err(AuthError::Message(format!(
            "Kimi Code device authorization failed with status {}{suffix}",
            status.as_u16()
        )));
    }
    let json = read_json(&text);
    let field = |name: &str| {
        json.as_ref()
            .and_then(|json| json.get(name))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let device_code = field("device_code");
    let user_code = field("user_code");
    let verification_uri = json
        .as_ref()
        .and_then(|json| json.get("verification_uri"))
        .and_then(Value::as_str)
        .and_then(trusted_http_url);
    let verification_uri_complete = json
        .as_ref()
        .and_then(|json| json.get("verification_uri_complete"))
        .and_then(Value::as_str)
        .and_then(trusted_http_url);
    let (
        Some(device_code),
        Some(user_code),
        Some(_verification_uri),
        Some(verification_uri_complete),
    ) = (
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete,
    )
    else {
        return Err(AuthError::Message(format!(
            "Invalid Kimi Code device authorization response: {text}"
        )));
    };
    let interval = json.as_ref().and_then(|json| json.get("interval"));
    let expires_in = json.as_ref().and_then(|json| json.get("expires_in"));
    Ok(DeviceAuthorization {
        device_code,
        user_code,
        // Upstream notifies `verificationUriComplete`.
        verification_uri: verification_uri_complete,
        interval_seconds: positive_number(interval).unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS),
        expires_in_seconds: positive_number(expires_in)
            .map_or(DEVICE_CODE_TIMEOUT_SECONDS, |seconds| seconds as u64),
    })
}

/// `parseTokenResponse` (`:102-122`): the same shape for poll and refresh.
fn parse_token_response(
    json: Option<&Value>,
    operation: &str,
) -> Result<OAuthCredential, AuthError> {
    let field = |name: &str| {
        json.and_then(|json| json.get(name))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let access = field("access_token");
    let refresh = field("refresh_token");
    let expires_in = json
        .and_then(|json| json.get("expires_in"))
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite() && *number > 0.0);
    match (access, refresh, expires_in) {
        (Some(access), Some(refresh), Some(expires_in)) => Ok(OAuthCredential {
            access,
            refresh,
            expires: now_ms().saturating_add((expires_in * 1000.0) as i64),
            account_id: None,
            enterprise_url: None,
            available_model_ids: Vec::new(),
            client_id: None,
            scopes: Vec::new(),
        }),
        _ => Err(AuthError::Message(format!(
            "Kimi Code token {operation} response missing fields: {}",
            json.map_or_else(|| "null".to_string(), ToString::to_string)
        ))),
    }
}

/// `pollForToken` (`:124-190`).
async fn poll_for_token(
    url: &str,
    device: &DeviceAuthorization,
    config: &FlowConfig,
    cancel: &CancelToken,
) -> Result<OAuthCredential, AuthError> {
    let mut options = DeviceCodePollOptions::new(cancel, || async {
        let response = send(
            config
                .client
                .post(url)
                .timeout(REQUEST_TIMEOUT)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("Accept", "application/json")
                .body(form_encode(&[
                    ("client_id", CLIENT_ID),
                    ("device_code", &device.device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
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
        if status.as_u16() >= 500 {
            let suffix = if text.is_empty() {
                String::new()
            } else {
                format!(": {text}")
            };
            return DeviceCodePollResult::Failed(format!(
                "Kimi Code device token request failed with status {}{suffix}",
                status.as_u16()
            ));
        }
        let json = read_json(&text);
        if status.is_success()
            && json
                .as_ref()
                .and_then(|json| json.get("access_token"))
                .is_some_and(Value::is_string)
        {
            return match parse_token_response(json.as_ref(), "poll") {
                Ok(credential) => DeviceCodePollResult::Complete(credential),
                Err(error) => DeviceCodePollResult::Failed(error.to_string()),
            };
        }
        let error = json
            .as_ref()
            .and_then(|json| json.get("error"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let description = json
            .as_ref()
            .and_then(|json| json.get("error_description"))
            .and_then(Value::as_str)
            .map_or_else(String::new, |text| format!(": {text}"));
        match error {
            "authorization_pending" => DeviceCodePollResult::Pending,
            "slow_down" => DeviceCodePollResult::SlowDown {
                interval_seconds: positive_number(
                    json.as_ref().and_then(|json| json.get("interval")),
                ),
            },
            "expired_token" => DeviceCodePollResult::Failed(
                "Kimi Code device authorization expired. Please restart login.".to_string(),
            ),
            "access_denied" => {
                DeviceCodePollResult::Failed("Kimi Code login was denied.".to_string())
            }
            _ => DeviceCodePollResult::Failed(format!(
                "Kimi Code device token request failed (status {}){error}{description}",
                status.as_u16()
            )),
        }
    });
    options.interval_seconds = Some(device.interval_seconds);
    options.expires_in_seconds = Some(device.expires_in_seconds);
    options.wait_before_first_poll = true;
    poll_oauth_device_code_flow(options).await
}

/// `loginKimiCoding` (`:249-261`).
pub async fn login(
    interaction: &Interaction,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let (device_url, token_url) = endpoints(config);
    let cancel = interaction.cancellation();
    let device = start_device_authorization(&device_url, config, &cancel).await?;
    interaction.notify(AuthEvent::DeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
        interval_seconds: Some(device.interval_seconds as u64),
        expires_in_seconds: Some(device.expires_in_seconds),
    });
    poll_for_token(&token_url, &device, config, &cancel).await
}

/// `refreshToken` (`:196-247`), including the 1s/2s/4s bounded back-off and
/// the 401/403/`invalid_grant` "session is dead" branch.
pub async fn refresh(
    refresh_token: &str,
    cancel: &CancelToken,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    let (_, token_url) = endpoints(config);
    let mut last_error: Option<String> = None;
    for attempt in 0..=REFRESH_MAX_RETRIES {
        if attempt > 0 {
            let backoff = 1000_u64 << (attempt - 1);
            super::device_code::abortable_sleep(backoff, cancel, "Kimi Code token refresh aborted")
                .await?;
        }
        if cancel.is_cancelled() {
            return Err(AuthError::Message(
                "Kimi Code token refresh aborted".to_string(),
            ));
        }
        let response = send(
            config
                .client
                .post(&token_url)
                .timeout(REQUEST_TIMEOUT)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("Accept", "application/json")
                .body(form_encode(&[
                    ("client_id", CLIENT_ID),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token),
                ])),
            cancel,
        )
        .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                last_error = Some(error.to_string());
                continue;
            }
        };
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let json = read_json(&text);
        if status.is_success() {
            return parse_token_response(json.as_ref(), "refresh");
        }
        let invalid = status.as_u16() == 401
            || status.as_u16() == 403
            || json
                .as_ref()
                .and_then(|json| json.get("error"))
                .and_then(Value::as_str)
                == Some("invalid_grant");
        if invalid {
            let description = json
                .as_ref()
                .and_then(|json| json.get("error_description"))
                .and_then(Value::as_str)
                .map_or_else(String::new, |text| format!(": {text}"));
            return Err(AuthError::Message(format!(
                "Kimi Code token refresh unauthorized (status {}){description}",
                status.as_u16()
            )));
        }
        let retryable = status.as_u16() == 429 || status.as_u16() >= 500;
        if retryable && attempt < REFRESH_MAX_RETRIES {
            last_error = Some(format!(
                "Kimi Code token refresh failed with status {}",
                status.as_u16()
            ));
            continue;
        }
        return Err(AuthError::Message(format!(
            "Kimi Code token refresh failed with status {}{}",
            status.as_u16(),
            json.as_ref()
                .map_or_else(String::new, |json| format!(": {json}"))
        )));
    }
    Err(AuthError::Message(last_error.unwrap_or_else(|| {
        "Kimi Code token refresh failed".to_string()
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_endpoints_match_upstream() {
        let config = FlowConfig::default();
        assert_eq!(
            config.kimi_coding.device_user_code_url,
            format!("{DEFAULT_OAUTH_HOST}/api/oauth/device_authorization")
        );
        assert_eq!(
            config.kimi_coding.device_token_url,
            "https://auth.kimi.com/api/oauth/token"
        );
    }

    #[test]
    fn token_parsing_reports_missing_fields_with_the_upstream_message() {
        let json = serde_json::json!({"access_token": "a", "refresh_token": "r", "expires_in": 60});
        let credential = parse_token_response(Some(&json), "poll").expect("token");
        assert_eq!(credential.access, "a");
        assert_eq!(credential.refresh, "r");
        assert!(credential.expires > now_ms());
        let error = parse_token_response(None, "refresh")
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "Kimi Code token refresh response missing fields: null"
        );
    }

    #[test]
    fn json_reader_ignores_arrays_and_garbage() {
        assert!(read_json("[]").is_none());
        assert!(read_json("not-json").is_none());
        assert!(read_json(r#"{"a":1}"#).is_some());
    }
}
