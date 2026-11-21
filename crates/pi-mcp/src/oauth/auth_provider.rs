//! The auth provider for connections to an OAuth-protected MCP server
//! (`packages/mcp/src/oauth/auth-provider.ts`): it sends the stored access
//! token and refreshes it, but never starts a browser flow. When the user
//! has to sign in, it fails with [`OAuthError::AuthorizationRequired`] and
//! the application runs [`crate::oauth::sign_in::start_mcp_sign_in`].
//!
//! Many servers rotate refresh tokens, so two refreshes with the same
//! refresh token lose the grant. Requests in this process share one
//! refresh — a singleflight slot where joiners get the same outcome — and
//! other processes are kept out by the store's refresh lock, held from
//! reading the tokens to saving new ones. Tokens that changed meanwhile
//! (another process refreshed them, or the user signed in) are used
//! without refreshing.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{Mutex as AsyncMutex, Notify};

use crate::auth_provider::{AuthProvider, McpFetch, UnauthorizedContext};
use crate::error::McpError;
use crate::oauth::client_settings::{
    McpOAuthClientOptions, McpOAuthServerStore, McpOAuthSettings, create_oauth_client_provider,
    loopback_settings, registered_redirect_urls,
};
use crate::oauth::discovery::parse_www_authenticate;
use crate::oauth::errors::OAuthError;
use crate::oauth::flow::{OAuthFlowOptions, OAuthFlowResult, authorize_mcp};
use crate::oauth::provider::McpOAuthState;
use crate::oauth::types::OAuthChallenge;

type Result<T> = std::result::Result<T, McpError>;

fn cancelled() -> McpError {
    McpError::OAuth(OAuthError::AuthorizationRequired)
}

/// Options of [`McpAuthProvider`] (the options of
/// `createMcpAuthProvider`, `auth-provider.ts:55-70`).
pub struct McpAuthProviderOptions {
    pub server_url: String,
    pub store: Arc<dyn McpOAuthServerStore>,
    /// Called only when a refresh is needed, so a secret that fails to
    /// resolve fails the refresh instead of the whole connection setup.
    pub settings: Arc<dyn Fn() -> McpOAuthSettings + Send + Sync>,
    /// `client_name` for dynamic client registration unless
    /// [`McpOAuthSettings::client_name`] names another: the application.
    pub client_name: String,
    /// Omitted: dynamic client registration only.
    pub client_metadata_documents:
        Option<Arc<dyn crate::oauth::client_settings::McpClientMetadataDocuments>>,
    /// Receives the server's `WWW-Authenticate` challenge so sign-in can
    /// use its resource metadata URL and scope.
    pub on_challenge: Arc<dyn Fn(OAuthChallenge) + Send + Sync>,
    /// A redirect URI the application signs in with, such as its https
    /// callback route. Refreshing tokens from a sign-in with a Client ID
    /// Metadata Document needs a redirect URI the documents list; a
    /// refresh never redirects, so any listed one works. Default: the
    /// loopback redirect URI of `settings`.
    pub redirect_url: Option<String>,
}

/// One in-flight refresh: joiners await the outcome it publishes
/// (`refreshing ??= …`, `auth-provider.ts:72-89`).
struct Refreshing {
    done: Notify,
    result: Mutex<Option<Arc<Result<()>>>>,
}

impl Default for Refreshing {
    fn default() -> Self {
        Self {
            done: Notify::new(),
            result: Mutex::new(None),
        }
    }
}

/// Auth provider for MCP connections: sends the stored access token and
/// refreshes it when it is about to expire or after a 401. Fails with
/// [`OAuthError::AuthorizationRequired`] when the user has to sign in,
/// including when the server asks for more scope (`insufficient_scope`).
/// The `on_challenge` hook receives the server's `WWW-Authenticate`
/// challenge so sign-in can use its resource metadata URL and scope
/// (`createMcpAuthProvider`, `auth-provider.ts:55-134`).
pub struct McpAuthProvider {
    server_url: String,
    store: Arc<dyn McpOAuthServerStore>,
    settings: Arc<dyn Fn() -> McpOAuthSettings + Send + Sync>,
    client_name: String,
    client_metadata_documents:
        Option<Arc<dyn crate::oauth::client_settings::McpClientMetadataDocuments>>,
    on_challenge: Arc<dyn Fn(OAuthChallenge) + Send + Sync>,
    redirect_url: Option<String>,
    /// The singleflight slot of the current refresh, if any.
    refreshing: AsyncMutex<Option<Arc<Refreshing>>>,
}

