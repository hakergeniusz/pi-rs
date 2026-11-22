//! Built-in per-provider OAuth login and refresh flows.
//!
//! Ground truth (canonical TypeScript, `packages/ai/src/auth/`):
//! - `types.ts` — `OAuthCredential` (`:24-37`), `AuthPrompt` (`:119-130`),
//!   `AuthEvent` (`:137-151`), `AuthInteraction`/`ProviderAuthInteraction`
//!   (`:153-168`), `OAuthAuth` (`:225-249`), `LoginOptions` (`:205-218`).
//! - `oauth/pkce.ts` — `base64urlEncode`/`generatePKCE` (`:9-34`).
//! - `oauth/trusted-url.ts` — `trustedHttpUrl` (`:6-10`).
//! - `oauth/callback-server.ts` — `OAuthCallbackServerOptions` (`:12-31`),
//!   `startOAuthCallbackServer` (`:50-148`), `waitForCallbackOrManualInput`
//!   (`:155-183`).
//! - `oauth/device-code.ts` — poll loop, `slow_down` and timeout messages
//!   (`:1-98`).
//! - `oauth/anthropic.ts` — constants (`:13-27`), token exchange (`:96-138`),
//!   browser/copy-code login (`:140-235`), refresh (`:240-276`), the
//!   `anthropicOAuth` surface (`:278-307`).
//! - `oauth/openai-codex.ts` — constants (`:22-37`), token responses
//!   (`:122-185`), device flow (`:187-287`), browser login (`:289-400`),
//!   refresh (`:405-438`).
//! - `oauth/github-copilot.ts` — domain/token helpers (`:42-88`), device flow
//!   (`:207-305`), refresh/model catalogue (`:307-361`), login (`:428-501`).
//!
//! Deviations (see `WIRING.md` at the worktree root):
//! - The app-owned interaction callbacks become the concrete
//!   [`Interaction`]/[`InteractionMessage`] channel pair; each flow is driven
//!   by `login`, which is dispatched by provider id instead of by a JS
//!   `OAuthAuth` object.
//! - Provider endpoints and callback ports live in [`FlowConfig`] so tests can
//!   point them at a synthetic loopback server; `FlowConfig::default()` is the
//!   verbatim upstream table.
//! - Randomness comes from `/dev/urandom` (with a time/pid fallback) instead
//!   of `crypto.getRandomValues`; SHA-256 is the `sha2` crate instead of
//!   Web Crypto.
//! - The callback server runs on `std::net` inside a blocking task rather than
//!   Node's `http.createServer`; the route, `state`, error and code handling,
//!   status codes and error strings mirror upstream exactly.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::{oneshot, watch};

pub mod anthropic;
pub mod callback;
pub mod device_code;
pub mod github_copilot;
pub mod http;
pub mod openai_codex;
pub mod pkce;
pub mod trusted_url;

/// `CANCEL_MESSAGE` (`device-code.ts:1`).
pub const CANCEL_MESSAGE: &str = "Login cancelled";

/// Stored canonical OAuth credential (`auth/types.ts:31-34`, plus the
/// provider-specific `accountId`, `enterpriseUrl` and `availableModelIds`
/// fields the three built-in flows attach).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthCredential {
    pub access: String,
    pub refresh: String,
    /// Unix milliseconds; Anthropic and Copilot already subtract their
    /// five-minute skew here (`anthropic.ts:136`, `github-copilot.ts:339`).
    pub expires: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enterprise_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_model_ids: Vec<String>,
}

impl OAuthCredential {
    /// The `auth.json` shape (`types.ts:31-37`): the `type` discriminant is
    /// added here so the credential can be persisted verbatim.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut object = Map::new();
        object.insert("type".to_string(), json!("oauth"));
        object.insert("access".to_string(), json!(self.access));
        object.insert("refresh".to_string(), json!(self.refresh));
        object.insert("expires".to_string(), json!(self.expires));
        if let Some(account_id) = &self.account_id {
            object.insert("accountId".to_string(), json!(account_id));
        }
        if let Some(enterprise_url) = &self.enterprise_url {
            object.insert("enterpriseUrl".to_string(), json!(enterprise_url));
        }
        if !self.available_model_ids.is_empty() {
            object.insert(
                "availableModelIds".to_string(),
                json!(self.available_model_ids),
            );
        }
        Value::Object(object)
    }
}

