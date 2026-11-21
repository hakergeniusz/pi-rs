//! The OAuth flows (`packages/mcp/src/oauth/flow.ts`, adapted from the MCP
//! TypeScript SDK v1.29.0 `src/client/auth.ts`): PKCE, the authorization
//! URL, the token endpoint (exchange and refresh), dynamic client
//! registration, and [`authorize_mcp`] — the whole run including
//! credential invalidation and reruns. Upstream's `adaptOAuthProvider`
//! wrapper is not ported: no pi caller uses it (pi wires
//! `createMcpAuthProvider` instead), and the transport consumes
//! [`crate::auth_provider::AuthProvider`] directly. Upstream's injectable
//! `addClientAuthentication` hook is not ported for the same reason — no
//! pi provider sets one.

use std::collections::HashSet;

use base64::Engine as _;
use reqwest::Method;
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

use crate::auth_provider::McpFetch;
use crate::error::McpError;
use crate::oauth::discovery::{
    AuthorizationServerMetadataOptions, DiscoverOptions, discover_authorization_server_metadata,
    discover_oauth_server_info, select_resource,
};
use crate::oauth::errors::OAuthError;
use crate::oauth::provider::{InvalidateCredentials, McpOAuthProvider};
use crate::oauth::types::{
    AuthorizationServerMetadata, OAuthClientInformationFull, OAuthClientMetadata,
    OAuthDiscoveryState, OAuthServerInfo, OAuthTokens, is_http_url, parse_client_information,
    parse_oauth_tokens,
};

type Result<T> = std::result::Result<T, McpError>;

fn other(message: impl Into<String>) -> McpError {
    McpError::OAuth(OAuthError::Other(message.into()))
}

/// A Client ID Metadata Document: an https URL used as `client_id`, and a
/// redirect URI it lists (`OAuthClientMetadataDocument`, `flow.ts:28-31`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthClientMetadataDocument {
    pub url: String,
    pub redirect_url: String,
}

/// Options of [`authorize_mcp`] (`OAuthFlowOptions`, `flow.ts:69-108`).
#[derive(Debug, Clone, Default)]
pub struct OAuthFlowOptions {
    pub server_url: String,
    pub authorization_code: Option<String>,
    /// `iss` parameter of the authorization response that delivered
    /// `authorization_code` (RFC 9207).
    pub iss: Option<String>,
    pub scope: Option<String>,
    pub resource_metadata_url: Option<String>,
    /// Authorization server metadata document to use instead of discovery,
    /// for servers that advertise a wrong authorization server or none. It
    /// is trusted as configured. Must use https, except on loopback.
    pub authorization_server_metadata_url: Option<String>,
    pub fetch: Option<McpFetch>,
    pub skip_issuer_validation: bool,
    /// Go straight to the authorization redirect instead of refreshing
    /// stored tokens, for example when the server asks for scopes the
    /// current grant lacks (a refresh keeps the old scope).
    pub skip_refresh: bool,
    /// Only refresh the stored tokens: return [`OAuthFlowResult::Redirect`]
    /// instead of registering a client or preparing an authorization
    /// request, so a pending authorization (its `state` and PKCE verifier)
    /// stays usable.
    pub refresh_only: bool,
    /// With `authorization_code`: the redirect URI the authorization
    /// request used. The exchange fails before sending the code when the
    /// flow would send another one (RFC 6749 section 4.1.3 requires the
    /// same).
    pub authorization_redirect_url: Option<String>,
}

/// What a flow achieved (`OAuthFlowResult`, `flow.ts:110`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthFlowResult {
    Authorized,
    Redirect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientAuthMethod {
    ClientSecretBasic,
    ClientSecretPost,
    None,
}

/// Inputs of one token-endpoint request (`TokenRequestOptions`,
/// `flow.ts:112-118`).
pub struct TokenRequestOptions<'a> {
    pub metadata: Option<&'a AuthorizationServerMetadata>,
    pub client_information: &'a OAuthClientInformationFull,
    pub resource: Option<&'a str>,
    pub fetch: Option<&'a McpFetch>,
}

fn is_loopback_hostname(hostname: &str) -> bool {
    hostname == "localhost" || hostname == "127.0.0.1" || hostname == "[::1]" || hostname == "::1"
}

/// The `OpenID Connect` `application_type` for `redirect_uris` (MCP
/// SEP-837). Without one, `OpenID Connect` servers assume `web`, which
/// rejects http loopback redirect URIs. Loopback hosts and custom schemes
/// are native (RFC 8252) (`applicationType`, `flow.ts:132-142`).
fn application_type(redirect_uris: &[String]) -> &'static str {
    let native = redirect_uris.iter().any(|uri| {
        let Ok(url) = Url::parse(uri) else {
            return false;
        };
        (url.scheme() != "http" && url.scheme() != "https")
            || is_loopback_hostname(url.host_str().unwrap_or_default())
    });
    if native { "native" } else { "web" }
}

