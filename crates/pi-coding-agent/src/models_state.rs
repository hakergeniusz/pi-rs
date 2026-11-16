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
//! Deferred to slices 6c/6d: auth composition (`composeApiKeyAuth`/
//! `composeOAuthAuth`) and `streamSimple` dispatch — the composition here is
//! the model-list layer only.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde_json::Value;

use pi_ai::types::Model;

use crate::config::get_models_path;
use crate::models_config::{self, ModelConfig};

static COMPOSED: RwLock<Option<Arc<Vec<Model>>>> = RwLock::new(None);
/// Composition errors (models.json / extension config problems), last
/// rebuild only. Upstream surfaces these through the registry's error state
/// (`ModelRegistry::getError`).
static ERRORS: RwLock<Vec<String>> = RwLock::new(Vec::new());
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
}
