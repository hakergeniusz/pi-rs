//! Model reference resolution and scope patterns.
//!
//! Ground truth: `core/model-resolver.ts` (1.5.2) — the reference machinery
//! (`resolveModelReference` and its helpers, `model-resolver.ts:74-313`) and
//! the scope resolver [`resolve_model_scope_from_models`]
//! (`resolveModelScopeFromModels`, `model-resolver.ts:321-393`) behind the
//! `--models` flag and the `enabledModels` setting. The consumers that turn
//! a `not-found` into a custom-model target (`ModelReferenceTarget`,
//! `model-resolver.ts:117-124`, plus `defaultModelPerProvider` /
//! `buildFallbackModel` / `resolveCliModel`) join with the P5 CLI work: they
//! need no behavior here, so `not-found` carries no target.
//!
//! Deviation (mechanism only, see HANDOFF.md): upstream globs through
//! `minimatch` (`nocase: true`); the port substitutes `globset` with
//! case-insensitive matching and separator-literal segments (minimatch never
//! lets `*` cross `/`). minimatch's dotfile rule (`*` skipping leading-dot
//! segments) is not reproduced — model ids and provider names do not start
//! with a dot. A pattern globset cannot compile matches nothing, where
//! minimatch would throw.
//!
//! `resolveModelScopeWithDiagnostics`'s runtime hop
//! (`model-resolver.ts:395-401`) is the caller's `getAvailable` read; the
//! port passes the model list directly (interactive resolves over
//! [`crate::modes::interactive::interactive_mode::InteractiveMode::
//! available_models`]).

use std::collections::HashSet;

use pi_agent::types::ThinkingLevel;
use pi_ai::types::Model;

/// A scoped model with its pattern-explicit thinking level
/// (`ScopedModel`, `model-resolver.ts:66-70`).
#[derive(Debug, Clone)]
pub struct ScopedModel {
    pub model: Model,
    /// The pattern's `:level` suffix, when it named a valid level
    /// (`model-resolver.ts:68-69`).
    pub thinking_level: Option<ThinkingLevel>,
}

/// A scope diagnostic's code (`ModelScopeDiagnostic`'s `code` union,
/// `model-resolver.ts:310-314`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelScopeDiagnosticCode {
    /// No model matched the pattern.
    NoMatch,
    /// The pattern's `:level` suffix is not a valid thinking level.
    InvalidThinkingLevel,
    /// The pattern matched the same id on several providers.
    Ambiguous,
}

/// A warning diagnostic (`ModelScopeDiagnostic`,
/// `model-resolver.ts:310-315`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelScopeDiagnostic {
    pub code: ModelScopeDiagnosticCode,
    pub message: String,
    /// The pattern the warning is about.
    pub pattern: String,
}

/// The scope resolution outcome
/// (`ResolveModelScopeResult`, `model-resolver.ts:317-320`).
#[derive(Debug, Clone, Default)]
pub struct ResolveModelScopeResult {
    pub scoped_models: Vec<ScopedModel>,
    pub diagnostics: Vec<ModelScopeDiagnostic>,
}

/// `modelsAreEqual` (`packages/ai/src/models.ts:1340-1344`): type, id, and
/// provider. The port's virtual catalog entries carry the virtual marker on
/// their api (`VIRTUAL_MODEL_API`) rather than a `type` field, so the type
/// comparison reads it (`getModelType`, `utils/model-operations.ts:18-20` —
/// models without `type` are chat models).
pub fn models_are_equal(a: &Model, b: &Model) -> bool {
    let a_virtual = crate::virtual_models::is_virtual_model(a);
    let b_virtual = crate::virtual_models::is_virtual_model(b);
    let same_type = if a_virtual || b_virtual {
        a_virtual == b_virtual
    } else {
        a.r#type.as_deref().unwrap_or("chat") == b.r#type.as_deref().unwrap_or("chat")
    };
    same_type && a.id == b.id && a.provider == b.provider
}