/// Credential endpoints must be https or loopback (`secureEndpoint`,
/// `flow.ts:144-149`).
fn secure_endpoint(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|error| other(error.to_string()))?;
    if url.scheme() != "https" && !is_loopback_hostname(url.host_str().unwrap_or_default()) {
        return Err(McpError::OAuth(OAuthError::InsecureEndpoint(
            url.to_string(),
        )));
    }
    Ok(url)
}

/// `new URL(path, base)` for the defaulted endpoints.
fn url_join(base: &str, relative: &str) -> Result<Url> {
    let url = Url::parse(base).map_err(|error| other(error.to_string()))?;
    url.join(relative).map_err(|error| other(error.to_string()))
}

/// Picks the client authentication of the token endpoint
/// (`selectClientAuthMethod`, `flow.ts:151-170`): the client's own hint
/// when the server supports it, else basic/post with a secret, else none.
fn select_client_auth_method(
    information: &OAuthClientInformationFull,
    supported: Option<&Vec<String>>,
) -> ClientAuthMethod {
    let supported = supported.map_or(&[][..], Vec::as_slice);
    let hinted = information.metadata.token_endpoint_auth_method.as_deref();
    if let Some(hinted) = hinted.filter(|hinted| {
        let hinted: &str = hinted;
        matches!(
            hinted,
            "client_secret_basic" | "client_secret_post" | "none"
        ) && (supported.is_empty() || supported.iter().any(|method| method.as_str() == hinted))
    }) {
        return match hinted {
            "client_secret_basic" => ClientAuthMethod::ClientSecretBasic,
            "client_secret_post" => ClientAuthMethod::ClientSecretPost,
            _ => ClientAuthMethod::None,
        };
    }
    if supported.is_empty() {
        return if information.client_secret.is_some() {
            ClientAuthMethod::ClientSecretBasic
        } else {
            ClientAuthMethod::None
        };
    }
    let secret = information.client_secret.is_some();
    if secret
        && supported
            .iter()
            .any(|method| method == "client_secret_basic")
    {
        return ClientAuthMethod::ClientSecretBasic;
    }
    if secret
        && supported
            .iter()
            .any(|method| method == "client_secret_post")
    {
        return ClientAuthMethod::ClientSecretPost;
    }
    if supported.iter().any(|method| method == "none") {
        return ClientAuthMethod::None;
    }
    if secret {
        ClientAuthMethod::ClientSecretPost
    } else {
        ClientAuthMethod::None
    }
}

/// `headers.set` / `params.set`: the first occurrence moves to the end
/// with the new value, further duplicates vanish.
fn upsert<K: PartialEq>(items: &mut Vec<(K, String)>, name: K, value: String) {
    items.retain(|(key, _)| *key != name);
    items.push((name, value));
}

/// Applies the selected client authentication
/// (`applyClientAuthentication`, `flow.ts:172-186`).
fn apply_client_authentication(
    method: ClientAuthMethod,
    information: &OAuthClientInformationFull,
    headers: &mut Vec<(&'static str, String)>,
    params: &mut Vec<(String, String)>,
) -> Result<()> {
    match method {
        ClientAuthMethod::ClientSecretBasic => {
            let Some(secret) = &information.client_secret else {
                return Err(other("client_secret_basic requires a client secret"));
            };
            let encoded = base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", information.client_id, secret));
            upsert(headers, "Authorization", format!("Basic {encoded}"));
        }
        ClientAuthMethod::ClientSecretPost | ClientAuthMethod::None => {
            upsert(
                params,
                "client_id".to_string(),
                information.client_id.clone(),
            );
            if method == ClientAuthMethod::ClientSecretPost
                && let Some(secret) = &information.client_secret
            {
                upsert(params, "client_secret".to_string(), secret.clone());
            }
        }
    }
    Ok(())
}

/// The PKCE pair (`pkce`, `flow.ts:188-194`): a 32-byte random
/// base64url-encoded verifier and its SHA-256 challenge.
fn pkce() -> Result<(String, String)> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| other(error.to_string()))?;
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    Ok((verifier, challenge))
}

/// Options of [`start_authorization`] (`flow.ts:196-203`).
pub struct StartAuthorizationOptions<'a> {
    pub metadata: Option<&'a AuthorizationServerMetadata>,
    pub client_information: &'a OAuthClientInformationFull,
    pub redirect_url: &'a str,
    pub scope: Option<&'a str>,
    pub state: Option<&'a str>,
    pub resource: Option<&'a str>,
}

