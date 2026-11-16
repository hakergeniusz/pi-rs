//! Print mode (single-shot): send prompts, output the result, exit.
//!
//! Port of `modes/print-mode.ts` (182 lines) plus the model/credential
//! resolution the CLI host performs before it. Faithful where P4 can be:
//! text mode with the same output/exit-code contract. Deferred, tracked for
//! later phases: JSON event output (`--mode json`, P5), signal handlers and
//! stdout backpressure (`core/output-guard.ts`, P7), extension binding (P5).
//! `@file` attachments are handled (`cli/file-processor.ts`).

use std::path::Path;
use std::sync::Arc;

use pi_agent::agent_loop::{AgentLoopConfig, DefaultConvertToLlm};
use pi_agent::agent_message::AgentMessage;
use pi_agent::events::{AgentEventSink, CollectingSink};
use pi_agent::queues::Queues;
use pi_agent::transcript::tool_declaration;
use pi_agent::types::{
    AbortSignal, AgentContext, StreamFn, ThinkingLevel, ToolExecutionMode, ToolExecutor, ToolSpec,
};
use pi_ai::types::{ImageContent, Model, StopReason};

use crate::auth::{self, Credential};
use crate::cli::args::{Args, Mode};
use crate::config::settings::RetrySettings;
use crate::session::store::{SessionManager, build_session_context_from};
use crate::tools::index::{ToolDefinition, ToolRegistry};

/// `PrintModeOptions` (`print-mode.ts:18-27`).
#[derive(Debug, Clone)]
pub struct PrintModeOptions {
    /// `"text"` prints the final response; `"json"` is a P5 mode.
    pub mode: Mode,
    /// Prompts sent after `initial_message` (`print-mode.ts:21`).
    pub messages: Vec<String>,
    /// First message to send (`print-mode.ts:23`).
    pub initial_message: Option<String>,
    /// `@file` image attachments riding with `initial_message`
    /// (`initialImages`, `main.ts:268-277`).
    pub initial_images: Vec<ImageContent>,
}

/// Everything one print run needs, resolved by the CLI host before the loop.
pub struct PrintRuntime {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    /// Wire credential, already resolved through the auth ladder.
    pub api_key: Option<String>,
    /// OAuth bearer token from the ladder (`Authorization: Bearer`).
    pub access_token: Option<String>,
    pub stream_fn: Arc<dyn StreamFn>,
    /// The composite executor: builtins + extension tool dispatch (E2
    /// slice 3). Without extensions it is the plain builtin executor.
    pub executor: Arc<dyn ToolExecutor>,
    pub system_prompt: String,
    pub session: SessionManager,
    /// The resolved trust decision, shared with the interactive host
    /// (extensions may decide through the `project_trust` hook).
    pub project_trusted: bool,
    /// The auto-retry budget (`settings.retry`), driving the run tails.
    pub retry_settings: RetrySettings,
    /// The automatic-compaction settings (`settings.compaction`), driving
    /// the run tails' threshold/overflow machine.
    pub compaction_settings: crate::session::compaction::CompactionSettings,
    /// The extension runner the mode-level surfaces share (E2 slice 4):
    /// the RPC dialog bridge, the `user_bash` interception, the session
    /// gates, and the context transform. `None` without the engine.
    #[cfg(feature = "ext-js")]
    pub extension_runner: Option<Arc<std::sync::Mutex<pi_ext_js::ExtensionRunner>>>,
}

impl PrintRuntime {
    /// The `transformContext` hook (`sdk.ts:430-434`) when extension
    /// `context`/`context_with_system` handlers exist; `None` otherwise
    /// (the loop then runs upstream's no-transform identity). Present in
    /// both builds — headless `None` without the engine — so the four
    /// `AgentLoopConfig` sites need no cfg.
    #[cfg(feature = "ext-js")]
    #[must_use]
    pub fn transform_context(&self) -> Option<Arc<dyn pi_agent::TransformContext>> {
        let runner = self.extension_runner.as_ref()?;
        crate::extensions::context_transform(Arc::clone(runner))
    }

    /// Headless twin: no engine, no transform.
    #[cfg(not(feature = "ext-js"))]
    #[must_use]
    pub fn transform_context(&self) -> Option<Arc<dyn pi_agent::TransformContext>> {
        None
    }
}