impl McpAuthProvider {
    /// Builds the provider (`createMcpAuthProvider`,
    /// `auth-provider.ts:55-134`).
    pub fn new(options: McpAuthProviderOptions) -> Self {
        Self {
            server_url: options.server_url,
            store: options.store,
            settings: options.settings,
            client_name: options.client_name,
            client_metadata_documents: options.client_metadata_documents,
            on_challenge: options.on_challenge,
            redirect_url: options.redirect_url,
            refreshing: AsyncMutex::new(None),
        }
    }

    /// Resolves when no refresh is running, so shutdown does not drop
    /// rotated tokens before they are saved (`settled`,
    /// `auth-provider.ts:18-20`). Failures of the refresh itself are not
    /// surfaced.
    pub async fn settled(&self) {
        if let Some(entry) = self.refreshing.lock().await.as_ref().map(Arc::clone) {
            let _ = Self::join(&entry).await;
        }
    }

    /// Replace `stale_token`, the access token that expired or was
    /// rejected (`refresh`, `auth-provider.ts:61-90`). Concurrent callers
    /// join the same refresh.
    async fn refresh(
        &self,
        stale_token: Option<&str>,
        fetch: McpFetch,
        challenge: Option<OAuthChallenge>,
    ) -> Result<()> {
        let mut slot = self.refreshing.lock().await;
        if let Some(entry) = slot.as_ref().map(Arc::clone) {
            drop(slot);
            return Self::join(&entry).await;
        }
        let entry = Arc::new(Refreshing::default());
        *slot = Some(Arc::clone(&entry));
        drop(slot);
        let result = Arc::new(self.run_refresh(stale_token, fetch, challenge).await);
        *entry.result.lock().expect("refresh result mutex poisoned") = Some(Arc::clone(&result));
        let mut slot = self.refreshing.lock().await;
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &entry))
        {
            *slot = None;
        }
        drop(slot);
        entry.done.notify_waiters();
        (*result).clone()
    }

    /// Waits for one in-flight refresh and copies its outcome.
    async fn join(entry: &Refreshing) -> Result<()> {
        loop {
            if let Some(result) = entry
                .result
                .lock()
                .expect("refresh result mutex poisoned")
                .as_ref()
            {
                return (**result).clone();
            }
            entry.done.notified().await;
        }
    }

    /// The lock body of one refresh (`auth-provider.ts:63-81`).
    async fn run_refresh(
        &self,
        stale_token: Option<&str>,
        fetch: McpFetch,
        challenge: Option<OAuthChallenge>,
    ) -> Result<()> {
        let client_options = McpOAuthClientOptions {
            server_url: self.server_url.clone(),
            store: Arc::clone(&self.store),
            settings: (self.settings)(),
            client_name: self.client_name.clone(),
            client_metadata_documents: self.client_metadata_documents.clone(),
        };
        let redirect_url = self.redirect_url.clone();
        self.store
            .with_refresh_lock(Box::pin(async move {
                let state = client_options.store.load().await?;
                // Another request already refreshed the tokens, or the
                // user signed in: use them without refreshing. Absent
                // tokens compare equal to an absent stale token, so the
                // refresh falls through to the no-refresh-token error
                // (`state?.tokens?.access_token !== staleToken`,
                // `auth-provider.ts:65-67`).
                if state
                    .as_ref()
                    .and_then(|state| state.tokens.as_ref())
                    .map(|tokens| tokens.access_token.as_str())
                    != stale_token
                {
                    return Ok(());
                }
                if state
                    .as_ref()
                    .and_then(|state| state.tokens.as_ref())
                    .and_then(|tokens| tokens.refresh_token.as_deref())
                    .is_none_or(str::is_empty)
                {
                    return Err(cancelled());
                }
                let result = refresh_stored_tokens(
                    &client_options,
                    state.as_ref().expect("tokens checked above"),
                    RefreshStoredOptions {
                        fetch: Some(fetch),
                        challenge,
                        redirect_url,
                    },
                )
                .await?;
                if result == OAuthFlowResult::Redirect {
                    return Err(cancelled());
                }
                Ok(())
            }))
            .await
    }
}

