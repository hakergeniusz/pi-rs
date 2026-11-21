//! OAuth wire types and their structural validation
//! (`packages/mcp/src/oauth/types.ts`, adapted from the MCP TypeScript
//! SDK v1.29.0 with dependency-free validation). Field names are the wire
//! names (`access_token`, `client_id`, …); unknown fields ride a flattened
//! map so they survive a save/load round trip like the TS spreads do.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

use super::errors::OAuthError;

type Result<T> = std::result::Result<T, OAuthError>;

fn object<'a>(value: &'a Value, name: &str) -> Result<&'a Map<String, Value>> {
    let Some(input) = value.as_object() else {
        return Err(OAuthError::Other(format!("Invalid {name}")));
    };
    Ok(input)
}

fn required_string(input: &Map<String, Value>, key: &str, name: &str) -> Result<String> {
    match input.get(key) {
        Some(Value::String(text)) if !text.is_empty() => Ok(text.clone()),
        _ => Err(OAuthError::Other(format!("Invalid {name}"))),
    }
}

/// Treats `null` and `""` as absent: servers send them for fields they have
/// no value for, like `scope: ""` (`absent`, `types.ts:111-113`).
fn absent(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(text)) => text.is_empty(),
        _ => false,
    }
}

fn optional_string(input: &Map<String, Value>, key: &str, name: &str) -> Result<Option<String>> {
    if absent(input.get(key)) {
        return Ok(None);
    }
    Ok(Some(required_string(input, key, name)?))
}

fn optional_strings(
    input: &Map<String, Value>,
    key: &str,
    name: &str,
) -> Result<Option<Vec<String>>> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(text) => out.push(text.clone()),
                    _ => return Err(OAuthError::Other(format!("Invalid {name}"))),
                }
            }
            Ok(Some(out))
        }
        _ => Err(OAuthError::Other(format!("Invalid {name}"))),
    }
}

/// Accepts only `http:` and `https:` URLs. Servers supply these URLs, and
/// the authorization endpoint is opened with the platform URL handler, so
/// a `file:` or custom protocol URL would launch local programs
/// (`safeUrl`, `types.ts:130-135`).
fn safe_url(input: &Map<String, Value>, key: &str, name: &str) -> Result<String> {
    let text = required_string(input, key, name)?;
    safe_url_value(&text, name)
}

fn safe_url_value(text: &str, name: &str) -> Result<String> {
    match Url::parse(text) {
        Ok(url) if is_http_url(&url) => Ok(text.to_string()),
        _ => Err(OAuthError::Other(format!("Invalid {name}"))),
    }
}

/// `isHttpUrl` (`types.ts:137-139`).
pub fn is_http_url(url: &Url) -> bool {
    url.scheme() == "http" || url.scheme() == "https"
}

fn optional_url(input: &Map<String, Value>, key: &str, name: &str) -> Result<Option<String>> {
    if absent(input.get(key)) {
        return Ok(None);
    }
    Ok(Some(safe_url(input, key, name)?))
}

