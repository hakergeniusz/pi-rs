//! models.json — the credential-blind provider/model configuration layer.
//!
//! Ports `core/model-config.ts` (schema shapes, the JSONC loader with
//! errors-as-values) and the models.json halves of `core/provider-composer.ts`
//! (`modelFromJson` `:212-251`, the merge helpers `:124-210`,
//! `applyModelOverride` `:179-210`, `applyModelsJson` `:302-344`). The
//! extension-composition half of the composer landed with slice 6b (see
//! docs/EXTENSIONS.md §8.1); slice 6c adds the `ExtensionOAuthConfig`
//! metadata subset ([`ExtensionOAuthMetadata`]) and
//! `configuredRequestAuthStatus` (ported in [`crate::oauth`]).
//!
//! Deviations (recorded per the fidelity rule):
//! - Upstream validates with TypeBox and surfaces TypeBox's message strings;
//!   the port runs the same semantic checks (`minLength` 1, `exclusiveMinimum`
//!   0, the `"radius"` oauth literal, required `providers`/`id`) and emits its
//!   own message text in the same `- path: message` layout.
//! - Upstream's `ModelCostSchema` requires the four rate fields; the port's
//!   serde `ModelCost` defaults them to 0, so a definition cost with missing
//!   rates loads as 0 instead of failing validation.
//! - Upstream strips `//` line comments and trailing commas only
//!   (`utils/json.ts:1-5`); the port's scanner does exactly the same two
//!   passes. `/* */` block comments are NOT stripped on either side.
//! - The port's catalog `Model` is chat-only (`pi-ai` `types.rs`), so the
//!   `isModelType(model, "chat")` branches collapse; the `type` field passes
//!   through as `None` (chat default).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use pi_ai::types::{
    JsonObject, Model, ModelCost, ModelImageInputLimits, ModelImageResizeOptions, ModelInputLimits,
    ModelInputModality, ModelPromptCache,
};

/// Compat keys whose objects merge nested instead of being replaced
/// (`provider-composer.ts:133`).
const NESTED_COMPAT_KEYS: [&str; 4] = [
    "openRouterRouting",
    "vercelGatewayRouting",
    "chatTemplateKwargs",
    "chatTemplateArgs",
];

/// Thinking levels `samplingParamsByThinkingLevel` merges over, in upstream's
/// order (`provider-composer.ts:172`).
const THINKING_LEVEL_KEYS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Strip a leading UTF-8 BOM (`utils/text.ts` `stripBom`).
fn strip_bom(input: &str) -> &str {
    input.strip_prefix('\u{feff}').unwrap_or(input)
}

/// `stripJsonComments` (`utils/json.ts:1-5`): strip `//` line comments and
/// trailing commas, leaving string literals untouched. `/* */` block comments
/// are deliberately not stripped (upstream does not either).
fn strip_json_comments(input: &str) -> String {
    let without_comments = strip_line_comments(input);
    strip_trailing_commas(&without_comments)
}

