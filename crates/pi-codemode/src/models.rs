//! The `models.*` globals as a data surface.
//!
//! Ground truth: `packages/coding-agent/src/extensions/codemode/execute.ts:110-150`
//! (`MODEL_TYPES`, `toModelType`, `toProvider`, `toModelInfo`, `describeValue`,
//! `withArticle`), `:172-263` (context checks and `ModelCallResult`), and
//! `:742-850` (`createModelGlobals`), plus
//! `packages/coding-agent/src/extensions/codemode/tool.ts:54-58` and `:128-139`
//! (`CodemodeModelRuntime`, the `describeGlobals` line).
//!
//! Upstream binds these five globals to the session's async `ModelRegistry`
//! and reports each classifier/image call as a nested call row with usage.
//! This crate has no model registry and no async executor, so the registry is a
//! synchronous [`ModelRuntime`] trait object supplied by the host. The five
//! spread globals returned by [`model_globals`] have the same names, argument
//! validation, and error messages; usage/call-row bookkeeping stays in the
//! host (see `WIRING.md`).

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value as Json;

use crate::sandbox::{FnTool, Tool, ToolError};
use crate::types::{CodemodeDeclaredGlobal, ToolContext};

/// The docs path used in upstream's validation messages (`tool.ts:121`). The
/// host's real absolute path is available in `pi-coding-agent`; this crate only
/// needs the stable relative suffix to reproduce the message.
pub const CODEMODE_DOCS_RELATIVE_PATH: &str = "docs/codemode.md";

/// Maximum concurrent `models.*` calls upstream (`execute.ts:50`). The inline
/// VM has one synchronous call stack, so this is a host-side concern; the
/// constant is kept for parity and declaration/docs code.
pub const MAX_CONCURRENT_MODEL_CALLS: usize = 4;

/// The names of the globals upstream declares for model access
/// (`execute.ts:813-848`). All are spread globals: the script passes an
/// argument list, which crosses the bridge as a JSON array.
pub const MODEL_GLOBAL_NAMES: [&str; 5] = [
    "models.getModelsOfType",
    "models.getAvailableOfType",
    "models.getModelOfType",
    "models.classify",
    "models.generateImages",
];

/// Upstream's `MODEL_TYPES` (`execute.ts:112`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelType {
    /// Chat/completion models.
    Chat,
    /// Image-generation models.
    Image,
    /// Classifier models.
    Classifier,
}

impl ModelType {
    /// The script-facing value (`execute.ts:134-137`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Image => "image",
            Self::Classifier => "classifier",
        }
    }

    /// Parse a script value, returning upstream's error for anything else.
    pub fn parse(value: &Json) -> Result<Self, ToolError> {
        if let Json::String(text) = value {
            match text.as_str() {
                "chat" => return Ok(Self::Chat),
                "image" => return Ok(Self::Image),
                "classifier" => return Ok(Self::Classifier),
                _ => {}
            }
        }
        Err(ToolError::new(format!(
            "Unknown model type {}. Use \"chat\", \"image\", or \"classifier\".",
            json_text(value)
        )))
    }
}

/// `withArticle` (`execute.ts:152-155`): `a classifier`, `an image`.
#[must_use]
pub fn with_article(word: &str) -> String {
    let article = if matches!(word.chars().next(), Some('a' | 'e' | 'i' | 'o' | 'u')) {
        "an"
    } else {
        "a"
    };
    format!("{article} {word}")
}

