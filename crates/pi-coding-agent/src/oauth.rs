//! OAuth and provider-auth composition (E2 slice 6c).
//!
//! Ground truth:
//! - `core/provider-composer.ts` — `adaptOAuth` (`:358-377`) maps an
//!   extension's `ExtensionOAuthConfig` onto the `OAuthAuth` shape;
//!   `composeOAuthAuth` (`:488-511`) picks the provider's OAuth method
//!   (extension oauth, else the base provider's); `composeApiKeyAuth`
//!   (`:422-486`) merges the inherited API-key auth with the configured
//!   `apiKey`, and OAuth-only providers get no fabricated API-key method
//!   (`:432`); the composed provider name is `extension?.name ?? config?.name
//!   ?? base?.name ?? providerId` (`:598-600`); `AuthStatus` sources
//!   (`:112-116`) and `configuredRequestAuthStatus` (`:707-721`).
//! - `core/model-runtime.ts` — `configureRadiusProviders` (`:274-287`)
//!   registers models.json providers with `oauth: "radius"` as Radius-gateway
//!   providers (`radiusProvider({id, name: config.name ?? providerId,
//!   gateway})`, so they inherit the Radius auth pair); `getProviderAuthStatus`
//!   (`:655-665`) and `isUsingOAuth` (`:544-546`).
//! - `auth/resolve.ts` — `resolveProviderAuth` (`:33-93`): a stored
//!   credential owns the provider; a credential whose type the provider does
//!   not support resolves to nothing with **no** env fallback, and ambient
//!   env vars are consulted only when nothing is stored.
//! - `auth/helpers.ts` — `envApiKeyAuth` (`:8-33`) is the standard
//!   stored-key-then-env-var API-key auth with a secret-prompt login;
//!   `lazyOAuth` (`:36-56`) is the OAuthAuth metadata carrier.
//! - `auth/types.ts` — `OAuthAuth` (`:225-249`), `AuthCheck` (`:112-115`).
//! - `core/preferred-providers.ts` — `PREFERRED_PROVIDER_IDS`/
//!   `compareProviders` (`:10-27`).
//! - `modes/interactive/interactive-mode.ts` — `getLoginProviderOptions`
//!   (`:6002-6036`), `getLogoutProviderOptions` (`:6038-6050`),
//!   `getLoginProviderCompletionOptions` + search/description formatters
//!   (`:393-433`); `modes/interactive/components/oauth-selector.ts` —
//!   `AuthSelectorProvider` (`:10-20`), `formatAuthSelectorProviderType`
//!   (`:23-27`), `formatAuthSelectorProviderStatus` (`:30-49`).
//!
//! The per-provider OAuth *flows* (`packages/ai/src/auth/oauth/*.ts` —
//! PKCE/device-code/callback-server implementations for anthropic, openai,
//! openai-codex, github-copilot, kimi-coding, meta, xai, openrouter, radius)
//! are network protocol machines that live in pi-ai upstream; this port does
//! not have them yet, so an OAuth login attempt reports the upstream error
//! shape with an honest reason. What this module ports is the composition
//! surface: which auth methods a provider exposes, their display metadata,
//! the login/logout option lists, and the request-time resolution rules the
//! wire ladder applies
//! ([`crate::modes::print_mode::resolve_wire_credential`]).
//!
//! Deviations: provider display names are `extension name ?? models.json
//! name ?? provider id` — the port's embedded catalog carries no built-in
//! provider names, so the `base?.name` layer is the provider id. The login
//! option list derives its providers from the composed model catalog
//! ([`crate::models_state::providers`]), which is model-derived — a
//! models.json provider that only configures `apiKey` (zero models) does not
//! appear, where upstream's `getProviders()` lists it. Upstream's RPC mode
//! has no auth dialogs at all (`modes/rpc/rpc-mode.ts` exposes only
//! `extension_ui_request` select/input methods and no login/logout commands),
//! so nothing rides the RPC dialog subprotocol here.

use crate::auth::read_stored_credential;
use crate::models_config::{ExtensionProviderConfig, ModelsJsonProvider};
use std::path::Path;

/// `RADIUS_PROVIDER_ID` (`core/radius.ts:3`).
pub const RADIUS_PROVIDER_ID: &str = "radius";

/// Providers whose API-key auth is configured outside the app — their custom
/// login flows (the AWS auth-method chooser `amazon-bedrock.ts:13`, the
/// Google Cloud method select `google-vertex.ts:15`, the Cloudflare account
/// config) are not ported, so `/login` reports the upstream ambient-dialog
/// message (`showAmbientAuthDialog`, `interactive-mode.ts:6367-6396`).
pub const AMBIENT_LOGIN_PROVIDERS: [&str; 4] = [
    "amazon-bedrock",
    "google-vertex",
    "cloudflare-workers-ai",
    "cloudflare-ai-gateway",
];

/// `OAuthAuth` metadata (`auth/types.ts:225-249`): the display name, whether
/// access is subscription-backed, and the optional selector label. The
/// `login`/`refresh`/`toAuth` functions are the per-provider flows in pi-ai
/// (`auth/oauth/*.ts`) and are not part of this port yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthAuthInfo {
    /// Display name, e.g. "Anthropic (Claude Pro/Max)".
    pub name: String,
    /// Whether access through this auth method is backed by a provider
    /// subscription (`isSubscription`).
    pub is_subscription: bool,
    /// Selector label for the OAuth login option (`loginLabel`).
    pub login_label: Option<&'static str>,
}

