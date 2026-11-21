//! OAuth discovery (`packages/mcp/src/oauth/discovery.ts:1-215`,
//! adapted from the MCP TypeScript SDK v1.29.0 `src/client/auth.ts`):
//! the `WWW-Authenticate` challenge parser, the protected-resource and
//! authorization-server metadata discovery chains, and the resource
//! selection the flows send as the `resource` parameter.
//!
//! Network failures surface as [`McpError::Network`] (upstream `TypeError`),
//! discovery misses as `None`; every plain upstream `Error` throw keeps its
//! message in [`OAuthError::Other`].

use url::Url;

use crate::auth_provider::McpFetch;
use crate::error::McpError;
use crate::oauth::errors::OAuthError;
use crate::oauth::types::{
    AuthorizationServerMetadata, OAuthChallenge, OAuthProtectedResourceMetadata, OAuthServerInfo,
    parse_authorization_server_metadata, parse_protected_resource_metadata,
};

type Result<T> = std::result::Result<T, McpError>;

fn other(message: impl Into<String>) -> McpError {
    McpError::OAuth(OAuthError::Other(message.into()))
}

/// 4xx and 502 mean "not here", so discovery tries the next candidate URL
/// (`isDiscoveryMiss`, `discovery.ts:24-26`).
fn is_discovery_miss(status: u16) -> bool {
    (400..500).contains(&status) || status == 502
}

/// Path suffix for `/.well-known/<kind><path>`; empty for the root path
/// (`pathSuffix`, `discovery.ts:29-31`).
fn path_suffix(pathname: &str) -> String {
    pathname.strip_suffix('/').unwrap_or(pathname).to_string()
}

/// `(?:^|[,\s])name=(?:"([^"]*)"|([^\s,]+))` /i (`field`, `discovery.ts:33-37`),
/// matched case-insensitively with the captured value keeping its case. The
/// leftmost occurrence where the whole pattern matches wins; its value is
/// `match?.[1] || match?.[2] || undefined`, so a quoted `""` is absent.
fn field(header: &str, name: &str) -> Option<String> {
    let lowered = header.to_lowercase();
    let needle = format!("{}=", name.to_lowercase());
    let mut from = 0;
    while let Some(found) = lowered[from..].find(&needle) {
        let at = from + found;
        let preceded = at == 0
            || lowered[..at]
                .chars()
                .next_back()
                .is_some_and(|char| char == ',' || char.is_whitespace());
        let rest_offset = at + needle.len();
        let lowered_rest = &lowered[rest_offset..];
        let raw_rest = &header[rest_offset..];
        let captured = if let Some(quoted) = lowered_rest.strip_prefix('"') {
            // `"([^"]*)"`: a closing quote makes the whole pattern match,
            // even around an empty value.
            quoted.find('"').map(|end| raw_rest[1..=end].to_string())
        } else {
            // `[^\s,]+`: without one more character the pattern does not
            // match at this position, so the scan continues.
            let end = lowered_rest
                .find(|char: char| char.is_whitespace() || char == ',')
                .unwrap_or(lowered_rest.len());
            (end > 0).then(|| raw_rest[..end].to_string())
        };
        let Some(captured) = captured else {
            // The pattern does not match at this anchor at all; look for
            // the next occurrence of `name=`.
            from = at + 1;
            continue;
        };
        if preceded {
            // An empty value (`scope=""`) carries no information, so it
            // counts as absent — but the scan stops there, like the
            // upstream `match?.[1] || match?.[2] || undefined` on the
            // leftmost whole-pattern match.
            return (!captured.is_empty()).then_some(captured);
        }
        from = at + 1;
    }
    None
}

/// The challenge of one rejected request (`parseWwwAuthenticate`,
/// `discovery.ts:39-56`): bearer and `DPoP` schemes carry the fields the
/// flows consume, other schemes challenge nothing.
pub fn parse_www_authenticate(header: Option<&str>) -> OAuthChallenge {
    let Some(header) = header else {
        return OAuthChallenge::default();
    };
    let scheme = header
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    if scheme != "bearer" && scheme != "dpop" {
        return OAuthChallenge::default();
    }
    let resource_metadata = field(header, "resource_metadata");
    OAuthChallenge {
        resource_metadata_url: resource_metadata
            .and_then(|value| Url::parse(&value).ok().map(|url| url.to_string())),
        scope: field(header, "scope"),
        error: field(header, "error"),
        error_description: field(header, "error_description"),
    }
}