/// `describeValue` (`execute.ts:160-170`) on a JSON value. A script's
/// `undefined` crosses the bridge as `null`, which is why both render as
/// `null`.
#[must_use]
pub fn describe_value(value: &Json) -> String {
    match value {
        Json::Null => "null".to_string(),
        Json::Array(items) if items.is_empty() => "an empty array".to_string(),
        Json::Array(_) => "an array".to_string(),
        Json::Object(map) if map.is_empty() => "{}".to_string(),
        Json::Object(map) => {
            let keys: Vec<&String> = map.keys().take(6).collect();
            let more = map.len() > 6;
            let body = keys
                .iter()
                .map(|key| key.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if more {
                format!("{{ {body}, ... }}")
            } else {
                format!("{{ {body} }}")
            }
        }
        Json::String(_) => "a string".to_string(),
        Json::Bool(_) => "a boolean".to_string(),
        Json::Number(_) => "a number".to_string(),
    }
}

fn json_text(value: &Json) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// One catalog entry a script sees (`toModelInfo`, `execute.ts:145-150`).
/// `headers` is dropped: `models.json` headers can carry credentials.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    /// Provider id, e.g. `typesafe`.
    pub provider: String,
    /// Model id, e.g. `jev-latest`.
    pub id: String,
    /// Other JSON fields of the catalog entry, without `headers`.
    pub fields: BTreeMap<String, Json>,
}

impl ModelInfo {
    /// Build a catalog entry from a JSON model object, dropping `headers`.
    #[must_use]
    pub fn from_json(value: &Json) -> Option<Self> {
        let object = value.as_object()?;
        let provider = object.get("provider")?.as_str()?.to_string();
        let id = object.get("id")?.as_str()?.to_string();
        let fields = object
            .iter()
            .filter(|(key, _)| {
                key.as_str() != "headers" && key.as_str() != "provider" && key.as_str() != "id"
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Some(Self {
            provider,
            id,
            fields,
        })
    }

    /// The JSON object handed back to the script. `headers` is dropped here as
    /// well as in [`Self::from_json`]: a host-built `ModelInfo` may still carry
    /// it, and upstream's `toModelInfo` never exposes credentials.
    #[must_use]
    pub fn to_json(&self) -> Json {
        let mut object = self.fields.clone();
        object.retain(|key, _| key != "headers");
        object.insert("provider".to_string(), Json::String(self.provider.clone()));
        object.insert("id".to_string(), Json::String(self.id.clone()));
        Json::Object(object.into_iter().collect())
    }
}

/// `ModelCallResult` (`execute.ts:258-263`): only the fields a nested call row
/// and the host's usage accounting need.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCallResult {
    /// Upstream's `stopReason`.
    pub stop_reason: ModelStopReason,
    /// Error text upstream copies into the nested call row.
    pub error_message: Option<String>,
    /// Total USD cost from `usage`, when the call reported usage.
    pub usage_cost: Option<f64>,
    /// Number of image blocks in `output` (used by the "script did not show
    /// images" note in `execute.ts:631`).
    pub image_count: usize,
}

/// The three `stopReason` values of a model call (`execute.ts:260`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelStopReason {
    /// The call completed.
    Stop,
    /// The provider failed.
    Error,
    /// The caller aborted.
    Aborted,
}

impl ModelStopReason {
    /// The upstream wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Error => "error",
            Self::Aborted => "aborted",
        }
    }
}

/// The subset of `ModelRegistry` scripts reach through `models`
/// (`tool.ts:54-58`). Synchronous because the embedded VM is synchronous; a
/// host with async models adapts by blocking (see `WIRING.md`).
pub trait ModelRuntime: Send + Sync {
    /// `getModelsOfType(type, provider?)` — all models of a type, optionally
    /// filtered by provider.
    fn get_models_of_type(&self, model_type: ModelType, provider: Option<&str>) -> Vec<ModelInfo>;

    /// `getAvailableOfType(type, provider?)` — the subset currently available.
    fn get_available_of_type(
        &self,
        model_type: ModelType,
        provider: Option<&str>,
    ) -> Vec<ModelInfo>;

    /// `getModelOfType(type, provider, id)` — look up one model by its two
    /// separate arguments.
    fn get_model_of_type(
        &self,
        model_type: ModelType,
        provider: &str,
        id: &str,
    ) -> Option<ModelInfo>;

    /// `classify(model, context)`.
    fn classify(&self, model: &ModelInfo, context: &Json) -> Result<ModelCallResult, String>;

