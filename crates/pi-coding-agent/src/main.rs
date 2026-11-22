//! Pi coding agent CLI (Rust port of `@earendil-works/pi-coding-agent`).
//!
//! Binary `pi`. The startup dispatch mirrors upstream `main.ts`: mode
//! resolution (`resolveAppMode`, `main.ts:115-126`), stdout takeover
//! (`main.ts:703-707`), piped stdin (`readPipedStdin`, `main.ts:79-100`,
//! skipped for rpc; `main.ts:960-968`), prompt assembly
//! (`prepareInitialMessage`, `main.ts:258-277`, into `buildInitialMessage`,
//! `cli/initial-message.ts:20-43`), and the mode runs (`main.ts:1013-1064`).

use std::io::{BufReader, IsTerminal};
use std::path::PathBuf;
use std::sync::Arc;

use pi_ai::providers;

use pi_ai::types::ImageContent;
use pi_coding_agent::changelog::ChangelogEntry;
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
use pi_coding_agent::output_guard::{restore_stdout, take_over_stdout};
use pi_coding_agent::session::store::{NewSessionOptions, SessionManager};
use pi_coding_agent::telemetry::host::{
    ChangelogPingDecision, HostTelemetry, HostTelemetryConfig, LAST_CHANGELOG_VERSION_SETTING,
    SETTING_OTLP_ENDPOINT, WiringEnv, changelog_ping_decision,
};
use pi_coding_agent::tools::index::ToolRegistry;

/// CLI entry (`runAgentMain`, `main.ts:630-1065`): subcommands, parsing,
/// mode resolution (`main.ts:702`), piped stdin (`main.ts:960-968`), and the
/// mode dispatch (`main.ts:1013-1064`).
#[allow(clippy::too_many_lines)] // mirrors the TS main dispatch; split when extending
fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();

    // Package subcommands route before mode startup (`main.ts:648` —
    // `pi install|remove|uninstall|list|update`), like `runAuthCommand`.
    if let Some(code) = pi_coding_agent::cli::package_command::handle_package_command(&raw) {
        std::process::exit(code);
    }

    // `pi remote` routes before mode startup like `pi mcp`. It is a new host
    // arm for `pi-env` (upstream has no remote subcommand) and stays inert
    // unless a daemon/host is explicitly configured.
    if let Some(code) = pi_coding_agent::remote_env::handle_remote_command(&raw) {
        std::process::exit(code);
    }

    // `pi mcp` routes before mode startup like the package commands
    // (upstream `main.ts:665-667`): `runMcpCommand(args.slice(1), { cwd,
    // agentDir })`. The CLI's own `parse_args` must never see the `mcp` word.
    if raw.first().is_some_and(|word| word == "mcp") {
        use pi_coding_agent::cli::mcp_command::{McpCommandOptions, StdoutOutput, run_mcp_command};
        use pi_coding_agent::cli::mcp_runtime::CliMcpRuntime;
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let agent_dir = config::get_agent_dir();
        let runtime = CliMcpRuntime;
        let open_browser = |url: &str| {
            let _ = std::process::Command::new("xdg-open").arg(url).spawn();
        };
        let interactive = std::io::stdin().is_terminal();
        let prompt_redirect = |text: &str| -> Option<String> {
            if !interactive {
                return None;
            }
            eprintln!("{text}");
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                return None;
            }
            let answer = line.trim().to_string();
            (!answer.is_empty()).then_some(answer)
        };
        let status = run_mcp_command(
            &raw[1..],
            &McpCommandOptions {
                cwd: &cwd,
                agent_dir: &agent_dir,
                runtime: &runtime,
                open_url: &open_browser,
                interactive,
                prompt_redirect: Some(&prompt_redirect),
                output: &StdoutOutput,
            },
        );
        std::process::exit(status);
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

    // App-mode resolution (`main.ts:702` — `resolveAppMode`): explicit
    // `--mode rpc`/`--mode json` win; `--print` or a non-TTY stdin/stdout
    // forces print; else interactive.
    let stdin_is_tty = std::io::stdin().is_terminal();
    let stdout_is_tty = std::io::stdout().is_terminal();
    let mut app_mode = resolve_app_mode(&args, stdin_is_tty, stdout_is_tty);
    // `takeOverStdout()` for every runtime mode (`main.ts:703-707`; the plain
    // metadata commands exited above). The port records the marker — the
    // stderr diversion it stands for is structural (see the guard's module
    // doc).
    if app_mode != AppMode::Interactive {
        take_over_stdout();
    }

    // Read piped stdin content (if any) - skip for RPC mode which uses stdin
    // for JSON-RPC (`main.ts:960-968`); content present while interactive
    // flips the mode to print.
    let stdin_content = if app_mode == AppMode::Rpc {
        None
    } else {
        read_piped_stdin()
    };
    if stdin_content.is_some() && app_mode == AppMode::Interactive {
        app_mode = AppMode::Print;
    }

    let outcome = match app_mode {
        AppMode::Rpc => run_rpc(&args, &cwd, &cwd_string),
        AppMode::Json => run_json(&args, &cwd, &cwd_string, stdin_content.as_deref()),
        AppMode::Print => run_print(&args, &cwd, &cwd_string, stdin_content.as_deref()),
        AppMode::Interactive => run_interactive(&args, &cwd, &cwd_string),
    };
    match outcome {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

/// App mode (`AppMode`, `core/project-trust.ts:13`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppMode {
    Interactive,
    Print,
    Json,
    Rpc,
}

