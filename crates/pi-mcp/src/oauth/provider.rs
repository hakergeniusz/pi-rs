//! The OAuth state provider (`packages/mcp/src/oauth/provider.ts`): the
//! per-server state shape, its store interface with an in-memory
//! implementation, and the default [`McpOAuthProvider`] the flows drive.
//! The store's async methods box their futures so hosts can hold
//! `Arc<dyn McpOAuthStateStore>`; the provider serializes its
//! load-modify-save cycles on one mutex (upstream's `writes` promise
//! chain) and ignores stored state of other server URLs.

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;
use url::Url;

use crate::error::McpError;
use crate::oauth::errors::OAuthError;
use crate::oauth::flow::OAuthClientMetadataDocument;
use crate::oauth::types::{
    AuthorizationServerMetadata, OAuthClientInformationFull, OAuthClientMetadata,
    OAuthDiscoveryState, OAuthTokens,
};

type Result<T> = std::result::Result<T, McpError>;

fn other(message: impl Into<String>) -> McpError {
    McpError::OAuth(OAuthError::Other(message.into()))
}

/// The stored state of one MCP server (`McpOAuthState`,
/// `provider.ts:11-20`), keyed by its URL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct McpOAuthState {
    pub server_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_information: Option<OAuthClientInformationFull>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<OAuthTokens>,
    /// When the access token expires, in milliseconds since the epoch,
    /// from `expires_in` at the time it was saved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_expire_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_verifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovery: Option<OAuthDiscoveryState>,
}

/// Persistence of one server's [`McpOAuthState`] (`McpOAuthStateStore`,
/// `provider.ts:22-26`). The async methods box their futures so hosts can
/// hold `Arc<dyn McpOAuthStateStore>`.
pub trait McpOAuthStateStore: Send + Sync {
    /// # Errors
    /// Storage failures surface as [`McpError`].
    fn load(&self) -> Pin<Box<dyn Future<Output = Result<Option<McpOAuthState>>> + Send + '_>>;

    /// # Errors
    /// Storage failures surface as [`McpError`].
    fn save(&self, state: McpOAuthState) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
}

/// What [`McpOAuthProvider::invalidate_credentials`] drops
/// (`"all" | "client" | "tokens" | "verifier" | "discovery"`,
/// `provider.ts:109`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidateCredentials {
    All,
    Client,
    Tokens,
    Verifier,
    Discovery,
}

/// The Client ID Metadata Document callback (`clientMetadataDocument`,
/// `flow.ts:38-42`): given the authorization server metadata (possibly
/// none), the document to sign in with, or `None` when the server does not
/// support documents for public clients. Errors bubble.
pub type ClientMetadataDocumentFn = Arc<
    dyn Fn(Option<&AuthorizationServerMetadata>) -> Result<Option<OAuthClientMetadataDocument>>
        + Send
        + Sync,
>;

/// Where the flow sends the browser for authorization (`onRedirect`). The
/// sync closure stands in for upstream's awaited callback — no pi caller
/// hands an async one, and the sign-in reads its captured URL right after.
pub type RedirectCallback = Arc<dyn Fn(&str) + Send + Sync>;

pub(crate) fn noop_redirect() -> RedirectCallback {
    Arc::new(|_| {})
}

/// Construction options (`McpOAuthProviderOptions`, `provider.ts:28-38`).
pub struct McpOAuthProviderOptions {
    pub server_url: String,
    pub redirect_url: String,
    /// The redirect URIs, grant types, response types, and token-endpoint
    /// auth method are defaulted here like upstream; an absent entry
    /// falls back (an explicitly empty `redirect_uris` list is treated as
    /// absent — no pi caller passes one).
    pub client_metadata: OAuthClientMetadata,
    pub client_metadata_document: Option<ClientMetadataDocumentFn>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// Without one, state lives in a [`MemoryOAuthStateStore`].
    pub store: Option<Arc<dyn McpOAuthStateStore>>,
    pub on_redirect: RedirectCallback,
}

