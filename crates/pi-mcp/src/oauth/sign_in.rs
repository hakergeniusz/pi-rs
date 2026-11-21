//! Sign-in to an OAuth-protected MCP server
//! (`packages/mcp/src/oauth/sign-in.ts`), in two halves so an application
//! can complete it in another request with the same state store:
//! [`start_mcp_sign_in`] refreshes the stored tokens when it can, or else
//! prepares the browser step and returns the authorization URL;
//! [`complete_mcp_sign_in`] exchanges the authorization code. The code
//! arrives through a loopback callback server
//! ([`crate::oauth::loopback`]), the application's own https callback
//! route, or a redirect URL the user pastes. Upstream's `AbortSignal`s
//! become [`CancellationToken`]s: `cancellable` races the work against the
//! token, and dropping a future stands in for aborting it.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use url::Url;

use crate::auth_provider::McpFetch;
use crate::error::McpError;
use crate::oauth::callback::OAuthCallback;
use crate::oauth::client_settings::{
    ClientRegistration, McpClientMetadataDocuments, McpOAuthClientOptions, McpOAuthServerStore,
    McpOAuthSettings, create_oauth_client_provider, offers_client_metadata_document,
    registered_redirect_urls,
};
use crate::oauth::errors::OAuthError;
use crate::oauth::flow::{OAuthFlowOptions, OAuthFlowResult, authorize_mcp, step_up_scope};
use crate::oauth::provider::McpOAuthState;
use crate::oauth::types::OAuthChallenge;

type Result<T> = std::result::Result<T, McpError>;

fn other(message: impl Into<String>) -> McpError {
    McpError::OAuth(OAuthError::Other(message.into()))
}

fn cancelled() -> McpError {
    McpError::OAuth(OAuthError::SignInCancelled)
}

/// Options of both halves of a sign-in (`McpSignInOptions`,
/// `sign-in.ts:23-40`). Pass the same values to
/// [`start_mcp_sign_in`] and [`complete_mcp_sign_in`].
pub struct McpSignInOptions {
    pub server_url: String,
    pub store: Arc<dyn McpOAuthServerStore>,
    pub settings: McpOAuthSettings,
    pub client_name: String,
    /// Omitted: dynamic client registration only.
    pub client_metadata_documents: Option<Arc<dyn McpClientMetadataDocuments>>,
    /// Redirect URI chosen by the caller: a loopback one
    /// ([`crate::oauth::loopback::listen_for_loopback_sign_in`]) or an
    /// https callback route of the application. Pass the same one to both
    /// halves: the authorization request may use a variant specific to the
    /// MCP server, which the flow derives from it.
    pub redirect_url: String,
    /// The server's challenge that asked for the sign-in; its resource
    /// metadata URL and scope apply.
    pub challenge: Option<OAuthChallenge>,
    /// Aborting stops the sign-in at once with a cancellation. A refresh
    /// already sent still completes and saves its tokens.
    pub signal: Option<CancellationToken>,
}

impl McpSignInOptions {
    /// The sign-in options as the client options the flows take.
    fn as_client_options(&self) -> McpOAuthClientOptions {
        McpOAuthClientOptions {
            server_url: self.server_url.clone(),
            store: Arc::clone(&self.store),
            settings: self.settings.clone(),
            client_name: self.client_name.clone(),
            client_metadata_documents: self.client_metadata_documents.clone(),
        }
    }
}

/// The browser step of a sign-in, from [`start_mcp_sign_in`]. Plain data,
/// so an application can persist it (`McpSignInRedirect`,
/// `sign-in.ts:48-58`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSignInRedirect {
    /// Open in the user's browser.
    pub authorization_url: String,
    /// The redirect URI of the authorization request; the authorization
    /// response must arrive on it.
    pub redirect_url: String,
    /// The `state` parameter the authorization response must carry.
    pub state: String,
    /// The scope requested, if any. Saved with the tokens when the token
    /// response names none.
    pub scope: Option<String>,
}

/// Result of [`start_mcp_sign_in`]: signed in by refreshing the stored
/// tokens, or the browser step (`McpSignInStart`, `sign-in.ts:61`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpSignInStart {
    Authorized,
    Redirect(McpSignInRedirect),
}

