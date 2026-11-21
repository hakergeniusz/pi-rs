//! OAuth error classes (`packages/mcp/src/oauth/errors.ts:1-66`, plus
//! `McpSignInCancelledError` from `sign-in.ts:60-65` — one Rust enum where
//! upstream has sibling `Error` subclasses).

/// An OAuth flow failure. One variant per upstream error class.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OAuthError {
    /// The token endpoint reported an RFC 6749 error
    /// (`OAuthError`; `error_description` defaults the message to the code).
    #[error("{message}")]
    Protocol {
        code: String,
        message: String,
        error_uri: Option<String>,
    },
    /// The discovered issuer or an authorization response's `iss` did not
    /// match (`OAuthIssuerMismatchError`). `None` renders as `none`, a
    /// present value as its JSON spelling.
    #[error("OAuth issuer mismatch: expected {}, received {}", fmt(.expected), fmt(.received))]
    IssuerMismatch {
        expected: Option<String>,
        received: Option<String>,
    },
    /// A credential endpoint was not https and not loopback
    /// (`OAuthInsecureEndpointError`).
    #[error("Refusing to send OAuth credentials to non-HTTPS endpoint {0}")]
    InsecureEndpoint(String),
    /// Dynamic client registration failed (`OAuthRegistrationError`).
    #[error("OAuth dynamic client registration failed with status {status}: {body}")]
    Registration { status: u16, body: String },
    /// A refresh cannot proceed without the user; sign-in runs through
    /// `/mcp` or `pi mcp login` (`McpOAuthAuthorizationRequiredError`).
    #[error("MCP OAuth authorization requires user interaction")]
    AuthorizationRequired,
    /// Another process held the refresh lock for the whole wait
    /// (`McpRefreshLockBusyError`).
    #[error("Another process is refreshing the MCP server's tokens")]
    RefreshLockBusy,
    /// The user or the application cancelled a sign-in
    /// (`McpSignInCancelledError`).
    #[error("Sign-in cancelled")]
    SignInCancelled,
    /// Every plain `new Error(...)` throw of the upstream oauth module —
    /// structural validation (`"Invalid …"`), unsupported servers, and
    /// flow preconditions carry their exact upstream message here.
    #[error("{0}")]
    Other(String),
}

/// The JSON spelling upstream's constructor uses for one issuer side
/// (`errors.ts:20`: `undefined` → `none`, else `JSON.stringify`).
#[allow(clippy::ref_option)] // thiserror's `fmt(.field)` hands `&Option<String>`
fn fmt(issuer: &Option<String>) -> String {
    match issuer {
        None => "none".to_string(),
        Some(value) => serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\"")),
    }
}