/// Pass 1: drop `//`-to-end-of-line outside string literals.
fn strip_line_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                chars.next();
                for skipped in chars.by_ref() {
                    if skipped == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Pass 2: drop commas directly before a closing `}` or `]` (whitespace
/// between allowed), outside string literals.
fn strip_trailing_commas(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            continue;
        }
        if c == ',' {
            let mut lookahead = chars.clone();
            let is_trailing = loop {
                match lookahead.next() {
                    Some(ws) if ws.is_whitespace() => {}
                    Some('}' | ']') => break true,
                    _ => break false,
                }
            };
            if is_trailing {
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// A model cost, possibly partial (`PartialModelCostSchema`,
/// `pi-ai-schemas.ts:35`): overrides merge field-by-field
/// (`provider-composer.ts:189-197`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostOverride {
    #[serde(default)]
    pub input: Option<f64>,
    #[serde(default)]
    pub output: Option<f64>,
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
    #[serde(default)]
    pub cache_write_1h: Option<f64>,
}

impl ModelCostOverride {
    fn into_cost(self) -> ModelCost {
        ModelCost {
            input: self.input.unwrap_or(0.0),
            output: self.output.unwrap_or(0.0),
            cache_read: self.cache_read.unwrap_or(0.0),
            cache_write: self.cache_write.unwrap_or(0.0),
            cache_write_1h: self.cache_write_1h,
            tiers: None,
        }
    }
}

/// A models.json model definition (`ModelDefinitionSchema`,
/// `model-config.ts:49-70`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsJsonModel {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub reasoning: Option<bool>,
    #[serde(default)]
    pub thinking_level_map: Option<JsonObject>,
    #[serde(default)]
    pub input: Option<Vec<ModelInputModality>>,
    #[serde(default)]
    pub input_limits: Option<ModelInputLimits>,
    #[serde(default)]
    pub cost: Option<ModelCostOverride>,
    #[serde(default)]
    pub prompt_cache: Option<ModelPromptCache>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub sampling_params: Option<JsonObject>,
    #[serde(default)]
    pub sampling_params_by_thinking_level: Option<JsonObject>,
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub compat: Option<JsonObject>,
}

/// A models.json model override (`ModelOverrideSchema`,
/// `model-config.ts:72-88`): every field optional, merged over the model.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsJsonModelOverride {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub reasoning: Option<bool>,
    #[serde(default)]
    pub thinking_level_map: Option<JsonObject>,
    #[serde(default)]
    pub input: Option<Vec<ModelInputModality>>,
    #[serde(default)]
    pub input_limits: Option<ModelInputLimits>,
    #[serde(default)]
    pub cost: Option<ModelCostOverride>,
    #[serde(default)]
    pub prompt_cache: Option<ModelPromptCache>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub sampling_params: Option<JsonObject>,
    #[serde(default)]
    pub sampling_params_by_thinking_level: Option<JsonObject>,
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub compat: Option<JsonObject>,
}

/// A models.json provider entry (`ProviderConfigSchema`,
/// `model-config.ts:90-116`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsJsonProvider {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    /// The only accepted value is `"radius"` (`Type.Literal("radius")`).
    #[serde(default)]
    pub oauth: Option<String>,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub compat: Option<JsonObject>,
    #[serde(default)]
    pub auth_header: Option<bool>,
    #[serde(default)]
    pub models: Option<Vec<ModelsJsonModel>>,
    #[serde(default)]
    pub model_overrides: Option<BTreeMap<String, ModelsJsonModelOverride>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelsConfigFile {
    #[serde(rename = "$schema", default)]
    #[allow(dead_code)] // consumed by the schema pass-through, never read
    schema: Option<String>,
    #[serde(default)]
    providers: Option<BTreeMap<String, ModelsJsonProvider>>,
}

/// One immutable load of models.json (`ModelConfig`, `model-config.ts:160-220`).
/// Parse and validation problems are VALUES (`error`), never throws; a
/// missing file is an empty config with no error.
pub struct ModelConfig {
    providers: BTreeMap<String, ModelsJsonProvider>,
    error: Option<String>,
}

impl ModelConfig {
    /// `ModelConfig.load` (`model-config.ts:169-207`).
    pub fn load(path: &Path) -> ModelConfig {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return ModelConfig {
                    providers: BTreeMap::new(),
                    error: None,
                };
            }
            Err(error) => {
                return ModelConfig {
                    providers: BTreeMap::new(),
                    error: Some(format!(
                        "Failed to load models.json: {error}\n\nFile: {}",
                        path.display()
                    )),
                };
            }
        };

        let stripped = strip_json_comments(strip_bom(&content));
        let parsed: Result<ModelsConfigFile, _> = serde_json::from_str(&stripped);
        let parsed = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                return ModelConfig {
                    providers: BTreeMap::new(),
                    error: Some(format!(
                        "Failed to parse models.json: {error}\n\nFile: {}",
                        path.display()
                    )),
                };
            }
        };

        let Some(providers) = parsed.providers else {
            return ModelConfig {
                providers: BTreeMap::new(),
                error: Some(format!(
                    "Invalid models.json schema:\n  - providers: is required\n\nFile: {}",
                    path.display()
                )),
            };
        };

        let problems = providers
            .iter()
            .flat_map(|(id, provider)| validate_provider(id, provider))
            .collect::<Vec<_>>();
        if !problems.is_empty() {
            return ModelConfig {
                providers: BTreeMap::new(),
                error: Some(format!(
                    "Invalid models.json schema:\n{}\n\nFile: {}",
                    problems
                        .into_iter()
                        .map(|problem| format!("  - {problem}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    path.display()
                )),
            };
        }

        ModelConfig {
            providers,
            error: None,
        }
    }

    pub fn get_provider(&self, provider_id: &str) -> Option<&ModelsJsonProvider> {
        self.providers.get(provider_id)
    }

    pub fn get_provider_ids(&self) -> Vec<&str> {
        self.providers.keys().map(String::as_str).collect()
    }

    /// The whole provider map (the compose/auth layers read individual
    /// entries; slice 6c keeps a snapshot for the credential ladder).
    pub fn providers(&self) -> &BTreeMap<String, ModelsJsonProvider> {
        &self.providers
    }

    pub fn get_error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// The semantic checks the `TypeBox` schema enforces (`model-config.ts:49-125`),
/// reported in the `- path: message` layout of `formatValidationPath`
/// (`model-config.ts:140-151`).
fn validate_provider(id: &str, provider: &ModelsJsonProvider) -> Vec<String> {
    let mut problems = Vec::new();
    let base = format!("providers.{id}");
    for (field, value) in [
        ("name", &provider.name),
        ("baseUrl", &provider.base_url),
        ("apiKey", &provider.api_key),
        ("api", &provider.api),
    ] {
        if value.as_ref().is_some_and(String::is_empty) {
            problems.push(format!("{base}.{field}: must not be empty"));
        }
    }
    if provider
        .oauth
        .as_ref()
        .is_some_and(|oauth| oauth != "radius")
    {
        problems.push(format!("{base}.oauth: must be \"radius\""));
    }
    for (index, model) in provider.models.iter().flatten().enumerate() {
        let base = format!("{base}.models.{index}");
        if model.id.is_empty() {
            problems.push(format!("{base}.id: must not be empty"));
        }
        for (field, value) in [
            ("name", &model.name),
            ("api", &model.api),
            ("baseUrl", &model.base_url),
        ] {
            if value.as_ref().is_some_and(String::is_empty) {
                problems.push(format!("{base}.{field}: must not be empty"));
            }
        }
        for (field, value) in [
            ("contextWindow", model.context_window),
            ("maxTokens", model.max_tokens),
        ] {
            if value.is_some_and(|value| value == 0) {
                problems.push(format!("{base}.{field}: must be greater than 0"));
            }
        }
    }
    for (model_id, override_value) in provider.model_overrides.iter().flatten() {
        let base = format!("{base}.modelOverrides.{model_id}");
        if override_value.name.as_ref().is_some_and(String::is_empty) {
            problems.push(format!("{base}.name: must not be empty"));
        }
        for (field, value) in [
            ("contextWindow", override_value.context_window),
            ("maxTokens", override_value.max_tokens),
        ] {
            if value.is_some_and(|value| value == 0) {
                problems.push(format!("{base}.{field}: must be greater than 0"));
            }
        }
    }
    problems
}

/// `mergeCompat` (`provider-composer.ts:124-144`): shallow merge, with the
/// four routing keys merging as nested objects.
fn merge_compat(base: Option<JsonObject>, r#override: Option<JsonObject>) -> Option<JsonObject> {
    let Some(r#override) = r#override else {
        return base;
    };
    let mut merged = base.unwrap_or_default();
    for (key, value) in r#override {
        if NESTED_COMPAT_KEYS.contains(&key.as_str())
            && let (Some(Value::Object(nested)), Value::Object(incoming)) =
                (merged.get(&key).cloned(), &value)
        {
            let mut nested = nested;
            for (nested_key, nested_value) in incoming {
                nested.insert(nested_key.clone(), nested_value.clone());
            }
            merged.insert(key, Value::Object(nested));
            continue;
        }
        merged.insert(key, value);
    }
    Some(merged)
}