/// `runPrintMode` (`print-mode.ts:33-181`), minus signals/backpressure (P7)
/// and extension binding (P5). Returns the process exit code.
///
/// Unlike upstream — which persists messages through the event stream as
/// they arrive — the port appends the persisted model/thinking changes and
/// every returned message after the loop; the JSONL bytes are the same for
/// a completed run.
// The 100-line cap fights the linear port of upstream's print-mode run
// sequence; kept 1:1 for parity-auditability.
#[allow(clippy::too_many_lines)]
pub fn run_print_mode(
    runtime: &mut PrintRuntime,
    options: &PrintModeOptions,
) -> Result<i32, String> {
    if options.mode == Mode::Json {
        return Err("json mode is not implemented yet (phase P5); use text mode".to_string());
    }

    let executor = runtime.executor.clone();
    let specs: Vec<ToolSpec> = executor.tools();

    // Session context: the transcript so far, or a fresh system message.
    let leaf_id = runtime.session.get_leaf_id().map(str::to_string);
    let session_context =
        build_session_context_from(&runtime.session.get_entries(), leaf_id.as_deref());
    let mut context_messages = session_context.messages;
    if context_messages.is_empty() {
        if let Some(system) = pi_agent::transcript::create_initial_system_message(
            Some(&runtime.system_prompt),
            &specs.iter().map(tool_declaration).collect::<Vec<_>>(),
        ) {
            context_messages.push(AgentMessage::System(system));
        }
    }
    let context = AgentContext {
        messages: context_messages,
        tools: specs.clone(),
    };

    // Record the model/thinking selection the way pi's session does, unless
    // a resumed session already carries it.
    let session_model = session_context.model.clone();
    let model_changed = session_model.as_ref().is_none_or(|reference| {
        reference.provider != runtime.model.provider || reference.model_id != runtime.model.id
    });
    if model_changed {
        runtime
            .session
            .append_model_change(&runtime.model.provider, &runtime.model.id)
            .map_err(|error| error.to_string())?;
    }
    let requested_level = runtime.thinking_level.as_str().to_string();
    if session_context.thinking_level != requested_level {
        runtime
            .session
            .append_thinking_level_change(&requested_level)
            .map_err(|error| error.to_string())?;
    }

    // Prompts (`print-mode.ts:138-144`).
    let mut prompts: Vec<AgentMessage> = Vec::new();
    let now = now_millis();
    if let Some(initial) = &options.initial_message {
        if options.initial_images.is_empty() {
            prompts.push(pi_agent::transcript::user_message(initial, now));
        } else {
            prompts.push(pi_agent::transcript::user_message_with_images(
                initial,
                &options.initial_images,
                now,
            ));
        }
    }
    for message in &options.messages {
        prompts.push(pi_agent::transcript::user_message(message, now));
    }

    let mut queues = Queues::new();
    let mut sink = CollectingSink { events: Vec::new() };
    let signal = Arc::new(AbortSignal::new());
    // Under a virtual selection every request routes first
    // (`_installAgentRequestProjection`): the hook replaces the model per
    // request and the stream dispatches per routed model.
    #[cfg(feature = "ext-js")]
    let routing = crate::virtual_models::TurnRouting::install(
        &runtime.model,
        runtime.extension_runner.as_ref(),
        runtime.api_key.clone(),
        &runtime.session,
    );
    let config = AgentLoopConfig {
        model: runtime.model.clone(),
        thinking_level: runtime.thinking_level,
        live_settings: None,
        session_id: None,
        api_key: runtime.api_key.clone(),
        access_token: runtime.access_token.clone(),
        tool_execution: ToolExecutionMode::Parallel,
        hooks: pi_agent::types::ToolCallHooks {
            before: None,
            after: None,
        },
        finish_turn: None,
        #[cfg(feature = "ext-js")]
        prepare_request: routing.as_ref().map(|routing| {
            Arc::clone(&routing.hook) as Arc<dyn pi_agent::agent_loop::PrepareRequest>
        }),
        #[cfg(not(feature = "ext-js"))]
        prepare_request: None,
        prepare_next_turn: None,
        transform_context: runtime.transform_context(),
        convert_to_llm: Arc::new(DefaultConvertToLlm),
    };

    #[cfg(feature = "ext-js")]
    let loop_stream_fn: Arc<dyn pi_agent::types::StreamFn> = match &routing {
        Some(routing) => routing.stream_fn(),
        None => Arc::clone(&runtime.stream_fn),
    };
    #[cfg(not(feature = "ext-js"))]
    let loop_stream_fn: Arc<dyn pi_agent::types::StreamFn> = Arc::clone(&runtime.stream_fn);
    // The auto-retry machine (`agent-session.ts` `_handlePostAgentRun`): a
    // retryable error within budget re-runs the same prompt after a backoff;
    // the failed response is never persisted.
    let mut retry = crate::modes::retry::AgentRetry::new(runtime.retry_settings);

    let mut compaction = crate::modes::compaction::AutoCompaction::new(runtime.compaction_settings);
    let new_messages = loop {
        // `_compactBeforeNextAssistantResponse` (`agent-session.ts:839-848`):
        // the threshold gate keys on the model about to answer; under a
        // virtual selection upstream re-checks the ROUTED model after the
        // resolve (:901-904) — deferred with the prepare-hook rework.
        if !crate::virtual_models::is_virtual_model(&runtime.model)
            && compaction.exceeds_threshold_for_session(&runtime.session, &runtime.model)
        {
            compaction.run(
                pi_agent::events::CompactionReason::Threshold,
                false,
                &mut runtime.session,
                &runtime.model,
                runtime.api_key.as_deref(),
                runtime.access_token.as_deref(),
                runtime.thinking_level,
                runtime.stream_fn.as_ref(),
                &mut sink,
            );
        }
        let messages = {
            // `agent_end` carries the merged `willRetry`
            // (`agent-session.ts:1211`).
            let mut aware = crate::modes::retry::RetryAwareSink {
                inner: &mut sink,
                retry_attempt: retry.attempt(),
                settings: runtime.retry_settings,
                signal: Arc::clone(&signal),
            };
            crate::runtime::block_on(pi_agent::run_agent_loop(
                prompts.clone(),
                context.clone(),
                executor.as_ref(),
                &config,
                &mut queues,
                &mut aware,
                &signal,
                loop_stream_fn.as_ref(),
            ))
        };
        let aborted = signal.aborted();
        if retry.will_retry(aborted, &messages) {
            let (attempt, max_attempts, delay_ms, error_message) = retry.schedule(&messages);
            // `_prepareRetry` (`agent-session.ts:3959-3963`): the failed
            // attempt persists raw and drops out of the projection.
            for entry in crate::modes::retry::omit_attempt(&messages, &mut runtime.session) {
                crate::runtime::block_on(
                    sink.emit(pi_agent::events::AgentEvent::EntryAppended { entry }),
                );
            }
            crate::runtime::block_on(sink.emit(pi_agent::events::AgentEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                error_message: error_message.clone(),
            }));
            if retry.sleep(delay_ms, &signal) {
                continue;
            }
            crate::runtime::block_on(sink.emit(pi_agent::events::AgentEvent::AutoRetryEnd {
                success: false,
                attempt,
                final_error: Some(error_message),
            }));
            break messages;
        }
        if retry.attempt() > 0 {
            let success = crate::modes::retry::run_succeeded(&messages);
            crate::runtime::block_on(sink.emit(pi_agent::events::AgentEvent::AutoRetryEnd {
                success,
                attempt: retry.attempt(),
                final_error: if success {
                    None
                } else {
                    Some(crate::modes::retry::last_error_text(&messages))
                },
            }));
            if success {
                retry.reset();
            }
        }
        // Upstream `_handlePostAgentRun` (`agent-session.ts:2062-2098`)
        // decides the retry first — compaction only runs when no retry was
        // scheduled.
        let mut full_messages = context.messages.clone();
        full_messages.extend(messages.iter().cloned());
        match compaction.post_run_decision(
            aborted,
            &messages,
            &full_messages,
            &runtime.session,
            &runtime.model,
        ) {
            crate::modes::compaction::Decision::None => break messages,
            crate::modes::compaction::Decision::Threshold => {
                compaction.run(
                    pi_agent::events::CompactionReason::Threshold,
                    false,
                    &mut runtime.session,
                    &runtime.model,
                    runtime.api_key.as_deref(),
                    runtime.access_token.as_deref(),
                    runtime.thinking_level,
                    runtime.stream_fn.as_ref(),
                    &mut sink,
                );
                break messages;
            }
            crate::modes::compaction::Decision::Overflow { will_retry } => {
                if will_retry {
                    // `_omitRecoveryAttempt` before the recovery compaction
                    // (`agent-session.ts:3209-3211`).
                    for entry in crate::modes::retry::omit_attempt(&messages, &mut runtime.session)
                    {
                        crate::runtime::block_on(
                            sink.emit(pi_agent::events::AgentEvent::EntryAppended { entry }),
                        );
                    }
                }
                // The overflow compact-and-retry re-runs the turn once
                // (`agent.continue()`).
                let continued = compaction.run(
                    pi_agent::events::CompactionReason::Overflow,
                    will_retry,
                    &mut runtime.session,
                    &runtime.model,
                    runtime.api_key.as_deref(),
                    runtime.access_token.as_deref(),
                    runtime.thinking_level,
                    runtime.stream_fn.as_ref(),
                    &mut sink,
                );
                if will_retry && continued {
                    continue;
                }
                break messages;
            }
            crate::modes::compaction::Decision::RecoveryFailed { message } => {
                // The verbatim one-shot failure (`agent-session.ts:3187-3206`).
                crate::runtime::block_on(sink.emit(pi_agent::events::AgentEvent::CompactionEnd {
                    reason: pi_agent::events::CompactionReason::Overflow,
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(message),
                }));
                break messages;
            }
        }
    };
    // Router state decided this turn lands on the branch now (upstream
    // appends mid-turn; the store is not shared with the loop thread).
    #[cfg(feature = "ext-js")]
    if let Some(routing) = &routing {
        // `entry_appended` for each router-state record
        // (`agent-session.ts:894-897`).
        for entry in routing.persist_state(&mut runtime.session) {
            crate::runtime::block_on(
                sink.emit(pi_agent::events::AgentEvent::EntryAppended { entry }),
            );
        }
    }

    // Persist the turn (`print-mode.ts` persists via MessageEnd events; the
    // completed-run bytes are identical).
    for message in &new_messages {
        runtime
            .session
            .append_message(message.clone())
            .map_err(|error| error.to_string())?;
    }

    // Output contract (`print-mode.ts:146-167`).
    let Some(last) = new_messages.last() else {
        return Ok(0);
    };
    let AgentMessage::Assistant(assistant) = last else {
        return Ok(0);
    };
    if assistant.stop_reason == StopReason::Error || assistant.stop_reason == StopReason::Aborted {
        eprintln!(
            "{}",
            assistant
                .error_message
                .clone()
                .unwrap_or_else(|| format!("Request {}", assistant.stop_reason.as_str()))
        );
        return Ok(1);
    }
    for block in &assistant.content {
        if let pi_ai::types::AssistantContent::Text(text) = block {
            println!("{}", text.text);
        }
    }
    if assistant.stop_reason == StopReason::Length {
        eprintln!("Response was truncated: the model reached its output token limit.");
        return Ok(1);
    }
    Ok(0)
}