    /// `generateImages(model, context)`.
    fn generate_images(&self, model: &ModelInfo, context: &Json)
    -> Result<ModelCallResult, String>;
}

/// A registry with no models, for hosts that have not wired model access yet.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyModelRuntime;

impl ModelRuntime for EmptyModelRuntime {
    fn get_models_of_type(
        &self,
        _model_type: ModelType,
        _provider: Option<&str>,
    ) -> Vec<ModelInfo> {
        Vec::new()
    }

    fn get_available_of_type(
        &self,
        _model_type: ModelType,
        _provider: Option<&str>,
    ) -> Vec<ModelInfo> {
        Vec::new()
    }

    fn get_model_of_type(
        &self,
        _model_type: ModelType,
        _provider: &str,
        _id: &str,
    ) -> Option<ModelInfo> {
        None
    }

    fn classify(&self, _model: &ModelInfo, _context: &Json) -> Result<ModelCallResult, String> {
        Err("No classifier model is available".to_string())
    }

    fn generate_images(
        &self,
        _model: &ModelInfo,
        _context: &Json,
    ) -> Result<ModelCallResult, String> {
        Err("No image model is available".to_string())
    }
}

/// The declarations a host passes to `Sandbox::start` / `VmStartData` when
/// model access is enabled (`execute.ts:849`).
#[must_use]
pub fn model_global_declarations() -> Vec<CodemodeDeclaredGlobal> {
    MODEL_GLOBAL_NAMES
        .iter()
        .map(|name| CodemodeDeclaredGlobal {
            name: (*name).to_string(),
            spread: true,
        })
        .collect()
}

fn spread_args(args: &Json) -> Vec<Json> {
    args.as_array().cloned().unwrap_or_default()
}

fn provider_argument(args: &[Json]) -> Result<Option<String>, ToolError> {
    match args.get(1) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ToolError::new("provider must be a string")),
    }
}

/// Resolve the model of a `models.classify`/`generateImages` call, with
/// upstream's error messages (`execute.ts:764-789`). Only `provider` and `id`
/// are read from a script-supplied model object: a script's `baseUrl` or
/// `headers` must never reach the credentials.
fn resolve_model(
    runtime: &dyn ModelRuntime,
    name: &str,
    model_type: ModelType,
    args: &[Json],
) -> Result<ModelInfo, ToolError> {
    let given = args.first().unwrap_or(&Json::Null);
    let Some(object) = given.as_object() else {
        let undefined_hint = if given.is_null() {
            " models.getModelOfType() returns undefined for an unknown provider or id."
        } else {
            ""
        };
        let list_hint = format!(
            "List the {} models you can use with models.getAvailableOfType(\"{}\").",
            model_type.as_str(),
            model_type.as_str()
        );
        return Err(ToolError::new(format!(
            "{name}() expects {} model as its first argument, got {}.{undefined_hint} {list_hint}",
            with_article(model_type.as_str()),
            describe_value(given)
        )));
    };
    let (Some(provider), Some(id)) = (
        object.get("provider").and_then(Json::as_str),
        object.get("id").and_then(Json::as_str),
    ) else {
        let undefined_hint = if given.is_null() {
            " models.getModelOfType() returns undefined for an unknown provider or id."
        } else {
            ""
        };
        let list_hint = format!(
            "List the {} models you can use with models.getAvailableOfType(\"{}\").",
            model_type.as_str(),
            model_type.as_str()
        );
        return Err(ToolError::new(format!(
            "{name}() expects {} model as its first argument, got {}.{undefined_hint} {list_hint}",
            with_article(model_type.as_str()),
            describe_value(given)
        )));
    };
    let reference = format!("{provider}/{id}");
    if let Some(model) = runtime.get_model_of_type(model_type, provider, id) {
        return Ok(model);
    }
    let list_hint = format!(
        "List the {} models you can use with models.getAvailableOfType(\"{}\").",
        model_type.as_str(),
        model_type.as_str()
    );
    let actual = [ModelType::Chat, ModelType::Image, ModelType::Classifier]
        .into_iter()
        .find(|other| {
            *other != model_type && runtime.get_model_of_type(*other, provider, id).is_some()
        });
    let message = match actual {
        Some(other) => format!(
            "\"{reference}\" is {} model, not {} model. {list_hint}",
            with_article(other.as_str()),
            with_article(model_type.as_str())
        ),
        None => format!(
            "Unknown {} model \"{reference}\". {list_hint}",
            model_type.as_str()
        ),
    };
    Err(ToolError::new(message))
}