/// Unknown fields of one metadata document, kept through parse/serialize
/// (the TS index signatures + spreads).
fn extras(input: &Map<String, Value>, known: &[&str]) -> Map<String, Value> {
    input
        .iter()
        .filter(|(key, _)| !known.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// `OAuthProtectedResourceMetadata` (`types.ts:9-14`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct OAuthProtectedResourceMetadata {
    pub resource: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_servers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `AuthorizationServerMetadata` (`types.ts:16-30`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AuthorizationServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
    pub response_types_supported: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant_types_supported: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_challenge_methods_supported: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id_metadata_document_supported: Option<bool>,
    /// Whether authorization responses carry an `iss` parameter (RFC 9207).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_response_iss_parameter_supported: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `OAuthTokens` (`types.ts:32-39`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct OAuthTokens {
    pub access_token: String,
    pub token_type: String,
    /// The wire number is kept verbatim (`types.ts:191-197` — `Number(null)`
    /// is 0, which would mark the token expired at once, and servers send
    /// strings like `"3600"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<serde_json::Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
}

/// `OAuthClientMetadata` (`types.ts:41-63`): the registration body. The
/// provider sets `client_name` and the redirect URIs; a stored
/// registration parses the server's echo into the same shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct OAuthClientMetadata {
    pub redirect_uris: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant_types: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_types: Option<Vec<String>>,
    /// `OpenID Connect` client type, `native` or `web` — derived from the
    /// redirect URIs when absent (MCP SEP-837).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contacts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tos_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwks: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub software_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub software_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub software_statement: Option<String>,
}

/// `OAuthClientInformationFull` — the stored registration: the client keys
/// and the server's metadata echo in one flat object
/// (`OAuthClientInformation & OAuthClientMetadata`, `types.ts:72`).
/// `parse_client_information` accepts the bare `OAuthClientInformation`
/// shape too (`OAuthClientInformationMixed`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct OAuthClientInformationFull {
    #[serde(flatten)]
    pub metadata: OAuthClientMetadata,
    pub client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id_issued_at: Option<serde_json::Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret_expires_at: Option<serde_json::Number>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Cached discovery state of one MCP server
/// (`OAuthDiscoveryState`, `types.ts:73-78`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct OAuthDiscoveryState {
    pub authorization_server_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_server_metadata: Option<AuthorizationServerMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_metadata: Option<OAuthProtectedResourceMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_metadata_url: Option<String>,
}

/// Both metadata documents of one server (`OAuthServerInfo`,
/// `types.ts:82-86`) — the result of [`super::discovery`] discovery.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OAuthServerInfo {
    pub authorization_server_url: String,
    pub authorization_server_metadata: Option<AuthorizationServerMetadata>,
    pub resource_metadata: Option<OAuthProtectedResourceMetadata>,
}

/// One rejected request's challenge (`OAuthChallenge`, `types.ts:88-93`):
/// the fields of a bearer or `DPoP` `WWW-Authenticate` header the flows
/// consume.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OAuthChallenge {
    pub resource_metadata_url: Option<String>,
    pub scope: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// Parses one `OAuthProtectedResourceMetadata` body