pub(crate) fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
}

/// Bridge pi-ai's provider stream functions onto the agent loop's
/// [`pi_agent::types::StreamFn`] seam. Upstream composes the same pieces in
/// `agent-session.ts` (`streamFn` wiring); here it is one explicit adapter.
///
/// `xhigh`/`max` clamp to `High` until pi-ai's `StreamOptions` models the
/// extended levels (P5 provider work).
pub struct ProviderStreamFn(pub pi_ai::providers::StreamFn);

impl pi_agent::types::StreamFn for ProviderStreamFn {
    fn stream(
        &self,
        request: pi_agent::types::StreamRequest,
    ) -> futures::stream::BoxStream<'static, pi_agent::types::AssistantMessageEvent> {
        let messages: Vec<pi_ai::types::Message> = request
            .messages
            .iter()
            .filter_map(AgentMessage::to_llm_message)
            .collect();
        let context = pi_ai::types::TranscriptContext { messages };
        let options = pi_ai::types::StreamOptions {
            api_key: request.api_key,
            access_token: request.access_token,
            reasoning: Some(match request.thinking_level {
                ThinkingLevel::Off => pi_ai::types::ThinkingLevel::Off,
                ThinkingLevel::Minimal => pi_ai::types::ThinkingLevel::Minimal,
                ThinkingLevel::Low => pi_ai::types::ThinkingLevel::Low,
                ThinkingLevel::Medium => pi_ai::types::ThinkingLevel::Medium,
                ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => {
                    pi_ai::types::ThinkingLevel::High
                }
            }),
            session_id: request.session_id,
            ..pi_ai::types::StreamOptions::default()
        };
        let event_stream = (self.0)(&request.model, &context, &options);
        Box::pin(event_stream)
    }
}

/// A model pattern with its optional `:thinking` suffix, split out of
/// `--model` (`args.ts:301-302` documents the shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPattern {
    pub provider: Option<String>,
    pub id: String,
    pub thinking: Option<ThinkingLevel>,
}

/// Split `provider/id:level` / `id:level` patterns. The suffix only counts
/// when it names a valid thinking level, so ids containing `:` degrade to a
/// plain pattern (same as pi's matcher).
pub fn parse_model_pattern(pattern: &str) -> ModelPattern {
    let (rest, thinking) = match pattern.rsplit_once(':') {
        Some((head, tail)) => match thinking_level_from_str(tail) {
            Ok(level) => (head, Some(level)),
            Err(_) => (pattern, None),
        },
        None => (pattern, None),
    };
    match rest.split_once('/') {
        Some((provider, id)) if !provider.is_empty() && !id.is_empty() => ModelPattern {
            provider: Some(provider.to_string()),
            id: id.to_string(),
            thinking,
        },
        _ => ModelPattern {
            provider: None,
            id: rest.to_string(),
            thinking,
        },
    }
}