/// The authorization URL and its PKCE verifier (`flow.ts:205-231`).
#[derive(Debug)]
pub struct StartAuthorizationResult {
    pub authorization_url: String,
    pub code_verifier: String,
}

/// Builds the authorization URL with PKCE (`startAuthorization`,
/// `flow.ts:196-231`).
///
/// # Errors
/// On servers without authorization codes or PKCE S256, an invalid
/// endpoint, or an unparsable URL.
pub async fn start_authorization(
    authorization_server_url: &str,
    options: StartAuthorizationOptions<'_>,
) -> Result<StartAuthorizationResult> {
    if let Some(metadata) = options.metadata {
        if !metadata
            .response_types_supported
            .iter()
            .any(|kind| kind == "code")
        {
            return Err(other(
                "Authorization server does not support authorization codes",
            ));
        }
        if let Some(methods) = &metadata.code_challenge_methods_supported
            && !methods.iter().any(|method| method == "S256")
        {
            return Err(other("Authorization server does not support PKCE S256"));
        }
    }
    let endpoint = match options
        .metadata
        .map(|metadata| metadata.authorization_endpoint.clone())
    {
        Some(endpoint) => endpoint,
        None => url_join(authorization_server_url, "/authorize")?.to_string(),
    };
    // Callers open this URL with the platform URL handler, which also
    // launches files and custom protocols (`flow.ts:210-212`).
    let mut url = Url::parse(&endpoint).map_err(|error| other(error.to_string()))?;
    if !is_http_url(&url) {
        return Err(other(format!("Invalid authorization endpoint {url}")));
    }
    let (verifier, challenge) = pkce()?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("response_type", "code");
        pairs.append_pair("client_id", &options.client_information.client_id);
        pairs.append_pair("code_challenge", &challenge);
        pairs.append_pair("code_challenge_method", "S256");
        pairs.append_pair("redirect_uri", options.redirect_url);
        if let Some(state) = options.state.filter(|state| !state.is_empty()) {
            pairs.append_pair("state", state);
        }
        if let Some(scope) = options.scope.filter(|scope| !scope.is_empty()) {
            pairs.append_pair("scope", scope);
        }
        // `offline_access` needs a consent prompt from OpenID Connect
        // servers (`flow.ts:223`).
        if options.scope.is_some_and(|scope| {
            scope
                .split_whitespace()
                .any(|token| token == "offline_access")
        }) {
            pairs.append_pair("prompt", "consent");
        }
        if let Some(resource) = options.resource.filter(|resource| !resource.is_empty()) {
            pairs.append_pair("resource", resource);
        }
    }
    Ok(StartAuthorizationResult {
        authorization_url: url.to_string(),
        code_verifier: verifier,
    })
}

/// One token-endpoint POST (`tokenRequest`, `flow.ts:233-266`). The body
/// is checked for an OAuth error before the status, since servers may
/// report errors with any status.
async fn token_request(
    authorization_server_url: &str,
    options: &TokenRequestOptions<'_>,
    mut params: Vec<(String, String)>,
) -> Result<OAuthTokens> {
    let endpoint = match options
        .metadata
        .map(|metadata| metadata.token_endpoint.clone())
    {
        Some(endpoint) => endpoint,
        None => url_join(authorization_server_url, "/token")?.to_string(),
    };
    let url = secure_endpoint(&endpoint)?;
    if let Some(resource) = options.resource.filter(|resource| !resource.is_empty()) {
        params.push(("resource".to_string(), resource.to_string()));
    }
    let mut headers: Vec<(&'static str, String)> = vec![
        ("accept", "application/json".to_string()),
        (
            "content-type",
            "application/x-www-form-urlencoded".to_string(),
        ),
    ];
    apply_client_authentication(
        select_client_auth_method(
            options.client_information,
            options
                .metadata
                .and_then(|metadata| metadata.token_endpoint_auth_methods_supported.as_ref()),
        ),
        options.client_information,
        &mut headers,
        &mut params,
    )?;
    let fetch = options.fetch.cloned().unwrap_or_default();
    // URLSearchParams.toString() ≡ the url crate's form serializer.
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(
            params
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
        .finish();
    let response = fetch
        .send(
            Method::POST,
            url.as_str(),
            &headers,
            Some(body.into_bytes()),
        )
        .await?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|error| McpError::Network(error.to_string()))?;
    let value: Option<Value> = serde_json::from_str(&text).ok();
    // Servers may report OAuth errors with any status, so check the body
    // before the status (`flow.ts:254-261`).
    let value = value.unwrap_or(Value::Null);
    if let Some(code) = value.get("error").and_then(Value::as_str) {
        let message = value
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or(code);
        let error_uri = value
            .get("error_uri")
            .and_then(Value::as_str)
            .map(str::to_string);
        return Err(McpError::OAuth(OAuthError::Protocol {
            code: code.to_string(),
            message: message.to_string(),
            error_uri,
        }));
    }
    if !(200..300).contains(&status) {
        return Err(McpError::OAuth(OAuthError::Protocol {
            code: "server_error".to_string(),
            message: format!("HTTP {status}: {text}"),
            error_uri: None,
        }));
    }
    parse_oauth_tokens(&value).map_err(McpError::OAuth)
}