/// `mergeInputLimits` (`provider-composer.ts:146-164`): per-field merge with
/// the `images`/`images.resize` subtrees merged nested.
fn merge_input_limits(
    base: Option<ModelInputLimits>,
    r#override: Option<ModelInputLimits>,
) -> Option<ModelInputLimits> {
    let Some(r#override) = r#override else {
        return base;
    };
    let Some(base) = base else {
        return Some(r#override);
    };
    let images = match (base.images, r#override.images) {
        (Some(base), Some(incoming)) => Some(ModelImageInputLimits {
            resize: match (base.resize, incoming.resize) {
                (Some(base), Some(incoming)) => Some(ModelImageResizeOptions {
                    max_width: incoming.max_width.or(base.max_width),
                    max_height: incoming.max_height.or(base.max_height),
                    max_bytes: incoming.max_bytes.or(base.max_bytes),
                    jpeg_quality: incoming.jpeg_quality.or(base.jpeg_quality),
                }),
                (base, incoming) => incoming.or(base),
            },
            max_per_message: incoming.max_per_message.or(base.max_per_message),
            max_per_request: incoming.max_per_request.or(base.max_per_request),
        }),
        (base, incoming) => incoming.or(base),
    };
    Some(ModelInputLimits {
        max_request_bytes: r#override.max_request_bytes.or(base.max_request_bytes),
        images,
    })
}

/// `mergeSamplingParamsByThinkingLevel` (`provider-composer.ts:166-177`):
/// shallow merge per level key.
fn merge_sampling_params_by_thinking_level(
    base: Option<JsonObject>,
    r#override: Option<JsonObject>,
) -> Option<JsonObject> {
    let Some(r#override) = r#override else {
        return base;
    };
    let mut merged = base.unwrap_or_default();
    for level in THINKING_LEVEL_KEYS {
        if let Some(params) = r#override.get(level) {
            let combined = match merged.get(level) {
                Some(Value::Object(base_params)) => {
                    let mut combined = base_params.clone();
                    if let Value::Object(incoming) = params {
                        for (key, value) in incoming {
                            combined.insert(key.clone(), value.clone());
                        }
                    }
                    Value::Object(combined)
                }
                _ => params.clone(),
            };
            merged.insert(level.to_string(), combined);
        }
    }
    Some(merged)
}