/// The `thinkingSuffix` modes (`ModelReferenceRequest`, `model-resolver.ts:86-99`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingSuffix {
    /// The suffix stays part of the id.
    None,
    /// A valid thinking level parses; any other suffix stays part of the id.
    Strict,
    /// Like [`ThinkingSuffix::Strict`], but an invalid suffix is dropped with
    /// a warning.
    Lenient,
}

/// A model-reference lookup request
/// (`ModelReferenceRequest`, `model-resolver.ts:80-106`).
#[derive(Debug, Clone, Copy)]
pub struct ModelReferenceRequest<'a> {
    /// Bare model id or `provider/id`, optionally followed by `:<thinking>`.
    pub text: &'a str,
    /// Only consider models from this provider.
    pub provider: Option<&'a str>,
    pub thinking_suffix: ThinkingSuffix,
    /// Match id and name substrings when no exact match exists.
    pub fuzzy: bool,
}

/// One reference-lookup outcome (`ModelReferenceResult`,
/// `model-resolver.ts:108-127`). The `not-found` target (the custom-model
/// fallback, `model-resolver.ts:117-124`) is not ported; see the module doc.
#[derive(Debug, Clone)]
pub enum ModelReferenceResult {
    Resolved {
        model: Model,
        thinking_level: Option<ThinkingLevel>,
        warning: Option<String>,
    },
    Ambiguous {
        candidates: Vec<Model>,
        usable_count: usize,
        thinking_level: Option<ThinkingLevel>,
        warning: Option<String>,
    },
    NotFound,
}

/// `splitThinkingSuffix` (`model-resolver.ts:130-135`): the `:level` tail,
/// only when it names a valid level.
fn split_thinking_suffix(text: &str) -> Option<(&str, ThinkingLevel)> {
    let colon_index = text.rfind(':')?;
    let suffix = &text[colon_index + 1..];
    let level = crate::modes::print_mode::thinking_level_from_str(suffix).ok()?;
    Some((&text[..colon_index], level))
}

/// `splitProviderPrefix` (`model-resolver.ts:137-153`): the known-provider
/// prefix before the first `/` after it, longest first — provider ids may
/// contain `/` themselves, so match known ids. Returns `(provider,
/// modelText)`.
fn split_provider_prefix(text: &str, pool: &[&Model]) -> Option<(String, String)> {
    let mut best: Option<(usize, String, String)> = None;
    for model in pool {
        let prefix = model.provider.trim().to_lowercase();
        if prefix.is_empty() {
            continue;
        }
        // `best.provider.length >= provider.length` compares the original
        // lengths (`model-resolver.ts:146`); carry the original alongside.
        if best
            .as_ref()
            .is_some_and(|(length, _, _)| *length >= model.provider.len())
        {
            continue;
        }
        for (slash_index, _) in text.match_indices('/') {
            if text[..slash_index].trim().to_lowercase() != prefix {
                continue;
            }
            let model_text = text[slash_index + 1..].trim();
            if model_text.is_empty() {
                continue;
            }
            best = Some((
                model.provider.len(),
                model.provider.clone(),
                model_text.to_string(),
            ));
            break;
        }
    }
    best.map(|(_, provider, model_text)| (provider, model_text))
}

impl ModelReferenceResult {
    /// The `{...inner, thinkingLevel}` spread (`model-resolver.ts:198-200`,
    /// `:222-226`): replace the thinking level unless a warning says
    /// otherwise.
    fn with_thinking_level(mut self, level: Option<ThinkingLevel>) -> Self {
        let warned = self.warning().is_some();
        let level = if warned { None } else { level };
        match &mut self {
            ModelReferenceResult::Resolved { thinking_level, .. }
            | ModelReferenceResult::Ambiguous { thinking_level, .. } => {
                *thinking_level = level;
            }
            ModelReferenceResult::NotFound => {}
        }
        self
    }

    fn warning(&self) -> Option<&str> {
        match self {
            ModelReferenceResult::Resolved { warning, .. }
            | ModelReferenceResult::Ambiguous { warning, .. } => warning.as_deref(),
            ModelReferenceResult::NotFound => None,
        }
    }

    /// Whether the lookup failed (`result.status === "not-found"`).
    pub fn is_not_found(&self) -> bool {
        matches!(self, ModelReferenceResult::NotFound)
    }
}

