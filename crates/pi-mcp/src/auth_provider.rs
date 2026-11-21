//! The auth-provider surface of `packages/mcp/src/auth-provider.ts`:
//! bearer tokens for the HTTP transport, plus the 401/insufficient-scope
//! recovery hook the OAuth provider implements. `McpFetch` stands in for
//! the injectable `fetch` functions of the TS module (a reqwest client
//! with the flows' 15 s per-request timeout baked in).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Method;

use crate::error::McpError;

/// Bounds each request to the authorization server, so an unresponsive one
/// cannot hold the refresh lock, delay shutdown, or keep a sign-in waiting
/// for long (`OAUTH_REQUEST_TIMEOUT_MS`, `client-settings.ts:178`).
pub const OAUTH_REQUEST_TIMEOUT_MS: u64 = 15_000;

/// A `fetch` the flows run their authorization-server requests through
/// (`McpFetch`): every request carries the timeout of
/// [`OAUTH_REQUEST_TIMEOUT_MS`], on top of the caller's own deadline.
#[derive(Clone, Debug)]
pub struct McpFetch {
    client: Arc<reqwest::Client>,
}

impl Default for McpFetch {
    fn default() -> Self {
        Self::timed()
    }
}

impl McpFetch {
    /// `timedFetch()` (`client-settings.ts:181-186`).
    pub fn timed() -> Self {
        Self::with_client(
            reqwest::Client::builder()
                .build()
                .expect("oauth fetch client builds"),
        )
    }

    /// A custom client (tests, host interception).
    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client: Arc::new(client),
        }
    }

    /// One request with the timeout applied (`McpFetch` invocation).
    ///
    /// # Errors
    /// Network failures surface as [`McpError::Network`].
    pub async fn send(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, String)],
        body: Option<Vec<u8>>,
    ) -> Result<reqwest::Response, McpError> {
        let mut request = self
            .client
            .request(method, url)
            .timeout(Duration::from_millis(OAUTH_REQUEST_TIMEOUT_MS));
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        request.send().await.map_err(|error| network_error(&error))
    }
}

/// Network failures map like the transport's (`network_error`,
/// `streamable-http.ts:179-181`; TS surfaces them as `TypeError`).
pub(crate) fn network_error(error: &reqwest::Error) -> McpError {
    McpError::Network(error.to_string())
}

/// What a rejected request hands to the auth provider (`UnauthorizedContext`,
/// `auth-provider.ts:3-10`). The response's body is still unread; the
/// transport discards it once the provider returns.
pub struct UnauthorizedContext {
    /// The 401 response, or a 403 response whose challenge reports
    /// `insufficient_scope`.
    pub response: reqwest::Response,
    pub server_url: String,
    /// The transport's fetch, which the provider's flows reuse for their
    /// authorization-server requests.
    pub fetch: McpFetch,
    /// Access token the rejected request carried, if any. A different
    /// current token means another request already refreshed it.
    pub token: Option<String>,
}

/// Supplies bearer tokens to an MCP HTTP transport and may refresh them
/// after a 401 response (`AuthProvider`, `auth-provider.ts:13-20`). The
/// async methods box their futures so the transport can hold
/// `Arc<dyn AuthProvider>`.
pub trait AuthProvider: Send + Sync {
    /// Token for the next request. May refresh it over the network first,
    /// so the transport calls it only before requests whose answer it
    /// waits for. Closing a session reuses the token of the last request.
    /// An error fails the request (`headers()` throwing upstream).
    fn token(&self) -> Pin<Box<dyn Future<Output = Result<Option<String>, McpError>> + Send + '_>> {
        Box::pin(std::future::ready(Ok(None)))
    }

    /// Whether [`AuthProvider::on_unauthorized`] can recover from a
    /// challenge (upstream: the optional `onUnauthorized` method being
    /// present). Without it the rejected response surfaces as-is.
    fn recovers(&self) -> bool {
        false
    }

    /// Recovers from a 401 (or a 403 asking for more scope). Only called
    /// when [`AuthProvider::recovers`] holds; the rejected response is
    /// consumed here and the request retried once afterwards. Errors
    /// bubble in place of the response (`authorizedFetch`,
    /// `streamable-http.ts:352-374`).
    fn on_unauthorized(
        &self,
        context: UnauthorizedContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), McpError>> + Send + '_>>;
}