impl AuthProvider for McpAuthProvider {
    fn token(&self) -> Pin<Box<dyn Future<Output = Result<Option<String>>> + Send + '_>> {
        Box::pin(async {
            // An in-flight refresh finishing first lets the request use its
            // result; its failures fall through (`token`,
            // `auth-provider.ts:92-105`).
            if let Some(entry) = self.refreshing.lock().await.as_ref().map(Arc::clone) {
                let _ = Self::join(&entry).await;
            }
            let state = self.store.load().await?;
            let token = state
                .as_ref()
                .and_then(|state| state.tokens.as_ref())
                .map(|tokens| tokens.access_token.clone());
            // Access tokens this close to expiry are refreshed before they
            // are sent (REFRESH_SKEW_MS, `auth-provider.ts:24`).
            let expired = state
                .as_ref()
                .and_then(|state| state.tokens_expire_at)
                .is_some_and(|expires_at| expires_at - REFRESH_SKEW_MS_F64 <= now_ms() as f64);
            let has_refresh_token = state
                .as_ref()
                .and_then(|state| state.tokens.as_ref())
                .and_then(|tokens| tokens.refresh_token.as_deref())
                .is_some_and(|token| !token.is_empty());
            if !expired || !has_refresh_token {
                return Ok(token);
            }
            // Failures fall through: the request goes out with the old
            // token and a 401 decides what happens
            // (`auth-provider.ts:101-104`).
            let _ = self
                .refresh(token.as_deref(), McpFetch::timed(), None)
                .await;
            Ok(self
                .store
                .load()
                .await?
                .and_then(|state| state.tokens)
                .map(|tokens| tokens.access_token))
        })
    }

    fn recovers(&self) -> bool {
        true
    }

    fn on_unauthorized(
        &self,
        context: UnauthorizedContext,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            let challenge = parse_www_authenticate(
                context
                    .response
                    .headers()
                    .get("www-authenticate")
                    .and_then(|value| value.to_str().ok()),
            );
            (self.on_challenge)(challenge.clone());
            // A refresh keeps the granted scope, so more scope needs a new
            // sign-in (`auth-provider.ts:107-110`).
            if challenge.error.as_deref() == Some("insufficient_scope") {
                return Err(cancelled());
            }
            self.refresh(context.token.as_deref(), context.fetch, Some(challenge))
                .await
        })
    }
}

/// Access tokens this close to expiry are refreshed before they are sent
/// (`REFRESH_SKEW_MS`, `auth-provider.ts:24`).
const REFRESH_SKEW_MS_F64: f64 = 30_000.0;

/// Options of [`refresh_stored_tokens`] (`auth-provider.ts:139-142`).
pub struct RefreshStoredOptions {
    pub fetch: Option<McpFetch>,
    pub challenge: Option<OAuthChallenge>,
    pub redirect_url: Option<String>,
}

