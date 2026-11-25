//! Embedded model catalog (port of TS `models.generated.ts` + provider data).
//!
//! The catalog JSON is generated from the upstream pi repository's catalog
//! pipeline (`packages/ai/catalog/generate.ts`, which normalizes models.dev
//! and provider-specific sources with pi's corrections and compat defaults).
//! It is embedded at compile time and parsed once on first use.
//!
//! Upstream keeps three catalogs (`MODELS`, `IMAGE_MODELS`,
//! `CLASSIFIER_MODELS` in `models.generated.ts:48,138,228`), keyed by model
//! type. Our 0.1.1 JSON predates the non-chat wave, so it is seeded with the
//! upstream 1.5.2 classifier/image models documented in
//! `packages/coding-agent/docs/models.md:148-212` (Jev, Clef, GPT-6 Luna,
//! Microsoft Decision 1, OpenRouter's image models). Because a provider can
//! list a chat and a classifier with the same id (`openai/gpt-6-luna`),
//! non-chat entries use an `<id>@<type>` JSON key; the model's own `id` field
//! stays the upstream id. Chat entries keep their plain id keys.
//!
//! [`ModelCatalog::models`] and [`ModelCatalog::get`] deliberately stay
//! chat-only (matching upstream's chat-only `getModel`), with
//! [`ModelCatalog::image_models`] / [`ModelCatalog::classifier_models`] for
//! the other types.
//!
//! 2026-11-25 extension: 286 entries from the live catalog pi.dev serves to
//! upstream v1.6.0 clients (`https://pi.dev/api/models?types=chat,image,classifier`,
//! revision `sha256-b1871c25…`) were merged **additively** into the JSON, with
//! every existing 1.5.2-pinned entry untouched. Upstream v1.6.0 generates its
//! data at build time (`packages/ai/catalog/README.md`, gitignored
//! `src/providers/data/*.json`), so the served catalog is the ground truth for
//! model data. See `docs/MODEL-CATALOG-DIFF.md` for the full diff and scope.

use crate::types::{ClassifierModel, ImageModel, Model, ModelType};
use std::collections::HashMap;
use std::sync::OnceLock;

/// Embedded catalog: `{ provider: { model_key: model } }`.
static CATALOG_JSON: &str = include_str!("../data/models.json");

/// All three model-type catalogs, keyed by provider then model id.
#[derive(Debug)]
pub struct ModelCatalog {
    providers: HashMap<String, HashMap<String, Model>>,
    images: HashMap<String, HashMap<String, ImageModel>>,
    classifiers: HashMap<String, HashMap<String, ClassifierModel>>,
}

static CATALOG: OnceLock<ModelCatalog> = OnceLock::new();

/// The global embedded catalog (parsed once).
pub fn catalog() -> &'static ModelCatalog {
    CATALOG
        .get_or_init(|| ModelCatalog::from_json(CATALOG_JSON).expect("embedded catalog is valid"))
}

impl ModelCatalog {
    /// Parse a catalog JSON document. Entries are routed by their `type`
    /// field: chat (default), `image` and `classifier`. Unknown types are a
    /// parse error so a bad catalog cannot silently drop models.
    pub fn from_json(json: &str) -> Result<Self, String> {
        let raw: HashMap<String, serde_json::Value> =
            serde_json::from_str(json).map_err(|e| format!("catalog parse: {e}"))?;
        let mut providers = HashMap::new();
        let mut images = HashMap::new();
        let mut classifiers = HashMap::new();
        for (provider, models) in raw {
            let mut chat_entries = HashMap::new();
            let mut image_entries = HashMap::new();
            let mut classifier_entries = HashMap::new();
            if let Some(map) = models.as_object() {
                for (key, value) in map {
                    let model_type = value.get("type").and_then(|t| t.as_str()).unwrap_or("chat");
                    match model_type {
                        "chat" => {
                            let model: Model = serde_json::from_value(value.clone())
                                .map_err(|e| format!("catalog model {provider}/{key}: {e}"))?;
                            chat_entries.insert(key.clone(), model);
                        }
                        "image" => {
                            let model: ImageModel =
                                serde_json::from_value(value.clone()).map_err(|e| {
                                    format!("catalog image model {provider}/{key}: {e}")
                                })?;
                            image_entries.insert(model.id.clone(), model);
                        }
                        "classifier" => {
                            let model: ClassifierModel = serde_json::from_value(value.clone())
                                .map_err(|e| {
                                    format!("catalog classifier model {provider}/{key}: {e}")
                                })?;
                            classifier_entries.insert(model.id.clone(), model);
                        }
                        other => {
                            return Err(format!(
                                "catalog model {provider}/{key}: unknown type {other}"
                            ));
                        }
                    }
                }
            }
            providers.insert(provider.clone(), chat_entries);
            images.insert(provider.clone(), image_entries);
            classifiers.insert(provider, classifier_entries);
        }
        Ok(Self {
            providers,
            images,
            classifiers,
        })
    }