/// Errors a login/refresh flow reports. Upstream throws plain `Error`s, so
/// the display text is the observable contract.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthError {
    /// Any upstream `new Error(message)`.
    #[error("{0}")]
    Message(String),
    /// An aborted flow (`CANCEL_MESSAGE`).
    #[error("Login cancelled")]
    Cancelled,
    /// The host dropped the interaction channel while a prompt was open.
    #[error("Login prompt unavailable")]
    PromptUnavailable,
}

impl From<String> for AuthError {
    fn from(value: String) -> Self {
        Self::Message(value)
    }
}

/// One option of a `select` prompt (`auth/types.ts:128`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthPromptOption {
    pub id: String,
    pub label: String,
}

/// `AuthPrompt` (`auth/types.ts:125-130`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthPrompt {
    Text {
        message: String,
        placeholder: Option<String>,
    },
    Secret {
        message: String,
        placeholder: Option<String>,
    },
    Select {
        message: String,
        options: Vec<AuthPromptOption>,
    },
    ManualCode {
        message: String,
        placeholder: Option<String>,
    },
}

/// `AuthInfoLink` (`auth/types.ts:132-135`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthInfoLink {
    pub url: String,
    pub label: Option<String>,
}

/// `AuthEvent` (`auth/types.ts:137-151`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthEvent {
    Info {
        message: String,
        links: Vec<AuthInfoLink>,
    },
    AuthUrl {
        url: String,
        instructions: Option<String>,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
        interval_seconds: Option<u64>,
        expires_in_seconds: Option<u64>,
    },
    Progress {
        message: String,
    },
}

/// What the driving host sees from an [`Interaction`]. Prompts are answered
/// by id with [`Interaction::answer`]; `PromptClosed` also fires when the
/// flow itself resolves a prompt (the callback beat a manual-code prompt).
#[derive(Debug, Clone, PartialEq)]
pub enum InteractionMessage {
    Notify(AuthEvent),
    Prompt { id: u64, prompt: AuthPrompt },
    PromptClosed { id: u64 },
}

struct PromptGuard<'a> {
    messages: &'a mpsc::Sender<InteractionMessage>,
    id: u64,
}

impl Drop for PromptGuard<'_> {
    fn drop(&mut self) {
        let _ = self
            .messages
            .send(InteractionMessage::PromptClosed { id: self.id });
    }
}

/// Cooperative cancellation (`AbortSignal` in `auth/types.ts:119-168`).
#[derive(Debug, Clone)]
pub struct CancelToken {
    tx: watch::Sender<bool>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    #[must_use]
    pub fn new() -> Self {
        Self {
            tx: watch::Sender::new(false),
        }
    }

    /// `signal.aborted`.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    /// Flip the signal; later `cancelled()` callers observe it too.
    ///
    /// `watch::Sender::send` drops the value when no receiver is subscribed
    /// yet, so use `send_replace` to keep cancellation sticky.
    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }

    /// Wait until cancellation (`AbortSignal`'s `abort` event).
    pub async fn cancelled(&self) {
        let mut receiver = self.tx.subscribe();
        if *receiver.borrow_and_update() {
            return;
        }
        while receiver.changed().await.is_ok() {
            if *receiver.borrow_and_update() {
                return;
            }
        }
    }
}

/// One pending prompt's answer channel (factored out to keep
/// [`Interaction`]'s field type readable).
type AnswerSender = oneshot::Sender<Result<String, AuthError>>;

