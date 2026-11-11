//! Pi coding agent CLI (Rust port of `@earendil-works/pi-coding-agent`).
//!
//! Binary `pi`. Phase P4 wires print mode (`pi -p`); JSON/RPC modes (P5) and
//! interactive mode (P6) print a phase note and exit for now.

use std::path::PathBuf;
use std::sync::Arc;

use pi_ai::catalog;
use pi_ai::providers;

use pi_coding_agent::cli::args::{DiagnosticKind, Mode, parse_args, print_help};
use pi_coding_agent::config;
use pi_coding_agent::config::settings::{SettingsManager, SettingsManagerCreateOptions};
use pi_coding_agent::modes::print_mode::{
    PrintModeOptions, PrintRuntime, ProviderStreamFn, parse_model_pattern, resolve_api_key,
    resolve_model, run_print_mode, select_builtin_tools, system_prompt_for,
};
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

    let print_mode_requested =
        args.print || args.mode == Some(Mode::Json) || args.mode == Some(Mode::Rpc);
    if !print_mode_requested {
        eprintln!(
            "pi-rs: interactive mode is not implemented yet (phase P6); use -p for print mode"
        );
        std::process::exit(2);
    }

    match run_print(&args, &cwd, &cwd_string) {
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

/// Assemble the print-mode runtime and run it. Mirrors the startup path of
/// upstream `cli.ts` for non-interactive runs: settings, model, credentials,
/// session, tools, system prompt.
fn run_print(
    args: &pi_coding_agent::cli::args::Args,
    cwd: &std::path::Path,
    cwd_string: &str,
) -> Result<i32, String> {
    let agent_dir = config::get_agent_dir();
    let settings = SettingsManager::create(
        cwd,
        &agent_dir,
        SettingsManagerCreateOptions {
            project_trusted: None,
        },
    );

    // Model: --model pattern, else the settings default.
    let pattern = args.model.as_deref().map(parse_model_pattern);
    let model = resolve_model(
        pattern.as_ref(),
        settings.get_default_provider().as_deref(),
        settings.get_default_model().as_deref(),
    )?;

    // Credential ladder: flag > auth.json > provider env vars.
    let api_key = resolve_api_key(
        &model.provider,
        args.api_key.as_deref(),
        &config::get_auth_path(),
    )
    .map(|(key, source)| {
        if source == "environment" {
            eprintln!("pi-rs: using {} key from environment", model.provider);
        }
        key
    });
    if api_key.is_none() {
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

    // Tools.
    let available = ToolRegistry::all().definitions().to_vec();
    let selected = select_builtin_tools(args, &available)?;
    let executor = ToolRegistry::new(selected).executor(cwd_string);

    // System prompt.
    let system_prompt = system_prompt_for(args, cwd_string)?;

    let thinking_level = args
        .thinking
        .as_deref()
        .map(pi_coding_agent::modes::print_mode::thinking_level_from_str)
        .transpose()?
        .unwrap_or_default();

    // `-p` puts the prompt in messages[0]; upstream sends it as the initial
    // message and any further positionals as follow-ups.
    let (initial, follow_ups) = match args.messages.split_first() {
        Some((first, rest)) if args.print && rest.is_empty() => (Some(first.clone()), Vec::new()),
        Some((first, rest)) if args.print => (Some(first.clone()), rest.to_vec()),
        _ => (None, args.messages.clone()),
    };
    let mut runtime = PrintRuntime {
        model,
        thinking_level,
        api_key,
        stream_fn: Arc::new(ProviderStreamFn(stream_fn)),
        executor,
        system_prompt,
        session,
    };
    let options = PrintModeOptions {
        mode: args.mode.unwrap_or(Mode::Text),
        messages: follow_ups,
        initial_message: initial,
    };

    run_print_mode(&mut runtime, &options)
}
