//! The OAuth client of one MCP server
//! (`packages/mcp/src/oauth/client-settings.ts`): its settings, how it
//! identifies itself (a pre-registered client, a Client ID Metadata
//! Document, or dynamic client registration), the loopback callback
//! settings, and the redirect URIs a stored client was registered with.
//! The injectable `fetch` of upstream is [`crate::auth_provider::McpFetch`] — already
//! time-bounded, so `timedFetch` composes into a clone-or-default.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::error::McpError;
use crate::oauth::errors::OAuthError;
use crate::oauth::flow::OAuthClientMetadataDocument;
use crate::oauth::provider::{
    McpOAuthProvider, McpOAuthProviderOptions, McpOAuthStateStore, noop_redirect,
};
use crate::oauth::types::{
    AuthorizationServerMetadata, OAuthClientInformationFull, OAuthClientMetadata,
};

type Result<T> = std::result::Result<T, McpError>;

fn other(message: impl Into<String>) -> McpError {
    McpError::OAuth(OAuthError::Other(message.into()))
}

/// Client settings of one MCP server, usually from the application's
/// configuration (`McpOAuthSettings`, `client-settings.ts:15-45`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct McpOAuthSettings {
    /// Pre-registered client id. Without it, sign-in uses a Client ID
    /// Metadata Document or registers a client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Already resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// Port of the loopback callback server, for clients registered with a
    /// fixed redirect URI. Without [`McpOAuthSettings::callback_url`], the
    /// redirect URI is `http://127.0.0.1:<port>/callback`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,
    /// Loopback redirect URI registered for `clientId`, for example
    /// `http://localhost:8080/oauth/callback`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_url: Option<String>,
    /// Scopes to request, separated by spaces. Default: the scopes the
    /// server advertises.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// `client_name` for dynamic client registration. Default: the
    /// application's client name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    /// How to identify without `clientId` (`clientRegistration`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_registration: Option<ClientRegistration>,
    /// Authorization server metadata document to use instead of discovery
    /// through the server. It is trusted as configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_server_metadata_url: Option<String>,
}

/// How to identify without a pre-registered client
/// (`"auto" | "dcr" | "cimd"`, `client-settings.ts:38`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientRegistration {
    Auto,
    Dcr,
    Cimd,
}

/// The Client ID Metadata Documents an application publishes:
/// `<baseUrl>/client.json`, and for each MCP server
/// `<baseUrl>/<callback ID>/client.json`, whose redirect URIs carry the
/// path `<path>/<callback ID>` (`McpClientMetadataDocuments`,
/// `client-settings.ts:47-52`).
pub trait McpClientMetadataDocuments: Send + Sync {
    /// For example `https://pi.dev/oauth`.
    fn base_url(&self) -> &str;
    /// Whether the documents list `redirectUrl`. Loopback redirect URIs
    /// match on any port (RFC 8252).
    fn lists_redirect_url(&self, redirect_url: &str) -> bool;
}

/// State store of one MCP server, with a lock that keeps other processes
/// from refreshing at the same time (`McpOAuthServerStore`,
/// `client-settings.ts:56-71`).
pub trait McpOAuthServerStore: McpOAuthStateStore {
    /// Run `f` while no other process refreshes the server's tokens. Not
    /// reentrant: `start_mcp_sign_in` and the auth provider take it
    /// themselves, so never call them from `f`. Fails with
    /// [`OAuthError::RefreshLockBusy`] when another process holds the lock
    /// for the whole wait; any other error fails the sign-in or refresh.
    fn with_refresh_lock<'a>(
        &'a self,
        f: Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
}

/// The OAuth client of one MCP server: where its state lives, its
/// settings, and the application's identity (`McpOAuthClientOptions`,
/// `client-settings.ts:73-80`).
pub struct McpOAuthClientOptions {
    pub server_url: String,
    pub store: Arc<dyn McpOAuthServerStore>,
    pub settings: McpOAuthSettings,
    /// `client_name` for dynamic client registration unless
    /// [`McpOAuthSettings::client_name`] names another: the application.
    pub client_name: String,
    /// Omitted: dynamic client registration only.
    pub client_metadata_documents: Option<Arc<dyn McpClientMetadataDocuments>>,
}