/// An authorization response: from a callback server, a callback route, or
/// [`crate::oauth::loopback::response_from_redirect_url`]
/// (`McpAuthorizationResponse`, `sign-in.ts:63`).
pub type McpAuthorizationResponse = OAuthCallback;

/// First half of a sign-in (`startMcpSignIn`, `sign-in.ts:89-99`). Without
/// a step-up (`insufficient_scope`), it first tries to refresh the stored
/// tokens with the stored client, whatever `redirect_url` is
/// ([`McpSignInStart::Authorized`]). The refresh takes the store's refresh
/// lock, so never call this while holding it; when another process holds
/// the lock for the whole wait ([`OAuthError::RefreshLockBusy`]), the
/// refresh is skipped, and any other failure of the lock fails the
/// sign-in. Otherwise it prepares the browser step
/// ([`McpSignInStart::Redirect`]): a fresh `state`, and a stored client
/// that cannot use `redirect_url` is dropped with its tokens. Requests to
/// the authorization server are time-limited.
///
/// # Errors
/// Storage failures and flow failures surface as [`McpError`].
pub async fn start_mcp_sign_in(options: &McpSignInOptions) -> Result<McpSignInStart> {
    if options
        .signal
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        return Err(cancelled());
    }
    // A refresh keeps the granted scope; a server asking for more needs
    // the browser step (`sign-in.ts:93`).
    let step_up = options
        .challenge
        .as_ref()
        .is_some_and(|challenge| challenge.error.as_deref() == Some("insufficient_scope"));
    if !step_up && refresh_before_sign_in(options).await? == OAuthFlowResult::Authorized {
        return Ok(McpSignInStart::Authorized);
    }
    Ok(McpSignInStart::Redirect(
        cancellable(
            options.signal.as_ref(),
            Box::pin(start_browser_step(options, step_up)),
        )
        .await?,
    ))
}

/// Refresh the stored tokens, unless `cimd` is to replace a registered
/// client (`refreshBeforeSignIn`, `sign-in.ts:101-128`). The refresh uses
/// `redirect_url` when the tokens came from a Client ID Metadata Document
/// whose documents list it.
async fn refresh_before_sign_in(options: &McpSignInOptions) -> Result<OAuthFlowResult> {
    // The lock body's outcome, handed back out of the `Result<(), McpError>`
    // the store's lock interface carries.
    let outcome: Arc<tokio::sync::Mutex<Option<Result<OAuthFlowResult>>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let body_outcome = Arc::clone(&outcome);
    let body_signal = options.signal.clone();
    let body = Box::pin(async move {
        // The lock may arrive after the sign-in was cancelled: return at
        // once, so it is released (`sign-in.ts:109-111`).
        if body_signal
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            *body_outcome.lock().await = Some(Ok(OAuthFlowResult::Redirect));
            return Ok(());
        }
        let state = options.store.load().await?;
        let has_refresh_token = state
            .as_ref()
            .and_then(|state| state.tokens.as_ref())
            .and_then(|tokens| tokens.refresh_token.as_deref())
            .is_some_and(|token| !token.is_empty());
        if !has_refresh_token {
            *body_outcome.lock().await = Some(Ok(OAuthFlowResult::Redirect));
            return Ok(());
        }
        if options.settings.client_registration == Some(ClientRegistration::Cimd)
            && state
                .as_ref()
                .and_then(|state| state.client_information.as_ref())
                .is_some()
            && options.settings.client_id.is_none()
        {
            *body_outcome.lock().await = Some(Ok(OAuthFlowResult::Redirect));
            return Ok(());
        }
        let Some(state) = state else {
            return Ok(());
        };
        let result = crate::oauth::auth_provider::refresh_stored_tokens(
            &options.as_client_options(),
            &state,
            crate::oauth::auth_provider::RefreshStoredOptions {
                fetch: None,
                challenge: options.challenge.clone(),
                redirect_url: Some(options.redirect_url.clone()),
            },
        )
        .await;
        *body_outcome.lock().await = Some(result);
        Ok(())
    });
    match options.store.with_refresh_lock(body).await {
        Err(error) => {
            // Another process is refreshing and took too long. The user
            // signs in instead (`sign-in.ts:121-124`).
            if matches!(error, McpError::OAuth(OAuthError::RefreshLockBusy)) {
                return Ok(OAuthFlowResult::Redirect);
            }
            Err(error)
        }
        Ok(()) => outcome
            .lock()
            .await
            .take()
            .expect("the lock body always writes an outcome"),
    }
}