/// `ApiKeyAuth` metadata (`auth/types.ts:174-179`): the display name used in
/// the login prompt ("Enter {name}") and the ambient-auth dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyAuthInfo {
    /// Display name, e.g. "Anthropic API key".
    pub name: &'static str,
}

/// One composed auth method of a provider (`ProviderAuth`,
/// `auth/types.ts:256-259`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMethod {
    /// The provider's API-key auth (`auth.apiKey`).
    ApiKey(ApiKeyAuthInfo),
    /// The provider's OAuth auth (`auth.oauth`).
    OAuth(OAuthAuthInfo),
}

impl AuthMethod {
    /// The `AuthType` discriminant (`auth/types.ts:117`).
    pub fn auth_type(&self) -> &'static str {
        match self {
            AuthMethod::ApiKey(..) => "api_key",
            AuthMethod::OAuth(..) => "oauth",
        }
    }
}

/// `PREFERRED_PROVIDER_IDS` (`core/preferred-providers.ts:12`): most
/// preferred first.
const PREFERRED_PROVIDER_IDS: [&str; 2] = [RADIUS_PROVIDER_ID, "openrouter"];

/// `isPreferredProvider` (`core/preferred-providers.ts:15-17`).
pub fn is_preferred_provider(provider_id: &str) -> bool {
    PREFERRED_PROVIDER_IDS.contains(&provider_id)
}

/// `preferredRank` (`core/preferred-providers.ts:25-27`).
fn preferred_rank(provider_id: &str) -> usize {
    PREFERRED_PROVIDER_IDS
        .iter()
        .position(|id| *id == provider_id)
        .unwrap_or(PREFERRED_PROVIDER_IDS.len())
}

/// `compareProviders` (`core/preferred-providers.ts:17-23`): preferred
/// partners first, in their order, then alphabetically by name —
/// `name.localeCompare`'s case-insensitive collation, so byte order (upper
/// before lower) does not leak through; ties fall to a byte comparison.
pub fn compare_providers(a: (&str, &str), b: (&str, &str)) -> std::cmp::Ordering {
    let (a_id, a_name) = a;
    let (b_id, b_name) = b;
    let primary = (preferred_rank(a_id), a_name.to_lowercase());
    let secondary = (preferred_rank(b_id), b_name.to_lowercase());
    match primary.cmp(&secondary) {
        std::cmp::Ordering::Equal => a_name.cmp(b_name),
        ord => ord,
    }
}

/// The standard `envApiKeyAuth` display names (`auth/helpers.ts:8`), per
/// provider id. These providers' API-key auth carries a secret-prompt
/// `login`, so `/login` can drive it. Sources: the `providers/*.ts` factory
/// cited per entry.
pub fn api_key_auth_name(provider_id: &str) -> Option<&'static str> {
    match provider_id {
        "anthropic" => Some("Anthropic API key"), // anthropic.ts:21
        "deepseek" => Some("DeepSeek API key"),   // deepseek.ts:11
        "groq" => Some("Groq API key"),           // groq.ts:11
        "qwen-token-plan" => Some("Qwen Token Plan Team API key"), // qwen-token-plan.ts:12
        "qwen-token-plan-cn" => Some("Qwen Token Plan Team CN API key"), // qwen-token-plan-cn.ts:12
        "qwen-token-plan-individual" => Some("Qwen Token Plan Individual API key"), // qwen-token-plan-individual.ts:12
        "qwen-token-plan-individual-cn" => Some("Qwen Token Plan Individual CN API key"), // qwen-token-plan-individual-cn.ts:12
        "cerebras" => Some("Cerebras API key"), // cerebras.ts:11
        "google" => Some("Gemini API key"),     // google.ts:11
        "huggingface" => Some("Hugging Face token"), // huggingface.ts:11
        "github-copilot" => Some("GitHub Copilot token"), // github-copilot.ts:30
        "xai" => Some("xAI API key"),           // xai.ts:13
        "vercel-ai-gateway" => Some("Vercel AI Gateway API key"), // vercel-ai-gateway.ts:12
        "kimi-coding" => Some("Kimi API key"),  // kimi-coding.ts:13
        "moonshotai" | "moonshotai-cn" => Some("Moonshot AI API key"), // moonshotai.ts:11
        "ant-ling" => Some("Ant Ling API key"), // ant-ling.ts:11
        "xiaomi" => Some("Xiaomi API key"),     // xiaomi.ts:11
        "meta" => Some("Meta Model API key"),   // meta.ts:13
        "fireworks" => Some("Fireworks API key"), // fireworks.ts:12
        "mistral" => Some("Mistral API key"),   // mistral.ts:11
        "opencode" | "opencode-go" => Some("OpenCode API key"), // opencode.ts:17
        "openai" => Some("OpenAI API key"),     // openai.ts:15
        "azure" => Some("Azure OpenAI API key"), // azure.ts:48
        "minimax" => Some("MiniMax API key"),   // minimax.ts:11
        "minimax-cn" => Some("MiniMax CN API key"), // minimax-cn.ts:11
        "baseten" => Some("Baseten API key"),   // baseten.ts:11
        "together" => Some("Together API key"), // together.ts:11
        "nvidia" => Some("NVIDIA API key"),     // nvidia.ts:11
        "typesafe" => Some("TypeSafe API key"), // typesafe.ts:11
        "zai" => Some("Z.AI API key"),          // zai.ts:11
        "zai-coding-cn" => Some("Z.AI Coding CN API key"), // zai-coding-cn.ts:11
        "openrouter" => Some("OpenRouter API key"), // openrouter.ts:68
        "radius" => Some("Radius API key"),     // radius.ts:39
        _ => None,
    }
}

