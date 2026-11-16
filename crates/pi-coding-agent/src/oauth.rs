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
//! - `core/runtime-credentials.ts` — the `RuntimeCredentials` overlay
//!   (`:8-49`): the `--api-key` flag's non-persistent key rides
//!   `getProviderAuthStatus` (`model-runtime.ts:656`) and the logout
//!   credential list (`:36-41`).
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
//! The login option list enumerates the composed provider registry
//! ([`crate::models_state::providers`] — upstream's `getProviders()`,
//! `model-runtime.ts:442-444`, fed by `providerIds()` `:288-295`): every
//! provider contributes an OAuth row when its composed auth has OAuth and an
//! API-key row when it has API-key auth, regardless of whether it carries
//! models. Upstream's RPC mode has no auth dialogs at all
//! (`modes/rpc/rpc-mode.ts` exposes only `extension_ui_request`
//! select/input methods and no login/logout commands), so nothing rides the
//! RPC dialog subprotocol here.
//!
//! Deviations: the built-in `envApiKeyAuth`/custom-resolver auth metadata and
//! the built-in provider display names are static tables here
//! ([`api_key_auth_name`]/[`custom_api_key_auth_name`]/[`builtin_provider_name`]),
//! ported verbatim from the `providers/*.ts` factories — the port's embedded
//! model catalog carries provider ids only. `configureRadiusProviders`'
//! gateway rebuild is mirrored in metadata
//! ([`is_radius_oauth`]/[`provider_auth_methods`]) rather than as a real
//! provider object. Upstream's RPC mode has no auth dialogs (see above).

use crate::auth::read_stored_credential;
use crate::models_config::{ExtensionProviderConfig, ModelsJsonProvider};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::RwLock;

/// `RADIUS_PROVIDER_ID` (`core/radius.ts:3`).
pub const RADIUS_PROVIDER_ID: &str = "radius";

/// The runtime API-key overlay (`core/runtime-credentials.ts:8-32`): the
/// `--api-key` flag registers the session provider's non-persistent key
/// (`main.ts:898-906`), which the auth surfaces report
/// (`getProviderAuthStatus`, `model-runtime.ts:656`) and the logout list
/// merges over the stored credentials (`RuntimeCredentials.list`,
/// `runtime-credentials.ts:36-41`).
static RUNTIME_API_KEYS: RwLock<BTreeMap<String, String>> = RwLock::new(BTreeMap::new());

/// `RuntimeCredentials.setRuntimeApiKey` (`runtime-credentials.ts:14-16`).
pub fn set_runtime_api_key(provider_id: &str, api_key: &str) {
    if let Ok(mut keys) = RUNTIME_API_KEYS.write() {
        keys.insert(provider_id.to_string(), api_key.to_string());
    }
}

/// `RuntimeCredentials.removeRuntimeApiKey` (`runtime-credentials.ts:18-20`).
pub fn remove_runtime_api_key(provider_id: &str) {
    if let Ok(mut keys) = RUNTIME_API_KEYS.write() {
        keys.remove(provider_id);
    }
}

/// `RuntimeCredentials.hasRuntimeApiKey` (`runtime-credentials.ts:22-24`).
fn has_runtime_api_key(provider_id: &str) -> bool {
    RUNTIME_API_KEYS
        .read()
        .is_ok_and(|keys| keys.contains_key(provider_id))
}

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

/// The API-key auth display names of the built-in providers whose `auth.apiKey`
/// is a custom resolver rather than the standard `envApiKeyAuth`
/// (`auth/helpers.ts`): the AWS auth-method chooser, the Google Cloud method
/// select, and the Cloudflare account config. Their `login` flows prompt
/// beyond a single secret, so `/login` reports the ambient-auth message
/// ([`AMBIENT_LOGIN_PROVIDERS`]) — but the rows themselves carry these
/// verbatim names (`method.name`, `oauth-selector.ts:142`).
fn custom_api_key_auth_name(provider_id: &str) -> Option<&'static str> {
    match provider_id {
        "amazon-bedrock" => Some("AWS credentials or bearer token"), // amazon-bedrock.ts:16
        "google-vertex" => Some("Google Cloud credentials"),         // google-vertex.ts:18
        "cloudflare-workers-ai" | "cloudflare-ai-gateway" => Some("Cloudflare API key"), // cloudflare-auth.ts:50,73
        _ => None,
    }
}