/// One metadata document fetch (`fetchMetadata`, `discovery.ts:58-68`):
/// JSON acceptance with the protocol version header. Network failures stay
/// [`McpError::Network`] — the one error class discovery rethrows.
async fn fetch_metadata(
    url: &str,
    fetch: &McpFetch,
    protocol_version: &str,
) -> Result<reqwest::Response> {
    fetch
        .send(
            reqwest::Method::GET,
            url,
            &[
                ("accept", "application/json".to_string()),
                ("MCP-Protocol-Version", protocol_version.to_string()),
            ],
            None,
        )
        .await
}

/// Options of [`discover_protected_resource_metadata`]
/// (`discovery.ts:71-77`). The injectable `fetch` of upstream becomes the
/// [`McpFetch`] client; there is no abort signal — the host cancels by
/// dropping the future.
#[derive(Clone, Default)]
pub struct ProtectedResourceMetadataOptions<'a> {
    /// Metadata URL from a challenge; without one the well-known URL is
    /// built from the server's path.
    pub resource_metadata_url: Option<&'a str>,
    pub protocol_version: Option<&'a str>,
    pub fetch: Option<&'a McpFetch>,
}

/// Protected resource metadata of one server
/// (`discoverProtectedResourceMetadata`, `discovery.ts:70-104`). Without a
/// configured URL the origin root is the fallback path.
pub async fn discover_protected_resource_metadata(
    server_url: &str,
    options: ProtectedResourceMetadataOptions<'_>,
) -> Result<OAuthProtectedResourceMetadata> {
    let server = Url::parse(server_url).map_err(|error| other(error.to_string()))?;
    let fetch = options.fetch.cloned().unwrap_or_default();
    let version = options
        .protocol_version
        .unwrap_or(crate::LATEST_PROTOCOL_VERSION);
    let origin = server.origin().ascii_serialization();
    let mut response = fetch_metadata(
        &match options.resource_metadata_url {
            Some(url) => url.to_string(),
            None => format!(
                "{origin}/.well-known/oauth-protected-resource{}",
                path_suffix(server.path())
            ),
        },
        &fetch,
        version,
    )
    .await?;
    if options.resource_metadata_url.is_none()
        && server.path() != "/"
        && is_discovery_miss(response.status().as_u16())
    {
        response = fetch_metadata(
            &format!("{origin}/.well-known/oauth-protected-resource"),
            &fetch,
            version,
        )
        .await?;
    }
    if !response.status().is_success() {
        return Err(other(format!(
            "HTTP {} loading OAuth protected resource metadata",
            response.status().as_u16()
        )));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|error| other(error.to_string()))?;
    parse_protected_resource_metadata(&body).map_err(McpError::OAuth)
}

/// Options of [`discover_authorization_server_metadata`]
/// (`discovery.ts:120`).
#[derive(Clone, Default)]
pub struct AuthorizationServerMetadataOptions<'a> {
    pub protocol_version: Option<&'a str>,
    pub fetch: Option<&'a McpFetch>,
    pub skip_issuer_validation: bool,
}

/// The discovery candidates of one issuer, oauth before oidc, plus the
/// path-scoped openid document for issuers with a path
/// (`buildAuthorizationServerDiscoveryUrls`, `discovery.ts:106-117`).
pub fn build_authorization_server_discovery_urls(
    authorization_server_url: &str,
) -> Result<Vec<(String, &'static str)>> {
    let issuer = Url::parse(authorization_server_url).map_err(|error| other(error.to_string()))?;
    let origin = issuer.origin().ascii_serialization();
    let path = path_suffix(issuer.path());
    let mut urls = vec![
        (
            format!("{origin}/.well-known/oauth-authorization-server{path}"),
            "oauth",
        ),
        (
            format!("{origin}/.well-known/openid-configuration{path}"),
            "oidc",
        ),
    ];
    if !path.is_empty() {
        urls.push((
            format!("{origin}{path}/.well-known/openid-configuration"),
            "oidc",
        ));
    }
    Ok(urls)
}

/// Authorization server metadata of one issuer
/// (`discoverAuthorizationServerMetadata`, `discovery.ts:119-146`): the
/// first candidate that answers with a valid document wins; a valid
/// document from another issuer fails hard unless validation is skipped.
pub async fn discover_authorization_server_metadata(
    authorization_server_url: &str,
    options: AuthorizationServerMetadataOptions<'_>,
) -> Result<Option<AuthorizationServerMetadata>> {
    let fetch = options.fetch.cloned().unwrap_or_default();
    let version = options
        .protocol_version
        .unwrap_or(crate::LATEST_PROTOCOL_VERSION);
    for (url, _kind) in build_authorization_server_discovery_urls(authorization_server_url)? {
        let response = fetch_metadata(&url, &fetch, version).await?;
        if !response.status().is_success() {
            if is_discovery_miss(response.status().as_u16()) {
                continue;
            }
            return Err(other(format!(
                "HTTP {} loading authorization server metadata from {url}",
                response.status().as_u16()
            )));
        }
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|error| other(error.to_string()))?;
        let metadata = parse_authorization_server_metadata(&body).map_err(McpError::OAuth)?;
        if !options.skip_issuer_validation {
            // URL parsing adds a trailing slash to bare origins, so compare
            // without one on either side (`discovery.ts:139-140`).
            fn trim(value: &str) -> &str {
                value.strip_suffix('/').unwrap_or(value)
            }
            let expected = authorization_server_url;
            if trim(&metadata.issuer) != trim(expected) {
                return Err(McpError::OAuth(OAuthError::IssuerMismatch {
                    expected: Some(expected.to_string()),
                    received: Some(metadata.issuer.clone()),
                }));
            }
        }
        return Ok(Some(metadata));
    }
    Ok(None)
}

