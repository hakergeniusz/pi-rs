//! The composed model catalog (E2 slice 6b): the port's live model store.
//!
//! Upstream `ModelRuntime` composes, per provider, the built-in models with
//! the `models.json` layer and the extension registrations, plus virtual
//! models, and answers every lookup from that composition
//! (`core/model-runtime.ts`; the composer is `core/provider-composer.ts`).
//! The port's base catalog is process-global (`pi_ai::catalog`), so the
//! composed snapshot is one too: `refresh`/`refresh_with_runner` rebuild it
//! (startup, and whenever the extension registry's generation moves), and
//! every consumer reads through [`all`]/[`lookup`]/[`models_for`]/
//! [`providers`]/[`find_by_id`] instead of the static catalog. Before the
//! first refresh the static catalog is the answer (the engine-free shape).
//!
//! Slice 6c added the config snapshots the auth layer reads
//! ([`models_json_provider`]/[`extension_provider_config`]) — upstream keeps
//! the loaded `ModelConfig` and the extension `ProviderConfigInput`s on the
//! runtime and consults them per auth resolution (`model-runtime.ts`
//! `composeProvider`/`getAuth`); the port mirrors them as the last rebuild's
//! maps. Slice 6d added the `streamSimple` dispatch half of the composer:
//! [`stream_with_extension`] is `streamWith`'s extension arm
//! (`core/provider-composer.ts:588-613`) and [`ComposedStreamFn`] wraps it
//! around the caller's base adapter, so every request under the engine
//! streams through the composed dispatch (engine-free builds never see it).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde_json::Value;

#[cfg(feature = "ext-js")]
use pi_agent::agent_message::AgentMessage;
use pi_ai::types::Model;
#[cfg(feature = "ext-js")]
use pi_ai::types::StopReason;

use crate::config::get_models_path;
use crate::models_config::{self, ModelConfig};

static COMPOSED: RwLock<Option<Arc<Vec<Model>>>> = RwLock::new(None);
/// Composition errors (models.json / extension config problems), last
/// rebuild only. Upstream surfaces these through the registry's error state
/// (`ModelRegistry::getError`).
static ERRORS: RwLock<Vec<String>> = RwLock::new(Vec::new());
/// The models.json provider entries of the last loaded config — the auth
/// composition reads them (`model-runtime.ts`'s `this.config`).
static MODELS_JSON_PROVIDERS: RwLock<
    Option<Arc<BTreeMap<String, models_config::ModelsJsonProvider>>>,
> = RwLock::new(None);
/// The extension-registered provider configs of the last rebuild
/// (`model-runtime.ts`'s `extensionProviders` map).
#[cfg(feature = "ext-js")]
static EXTENSION_PROVIDERS: RwLock<Vec<(String, models_config::ExtensionProviderConfig)>> =
    RwLock::new(Vec::new());
#[cfg(feature = "ext-js")]
static LAST_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Rebuild from the static catalog + models.json (no extension registry).
pub fn refresh() {
    refresh_with_config(&ModelConfig::load(&get_models_path()));
}

/// Rebuild from an explicit models.json snapshot (the loader seam tests use).
pub fn refresh_with_config(models_config: &ModelConfig) {
    rebuild(models_config, &[], &[]);
}

/// Rebuild when the extension registry moved since the last build
/// (`model-runtime.ts` applies registrations live; the port rebuilds the
/// process-global snapshot lazily). No-op — without even a models.json read —
/// while the generation is unchanged.
#[cfg(feature = "ext-js")]
pub fn refresh_with_runner(runner: &pi_ext_js::ExtensionRunner) {
    let generation = runner.generation();
    if generation == LAST_GENERATION.load(Ordering::Relaxed)
        && COMPOSED.read().is_ok_and(|composed| composed.is_some())
    {
        return;
    }
    let providers = runner.providers();
    let virtual_defs = runner.virtual_models();
    let models_config = ModelConfig::load(&get_models_path());
    rebuild(&models_config, &providers, &virtual_defs);
    LAST_GENERATION.store(generation, Ordering::Relaxed);
}