/// The host side of `ProviderAuthInteraction` (`auth/types.ts:153-168`): the
/// flow sends prompts/notifications out and awaits answers over oneshots.
pub struct Interaction {
    messages: mpsc::Sender<InteractionMessage>,
    answers: Arc<Mutex<BTreeMap<u64, AnswerSender>>>,
    cancel: CancelToken,
    next_id: AtomicU64,
}

impl Interaction {
    /// Create an interaction and the receiving end the host drains.
    #[must_use]
    pub fn new() -> (Self, mpsc::Receiver<InteractionMessage>) {
        let (messages, receiver) = mpsc::channel();
        (
            Self {
                messages,
                answers: Arc::new(Mutex::new(BTreeMap::new())),
                cancel: CancelToken::new(),
                next_id: AtomicU64::new(1),
            },
            receiver,
        )
    }

    /// The flow-wide signal; hosts may call [`CancelToken::cancel`].
    #[must_use]
    pub fn cancellation(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// `interaction.notify(event)`.
    pub fn notify(&self, event: AuthEvent) {
        let _ = self.messages.send(InteractionMessage::Notify(event));
    }

    /// `interaction.prompt(prompt)` with the flow-wide signal.
    ///
    /// # Errors
    /// [`AuthError::Cancelled`] on cancellation, [`AuthError::PromptUnavailable`]
    /// when the host dropped the answer side.
    pub async fn prompt(&self, prompt: AuthPrompt) -> Result<String, AuthError> {
        let extra = CancelToken::new();
        self.prompt_with(&prompt, &extra).await
    }

    /// A prompt with an additional per-prompt signal — upstream's
    /// `AuthPrompt.signal` used by `waitForCallbackOrManualInput`
    /// (`callback-server.ts:160-172`).
    ///
    /// # Errors
    /// Same as [`Self::prompt`].
    pub async fn prompt_with(
        &self,
        prompt: &AuthPrompt,
        extra_cancel: &CancelToken,
    ) -> Result<String, AuthError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.answers
            .lock()
            .expect("oauth answers mutex poisoned")
            .insert(id, sender);
        let _ = self.messages.send(InteractionMessage::Prompt {
            id,
            prompt: prompt.clone(),
        });
        // The guard also closes the host's mounted prompt when the flow
        // itself abandons it (the callback beat a manual-code prompt).
        let _guard = PromptGuard {
            messages: &self.messages,
            id,
        };
        tokio::select! {
            answer = receiver => answer.unwrap_or(Err(AuthError::PromptUnavailable)),
            () = self.cancel.cancelled() => Err(AuthError::Cancelled),
            () = extra_cancel.cancelled() => Err(AuthError::Cancelled),
        }
    }

    /// Resolve a pending prompt (`showPrompt`'s resolver/rejecter).
    pub fn answer(&self, id: u64, answer: Result<String, AuthError>) -> bool {
        let sender = self
            .answers
            .lock()
            .expect("oauth answers mutex poisoned")
            .remove(&id);
        sender.is_some_and(|sender| sender.send(answer).is_ok())
    }

    /// Abort the whole flow and reject every open prompt with
    /// `Login cancelled` (`interaction.signal.abort()`).
    pub fn cancel(&self) {
        self.cancel.cancel();
        // BTreeMap has no stable drain; take the whole map and reject the
        // senders it held.
        let answers =
            std::mem::take(&mut *self.answers.lock().expect("oauth answers mutex poisoned"));
        for (_, sender) in answers {
            let _ = sender.send(Err(AuthError::Cancelled));
        }
    }
}

/// Per-provider URLs and callback binding. `Default` is the verbatim
/// upstream table; tests override individual fields with loopback URLs.
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    pub authorize_url: String,
    pub token_url: String,
    pub device_user_code_url: String,
    pub device_token_url: String,
    pub verification_uri: String,
    pub device_redirect_uri: String,
    pub callback_host: String,
    pub callback_port: u16,
    pub callback_path: String,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            authorize_url: String::new(),
            token_url: String::new(),
            device_user_code_url: String::new(),
            device_token_url: String::new(),
            verification_uri: String::new(),
            device_redirect_uri: String::new(),
            callback_host: "127.0.0.1".to_string(),
            callback_port: 0,
            callback_path: "/callback".to_string(),
        }
    }
}