/// State that lives only for the process (`MemoryOAuthStateStore`,
/// `provider.ts:40-50`).
#[derive(Default)]
pub struct MemoryOAuthStateStore {
    value: Mutex<Option<McpOAuthState>>,
}

impl McpOAuthStateStore for MemoryOAuthStateStore {
    fn load(&self) -> Pin<Box<dyn Future<Output = Result<Option<McpOAuthState>>> + Send + '_>> {
        let value = self
            .value
            .lock()
            .expect("memory oauth state store mutex poisoned")
            .as_ref()
            .cloned();
        Box::pin(std::future::ready(Ok(value)))
    }

    fn save(&self, state: McpOAuthState) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        *self
            .value
            .lock()
            .expect("memory oauth state store mutex poisoned") = Some(state);
        Box::pin(std::future::ready(Ok(())))
    }
}

/// Default stateful provider for one exact MCP server URL. Applications
/// inject durable storage if needed (`McpOAuthProvider`,
/// `provider.ts:53-166`).
pub struct McpOAuthProvider {
    server_url: String,
    redirect_url: String,
    client_metadata: OAuthClientMetadata,
    client_metadata_document: Option<ClientMetadataDocumentFn>,
    configured_client: Option<OAuthClientInformationFull>,
    store: Arc<dyn McpOAuthStateStore>,
    on_redirect: RedirectCallback,
    /// Serializes the load-modify-save of the store (upstream's `writes`
    /// promise chain): reads wait for queued writes, writes never clobber.
    writes: AsyncMutex<()>,
}

impl McpOAuthProvider {
    /// Builds the provider (`provider.ts:66-85`).
    ///
    /// # Errors
    /// On an unparsable server URL (`new URL` throwing upstream).
    pub fn new(options: McpOAuthProviderOptions) -> Result<Self> {
        let server_url = Url::parse(&options.server_url)
            .map_err(|error| other(error.to_string()))?
            .to_string();
        let mut client_metadata = options.client_metadata;
        if client_metadata.redirect_uris.is_empty() {
            client_metadata.redirect_uris = vec![options.redirect_url.clone()];
        }
        if client_metadata.grant_types.is_none() {
            client_metadata.grant_types = Some(vec![
                "authorization_code".to_string(),
                "refresh_token".to_string(),
            ]);
        }
        if client_metadata.response_types.is_none() {
            client_metadata.response_types = Some(vec!["code".to_string()]);
        }
        if client_metadata.token_endpoint_auth_method.is_none() {
            client_metadata.token_endpoint_auth_method = Some(
                if options.client_secret.is_some() {
                    "client_secret_post"
                } else {
                    "none"
                }
                .to_string(),
            );
        }
        let configured_client = options
            .client_id
            .map(|client_id| OAuthClientInformationFull {
                client_id,
                client_secret: options.client_secret.clone(),
                ..OAuthClientInformationFull::default()
            });
        Ok(Self {
            server_url,
            redirect_url: options.redirect_url,
            client_metadata,
            client_metadata_document: options.client_metadata_document,
            configured_client,
            store: options
                .store
                .unwrap_or_else(|| Arc::new(MemoryOAuthStateStore::default())),
            on_redirect: options.on_redirect,
            writes: AsyncMutex::new(()),
        })
    }

    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    pub fn redirect_url(&self) -> &str {
        &self.redirect_url
    }

    pub fn client_metadata(&self) -> &OAuthClientMetadata {
        &self.client_metadata
    }

    pub fn client_metadata_document(
        &self,
        metadata: Option<&AuthorizationServerMetadata>,
    ) -> Result<Option<OAuthClientMetadataDocument>> {
        match &self.client_metadata_document {
            Some(document) => document(metadata),
            None => Ok(None),
        }
    }

    /// The stored `state` parameter, generated once per server
    /// (`provider.ts:87-93`).
    ///
    /// # Errors
    /// Storage failures surface as [`McpError`].
    pub async fn state(&self) -> Result<String> {
        if let Some(existing) = self.load().await?.oauth_state.filter(|s| !s.is_empty()) {
            return Ok(existing);
        }
        let state = random_hex_state();
        self.update(|mut value| {
            value.oauth_state = Some(state.clone());
            value
        })
        .await?;
        Ok(state)
    }