/// Compose the snapshot from the pure registration data — the extension
/// registry stays behind the `ext-js` gate.
fn rebuild(
    models_config: &ModelConfig,
    providers: &[(String, String, Value)],
    virtual_defs: &[(Value, String)],
) {
    let mut errors = Vec::new();

    // Provider ids: base catalog first, then the models.json and extension
    // layers, then native registrations (data-only in the port — their
    // models arrive through config, recorded as a slice-6 deviation).
    let mut provider_ids: Vec<String> = Vec::new();
    for provider in pi_ai::catalog::catalog().providers() {
        provider_ids.push(provider.to_string());
    }
    for id in models_config.get_provider_ids() {
        if !provider_ids.iter().any(|existing| existing == id) {
            provider_ids.push(id.to_string());
        }
    }
    let mut registered_providers: Vec<(String, models_config::ExtensionProviderConfig)> =
        Vec::new();
    for (name, _, config) in providers {
        if !provider_ids.iter().any(|existing| existing == name) {
            provider_ids.push(name.clone());
        }
        match models_config::ExtensionProviderConfig::from_config(config) {
            Ok(config) => registered_providers.push((name.clone(), config)),
            Err(error) => errors.push(error),
        }
    }
    // The auth layer's config snapshots (`model-runtime.ts`'s `this.config`
    // and `extensionProviders`).
    let snapshot: BTreeMap<String, models_config::ModelsJsonProvider> = models_config
        .providers()
        .clone();
    if let Ok(mut slot) = MODELS_JSON_PROVIDERS.write() {
        *slot = Some(Arc::new(snapshot));
    }
    #[cfg(feature = "ext-js")]
    if let Ok(mut slot) = EXTENSION_PROVIDERS.write() {
        *slot = registered_providers.clone();
    }

    let mut models: Vec<Model> = Vec::new();
    for id in &provider_ids {
        let base: Vec<Model> = pi_ai::catalog::catalog()
            .models(id)
            .into_iter()
            .cloned()
            .collect();
        let base = match models_config::apply_models_json(id, &base, models_config.get_provider(id))
        {
            Ok(layered) => layered,
            Err(error) => {
                errors.push(error);
                base
            }
        };
        let extension = registered_providers
            .iter()
            .find(|(name, _)| name == id)
            .map(|(_, config)| config);
        match models_config::apply_extension(id, &base, extension) {
            Ok(composed) => models.extend(composed),
            Err(error) => {
                errors.push(error);
                models.extend(base);
            }
        }
    }
    // Virtual models are selectable catalog entries (`model-runtime.ts`).
    for model in crate::virtual_models::virtual_models_from_defs(virtual_defs) {
        if !models
            .iter()
            .any(|existing| existing.provider == model.provider && existing.id == model.id)
        {
            models.push(model);
        }
    }

    if let Ok(mut slot) = COMPOSED.write() {
        *slot = Some(Arc::new(models));
    }
    if let Ok(mut slot) = ERRORS.write() {
        *slot = errors;
    }
}

/// The composed snapshot; the flattened static catalog before the first
/// refresh.
pub fn all() -> Vec<Model> {
    if let Ok(composed) = COMPOSED.read() {
        if let Some(composed) = composed.as_ref() {
            return composed.as_ref().clone();
        }
    }
    let mut models = Vec::new();
    for provider in pi_ai::catalog::catalog().providers() {
        models.extend(
            pi_ai::catalog::catalog()
                .models(provider)
                .into_iter()
                .cloned(),
        );
    }
    models
}

/// `ModelRegistry::find` (`core/model-registry.ts:72-74`).
pub fn lookup(provider: &str, id: &str) -> Option<Model> {
    all()
        .into_iter()
        .find(|model| model.provider == provider && model.id == id)
}

/// `ModelRegistry::getAll` filtered to one provider.
pub fn models_for(provider: &str) -> Vec<Model> {
    all()
        .into_iter()
        .filter(|model| model.provider == provider)
        .collect()
}

/// Every provider id in the composition, in base-catalog-first order.
pub fn providers() -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for model in all() {
        if !ids.iter().any(|existing| existing == &model.provider) {
            ids.push(model.provider.clone());
        }
    }
    ids
}

/// `ModelRegistry`'s id-only lookup over every provider
/// (`core/model-registry.ts` `getAll` consumers).
pub fn find_by_id(id: &str) -> Vec<Model> {
    all().into_iter().filter(|model| model.id == id).collect()
}

/// Composition errors from the last rebuild.
pub fn errors() -> Vec<String> {
    ERRORS
        .read()
        .map(|errors| errors.clone())
        .unwrap_or_default()
}