/// The flow's injectable HTTP client and endpoint table.
#[derive(Debug, Clone)]
pub struct FlowConfig {
    pub client: reqwest::Client,
    pub anthropic: EndpointConfig,
    pub openai_codex: EndpointConfig,
    pub github_copilot: EndpointConfig,
}

impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
            anthropic: EndpointConfig {
                authorize_url: "https://claude.ai/oauth/authorize".to_string(),
                token_url: "https://platform.claude.com/v1/oauth/token".to_string(),
                callback_port: 53692,
                callback_path: "/callback".to_string(),
                ..EndpointConfig::default()
            },
            openai_codex: EndpointConfig {
                authorize_url: "https://auth.openai.com/oauth/authorize".to_string(),
                token_url: "https://auth.openai.com/oauth/token".to_string(),
                device_user_code_url: "https://auth.openai.com/api/accounts/deviceauth/usercode"
                    .to_string(),
                device_token_url: "https://auth.openai.com/api/accounts/deviceauth/token"
                    .to_string(),
                verification_uri: "https://auth.openai.com/codex/device".to_string(),
                device_redirect_uri: "https://auth.openai.com/deviceauth/callback".to_string(),
                callback_port: 1455,
                callback_path: "/auth/callback".to_string(),
                ..EndpointConfig::default()
            },
            github_copilot: EndpointConfig {
                device_user_code_url: "https://github.com/login/device/code".to_string(),
                device_token_url: "https://github.com/login/oauth/access_token".to_string(),
                token_url: "https://api.githubcopilot.com/copilot_internal/v2/token".to_string(),
                ..EndpointConfig::default()
            },
        }
    }
}

/// The provider ids whose built-in flows are ported
/// (`auth/oauth/{anthropic,openai-codex,github-copilot}.ts`).
#[must_use]
pub fn is_builtin_oauth_provider(provider_id: &str) -> bool {
    matches!(provider_id, "anthropic" | "openai-codex" | "github-copilot")
}

/// `OAuthAuth.login` for a built-in provider.
///
/// # Errors
/// The upstream flow errors: `Login cancelled`, state mismatches, missing
/// codes, and the provider-specific HTTP/JSON failures.
pub async fn login(
    provider_id: &str,
    interaction: &Interaction,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    match provider_id {
        "anthropic" => anthropic::login(interaction, config).await,
        "openai-codex" => openai_codex::login(interaction, config).await,
        "github-copilot" => github_copilot::login(interaction, config).await,
        other => Err(AuthError::Message(format!(
            "Unknown OAuth provider: {other}"
        ))),
    }
}

/// `OAuthAuth.refresh` for a built-in provider. `refresh` is the stored
/// refresh token; `extra` carries `enterpriseUrl` for Copilot.
///
/// # Errors
/// Provider-specific refresh failures (`invalid_grant`, HTTP/JSON errors).
pub async fn refresh(
    provider_id: &str,
    refresh_token: &str,
    extra: &BTreeMap<String, Value>,
    cancel: &CancelToken,
    config: &FlowConfig,
) -> Result<OAuthCredential, AuthError> {
    match provider_id {
        "anthropic" => anthropic::refresh(refresh_token, cancel, config).await,
        "openai-codex" => openai_codex::refresh(refresh_token, cancel, config).await,
        "github-copilot" => {
            let domain = extra
                .get("enterpriseUrl")
                .and_then(Value::as_str)
                .and_then(http::normalize_domain);
            github_copilot::refresh(refresh_token, domain.as_deref(), cancel, config).await
        }
        other => Err(AuthError::Message(format!(
            "Unknown OAuth provider: {other}"
        ))),
    }
}