/// Run `fut`, or fail with a cancellation as soon as `signal` aborts
/// (`cancellable`, `sign-in.ts:203-215`). A completed success survives an
/// abort racing it; a failure under an abort reports the cancellation.
pub(crate) async fn cancellable<T, F>(signal: Option<&CancellationToken>, fut: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    tokio::select! {
        biased;
        result = fut => match result {
            Ok(value) => Ok(value),
            // Aborted requests fail with the signal's reason; report them
            // as the cancellation they are.
            Err(error) => {
                if signal.is_some_and(CancellationToken::is_cancelled) {
                    Err(cancelled())
                } else {
                    Err(error)
                }
            }
        },
        () = async {
            if let Some(signal) = signal {
                signal.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        } => Err(cancelled()),
    }
}

async fn start_browser_step(
    options: &McpSignInOptions,
    step_up: bool,
) -> Result<McpSignInRedirect> {
    let stored = options.store.load().await?;
    if let Some(stored) = &stored {
        options
            .store
            .save(state_for_sign_in(stored.clone(), options))
            .await?;
    }
    // The flow reports the authorization URL through the redirect hook
    // (`sign-in.ts:143-145`).
    let captured: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
    let on_redirect = {
        let captured = Arc::clone(&captured);
        Arc::new(move |url: &str| {
            *captured.lock().expect("authorization url slot poisoned") = Some(url.to_string());
        }) as crate::oauth::provider::RedirectCallback
    };
    let provider = create_oauth_client_provider(
        &options.as_client_options(),
        &options.redirect_url,
        Some(on_redirect),
    )?;
    // A server asking for more scope gets it on top of the configured
    // scope and, since the challenge may list only the missing scopes, on
    // top of the scope granted so far (`sign-in.ts:146-151`).
    let challenged = if step_up {
        step_up_scope(
            stored
                .as_ref()
                .and_then(|state| state.tokens.as_ref())
                .and_then(|tokens| tokens.scope.as_deref()),
            options
                .challenge
                .as_ref()
                .and_then(|challenge| challenge.scope.as_deref()),
        )
    } else {
        options
            .challenge
            .as_ref()
            .and_then(|challenge| challenge.scope.clone())
    };
    let scope = merge_scopes(&[options.settings.scope.as_deref(), challenged.as_deref()]);
    authorize_mcp(
        &provider,
        OAuthFlowOptions {
            server_url: options.server_url.clone(),
            scope,
            skip_refresh: true,
            ..flow_options(options)
        },
    )
    .await?;
    let authorization_url = captured
        .lock()
        .expect("authorization url slot poisoned")
        .take()
        .ok_or_else(|| other("OAuth flow did not produce an authorization URL"))?;
    let url = Url::parse(&authorization_url).map_err(|error| other(error.to_string()))?;
    // The flow picks the redirect URI, which may be specific to the MCP
    // server (`sign-in.ts:154-156`).
    let redirect_url =
        query_parameter(&url, "redirect_uri").unwrap_or_else(|| options.redirect_url.clone());
    let state = provider.state().await?;
    Ok(McpSignInRedirect {
        authorization_url,
        redirect_url,
        state,
        // Saved with the tokens when the token response names no scope, so
        // it must be the requested one (`sign-in.ts:159`).
        scope: query_parameter(&url, "scope").filter(|scope| !scope.is_empty()),
    })
}

/// The stored state for a browser step: without the last sign-in's
/// `state`, and without a client that cannot use `redirect_url`, along
/// with its tokens (`stateForSignIn`, `sign-in.ts:157-180`).
fn state_for_sign_in(stored: McpOAuthState, options: &McpSignInOptions) -> McpOAuthState {
    let mut next = stored;
    next.oauth_state = None;
    // A registered client cannot use another redirect URI, and its tokens
    // belong to it. `cimd` replaces a registered client; `auto` keeps it.
    // A Client ID Metadata Document is not stored, so tokens without a
    // stored client belong to the document (`sign-in.ts:164-171`).
    let keep_client = if options.settings.client_id.is_some() {
        true
    } else {
        match &next.client_information {
            Some(client) => {
                options.settings.client_registration != Some(ClientRegistration::Cimd)
                    && registered_redirect_urls(Some(client))
                        .iter()
                        .any(|uri| uri == &options.redirect_url)
            }
            None => offers_client_metadata_document(
                &options.settings,
                options.client_metadata_documents.as_deref(),
                &options.redirect_url,
            ),
        }
    };
    if !keep_client {
        next.client_information = None;
        next.tokens = None;
        next.tokens_expire_at = None;
    }
    next
}

/// Scopes of both lists, each once (`mergeScopes`, `sign-in.ts:182-186`).
fn merge_scopes(scopes: &[Option<&str>]) -> Option<String> {
    let mut seen = HashSet::new();
    let mut merged: Vec<&str> = Vec::new();
    for scope in scopes
        .iter()
        .flatten()
        .flat_map(|scope| scope.split_whitespace())
    {
        if seen.insert(scope) {
            merged.push(scope);
        }
    }
    (!merged.is_empty()).then(|| merged.join(" "))
}

/// Second half of a sign-in: exchanges the authorization code of
/// `response` and saves the tokens (`completeMcpSignIn`,
/// `sign-in.ts:188-213`). Pass the options given to
/// [`start_mcp_sign_in`], including its `redirect_url` (not
/// `authorization.redirect_url`). Fails when the response carries another
/// `state`, when another sign-in started with the store since, or when the
/// exchange would use another redirect URI than the authorization request
/// (the options changed). A redirect URL the user pasted goes through
/// [`crate::oauth::loopback::response_from_redirect_url`] first, which
/// checks its path.
///
/// # Errors
/// The mismatches above, plus storage and flow failures.
pub async fn complete_mcp_sign_in(
    options: &McpSignInOptions,
    authorization: &McpSignInRedirect,
    response: &McpAuthorizationResponse,
) -> Result<()> {
    cancellable(options.signal.as_ref(), async {
        if response.state != authorization.state {
            return Err(other(
                "The authorization response belongs to a different sign-in",
            ));
        }
        // The stored PKCE verifier belongs to the latest sign-in;
        // exchanging with it would fail with `invalid_grant`, which
        // discards the stored tokens (`sign-in.ts:198-201`).
        let stored_state = options.store.load().await?;
        if stored_state.and_then(|state| state.oauth_state).as_deref()
            != Some(authorization.state.as_str())
        {
            return Err(other(
                "Another sign-in to this MCP server started since; sign in again",
            ));
        }
        let provider = create_oauth_client_provider(
            &options.as_client_options(),
            &options.redirect_url,
            None,
        )?;
        authorize_mcp(
            &provider,
            OAuthFlowOptions {
                server_url: options.server_url.clone(),
                // Saved with the tokens when the token response names no
                // scope, so it must be the requested one
                // (`sign-in.ts:206`).
                scope: authorization.scope.clone(),
                authorization_redirect_url: Some(authorization.redirect_url.clone()),
                authorization_code: Some(response.code.clone()),
                iss: response.iss.clone(),
                ..flow_options(options)
            },
        )
        .await
        .map(|_| ())
    })
    .await
}

/// `authorize_mcp` options of a sign-in, without the scope
/// (`flowOptions`, `sign-in.ts:215-222`).
fn flow_options(options: &McpSignInOptions) -> OAuthFlowOptions {
    OAuthFlowOptions {
        server_url: options.server_url.clone(),
        fetch: Some(McpFetch::timed()),
        resource_metadata_url: options
            .challenge
            .as_ref()
            .and_then(|challenge| challenge.resource_metadata_url.clone()),
        authorization_server_metadata_url: options.settings.auth_server_metadata_url.clone(),
        ..OAuthFlowOptions::default()
    }
}

/// The first value of one query parameter (`searchParams.get`).
fn query_parameter(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}