/// `resolveModelReference` (`model-resolver.ts:161-182`): resolve `text`
/// against `models`; `usable` only breaks ties between providers, so a
/// unique match resolves even without credentials.
pub fn resolve_model_reference(
    request: &ModelReferenceRequest<'_>,
    models: &[Model],
    usable: &dyn Fn(&str) -> bool,
) -> ModelReferenceResult {
    let text = request.text.trim();
    if text.is_empty() {
        return ModelReferenceResult::NotFound;
    }
    let constraint = request.provider.map(str::to_lowercase);
    let pool: Vec<&Model> = match &constraint {
        Some(constraint) => models
            .iter()
            .filter(|model| model.provider.to_lowercase() == *constraint)
            .collect(),
        None => models.iter().collect(),
    };
    resolve_text(text, request, &pool, usable)
}

/// `resolveText` (`model-resolver.ts:184-227`).
fn resolve_text(
    text: &str,
    request: &ModelReferenceRequest<'_>,
    pool: &[&Model],
    usable: &dyn Fn(&str) -> bool,
) -> ModelReferenceResult {
    if let Some(reference) = select_reference(text, pool, request.fuzzy, usable) {
        return reference.into_result(None, None);
    }
    if request.fuzzy {
        let fuzzy = select_fuzzy(&fuzzy_matches(text, pool), usable);
        if let Some(fuzzy) = fuzzy {
            return fuzzy.into_result(None, None);
        }
    }
    if request.thinking_suffix == ThinkingSuffix::None {
        return ModelReferenceResult::NotFound;
    }
    if let Some((base, level)) = split_thinking_suffix(text) {
        let inner = resolve_text(base, request, pool, usable);
        if inner.is_not_found() {
            return inner;
        }
        // Only use this thinking level if no warning from the inner recursion
        // (`model-resolver.ts:198-200`).
        return inner.with_thinking_level(Some(level));
    }
    if request.thinking_suffix == ThinkingSuffix::Strict {
        return ModelReferenceResult::NotFound;
    }
    // Keep an invalid suffix as part of the id and fail, rather than resolve
    // a different model (`model-resolver.ts:213-226`).
    let Some(colon_index) = text.rfind(':') else {
        return ModelReferenceResult::NotFound;
    };
    let suffix = &text[colon_index + 1..];
    let inner = resolve_text(&text[..colon_index], request, pool, usable);
    if inner.is_not_found() {
        return inner;
    }
    let warning = format!(
        "Invalid thinking level \"{suffix}\" in pattern \"{text}\". Using default instead."
    );
    apply_warning(inner, Some(warning)).with_thinking_level(None)
}

/// The `{...inner, warning}` spread (`model-resolver.ts:222-226`).
fn apply_warning(
    mut result: ModelReferenceResult,
    warning: Option<String>,
) -> ModelReferenceResult {
    match &mut result {
        ModelReferenceResult::Resolved { warning: slot, .. }
        | ModelReferenceResult::Ambiguous { warning: slot, .. } => *slot = warning,
        ModelReferenceResult::NotFound => {}
    }
    result
}

/// The reference's candidate selection state (`ResolvedOrAmbiguous`,
/// `model-resolver.ts:128`), before it wraps into a [`ModelReferenceResult`].
/// The thinking level and warning ride the recursion's outermost wrap
/// ([`Reference::into_result`]), like upstream's spreads.
enum Reference {
    Resolved {
        model: Model,
    },
    Ambiguous {
        candidates: Vec<Model>,
        usable_count: usize,
    },
}

impl Reference {
    fn into_result(
        self,
        thinking_level: Option<ThinkingLevel>,
        warning: Option<String>,
    ) -> ModelReferenceResult {
        let warned = warning.is_some();
        let level = if warned { None } else { thinking_level };
        match self {
            Reference::Resolved { model } => ModelReferenceResult::Resolved {
                model,
                thinking_level: level,
                warning,
            },
            Reference::Ambiguous {
                candidates,
                usable_count,
            } => ModelReferenceResult::Ambiguous {
                candidates,
                usable_count,
                thinking_level: level,
                warning,
            },
        }
    }
}