/// The built-in providers advertising OAuth (`lazyOAuth` carriers;
/// `providers/*.ts`), with the verbatim `OAuthAuth` metadata. `radius`'s
/// OAuth comes from the radius-style branch of [`provider_auth_methods`] —
/// its name is the provider's display name (`auth/oauth/radius.ts:283-285`,
/// `createRadiusOAuth({name})`).
fn builtin_oauth(provider_id: &str) -> Option<OAuthAuthInfo> {
    let (name, is_subscription, login_label) = match provider_id {
        // anthropic.ts:83-85
        "anthropic" => ("Anthropic (Claude Pro/Max)", true, None),
        // github-copilot.ts:31
        "github-copilot" => ("GitHub Copilot", true, None),
        // kimi-coding.ts:14-16
        "kimi-coding" => ("Kimi Code (subscription)", true, None),
        // meta.ts:14-16
        "meta" => ("Meta (Muse subscription)", true, None),
        // openai.ts:16-18
        "openai" => ("OpenAI (ChatGPT subscription)", true, None),
        // xai.ts:14-16
        "xai" => ("xAI (Grok/X subscription)", true, None),
        // openrouter.ts:69-73
        "openrouter" => ("OpenRouter OAuth", false, Some("Sign in with OpenRouter")),
        // openai-codex.ts:13-16
        "openai-codex" => ("OpenAI (ChatGPT Plus/Pro)", true, None),
        _ => return None,
    };
    Some(OAuthAuthInfo {
        name: name.to_string(),
        is_subscription,
        login_label,
    })
}

/// The composed provider name (`provider-composer.ts:598-600`):
/// `extension?.name ?? config?.name ?? base?.name ?? providerId`. The port's
/// embedded catalog has no built-in provider names, so the base layer falls
/// to the id (module doc, deviations).
pub fn provider_display_name(
    provider_id: &str,
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ExtensionProviderConfig>,
) -> String {
    extension
        .and_then(|extension| extension.name.clone())
        .or_else(|| config.and_then(|config| config.name.clone()))
        .unwrap_or_else(|| provider_id.to_string())
}

/// A models.json provider entry with `oauth: "radius"` (the only accepted
/// value; `model-config.ts:108`).
fn is_radius_oauth(config: &ModelsJsonProvider) -> bool {
    config.oauth.as_deref() == Some("radius")
}

/// The composed auth methods of one provider — the outcome of
/// `composeApiKeyAuth` (`provider-composer.ts:422-486`) +
/// `composeOAuthAuth` (`:488-511`) for the metadata the port carries.
///
/// OAuth: an extension oauth config (`adaptOAuth`, `:358-377`) wins, then the
/// built-in provider's OAuth, then a models.json `oauth: "radius"` entry,
/// which `configureRadiusProviders` (`model-runtime.ts:274-287`) turns into a
/// Radius-gateway provider inheriting the Radius auth pair. API key: the
/// built-in `envApiKeyAuth` (inherited), else the configured `apiKey`
/// (`extension?.apiKey ?? config?.apiKey`); an OAuth-only provider gets no
/// fabricated API-key method (`:432`). The order matches
/// `getLoginProviderOptions` (`interactive-mode.ts:6002-6036`): the OAuth
/// entry before the API-key entry.
pub fn provider_auth_methods(
    provider_id: &str,
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ExtensionProviderConfig>,
) -> Vec<AuthMethod> {
    let provider_name = provider_display_name(provider_id, config, extension);
    let radius_style = provider_id == RADIUS_PROVIDER_ID || config.is_some_and(is_radius_oauth);
    let oauth = match extension.and_then(|extension| extension.oauth.as_ref()) {
        // `adaptOAuth` (`:358-377`): the extension's oauth metadata rides as
        // the OAuthAuth; its login/refresh/toAuth are JS functions the JSON
        // bridge drops, so only the metadata is carried.
        Some(extension_oauth) => Some(OAuthAuthInfo {
            name: extension_oauth.name.clone(),
            is_subscription: extension_oauth.is_subscription,
            login_label: None,
        }),
        None if radius_style => {
            // A models.json `oauth: "radius"` entry becomes a Radius-gateway
            // provider (`model-runtime.ts:274-287`); the OAuth name is the
            // provider's display name and `isSubscription` stays unset.
            Some(OAuthAuthInfo {
                name: provider_name,
                is_subscription: false,
                login_label: None,
            })
        }
        None => builtin_oauth(provider_id),
    };

    // `configuredApiKey` (`:393-398`).
    let raw_key = extension
        .and_then(|extension| extension.api_key.as_deref())
        .or_else(|| config.and_then(|config| config.api_key.as_deref()));
    // `composeApiKeyAuth` (`:428-432`): the inherited method (the built-in
    // envApiKeyAuth; a models.json radius provider inherits the Radius one)
    // or a configured key; OAuth-only providers get no fabricated method
    // (`:432`).
    let inherited = if radius_style {
        api_key_auth_name(RADIUS_PROVIDER_ID)
    } else {
        api_key_auth_name(provider_id)
    };
    let api_key = inherited
        .or_else(|| raw_key.map(|_| "API key"))
        .map(|name| AuthMethod::ApiKey(ApiKeyAuthInfo { name }));

    let mut methods = Vec::new();
    if let Some(oauth) = oauth {
        methods.push(AuthMethod::OAuth(oauth));
    }
    if let Some(api_key) = api_key {
        methods.push(api_key);
    }
    methods
}