fn merge_prompt_cache(
    base: Option<ModelPromptCache>,
    r#override: Option<ModelPromptCache>,
) -> Option<ModelPromptCache> {
    let Some(r#override) = r#override else {
        return base;
    };
    let Some(base) = base else {
        return Some(r#override);
    };
    Some(ModelPromptCache {
        short: r#override.short.or(base.short),
        long: r#override.long.or(base.long),
    })
}

/// `applyModelOverride` (`provider-composer.ts:179-210`): every present
/// override field replaces or merges into the model.
pub fn apply_model_override(model: &Model, r#override: &ModelsJsonModelOverride) -> Model {
    Model {
        name: r#override
            .name
            .clone()
            .unwrap_or_else(|| model.name.clone()),
        reasoning: r#override.reasoning.unwrap_or(model.reasoning),
        thinking_level_map: merge_json_objects(
            model.thinking_level_map.as_ref(),
            r#override.thinking_level_map.as_ref(),
        ),
        input: r#override
            .input
            .clone()
            .unwrap_or_else(|| model.input.clone()),
        input_limits: merge_input_limits(model.input_limits, r#override.input_limits),
        cost: merge_cost(&model.cost, r#override.cost.as_ref()),
        prompt_cache: merge_prompt_cache(model.prompt_cache, r#override.prompt_cache),
        context_window: r#override.context_window.unwrap_or(model.context_window),
        max_tokens: r#override.max_tokens.unwrap_or(model.max_tokens),
        sampling_params: merge_json_objects(
            model.sampling_params.as_ref(),
            r#override.sampling_params.as_ref(),
        ),
        sampling_params_by_thinking_level: merge_sampling_params_by_thinking_level(
            model.sampling_params_by_thinking_level.clone(),
            r#override.sampling_params_by_thinking_level.clone(),
        ),
        compat: merge_compat(model.compat.clone(), r#override.compat.clone()),
        ..model.clone()
    }
}

fn merge_json_objects(
    base: Option<&JsonObject>,
    r#override: Option<&JsonObject>,
) -> Option<JsonObject> {
    match (base, r#override) {
        (None, None) => None,
        (base, r#override) => {
            let mut merged = base.cloned().unwrap_or_default();
            if let Some(r#override) = r#override {
                for (key, value) in r#override {
                    merged.insert(key.clone(), value.clone());
                }
            }
            Some(merged)
        }
    }
}

fn merge_cost(base: &ModelCost, r#override: Option<&ModelCostOverride>) -> ModelCost {
    let Some(r#override) = r#override else {
        return base.clone();
    };
    ModelCost {
        input: r#override.input.unwrap_or(base.input),
        output: r#override.output.unwrap_or(base.output),
        cache_read: r#override.cache_read.unwrap_or(base.cache_read),
        cache_write: r#override.cache_write.unwrap_or(base.cache_write),
        cache_write_1h: match r#override.cache_write_1h {
            Some(value) => Some(value),
            None => base.cache_write_1h,
        },
        // Overrides never replace catalog tiers (`pi-ai-schemas.ts:35`).
        tiers: base.tiers.clone(),
    }
}

/// `findModelDefaults` (`provider-composer.ts:253-261`): same-id → same-api →
/// openai-completions → first.
fn find_model_defaults(models: &[Model], model_id: &str, api: Option<&str>) -> Option<Model> {
    models
        .iter()
        .find(|model| model.id == model_id)
        .or_else(|| api.and_then(|api| models.iter().find(|model| model.api == api)))
        .or_else(|| {
            models
                .iter()
                .find(|model| model.api == "openai-completions")
        })
        .or_else(|| models.first())
        .cloned()
}

/// `modelFromJson` (`provider-composer.ts:212-251`): build a [`Model`] from a
/// definition, defaulting `api`/`baseUrl` from the provider entry and the
/// closest existing model, with upstream's verbatim validation errors.
pub fn model_from_json(
    provider_id: &str,
    definition: &ModelsJsonModel,
    provider_config: &ModelsJsonProvider,
    defaults: Option<&Model>,
) -> Result<Model, String> {
    let api = definition
        .api
        .clone()
        .or_else(|| provider_config.api.clone())
        .or_else(|| defaults.map(|model| model.api.clone()));
    let Some(api) = api else {
        return Err(format!(
            "Provider {provider_id}, model {}: no \"api\" specified. Set at provider or model level.",
            definition.id
        ));
    };
    let base_url = definition
        .base_url
        .clone()
        .or_else(|| provider_config.base_url.clone())
        .or_else(|| defaults.map(|model| model.base_url.clone()));
    let Some(base_url) = base_url else {
        return Err(format!(
            "Provider {provider_id}: \"baseUrl\" is required when defining custom models."
        ));
    };
    if definition.context_window.is_some_and(|value| value == 0) {
        return Err(format!(
            "Provider {provider_id}, model {}: invalid contextWindow",
            definition.id
        ));
    }
    if definition.max_tokens.is_some_and(|value| value == 0) {
        return Err(format!(
            "Provider {provider_id}, model {}: invalid maxTokens",
            definition.id
        ));
    }
    Ok(Model {
        id: definition.id.clone(),
        name: definition
            .name
            .clone()
            .unwrap_or_else(|| definition.id.clone()),
        api,
        provider: provider_id.to_string(),
        base_url,
        input: definition
            .input
            .clone()
            .unwrap_or_else(|| vec![ModelInputModality::Text]),
        input_limits: definition.input_limits,
        cost: definition
            .cost
            .map(ModelCostOverride::into_cost)
            .unwrap_or_default(),
        headers: None,
        r#type: None,
        reasoning: definition.reasoning.unwrap_or(false),
        thinking_level_map: definition.thinking_level_map.clone(),
        prompt_cache: definition.prompt_cache,
        context_window: definition.context_window.unwrap_or(128_000),
        max_tokens: definition.max_tokens.unwrap_or(16_384),
        sampling_params: definition.sampling_params.clone(),
        sampling_params_by_thinking_level: definition.sampling_params_by_thinking_level.clone(),
        compat: merge_compat(provider_config.compat.clone(), definition.compat.clone()),
    })
}

/// `applyModelsJson` (`provider-composer.ts:302-344`): apply one provider's
/// models.json entry over the built-in models — baseUrl/compat overrides,
/// then model upserts. Returns the verbatim validation errors as `Err`.
pub fn apply_models_json(
    provider_id: &str,
    base_models: &[Model],
    config: Option<&ModelsJsonProvider>,
) -> Result<Vec<Model>, String> {
    let Some(config) = config else {
        return Ok(base_models.to_vec());
    };
    if config.oauth.is_some() && config.base_url.is_none() {
        return Err(format!(
            "Provider {provider_id}: \"baseUrl\" is required when \"oauth\" is set."
        ));
    }
    let has_overrides = config
        .model_overrides
        .as_ref()
        .is_some_and(|overrides| !overrides.is_empty());
    if config.models.as_ref().is_none_or(Vec::is_empty)
        && config.base_url.is_none()
        && config.headers.is_none()
        && config.compat.is_none()
        && !has_overrides
        && config.api_key.is_none()
        && config.oauth.is_none()
        && config.auth_header.is_none()
    {
        return Err(format!(
            "Provider {provider_id}: must specify \"baseUrl\", \"headers\", \"compat\", \"modelOverrides\", or \"models\"."
        ));
    }

    let keep_base_url = config.oauth.as_deref() == Some("radius");
    let mut models: Vec<Model> = base_models
        .iter()
        .map(|model| Model {
            base_url: if keep_base_url {
                model.base_url.clone()
            } else {
                config
                    .base_url
                    .clone()
                    .unwrap_or_else(|| model.base_url.clone())
            },
            compat: merge_compat(model.compat.clone(), config.compat.clone()),
            ..model.clone()
        })
        .collect();
    for definition in config.models.iter().flatten() {
        let existing = models.iter().position(|model| {
            model.id == definition.id && model.r#type.as_ref().is_none_or(|t| t == "chat")
        });
        let defaults = find_model_defaults(
            &models,
            &definition.id,
            definition.api.as_deref().or(config.api.as_deref()),
        );
        let model = model_from_json(provider_id, definition, config, defaults.as_ref())?;
        match existing {
            Some(index) => models[index] = model,
            None => models.push(model),
        }
    }
    Ok(models)
}

/// One extension-registered chat model definition (`ProviderChatModelConfig`,
/// `provider-composer.ts:64-75`; the base fields `:53-62`). The TS type
/// requires `name`/`input`/`cost`/`reasoning`/`contextWindow`/`maxTokens`, so
/// the port makes them non-optional — a definition missing them fails the
/// registration's load-time report where TS would carry `undefined` through.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionChatModelConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    pub input: Vec<ModelInputModality>,
    #[serde(default)]
    pub input_limits: Option<ModelInputLimits>,
    #[serde(default)]
    pub cost: ModelCost,
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub thinking_level_map: Option<JsonObject>,
    #[serde(default)]
    pub prompt_cache: Option<ModelPromptCache>,
    pub context_window: u64,
    pub max_tokens: u64,
    #[serde(default)]
    pub sampling_params: Option<JsonObject>,
    #[serde(default)]
    pub sampling_params_by_thinking_level: Option<JsonObject>,
    #[serde(default)]
    pub compat: Option<JsonObject>,
}

/// The metadata subset of an extension `ExtensionOAuthConfig`
/// (`provider-composer.ts:41-51`). The `login`/`refreshToken`/`getApiKey`/
/// `modifyModels` members are JS functions the JSON bridge drops — only the
/// data rides the registry (`ExtensionOAuthAuthInfo`, slice 6c).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionOAuthMetadata {
    /// Display name (`ExtensionOAuthConfig.name`, required upstream).
    pub name: String,
    /// Whether access through this auth method is subscription-backed.
    #[serde(default)]
    pub is_subscription: bool,
    /// Retained upstream for extension source compatibility; ignored by the
    /// canonical auth flows (`provider-composer.ts:45-46`).
    #[serde(default)]
    pub uses_callback_server: Option<bool>,
}

/// The model-composition subset of an extension `ProviderConfigInput`
/// (`provider-composer.ts:93-110`). The `streamSimple` *function* is
/// stashed JS-side and crosses here only as a `streamSimple: true`
/// presence marker (slice 6d — the dispatch reads the marker through the
/// runner's registry). The `oauth` member carries its metadata subset
/// only (slice 6c); `images`/`classifiers`/`refreshModels` remain
/// functions or per-operation objects the JSON bridge drops or defers.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionProviderConfig {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub auth_header: Option<bool>,
    /// The `ExtensionOAuthConfig` metadata subset (`provider-composer.ts:41-51`).
    #[serde(default)]
    pub oauth: Option<ExtensionOAuthMetadata>,
    #[serde(default)]
    pub models: Option<Vec<ExtensionChatModelConfig>>,
}