/// An [`McpOAuthProvider`] for the client, redirecting to `redirectUrl` or
/// the document's variant of it (`createOAuthClientProvider`,
/// `client-settings.ts:82-110`). `on_redirect` defaults to a no-op.
///
/// # Errors
/// On an unparsable server URL.
pub fn create_oauth_client_provider(
    options: &McpOAuthClientOptions,
    redirect_url: &str,
    on_redirect: Option<crate::oauth::provider::RedirectCallback>,
) -> Result<McpOAuthProvider> {
    let documents = options.client_metadata_documents.clone();
    let settings = options.settings.clone();
    let server_url = options.server_url.clone();
    let redirect = redirect_url.to_string();
    let client_metadata_document: Option<crate::oauth::provider::ClientMetadataDocumentFn> =
        if offers_client_metadata_document(
            &options.settings,
            options.client_metadata_documents.as_deref(),
            redirect_url,
        ) {
            Some(Arc::new(move |metadata| {
                let Some(documents) = documents.as_ref() else {
                    return Err(other(format!(
                        "oauth.clientRegistration \"cimd\" needs Client ID Metadata Documents that list the redirect URI {redirect}"
                    )));
                };
                if !documents.lists_redirect_url(&redirect) {
                    return Err(other(format!(
                        "oauth.clientRegistration \"cimd\" needs Client ID Metadata Documents that list the redirect URI {redirect}"
                    )));
                }
                let document = client_metadata_document(
                    &server_url,
                    &redirect,
                    metadata,
                    documents.base_url(),
                )?;
                if document.is_none()
                    && settings.client_registration == Some(ClientRegistration::Cimd)
                {
                    return Err(other(
                        "The authorization server does not support Client ID Metadata Documents for public clients; remove oauth.clientRegistration \"cimd\"",
                    ));
                }
                Ok(document)
            }))
        } else {
            None
        };
    McpOAuthProvider::new(McpOAuthProviderOptions {
        server_url: options.server_url.clone(),
        redirect_url: redirect_url.to_string(),
        client_metadata: OAuthClientMetadata {
            client_name: Some(
                options
                    .settings
                    .client_name
                    .clone()
                    .unwrap_or_else(|| options.client_name.clone()),
            ),
            ..OAuthClientMetadata::default()
        },
        client_metadata_document,
        client_id: options.settings.client_id.clone(),
        client_secret: options.settings.client_secret.clone(),
        store: Some(options.store.clone()),
        on_redirect: on_redirect.unwrap_or_else(noop_redirect),
    })
}

/// Whether the client may identify with a Client ID Metadata Document
/// instead of registering (`offersClientMetadataDocument`,
/// `client-settings.ts:112-119`). `auto` registers when the user asked for
/// a client name or the documents do not list the redirect URI.
pub fn offers_client_metadata_document(
    settings: &McpOAuthSettings,
    documents: Option<&dyn McpClientMetadataDocuments>,
    redirect_url: &str,
) -> bool {
    let registration = settings
        .client_registration
        .unwrap_or(ClientRegistration::Auto);
    if registration != ClientRegistration::Auto {
        return registration == ClientRegistration::Cimd;
    }
    settings.client_id.is_none()
        && settings.client_name.is_none()
        && documents.is_some_and(|documents| documents.lists_redirect_url(redirect_url))
}

/// The Client ID Metadata Document to use, chosen like Codex chooses its
/// own, or `None` when the authorization server does not support documents
/// for public clients (`clientMetadataDocument`,
/// `client-settings.ts:121-146`). Without the `iss` parameter in
/// authorization responses (RFC 9207), the redirect URI and the document
/// are specific to the MCP server, so a response cannot be mixed up with
/// one from another authorization server (RFC 9700 section 4.4.2.2).
fn client_metadata_document(
    server_url: &str,
    redirect_url: &str,
    metadata: Option<&AuthorizationServerMetadata>,
    base_url: &str,
) -> Result<Option<OAuthClientMetadataDocument>> {
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    if !metadata
        .client_id_metadata_document_supported
        .unwrap_or(false)
        || !metadata
            .token_endpoint_auth_methods_supported
            .as_ref()
            .is_some_and(|methods| methods.iter().any(|method| method == "none"))
    {
        return Ok(None);
    }
    if metadata
        .authorization_response_iss_parameter_supported
        .unwrap_or(false)
    {
        return Ok(Some(OAuthClientMetadataDocument {
            url: format!("{base_url}/client.json"),
            redirect_url: redirect_url.to_string(),
        }));
    }
    let redirect = Url::parse(redirect_url).map_err(|error| other(error.to_string()))?;
    let mut redirect = redirect;
    redirect.set_path(&server_specific_path(redirect.path(), server_url)?);
    Ok(Some(OAuthClientMetadataDocument {
        url: format!("{base_url}/{}/client.json", callback_id(server_url)?),
        redirect_url: redirect.to_string(),
    }))
}