/// Parse a `--thinking` value into the loop's level (`args.ts:61-65`).
pub fn thinking_level_from_str(level: &str) -> Result<ThinkingLevel, String> {
    match level {
        "off" => Ok(ThinkingLevel::Off),
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "xhigh" => Ok(ThinkingLevel::Xhigh),
        "max" => Ok(ThinkingLevel::Max),
        other => Err(format!(
            "Invalid thinking level \"{other}\". Valid values: {}",
            crate::cli::args::VALID_THINKING_LEVELS.join(", ")
        )),
    }
}

/// Resolve `--model` (or the settings default) against the embedded catalog.
///
/// `default_provider`/`default_model` come from settings
/// (`settings-manager.ts` `defaultProvider`/`defaultModel`).
pub fn resolve_model(
    pattern: Option<&ModelPattern>,
    default_provider: Option<&str>,
    default_model: Option<&str>,
) -> Result<Model, String> {
    resolve_model_with(pattern, default_provider, default_model, &[])
}

/// [`resolve_model`] over a surface that also carries extension-registered
/// virtual models (`withVirtualModels`): a virtual model hides a physical
/// catalog model with the same (provider, id), and bare-id patterns match
/// virtual models too.
pub fn resolve_model_with(
    pattern: Option<&ModelPattern>,
    default_provider: Option<&str>,
    default_model: Option<&str>,
    virtual_models: &[Model],
) -> Result<Model, String> {
    let lookup = |provider: &str, id: &str| -> Option<Model> {
        virtual_models
            .iter()
            .find(|model| model.provider == provider && model.id == id)
            .cloned()
            .or_else(|| crate::models_state::lookup(provider, id))
    };
    let Some(pattern) = pattern else {
        // Fall back to the settings default; otherwise ask for a model.
        return match (default_provider, default_model) {
            (Some(provider), Some(id)) => lookup(provider, id)
                .ok_or_else(|| format!("Default model not found in catalog: {provider}/{id}")),
            _ => Err("No model specified. Use --model <pattern> (e.g. anthropic/claude-sonnet-4) or set defaultProvider/defaultModel in settings".to_string()),
        };
    };
    let found = if let Some(provider) = &pattern.provider {
        lookup(provider, &pattern.id)
            .ok_or_else(|| format!("Model not found: {provider}/{}", pattern.id))?
    } else {
        let mut matches: Vec<Model> = virtual_models
            .iter()
            .filter(|model| model.id == pattern.id)
            .cloned()
            .collect();
        matches.extend(
            crate::models_state::find_by_id(&pattern.id)
                .into_iter()
                .filter(|model| {
                    !virtual_models.iter().any(|virtual_model| {
                        virtual_model.provider == model.provider && virtual_model.id == model.id
                    })
                }),
        );
        match matches.len() {
            0 => return Err(format!("Model not found: {}", pattern.id)),
            1 => matches.remove(0),
            _ => {
                let candidates: Vec<String> = matches
                    .iter()
                    .map(|model| format!("{}/{}", model.provider, model.id))
                    .collect();
                return Err(format!(
                    "Model pattern \"{}\" matches multiple models; be specific: {}",
                    pattern.id,
                    candidates.join(", ")
                ));
            }
        }
    };
    Ok(found)
}