/// Options of [`register_client`] (`flow.ts:268-274`).
pub struct RegisterClientOptions<'a> {
    pub metadata: Option<&'a AuthorizationServerMetadata>,
    pub client_metadata: &'a OAuthClientMetadata,
    pub scope: Option<&'a str>,
    pub fetch: Option<&'a McpFetch>,
}

/// Dynamic client registration (`registerClient`, `flow.ts:268-301`): the
/// metadata body gains `application_type` (from the redirect URIs when
/// absent) and the requested scope.
///
/// # Errors
/// [`OAuthError::Registration`] on a failed request, [`OAuthError::Other`]
/// on servers without a registration endpoint or invalid responses.
pub async fn register_client(
    authorization_server_url: &str,
    options: RegisterClientOptions<'_>,
) -> Result<OAuthClientInformationFull> {
    let endpoint = options
        .metadata
        .and_then(|metadata| metadata.registration_endpoint.clone());
    if options.metadata.is_some() && endpoint.is_none() {
        return Err(other(
            "Authorization server does not support dynamic client registration",
        ));
    }
    let url = match endpoint {
        Some(endpoint) => Url::parse(&endpoint).map_err(|error| other(error.to_string()))?,
        None => url_join(authorization_server_url, "/register")?,
    };
    let mut body =
        serde_json::to_value(options.client_metadata).map_err(|error| other(error.to_string()))?;
    if body.get("application_type").is_none() {
        body["application_type"] =
            Value::String(application_type(&options.client_metadata.redirect_uris).to_string());
    }
    if let Some(scope) = options.scope.filter(|scope| !scope.is_empty()) {
        body["scope"] = Value::String(scope.to_string());
    }
    let fetch = options.fetch.cloned().unwrap_or_default();
    let response = fetch
        .send(
            Method::POST,
            url.as_str(),
            &[
                ("accept", "application/json".to_string()),
                ("content-type", "application/json".to_string()),
            ],
            Some(body.to_string().into_bytes()),
        )
        .await?;
    let status = response.status().as_u16();
    if !response.status().is_success() {
        let text = response
            .text()
            .await
            .map_err(|error| McpError::Network(error.to_string()))?;
        return Err(McpError::OAuth(OAuthError::Registration {
            status,
            body: text,
        }));
    }
    let value: Value = response
        .json()
        .await
        .map_err(|error| other(error.to_string()))?;
    parse_client_information(&value).map_err(McpError::OAuth)
}

/// Exchanges an authorization code for tokens
/// (`exchangeAuthorizationCode`, `flow.ts:303-316`).
pub async fn exchange_authorization_code(
    authorization_server_url: &str,
    options: &TokenRequestOptions<'_>,
    code: &str,
    code_verifier: &str,
    redirect_url: &str,
) -> Result<OAuthTokens> {
    token_request(
        authorization_server_url,
        options,
        vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("code".to_string(), code.to_string()),
            ("code_verifier".to_string(), code_verifier.to_string()),
            ("redirect_uri".to_string(), redirect_url.to_string()),
        ],
    )
    .await
}

/// Refreshes tokens (`refreshAuthorization`, `flow.ts:318-328`). A
/// response without a `refresh_token` keeps the old one.
pub async fn refresh_authorization(
    authorization_server_url: &str,
    options: &TokenRequestOptions<'_>,
    refresh_token: &str,
) -> Result<OAuthTokens> {
    let tokens = token_request(
        authorization_server_url,
        options,
        vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.to_string()),
        ],
    )
    .await?;
    let mut merged = tokens;
    if merged.refresh_token.is_none() {
        merged.refresh_token = Some(refresh_token.to_string());
    }
    Ok(merged)
}

/// A response without `scope` grants the requested scope (RFC 6749 §5.1),
/// so it is recorded for a later step-up (`withScope`, `flow.ts:330-334`).
fn with_scope(mut tokens: OAuthTokens, scope: Option<&str>) -> OAuthTokens {
    if tokens.scope.is_none()
        && let Some(scope) = scope.filter(|scope| !scope.is_empty())
    {
        tokens.scope = Some(scope.to_string());
    }
    tokens
}