/// Refresh the tokens of `state` with the stored client (or the document
/// it signed in with), or report [`OAuthFlowResult::Redirect`] when that
/// is not possible (`refreshStoredTokens`,
/// `auth-provider.ts:137-160`). `options.redirect_url` is a redirect URI
/// the application signs in with, tried before the loopback ones of the
/// settings and the stored client. Never registers a client or prepares an
/// authorization request, so a pending sign-in stays usable. Call while
/// holding the store's refresh lock. Not cancellable: a refresh the server
/// answered may have rotated the refresh token, so its answer must be
/// saved.
///
/// # Errors
/// Whatever the flow throws.
pub async fn refresh_stored_tokens(
    client: &McpOAuthClientOptions,
    state: &McpOAuthState,
    options: RefreshStoredOptions,
) -> Result<OAuthFlowResult> {
    // Refreshing never redirects the user; the redirect URI only decides
    // whether tokens without a stored client can be refreshed with a
    // Client ID Metadata Document, so any URI the documents list works
    // (`auth-provider.ts:145-150`).
    let candidates = [
        options.redirect_url.clone(),
        loopback_settings(&client.settings)?.fixed_redirect_url,
        registered_redirect_urls(state.client_information.as_ref())
            .into_iter()
            .next(),
    ];
    let redirect_url = candidates
        .into_iter()
        .flatten()
        .find(|url| {
            client
                .client_metadata_documents
                .as_ref()
                .is_some_and(|documents| documents.lists_redirect_url(url))
        })
        .unwrap_or_else(|| crate::oauth::client_settings::DEFAULT_REDIRECT_URL.to_string());
    authorize_mcp(
        &create_oauth_client_provider(client, &redirect_url, None)?,
        OAuthFlowOptions {
            server_url: client.server_url.clone(),
            resource_metadata_url: options
                .challenge
                .as_ref()
                .and_then(|challenge| challenge.resource_metadata_url.clone()),
            authorization_server_metadata_url: client.settings.auth_server_metadata_url.clone(),
            // Upstream composes `timedFetch` around this fetch; the port's
            // [`McpFetch`] always carries the 15 s timeout itself.
            fetch: options.fetch,
            refresh_only: true,
            ..OAuthFlowOptions::default()
        },
    )
    .await
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::provider::{McpOAuthStateStore, MemoryOAuthStateStore};
    use crate::oauth::types::OAuthTokens;
    use futures::FutureExt as _;
    use std::fmt::Write as _;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    /// A store whose refresh lock just runs the body (single process).
    struct UnlockedStore(MemoryOAuthStateStore);

    impl McpOAuthStateStore for UnlockedStore {
        fn load(&self) -> Pin<Box<dyn Future<Output = Result<Option<McpOAuthState>>> + Send + '_>> {
            self.0.load()
        }
        fn save(
            &self,
            state: McpOAuthState,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
            self.0.save(state)
        }
    }

    impl McpOAuthServerStore for UnlockedStore {
        fn with_refresh_lock<'a>(
            &'a self,
            f: Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            f
        }
    }

    type TestHandler =
        Arc<dyn Fn(&str, &str) -> (u16, Vec<(&'static str, String)>, String) + Send + Sync>;

    /// A blocking synthetic authorization/MCP server answering per target.
    fn spawn_oauth_server(handler: TestHandler) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!(
            "http://127.0.0.1:{}",
            listener.local_addr().expect("addr").port()
        );
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = handle_connection(&mut stream, &handler);
            }
        });
        url
    }

    fn handle_connection(stream: &mut TcpStream, handler: &TestHandler) -> std::io::Result<()> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            match stream.read(&mut chunk)? {
                0 => break buffer.len(),
                read => {
                    buffer.extend_from_slice(&chunk[..read]);
                    if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                        break at + 4;
                    }
                    if buffer.len() > 65_536 {
                        break buffer.len();
                    }
                }
            }
        };
        let head = String::from_utf8_lossy(&buffer[..head_end.min(buffer.len())]).into_owned();
        let request_line = head.lines().next().unwrap_or_default().to_string();
        let mut parts = request_line.split(' ');
        let _method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or("/").to_string();
        let content_length: usize = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.trim().eq_ignore_ascii_case("content-length") {
                    value.trim().parse().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0);
        while buffer.len() < head_end + content_length {
            match stream.read(&mut chunk)? {
                0 => break,
                read => buffer.extend_from_slice(&chunk[..read]),
            }
        }
        let body = String::from_utf8_lossy(&buffer[head_end.min(buffer.len())..]).into_owned();
        let (status, headers, reply) = handler(&target, &body);
        let reason = if status == 200 { "OK" } else { "Failure" };
        let mut response = format!("HTTP/1.1 {status} {reason}\r\n");
        for (name, value) in &headers {
            let _ = write!(response, "{name}: {value}\r\n");
        }
        let _ = write!(
            response,
            "content-length: {}\r\nconnection: close\r\n\r\n",
            reply.len()
        );
        stream.write_all(response.as_bytes())?;
        stream.write_all(reply.as_bytes())?;
        stream.flush()
    }

    fn provider(server_url: &str, store: UnlockedStore) -> McpAuthProvider {
        McpAuthProvider::new(McpAuthProviderOptions {
            server_url: server_url.to_string(),
            store: Arc::new(store),
            settings: Arc::new(McpOAuthSettings::default),
            client_name: "pi".to_string(),
            client_metadata_documents: None,
            on_challenge: Arc::new(|_| {}),
            redirect_url: None,
        })
    }

    fn tokens(access_token: &str, refresh_token: &str) -> OAuthTokens {
        OAuthTokens {
            access_token: access_token.to_string(),
            token_type: "Bearer".to_string(),
            refresh_token: Some(refresh_token.to_string()),
            ..OAuthTokens::default()
        }
    }

    fn expired_client(cid: &str) -> crate::oauth::OAuthClientInformationFull {
        crate::oauth::OAuthClientInformationFull {
            metadata: crate::oauth::types::OAuthClientMetadata {
                redirect_uris: vec!["http://127.0.0.1/callback".to_string()],
                token_endpoint_auth_method: Some("none".to_string()),
                ..crate::oauth::types::OAuthClientMetadata::default()
            },
            client_id: cid.to_string(),
            ..crate::oauth::OAuthClientInformationFull::default()
        }
    }

    /// The discovery documents and token endpoint of one authorization
    /// server, plus the 404s of the MCP server's protected-resource
    /// discovery. The issuer is filled in after the server binds, since it
    /// must match the origin the discovery fell back to.
    fn discovery_and_token_handler(
        issuer: Arc<std::sync::Mutex<Option<String>>>,
        token_reply: std::sync::Mutex<Vec<(u16, String)>>,
        requests: Option<mpsc::Sender<String>>,
    ) -> TestHandler {
        Arc::new(move |target: &str, body: &str| {
            if let Some(requests) = &requests {
                let _ = requests.send(format!("{target} {body}"));
            }
            match target {
                t if t.contains("/.well-known/oauth-protected-resource") => {
                    (404, Vec::new(), String::new())
                }
                t if t.contains("/.well-known/oauth-authorization-server") => {
                    let server_url = issuer
                        .lock()
                        .expect("issuer slot poisoned")
                        .clone()
                        .expect("issuer set after bind");
                    let metadata = serde_json::json!({
                        "issuer": server_url,
                        "authorization_endpoint": format!("{server_url}/authorize"),
                        "token_endpoint": format!("{server_url}/token"),
                        "response_types_supported": ["code"],
                        "token_endpoint_auth_methods_supported": ["none"],
                    });
                    (
                        200,
                        vec![("content-type", "application/json".to_string())],
                        metadata.to_string(),
                    )
                }
                t if t.ends_with("/token") => {
                    let (status, reply) = token_reply
                        .lock()
                        .expect("token replies poisoned")
                        .pop()
                        .expect("a token reply was queued");
                    (
                        status,
                        vec![("content-type", "application/json".to_string())],
                        reply,
                    )
                }
                _ => (404, Vec::new(), String::new()),
            }
        })
    }

    /// The server the tests share: two 404s for the protected-resource
    /// discovery, one authorization-server document, one token answer.
    fn spawn_discovery_server(
        token_reply: std::sync::Mutex<Vec<(u16, String)>>,
        requests: Option<mpsc::Sender<String>>,
    ) -> String {
        let issuer = Arc::new(std::sync::Mutex::new(None));
        let handler = discovery_and_token_handler(Arc::clone(&issuer), token_reply, requests);
        let server_url = spawn_oauth_server(handler);
        *issuer.lock().expect("issuer slot poisoned") = Some(server_url.clone());
        server_url
    }

    fn seeded_state(server_url: &str, access: &str, refresh: &str) -> McpOAuthState {
        McpOAuthState {
            server_url: format!("{server_url}/mcp"),
            client_information: Some(expired_client("cid")),
            tokens: Some(tokens(access, refresh)),
            tokens_expire_at: Some(now_ms() as f64 - 1_000.0),
            ..McpOAuthState::default()
        }
    }

    fn seed(store: &UnlockedStore, state: McpOAuthState) {
        store
            .0
            .save(state)
            .now_or_never()
            .expect("ready save")
            .expect("seed");
    }

    #[tokio::test]
    async fn an_expiring_token_refreshes_and_the_new_one_is_used() {
        let requests = mpsc::channel();
        let server_url = spawn_discovery_server(
            std::sync::Mutex::new(vec![(
                200,
                serde_json::json!({
                    "access_token": "new-token",
                    "token_type": "Bearer",
                    "refresh_token": "new-refresh"
                })
                .to_string(),
            )]),
            Some(requests.0),
        );
        let store = UnlockedStore(MemoryOAuthStateStore::default());
        seed(&store, seeded_state(&server_url, "stale", "old-refresh"));
        let auth = provider(&format!("{server_url}/mcp"), store);
        let token = auth.token().await.expect("token");
        assert_eq!(token.as_deref(), Some("new-token"));
        // The store holds the rotated refresh token.
        let state = auth
            .store
            .load()
            .now_or_never()
            .expect("ready")
            .expect("state")
            .expect("state");
        assert_eq!(
            state
                .tokens
                .as_ref()
                .and_then(|tokens| tokens.refresh_token.as_deref()),
            Some("new-refresh")
        );
        // The refresh token endpoint request carried the old refresh
        // token and the client id.
        let token_request = requests
            .1
            .try_iter()
            .find(|request| request.contains("/token"))
            .expect("a token request");
        assert!(token_request.contains("grant_type=refresh_token"));
        assert!(token_request.contains("refresh_token=old-refresh"));
        assert!(token_request.contains("client_id=cid"));
    }

    #[tokio::test]
    async fn insufficient_scope_needs_a_user_sign_in_and_a_redirect_reports_one() {
        // A 403 whose challenge asks for more scope fails at once.
        let server_url = spawn_oauth_server(Arc::new(|_target, _body| {
            (
                403,
                vec![(
                    "www-authenticate",
                    r#"Bearer error="insufficient_scope", scope="mcp:admin""#.to_string(),
                )],
                String::new(),
            )
        }));
        let auth = provider(
            &format!("{server_url}/mcp"),
            UnlockedStore(MemoryOAuthStateStore::default()),
        );
        let fetch = McpFetch::timed();
        let response = fetch
            .send(
                reqwest::Method::GET,
                &format!("{server_url}/mcp"),
                &[],
                None,
            )
            .await
            .expect("challenge request");
        let error = auth
            .on_unauthorized(UnauthorizedContext {
                response,
                server_url: format!("{server_url}/mcp"),
                fetch,
                token: Some("tok".to_string()),
            })
            .await
            .expect_err("authorization required");
        assert!(matches!(
            error,
            McpError::OAuth(OAuthError::AuthorizationRequired)
        ));
    }

    #[tokio::test]
    async fn a_failing_refresh_falls_through_with_the_old_token() {
        // No server at all: the refresh fails with a network error, and
        // token() falls through to the stored token.
        let store = UnlockedStore(MemoryOAuthStateStore::default());
        seed(
            &store,
            McpOAuthState {
                server_url: "http://127.0.0.1:1/mcp".to_string(),
                client_information: Some(expired_client("cid")),
                tokens: Some(tokens("old", "old-refresh")),
                tokens_expire_at: Some(now_ms() as f64 - 1_000.0),
                ..McpOAuthState::default()
            },
        );
        let auth = provider("http://127.0.0.1:1/mcp", store);
        let token = auth.token().await.expect("token");
        assert_eq!(token.as_deref(), Some("old"));
        // The failed refresh left the old tokens in place.
        let state = auth
            .store
            .load()
            .now_or_never()
            .expect("ready")
            .expect("state")
            .expect("state");
        assert_eq!(
            state
                .tokens
                .as_ref()
                .map(|tokens| tokens.access_token.as_str()),
            Some("old")
        );
    }

    #[tokio::test]
    async fn settled_waits_for_a_running_refresh() {
        let server_url = spawn_discovery_server(
            std::sync::Mutex::new(vec![(
                200,
                serde_json::json!({"access_token": "later", "token_type": "Bearer"}).to_string(),
            )]),
            None,
        );
        let store = UnlockedStore(MemoryOAuthStateStore::default());
        seed(&store, seeded_state(&server_url, "old", "old-refresh"));
        let auth = Arc::new(provider(&format!("{server_url}/mcp"), store));
        // Start a refresh in the background and let settled() wait for it.
        let background = {
            let auth = Arc::clone(&auth);
            tokio::spawn(async move { auth.token().await })
        };
        auth.settled().await;
        let token = background.await.expect("task").expect("token");
        assert_eq!(token.as_deref(), Some("later"));
    }
}