/// `OAuthAuth.toAuth` (`anthropic.ts:304-306`, `openai-codex.ts:435-437`,
/// `github-copilot.ts:494-500`): API key from the access token, plus the
/// Copilot proxy base URL derived from it.
#[must_use]
pub fn to_auth(provider_id: &str, credential: &OAuthCredential) -> (String, Option<String>) {
    let base_url = if provider_id == "github-copilot" {
        github_copilot::base_url_from_token(&credential.access, None)
    } else {
        None
    };
    (credential.access.clone(), base_url)
}

/// `Date.now()` in milliseconds — injected here so tests can assert expiry
/// arithmetic with a bounded clock read.
#[must_use]
pub fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_serializes_with_the_type_discriminant() {
        let credential = OAuthCredential {
            access: "a".to_string(),
            refresh: "r".to_string(),
            expires: 42,
            account_id: Some("acct".to_string()),
            enterprise_url: None,
            available_model_ids: vec!["gpt-5".to_string()],
        };
        assert_eq!(
            credential.to_value(),
            json!({
                "type": "oauth",
                "access": "a",
                "refresh": "r",
                "expires": 42,
                "accountId": "acct",
                "availableModelIds": ["gpt-5"]
            })
        );
    }

    #[test]
    fn builtin_provider_dispatch_is_exact() {
        assert!(is_builtin_oauth_provider("anthropic"));
        assert!(is_builtin_oauth_provider("openai-codex"));
        assert!(is_builtin_oauth_provider("github-copilot"));
        assert!(!is_builtin_oauth_provider("radius"));
        assert!(!is_builtin_oauth_provider("openrouter"));
    }

    #[test]
    fn to_auth_derives_the_api_key_and_copilot_base_url() {
        let credential = OAuthCredential {
            access: "tok".to_string(),
            refresh: "r".to_string(),
            expires: 0,
            account_id: None,
            enterprise_url: None,
            available_model_ids: Vec::new(),
        };
        assert_eq!(to_auth("anthropic", &credential), ("tok".to_string(), None));
        assert_eq!(
            to_auth("github-copilot", &credential),
            (
                "tok".to_string(),
                Some("https://api.individual.githubcopilot.com".to_string())
            )
        );
    }

    #[tokio::test]
    async fn cancel_token_is_sticky_and_notifies_late_waiters() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
        token.cancelled().await;
        token.cancelled().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interaction_prompt_answers_by_id() {
        let (interaction, receiver) = Interaction::new();
        let interaction = Arc::new(interaction);
        let task = {
            let interaction = Arc::clone(&interaction);
            tokio::spawn(async move {
                interaction
                    .prompt(AuthPrompt::Text {
                        message: "value?".to_string(),
                        placeholder: None,
                    })
                    .await
            })
        };
        let message = receiver.recv().expect("prompt message");
        let InteractionMessage::Prompt { id, prompt } = message else {
            panic!("expected a prompt, got {message:?}");
        };
        assert_eq!(
            prompt,
            AuthPrompt::Text {
                message: "value?".to_string(),
                placeholder: None,
            }
        );
        assert!(interaction.answer(id, Ok("answer".to_string())));
        assert_eq!(task.await.expect("prompt task"), Ok("answer".to_string()));
        assert_eq!(
            receiver.recv().expect("closed message"),
            InteractionMessage::PromptClosed { id }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interaction_cancel_rejects_open_prompts() {
        let (interaction, receiver) = Interaction::new();
        let interaction = Arc::new(interaction);
        let task = {
            let interaction = Arc::clone(&interaction);
            tokio::spawn(async move {
                interaction
                    .prompt(AuthPrompt::ManualCode {
                        message: "code?".to_string(),
                        placeholder: None,
                    })
                    .await
            })
        };
        let InteractionMessage::Prompt { id, .. } = receiver.recv().expect("prompt") else {
            panic!("expected a prompt");
        };
        interaction.cancel();
        assert_eq!(task.await.expect("prompt task"), Err(AuthError::Cancelled));
        let _ = id;
    }
}