    /// The configured client wins over the stored one
    /// (`provider.ts:95-99`).
    ///
    /// # Errors
    /// Storage failures surface as [`McpError`].
    pub async fn client_information(&self) -> Result<Option<OAuthClientInformationFull>> {
        if self.configured_client.is_some() {
            return Ok(self.configured_client.clone());
        }
        Ok(self.load().await?.client_information)
    }

    /// # Errors
    /// Storage failures surface as [`McpError`].
    pub async fn save_client_information(
        &self,
        information: OAuthClientInformationFull,
    ) -> Result<()> {
        if self.configured_client.is_some() {
            return Ok(());
        }
        self.update(|mut value| {
            value.client_information = Some(information.clone());
            value
        })
        .await
    }

    /// # Errors
    /// Storage failures surface as [`McpError`].
    pub async fn tokens(&self) -> Result<Option<OAuthTokens>> {
        Ok(self.load().await?.tokens)
    }

    /// Saves tokens and recomputes `tokensExpireAt` from `expires_in`
    /// (`provider.ts:104-117`).
    ///
    /// # Errors
    /// Storage failures surface as [`McpError`].
    pub async fn save_tokens(&self, tokens: OAuthTokens) -> Result<()> {
        let expires_at = tokens
            .expires_in
            .as_ref()
            .and_then(serde_json::Number::as_f64)
            .map(|seconds| now_ms() as f64 + seconds * 1000.0);
        self.update(move |mut value| {
            value.tokens = Some(tokens.clone());
            value.tokens_expire_at = expires_at;
            value
        })
        .await
    }

    /// Hands the authorization URL to the application
    /// (`provider.ts:119-121`).
    pub fn redirect_to_authorization(&self, url: &str) {
        (self.on_redirect)(url);
    }

    /// # Errors
    /// Storage failures surface as [`McpError`].
    pub async fn save_code_verifier(&self, verifier: String) -> Result<()> {
        self.update(|mut value| {
            value.code_verifier = Some(verifier.clone());
            value
        })
        .await
    }

    /// The stored PKCE verifier (`provider.ts:123-129`).
    ///
    /// # Errors
    /// Without one: `No OAuth PKCE code verifier is stored`.
    pub async fn code_verifier(&self) -> Result<String> {
        self.load()
            .await?
            .code_verifier
            .filter(|verifier| !verifier.is_empty())
            .ok_or_else(|| other("No OAuth PKCE code verifier is stored"))
    }

    /// Drops credentials of one kind (`provider.ts:131-153`).
    ///
    /// # Errors
    /// Storage failures surface as [`McpError`].
    pub async fn invalidate_credentials(&self, kind: InvalidateCredentials) -> Result<()> {
        self.update(|mut value| {
            if matches!(
                kind,
                InvalidateCredentials::All | InvalidateCredentials::Client
            ) {
                value.client_information = None;
            }
            if matches!(
                kind,
                InvalidateCredentials::All | InvalidateCredentials::Tokens
            ) {
                value.tokens = None;
                value.tokens_expire_at = None;
            }
            if matches!(
                kind,
                InvalidateCredentials::All | InvalidateCredentials::Verifier
            ) {
                value.code_verifier = None;
            }
            if matches!(
                kind,
                InvalidateCredentials::All | InvalidateCredentials::Discovery
            ) {
                value.discovery = None;
            }
            if matches!(kind, InvalidateCredentials::All) {
                value.oauth_state = None;
            }
            value
        })
        .await
    }

    /// # Errors
    /// Storage failures surface as [`McpError`].
    pub async fn save_discovery_state(&self, discovery: OAuthDiscoveryState) -> Result<()> {
        self.update(|mut value| {
            value.discovery = Some(discovery.clone());
            value
        })
        .await
    }

