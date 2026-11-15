//! Pi coding agent CLI (Rust port of `@earendil-works/pi-coding-agent`).
//!
//! Binary `pi`. Phase P4 wires print mode (`pi -p`); P5 adds JSON and RPC
//! modes; interactive mode (P6) still exits with a phase note.

use std::io::{BufReader, stdout};
use std::path::PathBuf;
use std::sync::Arc;

use pi_ai::catalog;
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
    resolve_model, resolve_wire_credential, run_print_mode, select_tools, system_prompt_for,
    thinking_level_from_str,
};
use pi_coding_agent::modes::rpc::run_rpc_mode;
use pi_coding_agent::session::store::{NewSessionOptions, SessionManager};
use pi_coding_agent::tools::index::ToolRegistry;

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
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
    if args.resume || args.fork.is_some() || args.export.is_some() {
        eprintln!("pi-rs: --resume/--fork/--export are not implemented yet (phase P5/P6)");
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
    for provider in catalog::catalog().providers() {
        for model in catalog::catalog().models(provider) {
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
fn build_runtime(
    args: &Args,
    cwd: &std::path::Path,
    cwd_string: &str,
    mode: &str,
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

    // Extensions (E2 slice 3). Diagnostics report in order: warnings to
    // stderr, error-level entries abort with the load-failure hint
    // (`main.ts:982-990`).
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

    // Model: --model pattern, else the settings default.
    let pattern = args.model.as_deref().map(parse_model_pattern);
    let model = resolve_model(
        pattern.as_ref(),
        settings.get_default_provider().as_deref(),
        settings.get_default_model().as_deref(),
    )?;

    // Credential ladder: flag > auth.json > provider env vars. OAuth tokens
    // resolve as bearer credentials, distinct from API keys.
    let (api_key, access_token) = resolve_wire_credential(
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
    });
    if api_key.is_none() && access_token.is_none() {
        return Err(format!(
            "No API key for provider {}. Run `pi auth login --provider {}`, set the provider's environment variable, or pass --api-key.",
            model.provider, model.provider
        ));
    }

    let Some(stream_fn) = providers::stream_fn_for(&model.api) else {
        return Err(format!(
            "No provider adapter for api \"{}\" (model {}/{}).",
            model.api, model.provider, model.id
        ));
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

    // Tools: builtins plus extension tools (the CLI filters apply to both;
    // extension tools append after the builtins, like the upstream
    // registry order).
    let builtin_available = ToolRegistry::all().definitions().to_vec();
    let selected = select_tools(args, &builtin_available, extensions.host.tool_definitions())?;
    let executor = extensions.host.executor(&selected, cwd_string);

    // System prompt.
    let system_prompt = system_prompt_for(args, cwd_string)?;

    // Thinking level: the `--thinking` flag wins; else the per-model
    // override (`modelThinkingLevels[provider/id]`), then the
    // `defaultThinkingLevel` setting, else off
    // (`model-resolver.js:386-421`).
    let thinking_level = if let Some(level) = args
        .thinking
        .as_deref()
        .map(thinking_level_from_str)
        .transpose()?
    {
        level
    } else {
        let key = format!("{}/{}", model.provider, model.id);
        let level = settings
            .get("modelThinkingLevels")
            .and_then(|value| value.get(key.as_str()))
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .or_else(|| settings.get_default_thinking_level())
            .unwrap_or_else(|| "off".to_string());
        thinking_level_from_str(&level).unwrap_or_default()
    };

    Ok(PrintRuntime {
        model,
        thinking_level,
        api_key,
        access_token,
        stream_fn: Arc::new(ProviderStreamFn(stream_fn)),
        executor,
        system_prompt,
        session,
        project_trusted,
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
    let mut runtime = build_runtime(args, cwd, cwd_string, "rpc")?;
    let mut stdin = BufReader::new(std::io::stdin());
    let out = stdout();
    run_rpc_mode(&mut runtime, &mut stdin, &mut out.lock())
}