/// `AuthStatus` (`provider-composer.ts:112-116`) with the `AuthCheck` type
/// (`auth/types.ts:112-115`): whether a provider is configured and where the
/// credential comes from. `label ?? source` is the selector row's source
/// (`interactive-mode.ts:6008`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthStatus {
    /// `getProviderAuthStatus`'s `configured`.
    pub configured: bool,
    /// The resolved auth type (`isUsingOAuth` check): the stored credential's
    /// type, or `"api_key"` for configured keys and environment variables.
    pub auth_type: &'static str,
    /// `stored` | `environment` | `fallback` | `models_json_key` |
    /// `models_json_command`.
    pub source: Option<&'static str>,
    /// The env-var names for `environment`, or the resolved source label.
    pub label: Option<String>,
}

/// `getProviderAuthStatus` (`model-runtime.ts:655-665`) +
/// `configuredRequestAuthStatus` (`provider-composer.ts:707-721`): a stored
/// credential wins, then the configured models.json/extension key, then the
/// provider environment variables (`envApiKeyAuth.resolve`'s loop). `None`
/// renders as "not configured".
pub fn provider_auth_status(
    provider_id: &str,
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ExtensionProviderConfig>,
    auth_path: &Path,
) -> Option<AuthStatus> {
    // The stored credential owns the provider (`resolve.ts:70-87`); the
    // status reports its type (`isUsingOAuth`, `model-runtime.ts:544-546`).
    if let Some(credential) = read_stored_credential(provider_id, auth_path) {
        return Some(AuthStatus {
            configured: true,
            auth_type: match credential {
                crate::auth::Credential::Oauth { .. } => "oauth",
                crate::auth::Credential::ApiKey { .. } => "api_key",
            },
            source: Some("stored"),
            label: None,
        });
    }
    // `configuredRequestAuthStatus` (`:707-721`).
    let raw_key = extension
        .and_then(|extension| extension.api_key.as_deref())
        .or_else(|| config.and_then(|config| config.api_key.as_deref()));
    if let Some(raw_key) = raw_key {
        return Some(configured_key_status(raw_key, extension));
    }
    // The ambient env vars (`envApiKeyAuth.resolve`'s loop,
    // `helpers.ts:21-29`): the first set variable is the source label.
    for variable in crate::modes::print_mode::env_key_candidates(provider_id) {
        if let Ok(value) = std::env::var(variable) {
            if !value.is_empty() {
                return Some(AuthStatus {
                    configured: true,
                    auth_type: "api_key",
                    source: Some("environment"),
                    label: Some((*variable).to_string()),
                });
            }
        }
    }
    None
}

/// `configuredRequestAuthStatus` (`provider-composer.ts:707-721`).
fn configured_key_status(raw_key: &str, extension: Option<&ExtensionProviderConfig>) -> AuthStatus {
    if raw_key.starts_with('!') {
        // `isCommandConfigValue`.
        return AuthStatus {
            configured: true,
            auth_type: "api_key",
            source: Some("models_json_command"),
            label: None,
        };
    }
    let names = crate::auth::get_config_value_env_var_names(raw_key);
    if !names.is_empty() {
        let missing = crate::auth::get_missing_config_value_env_var_names(raw_key, None);
        let configured = missing.is_empty();
        return AuthStatus {
            configured,
            auth_type: "api_key",
            source: configured.then_some("environment"),
            label: configured.then(|| names.join(", ")),
        };
    }
    // The literal-key form: an extension key reports `fallback`, a
    // models.json key reports `models_json_key` (`:719-721`).
    AuthStatus {
        configured: true,
        auth_type: "api_key",
        source: Some(
            if extension.is_some_and(|extension| extension.api_key.is_some()) {
                "fallback"
            } else {
                "models_json_key"
            },
        ),
        label: None,
    }
}