/// `resolveAppMode` (`main.ts:115-126`): `--mode rpc` and `--mode json` win
/// outright; `--print`, a piped stdin, or a piped stdout forces print mode;
/// only a fully terminal run stays interactive. Pure in the TTY flags like
/// upstream's signature so tests can drive it.
fn resolve_app_mode(args: &Args, stdin_is_tty: bool, stdout_is_tty: bool) -> AppMode {
    match args.mode {
        Some(Mode::Rpc) => return AppMode::Rpc,
        Some(Mode::Json) => return AppMode::Json,
        _ => {}
    }
    if args.print || !stdin_is_tty || !stdout_is_tty {
        return AppMode::Print;
    }
    AppMode::Interactive
}

/// `readPipedStdin` (`main.ts:79-100`): `undefined` when stdin is a TTY
/// (interactive terminal — nothing to pipe); otherwise read everything to
/// EOF and resolve with `data.trim() || undefined`.
fn read_piped_stdin() -> Option<String> {
    if std::io::stdin().is_terminal() {
        return None;
    }
    read_piped_stdin_from(&mut std::io::stdin())
}

/// The read half of `readPipedStdin` (`main.ts:89-99`): accumulate every
/// byte to EOF and resolve with `data.trim() || undefined`; an IO error
/// resolves `undefined` (upstream has no error path to mirror). Invalid
/// UTF-8 decodes with replacement characters, matching upstream's
/// `setEncoding("utf8")` chunk accumulation (`main.ts:91`).
fn read_piped_stdin_from(reader: &mut impl std::io::Read) -> Option<String> {
    let mut bytes = Vec::new();
    if reader.read_to_end(&mut bytes).is_err() {
        return None;
    }
    let data = String::from_utf8_lossy(&bytes);
    let trimmed = data.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
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

/// Upstream's install/update ping lives in interactive mode's
/// `getChangelogForDisplay` (`interactive-mode.ts:1373-1403`), so headless
/// modes never run the changelog half of the wiring (`None` here).
fn interactive_changelog_decision(
    mode: &str,
    has_messages: bool,
    last_version: Option<&str>,
    entries: &[ChangelogEntry],
) -> Option<ChangelogPingDecision> {
    (mode == "interactive").then(|| changelog_ping_decision(has_messages, last_version, entries))
}

/// Every non-RPC mode: headless extension host, like upstream's
/// non-interactive paths that pass no `uiContext`. The returned
/// [`HostTelemetry`] owns the startup ping and the shutdown exporter; both
/// are inert unless settings/env opt in.
fn build_runtime(
    args: &Args,
    cwd: &std::path::Path,
    cwd_string: &str,
    mode: &str,
) -> Result<(PrintRuntime, HostTelemetry, bool), String> {
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
) -> Result<(PrintRuntime, HostTelemetry, bool), String> {
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
    // The extension-visible mode string: the port's internal "interactive"
    // is upstream's "tui" (the context's `mode` field,
    // `interactive-mode.ts:2764`; print/json/rpc keep their names).
    let extension_mode = if mode == "interactive" { "tui" } else { mode };
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
        extension_mode,
        ui.map(|sink| (sink, extension_mode.to_string())),
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
    let mut settings = SettingsManager::create(
        cwd,
        &agent_dir,
        SettingsManagerCreateOptions {
            project_trusted: Some(project_trusted),
        },
    );

    // Telemetry startup wiring (`core/telemetry.ts:8-13`): the trusted
    // settings manager supplies the install-telemetry setting, the additive
    // OTLP endpoint comes from the setting or `PI_RS_OTLP_ENDPOINT`, and the
    // default wiring stays inert (no ping, no HTTP transport).
    let host_telemetry = HostTelemetry::new(HostTelemetryConfig::resolve(
        settings.get_enable_install_telemetry(),
        settings
            .get(SETTING_OTLP_ENDPOINT)
            .and_then(serde_json::Value::as_str),
        &WiringEnv::from_process(),
    ));

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

    // Upstream's install/update ping fires from `getChangelogForDisplay`
    // (`interactive-mode.ts:1373-1403`): interactive mode, fresh session,
    // fresh install or a newer changelog entry. The saved version is recorded
    // first, exactly like upstream (`:1379`, `:1393`, `:1397`).
    if let Some(decision) = interactive_changelog_decision(
        mode,
        has_existing_session,
        settings
            .get(LAST_CHANGELOG_VERSION_SETTING)
            .and_then(serde_json::Value::as_str),
        &pi_coding_agent::changelog::parse_changelog(
            &pi_coding_agent::changelog::get_changelog_path(),
        ),
    ) {
        match decision {
            ChangelogPingDecision::Skip => {}
            ChangelogPingDecision::RecordVersionOnly => {
                settings.set(
                    LAST_CHANGELOG_VERSION_SETTING,
                    serde_json::json!(env!("CARGO_PKG_VERSION")),
                );
            }
            ChangelogPingDecision::PingAndRecord => {
                settings.set(
                    LAST_CHANGELOG_VERSION_SETTING,
                    serde_json::json!(env!("CARGO_PKG_VERSION")),
                );
                host_telemetry.report_install_ping(env!("CARGO_PKG_VERSION"));
            }
        }
    }

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
    let system_prompt = system_prompt_for(args, cwd_string, &agent_dir, project_trusted)?;

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
    // `images.blockImages` is a convertToLlm-time defense-in-depth filter
    // (sdk.ts:296-331). Snapshot it here like retry/compaction; the interactive
    // host takes its own per-run snapshot from the settings selector.
    let block_images = settings.get_block_images();
    Ok((
        PrintRuntime {
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
        },
        host_telemetry,
        block_images,
    ))
}

/// The initial prompt pieces: `(initialMessage, initialImages, followUps)`.
type PromptParts = (Option<String>, Vec<ImageContent>, Vec<String>);

/// `prepareInitialMessage` (`main.ts:258-277`) fanning into
/// `buildInitialMessage` (`cli/initial-message.ts:20-43`): with no `@file`
/// arguments only stdin content and the CLI messages join; otherwise the
/// files process first. The parts order is stdinContent, fileText, first
/// message (shifted off); they join with no separator (`parts.join("")`),
/// the stdin part keys on presence (`!== undefined`) while an empty `@file`
/// text block is skipped by the truthy `if (fileText)` check, and the
/// remaining messages become follow-ups.
fn resolve_prompt_messages(
    args: &Args,
    cwd: &std::path::Path,
    stdin_content: Option<&str>,
) -> Result<PromptParts, String> {
    let mut messages = args.messages.clone();
    let mut images = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    if let Some(stdin) = stdin_content {
        parts.push(stdin.to_string());
    }
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

/// The print-mode dispatch arm (`main.ts:1050-1059` — `runPrintMode` with
/// `toPrintOutputMode`'s text output, `main.ts:128-130`): prompt assembly
/// runs `prepareInitialMessage` (`main.ts:258-277`) over piped stdin
/// content, `@file` arguments, and the CLI messages.
fn run_print(
    args: &Args,
    cwd: &std::path::Path,
    cwd_string: &str,
    stdin_content: Option<&str>,
) -> Result<i32, String> {
    let (mut runtime, host_telemetry, block_images) =
        build_runtime(args, cwd, cwd_string, "print")?;
    let (initial, images, follow_ups) = resolve_prompt_messages(args, cwd, stdin_content)?;
    let options = PrintModeOptions {
        mode: Mode::Text,
        messages: follow_ups,
        initial_message: initial,
        initial_images: images,
    };
    // `restoreStdout()` after the mode returns (`main.ts:1059`; no theme
    // watcher to stop first).
    let outcome = run_print_mode(&mut runtime, &options, block_images);
    host_telemetry.shutdown();
    restore_stdout();
    outcome
}

/// Interactive mode: the chat TUI. Initial `@file`/message arguments ride
/// along like upstream's `initialMessage` (P6 part 2).
fn run_interactive(args: &Args, cwd: &std::path::Path, cwd_string: &str) -> Result<i32, String> {
    // The interactive `ctx.ui` bridge (E3 slice 6a): built before
    // extensions load so a dialog can open during `session_start`
    // (`createExtensionUIContext`, `interactive-mode.ts:2763-2830`; the
    // RPC twin wires the same sink at `main.ts:742`'s arm).
    #[cfg(feature = "ext-js")]
    let extension_ui = std::sync::Arc::new(
        pi_coding_agent::modes::interactive::extension_ui::ExtensionUiBridge::new(),
    );
    #[cfg(feature = "ext-js")]
    let (mut runtime, host_telemetry, _block_images) = build_runtime_with_ui(
        args,
        cwd,
        cwd_string,
        "interactive",
        Some(Arc::clone(&extension_ui) as Arc<dyn pi_ext_js::UiSink>),
    )?;
    #[cfg(not(feature = "ext-js"))]
    let (mut runtime, host_telemetry, _block_images) =
        build_runtime(args, cwd, cwd_string, "interactive")?;
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
        extension_cli_paths: args.extensions.clone().unwrap_or_default(),
        no_extensions: args.no_extensions,
        #[cfg(feature = "ext-js")]
        extension_ui: Some(extension_ui),
    };
    let outcome = pi_coding_agent::modes::interactive::interactive_mode::run_interactive_mode(
        &mut runtime,
        &options,
    );
    host_telemetry.shutdown();
    outcome
}

/// The JSON dispatch arm (`main.ts:1050-1059` with `toPrintOutputMode`'s
/// json output, `main.ts:128-130`): the same prompt assembly as print mode,
/// streamed as JSON events.
fn run_json(
    args: &Args,
    cwd: &std::path::Path,
    cwd_string: &str,
    stdin_content: Option<&str>,
) -> Result<i32, String> {
    let (mut runtime, host_telemetry, block_images) = build_runtime(args, cwd, cwd_string, "json")?;
    let (initial, images, follow_ups) = resolve_prompt_messages(args, cwd, stdin_content)?;
    let options = PrintModeOptions {
        mode: Mode::Json,
        messages: follow_ups,
        initial_message: initial,
        initial_images: images,
    };
    // `restoreStdout()` after the mode returns (`main.ts:1059`).
    let outcome = run_json_mode(&mut runtime, &options, block_images);
    host_telemetry.shutdown();
    restore_stdout();
    outcome
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
    let (mut runtime, host_telemetry, block_images) =
        build_runtime_with_ui(args, cwd, cwd_string, "rpc", Some(Arc::clone(&bridge.sink)))?;
    #[cfg(not(feature = "ext-js"))]
    let (mut runtime, host_telemetry, block_images) = build_runtime(args, cwd, cwd_string, "rpc")?;
    let outcome = run_rpc_mode(&mut runtime, &mut bridge, block_images);
    host_telemetry.shutdown();
    outcome
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

    /// The install/update ping is an interactive-only startup action
    /// (`interactive-mode.ts:1373-1403`). Headless modes never reach it, and
    /// a resumed session suppresses it in interactive mode too.
    #[test]
    fn changelog_ping_wiring_is_interactive_and_fresh_only() {
        let entries = [ChangelogEntry {
            major: 1,
            minor: 2,
            patch: 3,
            content: String::new(),
        }];
        assert_eq!(
            interactive_changelog_decision("print", false, None, &entries),
            None
        );
        assert_eq!(
            interactive_changelog_decision("json", false, None, &entries),
            None
        );
        assert_eq!(
            interactive_changelog_decision("rpc", false, None, &entries),
            None
        );
        assert_eq!(
            interactive_changelog_decision("interactive", false, None, &entries),
            Some(ChangelogPingDecision::PingAndRecord)
        );
        assert_eq!(
            interactive_changelog_decision("interactive", true, None, &entries),
            Some(ChangelogPingDecision::Skip)
        );
        assert_eq!(
            interactive_changelog_decision("interactive", false, Some("1.2.3"), &entries),
            Some(ChangelogPingDecision::Skip)
        );
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

    // --- piped stdin slice (`main.ts:79-126`, `main.ts:960-968`,
    // `cli/initial-message.ts:20-43`) ---

    /// `resolveAppMode` (`main.ts:115-126`): rpc and json win outright;
    /// `--print` or a non-TTY stdin/stdout forces print; only a fully
    /// terminal run stays interactive. `echo hi | pi` lands in print mode
    /// through the `!stdinIsTTY` arm.
    #[test]
    fn app_mode_resolution_mirrors_resolve_app_mode() {
        let plain = Args::default();
        assert_eq!(resolve_app_mode(&plain, true, true), AppMode::Interactive);
        assert_eq!(resolve_app_mode(&plain, false, true), AppMode::Print);
        assert_eq!(resolve_app_mode(&plain, true, false), AppMode::Print);
        assert_eq!(resolve_app_mode(&plain, false, false), AppMode::Print);
        let print_args = Args {
            print: true,
            ..Default::default()
        };
        assert_eq!(resolve_app_mode(&print_args, true, true), AppMode::Print);
        let rpc = Args {
            mode: Some(Mode::Rpc),
            ..Default::default()
        };
        assert_eq!(resolve_app_mode(&rpc, false, false), AppMode::Rpc);
        let json = Args {
            mode: Some(Mode::Json),
            ..Default::default()
        };
        assert_eq!(resolve_app_mode(&json, false, false), AppMode::Json);
        // `--mode text` is neither rpc nor json; the TTY check decides.
        let text = Args {
            mode: Some(Mode::Text),
            ..Default::default()
        };
        assert_eq!(resolve_app_mode(&text, true, true), AppMode::Interactive);
    }

    /// `readPipedStdin`'s read half (`main.ts:89-99`): everything to EOF,
    /// trimmed; whitespace-only content resolves to `undefined`
    /// (`data.trim() || undefined`, `main.ts:96`).
    #[test]
    fn piped_stdin_trims_and_maps_whitespace_only_to_none() {
        let got = read_piped_stdin_from(&mut &b"hi\n"[..]);
        assert_eq!(got, Some("hi".to_string()));
        let got = read_piped_stdin_from(&mut &b"  hello world  \n"[..]);
        assert_eq!(got, Some("hello world".to_string()));
        assert_eq!(read_piped_stdin_from(&mut &b"  \n\t"[..]), None);
        assert_eq!(read_piped_stdin_from(&mut &b""[..]), None);
    }

    /// Invalid UTF-8 decodes with replacement characters, matching
    /// upstream's `setEncoding("utf8")` chunk accumulation (`main.ts:91`).
    #[test]
    fn piped_stdin_decodes_invalid_utf8_lossily() {
        let got = read_piped_stdin_from(&mut &[0xff_u8, 0xfe][..]);
        assert_eq!(got, Some("\u{fffd}\u{fffd}".to_string()));
    }

    /// `buildInitialMessage` (`cli/initial-message.ts:20-43`) joins stdin
    /// content, `@file` text, and the first CLI message in that order with
    /// no separator (`parts.join("")`); remaining messages become follow-ups.
    #[test]
    fn stdin_file_text_and_first_message_join_with_no_separator() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"alpha").expect("write");
        let args = Args {
            file_args: vec!["notes.txt".to_string()],
            messages: vec!["first".to_string(), "second".to_string()],
            ..Default::default()
        };
        let resolved = resolve_prompt_messages(&args, dir.path(), Some("piped"));
        let (initial, images, follow_ups) = resolved.expect("ok");
        let expected = format!(
            "piped<file name=\"{}\">\nalpha\n</file>\nfirst",
            path.display()
        );
        assert_eq!(initial, Some(expected));
        assert_eq!(images, Vec::<ImageContent>::new());
        assert_eq!(follow_ups, vec!["second".to_string()]);
    }

    /// Stdin content rides first even without `@file` arguments; the first
    /// CLI message still shifts off (`initial-message.ts:34-37`).
    #[test]
    fn stdin_content_rides_first_without_file_arguments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = Args {
            messages: vec!["hello".to_string()],
            ..Default::default()
        };
        let resolved = resolve_prompt_messages(&args, dir.path(), Some("piped"));
        let (initial, images, follow_ups) = resolved.expect("ok");
        assert_eq!(initial, Some("pipedhello".to_string()));
        assert_eq!(images, Vec::<ImageContent>::new());
        assert_eq!(follow_ups, Vec::<String>::new());
    }

    /// The stdin part keys on `stdinContent !== undefined`
    /// (`initial-message.ts:27`), unlike the truthy `if (fileText)` check
    /// (`initial-message.ts:30`): a present-but-empty stdin part still makes
    /// the joined message defined, while an empty `@file` text block is
    /// skipped.
    #[test]
    fn stdin_part_keys_on_presence_while_empty_file_text_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let resolved = resolve_prompt_messages(&Args::default(), dir.path(), Some(""));
        let (initial, _, _) = resolved.expect("ok");
        assert_eq!(initial, Some(String::new()));
        std::fs::write(dir.path().join("empty.txt"), b"").expect("write");
        let args = Args {
            file_args: vec!["empty.txt".to_string()],
            ..Default::default()
        };
        let resolved = resolve_prompt_messages(&args, dir.path(), Some("piped"));
        let (initial, images, follow_ups) = resolved.expect("ok");
        assert_eq!(initial, Some("piped".to_string()));
        assert_eq!(images, Vec::<ImageContent>::new());
        assert_eq!(follow_ups, Vec::<String>::new());
    }

    /// No stdin, files, or messages yields no initial message
    /// (`initial-message.ts:40`).
    #[test]
    fn no_parts_yields_no_initial_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let resolved = resolve_prompt_messages(&Args::default(), dir.path(), None);
        let (initial, images, follow_ups) = resolved.expect("ok");
        assert_eq!(initial, None);
        assert_eq!(images, Vec::<ImageContent>::new());
        assert_eq!(follow_ups, Vec::<String>::new());
    }
}