impl ExtensionProviderConfig {
    /// Parse a registry config object; the registry already guarantees an
    /// object (`ProviderRegistry::register`).
    pub fn from_config(config: &serde_json::Value) -> Result<Self, String> {
        serde_json::from_value(config.clone())
            .map_err(|error| format!("Invalid provider config: {error}"))
    }
}

/// `extensionModelFromDefinition` (`provider-composer.ts:277-300`): defaults
/// `api`/`baseUrl` from same-id → same-api → openai-completions → first
/// (`findExtensionModelDefaults` `:263-275`, reusing [`find_model_defaults`]
/// — the port's models are chat-only). Verbatim errors, chat variant.
pub fn extension_model_from_definition(
    provider_id: &str,
    config: &ExtensionProviderConfig,
    definition: &ExtensionChatModelConfig,
    defaults: Option<&Model>,
) -> Result<Model, String> {
    let api = definition
        .api
        .clone()
        .or_else(|| config.api.clone())
        .or_else(|| defaults.map(|model| model.api.clone()));
    let Some(api) = api else {
        return Err(format!(
            "Provider {provider_id}, model {}: no \"api\" specified. Set it at model level or provider level.",
            definition.id
        ));
    };
    let base_url = definition
        .base_url
        .clone()
        .or_else(|| config.base_url.clone())
        .or_else(|| defaults.map(|model| model.base_url.clone()));
    let Some(base_url) = base_url else {
        return Err(format!(
            "Provider {provider_id}: \"baseUrl\" is required when defining custom models."
        ));
    };
    Ok(Model {
        id: definition.id.clone(),
        name: definition.name.clone(),
        api,
        provider: provider_id.to_string(),
        base_url,
        input: definition.input.clone(),
        input_limits: definition.input_limits,
        cost: definition.cost.clone(),
        headers: None,
        r#type: None,
        reasoning: definition.reasoning,
        thinking_level_map: definition.thinking_level_map.clone(),
        prompt_cache: definition.prompt_cache,
        context_window: definition.context_window,
        max_tokens: definition.max_tokens,
        sampling_params: definition.sampling_params.clone(),
        sampling_params_by_thinking_level: definition.sampling_params_by_thinking_level.clone(),
        compat: definition.compat.clone(),
    })
}