/// `AuthSelectorProvider` (`oauth-selector.ts:10-20`): one row of the
/// login/logout provider selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSelectorProvider {
    pub id: String,
    pub name: String,
    /// `"oauth" | "api_key"`.
    pub auth_type: &'static str,
    /// Whether the provider's OAuth sign-in is subscription-backed
    /// (`provider.auth.oauth?.isSubscription === true`); false labels it as
    /// an "account".
    pub subscription: bool,
    /// The row's auth method display name (`method?.name` in the search
    /// text, `oauth-selector.ts:142`).
    pub method_name: Option<String>,
    /// The OAuth selector label (`OAuthAuth.loginLabel`) — the
    /// auth-type menu's subscription option label when set.
    pub login_label: Option<&'static str>,
    /// The provider's composed status, formatted by
    /// [`format_auth_selector_provider_status`].
    pub status: Option<AuthStatus>,
}

/// `getLoginProviderOptions` (`interactive-mode.ts:6002-6036`): every
/// composed provider contributes an OAuth entry when it has OAuth auth and an
/// API-key entry when it has API-key auth; `compareProviders` sorts (stable,
/// so a provider's account option stays above its API-key option).
pub fn login_provider_options(auth_path: &Path) -> Vec<AuthSelectorProvider> {
    let mut options = Vec::new();
    for provider_id in crate::models_state::providers() {
        let config = crate::models_state::models_json_provider(&provider_id);
        let extension = crate::models_state::extension_provider_config(&provider_id);
        let name = provider_display_name(&provider_id, config.as_ref(), extension.as_ref());
        let methods = provider_auth_methods(&provider_id, config.as_ref(), extension.as_ref());
        let subscription = methods.first().is_some_and(
            |method| matches!(method, AuthMethod::OAuth(oauth) if oauth.is_subscription),
        );
        let status =
            provider_auth_status(&provider_id, config.as_ref(), extension.as_ref(), auth_path);
        for method in &methods {
            let (method_name, login_label) = match method {
                AuthMethod::OAuth(oauth) => (oauth.name.clone(), oauth.login_label),
                AuthMethod::ApiKey(info) => (info.name.to_string(), None),
            };
            options.push(AuthSelectorProvider {
                id: provider_id.clone(),
                name: name.clone(),
                auth_type: method.auth_type(),
                subscription,
                method_name: Some(method_name),
                login_label,
                status: status.clone(),
            });
        }
    }
    options.sort_by(|a, b| compare_providers((&a.id, &a.name), (&b.id, &b.name)));
    options
}

/// `getLogoutProviderOptions` (`interactive-mode.ts:6038-6040`): the stored
/// credentials, one row each, `compareProviders`-sorted. Storage failures
/// propagate (`showOAuthSelector` reports them).
pub fn logout_provider_options(
    auth_path: &Path,
) -> Result<Vec<AuthSelectorProvider>, crate::auth::AuthError> {
    let storage = crate::auth::AuthStorage::create(auth_path);
    let listed = storage.list()?;
    let mut options: Vec<AuthSelectorProvider> = listed
        .into_iter()
        .map(|info| {
            let config = crate::models_state::models_json_provider(&info.provider_id);
            let extension = crate::models_state::extension_provider_config(&info.provider_id);
            let name =
                provider_display_name(&info.provider_id, config.as_ref(), extension.as_ref());
            let subscription = provider_auth_methods(
                &info.provider_id,
                config.as_ref(),
                extension.as_ref(),
            )
            .first()
            .is_some_and(
                |method| matches!(method, AuthMethod::OAuth(oauth) if oauth.is_subscription),
            );
            AuthSelectorProvider {
                id: info.provider_id.clone(),
                name,
                auth_type: if info.credential_type == "oauth" {
                    "oauth"
                } else {
                    "api_key"
                },
                subscription,
                method_name: None,
                login_label: None,
                status: Some(AuthStatus {
                    configured: true,
                    auth_type: if info.credential_type == "oauth" {
                        "oauth"
                    } else {
                        "api_key"
                    },
                    source: Some("stored"),
                    label: None,
                }),
            }
        })
        .collect();
    options.sort_by(|a, b| compare_providers((&a.id, &a.name), (&b.id, &b.name)));
    Ok(options)
}

/// `findMatchingProviders` (`core/provider-matching.ts:5-11`): exact id or
/// display-name match, case- and whitespace-insensitive.
pub fn find_matching_providers<'a>(
    providers: &'a [AuthSelectorProvider],
    provider_ref: &str,
) -> Vec<&'a AuthSelectorProvider> {
    let normalized = provider_ref.trim().to_lowercase();
    if normalized.is_empty() {
        return Vec::new();
    }
    providers
        .iter()
        .filter(|provider| {
            provider.id.to_lowercase() == normalized || provider.name.to_lowercase() == normalized
        })
        .collect()
}

/// `formatAuthSelectorProviderType` (`oauth-selector.ts:23-27`).
pub fn format_auth_selector_provider_type(auth_type: &str, subscription: bool) -> &'static str {
    if auth_type == "api_key" {
        return "API key";
    }
    if subscription {
        "subscription"
    } else {
        "account"
    }
}