/// Scopes for a step-up authorization: the challenged scopes plus the ones
/// granted so far, since a challenge may list only the missing scopes and
/// a token with just those would lose access the old one had (SEP-2350).
/// Without challenged scopes, `None` lets the flow pick its default
/// (`stepUpScope`, `flow.ts:336-343`).
pub fn step_up_scope(granted: Option<&str>, challenged: Option<&str>) -> Option<String> {
    let challenged = challenged.filter(|challenged| !challenged.is_empty())?;
    let mut scopes: Vec<&str> = Vec::new();
    let mut seen = HashSet::new();
    for scope in [granted, Some(challenged)]
        .into_iter()
        .flatten()
        .flat_map(|scope| scope.split_whitespace())
    {
        if seen.insert(scope) {
            scopes.push(scope);
        }
    }
    Some(scopes.join(" "))
}

/// What a flow used, for deciding what to invalidate after it failed
/// (`FlowUse`, `flow.ts:345-349`).
#[derive(Default)]
struct FlowUse {
    /// `client_id` of the stored client the flow used; `None` when none
    /// was stored.
    stored_client_id: Option<String>,
}

// Mirrors the TS `runFlow`; split when extending.
#[allow(clippy::too_many_lines)]
async fn run_flow(
    provider: &McpOAuthProvider,
    options: &OAuthFlowOptions,
    flow_use: &mut FlowUse,
) -> Result<OAuthFlowResult> {
    let metadata_url = match &options.authorization_server_metadata_url {
        Some(url) => Some(secure_endpoint(url)?),
        None => None,
    };
    // With a configured metadata URL, discovery is not cached, so changing
    // the URL applies at once (`flow.ts:353-354`).
    let cached = if metadata_url.is_some() {
        None
    } else {
        provider.discovery_state().await?
    };
    let discovered: OAuthServerInfo =
        match cached.filter(|cached| !cached.authorization_server_url.is_empty()) {
            Some(cached) => OAuthServerInfo {
                authorization_server_url: cached.authorization_server_url.clone(),
                authorization_server_metadata: match &cached.authorization_server_metadata {
                    Some(metadata) => Some(metadata.clone()),
                    None => {
                        discover_authorization_server_metadata(
                            &cached.authorization_server_url,
                            AuthorizationServerMetadataOptions {
                                fetch: options.fetch.as_ref(),
                                skip_issuer_validation: options.skip_issuer_validation,
                                ..AuthorizationServerMetadataOptions::default()
                            },
                        )
                        .await?
                    }
                },
                resource_metadata: cached.resource_metadata.clone(),
            },
            None => {
                discover_oauth_server_info(
                    &options.server_url,
                    DiscoverOptions {
                        resource_metadata_url: options.resource_metadata_url.as_deref(),
                        authorization_server_metadata_url: metadata_url.as_ref().map(Url::as_str),
                        fetch: options.fetch.as_ref(),
                        skip_issuer_validation: options.skip_issuer_validation,
                    },
                )
                .await?
            }
        };
    if metadata_url.is_none() {
        provider
            .save_discovery_state(OAuthDiscoveryState {
                authorization_server_url: discovered.authorization_server_url.clone(),
                authorization_server_metadata: discovered.authorization_server_metadata.clone(),
                resource_metadata: discovered.resource_metadata.clone(),
                resource_metadata_url: options.resource_metadata_url.clone(),
            })
            .await?;
    }
    let metadata = discovered.authorization_server_metadata.as_ref();
    let resource = select_resource(&options.server_url, discovered.resource_metadata.as_ref())?;
    // `||`, not `??`: an empty scope (for example from
    // `scopes_supported: []`) falls through to the next source
    // (`flow.ts:365-367`).
    let scope = options
        .scope
        .clone()
        .filter(|scope| !scope.is_empty())
        .or_else(|| {
            discovered
                .resource_metadata
                .as_ref()
                .and_then(|metadata| metadata.scopes_supported.as_ref())
                .map(|scopes| scopes.join(" "))
                .filter(|scope| !scope.is_empty())
        })
        .or_else(|| {
            provider
                .client_metadata()
                .scope
                .clone()
                .filter(|scope| !scope.is_empty())
        });
    let stored = provider.client_information().await?;
    flow_use.stored_client_id = stored.as_ref().map(|client| client.client_id.clone());
    let client_document = if stored.is_none() {
        provider.client_metadata_document(metadata)?
    } else {
        None
    };
    if let Some(document) = &client_document {
        let url = Url::parse(&document.url).map_err(|error| other(error.to_string()))?;
        if url.scheme() != "https" || url.path() == "/" {
            return Err(other("Invalid OAuth client metadata URL"));
        }
    }
    // The document's redirect URI may differ from the provider's, for
    // example by a server-specific path (`flow.ts:374-375`).
    let redirect_url = client_document.as_ref().map_or_else(
        || provider.redirect_url().to_string(),
        |document| document.redirect_url.clone(),
    );
    // Checked first: another redirect URI also explains a client that is
    // missing for the exchange (`flow.ts:377-384`).
    if options.authorization_code.is_some()
        && let Some(authorization_redirect_url) = &options.authorization_redirect_url
        && redirect_url != *authorization_redirect_url
    {
        return Err(other(format!(
            "The code exchange would use the redirect URI {redirect_url}, but the authorization request used {authorization_redirect_url}"
        )));
    }
    let mut client = stored.clone().or_else(|| {
        client_document
            .as_ref()
            .map(|document| OAuthClientInformationFull {
                client_id: document.url.clone(),
                ..OAuthClientInformationFull::default()
            })
    });
    if client.is_none() {
        if options.refresh_only {
            return Ok(OAuthFlowResult::Redirect);
        }
        if options.authorization_code.is_some() {
            return Err(other(
                "OAuth client information is missing during code exchange: the authorization request's client was removed, or the client settings changed",
            ));
        }
        let registered = register_client(
            &discovered.authorization_server_url,
            RegisterClientOptions {
                metadata,
                client_metadata: provider.client_metadata(),
                scope: scope.as_deref(),
                fetch: options.fetch.as_ref(),
            },
        )
        .await?;
        provider.save_client_information(registered.clone()).await?;
        client = Some(registered);
    }
    let client = client.expect("client set above");
    let token_options = TokenRequestOptions {
        metadata,
        client_information: &client,
        resource: resource.as_deref(),
        fetch: options.fetch.as_ref(),
    };
    if let Some(code) = &options.authorization_code {
        // RFC 9207: never send a code from another authorization server to
        // this one. Without metadata there is no issuer to compare with,
        // so any `iss` is rejected (`flow.ts:415-420`).
        let iss = options.iss.as_deref();
        if (iss.is_some()
            || metadata.is_some_and(|metadata| {
                metadata
                    .authorization_response_iss_parameter_supported
                    .unwrap_or(false)
            }))
            && iss != metadata.map(|metadata| metadata.issuer.as_str())
        {
            return Err(McpError::OAuth(OAuthError::IssuerMismatch {
                expected: metadata.map(|metadata| metadata.issuer.clone()),
                received: iss.map(str::to_string),
            }));
        }
        let code_verifier = provider.code_verifier().await?;
        let tokens = exchange_authorization_code(
            &discovered.authorization_server_url,
            &token_options,
            code,
            &code_verifier,
            &redirect_url,
        )
        .await?;
        // A response without `scope` grants the requested scope (RFC 6749
        // §5.1). Recorded so a step-up can keep it. Callers pass the
        // options of the authorization request, so `scope` is what was
        // requested (`flow.ts:421-428`).
        provider
            .save_tokens(with_scope(tokens, scope.as_deref()))
            .await?;
        return Ok(OAuthFlowResult::Authorized);
    }
    let existing = if options.skip_refresh {
        None
    } else {
        provider.tokens().await?
    };
    if let Some(existing) = existing.filter(|tokens| {
        tokens
            .refresh_token
            .as_deref()
            .is_some_and(|token| !token.is_empty())
    }) {
        let refresh_token = existing
            .refresh_token
            .clone()
            .expect("checked non-empty above");
        match refresh_authorization(
            &discovered.authorization_server_url,
            &token_options,
            &refresh_token,
        )
        .await
        {
            Ok(tokens) => {
                // A refresh without `scope` keeps the scope of the grant
                // (RFC 6749 §6) (`flow.ts:434-440`).
                provider
                    .save_tokens(with_scope(tokens, existing.scope.as_deref()))
                    .await?;
                return Ok(OAuthFlowResult::Authorized);
            }
            Err(error) => {
                // Only the errors the flow can retry from fall through to
                // the browser step (`flow.ts:441-444`): a network failure
                // (upstream `TypeError`), a token parse failure, or the
                // `server_error` protocol code. Insecure endpoints and
                // other protocol errors rethrow.
                if matches!(&error, McpError::OAuth(OAuthError::InsecureEndpoint(_))) {
                    return Err(error);
                }
                if let McpError::OAuth(OAuthError::Protocol { code, .. }) = &error
                    && code != "server_error"
                {
                    return Err(error);
                }
            }
        }
    }
    if options.refresh_only {
        return Ok(OAuthFlowResult::Redirect);
    }
    let state = provider.state().await?;
    let authorization = start_authorization(
        &discovered.authorization_server_url,
        StartAuthorizationOptions {
            metadata,
            client_information: &client,
            redirect_url: &redirect_url,
            scope: scope.as_deref(),
            state: Some(&state),
            resource: resource.as_deref(),
        },
    )
    .await?;
    provider
        .save_code_verifier(authorization.code_verifier.clone())
        .await?;
    provider.redirect_to_authorization(&authorization.authorization_url);
    Ok(OAuthFlowResult::Redirect)
}