const CLASSIFIER_CONTEXT_SHAPE: &str = "{ state: { ... }, images?: [{ type: \"image\", data: <base64>, mimeType }], questions: { <id>: { type: \"choice\", instructions, criteria: { <label>: <meaning> } } | { type: \"score\", instructions, criteria: [<lowest level>, ..., <highest level>] } | { type: \"bool\", instructions, criteria: { true: <meaning>, false: <meaning> } } } }";
const IMAGES_CONTEXT_SHAPE: &str = "{ input: [{ type: \"text\", text: <prompt> }, ...optional { type: \"image\", data: <base64>, mimeType } references] }";

/// Port of `checkClassifierContext` (`execute.ts:176-235`).
#[allow(clippy::too_many_lines)] // mirrors upstream checkClassifierContext; split when extending
pub fn check_classifier_context(context: &Json) -> Result<(), ToolError> {
    let fail = |problem: &str| {
        ToolError::new(format!(
            "models.classify() {problem}. Expected context: {CLASSIFIER_CONTEXT_SHAPE}. See \"Classify\" in {CODEMODE_DOCS_RELATIVE_PATH}."
        ))
    };
    let object = context.as_object().ok_or_else(|| {
        fail(&format!(
            "expects a context object as its second argument, got {}",
            describe_value(context)
        ))
    })?;
    let state = object.get("state");
    if !state.is_some_and(Json::is_object) {
        return Err(fail(&format!(
            "context.state must be an object, got {}",
            describe_value(state.unwrap_or(&Json::Null))
        )));
    }
    if let Some(images) = object.get("images")
        && !images.is_null()
    {
        let items = images.as_array().ok_or_else(|| {
            fail(&format!(
                "context.images must be an array, got {}",
                describe_value(images)
            ))
        })?;
        for (index, image) in items.iter().enumerate() {
            let valid = image.as_object().is_some_and(|block| {
                block.get("type").and_then(Json::as_str) == Some("image")
                    && block.get("data").is_some_and(Json::is_string)
                    && block.get("mimeType").is_some_and(Json::is_string)
            });
            if !valid {
                return Err(fail(&format!(
                    "context.images[{index}] must be an image block, got {}",
                    describe_value(image)
                )));
            }
        }
    }
    let questions = object.get("questions");
    let Some(questions) = questions
        .and_then(Json::as_object)
        .filter(|map| !map.is_empty())
    else {
        return Err(fail(&format!(
            "context.questions must map question IDs to questions, got {}",
            describe_value(questions.unwrap_or(&Json::Null))
        )));
    };
    let all_strings =
        |values: Vec<&Json>| !values.is_empty() && values.iter().all(|value| value.is_string());
    for (id, question) in questions {
        let at = format!("context.questions.{id}");
        let block = question.as_object().ok_or_else(|| {
            fail(&format!(
                "{at} must be a question object, got {}",
                describe_value(question)
            ))
        })?;
        if !block.get("instructions").is_some_and(Json::is_string) {
            return Err(fail(&format!("{at}.instructions must be a string")));
        }
        let criteria = block.get("criteria");
        match block.get("type").and_then(Json::as_str) {
            Some("choice") => {
                let values = criteria
                    .and_then(Json::as_object)
                    .map(|map| map.values().collect())
                    .unwrap_or_default();
                if !all_strings(values) {
                    return Err(fail(&format!(
                        "{at} is a \"choice\" question, so criteria must map each label to its meaning"
                    )));
                }
            }
            Some("score") => {
                let values = criteria
                    .and_then(Json::as_array)
                    .map(|items| items.iter().collect())
                    .unwrap_or_default();
                if !all_strings(values) {
                    return Err(fail(&format!(
                        "{at} is a \"score\" question, so criteria must list the levels as strings, lowest first"
                    )));
                }
            }
            Some("bool") => {
                let valid = criteria.and_then(Json::as_object).is_some_and(|map| {
                    map.get("true").is_some_and(Json::is_string)
                        && map.get("false").is_some_and(Json::is_string)
                });
                if !valid {
                    return Err(fail(&format!(
                        "{at} is a \"bool\" question, so criteria must be {{ true: string, false: string }}"
                    )));
                }
            }
            other => {
                return Err(fail(&format!(
                    "{at}.type must be \"choice\", \"score\", or \"bool\", got {}",
                    json_text(&other.map_or(Json::Null, |text| Json::String(text.to_string())))
                )));
            }
        }
    }
    Ok(())
}