/// The path of the redirect URI of a server-specific Client ID Metadata
/// Document: `<path>/<callback ID>` (`serverSpecificPath`,
/// `client-settings.ts:148-150`).
///
/// # Errors
/// On an unparsable server URL (the callback id needs one).
pub fn server_specific_path(path: &str, server_url: &str) -> Result<String> {
    Ok(format!(
        "{}/{}",
        path.strip_suffix('/').unwrap_or(path),
        callback_id(server_url)?
    ))
}

/// 12 characters identifying an MCP server URL in callback paths, computed
/// like Codex does (`callbackId`, `client-settings.ts:152-156`).
///
/// # Errors
/// On an unparsable server URL.
pub fn callback_id(server_url: &str) -> Result<String> {
    let mut url = Url::parse(server_url).map_err(|error| other(error.to_string()))?;
    url.set_fragment(None);
    let digest = Sha256::digest(url.to_string().as_bytes());
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&digest[..9]))
}

/// Redirect URIs a stored client was registered with
/// (`registeredRedirectUrls`, `client-settings.ts:158-162`).
pub fn registered_redirect_urls(client: Option<&OAuthClientInformationFull>) -> Vec<String> {
    client
        .map(|client| client.metadata.redirect_uris.clone())
        .unwrap_or_default()
}

/// Redirect URI when the settings name none and no client is stored
/// (`DEFAULT_REDIRECT_URL`, `client-settings.ts:171`).
pub const DEFAULT_REDIRECT_URL: &str = "http://127.0.0.1/callback";

const CALLBACK_HOST: &str = "127.0.0.1";

/// Where the loopback callback server listens and the redirect URI it
/// serves, from `callbackUrl` and `callbackPort` (`LoopbackSettings` +
/// `loopbackSettings`, `client-settings.ts:164-208`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopbackSettings {
    /// The redirect URI, without the port unless one is configured.
    pub url: String,
    /// Address to listen on.
    pub host: String,
    /// Host name in the redirect URI.
    pub redirect_host: String,
    pub port: Option<u16>,
    pub path: String,
    /// The exact redirect URI, when the port is fixed.
    pub fixed_redirect_url: Option<String>,
}

