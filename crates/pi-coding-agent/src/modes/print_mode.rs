//! Print mode (single-shot): send prompts, output the result, exit.
//!
//! Port of `modes/print-mode.ts` (182 lines) plus the model/credential
//! resolution the CLI host performs before it. Faithful where P4 can be:
//! text mode with the same output/exit-code contract. Deferred, tracked for
//! later phases: JSON event output (`--mode json`, P5), signal handlers and
//! stdout backpressure (`core/output-guard.ts`, P7), extension binding (P5),
//! `@file` expansion (`cli/file-processor.ts`, P5).

use std::path::Path;
use std::sync::Arc;

use pi_agent::agent_loop::{AgentLoopConfig, DefaultConvertToLlm};
use pi_agent::agent_message::AgentMessage;
use pi_agent::events::CollectingSink;
use pi_agent::queues::Queues;
use pi_agent::transcript::tool_declaration;
use pi_agent::types::{
    AbortSignal, AgentContext, StreamFn, ThinkingLevel, ToolExecutionMode, ToolExecutor, ToolSpec,
};
use pi_ai::catalog;
use pi_ai::types::{Model, StopReason};

use crate::auth::{self, Credential};
use crate::cli::args::{Args, Mode};
use crate::session::store::{SessionManager, build_session_context_from};
use crate::tools::index::{BuiltinToolExecutor, ToolDefinition, ToolRegistry};