/// Options of [`discover_oauth_server_info`] (`discovery.ts:149-157`).
/// Upstream's options carry no `protocolVersion`: both sub-discoveries
/// default to [`crate::LATEST_PROTOCOL_VERSION`].
#[derive(Clone, Default)]
pub struct DiscoverOptions<'a> {
    /// Protected resource metadata URL from a challenge
    /// (`resource_metadata` of `WWW-Authenticate`).
    pub resource_metadata_url: Option<&'a str>,
    /// Metadata document to use instead of discovery. It is trusted as
    /// configured, so its issuer is not checked.
    pub authorization_server_metadata_url: Option<&'a str>,
    pub fetch: Option<&'a McpFetch>,
    pub skip_issuer_validation: bool,
}

/// Both metadata documents of one server (`discoverOAuthServerInfo`,
/// `discovery.ts:148-194`): the protected resource document when the
/// server answers for it, and the authorization server document — from the
/// configured URL, the resource metadata's first advertised server, or the
/// server's own origin.
pub async fn discover_oauth_server_info(
    server_url: &str,
    options: DiscoverOptions<'_>,
) -> Result<OAuthServerInfo> {
    let resource_metadata = match discover_protected_resource_metadata(
        server_url,
        ProtectedResourceMetadataOptions {
            resource_metadata_url: options.resource_metadata_url,
            fetch: options.fetch,
            ..ProtectedResourceMetadataOptions::default()
        },
    )
    .await
    {
        Ok(metadata) => Some(metadata),
        // Network failures (upstream `TypeError`) propagate; every other
        // discovery miss leaves no resource metadata.
        Err(error @ McpError::Network(_)) => return Err(error),
        Err(_) => None,
    };
    if let Some(metadata_url) = options.authorization_server_metadata_url {
        let fetch = options.fetch.cloned().unwrap_or_default();
        let response = fetch_metadata(metadata_url, &fetch, crate::LATEST_PROTOCOL_VERSION).await?;
        if !response.status().is_success() {
            return Err(other(format!(
                "HTTP {} loading authorization server metadata from {metadata_url}",
                response.status().as_u16()
            )));
        }
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|error| other(error.to_string()))?;
        let metadata = parse_authorization_server_metadata(&body).map_err(McpError::OAuth)?;
        return Ok(OAuthServerInfo {
            authorization_server_url: metadata.issuer.clone(),
            authorization_server_metadata: Some(metadata),
            resource_metadata,
        });
    }
    let authorization_server_url = resource_metadata
        .as_ref()
        .and_then(|metadata| metadata.authorization_servers.as_ref())
        .and_then(|servers| servers.first().cloned())
        .unwrap_or_else(|| {
            let server = Url::parse(server_url).expect("server url parsed by discovery");
            server
                .join("/")
                .expect("origin join never fails on a parsed URL")
                .to_string()
        });
    let authorization_server_metadata = discover_authorization_server_metadata(
        &authorization_server_url,
        AuthorizationServerMetadataOptions {
            fetch: options.fetch,
            skip_issuer_validation: options.skip_issuer_validation,
            ..AuthorizationServerMetadataOptions::default()
        },
    )
    .await?;
    Ok(OAuthServerInfo {
        authorization_server_url,
        authorization_server_metadata,
        resource_metadata,
    })
}

/// The server URL as the resource identifier, without its fragment
/// (`resourceUrlFromServerUrl`, `discovery.ts:196-200`).
pub fn resource_url_from_server_url(value: &str) -> Result<Url> {
    let mut url = Url::parse(value).map_err(|error| other(error.to_string()))?;
    url.set_fragment(None);
    Ok(url)
}