/// `formatAuthSelectorProviderStatus` (`oauth-selector.ts:30-49`): the themed
/// suffix describing whether and how a login option is configured. The row's
/// source is `status.label ?? status.source` (`interactive-mode.ts:6008`).
pub fn format_auth_selector_provider_status(
    provider: &AuthSelectorProvider,
    status: &AuthStatus,
) -> String {
    use pi_tui::theme::ThemeColor;
    let theme = crate::modes::interactive::theme::theme();
    let muted = |text: &str| theme.fg(ThemeColor::Muted, text);
    let success = |text: &str| theme.fg(ThemeColor::Success, text);
    let warning = |text: &str| theme.fg(ThemeColor::Warning, text);
    if !status.configured {
        return muted(" • not configured");
    }
    if status.auth_type != provider.auth_type {
        let label = format_auth_selector_provider_type(status.auth_type, provider.subscription);
        return format!(
            "{}{}",
            muted(" • "),
            warning(&format!("{label} configured"))
        );
    }
    let source = status
        .label
        .clone()
        .or_else(|| status.source.map(str::to_string))
        .unwrap_or_default();
    if source.is_empty() || source == "OAuth" || source == "stored credential" {
        return success(" ✓ configured");
    }
    // An all-caps source is one or more env-var names
    // (`/^[A-Z][A-Z0-9_]*(?:, [A-Z][A-Z0-9_]*)*$/`).
    let is_env_names = source.split(", ").all(|part| {
        let mut chars = part.chars();
        chars.next().is_some_and(|first| first.is_ascii_uppercase())
            && chars.all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
    });
    let rendered = if is_env_names {
        format!("env: {source}")
    } else {
        source
    };
    success(&format!(" ✓ {rendered}"))
}

/// `getLoginProviderCompletionOptions` (`interactive-mode.ts:397-416`): the
/// `/login <provider>` argument completions, one per provider id with both
/// auth types, `AUTH_TYPE_ORDER`-sorted (`:385`) and `compareProviders`-ordered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginProviderCompletionOption {
    pub id: String,
    pub name: String,
    /// Auth types, oauth before `api_key`.
    pub auth_types: Vec<&'static str>,
    pub subscription: bool,
}

pub fn login_provider_completion_options(
    providers: &[AuthSelectorProvider],
) -> Vec<LoginProviderCompletionOption> {
    let mut by_id: Vec<LoginProviderCompletionOption> = Vec::new();
    for provider in providers {
        if let Some(existing) = by_id.iter_mut().find(|option| option.id == provider.id) {
            if !existing.auth_types.contains(&provider.auth_type) {
                existing.auth_types.push(provider.auth_type);
                existing
                    .auth_types
                    .sort_by_key(|auth_type| i32::from(*auth_type != "oauth"));
            }
            continue;
        }
        by_id.push(LoginProviderCompletionOption {
            id: provider.id.clone(),
            name: provider.name.clone(),
            auth_types: vec![provider.auth_type],
            subscription: provider.subscription,
        });
    }
    by_id.sort_by(|a, b| compare_providers((&a.id, &a.name), (&b.id, &b.name)));
    by_id
}

