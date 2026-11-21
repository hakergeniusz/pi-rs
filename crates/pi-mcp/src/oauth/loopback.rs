//! How the authorization response of a sign-in reaches a local
//! application (`packages/mcp/src/oauth/loopback.ts`): a loopback callback
//! server the browser redirects to, or a redirect URL the user pastes from
//! the browser address bar. Upstream's `AbortSignal`s become
//! [`CancellationToken`]s; the losing side of the wait-race is dropped
//! where upstream aborts it.

use std::pin::Pin;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::McpError;
use crate::oauth::callback::{OAuthCallback, OAuthCallbackServer, RenderPage};
use crate::oauth::client_settings::{
    McpClientMetadataDocuments, McpOAuthSettings, loopback_settings,
    offers_client_metadata_document, registered_redirect_urls, server_specific_path,
};
use crate::oauth::errors::OAuthError;
use crate::oauth::provider::McpOAuthStateStore;
use crate::oauth::sign_in::{McpAuthorizationResponse, McpSignInRedirect};

type Result<T> = std::result::Result<T, McpError>;

fn other(message: impl Into<String>) -> McpError {
    McpError::OAuth(OAuthError::Other(message.into()))
}

fn cancelled() -> McpError {
    McpError::OAuth(OAuthError::SignInCancelled)
}

/// A loopback callback server and the redirect URI it serves
/// (`McpLoopbackCallback`, `loopback.ts:11-14`).
pub struct McpLoopbackCallback {
    pub callback: OAuthCallbackServer,
    /// Pass to [`crate::oauth::sign_in::start_mcp_sign_in`] and
    /// [`crate::oauth::sign_in::complete_mcp_sign_in`].
    pub redirect_url: String,
}

/// Options of [`listen_for_loopback_sign_in`] (`loopback.ts:24-32`).
pub struct ListenForLoopbackSignInOptions<'a> {
    pub server_url: &'a str,
    pub store: Arc<dyn McpOAuthStateStore>,
    pub settings: &'a McpOAuthSettings,
    pub client_metadata_documents: Option<Arc<dyn McpClientMetadataDocuments>>,
    /// Renders the browser page after the redirect. Default: plain text.
    pub render_page: Option<RenderPage>,
}

/// Listen for the authorization response of a sign-in on loopback, where
/// `settings` say: `callback_url` and `callback_port`, else the port of
/// the stored client's redirect URI (so that client stays valid) or a free
/// one. Pass the sign-in's `client_metadata_documents`, so the callback
/// server also serves the server-specific redirect URI
/// (`listenForLoopbackSignIn`, `loopback.ts:34-65`). Close the callback
/// server when the sign-in ends.
///
/// # Errors
/// Storage failures surface as [`McpError`]; binding twice on a required
/// configured port fails at once, a preferred port falls back to a free
/// one.
pub async fn listen_for_loopback_sign_in(
    options: ListenForLoopbackSignInOptions<'_>,
) -> Result<McpLoopbackCallback> {
    let loopback = loopback_settings(options.settings)?;
    let registered = registered_redirect_urls(
        options
            .store
            .load()
            .await?
            .as_ref()
            .and_then(|state| state.client_information.as_ref()),
    )
    .into_iter()
    .next();
    // The port of the stored client's redirect URI is only preferred
    // (`Number(new URL(registered).port) || undefined`,
    // `loopback.ts:39`).
    let registered_port = registered
        .as_deref()
        .and_then(|uri| Url::parse(uri).ok())
        .and_then(|url| url.port())
        .filter(|port| *port != 0);
    let port = loopback.port.or(registered_port);
    let offers_document = offers_client_metadata_document(
        options.settings,
        options.client_metadata_documents.as_deref(),
        &loopback.url,
    );
    let extra_paths = if offers_document {
        vec![server_specific_path(&loopback.path, options.server_url)?]
    } else {
        Vec::new()
    };
    let build_options = |port: Option<u16>| crate::oauth::callback::OAuthCallbackServerOptions {
        host: Some(loopback.host.as_str()),
        redirect_host: Some(loopback.redirect_host.as_str()),
        port,
        path: Some(loopback.path.as_str()),
        extra_paths: extra_paths.clone(),
        timeout_ms: None,
        render_page: options.render_page.clone(),
    };
    let callback = match OAuthCallbackServer::listen(build_options(port)).await {
        Ok(callback) => callback,
        Err(error) => {
            // A configured port is required; the stored client's port is
            // only preferred (`loopback.ts:55-60`).
            if loopback.port.is_some() || port.is_none() {
                return Err(error);
            }
            OAuthCallbackServer::listen(build_options(None)).await?
        }
    };
    let redirect_url = loopback
        .fixed_redirect_url
        .unwrap_or_else(|| callback.redirect_url().to_string());
    Ok(McpLoopbackCallback {
        callback,
        redirect_url,
    })
}