/// Provider-specific API-key environment variables, in the order each TS
/// provider checks them (`packages/ai/src/env-api-keys.ts:73-129`, verified).
/// `ANTHROPIC_AUTH_TOKEN` participates in discovery but rides the wire as a
/// Bearer header (see [`resolve_wire_credential`]); `amazon-bedrock` and
/// `google-vertex` ADC paths stay out of this table.
pub(crate) fn env_key_candidates(provider: &str) -> &'static [&'static str] {
    match provider {
        "anthropic" => &[
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ],
        "openai" => &["OPENAI_API_KEY"],
        "azure" => &["AZURE_OPENAI_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "nvidia" => &["NVIDIA_API_KEY"],
        "google" => &["GEMINI_API_KEY"],
        "google-vertex" => &["GOOGLE_CLOUD_API_KEY"],
        "groq" => &["GROQ_API_KEY"],
        "cerebras" => &["CEREBRAS_API_KEY"],
        "xai" => &["XAI_API_KEY"],
        "fireworks" => &["FIREWORKS_API_KEY"],
        "together" => &["TOGETHER_API_KEY"],
        "baseten" => &["BASETEN_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "vercel-ai-gateway" => &["AI_GATEWAY_API_KEY"],
        "zai" => &["ZAI_API_KEY"],
        "zai-coding-cn" => &["ZAI_CODING_CN_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        "minimax" => &["MINIMAX_API_KEY"],
        "minimax-cn" => &["MINIMAX_CN_API_KEY"],
        "moonshotai" | "moonshotai-cn" => &["MOONSHOT_API_KEY"],
        "opencode" | "opencode-go" => &["OPENCODE_API_KEY"],
        "kimi-coding" => &["KIMI_API_KEY"],
        "meta" => &["META_API_KEY"],
        "cloudflare-workers-ai" | "cloudflare-ai-gateway" => &["CLOUDFLARE_API_KEY"],
        // Qwen Token Plan: the per-plan variable first, then the region
        // variable both plans used to share (`env-api-keys.ts:84-90`).
        "qwen-token-plan" => &["QWEN_TOKEN_PLAN_TEAM_API_KEY", "QWEN_TOKEN_PLAN_API_KEY"],
        "qwen-token-plan-cn" => &[
            "QWEN_TOKEN_PLAN_TEAM_CN_API_KEY",
            "QWEN_TOKEN_PLAN_CN_API_KEY",
        ],
        "qwen-token-plan-individual" => &[
            "QWEN_TOKEN_PLAN_INDIVIDUAL_API_KEY",
            "QWEN_TOKEN_PLAN_API_KEY",
        ],
        "qwen-token-plan-individual-cn" => &[
            "QWEN_TOKEN_PLAN_INDIVIDUAL_CN_API_KEY",
            "QWEN_TOKEN_PLAN_CN_API_KEY",
        ],
        "xiaomi" => &["XIAOMI_API_KEY"],
        "xiaomi-token-plan-cn" => &["XIAOMI_TOKEN_PLAN_CN_API_KEY"],
        "xiaomi-token-plan-ams" => &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"],
        "xiaomi-token-plan-sgp" => &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"],
        "ant-ling" => &["ANT_LING_API_KEY"],
        "github-copilot" => &["COPILOT_GITHUB_TOKEN"],
        "huggingface" => &["HF_TOKEN"],
        "radius" => &["RADIUS_API_KEY"],
        "typesafe" => &["TYPESAFE_API_KEY"],
        _ => &[],
    }
}

/// The wire credential the ladder resolved, with its header semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireCredential {
    /// Provider API key (`x-api-key`-style headers).
    ApiKey(String),
    /// OAuth bearer token (`Authorization: Bearer`).
    AccessToken(String),
}

/// The auth ladder for one request (`providers/anthropic.ts:30-45` and
/// friends, composed through `auth/resolve.ts:33-93` and
/// `provider-composer.ts:422-511`): the `--api-key` flag (the port's runtime
/// credential; it wins for any provider id — see
/// [`resolve_wire_credential_composed`]), then the stored credential from
/// `auth.json` — which owns the provider: a stored credential whose type the
/// provider does not support resolves to nothing with no env fallback — then
/// the configured models.json/extension `apiKey`, then provider environment
/// variables. Returns the credential and its source, keeping bearer tokens
/// distinct from API keys so the adapters can pick the right header.
pub fn resolve_wire_credential(
    provider: &str,
    flag_key: Option<&str>,
    auth_path: &Path,
) -> Option<(WireCredential, &'static str)> {
    // The composed config layers (`model-runtime.ts`'s `this.config` and
    // `extensionProviders`); `None` before a refresh — the engine-free shape.
    let config = crate::models_state::models_json_provider(provider);
    let extension = crate::models_state::extension_provider_config(provider);
    resolve_wire_credential_composed(
        provider,
        flag_key,
        auth_path,
        config.as_ref(),
        extension.as_ref(),
    )
}

/// [`resolve_wire_credential`] over explicit config layers (the tests' seam;
/// the production path reads the composed snapshots).
pub fn resolve_wire_credential_composed(
    provider: &str,
    flag_key: Option<&str>,
    auth_path: &Path,
    config: Option<&crate::models_config::ModelsJsonProvider>,
    extension: Option<&crate::models_config::ExtensionProviderConfig>,
) -> Option<(WireCredential, &'static str)> {
    let methods = crate::oauth::provider_auth_methods(provider, config, extension);
    let has_api_key = methods
        .iter()
        .any(|method| matches!(method, crate::oauth::AuthMethod::ApiKey(_)));
    let has_oauth = methods
        .iter()
        .any(|method| matches!(method, crate::oauth::AuthMethod::OAuth(_)));
    // The `--api-key` flag is the port's runtime credential
    // (`credentials.setRuntimeApiKey`, `model-runtime.ts:655-665` reports it
    // configured first): it wins over every layer, for any provider id.
    // Upstream's `overrides.apiKey` requires `provider.auth.apiKey`
    // (`resolve.ts:56-68`); the port keeps the unconditional P4 behavior
    // because the flag is the runtime layer here, not a request override.
    if let Some(key) = flag_key {
        return Some((WireCredential::ApiKey(key.to_string()), "--api-key"));
    }
    if let Some(credential) = auth::read_stored_credential(provider, auth_path) {
        match &credential {
            Credential::ApiKey { key, env, .. } => {
                // A stored API key for an OAuth-only provider resolves to
                // nothing — no env fallback (`resolve.ts:82-86`).
                if !has_api_key {
                    return None;
                }
                if let Some(resolved) = key
                    .as_deref()
                    .and_then(|key| auth::resolve_config_value(key, env.as_ref()))
                {
                    return Some((WireCredential::ApiKey(resolved), "stored credential"));
                }
                // An unresolvable stored template falls through to the env
                // loop inside `envApiKeyAuth.resolve` (`helpers.ts:21-29`).
            }
            Credential::Oauth { access, .. } => {
                // A stored OAuth credential for a provider without OAuth
                // auth resolves to nothing (`resolve.ts:72-86`).
                if !has_oauth {
                    return None;
                }
                // Extension oauth providers resolve the wire key through
                // the stashed `getApiKey(credential)` (`adaptOAuth`'s
                // `toAuth`, `provider-composer.ts:375`) — the handler owns
                // what reaches the request, so the built-in arm below does
                // not apply. The full stored credential crosses (upstream
                // passes the `OAuthCredentials` object whole).
                #[cfg(feature = "ext-js")]
                if extension.is_some_and(|extension| extension.oauth.is_some()) {
                    if let Ok(credential_json) = serde_json::to_value(&credential) {
                        if let Some(key) =
                            crate::models_state::extension_oauth_api_key(provider, &credential_json)
                        {
                            return Some((WireCredential::ApiKey(key), "stored credential"));
                        }
                    }
                    // The handler is gone (engine reset) or failed: the
                    // provider reports unconfigured rather than falling
                    // back to the raw token.
                    return None;
                }
                // The built-in OAuth flows hand the access token to the
                // request through `toAuth`: most providers yield
                // `auth.apiKey` (the provider's standard key header —
                // `x-api-key` for anthropic,
                // `auth/oauth/anthropic.ts:304-306`); kimi-coding yields an
                // explicit Bearer header
                // (`auth/oauth/kimi-coding.ts:275-277`).
                let wire = if provider == "kimi-coding" {
                    WireCredential::AccessToken(access.clone())
                } else {
                    WireCredential::ApiKey(access.clone())
                };
                return Some((wire, "stored credential"));
            }
        }
    }
    // The configured models.json/extension key (`composeApiKeyAuth.resolve`,
    // `provider-composer.ts:470-475`): resolved through
    // `resolveConfigValueOrThrow`, wrapped into the inherited resolve when
    // the provider has one — no env-var fallback after this branch.
    let raw_key = extension
        .and_then(|extension| extension.api_key.as_deref())
        .or_else(|| config.and_then(|config| config.api_key.as_deref()));
    if let Some(raw_key) = raw_key {
        let resolved = auth::resolve_config_value(raw_key, None)?;
        let source = if crate::oauth::api_key_auth_name(provider).is_some() {
            // The inherited `envApiKeyAuth.resolve` sees the configured key
            // as a stored credential (`helpers.ts:21-23`).
            "stored credential"
        } else {
            "configured API key"
        };
        return Some((WireCredential::ApiKey(resolved), source));
    }
    if !has_api_key {
        return None;
    }
    for variable in env_key_candidates(provider) {
        if let Ok(value) = std::env::var(variable) {
            if value.is_empty() {
                continue;
            }
            // `ANTHROPIC_AUTH_TOKEN` rides as a custom Bearer header, ahead
            // of the key variables (`providers/anthropic.ts:36-40`); every
            // other env key is the provider's standard key auth.
            let wire = if provider == "anthropic" && *variable == "ANTHROPIC_AUTH_TOKEN" {
                WireCredential::AccessToken(value)
            } else {
                WireCredential::ApiKey(value)
            };
            return Some((wire, "environment"));
        }
    }
    None
}

/// API-key-only view of [`resolve_wire_credential`] (P4 callers and tests).
pub fn resolve_api_key(
    provider: &str,
    flag_key: Option<&str>,
    auth_path: &Path,
) -> Option<(String, &'static str)> {
    resolve_wire_credential(provider, flag_key, auth_path).and_then(|(credential, source)| {
        match credential {
            WireCredential::ApiKey(key) => Some((key, source)),
            WireCredential::AccessToken(_) => None,
        }
    })
}

/// `applyToolModifiers` (`core/settings-manager.ts:110-120`): apply
/// `+name`/`-name` entries to a base list in order.
pub fn apply_tool_modifiers(base: &[String], entries: &[String]) -> Vec<String> {
    let mut tools = base.to_vec();
    for entry in entries {
        let Some(name) = entry.strip_prefix('+').or_else(|| entry.strip_prefix('-')) else {
            continue;
        };
        let index = tools.iter().position(|tool| tool == name);
        if entry.starts_with('+') && index.is_none() && !name.is_empty() {
            tools.push(name.to_string());
        } else if entry.starts_with('-') && index.is_some() {
            tools.remove(index.expect("checked above"));
        }
    }
    tools
}

/// Select the tool set from the CLI flags (`main.ts:587-598` maps
/// `--no-tools`/`--no-builtin-tools` to `noTools: "all"|"builtin"`;
/// `sdk.ts:280-294` builds the allowlist): `--no-tools` empties everything;
/// `--no-builtin-tools` keeps extension tools; `--tools` is either an
/// all-names allowlist (builtins and extension tools) or `+name`/`-name`
/// modifiers over the builtin defaults — extension tools stay (modifiers
/// apply to the builtin base only), and `--exclude-tools` always removes.
/// Unknown builtin names error like the port's executor wiring; the
/// message lists every resolvable tool.
pub fn select_tools(
    args: &Args,
    builtin: &[ToolDefinition],
    extension: &[ToolDefinition],
) -> Result<Vec<ToolDefinition>, String> {
    if args.no_tools {
        return Ok(Vec::new());
    }
    let modifier_form = args
        .tools
        .as_ref()
        .is_some_and(|entries| entries.iter().any(|entry| is_tool_modifier_entry(entry)));
    let mut selected: Vec<ToolDefinition> = if args.no_builtin_tools {
        Vec::new()
    } else {
        let default_names: Vec<String> = ToolRegistry::coding()
            .definitions()
            .iter()
            .map(|definition| definition.name.to_string())
            .collect();
        let mut names: Vec<String> = match &args.tools {
            Some(entries) => {
                let all_names = entries.iter().all(|entry| !is_tool_modifier_entry(entry));
                if all_names {
                    entries.clone()
                } else {
                    apply_tool_modifiers(&default_names, entries)
                }
            }
            None => default_names,
        };
        if let Some(excluded) = &args.exclude_tools {
            names.retain(|tool| !excluded.contains(tool));
        }
        let universe: Vec<&str> = builtin
            .iter()
            .map(|definition| definition.name)
            .chain(extension.iter().map(|definition| definition.name))
            .collect();
        let extension_names: Vec<&str> =
            extension.iter().map(|definition| definition.name).collect();
        let mut definitions = Vec::new();
        for name in &names {
            // Extension names in the list resolve through the extension
            // filter below (the allowlist keeps them, `+name` activates
            // them); they are not unknown tools.
            if extension_names.contains(&name.as_str()) {
                continue;
            }
            let Some(definition) = builtin
                .iter()
                .find(|definition| definition.name == name.as_str())
            else {
                return Err(format!(
                    "Unknown tool: {name}. Built-in tools: {}",
                    universe.join(", ")
                ));
            };
            definitions.push(definition.clone());
        }
        definitions
    };
    // Extension tools: kept unless excluded, or dropped by an all-names
    // `--tools` allowlist (`sdk.ts:287-291` — the modifier form leaves
    // `allowedToolNames` undefined, so nothing filters them).
    for definition in extension {
        if let Some(excluded) = &args.exclude_tools {
            if excluded.contains(&definition.name.to_string()) {
                continue;
            }
        }
        if !modifier_form
            && let Some(allow) = &args.tools
            && !allow.contains(&definition.name.to_string())
        {
            continue;
        }
        selected.push(definition.clone());
    }
    Ok(selected)
}

fn is_tool_modifier_entry(entry: &str) -> bool {
    entry.starts_with('+') || entry.starts_with('-')
}

/// Build the runtime's system prompt with CLI overrides.
/// `--system-prompt` replaces the prefix; `--append-system-prompt` entries
/// append (joined with newlines, matching the CLI's startup join).
pub fn system_prompt_for(args: &Args, cwd: &str) -> Result<String, String> {
    let mut options = crate::prompt::BuildSystemPromptOptions::new(cwd);
    options.custom_prompt.clone_from(&args.system_prompt);
    if let Some(appends) = &args.append_system_prompt {
        options.append_system_prompt = Some(appends.join("\n"));
    }
    crate::prompt::build_system_prompt(&options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_patterns_split_provider_and_thinking() {
        let pattern = parse_model_pattern("openai/gpt-4o:high");
        assert_eq!(pattern.provider.as_deref(), Some("openai"));
        assert_eq!(pattern.id, "gpt-4o");
        assert_eq!(pattern.thinking, Some(ThinkingLevel::High));

        let pattern = parse_model_pattern("claude-sonnet-4");
        assert_eq!(pattern.provider, None);
        assert_eq!(pattern.id, "claude-sonnet-4");
        assert_eq!(pattern.thinking, None);

        // An invalid suffix is part of the id.
        let pattern = parse_model_pattern("gpt-4o:not-a-level");
        assert_eq!(pattern.id, "gpt-4o:not-a-level");
        assert_eq!(pattern.thinking, None);
    }

    #[test]
    fn resolve_model_finds_unique_and_ambiguous() {
        // An exact provider/id hit.
        let pattern = parse_model_pattern("anthropic/claude-sonnet-4-6");
        let resolved = resolve_model(Some(&pattern), None, None);
        assert!(
            resolved.is_ok(),
            "anthropic/claude-sonnet-4-6 should resolve: {resolved:?}"
        );

        // Ambiguous bare ids report the candidates. The id ships on several
        // providers, so a bare pattern cannot resolve uniquely.
        let pattern = parse_model_pattern("claude-sonnet-4-6");
        match resolve_model(Some(&pattern), None, None) {
            Ok(_) => {}
            Err(message) => assert!(
                message.contains("matches multiple models") || !message.contains("not found")
            ),
        }
        let pattern = parse_model_pattern("no-such-model-xyz");
        let error = resolve_model(Some(&pattern), None, None).unwrap_err();
        assert!(error.contains("not found"));

        // No pattern and no settings default asks for a model.
        assert!(resolve_model(None, None, None).is_err());
    }

    #[test]
    fn api_key_ladder_prefers_flag_then_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        // No flag, no stored credential, no env in the test process for this
        // provider id -> None (env vars like OPENAI_API_KEY may exist in the
        // ambient environment, so use an id with no env candidates).
        assert!(resolve_api_key("bogus-provider", None, &auth_path).is_none());
        // The flag wins unconditionally (the runtime credential;
        // `resolve_wire_credential`'s doc comment).
        assert_eq!(
            resolve_api_key("bogus-provider", Some("sk-test"), &auth_path)
                .map(|(key, source)| (key, source == "--api-key")),
            Some(("sk-test".to_string(), true))
        );
    }

    /// The auth-kind gate (`auth/resolve.ts:70-87`): a stored credential
    /// resolves only through a matching auth method, with no env fallback
    /// for a mismatched type.
    #[test]
    fn stored_credentials_gate_on_the_provider_auth_methods() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        std::fs::write(
            &auth_path,
            serde_json::to_string(&serde_json::json!({
                "anthropic": {"type": "oauth", "access": "tok", "refresh": "r", "expires": 9_000_000_000_000i64},
                "kimi-coding": {"type": "oauth", "access": "tok", "refresh": "r", "expires": 9_000_000_000_000i64},
                "groq": {"type": "oauth", "access": "tok", "refresh": "r", "expires": 9_000_000_000_000i64},
                "openai-codex": {"type": "api_key", "key": "sk-test"},
            }))
            .expect("auth.json"),
        )
        .expect("write");
        // OAuth for a provider with OAuth auth: the access token rides the
        // provider's standard key header (`toAuth`).
        assert_eq!(
            resolve_wire_credential("anthropic", None, &auth_path),
            Some((
                WireCredential::ApiKey("tok".to_string()),
                "stored credential"
            ))
        );
        // kimi-coding yields an explicit Bearer header.
        assert_eq!(
            resolve_wire_credential("kimi-coding", None, &auth_path),
            Some((
                WireCredential::AccessToken("tok".to_string()),
                "stored credential"
            ))
        );
        // A stored OAuth credential for a provider without OAuth auth
        // resolves to nothing — no env fallback (`resolve.ts:82-86`).
        assert_eq!(resolve_wire_credential("groq", None, &auth_path), None);
        // A stored API key for an OAuth-only provider resolves to nothing.
        assert_eq!(
            resolve_wire_credential("openai-codex", None, &auth_path),
            None
        );
    }

    /// The composed config layers (`composeApiKeyAuth.resolve`,
    /// `provider-composer.ts:470-475`): the models.json/extension key
    /// resolves through `resolveConfigValue`, without an env fallback, and
    /// the inherited `envApiKeyAuth` labels it a stored credential.
    #[test]
    fn configured_keys_resolve_through_the_composition() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let config = |value: serde_json::Value| {
            serde_json::from_value::<crate::models_config::ModelsJsonProvider>(serde_json::json!({
                "apiKey": value,
            }))
            .expect("models.json provider")
        };
        // A literal key on a models.json-only provider: "configured API key".
        let literal = config(serde_json::json!("acme-key"));
        assert_eq!(
            resolve_wire_credential_composed("acme", None, &auth_path, Some(&literal), None),
            Some((
                WireCredential::ApiKey("acme-key".to_string()),
                "configured API key"
            ))
        );
        // The configured key on a provider with an inherited auth resolves
        // as a stored credential (`helpers.ts:21-23`).
        let literal = config(serde_json::json!("openai-key"));
        assert_eq!(
            resolve_wire_credential_composed("openai", None, &auth_path, Some(&literal), None),
            Some((
                WireCredential::ApiKey("openai-key".to_string()),
                "stored credential"
            ))
        );
        // An unresolvable `$VAR` template resolves to nothing — no env
        // fallback after the configured-key branch.
        let template = config(serde_json::json!("$PI_RS_SLICE6C_TEST_UNSET_VAR"));
        assert_eq!(
            resolve_wire_credential_composed("openai", None, &auth_path, Some(&template), None),
            None
        );
    }

    #[test]
    fn tool_modifiers_apply_in_order() {
        let base = vec!["read".to_string(), "bash".to_string(), "edit".to_string()];
        let tools = apply_tool_modifiers(&base, &["+write".to_string(), "-bash".to_string()]);
        assert_eq!(tools, vec!["read", "edit", "write"]);
        // Removing an absent tool is a no-op.
        let tools = apply_tool_modifiers(&base, &["-grep".to_string()]);
        assert_eq!(tools, base);
    }

    #[test]
    fn tool_selection_honors_flags() {
        let available = ToolRegistry::all().definitions().to_vec();
        let args = Args {
            no_tools: true,
            ..Args::default()
        };
        assert!(select_tools(&args, &available, &[]).unwrap().is_empty());

        // Mixed names and modifiers are rejected at parse time upstream, so
        // the mixed case never reaches here; direct calls follow
        // `applyToolModifiers`, which ignores bare entries.
        let args = Args {
            tools: Some(vec!["read".to_string(), "+write".to_string()]),
            ..Args::default()
        };
        let selected = select_tools(&args, &available, &[]).unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|definition| definition.name)
                .collect::<Vec<&str>>(),
            vec!["read", "bash", "edit", "write"]
        );

        let args = Args {
            tools: Some(vec!["bogus".to_string()]),
            ..Args::default()
        };
        let error = match select_tools(&args, &available, &[]) {
            Err(error) => error,
            Ok(selected) => panic!(
                "expected unknown-tool error, got {} definitions",
                selected.len()
            ),
        };
        assert!(error.starts_with("Unknown tool: bogus"));

        let args = Args {
            exclude_tools: Some(vec!["edit".to_string(), "write".to_string()]),
            ..Args::default()
        };
        let selected = select_tools(&args, &available, &[]).unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|definition| definition.name)
                .collect::<Vec<&str>>(),
            vec!["read", "bash"]
        );
    }

    /// An extension tool definition with a stable static name.
    fn extension_definition(name: &'static str) -> ToolDefinition {
        ToolDefinition {
            name,
            label: name,
            description: format!("{name} from an extension"),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            output_schema: None,
            prompt_snippet: None,
            prompt_guidelines: Vec::new(),
            constrained_sampling: None,
            prepare_arguments: None,
            execution_mode: None,
            render_shell: None,
            renderers: crate::tools::renderers::ToolRenderers {
                render_call: |_args, _context| String::new(),
                render_result: |_args, _result, _context| String::new(),
            },
        }
    }

    #[test]
    fn extension_tools_follow_the_cli_filters() {
        let builtin = ToolRegistry::all().definitions().to_vec();
        let extension = vec![extension_definition("exta"), extension_definition("extb")];

        // Default: extension tools join the builtins.
        let args = Args::default();
        let selected = select_tools(&args, &builtin, &extension).unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|definition| definition.name)
                .collect::<Vec<&str>>(),
            vec!["read", "bash", "edit", "write", "exta", "extb"]
        );

        // `--no-builtin-tools` keeps extension tools (`noTools: "builtin"`).
        let args = Args {
            no_builtin_tools: true,
            ..Args::default()
        };
        let selected = select_tools(&args, &builtin, &extension).unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|definition| definition.name)
                .collect::<Vec<&str>>(),
            vec!["exta", "extb"]
        );

        // The all-names `--tools` allowlist filters extension tools too.
        let args = Args {
            tools: Some(vec!["read".to_string(), "extb".to_string()]),
            ..Args::default()
        };
        let selected = select_tools(&args, &builtin, &extension).unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|definition| definition.name)
                .collect::<Vec<&str>>(),
            vec!["read", "extb"]
        );

        // The modifier form keeps extension tools (`allowedToolNames`
        // undefined) and `--exclude-tools` still removes them.
        let args = Args {
            tools: Some(vec!["+write".to_string(), "-edit".to_string()]),
            exclude_tools: Some(vec!["exta".to_string()]),
            ..Args::default()
        };
        let selected = select_tools(&args, &builtin, &extension).unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|definition| definition.name)
                .collect::<Vec<&str>>(),
            vec!["read", "bash", "write", "extb"]
        );

        // `--no-tools` drops everything (`noTools: "all"`).
        let args = Args {
            no_tools: true,
            ..Args::default()
        };
        assert!(
            select_tools(&args, &builtin, &extension)
                .unwrap()
                .is_empty()
        );
    }

    /// The extension-oauth ladder arm (`adaptOAuth`'s `toAuth`,
    /// `provider-composer.ts:375`): a stored oauth credential for a
    /// provider with extension oauth metadata resolves through the
    /// stashed `getApiKey` — the handler owns the wire key, so the
    /// built-in arm (the raw access token) does not apply.
    #[cfg(feature = "ext-js")]
    #[test]
    fn extension_oauth_credentials_resolve_through_the_stashed_get_api_key() {
        let source = r#"
export default function (pi) {
  pi.registerProvider("acme", {
    api: "acme-api",
    oauth: {
      login: async () => ({}),
      refreshToken: async () => ({}),
      getApiKey: (credential) => `key-${credential.access}`,
    },
  });
}
"#;
        let mut runner = pi_ext_js::ExtensionRunner::new(pi_ext_js::HostCore {
            cwd: "/tmp".to_string(),
            mode: "print".to_string(),
            settings: serde_json::json!({}),
            ..Default::default()
        })
        .expect("runner");
        runner
            .load_extension("<test-0>", Some(source))
            .expect("load");
        runner.bind().expect("bind");
        crate::models_state::set_extension_runner(std::sync::Arc::new(std::sync::Mutex::new(
            runner,
        )));

        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        std::fs::write(
            &auth_path,
            serde_json::to_string(&serde_json::json!({
                "acme": {"type": "oauth", "access": "tok", "refresh": "r", "expires": 9_000_000_000_000i64},
            }))
            .expect("auth.json"),
        )
        .expect("write");
        let extension = crate::models_config::ExtensionProviderConfig {
            oauth: Some(crate::models_config::ExtensionOAuthMetadata {
                name: "Acme".to_string(),
                is_subscription: false,
                uses_callback_server: None,
            }),
            ..Default::default()
        };
        assert_eq!(
            resolve_wire_credential_composed("acme", None, &auth_path, None, Some(&extension)),
            Some((
                WireCredential::ApiKey("key-tok".to_string()),
                "stored credential"
            ))
        );
        // A provider id no engine registration covers answers nothing.
        assert_eq!(
            resolve_wire_credential_composed("nosuch", None, &auth_path, None, Some(&extension)),
            None
        );
    }
}