/// `applyExtension` (`provider-composer.ts:346-356`): an extension config
/// either overrides the `baseUrl` of every existing model or replaces the
/// provider's models with its definitions.
pub fn apply_extension(
    provider_id: &str,
    models: &[Model],
    config: Option<&ExtensionProviderConfig>,
) -> Result<Vec<Model>, String> {
    let Some(config) = config else {
        return Ok(models.to_vec());
    };
    match &config.models {
        None => {
            if let Some(base_url) = &config.base_url {
                Ok(models
                    .iter()
                    .map(|model| Model {
                        base_url: base_url.clone(),
                        ..model.clone()
                    })
                    .collect())
            } else {
                Ok(models.to_vec())
            }
        }
        Some(definitions) => definitions
            .iter()
            .map(|definition| {
                let defaults =
                    find_model_defaults(models, &definition.id, definition.api.as_deref());
                extension_model_from_definition(provider_id, config, definition, defaults.as_ref())
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Write `content` to a unique temp file and return its path.
    fn temp_models_json(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("pi-rs-models-config-tests");
        fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join(format!("{}-{name}.json", std::process::id()));
        fs::write(&path, content).expect("write temp models.json");
        path
    }

    const BASE_MODEL_JSON: &str = r#"{
        "id": "builtin-a", "name": "Built-in A", "api": "openai-completions",
        "provider": "acme", "baseUrl": "https://api.acme.com/v1",
        "reasoning": true, "input": ["text"],
        "cost": {"input": 1.0, "output": 2.0, "cacheRead": 0.1, "cacheWrite": 0.2},
        "contextWindow": 100000, "maxTokens": 8192
    }"#;

    fn base_model() -> Model {
        serde_json::from_str::<Model>(BASE_MODEL_JSON).expect("parse base model")
    }

    #[test]
    fn apply_extension_overrides_base_urls_without_models() {
        let config = ExtensionProviderConfig {
            base_url: Some("https://proxy.local/v1".to_string()),
            ..Default::default()
        };
        let models = apply_extension("acme", &[base_model()], Some(&config)).expect("override");
        assert_eq!(models[0].base_url, "https://proxy.local/v1");
        assert_eq!(models[0].id, "builtin-a"); // model object otherwise kept
        assert_eq!(
            apply_extension("acme", &[base_model()], None).expect("no config"),
            vec![base_model()]
        );
        let empty = ExtensionProviderConfig::default();
        assert_eq!(
            apply_extension("acme", &[base_model()], Some(&empty)).expect("no baseUrl"),
            vec![base_model()]
        );
    }

    #[test]
    fn apply_extension_replaces_models_with_definitions() {
        let config = ExtensionProviderConfig {
            api: Some("openai-completions".to_string()),
            base_url: Some("https://ext.local/v1".to_string()),
            models: Some(vec![ExtensionChatModelConfig {
                id: "ext-a".to_string(),
                name: "Ext A".to_string(),
                input: vec![ModelInputModality::Text],
                cost: ModelCost::default(),
                reasoning: true,
                context_window: 200_000,
                max_tokens: 8_192,
                thinking_level_map: Some(
                    serde_json::json!({"off": "off"})
                        .as_object()
                        .cloned()
                        .unwrap(),
                ),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let models = apply_extension("acme", &[base_model()], Some(&config)).expect("replace");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "ext-a");
        assert_eq!(models[0].provider, "acme");
        assert_eq!(models[0].base_url, "https://ext.local/v1"); // config default
        assert_eq!(models[0].api, "openai-completions"); // config default
        assert_eq!(models[0].headers, None); // definitions never carry headers
        assert_eq!(models[0].context_window, 200_000); // no schema defaults here
    }

    #[test]
    fn extension_definition_errors_are_verbatim() {
        let config = ExtensionProviderConfig::default();
        let definition = ExtensionChatModelConfig {
            id: "ext-a".to_string(),
            name: "Ext A".to_string(),
            input: vec![ModelInputModality::Text],
            cost: ModelCost::default(),
            reasoning: false,
            context_window: 1,
            max_tokens: 1,
            ..Default::default()
        };
        let error = extension_model_from_definition("acme", &config, &definition, None)
            .expect_err("no api");
        assert_eq!(
            error,
            "Provider acme, model ext-a: no \"api\" specified. Set it at model level or provider level."
        );
        let with_api = ExtensionProviderConfig {
            api: Some("openai-completions".to_string()),
            ..Default::default()
        };
        let error = extension_model_from_definition("acme", &with_api, &definition, None)
            .expect_err("no baseUrl");
        assert_eq!(
            error,
            "Provider acme: \"baseUrl\" is required when defining custom models."
        );
    }

    #[test]
    fn missing_file_is_an_empty_config_without_error() {
        let config = ModelConfig::load(Path::new("/nonexistent/pi-rs/models.json"));
        assert_eq!(config.get_provider_ids().len(), 0);
        assert_eq!(config.get_error(), None);
    }

    #[test]
    fn load_reads_providers_and_strips_comments_and_bom() {
        let path = temp_models_json(
            "comments",
            "\u{feff}{\n// top comment\n  \"providers\": {\n    \"proxy\": {\n      \"baseUrl\": \"https://proxy.local/v1\", // inline\n      \"apiKey\": \"$PROXY_KEY\",\n      \"api\": \"openai-completions\",\n      \"models\": [{\"id\": \"m1\", \"name\": \"M1\", \"reasoning\": false, \"input\": [\"text\"], \"contextWindow\": 200000, \"maxTokens\": 4096,}]\n    },\n  }\n}\n",
        );
        let config = ModelConfig::load(&path);
        assert_eq!(config.get_error(), None);
        let provider = config.get_provider("proxy").expect("proxy provider");
        assert_eq!(provider.base_url.as_deref(), Some("https://proxy.local/v1"));
        assert_eq!(provider.api_key.as_deref(), Some("$PROXY_KEY"));
        let models = provider.models.as_ref().expect("models");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].context_window, Some(200_000));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn load_reports_parse_and_schema_errors_as_values() {
        let broken = temp_models_json("broken", "{ \"providers\": { \"p\": ");
        let config = ModelConfig::load(&broken);
        let error = config.get_error().expect("parse error");
        assert!(
            error.starts_with("Failed to parse models.json: "),
            "{error}"
        );
        assert!(error.contains("File: "), "{error}");
        let _ = fs::remove_file(&broken);

        let invalid = temp_models_json(
            "invalid",
            "{ \"providers\": { \"p\": { \"models\": [{\"id\": \"\", \"contextWindow\": 0}] } } }",
        );
        let config = ModelConfig::load(&invalid);
        let error = config.get_error().expect("schema error");
        assert!(
            error.starts_with("Invalid models.json schema:\n  - "),
            "{error}"
        );
        assert!(
            error.contains("providers.p.models.0.id: must not be empty"),
            "{error}"
        );
        assert!(
            error.contains("providers.p.models.0.contextWindow: must be greater than 0"),
            "{error}"
        );
        let _ = fs::remove_file(&invalid);

        let missing = temp_models_json("missing-providers", "{}");
        let config = ModelConfig::load(&missing);
        assert_eq!(
            config.get_error(),
            Some(
                "Invalid models.json schema:\n  - providers: is required\n\nFile: ".to_string()
                    + &missing.display().to_string(),
            )
            .as_deref()
        );
        let _ = fs::remove_file(&missing);
    }

    #[test]
    fn apply_models_json_upserts_and_overrides_base_models() {
        let base = base_model();
        let config = ModelsJsonProvider {
            base_url: Some("https://proxy.local/v1".to_string()),
            compat: Some(
                serde_json::json!({"useTools": true})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
            models: Some(vec![ModelsJsonModel {
                id: "builtin-a".to_string(),
                name: Some("Built-in A (proxied)".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let models = apply_models_json("acme", &[base], Some(&config)).expect("apply");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].base_url, "https://proxy.local/v1");
        assert_eq!(models[0].name, "Built-in A (proxied)");
        // `defaults` feed ONLY api/baseUrl (`provider-composer.ts:218-224`): a
        // `models` entry is a full definition, so omitted fields take the
        // schema defaults (128000/16384), not the replaced model's values.
        assert_eq!(models[0].api, "openai-completions");
        assert_eq!(models[0].context_window, 128_000);
        assert_eq!(
            models[0]
                .compat
                .as_ref()
                .and_then(|compat| compat.get("useTools")),
            Some(&serde_json::json!(true))
        );
    }

    #[test]
    fn apply_models_json_rejects_empty_and_oauth_without_base_url() {
        let error = apply_models_json(
            "acme",
            &[base_model()],
            Some(&ModelsJsonProvider::default()),
        )
        .expect_err("empty config rejected");
        assert_eq!(
            error,
            "Provider acme: must specify \"baseUrl\", \"headers\", \"compat\", \"modelOverrides\", or \"models\"."
        );
        let error = apply_models_json(
            "acme",
            &[base_model()],
            Some(&ModelsJsonProvider {
                oauth: Some("radius".to_string()),
                ..Default::default()
            }),
        )
        .expect_err("oauth without baseUrl rejected");
        assert_eq!(
            error,
            "Provider acme: \"baseUrl\" is required when \"oauth\" is set."
        );
        // The "radius" oauth keeps built-in baseUrls.
        let models = apply_models_json(
            "acme",
            &[base_model()],
            Some(&ModelsJsonProvider {
                oauth: Some("radius".to_string()),
                base_url: Some("https://radius.local/v1".to_string()),
                api_key: Some("k".to_string()),
                ..Default::default()
            }),
        )
        .expect("radius keeps base urls");
        assert_eq!(models[0].base_url, "https://api.acme.com/v1");
    }

    #[test]
    fn model_from_json_applies_defaults_and_verbatim_errors() {
        let definition = ModelsJsonModel {
            id: "m2".to_string(),
            ..Default::default()
        };
        let provider = ModelsJsonProvider {
            base_url: Some("https://p.local/v1".to_string()),
            api: Some("openai-completions".to_string()),
            ..Default::default()
        };
        let model = model_from_json("acme", &definition, &provider, None).expect("defaults");
        assert_eq!(model.name, "m2");
        assert_eq!(model.context_window, 128_000);
        assert_eq!(model.max_tokens, 16_384);
        assert_eq!(model.input, vec![ModelInputModality::Text]);
        assert!(!model.reasoning);
        assert_eq!(model.cost, ModelCost::default());
        assert_eq!(model.headers, None);
        assert_eq!(model.r#type, None);

        let error = model_from_json("acme", &definition, &ModelsJsonProvider::default(), None)
            .expect_err("no api");
        assert_eq!(
            error,
            "Provider acme, model m2: no \"api\" specified. Set at provider or model level."
        );
        let error = model_from_json(
            "acme",
            &definition,
            &ModelsJsonProvider {
                api: Some("openai-completions".to_string()),
                ..Default::default()
            },
            None,
        )
        .expect_err("no baseUrl");
        assert_eq!(
            error,
            "Provider acme: \"baseUrl\" is required when defining custom models."
        );
    }

    #[test]
    fn apply_model_override_merges_field_by_field() {
        let mut model = base_model();
        model.compat = Some(
            serde_json::json!({
                "useTools": false,
                "openRouterRouting": {"provider": "a"}
            })
            .as_object()
            .cloned()
            .unwrap(),
        );
        model.input_limits = Some(ModelInputLimits {
            max_request_bytes: Some(1_000),
            images: Some(ModelImageInputLimits {
                resize: Some(ModelImageResizeOptions {
                    max_width: Some(512),
                    ..Default::default()
                }),
                max_per_message: Some(4),
                max_per_request: None,
            }),
        });
        let r#override = ModelsJsonModelOverride {
            reasoning: Some(false),
            context_window: Some(256_000),
            cost: Some(ModelCostOverride {
                output: Some(3.0),
                ..Default::default()
            }),
            input_limits: Some(ModelInputLimits {
                max_request_bytes: Some(2_000),
                images: Some(ModelImageInputLimits {
                    resize: Some(ModelImageResizeOptions {
                        max_height: Some(1024),
                        ..Default::default()
                    }),
                    max_per_request: Some(8),
                    ..Default::default()
                }),
            }),
            compat: Some(
                serde_json::json!({
                    "openRouterRouting": {"model": "b"},
                    "other": 1
                })
                .as_object()
                .cloned()
                .unwrap(),
            ),
            ..Default::default()
        };
        let overridden = apply_model_override(&model, &r#override);
        assert_eq!(overridden.name, "Built-in A"); // untouched
        assert!(!overridden.reasoning);
        assert_eq!(overridden.context_window, 256_000);
        assert_eq!(overridden.cost.output, 3.0);
        assert_eq!(overridden.cost.input, 1.0); // untouched
        let limits = overridden.input_limits.expect("limits");
        assert_eq!(limits.max_request_bytes, Some(2_000));
        let images = limits.images.expect("images");
        assert_eq!(images.max_per_message, Some(4)); // untouched
        assert_eq!(images.max_per_request, Some(8)); // filled in
        let resize = images.resize.expect("resize");
        assert_eq!(resize.max_width, Some(512)); // base kept
        assert_eq!(resize.max_height, Some(1024)); // override added
        let compat = overridden.compat.expect("compat");
        assert_eq!(compat.get("useTools"), Some(&serde_json::json!(false)));
        assert_eq!(compat.get("other"), Some(&serde_json::json!(1)));
        let routing = compat.get("openRouterRouting").expect("routing");
        assert_eq!(routing, &serde_json::json!({"provider": "a", "model": "b"}));
    }

    #[test]
    fn sampling_params_by_thinking_level_merges_per_level() {
        let mut model = base_model();
        model.sampling_params_by_thinking_level = Some(
            serde_json::json!({"high": {"temperature": 0.5}})
                .as_object()
                .cloned()
                .unwrap(),
        );
        let r#override = ModelsJsonModelOverride {
            sampling_params_by_thinking_level: Some(
                serde_json::json!({"high": {"topK": 40}, "max": {"temperature": 0.1}})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
            ..Default::default()
        };
        let merged = apply_model_override(&model, &r#override)
            .sampling_params_by_thinking_level
            .expect("merged map");
        assert_eq!(
            merged.get("high"),
            Some(&serde_json::json!({"temperature": 0.5, "topK": 40}))
        );
        assert_eq!(
            merged.get("max"),
            Some(&serde_json::json!({"temperature": 0.1}))
        );
    }
}