/// Asks for the redirect URL of the browser address bar when the browser
/// cannot reach the loopback callback, for example over SSH. Aborted once
/// the callback arrives or the sign-in is cancelled. Resolving to an empty
/// string cancels the sign-in (`promptForRedirectUrl`,
/// `loopback.ts:47-51`).
pub type PromptForRedirectUrl = Arc<
    dyn Fn(CancellationToken) -> Pin<Box<dyn Future<Output = Result<String>> + Send>> + Send + Sync,
>;

/// Options of [`wait_for_loopback_sign_in`] (`loopback.ts:68-76`).
pub struct WaitForLoopbackSignInOptions {
    pub callback: OAuthCallbackServer,
    /// The `redirect` result of
    /// [`crate::oauth::sign_in::start_mcp_sign_in`].
    pub authorization: McpSignInRedirect,
    pub prompt_for_redirect_url: Option<PromptForRedirectUrl>,
    /// Aborting stops the wait at once with a cancellation.
    pub signal: Option<CancellationToken>,
}

/// Wait for the authorization response on a loopback callback server, or
/// for a redirect URL the user pastes from the browser address bar,
/// whichever comes first (`waitForLoopbackSignIn`, `loopback.ts:78-104`).
///
/// # Errors
/// The callback's failure, the prompt's failure, or a cancellation.
pub async fn wait_for_loopback_sign_in(
    options: WaitForLoopbackSignInOptions,
) -> Result<McpAuthorizationResponse> {
    if options
        .signal
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        return Err(cancelled());
    }
    // Aborted once the race ends, so the losing prompt stops
    // (`controller.abort()`, `loopback.ts:101`).
    let prompt_signal = options
        .signal
        .as_ref()
        .map_or_else(CancellationToken::new, CancellationToken::child_token);
    let path = Url::parse(&options.authorization.redirect_url)
        .map_err(|error| other(error.to_string()))?
        .path()
        .to_string();
    let from_browser = options
        .callback
        .wait_for_callback(&options.authorization.state, Some(path))?;
    let from_user: Pin<Box<dyn Future<Output = Result<McpAuthorizationResponse>> + Send>> =
        if let Some(prompt) = &options.prompt_for_redirect_url {
            let authorization = options.authorization.clone();
            let prompt_signal = prompt_signal.clone();
            Box::pin(async move {
                let input = prompt(prompt_signal).await?;
                if input.trim().is_empty() {
                    return Err(cancelled());
                }
                response_from_redirect_url(&input, &authorization)
            })
        } else {
            let prompt_signal = prompt_signal.clone();
            Box::pin(async move {
                prompt_signal.cancelled().await;
                Err(cancelled())
            })
        };
    let result = tokio::select! {
        response = from_browser => response,
        response = from_user => response,
        () = async {
            if let Some(signal) = &options.signal {
                signal.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        } => Err(cancelled()),
    };
    prompt_signal.cancel();
    result
}