/// The loopback callback settings of `settings` (`loopbackSettings`,
/// `client-settings.ts:174-208`).
///
/// # Errors
/// On an unparsable configured callback URL.
pub fn loopback_settings(settings: &McpOAuthSettings) -> Result<LoopbackSettings> {
    let configured = settings
        .callback_url
        .clone()
        .unwrap_or_else(|| DEFAULT_REDIRECT_URL.to_string());
    let url = Url::parse(&configured).map_err(|error| other(error.to_string()))?;
    // `localhost` is served on 127.0.0.1; browsers fall back to it when
    // ::1 refuses (`client-settings.ts:198-199`).
    let hostname = url.host_str().unwrap_or_default();
    let address = hostname
        .strip_prefix('[')
        .unwrap_or(hostname)
        .strip_suffix(']')
        .unwrap_or(hostname)
        .to_string();
    let url_port = url.port();
    let port = url_port.or(settings.callback_port);
    let mut fixed_redirect_url = None;
    if url_port.is_some() {
        // A configured URI with a port is sent exactly as written, since
        // servers compare it as a string (`client-settings.ts:193-194`).
        fixed_redirect_url = Some(configured.clone());
    } else if let Some(port) = port {
        let mut with_port = url.clone();
        if with_port.set_port(Some(port)).is_err() {
            return Err(other("Invalid callback URL for a port"));
        }
        fixed_redirect_url = Some(with_port.to_string());
    }
    Ok(LoopbackSettings {
        url: configured,
        host: if address == "localhost" {
            CALLBACK_HOST.to_string()
        } else {
            address.clone()
        },
        redirect_host: address,
        port,
        path: url.path().to_string(),
        fixed_redirect_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_settings_default_to_the_127_0_0_1_callback() {
        let settings = loopback_settings(&McpOAuthSettings::default()).expect("settings");
        assert_eq!(settings.url, "http://127.0.0.1/callback");
        assert_eq!(settings.host, "127.0.0.1");
        assert_eq!(settings.redirect_host, "127.0.0.1");
        assert_eq!(settings.port, None);
        assert_eq!(settings.path, "/callback");
        assert_eq!(settings.fixed_redirect_url, None);
    }

    #[test]
    fn a_configured_uri_with_a_port_is_sent_exactly_as_written() {
        let settings = loopback_settings(&McpOAuthSettings {
            callback_url: Some("http://localhost:8080/oauth/callback".to_string()),
            ..McpOAuthSettings::default()
        })
        .expect("settings");
        assert_eq!(settings.host, "127.0.0.1");
        assert_eq!(settings.redirect_host, "localhost");
        assert_eq!(settings.port, Some(8080));
        assert_eq!(settings.path, "/oauth/callback");
        assert_eq!(
            settings.fixed_redirect_url.as_deref(),
            Some("http://localhost:8080/oauth/callback")
        );
        // Without a port in the URI, callback_port fills one in and the
        // fixed URI carries it.
        let settings = loopback_settings(&McpOAuthSettings {
            callback_url: Some("http://localhost/oauth/callback".to_string()),
            callback_port: Some(9001),
            ..McpOAuthSettings::default()
        })
        .expect("settings");
        assert_eq!(settings.port, Some(9001));
        assert_eq!(
            settings.fixed_redirect_url.as_deref(),
            Some("http://localhost:9001/oauth/callback")
        );
    }

    #[test]
    fn the_callback_id_hashes_the_server_url_without_its_fragment() {
        let id = callback_id("https://mcp.example/v1/mcp").expect("id");
        assert_eq!(id.len(), 12);
        assert_eq!(
            id,
            callback_id("https://mcp.example/v1/mcp#hash").expect("id")
        );
        assert_ne!(
            id,
            callback_id("https://mcp.example/v2/mcp").expect("other id")
        );
        let path = server_specific_path("/callback", "https://mcp.example/v1/mcp").expect("path");
        assert_eq!(path, format!("/callback/{id}"));
    }

    #[test]
    fn client_id_metadata_documents_are_offered_only_in_the_auto_and_cimd_cases() {
        struct Documents;
        impl McpClientMetadataDocuments for Documents {
            fn base_url(&self) -> &'static str {
                "https://pi.dev/oauth"
            }
            fn lists_redirect_url(&self, _redirect_url: &str) -> bool {
                true
            }
        }
        let documents: Option<&dyn McpClientMetadataDocuments> = Some(&Documents);
        let base = McpOAuthSettings::default();
        assert!(offers_client_metadata_document(
            &base,
            documents,
            "http://127.0.0.1/callback"
        ));
        // A configured client id or client name registers instead.
        assert!(!offers_client_metadata_document(
            &McpOAuthSettings {
                client_id: Some("cid".to_string()),
                ..McpOAuthSettings::default()
            },
            documents,
            "http://127.0.0.1/callback"
        ));
        assert!(!offers_client_metadata_document(
            &McpOAuthSettings {
                client_name: Some("my client".to_string()),
                ..McpOAuthSettings::default()
            },
            documents,
            "http://127.0.0.1/callback"
        ));
        // `dcr` never offers documents; `cimd` does even without them (the
        // closure then fails the sign-in).
        assert!(!offers_client_metadata_document(
            &McpOAuthSettings {
                client_registration: Some(ClientRegistration::Dcr),
                ..McpOAuthSettings::default()
            },
            documents,
            "http://127.0.0.1/callback"
        ));
        assert!(offers_client_metadata_document(
            &McpOAuthSettings {
                client_registration: Some(ClientRegistration::Cimd),
                ..McpOAuthSettings::default()
            },
            None,
            "http://127.0.0.1/callback"
        ));
    }
}