/// Runs one OAuth flow to authorization or tokens (`authorizeMcp`,
/// `flow.ts:448-478`): a rejected client or grant invalidates the
/// credentials it used and the flow reruns once.
///
/// # Errors
/// Whatever the rerun throws, or the original error when it is not an
/// invalidation case.
pub async fn authorize_mcp(
    provider: &McpOAuthProvider,
    options: OAuthFlowOptions,
) -> Result<OAuthFlowResult> {
    let mut flow_use = FlowUse::default();
    match run_flow(provider, &options, &mut flow_use).await {
        Ok(result) => Ok(result),
        Err(error) => {
            let code = match &error {
                McpError::OAuth(OAuthError::Protocol { code, .. }) => Some(code.as_str()),
                _ => None,
            };
            if matches!(code, Some("invalid_client" | "unauthorized_client")) {
                // A refresh keeps a pending authorization's `state` and
                // verifier: they are the user's sign-in. A client stored
                // meanwhile (by a sign-in that completed) is not the one
                // the server rejected, so it stays (`flow.ts:456-469`).
                if options.refresh_only {
                    let current = provider.client_information().await?;
                    if current.as_ref().map(|client| client.client_id.as_str())
                        == flow_use.stored_client_id.as_deref()
                    {
                        for kind in [
                            InvalidateCredentials::Client,
                            InvalidateCredentials::Tokens,
                            InvalidateCredentials::Discovery,
                        ] {
                            provider.invalidate_credentials(kind).await?;
                        }
                    }
                } else {
                    provider
                        .invalidate_credentials(InvalidateCredentials::All)
                        .await?;
                }
                return run_flow(provider, &options, &mut FlowUse::default()).await;
            }
            if code == Some("invalid_grant") {
                provider
                    .invalidate_credentials(InvalidateCredentials::Tokens)
                    .await?;
                return run_flow(provider, &options, &mut FlowUse::default()).await;
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_up_scope_merges_granted_and_challenged_without_duplicates() {
        assert_eq!(
            step_up_scope(Some("mcp:read mcp:write"), Some("mcp:write mcp:admin")).as_deref(),
            Some("mcp:read mcp:write mcp:admin")
        );
        assert_eq!(
            step_up_scope(None, Some("mcp:read")).as_deref(),
            Some("mcp:read")
        );
        // Without challenged scopes the flow picks its default.
        assert_eq!(step_up_scope(Some("mcp:read"), None), None);
        assert_eq!(step_up_scope(Some("mcp:read"), Some("")), None);
    }

    #[test]
    fn the_selected_client_authentication_follows_the_hint_and_the_server() {
        let client = |auth_method: Option<&str>, secret: Option<&str>| OAuthClientInformationFull {
            metadata: OAuthClientMetadata {
                token_endpoint_auth_method: auth_method.map(str::to_string),
                ..OAuthClientMetadata::default()
            },
            client_id: "cid".to_string(),
            client_secret: secret.map(str::to_string),
            ..OAuthClientInformationFull::default()
        };
        let supported = |methods: &[&str]| {
            Some(
                methods
                    .iter()
                    .copied()
                    .map(str::to_string)
                    .collect::<Vec<_>>(),
            )
        };
        // A hint the server supports wins.
        assert_eq!(
            select_client_auth_method(
                &client(Some("none"), Some("sec")),
                Some(&supported(&["none"]).expect("list"))
            ),
            ClientAuthMethod::None
        );
        // A hint the server does not support is ignored.
        assert_eq!(
            select_client_auth_method(
                &client(Some("client_secret_post"), Some("sec")),
                Some(&supported(&["client_secret_basic"]).expect("list"))
            ),
            ClientAuthMethod::ClientSecretBasic
        );
        // No supported list at all: basic with a secret, none without.
        assert_eq!(
            select_client_auth_method(&client(None, Some("sec")), None),
            ClientAuthMethod::ClientSecretBasic
        );
        assert_eq!(
            select_client_auth_method(&client(None, None), None),
            ClientAuthMethod::None
        );
        // Preference order with a secret: basic, then post, then none.
        assert_eq!(
            select_client_auth_method(
                &client(None, Some("sec")),
                Some(&supported(&["none", "client_secret_post"]).expect("list"))
            ),
            ClientAuthMethod::ClientSecretPost
        );
        assert_eq!(
            select_client_auth_method(
                &client(None, None),
                Some(&supported(&["none"]).expect("list"))
            ),
            ClientAuthMethod::None
        );
        // A secret with neither basic nor post advertised falls to post.
        assert_eq!(
            select_client_auth_method(
                &client(None, Some("sec")),
                Some(&supported(&["client_secret_post"]).expect("list"))
            ),
            ClientAuthMethod::ClientSecretPost
        );
    }

    #[tokio::test]
    async fn the_authorization_url_carries_pkce_and_the_consent_prompt() {
        let client = OAuthClientInformationFull {
            client_id: "cid".to_string(),
            ..OAuthClientInformationFull::default()
        };
        let authorization = start_authorization(
            "https://auth.example",
            StartAuthorizationOptions {
                metadata: None,
                client_information: &client,
                redirect_url: "http://127.0.0.1:9000/callback",
                scope: Some("mcp:read offline_access"),
                state: Some("state-1"),
                resource: Some("https://api.example/mcp"),
            },
        )
        .await
        .expect("authorization");
        let url = Url::parse(&authorization.authorization_url).expect("url");
        assert!(url.as_str().starts_with("https://auth.example/authorize?"));
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        let find = |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(find("response_type"), Some("code"));
        assert_eq!(find("client_id"), Some("cid"));
        assert_eq!(find("code_challenge_method"), Some("S256"));
        assert_eq!(find("redirect_uri"), Some("http://127.0.0.1:9000/callback"));
        assert_eq!(find("state"), Some("state-1"));
        assert_eq!(find("scope"), Some("mcp:read offline_access"));
        assert_eq!(find("resource"), Some("https://api.example/mcp"));
        assert_eq!(find("prompt"), Some("consent"));
        let challenge = find("code_challenge").expect("challenge").to_string();
        // S256: the challenge is the base64url SHA-256 of the verifier.
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest(authorization.code_verifier.as_bytes())),
            challenge
        );
        // Empty scope and state are omitted, and no prompt without
        // offline_access.
        let authorization = start_authorization(
            "https://auth.example",
            StartAuthorizationOptions {
                metadata: None,
                client_information: &client,
                redirect_url: "http://127.0.0.1:9000/callback",
                scope: Some(""),
                state: None,
                resource: None,
            },
        )
        .await
        .expect("authorization");
        assert!(!authorization.authorization_url.contains("scope="));
        assert!(!authorization.authorization_url.contains("state="));
        assert!(!authorization.authorization_url.contains("prompt="));
    }

    #[tokio::test]
    async fn servers_without_authorization_codes_or_s256_fail_before_the_redirect() {
        let client = OAuthClientInformationFull {
            client_id: "cid".to_string(),
            ..OAuthClientInformationFull::default()
        };
        let metadata = AuthorizationServerMetadata {
            response_types_supported: vec!["token".to_string()],
            ..AuthorizationServerMetadata::default()
        };
        let error = start_authorization(
            "https://auth.example",
            StartAuthorizationOptions {
                metadata: Some(&metadata),
                client_information: &client,
                redirect_url: "http://127.0.0.1/callback",
                scope: None,
                state: None,
                resource: None,
            },
        )
        .await
        .expect_err("no authorization codes");
        assert_eq!(
            error.to_string(),
            "Authorization server does not support authorization codes"
        );
        let metadata = AuthorizationServerMetadata {
            response_types_supported: vec!["code".to_string()],
            code_challenge_methods_supported: Some(vec!["plain".to_string()]),
            ..AuthorizationServerMetadata::default()
        };
        let error = start_authorization(
            "https://auth.example",
            StartAuthorizationOptions {
                metadata: Some(&metadata),
                client_information: &client,
                redirect_url: "http://127.0.0.1/callback",
                scope: None,
                state: None,
                resource: None,
            },
        )
        .await
        .expect_err("no S256");
        assert_eq!(
            error.to_string(),
            "Authorization server does not support PKCE S256"
        );
        // Non-http endpoints are refused (the platform URL handler would
        // launch them).
        let metadata = AuthorizationServerMetadata {
            response_types_supported: vec!["code".to_string()],
            authorization_endpoint: "file:///etc/passwd".to_string(),
            token_endpoint: "https://auth.example/token".to_string(),
            issuer: "https://auth.example".to_string(),
            ..AuthorizationServerMetadata::default()
        };
        let error = start_authorization(
            "https://auth.example",
            StartAuthorizationOptions {
                metadata: Some(&metadata),
                client_information: &client,
                redirect_url: "http://127.0.0.1/callback",
                scope: None,
                state: None,
                resource: None,
            },
        )
        .await
        .expect_err("file endpoint");
        assert_eq!(
            error.to_string(),
            "Invalid authorization endpoint file:///etc/passwd"
        );
    }
}