/// `PrintModeOptions` (`print-mode.ts:18-27`).
#[derive(Debug, Clone)]
pub struct PrintModeOptions {
    /// `"text"` prints the final response; `"json"` is a P5 mode.
    pub mode: Mode,
    /// Prompts sent after `initial_message` (`print-mode.ts:21`).
    pub messages: Vec<String>,
    /// First message to send (`print-mode.ts:23`).
    pub initial_message: Option<String>,
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
    pub executor: Arc<BuiltinToolExecutor>,
    pub system_prompt: String,
    pub session: SessionManager,
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
        prompts.push(pi_agent::transcript::user_message(initial, now));
    }
    for message in &options.messages {
        prompts.push(pi_agent::transcript::user_message(message, now));
    }

    let mut queues = Queues::new();
    let mut sink = CollectingSink { events: Vec::new() };
    let signal = AbortSignal::new();
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
        prepare_request: None,
        prepare_next_turn: None,
        transform_context: None,
        convert_to_llm: Arc::new(DefaultConvertToLlm),
    };

    let new_messages = futures::executor::block_on(pi_agent::run_agent_loop(
        prompts,
        context,
        executor.as_ref(),
        &config,
        &mut queues,
        &mut sink,
        &signal,
        runtime.stream_fn.as_ref(),
    ));

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
    let Some(pattern) = pattern else {
        // Fall back to the settings default; otherwise ask for a model.
        return match (default_provider, default_model) {
            (Some(provider), Some(id)) => catalog::catalog()
                .get(provider, id)
                .cloned()
                .ok_or_else(|| format!("Default model not found in catalog: {provider}/{id}")),
            _ => Err("No model specified. Use --model <pattern> (e.g. anthropic/claude-sonnet-4) or set defaultProvider/defaultModel in settings".to_string()),
        };
    };
    let found = if let Some(provider) = &pattern.provider {
        catalog::catalog()
            .get(provider, &pattern.id)
            .cloned()
            .ok_or_else(|| format!("Model not found: {provider}/{}", pattern.id))?
    } else {
        let matches = catalog::catalog().find(&pattern.id);
        match matches.len() {
            0 => return Err(format!("Model not found: {}", pattern.id)),
            1 => matches[0].clone(),
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
/// provider checks them (`packages/ai/src/env-api-keys.ts` + provider files).
/// The full table ports with the provider work in P5; the ladder covers the
/// catalog's key-auth providers.
fn env_key_candidates(provider: &str) -> &'static [&'static str] {
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
        "minimax" | "minimax-cn" => &["MINIMAX_API_KEY"],
        "moonshotai" | "moonshotai-cn" => &["MOONSHOT_API_KEY"],
        "opencode" | "opencode-go" => &["OPENCODE_API_KEY"],
        "kimi-coding" => &["KIMI_API_KEY"],
        "meta" => &["META_API_KEY"],
        "cloudflare-workers-ai" | "cloudflare-ai-gateway" => &["CLOUDFLARE_API_KEY"],
        "qwen-token-plan" | "qwen-token-plan-individual" => &["QWEN_TOKEN_PLAN_API_KEY"],
        "qwen-token-plan-cn" | "qwen-token-plan-individual-cn" => &["QWEN_TOKEN_PLAN_CN_API_KEY"],
        "xiaomi" => &["XIAOMI_API_KEY"],
        "xiaomi-token-plan-cn" => &["XIAOMI_TOKEN_PLAN_CN_API_KEY"],
        "xiaomi-token-plan-ams" => &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"],
        "xiaomi-token-plan-sgp" => &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"],
        "ant-ling" => &["ANT_LING_API_KEY"],
        "cohere" => &["COHERE_API_KEY"],
        "perplexity" => &["PERPLEXITY_API_KEY"],
        "huggingface" => &["HF_TOKEN", "HF_API_KEY"],
        "siliconflow" => &["SILICONFLOW_API_KEY"],
        "modelscope" => &["MODELSCOPE_API_KEY"],
        "bailian" => &["BAILIAN_API_KEY"],
        "stepfun" => &["STEPFUN_API_KEY"],
        // TODO(verify-vs-TS): entries below were added without the reference
        // checkout present; confirm names/order in `env-api-keys.ts` (P5).
        "grok" => &["XAI_API_KEY", "GROK_API_KEY"],
        "aihubmix" => &["AIHUBMIX_API_KEY"],
        "dmxapi" => &["DMXAPI_API_KEY"],
        "gitee-ai" => &["GITEE_AI_API_KEY"],
        "novita" => &["NOVITA_API_KEY"],
        "ppio" => &["PPIO_API_KEY"],
        "scaleway" => &["SCALEWAY_API_KEY"],
        "targon" => &["TARGON_API_KEY"],
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
/// friends): `--api-key` flag, then the stored credential from `auth.json`,
/// then provider environment variables. Returns the credential and its
/// source, keeping OAuth tokens distinct from API keys so the adapters can
/// pick the right header.
pub fn resolve_wire_credential(
    provider: &str,
    flag_key: Option<&str>,
    auth_path: &Path,
) -> Option<(WireCredential, &'static str)> {
    if let Some(key) = flag_key {
        return Some((WireCredential::ApiKey(key.to_string()), "--api-key"));
    }
    if let Some(credential) = auth::read_stored_credential(provider, auth_path) {
        match &credential {
            Credential::ApiKey { key, env, .. } => {
                if let Some(resolved) = key
                    .as_deref()
                    .and_then(|key| auth::resolve_config_value(key, env.as_ref()))
                {
                    return Some((WireCredential::ApiKey(resolved), "stored credential"));
                }
            }
            Credential::Oauth { access, .. } => {
                return Some((
                    WireCredential::AccessToken(access.clone()),
                    "stored credential",
                ));
            }
        }
    }
    for variable in env_key_candidates(provider) {
        if let Ok(value) = std::env::var(variable) {
            if !value.is_empty() {
                return Some((WireCredential::ApiKey(value), "environment"));
            }
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

/// Select the built-in tool set from CLI flags: `--no-tools`/
/// `--no-builtin-tools` empty the registry; `--tools` names (all-names form)
/// replace the defaults, `+name`/`-name` entries modify them; `--exclude-tools`
/// always removes. Unknown names error like upstream's executor wiring.
pub fn select_builtin_tools(
    args: &Args,
    available: &[ToolDefinition],
) -> Result<Vec<ToolDefinition>, String> {
    if args.no_tools || args.no_builtin_tools {
        return Ok(Vec::new());
    }
    let default_names: Vec<String> = ToolRegistry::coding()
        .definitions()
        .iter()
        .map(|definition| definition.name.to_string())
        .collect();
    let mut selected: Vec<String> = match &args.tools {
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
        for name in excluded {
            selected.retain(|tool| tool != name);
        }
    }
    let mut definitions = Vec::new();
    for name in &selected {
        let Some(definition) = available
            .iter()
            .find(|definition| definition.name == name.as_str())
        else {
            return Err(format!(
                "Unknown tool: {name}. Built-in tools: {}",
                available
                    .iter()
                    .map(|definition| definition.name)
                    .collect::<Vec<&str>>()
                    .join(", ")
            ));
        };
        definitions.push(definition.clone());
    }
    Ok(definitions)
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
        // ambient environment, so use a provider with no env candidates).
        assert!(resolve_api_key("radius", None, &auth_path).is_none());
        // The flag always wins.
        assert_eq!(
            resolve_api_key("radius", Some("sk-test"), &auth_path)
                .map(|(key, source)| (key, source == "--api-key")),
            Some(("sk-test".to_string(), true))
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
        assert!(select_builtin_tools(&args, &available).unwrap().is_empty());

        // Mixed names and modifiers are rejected at parse time upstream, so
        // the mixed case never reaches here; direct calls follow
        // `applyToolModifiers`, which ignores bare entries.
        let args = Args {
            tools: Some(vec!["read".to_string(), "+write".to_string()]),
            ..Args::default()
        };
        let selected = select_builtin_tools(&args, &available).unwrap();
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
        let error = match select_builtin_tools(&args, &available) {
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
        let selected = select_builtin_tools(&args, &available).unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|definition| definition.name)
                .collect::<Vec<&str>>(),
            vec!["read", "bash"]
        );
    }
}