/// `selectReference` (`model-resolver.ts:229-280`): `provider/rest` is only a
/// hint — the whole text may also be a model id on another provider. The
/// named provider wins when usable, since that is what the syntax usually
/// means; otherwise a sole usable raw match wins; an unusable named match is
/// still preferred over unusable raw matches, so `--api-key` can set up the
/// named provider.
fn select_reference(
    text: &str,
    pool: &[&Model],
    fuzzy: bool,
    usable: &dyn Fn(&str) -> bool,
) -> Option<Reference> {
    let lower_text = text.to_lowercase();
    let raw: Vec<usize> = pool
        .iter()
        .enumerate()
        .filter(|(_, model)| model.id.to_lowercase() == lower_text)
        .map(|(index, _)| index)
        .collect();
    let named: Option<usize> =
        split_provider_prefix(text, pool).and_then(|(provider, model_text)| {
            let provider_models: Vec<usize> = pool
                .iter()
                .enumerate()
                .filter(|(_, model)| model.provider == provider)
                .map(|(index, _)| index)
                .collect();
            let lower_model_text = model_text.to_lowercase();
            // The provider's own literal id (OpenRouter's `openrouter/free`)
            // beats its fuzzy matches (`model-resolver.ts:247-251`).
            provider_models
                .iter()
                .find(|&&index| pool[index].id.to_lowercase() == lower_model_text)
                .copied()
                .or_else(|| {
                    raw.iter()
                        .find(|&&index| pool[index].provider == provider)
                        .copied()
                })
                .or_else(|| {
                    if !fuzzy {
                        return None;
                    }
                    let matches: Vec<Model> = fuzzy_matches(
                        &model_text,
                        &provider_models
                            .iter()
                            .map(|&index| pool[index])
                            .collect::<Vec<_>>(),
                    );
                    let best = best_fuzzy_match_owned(&matches)?;
                    provider_models
                        .iter()
                        .find(|&&index| {
                            pool[index].provider == best.provider && pool[index].id == best.id
                        })
                        .copied()
                })
        });
    // Identity comparison upstream (`raw.filter((model) => model !== named)`).
    let others: Vec<usize> = raw
        .iter()
        .copied()
        .filter(|index| Some(*index) != named)
        .collect();
    let candidates: Vec<usize> = named.into_iter().chain(others.iter().copied()).collect();
    if candidates.is_empty() {
        return None;
    }
    let unique_providers: HashSet<&str> = candidates
        .iter()
        .map(|&index| pool[index].provider.as_str())
        .collect();
    if unique_providers.len() == 1 {
        return Some(Reference::Resolved {
            model: pool[candidates[0]].clone(),
        });
    }
    if named.is_some_and(|index| usable(&pool[index].provider)) {
        return Some(Reference::Resolved {
            model: pool[named.expect("usable checked the named index")].clone(),
        });
    }
    let usable_others: Vec<usize> = others
        .iter()
        .copied()
        .filter(|&index| usable(&pool[index].provider))
        .collect();
    if usable_others.len() == 1 {
        return Some(Reference::Resolved {
            model: pool[usable_others[0]].clone(),
        });
    }
    if usable_others.is_empty()
        && let Some(index) = named
    {
        return Some(Reference::Resolved {
            model: pool[index].clone(),
        });
    }
    Some(Reference::Ambiguous {
        candidates: candidates
            .iter()
            .map(|&index| pool[index].clone())
            .collect(),
        usable_count: usable_others.len(),
    })
}

/// `fuzzyMatches` (`model-resolver.ts:282-288`).
fn fuzzy_matches(text: &str, models: &[&Model]) -> Vec<Model> {
    let lower_text = text.to_lowercase();
    models
        .iter()
        .filter(|model| {
            model.id.to_lowercase().contains(&lower_text)
                || model.name.to_lowercase().contains(&lower_text)
        })
        .map(|model| (*model).clone())
        .collect()
}