/// The composed provider's stream dispatch (`streamWith` closure,
/// `core/provider-composer.ts:588-613`): every request first tries the
/// extension arm — the provider's registered `streamSimple` handler when
/// `model.api === extension.api` — then falls through to the base/api
/// adapter. The port's second and third upstream arms
/// (`base.streamSimple` when the base provider supports the api, then
/// `getApiProvider(model.api)`) share the caller-supplied fallback: the
/// port's base adapters are the static `pi-ai` API adapters the caller
/// already resolved.
#[cfg(feature = "ext-js")]
pub struct ComposedStreamFn {
    /// The engine the stashed `streamSimple` handlers live behind.
    pub runner: Arc<std::sync::Mutex<pi_ext_js::ExtensionRunner>>,
    /// The base/api arm: the adapter the caller would have installed
    /// without the engine (the selection's `ProviderStreamFn`).
    pub fallback: Arc<dyn pi_agent::types::StreamFn>,
}

#[cfg(feature = "ext-js")]
impl pi_agent::types::StreamFn for ComposedStreamFn {
    fn stream(
        &self,
        request: pi_agent::types::StreamRequest,
    ) -> futures::stream::BoxStream<'static, pi_agent::types::AssistantMessageEvent> {
        if let Some(stream) = stream_with_extension(&self.runner, &request) {
            return stream;
        }
        self.fallback.stream(request)
    }
}

/// `streamWith`'s extension arm (`provider-composer.ts:588-613` →
/// `extension.streamSimple(model, context, options)`): when the request's
/// provider has a registered `streamSimple` handler covering `model.api`,
/// the request marshals into the engine, the handler's event stream drains
/// there, and the events replay as the returned stream. A handler failure
/// — before or mid-iteration — appends the terminal error event after the
/// events already emitted, exactly like upstream's `lazyStream` forwards
/// until the failure and then ends with the error
/// (`packages/ai/src/api/lazy.ts:65-105`; the setup-failure message shape
/// is `createSetupErrorMessage`, `lazy.ts:8-36`). `None` when the provider
/// has no matching handler — the caller falls through the remaining
/// `streamWith` arms.
///
/// Deviation (mechanism only, see HANDOFF.md): upstream forwards events as
/// the handler emits them; the port drains the whole iterable inside the
/// engine (no incremental crossing of the `QuickJS` boundary) and replays
/// them, so downstream sees the events once the handler finishes. Ordering
/// and terminal-event semantics are unchanged.
#[cfg(feature = "ext-js")]
pub fn stream_with_extension(
    runner: &Arc<std::sync::Mutex<pi_ext_js::ExtensionRunner>>,
    request: &pi_agent::types::StreamRequest,
) -> Option<futures::stream::BoxStream<'static, pi_agent::types::AssistantMessageEvent>> {
    {
        let runner = runner.lock().expect("extension runner mutex poisoned");
        let api = runner.provider_stream_api(&request.model.provider)?;
        if api != request.model.api {
            return None;
        }
    }
    let payload = marshal_stream_request(request);
    let (events, mut failure) = match runner
        .lock()
        .expect("extension runner mutex poisoned")
        .provider_stream(&request.model.provider, &payload)
    {
        Ok(outcome) => (outcome.events, outcome.error),
        Err(err) => (Vec::new(), Some(err.to_string())),
    };
    let mut stream_events: Vec<pi_agent::types::AssistantMessageEvent> = Vec::new();
    for event in events {
        match serde_json::from_value(event) {
            Ok(event) => stream_events.push(event),
            Err(err) => {
                failure = Some(format!("streamSimple event is not a protocol event: {err}"));
                break;
            }
        }
    }
    if let Some(message) = failure {
        stream_events.push(extension_error_event(&request.model, message));
    }
    Some(Box::pin(futures::stream::iter(stream_events)))
}

/// The marshalled request — `streamSimple(model, context, options)`'s three
/// arguments (`core/extensions/types.ts:1989-2007`): the composed model,
/// the LLM-shaped transcript context (`TranscriptContext { messages }`), and
/// the options. Option keys mirror upstream's `SimpleStreamOptions` —
/// `apiKey`/`accessToken` ride `ProviderRequestOptions`
/// (`packages/ai/src/types.ts:161-183`) and only present values cross
/// (JSON drops `undefined`); `reasoning` is the clamped thinking level, the
/// same clamp `ProviderStreamFn` applies (`xhigh`/`max` → `high`).
#[cfg(feature = "ext-js")]
fn marshal_stream_request(request: &pi_agent::types::StreamRequest) -> Value {
    use pi_agent::types::ThinkingLevel;

    let mut options = serde_json::Map::new();
    if let Some(api_key) = &request.api_key {
        options.insert("apiKey".to_string(), Value::String(api_key.clone()));
    }
    if let Some(access_token) = &request.access_token {
        options.insert(
            "accessToken".to_string(),
            Value::String(access_token.clone()),
        );
    }
    let reasoning = match request.thinking_level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => "high",
    };
    options.insert(
        "reasoning".to_string(),
        Value::String(reasoning.to_string()),
    );
    if let Some(session_id) = &request.session_id {
        options.insert("sessionId".to_string(), Value::String(session_id.clone()));
    }
    serde_json::json!({
        "model": request.model,
        "context": {
            "messages": request
                .messages
                .iter()
                .filter_map(AgentMessage::to_llm_message)
                .collect::<Vec<_>>(),
        },
        "options": Value::Object(options),
    })
}