/// The resource identifier a flow sends, when the server advertised one
/// (`selectResource`, `discovery.ts:202-215`): the MCP server must sit on
/// the protected resource's origin and under its path.
pub fn select_resource(
    server_url: &str,
    metadata: Option<&OAuthProtectedResourceMetadata>,
) -> Result<Option<String>> {
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    let requested = resource_url_from_server_url(server_url)?;
    let configured = Url::parse(&metadata.resource).map_err(|error| other(error.to_string()))?;
    if requested.origin() != configured.origin() || !path_starts_with(&requested, &configured) {
        return Err(other(format!(
            "Protected resource {} does not match MCP server {}",
            metadata.resource, requested
        )));
    }
    Ok(Some(metadata.resource.clone()))
}

/// Path containment with trailing-slash normalization
/// (`discovery.ts:209-212`).
fn path_starts_with(requested: &Url, configured: &Url) -> bool {
    let normalize = |url: &Url| {
        let path = url.path();
        if path.ends_with('/') {
            path.to_string()
        } else {
            format!("{path}/")
        }
    };
    normalize(requested).starts_with(&normalize(configured))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_challenge_parser_keeps_the_leftmost_whole_match() {
        // Quoted and unquoted values, case-insensitive names.
        let challenge = parse_www_authenticate(Some(
            r#"Bearer realm="mcp", SCOPE="mcp:read write", error="insufficient_scope""#,
        ));
        assert_eq!(challenge.scope.as_deref(), Some("mcp:read write"));
        assert_eq!(challenge.error.as_deref(), Some("insufficient_scope"));
        // An empty quoted value is absent, and the scan STOPS there like
        // the upstream `?.[1] || ?. [2]` on the leftmost match.
        let challenge = parse_www_authenticate(Some(r#"Bearer scope="" scope=real"#));
        assert_eq!(challenge.scope, None);
        // Unquoted empty values never match, so a later value wins.
        let challenge = parse_www_authenticate(Some("Bearer scope=, scope=real"));
        assert_eq!(challenge.scope.as_deref(), Some("real"));
        // `scope=` inside another word is not preceded by start/comma/space.
        let challenge = parse_www_authenticate(Some("Bearer xscope=real scope=won"));
        assert_eq!(challenge.scope.as_deref(), Some("won"));
        // Unknown schemes challenge nothing.
        assert_eq!(
            parse_www_authenticate(Some("Basic realm=\"mcp\"")),
            OAuthChallenge::default()
        );
        assert_eq!(parse_www_authenticate(None), OAuthChallenge::default());
        // A resource_metadata URL survives only when it parses.
        let challenge =
            parse_www_authenticate(Some("Bearer resource_metadata=\"https://s.example/meta\""));
        assert_eq!(
            challenge.resource_metadata_url.as_deref(),
            Some("https://s.example/meta")
        );
        let challenge = parse_www_authenticate(Some("Bearer resource_metadata=not-a-url"));
        assert_eq!(challenge.resource_metadata_url, None);
    }

    #[test]
    fn discovery_urls_cover_the_oauth_oidc_and_path_scoped_candidates() {
        let urls =
            build_authorization_server_discovery_urls("https://auth.example/idp").expect("urls");
        assert_eq!(
            urls.iter().map(|(url, _)| url.as_str()).collect::<Vec<_>>(),
            [
                "https://auth.example/.well-known/oauth-authorization-server/idp",
                "https://auth.example/.well-known/openid-configuration/idp",
                "https://auth.example/idp/.well-known/openid-configuration",
            ]
        );
        let urls = build_authorization_server_discovery_urls("https://auth.example").expect("urls");
        assert_eq!(urls.len(), 2);
    }

    #[test]
    fn select_resource_checks_origin_and_path() {
        let metadata = OAuthProtectedResourceMetadata {
            resource: "https://api.example/mcp/".to_string(),
            ..OAuthProtectedResourceMetadata::default()
        };
        assert_eq!(
            select_resource("https://api.example/mcp/tools", Some(&metadata))
                .expect("select")
                .as_deref(),
            Some("https://api.example/mcp/")
        );
        // The fragment of the server URL does not count.
        assert_eq!(
            select_resource("https://api.example/mcp#frag", Some(&metadata))
                .expect("select")
                .as_deref(),
            Some("https://api.example/mcp/")
        );
        // Another origin, and a path outside the resource's, both fail.
        let error =
            select_resource("https://other.example/mcp", Some(&metadata)).expect_err("origin");
        assert_eq!(
            error.to_string(),
            "Protected resource https://api.example/mcp/ does not match MCP server https://other.example/mcp"
        );
        let error =
            select_resource("https://api.example/other", Some(&metadata)).expect_err("path");
        assert!(error.to_string().contains("does not match MCP server"));
        assert_eq!(
            select_resource("https://api.example/mcp", None).expect("none"),
            None
        );
    }
}