/// `selectFuzzy` (`model-resolver.ts:290-312`).
fn select_fuzzy(matches: &[Model], usable: &dyn Fn(&str) -> bool) -> Option<Reference> {
    if matches.is_empty() {
        return None;
    }
    let multiple_providers = matches
        .iter()
        .map(|model| model.provider.as_str())
        .collect::<HashSet<_>>()
        .len()
        > 1;
    let usable_matches: Vec<Model> = if multiple_providers {
        matches
            .iter()
            .filter(|model| usable(&model.provider))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let pool: Vec<Model> = if usable_matches.is_empty() {
        matches.to_vec()
    } else {
        usable_matches.clone()
    };
    let best = best_fuzzy_match_owned(&pool)?;
    // The same id on several providers would otherwise be decided by catalog
    // order (`model-resolver.ts:297-300`).
    let tied: Vec<Model> = pool
        .iter()
        .filter(|model| model.id == best.id)
        .cloned()
        .collect();
    let tied_providers = tied
        .iter()
        .map(|model| model.provider.as_str())
        .collect::<HashSet<_>>();
    if tied_providers.len() == 1 {
        return Some(Reference::Resolved { model: best });
    }
    let usable_count = if usable_matches.is_empty() {
        0
    } else {
        tied.len()
    };
    Some(Reference::Ambiguous {
        candidates: tied,
        usable_count,
    })
}

/// `bestFuzzyMatch` (`model-resolver.ts:315-319`): prefer aliases
/// (`claude-sonnet-4-5`) over dated versions
/// (`claude-sonnet-4-5-20250929`), then the latest.
fn best_fuzzy_match_owned(matches: &[Model]) -> Option<Model> {
    let aliases: Vec<Model> = matches
        .iter()
        .filter(|model| is_alias(&model.id))
        .cloned()
        .collect();
    let pool: &[Model] = if aliases.is_empty() {
        matches
    } else {
        &aliases
    };
    pool.iter().max_by(|a, b| a.id.cmp(&b.id)).cloned()
}

/// `isAlias` (`model-resolver.ts:321-323`).
fn is_alias(id: &str) -> bool {
    id.ends_with("-latest") || !has_date_suffix(id)
}

/// The `/-\d{8}$` test (`model-resolver.ts:322`).
fn has_date_suffix(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() > 9
        && bytes[bytes.len() - 9] == b'-'
        && bytes[bytes.len() - 8..].iter().all(u8::is_ascii_digit)
}

/// `resolveModelScopeFromModels` (`model-resolver.ts:333-393`): resolve scope
/// patterns (`--models`, `enabledModels`) to models with optional thinking
/// levels. Every model in `models` is usable, so credentials cannot choose
/// between providers offering the same id; all of them are added, with a
/// warning so the user can pick one with `provider/id`.
#[allow(clippy::too_many_lines)] // mirrors the TS resolveModelScopeFromModels; split when extending
pub fn resolve_model_scope_from_models(
    patterns: &[String],
    models: &[Model],
) -> ResolveModelScopeResult {
    let mut result = ResolveModelScopeResult::default();
    let add = |scoped_models: &mut Vec<ScopedModel>,
               model: Model,
               thinking_level: Option<ThinkingLevel>| {
        if !scoped_models
            .iter()
            .any(|scoped| models_are_equal(&scoped.model, &model))
        {
            scoped_models.push(ScopedModel {
                model,
                thinking_level,
            });
        }
    };
    let no_match = |diagnostics: &mut Vec<ModelScopeDiagnostic>, pattern: &str| {
        diagnostics.push(ModelScopeDiagnostic {
            code: ModelScopeDiagnosticCode::NoMatch,
            message: format!("No models match pattern \"{pattern}\""),
            pattern: pattern.to_string(),
        });
    };
    let ambiguous = |diagnostics: &mut Vec<ModelScopeDiagnostic>,
                     pattern: &str,
                     candidates: &[Model]| {
        let mut providers: Vec<String> = candidates
            .iter()
            .map(|model| model.provider.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        providers.sort();
        diagnostics.push(ModelScopeDiagnostic {
            code: ModelScopeDiagnosticCode::Ambiguous,
            message: format!(
                "Pattern \"{pattern}\" matches several providers: {}. Use provider/id to choose one.",
                providers.join(", ")
            ),
            pattern: pattern.to_string(),
        });
    };

    for pattern in patterns {
        // Check if pattern contains glob characters
        // (`model-resolver.ts:346-347`).
        if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
            // Extract optional thinking level suffix (e.g., "provider/*:high").
            let thinking = split_thinking_suffix(pattern);
            let glob_pattern = thinking.map_or(pattern.as_str(), |(base, _)| base);
            let thinking_level = thinking.map(|(_, level)| level);

            // Ids such as `bracketed-model[1m]` are not globs
            // (`model-resolver.ts:353-355`).
            let exact = resolve_model_reference(
                &ModelReferenceRequest {
                    text: glob_pattern,
                    provider: None,
                    thinking_suffix: ThinkingSuffix::None,
                    fuzzy: false,
                },
                models,
                &|_| true,
            );
            let mut matches: Vec<Model> = Vec::new();
            match exact {
                ModelReferenceResult::Resolved { model, .. } => matches.push(model),
                ModelReferenceResult::Ambiguous { candidates, .. } => {
                    matches.extend(candidates.iter().cloned());
                    ambiguous(&mut result.diagnostics, pattern, &candidates);
                }
                ModelReferenceResult::NotFound => {
                    // Match against "provider/modelId" format OR just model ID
                    // (`model-resolver.ts:364-369`): this allows "*sonnet*" to
                    // match without requiring "anthropic/*sonnet*".
                    matches.extend(
                        models
                            .iter()
                            .filter(|model| {
                                glob_matches(
                                    &format!("{}/{}", model.provider, model.id),
                                    glob_pattern,
                                ) || glob_matches(&model.id, glob_pattern)
                            })
                            .cloned(),
                    );
                }
            }
            if matches.is_empty() {
                no_match(&mut result.diagnostics, pattern);
            }
            for model in matches {
                add(&mut result.scoped_models, model, thinking_level);
            }
            continue;
        }

        let reference = resolve_model_reference(
            &ModelReferenceRequest {
                text: pattern,
                provider: None,
                thinking_suffix: ThinkingSuffix::Lenient,
                fuzzy: true,
            },
            models,
            &|_| true,
        );
        match reference {
            ModelReferenceResult::NotFound => {
                no_match(&mut result.diagnostics, pattern);
            }
            ModelReferenceResult::Resolved {
                model,
                thinking_level,
                warning,
            } => {
                if let Some(warning) = warning {
                    result.diagnostics.push(ModelScopeDiagnostic {
                        code: ModelScopeDiagnosticCode::InvalidThinkingLevel,
                        message: warning,
                        pattern: pattern.clone(),
                    });
                }
                add(&mut result.scoped_models, model, thinking_level);
            }
            ModelReferenceResult::Ambiguous {
                candidates,
                thinking_level,
                warning,
                ..
            } => {
                if let Some(warning) = warning {
                    result.diagnostics.push(ModelScopeDiagnostic {
                        code: ModelScopeDiagnosticCode::InvalidThinkingLevel,
                        message: warning,
                        pattern: pattern.clone(),
                    });
                }
                ambiguous(&mut result.diagnostics, pattern, &candidates);
                for model in candidates {
                    add(&mut result.scoped_models, model, thinking_level);
                }
            }
        }
    }

    result
}

/// `minimatch(candidate, pattern, { nocase: true })` through globset
/// (the module doc carries the deviation notes).
fn glob_matches(candidate: &str, pattern: &str) -> bool {
    globset::GlobBuilder::new(pattern)
        .case_insensitive(true)
        .literal_separator(true)
        .build()
        .is_ok_and(|glob| glob.compile_matcher().is_match(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_agent::types::ThinkingLevel;

    fn model(provider: &str, id: &str, name: &str) -> Model {
        Model {
            provider: provider.to_string(),
            id: id.to_string(),
            name: name.to_string(),
            ..Model::default()
        }
    }

    fn scope(patterns: &[&str], models: &[Model]) -> ResolveModelScopeResult {
        let owned: Vec<String> = patterns
            .iter()
            .map(|pattern| (*pattern).to_string())
            .collect();
        resolve_model_scope_from_models(&owned, models)
    }

    // The upstream `resolveModelScopeWithDiagnostics` tests
    // (`test/model-resolver.test.ts:349-463`), mirrored behaviorally.

    #[test]
    fn resolves_levels_and_reports_diagnostics_without_side_channels() {
        let models = vec![
            model("anthropic", "claude-sonnet-4-5", "Claude Sonnet 4.5"),
            model("openai", "gpt-4o", "GPT-4o"),
        ];
        let result = scope(&["sonnet:high", "gpt-4o:invalid", "missing"], &models);
        let ids: Vec<&str> = result
            .scoped_models
            .iter()
            .map(|scoped| scoped.model.id.as_str())
            .collect();
        assert_eq!(ids, ["claude-sonnet-4-5", "gpt-4o"]);
        assert_eq!(
            result.scoped_models[0].thinking_level,
            Some(ThinkingLevel::High)
        );
        assert_eq!(result.scoped_models[1].thinking_level, None);
        assert_eq!(
            result.diagnostics,
            vec![
                ModelScopeDiagnostic {
                    code: ModelScopeDiagnosticCode::InvalidThinkingLevel,
                    message:
                        "Invalid thinking level \"invalid\" in pattern \"gpt-4o:invalid\". Using default instead."
                            .to_string(),
                    pattern: "gpt-4o:invalid".to_string(),
                },
                ModelScopeDiagnostic {
                    code: ModelScopeDiagnosticCode::NoMatch,
                    message: "No models match pattern \"missing\"".to_string(),
                    pattern: "missing".to_string(),
                },
            ]
        );
    }

    #[test]
    fn adds_every_provider_for_a_cross_provider_id_with_a_warning() {
        let models = vec![
            model("azure", "gpt-4o", "GPT-4o"),
            model("openai", "gpt-4o", "GPT-4o"),
        ];
        for (pattern, level) in [("gpt-4o:high", Some(ThinkingLevel::High)), ("GPT-4o", None)] {
            let result = scope(&[pattern], &models);
            let resolved: Vec<(String, Option<ThinkingLevel>)> = result
                .scoped_models
                .iter()
                .map(|scoped| {
                    (
                        format!("{}/{}", scoped.model.provider, scoped.model.id),
                        scoped.thinking_level,
                    )
                })
                .collect();
            assert_eq!(resolved.len(), 2, "{pattern}");
            assert!(
                resolved.contains(&("azure/gpt-4o".to_string(), level)),
                "{pattern}"
            );
            assert!(
                resolved.contains(&("openai/gpt-4o".to_string(), level)),
                "{pattern}"
            );
            // Upstream's message carries the pattern verbatim, suffix
            // included (`model-resolver.test.ts:410`).
            assert_eq!(
                result.diagnostics,
                vec![ModelScopeDiagnostic {
                    code: ModelScopeDiagnosticCode::Ambiguous,
                    message: format!(
                        "Pattern \"{pattern}\" matches several providers: azure, openai. Use provider/id to choose one."
                    ),
                    pattern: pattern.to_string(),
                }]
            );
        }
    }

    #[test]
    fn resolves_bracketed_ids_as_exact_references_before_glob_matching() {
        let models = vec![model("custom", "bracketed-model[1m]", "Bracketed Model")];
        let result = scope(&["custom/bracketed-model[1m]"], &models);
        let ids: Vec<&str> = result
            .scoped_models
            .iter()
            .map(|scoped| scoped.model.id.as_str())
            .collect();
        assert_eq!(ids, ["bracketed-model[1m]"]);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }

    #[test]
    fn resolves_bracketed_ids_with_thinking_levels() {
        let models = vec![model("custom", "bracketed-model[1m]", "Bracketed Model")];
        let result = scope(&["custom/bracketed-model[1m]:high"], &models);
        let ids: Vec<&str> = result
            .scoped_models
            .iter()
            .map(|scoped| scoped.model.id.as_str())
            .collect();
        assert_eq!(ids, ["bracketed-model[1m]"]);
        assert_eq!(
            result.scoped_models[0].thinking_level,
            Some(ThinkingLevel::High)
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }

    #[test]
    fn globs_match_provider_id_and_bare_id_case_insensitively() {
        let models = vec![
            model("anthropic", "claude-sonnet-4-5", "Claude"),
            model("openai", "gpt-5-mini", "GPT-5 mini"),
        ];
        let result = scope(&["*sonnet*", "anthropic/*", "GPT-5*"], &models);
        let ids: Vec<&str> = result
            .scoped_models
            .iter()
            .map(|scoped| scoped.model.id.as_str())
            .collect();
        // "*sonnet*" matches the bare id, "anthropic/*" the provider path —
        // deduplicated to one entry per model; "GPT-5*" is case-insensitive.
        assert_eq!(ids, ["claude-sonnet-4-5", "gpt-5-mini"], "{result:?}");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }

    #[test]
    fn no_match_diagnostic_surfaces_unresolvable_glob_patterns() {
        let models = vec![model("anthropic", "claude-sonnet-4-5", "Claude")];
        let result = scope(&["openai/*"], &models);
        assert!(result.scoped_models.is_empty());
        assert_eq!(
            result.diagnostics,
            vec![ModelScopeDiagnostic {
                code: ModelScopeDiagnosticCode::NoMatch,
                message: "No models match pattern \"openai/*\"".to_string(),
                pattern: "openai/*".to_string(),
            }]
        );
    }

    #[test]
    fn deduplicates_repeated_models_keeping_the_first_level() {
        let models = vec![model("anthropic", "claude-sonnet-4-5", "Claude")];
        let result = scope(
            &["claude-sonnet-4-5:low", "anthropic/claude-sonnet-4-5:high"],
            &models,
        );
        assert_eq!(result.scoped_models.len(), 1);
        assert_eq!(
            result.scoped_models[0].thinking_level,
            Some(ThinkingLevel::Low)
        );
    }

    // `resolveModelReference` behavior the scope resolver leans on
    // (`test/model-resolver.test.ts:317-347` covers it directly upstream).

    #[test]
    fn reference_prefers_the_named_provider_over_fuzzy_others() {
        let models = vec![
            model("openrouter", "zai/glm-5", "GLM"),
            model("zai", "glm-5", "GLM"),
        ];
        // `zai/glm-5` names the zai provider's literal id... which is also
        // openrouter's full id. The named provider wins when usable.
        let result = resolve_model_reference(
            &ModelReferenceRequest {
                text: "zai/glm-5",
                provider: None,
                thinking_suffix: ThinkingSuffix::None,
                fuzzy: true,
            },
            &models,
            &|_| true,
        );
        let ModelReferenceResult::Resolved { model, .. } = result else {
            panic!("expected a resolution, got {result:?}");
        };
        assert_eq!(
            (model.provider.as_str(), model.id.as_str()),
            ("zai", "glm-5")
        );
    }

    #[test]
    fn reference_reports_ambiguity_across_providers() {
        let models = vec![model("a", "one", ""), model("b", "one", "")];
        let result = resolve_model_reference(
            &ModelReferenceRequest {
                text: "one",
                provider: None,
                thinking_suffix: ThinkingSuffix::Lenient,
                fuzzy: true,
            },
            &models,
            &|_| true,
        );
        let ModelReferenceResult::Ambiguous { candidates, .. } = result else {
            panic!("expected ambiguity, got {result:?}");
        };
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn reference_lenient_suffix_warns_on_invalid_levels() {
        let models = vec![model("openai", "gpt-4o", "GPT-4o")];
        let result = resolve_model_reference(
            &ModelReferenceRequest {
                text: "gpt-4o:invalid",
                provider: None,
                thinking_suffix: ThinkingSuffix::Lenient,
                fuzzy: true,
            },
            &models,
            &|_| true,
        );
        let ModelReferenceResult::Resolved {
            model,
            thinking_level,
            warning,
        } = result
        else {
            panic!("expected a resolution, got {result:?}");
        };
        assert_eq!(model.id, "gpt-4o");
        assert_eq!(thinking_level, None);
        assert_eq!(
            warning.as_deref(),
            Some(
                "Invalid thinking level \"invalid\" in pattern \"gpt-4o:invalid\". Using default instead."
            )
        );
    }
}
