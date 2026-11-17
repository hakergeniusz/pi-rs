//! Pi coding agent CLI (Rust port of `@earendil-works/pi-coding-agent`).
//!
//! Binary `pi`. Phase P4 wires print mode (`pi -p`); P5 adds JSON and RPC
//! modes; interactive mode (P6) still exits with a phase note.

use std::io::BufReader;
use std::path::PathBuf;
use std::sync::Arc;

use pi_ai::providers;

use pi_ai::types::ImageContent;
use pi_coding_agent::cli::args::{Args, DiagnosticKind, Mode, parse_args, print_help};
use pi_coding_agent::cli::file_processor::process_file_arguments;
use pi_coding_agent::cli::project_trust::{DefaultProjectTrust, ProjectTrustStore};
use pi_coding_agent::config;
use pi_coding_agent::config::settings::{SettingsManager, SettingsManagerCreateOptions};
use pi_coding_agent::extensions;
use pi_coding_agent::modes::json_event::run_json_mode;
use pi_coding_agent::modes::print_mode::{
    PrintModeOptions, PrintRuntime, ProviderStreamFn, WireCredential, parse_model_pattern,
    resolve_model_with, resolve_wire_credential, run_print_mode, select_tools, system_prompt_for,
    thinking_level_from_str,
};
use pi_coding_agent::modes::rpc::run_rpc_mode;
use pi_coding_agent::session::store::{NewSessionOptions, SessionManager};
use pi_coding_agent::tools::index::ToolRegistry;

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();

    // Package subcommands route before mode startup (`main.ts:648` —
    // `pi install|remove|uninstall|list|update`), like `runAuthCommand`.
    if let Some(code) = pi_coding_agent::cli::package_command::handle_package_command(&raw) {
        std::process::exit(code);
    }

    let args = parse_args(&raw);

    let errors: Vec<&_> = args
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.kind == DiagnosticKind::Error)
        .collect();
    if !errors.is_empty() {
        for error in errors {
            eprintln!("{}", error.message);
        }
        std::process::exit(2);
    }

    if args.version {
        println!("{} {}", config::APP_NAME, env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.help {
        print_help();
        return;
    }
    // `--export <file> [output]` (`main.ts:688-703`): export a session file
    // to HTML and exit; the first positional argument is the output path.
    if let Some(session_file) = &args.export {
        let output_path = args.messages.first().map(String::as_str);
        match pi_coding_agent::session::export_html::export_from_file(
            session_file,
            &pi_coding_agent::session::export_html::ExportOptions {
                output_path,
                ..Default::default()
            },
        ) {
            Ok(result) => {
                println!("Exported to: {}", result.display());
            }
            Err(message) => {
                eprintln!("Error: {message}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.resume || args.fork.is_some() {
        eprintln!("pi-rs: --resume/--fork are not implemented yet (phase P5/P6)");
        std::process::exit(2);
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let cwd_string = cwd.to_string_lossy().to_string();

    if let Some(search) = &args.list_models {
        list_models(search.as_deref());
        return;
    }

    if args.mode.is_none() && !args.print {
        // Interactive mode requires a terminal; piped stdin stays a P7 item.
        if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            eprintln!("pi-rs: no prompt given and stdout is not a terminal; use -p for print mode");
            std::process::exit(2);
        }
        let outcome = run_interactive(&args, &cwd, &cwd_string);
        match outcome {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }

    let outcome = match args.mode {
        Some(Mode::Rpc) => run_rpc(&args, &cwd, &cwd_string),
        Some(Mode::Json) => run_json(&args, &cwd, &cwd_string),
        _ => run_print(&args, &cwd, &cwd_string),
    };
    match outcome {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

/// `--list-models [search]`: catalog listing with substring filter. The fuzzy
/// search of upstream `cli/list-models.ts` lands in P5.
fn list_models(search: Option<&str>) {
    let search = search.unwrap_or("");
    for provider in pi_coding_agent::models_state::providers() {
        for model in pi_coding_agent::models_state::models_for(&provider) {
            let full = format!("{provider}/{}", model.id);
            if search.is_empty() || full.contains(search) || model.id.contains(search) {
                println!("{full}\t{}", model.name);
            }
        }
    }
}

/// Upstream creates a settings manager before trust is resolved
/// (`main.js:583`) so the global `defaultProjectTrust` can inform
/// `resolveProjectTrusted` (`main.js:669`). `getDefaultProjectTrust`
/// (`settings-manager.js:789-792`) accepts only "always"/"never"; anything
/// else falls back to the "ask" default (`None` here).
fn startup_default_project_trust(settings: &SettingsManager) -> Option<DefaultProjectTrust> {
    match settings
        .get("defaultProjectTrust")
        .and_then(|value| value.as_str())
    {
        Some("always") => Some(DefaultProjectTrust::Always),
        Some("never") => Some(DefaultProjectTrust::Never),
        _ => None,
    }
}

/// Assemble the runtime shared by print/JSON/RPC modes. Mirrors the startup
/// path of upstream `main.ts` for non-interactive runs: settings, the
/// extensions host (discovery, pre-trust pass, trust decision, final load,
/// bind, flag claiming), model, credentials, session, tools, system prompt.
#[allow(clippy::too_many_lines)] // mirrors the main.ts createRuntime assembly; split when extending
/// The mode's UI surface for the extension host (`bindExtensions`'s
/// `uiContext`). The RPC host passes its dialog bridge; without the
/// engine nothing binds and the unit default carries no surface.
#[cfg(feature = "ext-js")]
type ModeUi = Option<Arc<dyn pi_ext_js::UiSink>>;
#[cfg(not(feature = "ext-js"))]
type ModeUi = ();

/// `DEFAULT_THINKING_LEVEL` (`core/defaults.ts:3`): the session's starting
/// level when neither the CLI, a resumed session, the per-model override,
/// nor the `defaultThinkingLevel` setting provides one.
const DEFAULT_THINKING_LEVEL: pi_agent::types::ThinkingLevel =
    pi_agent::types::ThinkingLevel::Medium;

/// The session's thinking level at startup (`createAgentSession`,
/// `core/sdk.ts:253-278`): `cli_level` wins; else a resumed session with
/// messages restores `recorded`, or the settings default when it recorded
/// none; else the per-model override; else the settings default; else
/// [`DEFAULT_THINKING_LEVEL`]. The result clamps to the model's supported
/// levels (`clampThinkingLevel`, `packages/ai/src/models.ts:1315-1334`) —
/// upstream's no-model arm (`sdk.ts:274`) cannot fire here, because
/// `resolve_model_with` errors before a model-less runtime reaches this.
fn initial_thinking_level(
    cli_level: Option<pi_agent::types::ThinkingLevel>,
    has_existing_session: bool,
    recorded: Option<pi_agent::types::ThinkingLevel>,
    per_model: Option<pi_agent::types::ThinkingLevel>,
    default_setting: Option<pi_agent::types::ThinkingLevel>,
    model: &pi_ai::types::Model,
) -> pi_agent::types::ThinkingLevel {
    let default_level = default_setting.unwrap_or(DEFAULT_THINKING_LEVEL);
    let level = if let Some(level) = cli_level {
        level
    } else if has_existing_session {
        // sdk.ts:255-260: a session with data restores its recorded level;
        // with no recorded entry, the settings default (the per-model
        // override is not consulted).
        recorded.unwrap_or(default_level)
    } else {
        // sdk.ts:262-271: the per-model override, then the global default.
        per_model.unwrap_or(default_level)
    };
    pi_agent::types::clamp_thinking_level(model, level)
}

/// Every non-RPC mode: headless extension host, like upstream's
/// non-interactive paths that pass no `uiContext`.
fn build_runtime(
    args: &Args,
    cwd: &std::path::Path,
    cwd_string: &str,
    mode: &str,
) -> Result<PrintRuntime, String> {
    build_runtime_with_ui(args, cwd, cwd_string, mode, ModeUi::default())
}

/// The runtime assembly with an optional UI surface. `ui` binds before
/// load's bind step so `session_start` handlers can already prompt
/// (`bindExtensions` applies the bindings, then emits,
/// `agent-session.ts:3429-3449`).
#[allow(clippy::too_many_lines)] // mirrors the TS service assembly (main.ts); split when extending
fn build_runtime_with_ui(
    args: &Args,
    cwd: &std::path::Path,
    cwd_string: &str,
    mode: &str,
    ui: ModeUi,
) -> Result<PrintRuntime, String> {
    let agent_dir = config::get_agent_dir();
    let trust_store = ProjectTrustStore::new(&agent_dir);
    let startup_settings = SettingsManager::create(
        cwd,
        &agent_dir,
        SettingsManagerCreateOptions {
            project_trusted: None,
        },
    );
    let default_project_trust = startup_default_project_trust(&startup_settings);

    #[cfg(not(feature = "ext-js"))]
    let _ = ui;
    // Extensions (E2 slice 3). Diagnostics report in order: warnings to
    // stderr, error-level entries abort with the load-failure hint
    // (`main.ts:982-990`).
    #[cfg(feature = "ext-js")]
    let (extensions, diagnostics) = extensions::load(
        args,
        cwd,
        &agent_dir,
        &trust_store,
        default_project_trust,
        &startup_settings,
        mode,
        ui.map(|sink| (sink, mode.to_string())),
    )?;
    #[cfg(not(feature = "ext-js"))]
    let (extensions, diagnostics) = extensions::load(
        args,
        cwd,
        &agent_dir,
        &trust_store,
        default_project_trust,
        &startup_settings,
        mode,
    )?;
    for diagnostic in &diagnostics {
        if diagnostic.kind == DiagnosticKind::Warning {
            eprintln!("Warning: {}", diagnostic.message);
        }
    }
    let errors: Vec<&str> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.kind == DiagnosticKind::Error)
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();
    if !errors.is_empty() {
        let mut message = errors
            .iter()
            .map(|error| format!("Error: {error}"))
            .collect::<Vec<_>>()
            .join("\n");
        if errors
            .iter()
            .any(|error| error.contains("Failed to load extension"))
        {
            message.push('\n');
            message.push_str(extensions::EXTENSION_LOAD_FAILURE_HINT);
        }
        return Err(message);
    }
    // Composed model catalog: models.json ⊕ extension providers ⊕ virtual
    // models (`model-runtime.ts`; consumers read `models_state`).
    #[cfg(feature = "ext-js")]
    if let Some(runner) = extensions.host.runner() {
        pi_coding_agent::models_state::set_extension_runner(Arc::clone(&runner));
        pi_coding_agent::models_state::refresh_with_runner(
            &runner.lock().expect("extension runner mutex poisoned"),
        );
    }
    #[cfg(not(feature = "ext-js"))]
    pi_coding_agent::models_state::refresh();
    let project_trusted = extensions.project_trusted;

    // `resolveProjectTrusted` (`project-trust.ts:47-97`): the `--approve`
    // override, the trust store, then the `defaultProjectTrust` setting
    // ("ask" without a UI resolves to not-trusted, like upstream's
    // non-interactive paths) — with the extension hook consulted when the
    // bootstrap pass ran.
    let settings = SettingsManager::create(
        cwd,
        &agent_dir,
        SettingsManagerCreateOptions {
            project_trusted: Some(project_trusted),
        },
    );

    // Model: --model pattern, else the settings default. The surface also
    // carries extension-registered virtual models (`withVirtualModels`).
    #[cfg(feature = "ext-js")]
    let virtual_models = pi_coding_agent::virtual_models::virtual_models_from_defs(
        &extensions
            .host
            .runner()
            .map(|runner| {
                runner
                    .lock()
                    .expect("extension runner mutex poisoned")
                    .virtual_models()
            })
            .unwrap_or_default(),
    );
    #[cfg(not(feature = "ext-js"))]
    let virtual_models: Vec<pi_ai::types::Model> = Vec::new();
    let pattern = args.model.as_deref().map(parse_model_pattern);
    let model = resolve_model_with(
        pattern.as_ref(),
        settings.get_default_provider().as_deref(),
        settings.get_default_model().as_deref(),
        &virtual_models,
    )?;

    // `--api-key` registers the session provider's non-persistent runtime key
    // (`main.ts:898-906`): the login/logout selectors report it
    // (`getProviderAuthStatus`, `model-runtime.ts:656`) and /logout merges it
    // into the credential list (`runtime-credentials.ts:36-41`).
    if let Some(api_key) = args.api_key.as_deref() {
        pi_coding_agent::oauth::set_runtime_api_key(&model.provider, api_key);
    }

    // Credential ladder: flag > auth.json > models.json/extension `apiKey` >
    // provider env vars, gated on the provider's composed auth methods
    // (slice 6c). OAuth tokens resolve as bearer credentials, distinct from
    // API keys. A virtual selection defers the check to routing (the routed
    // provider's credentials decide).
    let wire: (Option<String>, Option<String>) =
        if pi_coding_agent::virtual_models::is_virtual_model(&model) {
            (None, None)
        } else {
            resolve_wire_credential(
                &model.provider,
                args.api_key.as_deref(),
                &config::get_auth_path(),
            )
            .map_or((None, None), |(credential, source)| {
                if source == "environment" {
                    eprintln!("pi-rs: using {} key from environment", model.provider);
                }
                match credential {
                    WireCredential::ApiKey(key) => (Some(key), None),
                    WireCredential::AccessToken(token) => (None, Some(token)),
                }
            })
        };
    let (api_key, access_token) = wire;
    if api_key.is_none()
        && access_token.is_none()
        && !pi_coding_agent::virtual_models::is_virtual_model(&model)
    {
        return Err(format!(
            "No API key for provider {}. Run `pi auth login --provider {}`, set the provider's environment variable, or pass --api-key.",
            model.provider, model.provider
        ));
    }

    // Under a virtual selection the stream dispatches per routed model
    // (`withVirtualModels`' `unroutedStream` fails unrouted requests with the
    // TS message); physical selections bind one adapter — wrapped in the
    // composed dispatch when the engine exists, so a provider's registered
    // `streamSimple` handler wins per request (`streamWith`,
    // provider-composer.ts:588-613).
    let stream_fn: Arc<dyn pi_agent::types::StreamFn> =
        if pi_coding_agent::virtual_models::is_virtual_model(&model) {
            Arc::new(pi_coding_agent::virtual_models::RoutedStreamFn {
                selected_provider: model.provider.clone(),
                flag_api_key: args.api_key.clone(),
                auth_path: config::get_auth_path(),
                route_error: Arc::new(pi_coding_agent::virtual_models::RouteErrorSlot::default()),
                #[cfg(feature = "ext-js")]
                runner: extensions.host.runner(),
            })
        } else {
            let Some(stream_fn) = providers::stream_fn_for(&model.api) else {
                return Err(format!(
                    "No provider adapter for api \"{}\" (model {}/{}).",
                    model.api, model.provider, model.id
                ));
            };
            let base: Arc<dyn pi_agent::types::StreamFn> = Arc::new(ProviderStreamFn(stream_fn));
            #[cfg(feature = "ext-js")]
            let base: Arc<dyn pi_agent::types::StreamFn> = match extensions.host.runner() {
                Some(runner) => Arc::new(pi_coding_agent::models_state::ComposedStreamFn {
                    runner,
                    fallback: base,
                }),
                None => base,
            };
            base
        };

    // Session: --no-session is ephemeral, --continue resumes the most recent
    // session for this cwd, --session opens by path, --session-id reuses or
    // creates, otherwise create.
    let session_dir = args.session_dir.as_deref().map(PathBuf::from);
    let session = if args.no_session {
        SessionManager::in_memory(cwd, &NewSessionOptions::default(), Vec::new())
    } else if args.r#continue {
        SessionManager::continue_recent(cwd, session_dir.as_deref())
    } else if let Some(path) = &args.session {
        SessionManager::open(&PathBuf::from(path), session_dir.as_deref(), None)
    } else if let Some(id) = &args.session_id {
        match SessionManager::find_by_id(cwd, id, session_dir.as_deref()) {
            Some(path) => SessionManager::open(&path, session_dir.as_deref(), None),
            None => {
                SessionManager::create(cwd, session_dir.as_deref(), &NewSessionOptions::default())
            }
        }
    } else {
        SessionManager::create(cwd, session_dir.as_deref(), &NewSessionOptions::default())
    }
    .map_err(|error| error.to_string())?;

    // Restore facts (`createAgentSession`, `core/sdk.ts:195-198`): whether the
    // resumed session carries messages, and the level its branch recorded
    // (`getSessionContextSettings`, `session-manager.ts:418-432` — the last
    // `thinking_level_change` on the leaf path).
    let projection = session.build_session_projection().ok();
    let has_existing_session = projection
        .as_ref()
        .is_some_and(|projection| !projection.messages.is_empty());
    let branch_has_thinking_entry =
        session
            .get_branch(None)
            .unwrap_or_default()
            .iter()
            .any(|entry| {
                matches!(
                    entry,
                    pi_coding_agent::session::records::SessionEntry::ThinkingLevelChange(_)
                )
            });
    let recorded_thinking_level = if branch_has_thinking_entry {
        projection
            .as_ref()
            .and_then(|projection| thinking_level_from_str(&projection.thinking_level).ok())
    } else {
        None
    };

    // Tools: builtins plus extension tools (the CLI filters apply to both;
    // extension tools append after the builtins, like the upstream
    // registry order).
    let builtin_available = ToolRegistry::all().definitions().to_vec();
    let selected = select_tools(args, &builtin_available, extensions.host.tool_definitions())?;
    let executor = extensions.host.executor(&selected, cwd_string);

    // System prompt.
    let system_prompt = system_prompt_for(args, cwd_string)?;

    // Thinking level (`createAgentSession`, `core/sdk.ts:253-278`): the
    // `--thinking` flag wins; else a resumed session restores its recorded
    // level (or the default when the session recorded none — the per-model
    // override is not consulted); else the per-model override
    // (`modelThinkingLevels[provider/id]`); else the `defaultThinkingLevel`
    // setting; else `DEFAULT_THINKING_LEVEL` = "medium"
    // (`core/defaults.ts:3`). The resolved level clamps to the model's
    // supported levels (`clampThinkingLevel`,
    // `packages/ai/src/models.ts:1315-1334`).
    let thinking_level = initial_thinking_level(
        args.thinking
            .as_deref()
            .map(thinking_level_from_str)
            .transpose()?,
        has_existing_session,
        recorded_thinking_level,
        settings
            .get("modelThinkingLevels")
            .and_then(|value| value.get(format!("{}/{}", model.provider, model.id).as_str()))
            .and_then(|value| value.as_str())
            .and_then(|level| thinking_level_from_str(level).ok()),
        settings
            .get_default_thinking_level()
            .and_then(|level| thinking_level_from_str(&level).ok()),
        &model,
    );

    let compaction_settings = settings
        .get_compaction_settings(Some((model.provider.as_str(), model.id.as_str())))
        .unwrap_or(pi_coding_agent::session::compaction::DEFAULT_COMPACTION_SETTINGS);
    Ok(PrintRuntime {
        model,
        thinking_level,
        api_key,
        access_token,
        stream_fn,
        executor,
        system_prompt,
        session,
        project_trusted,
        retry_settings: settings.get_retry_settings(),
        compaction_settings,
        #[cfg(feature = "ext-js")]
        extension_runner: extensions.host.runner(),
    })
}

/// Combines `@file` text, image attachments, and the first CLI message into
/// the initial prompt (`cli/initial-message.ts` `buildInitialMessage`): the
/// parts join with no separator, remaining messages become follow-ups.
/// Upstream also prepends piped stdin content here; stdin handling lands
/// with P7's output-guard work.
type PromptParts = (Option<String>, Vec<ImageContent>, Vec<String>);

fn resolve_prompt_messages(args: &Args, cwd: &std::path::Path) -> Result<PromptParts, String> {
    let mut messages = args.messages.clone();
    let mut images = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    if !args.file_args.is_empty() {
        let files = process_file_arguments(&args.file_args, cwd)?;
        images = files.images;
        if !files.text.is_empty() {
            parts.push(files.text);
        }
    }
    if !messages.is_empty() {
        parts.push(messages.remove(0));
    }
    let initial = if parts.is_empty() {
        None
    } else {
        Some(parts.concat())
    };
    Ok((initial, images, messages))
}

fn run_print(args: &Args, cwd: &std::path::Path, cwd_string: &str) -> Result<i32, String> {
    let mut runtime = build_runtime(args, cwd, cwd_string, "print")?;
    let (initial, images, follow_ups) = resolve_prompt_messages(args, cwd)?;
    let options = PrintModeOptions {
        mode: Mode::Text,
        messages: follow_ups,
        initial_message: initial,
        initial_images: images,
    };
    run_print_mode(&mut runtime, &options)
}

/// Interactive mode: the chat TUI. Initial `@file`/message arguments ride
/// along like upstream's `initialMessage` (P6 part 2).
fn run_interactive(args: &Args, cwd: &std::path::Path, cwd_string: &str) -> Result<i32, String> {
    let mut runtime = build_runtime(args, cwd, cwd_string, "interactive")?;
    // The trust ladder resolved once in build_runtime — extensions may have
    // decided through the `project_trust` hook.
    let project_trusted = runtime.project_trusted;
    let agent_dir = config::get_agent_dir();
    // The theme setting seeds the controller: `--use-theme` wins, else
    // settings.json's `theme` (upstream passes the settings manager into the
    // mode; the port funnels the resolved setting through options).
    let theme_name = args.use_theme.clone().or_else(|| {
        let settings = SettingsManager::create(
            cwd,
            &agent_dir,
            SettingsManagerCreateOptions {
                project_trusted: Some(project_trusted),
            },
        );
        settings
            .get("theme")
            .and_then(|value| value.as_str())
            .map(str::to_string)
    });
    let options = pi_coding_agent::modes::interactive::interactive_mode::InteractiveOptions {
        version: env!("CARGO_PKG_VERSION").to_string(),
        theme_name,
        tui_mode: args.tui_mode,
        auth_path: None,
        agent_dir: None,
        project_trusted,
    };
    pi_coding_agent::modes::interactive::interactive_mode::run_interactive_mode(
        &mut runtime,
        &options,
    )
}

fn run_json(args: &Args, cwd: &std::path::Path, cwd_string: &str) -> Result<i32, String> {
    let mut runtime = build_runtime(args, cwd, cwd_string, "json")?;
    let (initial, images, follow_ups) = resolve_prompt_messages(args, cwd)?;
    let options = PrintModeOptions {
        mode: Mode::Json,
        messages: follow_ups,
        initial_message: initial,
        initial_images: images,
    };
    run_json_mode(&mut runtime, &options)
}

fn run_rpc(args: &Args, cwd: &std::path::Path, cwd_string: &str) -> Result<i32, String> {
    if !args.file_args.is_empty() {
        // `main.ts:708-711`.
        return Err("Error: @file arguments are not supported in RPC mode".to_string());
    }
    // The bridge owns stdin/stdout for the run: the reader thread routes
    // `extension_ui_response` lines to pending dialogs while the loop is
    // inside a command (upstream routes both through the same stdin
    // handler, `rpc-mode.ts:759-824`).
    let mut bridge = pi_coding_agent::modes::rpc::create_bridge(
        Box::new(BufReader::new(std::io::stdin())),
        Box::new(pi_coding_agent::modes::rpc::RpcStdout::new()),
    );
    #[cfg(feature = "ext-js")]
    let mut runtime =
        build_runtime_with_ui(args, cwd, cwd_string, "rpc", Some(Arc::clone(&bridge.sink)))?;
    #[cfg(not(feature = "ext-js"))]
    let mut runtime = build_runtime(args, cwd, cwd_string, "rpc")?;
    run_rpc_mode(&mut runtime, &mut bridge)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(reasoning: bool) -> pi_ai::types::Model {
        pi_ai::types::Model {
            id: "test-model".to_string(),
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            reasoning,
            ..pi_ai::types::Model::default()
        }
    }

    /// Nothing configured: upstream starts a reasoning model at
    /// `DEFAULT_THINKING_LEVEL` = "medium" (`core/defaults.ts:3`,
    /// `core/sdk.ts:270`).
    #[test]
    fn a_reasoning_model_defaults_to_medium() {
        assert_eq!(
            initial_thinking_level(None, false, None, None, None, &model(true)),
            pi_agent::types::ThinkingLevel::Medium
        );
    }

    /// A resumed session restores its recorded level; an explicit "off" wins
    /// over the default, and a session without a recorded entry takes the
    /// settings default with the per-model override ignored (`sdk.ts:255-260`).
    #[test]
    fn a_resumed_session_restores_its_recorded_level() {
        let reasoning = model(true);
        assert_eq!(
            initial_thinking_level(
                None,
                true,
                Some(pi_agent::types::ThinkingLevel::High),
                Some(pi_agent::types::ThinkingLevel::Low),
                None,
                &reasoning
            ),
            pi_agent::types::ThinkingLevel::High
        );
        assert_eq!(
            initial_thinking_level(
                None,
                true,
                Some(pi_agent::types::ThinkingLevel::Off),
                Some(pi_agent::types::ThinkingLevel::Low),
                None,
                &reasoning
            ),
            pi_agent::types::ThinkingLevel::Off
        );
        assert_eq!(
            initial_thinking_level(
                None,
                true,
                None,
                Some(pi_agent::types::ThinkingLevel::Low),
                None,
                &reasoning
            ),
            pi_agent::types::ThinkingLevel::Medium
        );
        assert_eq!(
            initial_thinking_level(
                None,
                true,
                None,
                Some(pi_agent::types::ThinkingLevel::Low),
                Some(pi_agent::types::ThinkingLevel::High),
                &reasoning
            ),
            pi_agent::types::ThinkingLevel::High
        );
    }

    /// A fresh session takes the per-model override, then the settings
    /// default, then "medium" (`sdk.ts:262-271`).
    #[test]
    fn a_fresh_session_takes_the_override_then_the_default() {
        let reasoning = model(true);
        assert_eq!(
            initial_thinking_level(
                None,
                false,
                None,
                Some(pi_agent::types::ThinkingLevel::Low),
                Some(pi_agent::types::ThinkingLevel::High),
                &reasoning
            ),
            pi_agent::types::ThinkingLevel::Low
        );
        assert_eq!(
            initial_thinking_level(
                None,
                false,
                None,
                None,
                Some(pi_agent::types::ThinkingLevel::High),
                &reasoning
            ),
            pi_agent::types::ThinkingLevel::High
        );
        assert_eq!(
            initial_thinking_level(None, false, None, None, None, &reasoning),
            pi_agent::types::ThinkingLevel::Medium
        );
    }

    /// The CLI flag wins over everything and the resolved level clamps to the
    /// model's supported levels (`sdk.ts:253` + `sdk.ts:277`,
    /// `clampThinkingLevel`): `xhigh` without a `thinkingLevelMap` lands on
    /// `high`, and a non-reasoning model only ever supports `off`.
    #[test]
    fn the_cli_flag_wins_and_clamps_to_the_model() {
        assert_eq!(
            initial_thinking_level(
                Some(pi_agent::types::ThinkingLevel::Xhigh),
                true,
                Some(pi_agent::types::ThinkingLevel::Off),
                None,
                None,
                &model(true)
            ),
            pi_agent::types::ThinkingLevel::High
        );
        assert_eq!(
            initial_thinking_level(
                Some(pi_agent::types::ThinkingLevel::High),
                false,
                None,
                None,
                None,
                &model(false)
            ),
            pi_agent::types::ThinkingLevel::Off
        );
    }
}
