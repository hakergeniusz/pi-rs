//! Provider and virtual-model registration: the session-scoped registry
//! behind `pi.registerProvider`, `pi.unregisterProvider`,
//! `pi.registerVirtualModel`, and `pi.unregisterVirtualModel`.
//!
//! Ports the registration rules of `core/extensions/loader.ts:468-524`
//! (assert-then-apply, config required when registering by name) and the
//! validation of `core/model-runtime.ts:924-1030` (non-empty ids,
//! merge-over-previous re-registration preserving undefined keys). The
//! physical-model conflict check for virtual models needs the model
//! catalog, so the host applies it when it consumes the registry — the
//! engine registry stores definitions verbatim. Routing and OAuth
//! callbacks stay JS functions the host invokes through the engine
//! (routing protocol lands with the model-runtime wiring).

use serde_json::{Map, Value};

/// One provider registered by name + config (`model-runtime.ts`
/// `extensionProviders`): re-registration merges defined values over the
/// previous config and preserves undefined ones.
#[derive(Debug, Clone)]
pub struct RegisteredProvider {
    /// The provider id.
    pub name: String,
    /// The extension that registered it (informational; unlike MCP there
    /// is no cross-extension ownership error).
    pub extension_path: String,
    /// The merged config (a JSON object).
    pub config: Value,
}

/// One native provider object registered whole
/// (`model-runtime.ts` `nativeExtensionProviders`).
#[derive(Debug, Clone)]
pub struct RegisteredNativeProvider {
    /// The provider object as registered (must carry a non-empty `id`).
    pub provider: Value,
    /// The extension that registered it.
    pub extension_path: String,
}

/// One virtual model definition (`model-runtime.ts` `virtualModels`).
#[derive(Debug, Clone)]
pub struct RegisteredVirtualModel {
    /// The definition as registered (carries `provider`, `id`, `route`).
    pub definition: Value,
    /// The extension that registered it.
    pub extension_path: String,
}

/// The registry the provider ops serve from.
#[derive(Debug, Default)]
pub struct ProviderRegistry {
    providers: Vec<RegisteredProvider>,
    native_providers: Vec<RegisteredNativeProvider>,
    virtual_models: Vec<RegisteredVirtualModel>,
}

impl ProviderRegistry {
    /// `runtime.registerProvider` (`model-runtime.ts:965-986`): validate
    /// standalone, then merge the config over any previous registration.
    pub fn register(
        &mut self,
        name: &str,
        extension_path: &str,
        config: &Value,
    ) -> Result<(), String> {
        let Some(config) = config.as_object() else {
            return Err("Provider config is required when registering by name".to_string());
        };
        // The merged config: previous first, defined values over it.
        let mut merged: Map<String, Value> = self
            .providers
            .iter()
            .find(|provider| provider.name == name)
            .and_then(|provider| provider.config.as_object().cloned())
            .unwrap_or_default();
        for (key, value) in config {
            merged.insert(key.clone(), value.clone());
        }
        match self
            .providers
            .iter_mut()
            .find(|provider| provider.name == name)
        {
            Some(existing) => {
                existing.config = Value::Object(merged);
            }
            None => self.providers.push(RegisteredProvider {
                name: name.to_string(),
                extension_path: extension_path.to_string(),
                config: Value::Object(merged),
            }),
        }
        Ok(())
    }

    /// `runtime.registerNativeProvider` (`model-runtime.ts:924-940`): a
    /// full provider object replaces any prior native registration with
    /// the same id.
    pub fn register_native(&mut self, provider: Value, extension_path: &str) -> Result<(), String> {
        let id = provider
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if id.trim().is_empty() {
            return Err("Provider id must not be empty.".to_string());
        }
        self.native_providers
            .retain(|existing| existing.provider.get("id").and_then(Value::as_str) != Some(id));
        self.native_providers.push(RegisteredNativeProvider {
            provider,
            extension_path: extension_path.to_string(),
        });
        Ok(())
    }

    /// `runtime.unregisterProvider` (`model-runtime.ts:988-994`): removes
    /// both registration kinds; no-op when absent.
    pub fn unregister(&mut self, name: &str) {
        self.providers.retain(|provider| provider.name != name);
        self.native_providers
            .retain(|existing| existing.provider.get("id").and_then(Value::as_str) != Some(name));
    }