/// `getLoginProviderSearchText` (`interactive-mode.ts:421-426`).
pub fn login_provider_search_text(option: &LoginProviderCompletionOption) -> String {
    let auth_types = option
        .auth_types
        .iter()
        .map(|auth_type| {
            format!(
                "{auth_type} {}",
                format_auth_selector_provider_type(auth_type, option.subscription)
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!("{} {} {}", option.id, option.name, auth_types)
}

/// `formatLoginProviderCompletionDescription` (`interactive-mode.ts:428-433`).
pub fn login_provider_completion_description(option: &LoginProviderCompletionOption) -> String {
    let auth_types = option
        .auth_types
        .iter()
        .map(|auth_type| format_auth_selector_provider_type(auth_type, option.subscription))
        .collect::<Vec<_>>()
        .join("/");
    if option.name == option.id {
        auth_types
    } else {
        format!("{} · {}", option.name, auth_types)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models_config::{ExtensionOAuthMetadata, ModelsJsonProvider};
    use serde_json::json;
    use std::path::PathBuf;

    fn models_json_provider(fields: serde_json::Value) -> ModelsJsonProvider {
        serde_json::from_value(fields).expect("models.json provider")
    }

    fn auth_path(data: &serde_json::Value) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        std::fs::write(&path, serde_json::to_string(data).expect("auth.json")).expect("write");
        (dir, path)
    }

    /// `composeApiKeyAuth`/`composeOAuthAuth` outcomes
    /// (`provider-composer.ts:422-511`).
    #[test]
    fn oauth_only_providers_get_no_fabricated_api_key_method() {
        // openai-codex ships OAuth only (`openai-codex.ts:12-17`).
        let methods = provider_auth_methods("openai-codex", None, None);
        assert_eq!(
            methods,
            vec![AuthMethod::OAuth(OAuthAuthInfo {
                name: "OpenAI (ChatGPT Plus/Pro)".to_string(),
                is_subscription: true,
                login_label: None,
            })]
        );
    }

    #[test]
    fn subscription_providers_carry_the_verbatim_metadata() {
        let methods = provider_auth_methods("anthropic", None, None);
        assert_eq!(
            methods,
            vec![
                AuthMethod::OAuth(OAuthAuthInfo {
                    name: "Anthropic (Claude Pro/Max)".to_string(),
                    is_subscription: true,
                    login_label: None,
                }),
                AuthMethod::ApiKey(ApiKeyAuthInfo {
                    name: "Anthropic API key",
                }),
            ]
        );
        let methods = provider_auth_methods("openrouter", None, None);
        match methods.first().expect("oauth first") {
            AuthMethod::OAuth(oauth) => {
                assert_eq!(oauth.name, "OpenRouter OAuth");
                assert!(!oauth.is_subscription);
                assert_eq!(oauth.login_label, Some("Sign in with OpenRouter"));
            }
            other @ AuthMethod::ApiKey(_) => panic!("expected oauth, got {other:?}"),
        }
    }

    #[test]
    fn models_json_radius_providers_inherit_the_radius_auth_pair() {
        // `configureRadiusProviders` (`model-runtime.ts:274-287`): a
        // models.json `oauth: "radius"` provider becomes a Radius-gateway
        // provider — the Radius API-key auth plus a named OAuth method.
        let config = models_json_provider(json!({
            "name": "Acme Radius",
            "baseUrl": "https://acme.local/v1",
            "oauth": "radius",
        }));
        let methods = provider_auth_methods("acme", Some(&config), None);
        assert_eq!(
            methods,
            vec![
                AuthMethod::OAuth(OAuthAuthInfo {
                    name: "Acme Radius".to_string(),
                    is_subscription: false,
                    login_label: None,
                }),
                AuthMethod::ApiKey(ApiKeyAuthInfo {
                    name: "Radius API key",
                }),
            ]
        );
    }

    #[test]
    fn extension_oauth_metadata_rides_as_the_oauth_method() {
        // `adaptOAuth` (`provider-composer.ts:358-377`): the extension's
        // config is the OAuthAuth; the JSON bridge carries its metadata.
        let extension = ExtensionProviderConfig {
            name: Some("Ext Provider".to_string()),
            oauth: Some(ExtensionOAuthMetadata {
                name: "Ext Account".to_string(),
                is_subscription: true,
                uses_callback_server: None,
            }),
            ..Default::default()
        };
        let methods = provider_auth_methods("ext-provider", None, Some(&extension));
        assert_eq!(
            methods,
            vec![AuthMethod::OAuth(OAuthAuthInfo {
                name: "Ext Account".to_string(),
                is_subscription: true,
                login_label: None,
            })]
        );
    }

    #[test]
    fn configured_keys_compose_the_api_key_method() {
        // A models.json key on a provider without an inherited auth
        // resolves as "API key" (`provider-composer.ts:438-441`).
        let config = models_json_provider(json!({"apiKey": "$ACME_KEY"}));
        let methods = provider_auth_methods("acme", Some(&config), None);
        assert_eq!(
            methods,
            vec![AuthMethod::ApiKey(ApiKeyAuthInfo { name: "API key" })]
        );
    }

    #[test]
    fn display_names_prefer_extension_then_models_json_then_id() {
        let config = models_json_provider(json!({"name": "Configured Name"}));
        let extension = ExtensionProviderConfig {
            name: Some("Extension Name".to_string()),
            ..Default::default()
        };
        assert_eq!(
            provider_display_name("acme", Some(&config), Some(&extension)),
            "Extension Name"
        );
        assert_eq!(
            provider_display_name("acme", Some(&config), None),
            "Configured Name"
        );
        assert_eq!(provider_display_name("acme", None, None), "acme");
    }

    /// `compareProviders` (`core/preferred-providers.ts:17-23`).
    #[test]
    fn preferred_partners_sort_first_then_names() {
        let mut ids = ["zai", "openrouter", "aardvark", "radius"];
        ids.sort_by(|a, b| compare_providers((a, a), (b, b)));
        assert_eq!(ids, ["radius", "openrouter", "aardvark", "zai"]);
    }

    /// `getProviderAuthStatus` + `configuredRequestAuthStatus`
    /// (`model-runtime.ts:655-665`, `provider-composer.ts:707-721`).
    #[test]
    fn stored_credentials_own_the_status() {
        let (dir, path) = auth_path(&json!({
            "acme": {"type": "oauth", "access": "tok", "refresh": "r", "expires": 9_000_000_000_000i64},
            "other": {"type": "api_key", "key": "sk-test"},
        }));
        let status = provider_auth_status("acme", None, None, &path).expect("acme status");
        assert_eq!(status.auth_type, "oauth");
        assert_eq!(status.source, Some("stored"));
        let status = provider_auth_status("other", None, None, &path).expect("other status");
        assert_eq!(status.auth_type, "api_key");
        assert_eq!(status.source, Some("stored"));
        assert!(provider_auth_status("missing", None, None, &path).is_none());
        drop(dir);
    }

    #[test]
    fn configured_keys_report_the_models_json_sources() {
        let config = models_json_provider(json!({"apiKey": "literal-key"}));
        let status = provider_auth_status("acme", Some(&config), None, Path::new("/nowhere"))
            .expect("configured");
        assert_eq!(status.source, Some("models_json_key"));
        assert_eq!(status.auth_type, "api_key");

        // The command form (`isCommandConfigValue`).
        let config = models_json_provider(json!({"apiKey": "!cat key"}));
        let status = provider_auth_status("acme", Some(&config), None, Path::new("/nowhere"))
            .expect("configured");
        assert_eq!(status.source, Some("models_json_command"));

        // The `$VAR` form reports `environment` with the names, and is not
        // configured while a referenced variable is missing.
        let config = models_json_provider(json!({"apiKey": "$PI_RS_SLICE6C_TEST_VAR"}));
        let status = provider_auth_status("acme", Some(&config), None, Path::new("/nowhere"));
        assert!(!status.expect("status").configured);

        let extension = ExtensionProviderConfig {
            api_key: Some("literal-key".to_string()),
            ..Default::default()
        };
        let status = provider_auth_status("acme", None, Some(&extension), Path::new("/nowhere"))
            .expect("configured");
        assert_eq!(status.source, Some("fallback"));
    }

    /// `formatAuthSelectorProviderStatus` (`oauth-selector.ts:30-49`).
    #[test]
    fn status_rows_render_the_upstream_suffixes() {
        let plain = |text: &str| pi_tui::ansi::strip_terminal_sequences(text);
        let stored = AuthSelectorProvider {
            id: "acme".to_string(),
            name: "Acme".to_string(),
            auth_type: "api_key",
            subscription: false,
            method_name: Some("Acme API key".to_string()),
            login_label: None,
            status: Some(AuthStatus {
                configured: true,
                auth_type: "api_key",
                source: Some("stored"),
                label: None,
            }),
        };
        let status = stored.status.clone().expect("status");
        assert_eq!(
            plain(&format_auth_selector_provider_status(&stored, &status)),
            " ✓ stored"
        );
        // The stored credential type drives the mismatch warning.
        let provider = AuthSelectorProvider {
            auth_type: "oauth",
            ..stored.clone()
        };
        assert_eq!(
            plain(&format_auth_selector_provider_status(&provider, &status)),
            " • API key configured"
        );
        // Not configured.
        let provider = AuthSelectorProvider {
            status: None,
            ..stored.clone()
        };
        let none_status = AuthStatus {
            configured: false,
            auth_type: "api_key",
            source: None,
            label: None,
        };
        assert_eq!(
            plain(&format_auth_selector_provider_status(
                &provider,
                &none_status
            )),
            " • not configured"
        );
        // An all-caps source renders as env names.
        let provider = AuthSelectorProvider {
            status: Some(AuthStatus {
                configured: true,
                auth_type: "api_key",
                source: Some("environment"),
                label: Some("OPENAI_API_KEY".to_string()),
            }),
            ..stored
        };
        let status = provider.status.clone().expect("status");
        assert_eq!(
            plain(&format_auth_selector_provider_status(&provider, &status)),
            " ✓ env: OPENAI_API_KEY"
        );
    }

    /// `findMatchingProviders` (`core/provider-matching.ts:5-11`).
    #[test]
    fn matching_is_exact_and_case_insensitive() {
        let providers = vec![AuthSelectorProvider {
            id: "openai-codex".to_string(),
            name: "OpenAI Codex".to_string(),
            auth_type: "oauth",
            subscription: true,
            method_name: None,
            login_label: None,
            status: None,
        }];
        assert_eq!(
            find_matching_providers(&providers, "  OpenAI-Codex ")
                .iter()
                .map(|provider| provider.id.as_str())
                .collect::<Vec<_>>(),
            vec!["openai-codex"]
        );
        // Display names match too; partial references do not.
        assert_eq!(find_matching_providers(&providers, "openai codex").len(), 1);
        assert_eq!(
            find_matching_providers(&providers, "openai"),
            [] as [&AuthSelectorProvider; 0]
        );
        assert_eq!(
            find_matching_providers(&providers, ""),
            [] as [&AuthSelectorProvider; 0]
        );
    }

    /// `getLoginProviderCompletionOptions` + the description formatter
    /// (`interactive-mode.ts:397-433`).
    #[test]
    fn completions_merge_auth_types_and_describe_them() {
        let providers = vec![
            AuthSelectorProvider {
                id: "acme".to_string(),
                name: "acme".to_string(),
                auth_type: "api_key",
                subscription: false,
                method_name: None,
                login_label: None,
                status: None,
            },
            AuthSelectorProvider {
                id: "acme".to_string(),
                name: "acme".to_string(),
                auth_type: "oauth",
                subscription: false,
                method_name: None,
                login_label: None,
                status: None,
            },
            AuthSelectorProvider {
                id: "other".to_string(),
                name: "Other Corp".to_string(),
                auth_type: "oauth",
                subscription: true,
                method_name: None,
                login_label: None,
                status: None,
            },
        ];
        let completions = login_provider_completion_options(&providers);
        assert_eq!(
            completions
                .iter()
                .map(|option| (option.id.as_str(), option.auth_types.clone()))
                .collect::<Vec<_>>(),
            vec![("acme", vec!["oauth", "api_key"]), ("other", vec!["oauth"]),]
        );
        let option = &completions[0];
        assert_eq!(
            login_provider_completion_description(option),
            "account/API key"
        );
        // A name distinct from the id is prefixed (`interactive-mode.ts:431-433`).
        let option = &completions[1];
        assert_eq!(
            login_provider_completion_description(option),
            "Other Corp · subscription"
        );
        assert_eq!(
            login_provider_search_text(option),
            "other Other Corp oauth subscription"
        );
    }
}