/// Port of `checkImagesContext` (`execute.ts:238-256`).
pub fn check_images_context(context: &Json) -> Result<(), ToolError> {
    let fail = |problem: &str| {
        ToolError::new(format!(
            "models.generateImages() {problem}. Expected context: {IMAGES_CONTEXT_SHAPE}. See \"Generate images\" in {CODEMODE_DOCS_RELATIVE_PATH}."
        ))
    };
    let object = context.as_object().ok_or_else(|| {
        fail(&format!(
            "expects a context object as its second argument, got {}",
            describe_value(context)
        ))
    })?;
    let items = object
        .get("input")
        .and_then(Json::as_array)
        .filter(|items| !items.is_empty())
        .ok_or_else(|| {
            fail(&format!(
                "context.input must be a non-empty array of blocks, got {}",
                describe_value(object.get("input").unwrap_or(&Json::Null))
            ))
        })?;
    for (index, block) in items.iter().enumerate() {
        let valid = block.as_object().is_some_and(|block| {
            (block.get("type").and_then(Json::as_str) == Some("text")
                && block.get("text").is_some_and(Json::is_string))
                || (block.get("type").and_then(Json::as_str) == Some("image")
                    && block.get("data").is_some_and(Json::is_string)
                    && block.get("mimeType").is_some_and(Json::is_string))
        });
        if !valid {
            return Err(fail(&format!(
                "context.input[{index}] must be a text or image block, got {}",
                describe_value(block)
            )));
        }
    }
    Ok(())
}

fn describe_args(args: &[Json]) -> String {
    args.iter()
        .map(describe_value)
        .collect::<Vec<_>>()
        .join(", ")
}

fn model_list(
    runtime: &dyn ModelRuntime,
    args: &[Json],
    available: bool,
) -> Result<Json, ToolError> {
    let model_type = ModelType::parse(args.first().unwrap_or(&Json::Null))?;
    let provider = provider_argument(args)?;
    let models = if available {
        runtime.get_available_of_type(model_type, provider.as_deref())
    } else {
        runtime.get_models_of_type(model_type, provider.as_deref())
    };
    Ok(Json::Array(models.iter().map(ModelInfo::to_json).collect()))
}

fn model_call(
    runtime: &dyn ModelRuntime,
    name: &str,
    model_type: ModelType,
    args: &[Json],
    check: fn(&Json) -> Result<(), ToolError>,
    run: fn(&dyn ModelRuntime, &ModelInfo, &Json) -> Result<ModelCallResult, String>,
    context: &ToolContext,
) -> Result<Json, ToolError> {
    let _ = context; // upstream forwards its AbortSignal; the registry seam is synchronous
    let model = resolve_model(runtime, name, model_type, args)?;
    let arguments = args.get(1).unwrap_or(&Json::Null);
    check(arguments)?;
    let result = run(runtime, &model, arguments).map_err(ToolError::new)?;
    let mut object = serde_json::Map::new();
    object.insert(
        "stopReason".to_string(),
        Json::String(result.stop_reason.as_str().to_string()),
    );
    if let Some(error) = result.error_message {
        object.insert("errorMessage".to_string(), Json::String(error));
    }
    if let Some(cost) = result.usage_cost
        && let Some(number) = serde_json::Number::from_f64(cost)
    {
        object.insert("cost".to_string(), Json::Number(number));
    }
    object.insert(
        "imageCount".to_string(),
        Json::from(u64::try_from(result.image_count).unwrap_or(u64::MAX)),
    );
    Ok(Json::Object(object.into_iter().collect()))
}