/// (`parseProtectedResourceMetadata`, `types.ts:145-155`).
pub fn parse_protected_resource_metadata(value: &Value) -> Result<OAuthProtectedResourceMetadata> {
    let input = object(value, "OAuth protected resource metadata")?;
    let known = ["resource", "authorization_servers", "scopes_supported"];
    Ok(OAuthProtectedResourceMetadata {
        resource: safe_url(
            input,
            "resource",
            "OAuth protected resource metadata resource",
        )?,
        authorization_servers: optional_strings(
            input,
            "authorization_servers",
            "authorization_servers",
        )?
        .map(|urls| {
            urls.into_iter()
                .map(|url| safe_url_value(&url, "authorization server URL"))
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?,
        scopes_supported: optional_strings(input, "scopes_supported", "scopes_supported")?,
        extra: extras(input, &known),
    })
}

/// Parses one authorization server metadata document
/// (`parseAuthorizationServerMetadata`, `types.ts:157-187`).
pub fn parse_authorization_server_metadata(value: &Value) -> Result<AuthorizationServerMetadata> {
    let input = object(value, "authorization server metadata")?;
    let known = [
        "issuer",
        "authorization_endpoint",
        "token_endpoint",
        "registration_endpoint",
        "scopes_supported",
        "response_types_supported",
        "grant_types_supported",
        "token_endpoint_auth_methods_supported",
        "code_challenge_methods_supported",
        "client_id_metadata_document_supported",
        "authorization_response_iss_parameter_supported",
    ];
    let response_types = optional_strings(
        input,
        "response_types_supported",
        "response_types_supported",
    )?
    .ok_or_else(|| OAuthError::Other("Invalid response_types_supported".to_string()))?;
    Ok(AuthorizationServerMetadata {
        issuer: safe_url(input, "issuer", "authorization server issuer")?,
        authorization_endpoint: safe_url(
            input,
            "authorization_endpoint",
            "authorization endpoint",
        )?,
        token_endpoint: safe_url(input, "token_endpoint", "token endpoint")?,
        registration_endpoint: optional_url(
            input,
            "registration_endpoint",
            "registration endpoint",
        )?,
        scopes_supported: optional_strings(input, "scopes_supported", "scopes_supported")?,
        response_types_supported: response_types,
        grant_types_supported: optional_strings(
            input,
            "grant_types_supported",
            "grant_types_supported",
        )?,
        token_endpoint_auth_methods_supported: optional_strings(
            input,
            "token_endpoint_auth_methods_supported",
            "token_endpoint_auth_methods_supported",
        )?,
        code_challenge_methods_supported: optional_strings(
            input,
            "code_challenge_methods_supported",
            "code_challenge_methods_supported",
        )?,
        // A non-boolean value becomes absent, not an error (`types.ts:178-185`).
        client_id_metadata_document_supported: input
            .get("client_id_metadata_document_supported")
            .and_then(Value::as_bool),
        authorization_response_iss_parameter_supported: input
            .get("authorization_response_iss_parameter_supported")
            .and_then(Value::as_bool),
        extra: extras(input, &known),
    })
}

/// Parses one token response (`parseOAuthTokens`, `types.ts:189-202`).
pub fn parse_oauth_tokens(value: &Value) -> Result<OAuthTokens> {
    let input = object(value, "OAuth token response")?;
    Ok(OAuthTokens {
        access_token: required_string(input, "access_token", "access_token")?,
        token_type: required_string(input, "token_type", "token_type")?,
        expires_in: optional_number(input, "expires_in", "expires_in")?,
        scope: optional_string(input, "scope", "scope")?,
        refresh_token: optional_string(input, "refresh_token", "refresh_token")?,
        id_token: optional_string(input, "id_token", "id_token")?,
    })
}

/// `Number(value)` with the `Number(null)` trap removed: absent → `None`,
/// numeric strings parse like JavaScript, non-finite throws
/// (`types.ts:191-193`).
fn optional_number(
    input: &Map<String, Value>,
    key: &str,
    name: &str,
) -> Result<Option<serde_json::Number>> {
    let Some(value) = input.get(key) else {
        return Ok(None);
    };
    if absent(Some(value)) {
        return Ok(None);
    }
    let as_f64 = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    match as_f64
        .filter(|value| value.is_finite())
        .and_then(serde_json::Number::from_f64)
    {
        Some(number) => Ok(Some(number)),
        None => Err(OAuthError::Other(format!("Invalid {name}"))),
    }
}

/// Parses one dynamic-client-registration response
/// (`parseClientInformation`, `types.ts:204-215`): the metadata echo rides
/// the flattened struct, `redirect_uris` defaults to `[]`.
pub fn parse_client_information(value: &Value) -> Result<OAuthClientInformationFull> {
    let input = object(value, "OAuth client registration response")?;
    let known = [
        "client_id",
        "client_secret",
        "client_id_issued_at",
        "client_secret_expires_at",
        "redirect_uris",
        "token_endpoint_auth_method",
        "grant_types",
        "response_types",
        "application_type",
        "client_name",
        "client_uri",
        "logo_uri",
        "scope",
        "contacts",
        "tos_uri",
        "policy_uri",
        "jwks_uri",
        "jwks",
        "software_id",
        "software_version",
        "software_statement",
    ];
    let mut metadata: OAuthClientMetadata =
        serde_json::from_value(value.clone()).unwrap_or_default();
    metadata.redirect_uris =
        optional_strings(input, "redirect_uris", "redirect_uris")?.unwrap_or_default();
    Ok(OAuthClientInformationFull {
        client_id: required_string(input, "client_id", "client_id")?,
        client_secret: optional_string(input, "client_secret", "client_secret")?,
        client_id_issued_at: input
            .get("client_id_issued_at")
            .and_then(Value::as_number)
            .cloned(),
        client_secret_expires_at: input
            .get("client_secret_expires_at")
            .and_then(Value::as_number)
            .cloned(),
        metadata,
        extra: extras(input, &known),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Number, json};

    #[test]
    fn token_parsing_keeps_the_wire_expires_in_and_treats_null_scope_as_absent() {
        let tokens = parse_oauth_tokens(&json!({
            "access_token": "at",
            "token_type": "Bearer",
            "expires_in": "3600",
            "scope": null,
            "refresh_token": "rt"
        }))
        .expect("tokens");
        assert_eq!(tokens.access_token, "at");
        assert_eq!(
            tokens.expires_in.as_ref().and_then(Number::as_f64),
            Some(3600.0)
        );
        assert_eq!(tokens.scope, None);
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt"));
        // A missing access_token, a non-object, and a bad number all fail.
        let error =
            parse_oauth_tokens(&json!({"token_type": "Bearer"})).expect_err("no access token");
        assert_eq!(error.to_string(), "Invalid access_token");
        let error = parse_oauth_tokens(&Value::Null).expect_err("null");
        assert_eq!(error.to_string(), "Invalid OAuth token response");
        let error = parse_oauth_tokens(
            &json!({"access_token": "at", "token_type": "B", "expires_in": "soon"}),
        )
        .expect_err("bad number");
        assert_eq!(error.to_string(), "Invalid expires_in");
    }

    #[test]
    fn authorization_server_metadata_requires_response_types() {
        let error = parse_authorization_server_metadata(&json!({
            "issuer": "https://auth.example",
            "authorization_endpoint": "https://auth.example/authorize",
            "token_endpoint": "https://auth.example/token"
        }))
        .expect_err("no response types");
        assert_eq!(error.to_string(), "Invalid response_types_supported");
        let metadata = parse_authorization_server_metadata(&json!({
            "issuer": "https://auth.example/",
            "authorization_endpoint": "https://auth.example/authorize",
            "token_endpoint": "https://auth.example/token",
            "response_types_supported": ["code"],
            "unknown_field": {"nested": true}
        }))
        .expect("metadata");
        assert_eq!(metadata.issuer, "https://auth.example/");
        // Unknown fields survive the parse for the save/load round trip.
        assert_eq!(
            metadata.extra.get("unknown_field"),
            Some(&json!({"nested": true}))
        );
    }

    #[test]
    fn client_information_accepts_the_bare_shape_and_keeps_extras() {
        let information = parse_client_information(&json!({"client_id": "cid"})).expect("bare");
        assert_eq!(information.client_id, "cid");
        assert!(information.client_secret.is_none());
        assert_eq!(information.metadata.redirect_uris, Vec::<String>::new());
        let information = parse_client_information(&json!({
            "client_id": "cid",
            "client_secret": "sec",
            "redirect_uris": ["http://127.0.0.1/callback"],
            "client_id_issued_at": 17,
            "token_endpoint_auth_method": "none",
            "custom": "kept"
        }))
        .expect("full");
        assert_eq!(
            information.metadata.redirect_uris,
            vec!["http://127.0.0.1/callback".to_string()]
        );
        assert_eq!(
            information.metadata.token_endpoint_auth_method.as_deref(),
            Some("none")
        );
        assert_eq!(information.extra.get("custom"), Some(&json!("kept")));
        // The JSON round trip keeps the flat registration shape.
        let round = serde_json::to_value(&information).expect("serialize");
        assert_eq!(round["client_id"], json!("cid"));
        assert_eq!(round["redirect_uris"], json!(["http://127.0.0.1/callback"]));
        assert_eq!(round["custom"], json!("kept"));
    }
}