/// The terminal error event a failed extension stream ends with — the
/// `lazyStream` setup-failure shape (`packages/ai/src/api/lazy.ts:8-36`):
/// an assistant message with `stopReason: "error"` and the raw error
/// message, pushed as `{type: "error", reason, error}`
/// (`lazy.ts:104-108`).
#[cfg(feature = "ext-js")]
fn extension_error_event(model: &Model, message: String) -> pi_agent::types::AssistantMessageEvent {
    let error = pi_ai::types::AssistantMessage {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        usage: pi_ai::types::Usage::default(),
        stop_reason: StopReason::Error,
        error_message: Some(message),
        timestamp: pi_agent::agent_loop::now_millis(),
        ..pi_agent::empty_assistant_message()
    };
    pi_agent::types::AssistantMessageEvent::Error {
        reason: StopReason::Error,
        error,
    }
}

/// One provider's models.json entry from the last loaded config — the auth
/// composition's `config` layer (`model-runtime.ts`'s `this.config`).
/// `None` before the first refresh.
pub fn models_json_provider(provider_id: &str) -> Option<models_config::ModelsJsonProvider> {
    MODELS_JSON_PROVIDERS
        .read()
        .ok()
        .and_then(|slot| slot.as_ref().and_then(|providers| providers.get(provider_id).cloned()))
}

/// One provider's extension-registered config — the auth composition's
/// `extension` layer (`model-runtime.ts`'s `extensionProviders`). `None`
/// before a refresh with the engine.
#[cfg(feature = "ext-js")]
pub fn extension_provider_config(provider_id: &str) -> Option<models_config::ExtensionProviderConfig> {
    EXTENSION_PROVIDERS
        .read()
        .ok()
        .and_then(|slot| {
            slot.iter()
                .find(|(name, _)| name == provider_id)
                .map(|(_, config)| config.clone())
        })
}

