//! Shared HTTP and URL helpers for the built-in OAuth flows.
//!
//! Ground truth (`packages/ai/src/auth/oauth/`):
//! - `pkce.ts:9-15` — the base64url encoder these helpers share.
//! - `anthropic.ts:29-57` — `parseAuthorizationInput` (URL, `code#state`,
//!   query-string and bare-code forms).
//! - `openai-codex.ts:69-97` — the same parser with a `State mismatch` caller.
//! - `github-copilot.ts:42-63` — domain normalization and endpoint table.
//! - `openai-codex.ts:122-143` — token response field validation.

use std::collections::BTreeMap;

use reqwest::Response;
use serde_json::Value;

use super::{AuthError, CancelToken};

/// `encodeURIComponent`-style form encoding as `URLSearchParams` produces it
/// (`openai-codex.ts:154-160`, `:173-177`): spaces become `+`, not `%20`.
#[must_use]
pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", form_encode_one(key), form_encode_one(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn form_encode_one(value: &str) -> String {
    let mut result = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(char::from(*byte));
            }
            b' ' => result.push('+'),
            other => {
                result.push('%');
                result.push(char::from(HEX[(other >> 4) as usize]));
                result.push(char::from(HEX[(other & 0x0f) as usize]));
            }
        }
    }
    result
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Decode percent escapes and `+` (`URLSearchParams.get`).
#[must_use]
pub fn form_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                result.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let high = hex_digit(bytes[index + 1]);
                let low = hex_digit(bytes[index + 2]);
                if let (Some(high), Some(low)) = (high, low) {
                    result.push((high << 4) | low);
                    index += 3;
                } else {
                    result.push(bytes[index]);
                    index += 1;
                }
            }
            other => {
                result.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&result).into_owned()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The query of an authorization redirect, or of a pasted URL.
#[must_use]
pub fn parse_query(query: &str) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        result.insert(form_decode(key), form_decode(value));
    }
    result
}

/// `parseAuthorizationInput` (`anthropic.ts:29-57`, `openai-codex.ts:69-97`).
#[must_use]
pub fn parse_authorization_input(input: &str, verifier: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }
    if let Some((scheme, rest)) = value.split_once("://")
        && matches!(scheme, "http" | "https")
    {
        let (_, query) = rest.split_once('?').unwrap_or((rest, ""));
        let params = parse_query(query);
        return (
            params.get("code").cloned(),
            params
                .get("state")
                .cloned()
                .or_else(|| Some(verifier.to_string())),
        );
    }
    if let Some((code, state)) = value.split_once('#') {
        return (Some(code.to_string()), Some(state.to_string()));
    }
    if value.contains("code=") {
        let params = parse_query(value);
        return (
            params.get("code").cloned(),
            params
                .get("state")
                .cloned()
                .or_else(|| Some(verifier.to_string())),
        );
    }
    (Some(value.to_string()), Some(verifier.to_string()))
}

/// `normalizeDomain` (`github-copilot.ts:42-51`).
#[must_use]
pub fn normalize_domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    // The upstream parser only keeps the hostname; reject anything that does
    // not look like a host (spaces, paths with no host, empty host).
    let rest = with_scheme.split_once("://")?.1;
    let host = rest.split(['/', '?', '#', ':']).next()?.trim();
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return None;
    }
    Some(host.to_lowercase())
}

/// `fetchWithLoginCancellation` (`openai-codex.ts:111-120`): map a transport
/// error on an aborted flow to upstream's `Login cancelled`.
///
/// # Errors
/// The request error, or [`AuthError::Cancelled`] when aborted.
pub async fn send(
    request: reqwest::RequestBuilder,
    cancel: &CancelToken,
) -> Result<Response, AuthError> {
    let response = request.send().await.map_err(|error| {
        if cancel.is_cancelled() {
            AuthError::Cancelled
        } else {
            AuthError::Message(error.to_string())
        }
    })?;
    if cancel.is_cancelled() {
        return Err(AuthError::Cancelled);
    }
    Ok(response)
}

/// `readTokenResponse`'s JSON step (`openai-codex.ts:128-143`).
///
/// # Errors
/// When the body is not a JSON object.
pub async fn response_json(response: Response) -> Result<Value, AuthError> {
    let value = response.json::<Value>().await.map_err(|error| {
        AuthError::Message(format!("OpenAI Codex token response invalid JSON: {error}"))
    })?;
    if !value.is_object() {
        return Err(AuthError::Message(format!(
            "OpenAI Codex token response missing fields: {value}"
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_encoding_matches_url_search_params() {
        assert_eq!(
            form_encode(&[("grant_type", "authorization code"), ("state", "a+b/c?")]),
            "grant_type=authorization+code&state=a%2Bb%2Fc%3F"
        );
    }

    #[test]
    fn query_parsing_decodes_escapes() {
        let query = parse_query("code=a%2Bb&state=x+y&error=");
        assert_eq!(query.get("code").map(String::as_str), Some("a+b"));
        assert_eq!(query.get("state").map(String::as_str), Some("x y"));
        assert_eq!(query.get("error").map(String::as_str), Some(""));
    }

    #[test]
    fn authorization_input_covers_all_upstream_forms() {
        assert_eq!(
            parse_authorization_input(" https://claude.ai/cb?code=abc&state=s ", "v"),
            (Some("abc".to_string()), Some("s".to_string()))
        );
        assert_eq!(
            parse_authorization_input("code#state", "v"),
            (Some("code".to_string()), Some("state".to_string()))
        );
        assert_eq!(
            parse_authorization_input("code=abc&state=s", "v"),
            (Some("abc".to_string()), Some("s".to_string()))
        );
        assert_eq!(
            parse_authorization_input("bare-code", "v"),
            (Some("bare-code".to_string()), Some("v".to_string()))
        );
        assert_eq!(parse_authorization_input("  ", "v"), (None, None));
    }

    #[test]
    fn enterprise_domains_normalize_like_upstream() {
        assert_eq!(
            normalize_domain("company.ghe.com"),
            Some("company.ghe.com".to_string())
        );
        assert_eq!(
            normalize_domain("https://Company.GHE.com/x"),
            Some("company.ghe.com".to_string())
        );
        assert_eq!(normalize_domain(" "), None);
    }
}