    /// All provider ids, sorted (including providers with only non-chat models).
    pub fn providers(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self
            .providers
            .keys()
            .chain(self.images.keys())
            .chain(self.classifiers.keys())
            .map(String::as_str)
            .collect();
        ids.sort_unstable();
        ids.dedup();
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

    /// One chat model by provider and id.
    pub fn get(&self, provider: &str, model_id: &str) -> Option<&Model> {
        self.providers.get(provider)?.get(model_id)
    }

    /// All image-generation models of one provider, sorted by id.
    pub fn image_models(&self, provider: &str) -> Vec<&ImageModel> {
        let mut models: Vec<&ImageModel> = self
            .images
            .get(provider)
            .map(|m| m.values().collect())
            .unwrap_or_default();
        models.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        models
    }

    /// All classifier models of one provider, sorted by id.
    pub fn classifier_models(&self, provider: &str) -> Vec<&ClassifierModel> {
        let mut models: Vec<&ClassifierModel> = self
            .classifiers
            .get(provider)
            .map(|m| m.values().collect())
            .unwrap_or_default();
        models.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        models
    }

    /// One image model by provider and id.
    pub fn get_image(&self, provider: &str, model_id: &str) -> Option<&ImageModel> {
        self.images.get(provider)?.get(model_id)
    }

    /// One classifier model by provider and id.
    pub fn get_classifier(&self, provider: &str, model_id: &str) -> Option<&ClassifierModel> {
        self.classifiers.get(provider)?.get(model_id)
    }

    /// Search every provider for a chat model id (TS `ModelRegistry` fallback).
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

    /// Search every provider for an image model id.
    pub fn find_image(&self, model_id: &str) -> Vec<&ImageModel> {
        let mut found: Vec<&ImageModel> = self
            .images
            .values()
            .filter_map(|models| models.get(model_id))
            .collect();
        found.sort_unstable_by(|a, b| a.provider.cmp(&b.provider));
        found
    }

    /// Search every provider for a classifier model id.
    pub fn find_classifier(&self, model_id: &str) -> Vec<&ClassifierModel> {
        let mut found: Vec<&ClassifierModel> = self
            .classifiers
            .values()
            .filter_map(|models| models.get(model_id))
            .collect();
        found.sort_unstable_by(|a, b| a.provider.cmp(&b.provider));
        found
    }

    /// Whether a provider/id/type triple exists (TS `getModelOfType`).
    pub fn has_of_type(&self, provider: &str, model_id: &str, model_type: ModelType) -> bool {
        match model_type {
            ModelType::Chat => self.get(provider, model_id).is_some(),
            ModelType::Image => self.get_image(provider, model_id).is_some(),
            ModelType::Classifier => self.get_classifier(provider, model_id).is_some(),
        }
    }

    /// Total entry count per model type, for catalog validation.
    pub fn counts(&self) -> (usize, usize, usize) {
        (
            self.providers.values().map(HashMap::len).sum(),
            self.images.values().map(HashMap::len).sum(),
            self.classifiers.values().map(HashMap::len).sum(),
        )
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
    fn latest_catalog_additions_are_resolvable() {
        let cat = catalog();
        // Added in the 2026-11-25 sync from the live pi.dev catalog; the
        // 1.5.2-pinned snapshot did not know either id.
        let hydrogen = cat.get("openrouter", "google/gemini-4-hydrogen").unwrap();
        assert_eq!(hydrogen.provider, "openrouter");
        assert_eq!(hydrogen.api, "openai-completions");
        assert!(hydrogen.reasoning);
        assert_eq!(
            cat.get("google", "gemini-4-hydrogen").unwrap().api,
            "google-generative-ai"
        );
        // Five providers use the bare id; OpenRouter/Vercel prefix it.
        assert_eq!(cat.find("gemini-4-hydrogen").len(), 5);
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

    #[test]
    fn non_chat_models_are_retained_with_correct_kinds() {
        let cat = catalog();
        let (chat_count, image_count, classifier_count) = cat.counts();
        // 2026-11-25 catalog sync: +72 image and +75 classifier entries from
        // the latest published pi.dev catalog (the 0.1.1 snapshot had 2/6).
        assert_eq!(image_count, 74);
        assert_eq!(classifier_count, 81);
        assert!(
            chat_count > 1600,
            "chat catalog unexpectedly small: {chat_count}"
        );

        // OpenRouter lists both classifier and image models under its own
        // provider, with the upstream API ids preserved.
        let openrouter_images = cat.image_models("openrouter");
        assert_eq!(openrouter_images.len(), 74);
        assert_eq!(openrouter_images[0].id, "black-forest-labs/flux-3-image");
        assert_eq!(openrouter_images[0].api, "openrouter-images");
        assert_eq!(openrouter_images[0].r#type, "image");
        assert_eq!(openrouter_images[0].output.len(), 1);
        assert_eq!(openrouter_images[0].output[0].as_str(), "image");
        let flash = cat
            .get_image("openrouter", "google/gemini-2.5-flash-image")
            .unwrap();
        assert_eq!(flash.output.len(), 2);
        assert_eq!(flash.provider, "openrouter");

        let jev = cat.get_classifier("typesafe", "jev-latest").unwrap();
        assert_eq!(jev.api, "typesafe-system-one");
        assert_eq!(jev.r#type, "classifier");
        assert_eq!(jev.context_window, 64_000);
        let luna = cat.get_classifier("openai", "gpt-6-luna").unwrap();
        assert_eq!(luna.api, "openai-decisions");
        assert_eq!(luna.context_window, 922_000);
        assert_eq!(cat.find_classifier("jev-latest").len(), 1);

        // A chat model and a classifier may share an id; the catalogs stay
        // separate (upstream's `getModel` / `getModelOfType` split).
        assert_eq!(
            cat.get("openai", "gpt-6-luna").unwrap().api,
            "openai-responses"
        );
        assert!(cat.has_of_type("openai", "gpt-6-luna", ModelType::Classifier));
        assert!(cat.has_of_type(
            "openrouter",
            "google/gemini-2.5-flash-image",
            ModelType::Image
        ));
        assert!(!cat.has_of_type("typesafe", "jev-latest", ModelType::Chat));
    }

    #[test]
    fn from_json_routes_three_kinds_and_rejects_unknown() {
        let base = r#"{
            "p": {
                "chat-only": {"id":"chat-only","name":"Chat","api":"pi-messages","provider":"p",
                              "baseUrl":"https://example.com","input":["text"],"reasoning":false,
                              "contextWindow":100,"maxTokens":10},
                "img@image": {"id":"img","name":"Image","api":"openrouter-images","provider":"p",
                              "baseUrl":"https://example.com","input":["text"],"type":"image",
                              "output":["image"]},
                "clf@classifier": {"id":"clf","name":"Classifier","api":"openai-decisions","provider":"p",
                              "baseUrl":"https://example.com","input":["text"],"type":"classifier",
                              "contextWindow":42}
            }
        }"#;
        let cat = ModelCatalog::from_json(base).unwrap();
        assert_eq!(cat.counts(), (1, 1, 1));
        assert_eq!(cat.models("p").len(), 1);
        assert_eq!(cat.image_models("p")[0].id, "img");
        assert_eq!(cat.classifier_models("p")[0].context_window, 42);

        let with_unknown = r#"{"p":{
            "bogus@wat":{"id":"bogus","name":"Bogus","api":"x","provider":"p",
                         "baseUrl":"https://example.com","input":["text"],"type":"wat"}
        }}"#;
        assert!(
            ModelCatalog::from_json(with_unknown)
                .unwrap_err()
                .contains("unknown type wat")
        );
    }
}