fn classify_call(
    runtime: &dyn ModelRuntime,
    model: &ModelInfo,
    context: &Json,
) -> Result<ModelCallResult, String> {
    runtime.classify(model, context)
}

fn images_call(
    runtime: &dyn ModelRuntime,
    model: &ModelInfo,
    context: &Json,
) -> Result<ModelCallResult, String> {
    runtime.generate_images(model, context)
}

/// Build the five spread globals upstream declares for model access
/// (`execute.ts:742-850`). They are ordinary [`Tool`]s so a host can pass them
/// to `Sandbox::new(SandboxOptions { globals, .. })`.
#[must_use]
pub fn model_globals(runtime: Arc<dyn ModelRuntime>) -> Vec<Arc<dyn Tool>> {
    let get_models = Arc::clone(&runtime);
    let get_available = Arc::clone(&runtime);
    let get_model = Arc::clone(&runtime);
    let classify = Arc::clone(&runtime);
    let images = runtime;
    vec![
        Arc::new(
            FnTool::new(
                "models.getModelsOfType",
                Box::new(move |args, _| model_list(get_models.as_ref(), &spread_args(&args), false)),
            )
            .spread(true),
        ),
        Arc::new(
            FnTool::new(
                "models.getAvailableOfType",
                Box::new(move |args, _| model_list(get_available.as_ref(), &spread_args(&args), true)),
            )
            .spread(true),
        ),
        Arc::new(
            FnTool::new(
                "models.getModelOfType",
                Box::new(move |args, _| {
                    let args = spread_args(&args);
                    let model_type = ModelType::parse(args.first().unwrap_or(&Json::Null))?;
                    let error = || {
                        ToolError::new(format!(
                            "models.getModelOfType(type, provider, id) expects three strings, got ({}). The provider and the id are separate arguments, for example models.getModelOfType(\"classifier\", \"typesafe\", \"jev-latest\").",
                            describe_args(&args)
                        ))
                    };
                    let provider = args.get(1).and_then(Json::as_str).ok_or_else(&error)?.to_string();
                    let id = args.get(2).and_then(Json::as_str).ok_or_else(&error)?.to_string();
                    Ok(get_model
                        .get_model_of_type(model_type, &provider, &id)
                        .map_or(Json::Null, |model| model.to_json()))
                }),
            )
            .spread(true),
        ),
        Arc::new(
            FnTool::new(
                "models.classify",
                Box::new(move |args, context| {
                    model_call(
                        classify.as_ref(),
                        "models.classify",
                        ModelType::Classifier,
                        &spread_args(&args),
                        check_classifier_context,
                        classify_call,
                        context,
                    )
                }),
            )
            .spread(true),
        ),
        Arc::new(
            FnTool::new(
                "models.generateImages",
                Box::new(move |args, context| {
                    model_call(
                        images.as_ref(),
                        "models.generateImages",
                        ModelType::Image,
                        &spread_args(&args),
                        check_images_context,
                        images_call,
                        context,
                    )
                }),
            )
            .spread(true),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(provider: &str, id: &str) -> ModelInfo {
        ModelInfo {
            provider: provider.to_string(),
            id: id.to_string(),
            fields: BTreeMap::new(),
        }
    }

    struct StubRegistry;

    impl ModelRuntime for StubRegistry {
        fn get_models_of_type(
            &self,
            model_type: ModelType,
            _provider: Option<&str>,
        ) -> Vec<ModelInfo> {
            vec![model(model_type.as_str(), "m1")]
        }

        fn get_available_of_type(
            &self,
            model_type: ModelType,
            _provider: Option<&str>,
        ) -> Vec<ModelInfo> {
            vec![model(model_type.as_str(), "available")]
        }

        fn get_model_of_type(
            &self,
            _model_type: ModelType,
            provider: &str,
            id: &str,
        ) -> Option<ModelInfo> {
            (provider == "typesafe" && id == "jev-latest").then(|| {
                let mut info = model("typesafe", "jev-latest");
                info.fields
                    .insert("headers".to_string(), json!({"secret": 1}));
                info
            })
        }

        fn classify(&self, _model: &ModelInfo, _context: &Json) -> Result<ModelCallResult, String> {
            Ok(ModelCallResult {
                stop_reason: ModelStopReason::Stop,
                error_message: None,
                usage_cost: Some(0.5),
                image_count: 0,
            })
        }

        fn generate_images(
            &self,
            _model: &ModelInfo,
            _context: &Json,
        ) -> Result<ModelCallResult, String> {
            Ok(ModelCallResult {
                stop_reason: ModelStopReason::Stop,
                error_message: None,
                usage_cost: None,
                image_count: 2,
            })
        }
    }

    struct ImageOnlyRegistry;

    impl ModelRuntime for ImageOnlyRegistry {
        fn get_models_of_type(&self, _: ModelType, _: Option<&str>) -> Vec<ModelInfo> {
            Vec::new()
        }
        fn get_available_of_type(&self, _: ModelType, _: Option<&str>) -> Vec<ModelInfo> {
            Vec::new()
        }
        fn get_model_of_type(
            &self,
            model_type: ModelType,
            provider: &str,
            id: &str,
        ) -> Option<ModelInfo> {
            (model_type == ModelType::Image && provider == "typesafe" && id == "jev-latest")
                .then(|| model("typesafe", "jev-latest"))
        }
        fn classify(&self, _: &ModelInfo, _: &Json) -> Result<ModelCallResult, String> {
            unreachable!("resolution fails before the call")
        }
        fn generate_images(&self, _: &ModelInfo, _: &Json) -> Result<ModelCallResult, String> {
            unreachable!("resolution fails before the call")
        }
    }

    fn globals() -> Vec<Arc<dyn Tool>> {
        model_globals(Arc::new(StubRegistry))
    }

    fn find(name: &str) -> Arc<dyn Tool> {
        globals()
            .into_iter()
            .find(|tool| tool.name() == name)
            .expect("global exists")
    }

    fn call(name: &str, args: Json) -> Result<Json, ToolError> {
        let tool = find(name);
        assert!(tool.spread(), "model globals are spread");
        tool.execute(args, &ToolContext::default())
    }

    #[test]
    fn model_type_and_article_match_upstream() {
        assert_eq!(ModelType::parse(&json!("image")).unwrap(), ModelType::Image);
        assert_eq!(with_article("image"), "an image");
        assert_eq!(with_article("classifier"), "a classifier");
        let error = ModelType::parse(&json!("other")).unwrap_err();
        assert_eq!(
            error.message,
            "Unknown model type \"other\". Use \"chat\", \"image\", or \"classifier\"."
        );
    }

    #[test]
    fn describe_value_covers_script_shapes() {
        assert_eq!(describe_value(&Json::Null), "null");
        assert_eq!(describe_value(&json!([])), "an empty array");
        assert_eq!(describe_value(&json!([1])), "an array");
        assert_eq!(describe_value(&json!("x")), "a string");
        assert_eq!(describe_value(&json!({"a": 1, "b": 2})), "{ a, b }");
    }

    #[test]
    fn catalog_globals_drop_headers_and_support_provider_filter() {
        let models = call("models.getModelsOfType", json!(["chat"])).unwrap();
        assert_eq!(models[0]["provider"], "chat");
        assert!(models[0].get("headers").is_none());
        let available = call("models.getAvailableOfType", json!(["image", "openai"])).unwrap();
        assert_eq!(available[0]["id"], "available");
        let info = ModelInfo::from_json(&json!({"provider": "p", "id": "i", "headers": {"x": 1}}))
            .unwrap();
        assert!(info.to_json().get("headers").is_none());
    }

    #[test]
    fn get_model_of_type_uses_upstream_argument_error() {
        let error = call("models.getModelOfType", json!(["classifier", 7, "id"])).unwrap_err();
        assert!(error.message.contains("expects three strings, got ("));
        assert!(error.message.contains("typesafe\", \"jev-latest"));
        let found = call(
            "models.getModelOfType",
            json!(["classifier", "typesafe", "jev-latest"]),
        )
        .unwrap();
        assert_eq!(found["id"], "jev-latest");
        assert!(found.get("headers").is_none());
        let missing = call("models.getModelOfType", json!(["classifier", "x", "y"])).unwrap();
        assert_eq!(missing, Json::Null);
    }

    #[test]
    fn model_call_resolves_by_provider_and_id_only() {
        let context = json!({
            "state": {},
            "questions": {"q": {"type": "bool", "instructions": "?", "criteria": {"true": "y", "false": "n"}}}
        });
        let result = call(
            "models.classify",
            json!([{"provider": "typesafe", "id": "jev-latest", "baseUrl": "evil"}, context]),
        )
        .unwrap();
        assert_eq!(result["stopReason"], "stop");
        assert_eq!(result["cost"], 0.5);
    }

    #[test]
    fn model_call_reports_unknown_and_wrong_type_models() {
        let context = json!({"state": {}, "questions": {"q": {"type": "bool", "instructions": "?", "criteria": {"true": "y", "false": "n"}}}});
        let unknown = call(
            "models.classify",
            json!([{"provider": "x", "id": "y"}, context]),
        )
        .unwrap_err();
        assert!(unknown.message.contains("Unknown classifier model \"x/y\""));
        let wrong_tool = model_globals(Arc::new(ImageOnlyRegistry))
            .into_iter()
            .find(|tool| tool.name() == "models.classify")
            .expect("global exists");
        let wrong = wrong_tool
            .execute(
                json!([{"provider": "typesafe", "id": "jev-latest"}, context]),
                &ToolContext::default(),
            )
            .unwrap_err();
        assert!(
            wrong
                .message
                .contains("is an image model, not a classifier model")
        );
        let empty: Arc<dyn ModelRuntime> = Arc::new(EmptyModelRuntime);
        let tool = model_globals(empty)
            .into_iter()
            .find(|tool| tool.name() == "models.classify")
            .expect("global exists");
        let error = tool
            .execute(json!([null, context]), &ToolContext::default())
            .unwrap_err();
        assert!(
            error
                .message
                .contains("expects a classifier model as its first argument")
        );
        assert!(
            error
                .message
                .contains("models.getModelOfType() returns undefined")
        );
    }

    #[test]
    fn context_validation_uses_upstream_shapes() {
        let error = check_classifier_context(&json!({"prompt": "x"})).unwrap_err();
        assert!(error.message.contains("context.state must be an object"));
        assert!(error.message.contains("Expected context:"));
        let error = check_classifier_context(&json!({
            "state": {}, "questions": {"q": {"type": "score", "instructions": "?", "criteria": []}}
        }))
        .unwrap_err();
        assert!(
            error
                .message
                .contains("criteria must list the levels as strings")
        );
        let error = check_images_context(&json!({"prompt": "x"})).unwrap_err();
        assert!(
            error
                .message
                .contains("context.input must be a non-empty array")
        );
        check_images_context(&json!({"input": [{"type": "text", "text": "hi"}]})).unwrap();
    }

    #[test]
    fn declarations_name_all_five_spread_globals() {
        let declarations = model_global_declarations();
        assert_eq!(declarations.len(), 5);
        assert!(declarations.iter().all(|global| global.spread));
        assert_eq!(declarations[0].name, "models.getModelsOfType");
    }
}