/// The authorization response in a redirect URL the user pasted from the
/// browser address bar (`responseFromRedirectUrl`,
/// `loopback.ts:106-133`). Fails when the URL is not on the sign-in's
/// redirect URI, carries another `state`, or reports an error.
///
/// # Errors
/// One of the messages above.
pub fn response_from_redirect_url(
    input: &str,
    authorization: &McpSignInRedirect,
) -> Result<McpAuthorizationResponse> {
    let url = Url::parse(input.trim())
        .map_err(|_| other("Expected the full redirect URL from the browser address bar"))?;
    let redirect_url =
        Url::parse(&authorization.redirect_url).map_err(|error| other(error.to_string()))?;
    // A server-specific redirect URI tells authorization servers apart, so
    // it must match exactly (`loopback.ts:117-120`).
    if url.origin().ascii_serialization() != redirect_url.origin().ascii_serialization()
        || url.path() != redirect_url.path()
    {
        return Err(other(
            "The redirect URL does not match this sign-in's redirect URI",
        ));
    }
    if let Some(error) = query_parameter(&url, "error") {
        return Err(other(
            query_parameter(&url, "error_description").unwrap_or(error),
        ));
    }
    if query_parameter(&url, "state").as_deref() != Some(authorization.state.as_str()) {
        return Err(other("The redirect URL belongs to a different sign-in"));
    }
    let Some(code) = query_parameter(&url, "code").filter(|code| !code.is_empty()) else {
        return Err(other(
            "The redirect URL does not contain an authorization code",
        ));
    };
    let iss = query_parameter(&url, "iss").filter(|iss| !iss.is_empty());
    Ok(OAuthCallback {
        code,
        state: authorization.state.clone(),
        iss,
    })
}

/// The first value of one query parameter (`searchParams.get`).
fn query_parameter(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authorization() -> McpSignInRedirect {
        McpSignInRedirect {
            authorization_url: "https://auth.example/authorize?state=s1".to_string(),
            redirect_url: "http://localhost:9000/callback".to_string(),
            state: "s1".to_string(),
            scope: None,
        }
    }

    #[test]
    fn a_pasted_redirect_url_is_parsed_and_checked() {
        let authorization = authorization();
        let response = response_from_redirect_url(
            " http://localhost:9000/callback?code=c&state=s1&iss=https://auth.example",
            &authorization,
        )
        .expect("response");
        assert_eq!(response.code, "c");
        assert_eq!(response.state, "s1");
        assert_eq!(response.iss.as_deref(), Some("https://auth.example"));
        // Not the sign-in's redirect URI (port or path).
        let error = response_from_redirect_url(
            "http://localhost:9001/callback?code=c&state=s1",
            &authorization,
        )
        .expect_err("port");
        assert_eq!(
            error.to_string(),
            "The redirect URL does not match this sign-in's redirect URI"
        );
        let error = response_from_redirect_url(
            "http://localhost:9000/other?code=c&state=s1",
            &authorization,
        )
        .expect_err("path");
        assert_eq!(
            error.to_string(),
            "The redirect URL does not match this sign-in's redirect URI"
        );
        // Not a URL at all.
        let error = response_from_redirect_url("not a url", &authorization).expect_err("garbage");
        assert_eq!(
            error.to_string(),
            "Expected the full redirect URL from the browser address bar"
        );
        // An error parameter wins with its description.
        let error = response_from_redirect_url(
            "http://localhost:9000/callback?error=access_denied&error_description=nope&state=s1",
            &authorization,
        )
        .expect_err("denied");
        assert_eq!(error.to_string(), "nope");
        // Another sign-in's state, and a missing code.
        let error = response_from_redirect_url(
            "http://localhost:9000/callback?code=c&state=s2",
            &authorization,
        )
        .expect_err("state");
        assert_eq!(
            error.to_string(),
            "The redirect URL belongs to a different sign-in"
        );
        let error =
            response_from_redirect_url("http://localhost:9000/callback?state=s1", &authorization)
                .expect_err("no code");
        assert_eq!(
            error.to_string(),
            "The redirect URL does not contain an authorization code"
        );
    }
}
