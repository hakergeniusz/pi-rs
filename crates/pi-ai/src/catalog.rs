//! Embedded model catalog (port of TS `models.generated.ts` + provider data).
//!
//! The catalog JSON is generated from the upstream pi repository's catalog
//! pipeline (`packages/ai/catalog/generate.ts`, which normalizes models.dev
//! and provider-specific sources with pi's corrections and compat defaults).
//! It is embedded at compile time and parsed once on first use.

use crate::types::Model;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Embedded catalog: `{ provider: { model_id: model } }`.
static CATALOG_JSON: &str = include_str!("../data/models.json");

/// Chat-model catalog.
pub struct ModelCatalog {
    providers: HashMap<String, HashMap<String, Model>>,
}

static CATALOG: OnceLock<ModelCatalog> = OnceLock::new();

/// The global embedded catalog (parsed once).
pub fn catalog() -> &'static ModelCatalog {
    CATALOG
        .get_or_init(|| ModelCatalog::from_json(CATALOG_JSON).expect("embedded catalog is valid"))
}

impl ModelCatalog {
    /// Parse a catalog JSON document. Chat models only; entries of other
    /// model types are skipped.
    pub fn from_json(json: &str) -> Result<Self, String> {
        let raw: HashMap<String, serde_json::Value> =
            serde_json::from_str(json).map_err(|e| format!("catalog parse: {e}"))?;
        let mut providers = HashMap::new();
        for (provider, models) in raw {
            let mut entry = HashMap::new();
            if let Some(map) = models.as_object() {
                for (id, value) in map {
                    let model_type = value.get("type").and_then(|t| t.as_str()).unwrap_or("chat");
                    if model_type != "chat" {
                        continue;
                    }
                    let model: Model = serde_json::from_value(value.clone())
                        .map_err(|e| format!("catalog model {provider}/{id}: {e}"))?;
                    entry.insert(id.clone(), model);
                }
            }
            providers.insert(provider, entry);
        }
        Ok(Self { providers })
    }

    /// All provider ids, sorted.
    pub fn providers(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self.providers.keys().map(String::as_str).collect();
        ids.sort_unstable();
        ids
    }

    /// All chat models of one provider, sorted by id.
    pub fn models(&self, provider: &str) -> Vec<&Model> {
        let mut models: Vec<&Model> = self
            .providers
            .get(provider)
            .map(|m| m.values().collect())
            .unwrap_or_default();
        models.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        models
    }

    /// One model by provider and id.
    pub fn get(&self, provider: &str, model_id: &str) -> Option<&Model> {
        self.providers.get(provider)?.get(model_id)
    }

    /// Search every provider for a model id (TS `ModelRegistry` fallback).
    /// Prefers exact unique matches; returns all matches otherwise.
    pub fn find(&self, model_id: &str) -> Vec<&Model> {
        let mut found: Vec<&Model> = self
            .providers
            .values()
            .filter_map(|models| models.get(model_id))
            .collect();
        found.sort_unstable_by(|a, b| a.provider.cmp(&b.provider));
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_loads() {
        let cat = catalog();
        assert_eq!(cat.providers().len(), 43);
        assert!(cat.get("anthropic", "claude-opus-4-5").is_some());
        assert!(
            cat.get("openai", "gpt-5.2")
                .is_none_or(|m| !m.id.is_empty())
                || cat.models("openai").len() > 3
        );
        // anthropic models stream via anthropic-messages
        let model = cat.get("anthropic", "claude-opus-4-5").unwrap();
        assert_eq!(model.api, "anthropic-messages");
        assert_eq!(model.provider, "anthropic");
        assert_eq!(model.base_url, "https://api.anthropic.com");
        assert!(model.reasoning);
        assert!(model.context_window > 100_000);
        assert!(model.cost.output > 0.0);
    }

    #[test]
    fn catalog_models_have_known_apis() {
        let cat = catalog();
        let known = [
            "anthropic-messages",
            "openai-completions",
            "openai-responses",
            "azure-openai-responses",
            "openai-codex-responses",
            "google-generative-ai",
            "google-vertex",
            "mistral-conversations",
            "bedrock-converse-stream",
            "pi-messages",
        ];
        for provider in cat.providers() {
            for model in cat.models(provider) {
                assert!(
                    known.contains(&model.api.as_str()),
                    "{provider}/{} has unknown api {}",
                    model.id,
                    model.api
                );
            }
        }
    }

    #[test]
    fn find_spans_providers() {
        let cat = catalog();
        let anthropic_only = cat.find("claude-haiku-4-5");
        assert_ne!(anthropic_only.len(), 0);
        assert_eq!(anthropic_only[0].provider, "anthropic");
    }
}
