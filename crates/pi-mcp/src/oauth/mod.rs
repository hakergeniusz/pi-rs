//! The OAuth client for MCP servers (`packages/mcp/src/oauth/`, the
//! module list of `index.ts`): discovery, wire types and their errors, the
//! flows, the state provider, client settings, the loopback callback, and
//! the two sign-in halves.
//!
//! Deviations of the port, module-wide: there are no `AbortSignal`s —
//! sign-in cancellation rides [`tokio_util::sync::CancellationToken`] and
//! dropping a future stands in for aborting a request; the injectable
//! `fetch` functions are [`crate::auth_provider::McpFetch`] (always
//! carrying the 15 s per-request timeout); and the `OAuthClientProvider`
//! interface is the concrete [`provider::McpOAuthProvider`] — no pi code
//! supplies a custom one.

pub mod auth_provider;
pub mod callback;
pub mod client_settings;
pub mod discovery;
pub mod errors;
pub mod flow;
pub mod loopback;
pub mod provider;
pub mod sign_in;
pub mod types;

pub use auth_provider::{McpAuthProvider, McpAuthProviderOptions, RefreshStoredOptions};
pub use callback::{
    OAuthCallback, OAuthCallbackPage, OAuthCallbackServer, OAuthCallbackServerOptions, RenderPage,
};
pub use client_settings::{
    ClientRegistration, DEFAULT_REDIRECT_URL, LoopbackSettings, McpClientMetadataDocuments,
    McpOAuthClientOptions, McpOAuthServerStore, McpOAuthSettings, callback_id,
    create_oauth_client_provider, loopback_settings, offers_client_metadata_document,
    registered_redirect_urls, server_specific_path,
};
pub use discovery::{
    AuthorizationServerMetadataOptions, DiscoverOptions, ProtectedResourceMetadataOptions,
    discover_authorization_server_metadata, discover_oauth_server_info,
    discover_protected_resource_metadata, parse_www_authenticate, resource_url_from_server_url,
    select_resource,
};
pub use errors::OAuthError;
pub use flow::{
    OAuthClientMetadataDocument, OAuthFlowOptions, OAuthFlowResult, authorize_mcp,
    exchange_authorization_code, refresh_authorization, register_client, start_authorization,
    step_up_scope,
};
pub use loopback::{
    ListenForLoopbackSignInOptions, McpLoopbackCallback, PromptForRedirectUrl,
    WaitForLoopbackSignInOptions, listen_for_loopback_sign_in, response_from_redirect_url,
    wait_for_loopback_sign_in,
};
pub use provider::{
    McpOAuthProvider, McpOAuthProviderOptions, McpOAuthState, McpOAuthStateStore,
    MemoryOAuthStateStore,
};
pub use sign_in::{
    McpAuthorizationResponse, McpSignInOptions, McpSignInRedirect, McpSignInStart,
    complete_mcp_sign_in, start_mcp_sign_in,
};
pub use types::{
    AuthorizationServerMetadata, OAuthChallenge, OAuthClientInformationFull, OAuthClientMetadata,
    OAuthDiscoveryState, OAuthProtectedResourceMetadata, OAuthServerInfo, OAuthTokens,
    parse_authorization_server_metadata, parse_client_information, parse_oauth_tokens,
    parse_protected_resource_metadata,
};