/// Engine-free builds have no extension providers.
#[cfg(not(feature = "ext-js"))]
pub fn extension_provider_config(_provider_id: &str) -> Option<models_config::ExtensionProviderConfig> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn temp_models_json(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("pi-rs-models-state-tests");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join(format!("{}-{name}.json", std::process::id()));
        std::fs::write(&path, content).expect("write temp models.json");
        path
    }

    /// The composing tests share the process-global snapshot; the test
    /// harness runs them on parallel threads.
    static REBUILD_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn fallback_without_refresh_is_the_static_catalog() {
        // A known catalog model resolves through the reader surface either
        // way; before any refresh the static catalog is the answer.
        let model = lookup("openai-codex", "gpt-6-luna").expect("catalog model");
        assert_eq!(model.provider, "openai-codex");
        assert!(all().iter().any(|entry| entry.provider == "openai-codex"));
    }

    #[test]
    fn refresh_composes_models_json_models() {
        let _guard = REBUILD_LOCK.lock().expect("lock");
        let path = temp_models_json(
            "compose",
            r#"{"providers": {"acme": {"baseUrl": "https://acme.local/v1", "api": "openai-completions",
                "models": [{"id": "acme-1", "name": "Acme One", "reasoning": true, "input": ["text"],
                            "contextWindow": 90000, "maxTokens": 4096}]}}}"#,
        );
        refresh_with_config(&ModelConfig::load(&path));
        let model = lookup("acme", "acme-1").expect("models.json model");
        assert_eq!(model.context_window, 90_000);
        assert_eq!(model.base_url, "https://acme.local/v1");
        // The static catalog survives alongside the composed layer, and the
        // provider-id surface includes the models.json-only provider.
        assert!(lookup("openai-codex", "gpt-6-luna").is_some());
        assert!(providers().iter().any(|provider| provider == "acme"));
        assert_eq!(errors(), Vec::<String>::new());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn refresh_reports_composition_errors_as_values() {
        let _guard = REBUILD_LOCK.lock().expect("lock");
        // An apiKey-only provider is VALID upstream (auth without models);
        // a bare provider object is what trips the "must specify" check.
        let path = temp_models_json("bad", r#"{"providers": {"acme": {}}}"#);
        refresh_with_config(&ModelConfig::load(&path));
        let error = errors()
            .into_iter()
            .find(|error| error.contains("acme"))
            .expect("acme error");
        assert!(error.contains("must specify"), "{error}");
        let _ = std::fs::remove_file(&path);
    }

    // ---- streamWith (slice 6d) ----

    /// The message skeleton the handler's events carry — the fields the
    /// protocol requires (`AssistantMessage` with the zero `Usage`), built
    /// per event so the model fields can echo the marshalled request.
    const STREAM_EXTENSION: &str = r#"
export default function (pi) {
  const usage = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0,
                  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } };
  const message = (text) => ({ role: "assistant", content: [{ type: "text", text }],
    api: "acme-api", provider: "acme", model: "acme-1", usage, stopReason: "stop", timestamp: 0 });
  pi.registerProvider("acme", {
    api: "acme-api",
    baseUrl: "https://acme.test/v1",
    models: [{ id: "acme-1", name: "Acme One", reasoning: true, input: ["text"], contextWindow: 90000, maxTokens: 4096 }],
    streamSimple: async function* (model, context, options) {
      yield { type: "text_start", contentIndex: 0, partial: message("") };
      yield { type: "text_delta", contentIndex: 0,
              delta: `${model.id}|${context.messages[0].role}|${options.reasoning}|${options.apiKey}|${options.sessionId}`,
              partial: message("hi") };
      yield { type: "done", reason: "stop", message: message("done") };
    },
  });
}
"#;

    /// A handler that streams one event and then throws.
    const STREAM_EXTENSION_THROW: &str = r#"