    /// `runtime.registerVirtualModel` (`model-runtime.ts:1001-1021`):
    /// non-empty provider/id, same (provider, id) replaces. The
    /// physical-model conflict check runs host-side when the registry is
    /// applied to the catalog.
    pub fn register_virtual_model(
        &mut self,
        definition: Value,
        extension_path: &str,
    ) -> Result<(), String> {
        let provider = definition
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id = definition
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if provider.trim().is_empty() || id.trim().is_empty() {
            return Err("Virtual model provider and id must not be empty.".to_string());
        }
        self.virtual_models
            .retain(|existing| !Self::same_key(existing, provider, id));
        self.virtual_models.push(RegisteredVirtualModel {
            definition,
            extension_path: extension_path.to_string(),
        });
        Ok(())
    }

    /// `runtime.unregisterVirtualModel` (`model-runtime.ts:1022-1029`).
    pub fn unregister_virtual_model(&mut self, provider: &str, id: &str) {
        self.virtual_models
            .retain(|existing| !Self::same_key(existing, provider, id));
    }

    fn same_key(existing: &RegisteredVirtualModel, provider: &str, id: &str) -> bool {
        existing.definition.get("provider").and_then(Value::as_str) == Some(provider)
            && existing.definition.get("id").and_then(Value::as_str) == Some(id)
    }

    /// Current provider registrations, in registration order.
    #[must_use]
    pub fn providers(&self) -> &[RegisteredProvider] {
        &self.providers
    }

    /// Current native provider registrations, in registration order.
    #[must_use]
    pub fn native_providers(&self) -> &[RegisteredNativeProvider] {
        &self.native_providers
    }

    /// Current virtual-model definitions, in registration order.
    #[must_use]
    pub fn virtual_models(&self) -> &[RegisteredVirtualModel] {
        &self.virtual_models
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn register_merges_over_previous_and_preserves_undefined() {
        let mut registry = ProviderRegistry::default();
        registry
            .register(
                "acme",
                "<a>",
                &json!({ "baseUrl": "https://a", "api": "openai-completions" }),
            )
            .expect("first");
        registry
            .register("acme", "<b>", &json!({ "baseUrl": "https://b" }))
            .expect("second");
        let providers = registry.providers();
        assert_eq!(providers.len(), 1);
        assert_eq!(
            providers[0].config,
            json!({ "baseUrl": "https://b", "api": "openai-completions" })
        );
    }

    #[test]
    fn register_requires_a_config_object() {
        let mut registry = ProviderRegistry::default();
        let error = registry
            .register("acme", "<a>", &json!("nope"))
            .expect_err("rejected");
        assert_eq!(
            error,
            "Provider config is required when registering by name"
        );
    }

    #[test]
    fn native_provider_rejects_empty_id_and_replaces() {
        let mut registry = ProviderRegistry::default();
        assert_eq!(
            registry
                .register_native(json!({ "id": "  " }), "<a>")
                .expect_err("rejected"),
            "Provider id must not be empty."
        );
        registry
            .register_native(json!({ "id": "acme", "api": "openai-completions" }), "<a>")
            .expect("first");
        registry
            .register_native(json!({ "id": "acme", "api": "anthropic-messages" }), "<b>")
            .expect("second");
        assert_eq!(registry.native_providers().len(), 1);
        assert_eq!(
            registry.native_providers()[0].provider["api"],
            "anthropic-messages"
        );
    }

    #[test]
    fn unregister_removes_both_kinds() {
        let mut registry = ProviderRegistry::default();
        registry
            .register("acme", "<a>", &json!({ "baseUrl": "https://a" }))
            .expect("config");
        registry
            .register_native(json!({ "id": "acme" }), "<a>")
            .expect("native");
        registry.unregister("acme");
        assert!(registry.providers().is_empty());
        assert!(registry.native_providers().is_empty());
    }

    #[test]
    fn virtual_models_validate_and_replace_by_key() {
        let mut registry = ProviderRegistry::default();
        assert_eq!(
            registry
                .register_virtual_model(json!({ "provider": "acme", "id": "" }), "<a>")
                .expect_err("rejected"),
            "Virtual model provider and id must not be empty."
        );
        registry
            .register_virtual_model(
                json!({ "provider": "acme", "id": "router", "route": {} }),
                "<a>",
            )
            .expect("first");
        registry
            .register_virtual_model(
                json!({ "provider": "acme", "id": "router", "route": {} }),
                "<b>",
            )
            .expect("second");
        registry
            .register_virtual_model(
                json!({ "provider": "acme", "id": "other", "route": {} }),
                "<a>",
            )
            .expect("third");
        assert_eq!(registry.virtual_models().len(), 2);
        registry.unregister_virtual_model("acme", "router");
        assert_eq!(registry.virtual_models().len(), 1);
    }
}