    /// # Errors
    /// Storage failures surface as [`McpError`].
    pub async fn discovery_state(&self) -> Result<Option<OAuthDiscoveryState>> {
        Ok(self.load().await?.discovery)
    }

    async fn load(&self) -> Result<McpOAuthState> {
        let _guard = self.writes.lock().await;
        Ok(self.own(self.store.load().await?))
    }

    async fn update(
        &self,
        apply: impl FnOnce(McpOAuthState) -> McpOAuthState + Send,
    ) -> Result<()> {
        let _guard = self.writes.lock().await;
        let current = self.own(self.store.load().await?);
        self.store.save(apply(current)).await
    }

    /// Stored state for another server URL is ignored so credentials never
    /// leak across servers (`own`, `provider.ts:158-165`).
    fn own(&self, state: Option<McpOAuthState>) -> McpOAuthState {
        match state {
            Some(state) if state.server_url == self.server_url => state,
            _ => McpOAuthState {
                server_url: self.server_url.clone(),
                ..McpOAuthState::default()
            },
        }
    }
}

/// `Buffer.from(crypto.getRandomValues(new Uint8Array(32))).toString("hex")`
/// (`provider.ts:90`).
fn random_hex_state() -> String {
    const HEX: [char; 16] = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
    ];
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS randomness is available");
    let mut out = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(
            out,
            "{}{}",
            HEX[usize::from(byte >> 4)],
            HEX[usize::from(byte & 0x0f)]
        );
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider(server_url: &str, store: Option<Arc<dyn McpOAuthStateStore>>) -> McpOAuthProvider {
        McpOAuthProvider::new(McpOAuthProviderOptions {
            server_url: server_url.to_string(),
            redirect_url: "http://127.0.0.1/callback".to_string(),
            client_metadata: OAuthClientMetadata {
                client_name: Some("pi".to_string()),
                ..OAuthClientMetadata::default()
            },
            client_metadata_document: None,
            client_id: None,
            client_secret: None,
            store,
            on_redirect: noop_redirect(),
        })
        .expect("provider")
    }

    #[test]
    fn the_constructor_defaults_the_registration_fields() {
        let provider = provider("https://mcp.example/mcp", None);
        let metadata = provider.client_metadata();
        assert_eq!(
            metadata.redirect_uris,
            vec!["http://127.0.0.1/callback".to_string()]
        );
        assert_eq!(
            metadata.grant_types.clone(),
            Some(vec![
                "authorization_code".to_string(),
                "refresh_token".to_string()
            ])
        );
        assert_eq!(
            metadata.response_types.clone(),
            Some(vec!["code".to_string()])
        );
        assert_eq!(metadata.token_endpoint_auth_method.as_deref(), Some("none"));
        // The server URL is normalized like `String(new URL(...))`.
        assert_eq!(provider.server_url(), "https://mcp.example/mcp");
    }

    #[test]
    fn a_client_secret_switches_the_default_token_endpoint_auth_method() {
        let provider = McpOAuthProvider::new(McpOAuthProviderOptions {
            server_url: "https://mcp.example".to_string(),
            redirect_url: "http://127.0.0.1/callback".to_string(),
            client_metadata: OAuthClientMetadata::default(),
            client_metadata_document: None,
            client_id: Some("cid".to_string()),
            client_secret: Some("sec".to_string()),
            store: None,
            on_redirect: noop_redirect(),
        })
        .expect("provider");
        assert_eq!(
            provider
                .client_metadata()
                .token_endpoint_auth_method
                .as_deref(),
            Some("client_secret_post")
        );
        // The configured client wins over the store, and registrations are
        // not saved over it.
        futures::executor::block_on(async {
            assert_eq!(
                provider
                    .client_information()
                    .await
                    .expect("information")
                    .map(|client| client.client_id),
                Some("cid".to_string())
            );
            provider
                .save_client_information(OAuthClientInformationFull {
                    client_id: "registered".to_string(),
                    ..OAuthClientInformationFull::default()
                })
                .await
                .expect("save ignored");
            assert_eq!(
                provider
                    .client_information()
                    .await
                    .expect("information")
                    .map(|client| client.client_id),
                Some("cid".to_string())
            );
        });
    }

    #[test]
    fn state_is_generated_once_and_invalidation_drops_the_right_fields() {
        futures::executor::block_on(async {
            let provider = provider("https://mcp.example/mcp", None);
            let state = provider.state().await.expect("state");
            assert_eq!(state.len(), 64);
            assert!(state.chars().all(|char| char.is_ascii_hexdigit()));
            assert_eq!(provider.state().await.expect("again"), state);

            provider
                .save_code_verifier("verifier".to_string())
                .await
                .expect("verifier");
            provider
                .invalidate_credentials(InvalidateCredentials::Client)
                .await
                .expect("invalidate client");
            let stored = provider.discovery_state().await.expect("load");
            assert!(stored.is_none());
            assert_eq!(provider.state().await.expect("state kept"), state);
            assert_eq!(
                provider.code_verifier().await.expect("verifier kept"),
                "verifier"
            );
            provider
                .invalidate_credentials(InvalidateCredentials::All)
                .await
                .expect("invalidate all");
            // A fresh state, and no verifier left.
            assert_ne!(provider.state().await.expect("new state"), state);
            let error = provider.code_verifier().await.expect_err("no verifier");
            assert_eq!(error.to_string(), "No OAuth PKCE code verifier is stored");
        });
    }

    #[test]
    fn saved_tokens_carry_the_expiry_from_expires_in() {
        futures::executor::block_on(async {
            let provider = provider("https://mcp.example/mcp", None);
            provider
                .save_tokens(OAuthTokens {
                    access_token: "at".to_string(),
                    token_type: "Bearer".to_string(),
                    expires_in: Some(json!(3600).as_number().expect("number").clone()),
                    ..OAuthTokens::default()
                })
                .await
                .expect("save");
            let state = provider.tokens().await.expect("tokens").expect("tokens");
            assert_eq!(state.access_token, "at");
            // tokensExpireAt = now + expires_in * 1000, within a second of it.
            let now = now_ms() as f64;
            let expires_at = provider.load().await.expect("load").tokens_expire_at;
            let expires_at = expires_at.expect("expiry set");
            assert!(
                expires_at - (now + 3_600_000.0) < 1000.0
                    && (now + 3_600_000.0) - expires_at < 1000.0,
                "{expires_at} vs now {now}"
            );
            // Without expires_in the expiry is cleared.
            provider
                .save_tokens(OAuthTokens {
                    access_token: "at2".to_string(),
                    token_type: "Bearer".to_string(),
                    ..OAuthTokens::default()
                })
                .await
                .expect("save");
            assert!(
                provider
                    .load()
                    .await
                    .expect("load")
                    .tokens_expire_at
                    .is_none()
            );
        });
    }

    #[test]
    fn stored_state_of_other_server_urls_is_ignored() {
        let store = Arc::new(MemoryOAuthStateStore::default());
        futures::executor::block_on({
            let store = Arc::clone(&store);
            async move {
                store
                    .save(McpOAuthState {
                        server_url: "https://other.example/mcp".to_string(),
                        tokens: Some(OAuthTokens {
                            access_token: "leaked".to_string(),
                            token_type: "Bearer".to_string(),
                            ..OAuthTokens::default()
                        }),
                        ..McpOAuthState::default()
                    })
                    .await
                    .expect("seed");
                let provider = provider("https://mcp.example/mcp", Some(store));
                // Fresh state for this server; nothing leaks across.
                assert!(provider.tokens().await.expect("tokens").is_none());
                let state = provider.state().await.expect("state");
                provider
                    .save_code_verifier("v".to_string())
                    .await
                    .expect("save");
                // The other server's state is replaced wholesale.
                let stored = provider.load().await.expect("load");
                assert_eq!(stored.server_url, "https://mcp.example/mcp");
                assert_eq!(stored.code_verifier.as_deref(), Some("v"));
                assert_eq!(stored.oauth_state.as_deref(), Some(state.as_str()));
            }
        });
    }
}