/// The built-in provider display names — the `base?.name` layer of
/// `provider_display_name` (`provider-composer.ts:598-600`), the verbatim
/// `createProvider({name})` values (`providers/*.ts`, `radius.ts:24`).
pub fn builtin_provider_name(provider_id: &str) -> Option<&'static str> {
    match provider_id {
        "amazon-bedrock" => Some("Amazon Bedrock"), // amazon-bedrock.ts:85
        "ant-ling" => Some("Ant Ling"),             // ant-ling.ts:9
        "anthropic" => Some("Anthropic"),           // anthropic.ts:77
        "azure" => Some("Azure"),                   // azure.ts:47
        "baseten" => Some("Baseten"),               // baseten.ts:9
        "cerebras" => Some("Cerebras"),             // cerebras.ts:9
        "cloudflare-ai-gateway" => Some("Cloudflare AI Gateway"), // cloudflare-ai-gateway.ts:18
        "cloudflare-workers-ai" => Some("Cloudflare Workers AI"), // cloudflare-workers-ai.ts:14
        "deepseek" => Some("DeepSeek"),             // deepseek.ts:9
        "fireworks" => Some("Fireworks"),           // fireworks.ts:10
        "github-copilot" => Some("GitHub Copilot"), // github-copilot.ts:27
        "google" => Some("Google"),                 // google.ts:9
        "google-vertex" => Some("Google Vertex AI"), // google-vertex.ts:95
        "groq" => Some("Groq"),                     // groq.ts:9
        "huggingface" => Some("Hugging Face"),      // huggingface.ts:9
        "kimi-coding" => Some("Kimi For Coding"),   // kimi-coding.ts:10
        "meta" => Some("Meta"),                     // meta.ts:10
        "minimax" => Some("MiniMax"),               // minimax.ts:9
        "minimax-cn" => Some("MiniMax CN"),         // minimax-cn.ts:9
        "mistral" => Some("Mistral"),               // mistral.ts:9
        "moonshotai" => Some("Moonshot AI"),        // moonshotai.ts:9
        "moonshotai-cn" => Some("Moonshot AI CN"),  // moonshotai-cn.ts:9
        "nvidia" => Some("NVIDIA"),                 // nvidia.ts:9
        "openai" => Some("OpenAI"),                 // openai.ts:12
        "openai-codex" => Some("OpenAI Codex"),     // openai-codex.ts:10
        "opencode" => Some("OpenCode Zen"),         // opencode.ts:16
        "opencode-go" => Some("OpenCode Go"),       // opencode-go.ts:15
        "openrouter" => Some("OpenRouter"),         // openrouter.ts:65
        "qwen-token-plan" => Some("Qwen Token Plan Team"), // qwen-token-plan.ts:9
        "qwen-token-plan-cn" => Some("Qwen Token Plan Team CN"), // qwen-token-plan-cn.ts:9
        "qwen-token-plan-individual" => Some("Qwen Token Plan Individual"), // qwen-token-plan-individual.ts:9
        "qwen-token-plan-individual-cn" => Some("Qwen Token Plan Individual CN"), // qwen-token-plan-individual-cn.ts:9
        "radius" => Some("Radius"),                 // radius.ts:24
        "together" => Some("Together"),             // together.ts:9
        "typesafe" => Some("TypeSafe"),             // typesafe.ts:9
        "vercel-ai-gateway" => Some("Vercel AI Gateway"), // vercel-ai-gateway.ts:10
        "xai" => Some("xAI"),                       // xai.ts:10
        "xiaomi" => Some("Xiaomi"),                 // xiaomi.ts:9
        "xiaomi-token-plan-ams" => Some("Xiaomi Token Plan AMS"), // xiaomi-token-plan-ams.ts:9
        "xiaomi-token-plan-cn" => Some("Xiaomi Token Plan CN"), // xiaomi-token-plan-cn.ts:9
        "xiaomi-token-plan-sgp" => Some("Xiaomi Token Plan SGP"), // xiaomi-token-plan-sgp.ts:9
        "zai" => Some("Z.AI"),                      // zai.ts:9
        "zai-coding-cn" => Some("Z.AI Coding CN"),  // zai-coding-cn.ts:9
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
/// `extension?.name ?? config?.name ?? base?.name ?? providerId`, where the
/// base layer is the built-in provider's verbatim display name
/// ([`builtin_provider_name`]) — except for models.json Radius-gateway
/// providers, whose base is rebuilt with `name: config.name ?? providerId`
/// (`model-runtime.ts:277-284`), so their base layer never contributes.
pub fn provider_display_name(
    provider_id: &str,
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ExtensionProviderConfig>,
) -> String {
    let base_name = if config.is_some_and(is_radius_oauth) {
        None
    } else {
        builtin_provider_name(provider_id)
    };
    extension
        .and_then(|extension| extension.name.clone())
        .or_else(|| config.and_then(|config| config.name.clone()))
        .or_else(|| base_name.map(str::to_string))
        .unwrap_or_else(|| provider_id.to_string())
}

/// A models.json provider entry `configureRadiusProviders` turns into a
/// Radius-gateway provider: `oauth: "radius"` (the only accepted value;
/// `model-config.ts:108`) with the `baseUrl` it requires
/// (`model-runtime.ts:276` skips entries without one; `provider-composer.ts:309`
/// rejects `oauth` without `baseUrl` outright).
fn is_radius_oauth(config: &ModelsJsonProvider) -> bool {
    config.oauth.as_deref() == Some("radius") && config.base_url.is_some()
}

/// The composed auth methods of one provider — the outcome of
/// `composeApiKeyAuth` (`provider-composer.ts:422-486`) +
/// `composeOAuthAuth` (`:488-511`) for the metadata the port carries.
///
/// OAuth: an extension oauth config (`adaptOAuth`, `:358-377`) wins, then the
/// built-in provider's OAuth, then a models.json `oauth: "radius"` entry,
/// which `configureRadiusProviders` (`model-runtime.ts:274-287`) turns into a
/// Radius-gateway provider inheriting the Radius auth pair — its OAuth name
/// is `config.name ?? providerId` (the rebuilt `radiusProvider({name})`'s
/// `lazyOAuth({name})`, `radius.ts:39-40`). API key: the built-in
/// `envApiKeyAuth` or the custom-resolver auth (inherited), else the
/// configured `apiKey` (`extension?.apiKey ?? config?.apiKey`); an OAuth-only
/// provider gets no fabricated API-key method (`:432`). The order matches
/// `getLoginProviderOptions` (`interactive-mode.ts:6002-6036`): the OAuth
/// entry before the API-key entry.
pub fn provider_auth_methods(
    provider_id: &str,
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ExtensionProviderConfig>,
) -> Vec<AuthMethod> {
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
            // provider (`model-runtime.ts:274-287`): the rebuilt
            // `radiusProvider({name: config.name ?? providerId})`'s
            // `lazyOAuth({name})` carries that name (`radius.ts:39-40`).
            // Otherwise this is the default `radiusProvider()` builtin, whose
            // OAuth name is its default display name (`radius.ts:24`).
            // `isSubscription` stays unset either way.
            let name = if config.is_some_and(is_radius_oauth) {
                config
                    .and_then(|config| config.name.clone())
                    .unwrap_or_else(|| provider_id.to_string())
            } else {
                builtin_provider_name(provider_id)
                    .map(str::to_string)
                    .unwrap_or_else(|| provider_id.to_string())
            };
            Some(OAuthAuthInfo {
                name,
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
    // envApiKeyAuth or the custom-resolver auth; a models.json radius
    // provider inherits the Radius one) or a configured key; OAuth-only
    // providers get no fabricated method (`:432`).
    let inherited = if radius_style {
        api_key_auth_name(RADIUS_PROVIDER_ID)
    } else {
        api_key_auth_name(provider_id).or_else(|| custom_api_key_auth_name(provider_id))
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
/// `configuredRequestAuthStatus` (`provider-composer.ts:707-721`): the
/// runtime `--api-key` overlay wins, then a stored credential, then the
/// configured models.json/extension key, then the provider environment
/// variables (`envApiKeyAuth.resolve`'s loop). `None` renders as
/// "not configured".
pub fn provider_auth_status(
    provider_id: &str,
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ExtensionProviderConfig>,
    auth_path: &Path,
) -> Option<AuthStatus> {
    // The runtime `--api-key` key wins (`model-runtime.ts:656`: `{ configured:
    // true, source: "runtime" }`).
    if has_runtime_api_key(provider_id) {
        return Some(AuthStatus {
            configured: true,
            auth_type: "api_key",
            source: Some("runtime"),
            label: None,
        });
    }
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

/// `getLogoutProviderOptions` (`interactive-mode.ts:6038-6050`): the stored
/// credentials — plus the runtime `--api-key` overrides
/// ([`RUNTIME_API_KEYS`]) — one row each, `compareProviders`-sorted. The
/// status rides the verbatim `"stored credential"` source (`:6046`), which
/// renders as " ✓ configured" (`formatAuthSelectorProviderStatus`'s special
/// case). Storage failures propagate (`showOAuthSelector` reports them).
pub fn logout_provider_options(
    auth_path: &Path,
) -> Result<Vec<AuthSelectorProvider>, crate::auth::AuthError> {
    let storage = crate::auth::AuthStorage::create(auth_path);
    let listed = storage.list()?;
    // `RuntimeCredentials.list` (`runtime-credentials.ts:36-41`): the runtime
    // `--api-key` overrides ride the list, replacing same-provider entries.
    let mut listed = listed;
    if let Ok(keys) = RUNTIME_API_KEYS.read() {
        for provider_id in keys.keys() {
            listed.retain(|info| info.provider_id != *provider_id);
            listed.push(crate::auth::CredentialInfo {
                provider_id: provider_id.clone(),
                credential_type: "api_key".to_string(),
            });
        }
    }
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
                    // The verbatim `getLogoutProviderOptions` status source
                    // (`interactive-mode.ts:6046`) — renders " ✓ configured".
                    source: Some("stored credential"),
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

    /// A synthetic models.json the composing tests refresh the global
    /// snapshot from ([`crate::models_state::refresh_with_config`]).
    fn temp_models_json(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("pi-rs-oauth-tests");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join(format!("{}-{name}.json", std::process::id()));
        std::fs::write(&path, content).expect("write temp models.json");
        path
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
        let _guard = crate::models_state::REBUILD_LOCK.lock().expect("lock");
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
        let _guard = crate::models_state::REBUILD_LOCK.lock().expect("lock");
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

    /// The built-in provider display names (`createProvider({name})`,
    /// `providers/*.ts`) — the `base?.name` layer of
    /// `provider_display_name` (`provider-composer.ts:598-600`).
    #[test]
    fn builtin_provider_names_are_verbatim() {
        assert_eq!(builtin_provider_name("anthropic"), Some("Anthropic"));
        assert_eq!(builtin_provider_name("amazon-bedrock"), Some("Amazon Bedrock"));
        assert_eq!(builtin_provider_name("kimi-coding"), Some("Kimi For Coding"));
        assert_eq!(builtin_provider_name("opencode"), Some("OpenCode Zen"));
        assert_eq!(builtin_provider_name("radius"), Some("Radius"));
        assert_eq!(builtin_provider_name("acme"), None);
    }

    /// The custom-resolver API-key auth names (`providers/*.ts`) — rows the
    /// standard `envApiKeyAuth` table does not carry.
    #[test]
    fn custom_api_key_names_cover_the_ambient_providers() {
        assert_eq!(
            custom_api_key_auth_name("amazon-bedrock"),
            Some("AWS credentials or bearer token")
        );
        assert_eq!(
            custom_api_key_auth_name("google-vertex"),
            Some("Google Cloud credentials")
        );
        assert_eq!(
            custom_api_key_auth_name("cloudflare-ai-gateway"),
            Some("Cloudflare API key")
        );
        assert_eq!(custom_api_key_auth_name("openai"), None);
    }

    /// A models.json provider with `oauth: "radius"` but no `baseUrl` is not
    /// a Radius gateway (`model-runtime.ts:276` skips it;
    /// `provider-composer.ts:309` rejects it) — no fabricated Radius pair.
    #[test]
    fn radius_oauth_without_base_url_is_not_a_gateway() {
        let config = models_json_provider(json!({"oauth": "radius"}));
        assert_eq!(provider_auth_methods("acme", Some(&config), None), vec![]);
        // An explicit `apiKey` still composes the plain API-key method
        // (`composeApiKeyAuth`'s rawKey arm, `:438-441`).
        let config = models_json_provider(json!({"oauth": "radius", "apiKey": "sk-acme"}));
        assert_eq!(
            provider_auth_methods("acme", Some(&config), None),
            vec![AuthMethod::ApiKey(ApiKeyAuthInfo { name: "API key" })]
        );
    }

    /// `getLoginProviderOptions` over the whole composed registry
    /// (`interactive-mode.ts:6002-6036`): the built-in catalog rows carry
    /// their verbatim display names and auth pairs, an api-key-only
    /// models.json provider lists despite having no models
    /// (`getProviders()`, `model-runtime.ts:442-444`), and a provider with
    /// no auth method contributes nothing (`composeModelProvider` throws and
    /// the runtime drops it, `:309-325` / `:585`).
    #[test]
    fn login_rows_enumerate_the_whole_provider_registry() {
        let _guard = crate::models_state::REBUILD_LOCK.lock().expect("lock");
        let path = temp_models_json(
            "login-rows",
            r#"{"providers": {"acme-key": {"apiKey": "sk-acme"}, "bare": {}}}"#,
        );
        crate::models_state::refresh_with_config(&crate::models_config::ModelConfig::load(&path));

        let options = login_provider_options(Path::new("/nonexistent/auth.json"));
        let plain_name = |id: &str, auth_type: &str| {
            options
                .iter()
                .find(|provider| provider.id == id && provider.auth_type == auth_type)
        };

        // Preferred partners first, in `PREFERRED_PROVIDER_IDS` order
        // (`core/preferred-providers.ts:12`); a provider's account row stays
        // above its API-key row (the stable sort, `:6035`).
        assert_eq!(options[0].id, "radius");
        assert_eq!(options[0].auth_type, "oauth");
        assert_eq!(options[0].name, "Radius");
        assert_eq!(options[1].id, "radius");
        assert_eq!(options[1].auth_type, "api_key");
        assert_eq!(options[2].id, "openrouter");
        assert_eq!(options[2].auth_type, "oauth");
        assert_eq!(options[3].id, "openrouter");
        assert_eq!(options[3].auth_type, "api_key");
        // Then the catalog alphabetically by display name.
        assert_eq!(options[4].id, "acme-key");
        assert_eq!(options[4].name, "acme-key");
        assert_eq!(options[5].id, "amazon-bedrock");
        assert_eq!(options[5].name, "Amazon Bedrock");

        // An api-key-only models.json provider lists without models, with
        // the composed "API key" method (`provider-composer.ts:438-441`) and
        // the literal-key status (`configuredRequestAuthStatus`, `:719-721`).
        let acme = plain_name("acme-key", "api_key").expect("acme-key row");
        assert_eq!(acme.method_name.as_deref(), Some("API key"));
        let status = acme.status.as_ref().expect("configured");
        assert_eq!(status.source, Some("models_json_key"));
        assert!(options.iter().all(|provider| provider.id != "bare"));

        // A subscription OAuth provider carries both rows with the verbatim
        // metadata (`anthropic.ts:79-85`).
        let oauth = plain_name("anthropic", "oauth").expect("oauth row");
        assert_eq!(oauth.name, "Anthropic");
        assert_eq!(oauth.method_name.as_deref(), Some("Anthropic (Claude Pro/Max)"));
        assert!(oauth.subscription);
        let api_key = plain_name("anthropic", "api_key").expect("api key row");
        assert_eq!(api_key.method_name.as_deref(), Some("Anthropic API key"));
        assert_eq!(
            options
                .iter()
                .filter(|provider| provider.id == "anthropic")
                .count(),
            2
        );

        // An OAuth-only provider has no fabricated API-key row
        // (`openai-codex.ts:12-17`, `provider-composer.ts:432`).
        let codex = plain_name("openai-codex", "oauth").expect("codex oauth row");
        assert_eq!(codex.name, "OpenAI Codex");
        assert!(plain_name("openai-codex", "api_key").is_none());
        // OpenRouter's OAuth is not subscription-backed, so the row labels
        // as an account and carries the `loginLabel` (`openrouter.ts:69-73`).
        let openrouter = plain_name("openrouter", "oauth").expect("openrouter row");
        assert!(!openrouter.subscription);
        assert_eq!(openrouter.login_label, Some("Sign in with OpenRouter"));

        let _ = std::fs::remove_file(&path);
    }

    /// `getLogoutProviderOptions` (`interactive-mode.ts:6038-6050`): one row
    /// per stored credential — including providers the registry does not
    /// know (the name falls back to the id) — with the verbatim
    /// `"stored credential"` status source that renders " ✓ configured".
    #[test]
    fn logout_rows_list_the_stored_credentials() {
        let _guard = crate::models_state::REBUILD_LOCK.lock().expect("lock");
        let (dir, path) = auth_path(&json!({
            "acme": {"type": "oauth", "access": "tok", "refresh": "r", "expires": 9_000_000_000_000i64},
            "other": {"type": "api_key", "key": "sk-test"},
            "ghost": {"type": "api_key", "key": "sk-ghost"},
        }));

        let options = logout_provider_options(&path).expect("listed");
        assert_eq!(
            options
                .iter()
                .map(|provider| (provider.id.as_str(), provider.auth_type))
                .collect::<Vec<_>>(),
            [("acme", "oauth"), ("ghost", "api_key"), ("other", "api_key")]
        );
        // Unknown providers keep the id as the name (`:6044`).
        let ghost = &options[1];
        assert_eq!(ghost.name, "ghost");
        // The verbatim status source renders " ✓ configured"
        // (`oauth-selector.ts:44-46`).
        let status = ghost.status.as_ref().expect("status");
        assert_eq!(status.source, Some("stored credential"));
        let plain = pi_tui::ansi::strip_terminal_sequences(&format_auth_selector_provider_status(
            ghost,
            status,
        ));
        assert_eq!(plain, " ✓ configured");

        drop(dir);
    }

    /// The runtime `--api-key` overlay (`main.ts:898-906`): the status
    /// reports `runtime` before any stored credential
    /// (`getProviderAuthStatus`, `model-runtime.ts:656`), and the logout list
    /// merges the override over the same provider's stored entry
    /// (`RuntimeCredentials.list`, `runtime-credentials.ts:36-41`).
    #[test]
    fn runtime_api_key_overrides_status_and_logout_rows() {
        let _guard = crate::models_state::REBUILD_LOCK.lock().expect("lock");
        let (dir, path) = auth_path(&json!({
            "acme": {"type": "oauth", "access": "tok", "refresh": "r", "expires": 9_000_000_000_000i64},
            "other": {"type": "api_key", "key": "sk-test"},
        }));
        set_runtime_api_key("acme", "sk-flag");
        set_runtime_api_key("flagged", "sk-flagged");

        // The status rides `runtime` ahead of the stored credential.
        let status = provider_auth_status("acme", None, None, &path).expect("status");
        assert_eq!(status.source, Some("runtime"));
        assert_eq!(status.auth_type, "api_key");

        // The logout list carries the override rows, and the override
        // replaces the same provider's stored entry.
        let options = logout_provider_options(&path).expect("listed");
        assert_eq!(
            options
                .iter()
                .map(|provider| (provider.id.as_str(), provider.auth_type))
                .collect::<Vec<_>>(),
            [("acme", "api_key"), ("flagged", "api_key"), ("other", "api_key")]
        );

        remove_runtime_api_key("acme");
        remove_runtime_api_key("flagged");
        let status = provider_auth_status("acme", None, None, &path).expect("status");
        assert_eq!(status.source, Some("stored"));
        drop(dir);
    }
}