export default function (pi) {
  const usage = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0,
                  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } };
  pi.registerProvider("acme", {
    api: "acme-api",
    baseUrl: "https://acme.test/v1",
    streamSimple: async function* () {
      yield { type: "text_delta", contentIndex: 0, delta: "first", partial: {
        role: "assistant", content: [], api: "acme-api", provider: "acme", model: "acme-1",
        usage, stopReason: "stop", timestamp: 0 } };
      throw new Error("stream-boom");
    },
  });
}
"#;

    #[cfg(feature = "ext-js")]
    struct StubStreamFn;

    #[cfg(feature = "ext-js")]
    impl pi_agent::types::StreamFn for StubStreamFn {
        fn stream(
            &self,
            _request: pi_agent::types::StreamRequest,
        ) -> futures::stream::BoxStream<'static, pi_agent::types::AssistantMessageEvent> {
            Box::pin(futures::stream::iter(Vec::new()))
        }
    }

    #[cfg(feature = "ext-js")]
    fn stream_runner(extension: &str) -> pi_ext_js::ExtensionRunner {
        let mut runner = pi_ext_js::ExtensionRunner::new(pi_ext_js::HostCore {
            cwd: "/tmp".to_string(),
            mode: "print".to_string(),
            settings: serde_json::json!({}),
            ..Default::default()
        })
        .expect("runner");
        runner
            .load_extension("<test>", Some(extension))
            .expect("load");
        runner.bind().expect("bind");
        runner
    }

    #[cfg(feature = "ext-js")]
    fn stream_request(api: &str) -> pi_agent::types::StreamRequest {
        pi_agent::types::StreamRequest {
            model: Model {
                provider: "acme".to_string(),
                id: "acme-1".to_string(),
                name: "Acme One".to_string(),
                api: api.to_string(),
                base_url: "https://acme.test/v1".to_string(),
                input: vec![pi_ai::types::ModelInputModality::Text],
                reasoning: true,
                context_window: 90_000,
                max_tokens: 4096,
                ..Model::default()
            },
            messages: vec![pi_agent::transcript::user_message("hi", 0)],
            thinking_level: pi_agent::types::ThinkingLevel::High,
            session_id: Some("s1".to_string()),
            api_key: Some("sk-acme".to_string()),
            access_token: None,
        }
    }

    #[cfg(feature = "ext-js")]
    fn collect(
        stream: futures::stream::BoxStream<'static, pi_agent::types::AssistantMessageEvent>,
    ) -> Vec<pi_agent::types::AssistantMessageEvent> {
        use futures::StreamExt;
        crate::runtime::block_on(stream.collect::<Vec<_>>())
    }

    #[test]
    #[cfg(feature = "ext-js")]
    fn stream_with_extension_replays_the_registered_handler() {
        let runner = Arc::new(std::sync::Mutex::new(stream_runner(STREAM_EXTENSION)));
        let events = collect(
            stream_with_extension(&runner, &stream_request("acme-api")).expect("extension arm"),
        );
        // start → delta → done, in emission order; the delta proves the
        // marshalled model/context/options reached the handler.
        assert_eq!(events.len(), 3);
        let pi_agent::types::AssistantMessageEvent::TextDelta { delta, .. } = &events[1] else {
            panic!("expected a text delta, got {:?}", events[1]);
        };
        assert_eq!(delta, "acme-1|user|high|sk-acme|s1");
        let pi_agent::types::AssistantMessageEvent::Done { message, .. } = &events[2] else {
            panic!("expected a done event, got {:?}", events[2]);
        };
        assert_eq!(message.model, "acme-1");
    }

    #[test]
    #[cfg(feature = "ext-js")]
    fn stream_with_extension_arms_decide_by_api_and_registration() {
        use pi_agent::types::StreamFn as _;

        let runner = Arc::new(std::sync::Mutex::new(stream_runner(STREAM_EXTENSION)));
        // The api is the streamWith gate: `model.api === extension.api`.
        assert!(stream_with_extension(&runner, &stream_request("other-api")).is_none());
        // No registered handler for the provider at all.
        let mut request = stream_request("acme-api");
        request.model.provider = "openai-codex".to_string();
        assert!(stream_with_extension(&runner, &request).is_none());
        // The composed dispatch consults the arm per request: a matching
        // api replays the handler, any other falls to the base adapter.
        let dispatch = ComposedStreamFn {
            runner: Arc::clone(&runner),
            fallback: Arc::new(StubStreamFn),
        };
        assert_eq!(
            collect(dispatch.stream(stream_request("acme-api"))).len(),
            3
        );
        assert_eq!(
            collect(dispatch.stream(stream_request("other-api"))).len(),
            0
        );
    }

    #[test]
    #[cfg(feature = "ext-js")]
    fn stream_with_extension_failure_appends_the_terminal_error_event() {
        let runner = Arc::new(std::sync::Mutex::new(stream_runner(STREAM_EXTENSION_THROW)));
        let events = collect(
            stream_with_extension(&runner, &stream_request("acme-api")).expect("extension arm"),
        );
        // The event emitted before the throw arrives, then lazyStream's
        // setup-failure shape: an Error event carrying the raw message.
        assert_eq!(events.len(), 2);
        let pi_agent::types::AssistantMessageEvent::TextDelta { delta, .. } = &events[0] else {
            panic!("expected the streamed delta, got {:?}", events[0]);
        };
        assert_eq!(delta, "first");
        let pi_agent::types::AssistantMessageEvent::Error { error, .. } = &events[1] else {
            panic!("expected the terminal error, got {:?}", events[1]);
        };
        assert_eq!(error.stop_reason, pi_ai::types::StopReason::Error);
        assert_eq!(error.error_message.as_deref(), Some("stream-boom"));
        assert_eq!(error.provider, "acme");
}

    /// The slice 6c config snapshot: the auth layer reads one provider's
    /// models.json entry from the last loaded config
    /// (`model-runtime.ts`'s `this.config`).
    #[test]
    fn provider_configs_snapshot_after_refresh() {
        let _guard = REBUILD_LOCK.lock().expect("lock");
        let path = temp_models_json(
            "snap",
            r#"{"providers": {"acme": {"name": "Acme", "apiKey": "$ACME_KEY",
                "baseUrl": "https://acme.local/v1", "api": "openai-completions",
                "models": [{"id": "acme-1", "name": "Acme One", "reasoning": true, "input": ["text"],
                            "contextWindow": 90000, "maxTokens": 4096}]}}}"#,
        );
        refresh_with_config(&ModelConfig::load(&path));
        let config = models_json_provider("acme").expect("snapshot");
        assert_eq!(config.name.as_deref(), Some("Acme"));
        assert_eq!(config.api_key.as_deref(), Some("$ACME_KEY"));
        assert!(models_json_provider("no-such-provider").is_none());
        let _ = std::fs::remove_file(&path);
    }
}
