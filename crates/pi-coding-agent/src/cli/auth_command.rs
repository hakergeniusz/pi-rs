//! `pi auth <command>` (`cli/auth-command.ts`, `cli/auth-login.ts`,
//! `cli/auth-logout.ts`, `cli/credential-print.ts`, `cli/auth-check.ts`).
//!
//! Upstream routes this before argument parsing (`main.ts:622`,
//! `runAuthCommand` `:171-262`). The port keeps the same entry point:
//! [`handle_auth_command`] returns `None` when the first word is not `auth`,
//! otherwise the process exit code. Every parse/usage error below is the
//! upstream string, so an unimplemented branch reports honestly instead of
//! falling through into the prompt text.
//!
//! Deviation (mechanism only): the upstream login interaction is an async
//! terminal with secret echo suppression and a browser callback server; the
//! port drives the same pi-ai OAuth flow through a line-oriented terminal
//! ([`run_oauth_login`]) and stores the result through [`AuthStorage`].

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use pi_ai::auth::{AuthEvent, AuthPrompt};

use crate::auth::{AuthStorage, Credential};
use crate::cli::args::{Args, parse_args};
use crate::config::{APP_NAME, get_auth_path};
use crate::model_resolver::{
    ModelReferenceRequest, ModelReferenceResult, ThinkingSuffix, resolve_model_reference,
};
use crate::models_state;
use crate::modes::print_mode::{WireCredential, resolve_wire_credential_checked};
use crate::oauth::{
    BuiltinFlowPoll, BuiltinLoginFlow, find_matching_providers, login_provider_options,
    provider_auth_status, refresh_stored_oauth,
};

const AUTH_LOGIN_USAGE: &str = "pi auth login <provider> [--api-key] [--no-browser]";
const AUTH_LOGOUT_USAGE: &str = "pi auth logout <provider>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthKind {
    Check,
    ApiKey,
    BearerToken,
}

impl AuthKind {
    fn name(self) -> &'static str {
        match self {
            Self::Check => "auth check",
            Self::ApiKey => "auth print-api-key",
            Self::BearerToken => "auth print-bearer-token",
        }
    }

    fn usage(self) -> String {
        match self {
            Self::Check => format!(
                "{APP_NAME} auth check --provider <provider> [--json] [--credentials] [--no-refresh]"
            ),
            Self::ApiKey => {
                format!("{APP_NAME} auth print-api-key [--provider <provider>] [--model <model>]")
            }
            Self::BearerToken => format!(
                "{APP_NAME} auth print-bearer-token [--provider <provider>] [--model <model>] [--min-expiry <duration>]"
            ),
        }
    }
}

/// `isAuthCommandHelp` (`auth-command.ts:30-36`) + `printAuthCommandHelp`
/// (`:38-66`), with upstream's exact text.
pub fn is_auth_command_help(args: &[String]) -> bool {
    args.first().is_some_and(|word| word == "auth")
        && (args.get(1).is_none_or(|word| word == "help")
            || args.iter().any(|word| word == "--help" || word == "-h"))
}

fn print_auth_help() {
    println!("Usage:");
    println!("  {AUTH_LOGIN_USAGE}");
    println!("  {APP_NAME} auth logout <provider>");
    println!("  {APP_NAME} auth print-api-key [--provider <provider>] [--model <model>]");
    println!(
        "  {APP_NAME} auth print-bearer-token [--provider <provider>] [--model <model>] [--min-expiry <duration>]"
    );
    println!(
        "  {APP_NAME} auth check [--provider <provider>] [--model <model>] [--json] [--credentials] [--no-refresh]"
    );
    println!();
    println!(
        "Log in to and out of model providers, print their credentials for external clients, or check that they are ready."
    );
    // The port keeps credentials under XDG roots, not a literal ~/.pi/agent.
    println!("Credentials are stored in {}.", get_auth_path().display());
    println!();
    println!("Commands:");
    println!(
        "  login <provider>        Sign in with an account in the browser, or save an API key, then exit"
    );
    println!(
        "  logout <provider>       Remove the stored credentials; environment variables and models.json stay"
    );
    println!("  print-api-key           Print the resolved API key");
    println!(
        "  print-bearer-token      Print a resolved OAuth bearer token (refreshes if expired)"
    );
    println!("  check                   Print ready, not_ready, or invalid; exit with 0, 1, or 2");
    println!();
    println!(
        "Options for print-api-key, print-bearer-token, and check (at least one is required):"
    );
    println!("  --provider <provider>   Resolve credentials for a provider");
    println!("  --model <model>         Resolve credentials from a model");
    println!();
    println!("Options for login:");
    println!("  --api-key               Save an API key instead of signing in with an account");
    println!(
        "  --no-browser            Do not open a browser; use another sign-in method, e.g. device code"
    );
    println!();
    println!("Other options:");
    println!(
        "  --min-expiry <duration> Minimum remaining token lifetime for print-bearer-token, e.g. 30m or 1h"
    );
    println!("  --json                  Print the check result as JSON");
    println!(
        "  --credentials           Emit the credential from check, or include it in JSON output"
    );
    println!("  --no-refresh            Do not refresh expired OAuth credentials during check");
}

/// The `runAuthCommand` entry (`main.ts:171-262`).
pub fn handle_auth_command(raw: &[String]) -> Option<i32> {
    if raw.first().is_none_or(|word| word != "auth") {
        return None;
    }
    Some(match raw.get(1).map(String::as_str) {
        None | Some("help") => {
            print_auth_help();
            0
        }
        Some("login") => run_login(&raw[2..]),
        Some("logout") => run_logout(&raw[2..]),
        Some(other) => match parse_kind(other) {
            Ok(kind) => run_print_or_check(kind, &raw[2..]),
            Err(message) => {
                eprintln!("Error: {message}");
                1
            }
        },
    })
}

fn parse_kind(subcommand: &str) -> Result<AuthKind, String> {
    match subcommand {
        "check" => Ok(AuthKind::Check),
        "print-api-key" => Ok(AuthKind::ApiKey),
        "print-bearer-token" => Ok(AuthKind::BearerToken),
        other => Err(format!(
            "Unknown auth command \"{other}\". Use \"{APP_NAME} auth login\", \"{APP_NAME} auth logout\", \"{APP_NAME} auth print-api-key\", \"{APP_NAME} auth print-bearer-token\", or \"{APP_NAME} auth check\"."
        )),
    }
}

/// `parseAuthCommand` (`auth-command.ts:68-110`) plus the option validation
/// in `runAuthCommand` (`:200-215`).
fn parse_print_args(kind: AuthKind, args: &[String]) -> Result<(Args, bool, bool, bool), String> {
    let mut rest = Vec::new();
    let mut json = false;
    let mut credentials = false;
    let mut no_refresh = false;
    let mut index = 0usize;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--min-expiry" => {
                if kind != AuthKind::BearerToken {
                    return Err("--min-expiry is only supported by print-bearer-token".to_string());
                }
                index += 1;
                let value = args.get(index).map_or("", String::as_str);
                if parse_duration(value).is_none() {
                    return Err("--min-expiry must use a duration such as 30m or 1h".to_string());
                }
            }
            "--json" | "--credentials" | "--no-refresh" => {
                if kind != AuthKind::Check {
                    return Err(format!("{arg} is only supported by auth check"));
                }
                match arg.as_str() {
                    "--json" => json = true,
                    "--credentials" => credentials = true,
                    _ => no_refresh = true,
                }
            }
            _ => rest.push(arg.clone()),
        }
        index += 1;
    }

    let parsed = parse_args(&rest);
    if let Some(option) = parsed.unknown_flags.keys().next() {
        eprintln!("Unknown option --{option} for \"{}\".", kind.name());
        eprintln!("Use \"{APP_NAME} --help\" or \"{}\".", kind.usage());
        return Err(String::new());
    }
    if !parsed.diagnostics.is_empty() {
        let joined = parsed
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        return Err(joined);
    }
    validate_auth_args(&parsed, kind)?;
    Ok((parsed, json, credentials, no_refresh))
}

/// `validateAuthCommandArgs` (`auth-command.ts:112-134`).
fn validate_auth_args(args: &Args, kind: AuthKind) -> Result<(), String> {
    let provider = args
        .provider
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    let model = args
        .model
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    if args.api_key.is_some() || !args.messages.is_empty() || !args.file_args.is_empty() {
        return Err("Auth commands only accept --provider and --model".to_string());
    }
    if !provider && !model {
        return Err(if kind == AuthKind::Check {
            "Auth checks require --provider <provider> or --model <model>"
        } else {
            "Credential printing requires --provider <provider> or --model <model>"
        }
        .to_string());
    }
    Ok(())
}

/// `--min-expiry` parse (`auth-command.ts:88-96`): `<number><ms|s|m|h>`.
fn parse_duration(value: &str) -> Option<Duration> {
    let split = value.find(|c: char| !c.is_ascii_digit())?;
    let (amount, unit) = value.split_at(split);
    let amount: u64 = amount.parse().ok()?;
    let multiplier = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return None,
    };
    Some(Duration::from_millis(amount.saturating_mul(multiplier)))
}

/// Resolve `--provider`/`--model` to one provider id, mirroring
/// `resolveCredentialForPrint` (`credential-print.ts:20-70`) and
/// `checkProviderAuth` (`auth-check.ts:17-40`).
fn resolve_provider(parsed: &Args) -> Result<String, String> {
    let provider = parsed
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let model = parsed
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(provider) = provider {
        if !models_state::providers().iter().any(|id| id == provider) {
            return Err(format!(
                "Unknown provider \"{provider}\". Use --list-models to see available providers."
            ));
        }
        return Ok(provider.to_string());
    }
    let Some(model) = model else {
        return Err("Unable to resolve an auth provider".to_string());
    };
    let all = models_state::all();
    let result = resolve_model_reference(
        &ModelReferenceRequest {
            text: model,
            provider: None,
            thinking_suffix: ThinkingSuffix::Strict,
            fuzzy: true,
        },
        &all,
        &|_| true,
    );
    match result {
        ModelReferenceResult::Resolved { model, .. } => Ok(model.provider),
        ModelReferenceResult::Ambiguous { .. } | ModelReferenceResult::NotFound => Err(format!(
            "Model \"{model}\" not found. Use --list-models to see available models."
        )),
    }
}

fn run_print_or_check(kind: AuthKind, args: &[String]) -> i32 {
    let (parsed, json, credentials, no_refresh) = match parse_print_args(kind, args) {
        Ok(values) => values,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("Error: {message}");
            }
            return 1;
        }
    };
    let provider = match resolve_provider(&parsed) {
        Ok(provider) => provider,
        Err(message) => {
            eprintln!("Error: {message}");
            return 1;
        }
    };
    let auth_path = get_auth_path();
    let stored = AuthStorage::create(&auth_path)
        .read(&provider)
        .ok()
        .flatten();
    match kind {
        AuthKind::ApiKey => match stored {
            Some(Credential::Oauth { .. }) => {
                eprintln!(
                    "Error: Provider \"{provider}\" is configured with OAuth, not an API key"
                );
                1
            }
            _ => match resolve_wire_credential_checked(&provider, None, &auth_path) {
                Ok(Some(resolution)) => match resolution.credential {
                    WireCredential::ApiKey(key) => {
                        println!("{key}");
                        0
                    }
                    WireCredential::AccessToken(_) => {
                        eprintln!(
                            "Error: Provider \"{provider}\" is configured with OAuth, not an API key"
                        );
                        1
                    }
                },
                Ok(None) => {
                    eprintln!("Error: No usable API key is configured");
                    1
                }
                Err(error) => {
                    eprintln!("Error: {}", error.message);
                    1
                }
            },
        },
        AuthKind::BearerToken => {
            if let Some(Credential::Oauth { .. }) = &stored {
                let refreshed = if no_refresh {
                    stored.clone()
                } else {
                    refresh_stored_oauth(
                        &provider,
                        stored.as_ref().expect("oauth credential"),
                        &auth_path,
                    )
                };
                match refreshed {
                    Some(Credential::Oauth { access, .. }) => {
                        println!("{access}");
                        0
                    }
                    Some(Credential::ApiKey { .. }) => {
                        eprintln!(
                            "Error: Provider \"{provider}\" is not configured with an OAuth bearer token"
                        );
                        1
                    }
                    None => {
                        eprintln!("Error: No usable OAuth bearer token is configured");
                        1
                    }
                }
            } else {
                eprintln!(
                    "Error: Provider \"{provider}\" is not configured with an OAuth bearer token"
                );
                1
            }
        }
        AuthKind::Check => run_check(&provider, &auth_path, json, credentials, no_refresh),
    }
}

/// `checkProviderAuth` (`auth-check.ts:17-62`) and `runAuthCommand`'s check
/// arm (`main.ts:226-254`).
fn run_check(
    provider: &str,
    auth_path: &Path,
    json: bool,
    credentials: bool,
    no_refresh: bool,
) -> i32 {
    let known = models_state::providers().iter().any(|id| id == provider);
    let config = models_state::models_json_provider(provider);
    let extension = models_state::extension_provider_config(provider);
    let status = provider_auth_status(provider, config.as_ref(), extension.as_ref(), auth_path);
    let (state, reason, auth_type, credential) = if !known {
        ("not_ready", "provider_not_found", None, None)
    } else if !status.as_ref().is_some_and(|status| status.configured) {
        ("not_ready", "credentials_not_configured", None, None)
    } else {
        let auth_type = status.as_ref().map(|status| status.auth_type);
        if no_refresh {
            let stored = AuthStorage::create(auth_path).read(provider).ok().flatten();
            let value = match stored {
                Some(Credential::ApiKey { key: Some(key), .. }) => Some(key),
                Some(Credential::Oauth { access, .. }) => Some(access),
                _ => None,
            };
            if value.is_none() {
                ("not_ready", "credentials_not_configured", auth_type, None)
            } else {
                ("ready", "", auth_type, value)
            }
        } else {
            match resolve_wire_credential_checked(provider, None, auth_path) {
                Ok(Some(resolution)) => {
                    let value = match resolution.credential {
                        WireCredential::ApiKey(key) => key,
                        WireCredential::AccessToken(token) => token,
                    };
                    ("ready", "", auth_type, Some(value))
                }
                Ok(None) => ("not_ready", "credentials_not_configured", auth_type, None),
                Err(_) => ("invalid", "invalid_state", auth_type, None),
            }
        }
    };
    if json {
        let mut object = serde_json::json!({ "status": state, "provider": provider });
        if !reason.is_empty() {
            object["reason"] = serde_json::json!(reason);
        }
        if let Some(auth_type) = auth_type {
            object["authType"] = serde_json::json!(auth_type);
        }
        if credentials && let Some(value) = credential {
            object["credentials"] = serde_json::json!(value);
        }
        println!("{object}");
    } else if credentials {
        println!("{}", credential.unwrap_or_else(|| state.to_string()));
    } else {
        println!("{state}");
    }
    match state {
        "ready" => 0,
        "not_ready" => 1,
        _ => 2,
    }
}

/// `parseLoginRequest` (`auth-login.ts:100-117`).
fn parse_login_args(args: &[String]) -> Result<(String, bool, bool), String> {
    let mut provider_ref = String::new();
    let mut api_key = false;
    let mut browser = true;
    for arg in args {
        match arg.as_str() {
            "--api-key" => api_key = true,
            "--no-browser" => browser = false,
            other if other.starts_with('-') || !provider_ref.is_empty() => {
                return Err(format!("Usage: {AUTH_LOGIN_USAGE}"));
            }
            other => provider_ref = other.to_string(),
        }
    }
    if provider_ref.is_empty() {
        return Err(format!("Usage: {AUTH_LOGIN_USAGE}"));
    }
    if api_key && !browser {
        return Err(
            "--no-browser only applies to account sign-in; omit it with --api-key.".to_string(),
        );
    }
    Ok((provider_ref, api_key, browser))
}

/// `runAuthLogin` (`auth-login.ts:28-95`).
fn run_login(args: &[String]) -> i32 {
    let (provider_ref, wants_api_key, browser) = match parse_login_args(args) {
        Ok(values) => values,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    let auth_path = get_auth_path();
    let options = login_provider_options(&auth_path);
    let Some(provider) = find_matching_providers(&options, &provider_ref)
        .first()
        .copied()
    else {
        eprintln!("Unknown provider \"{provider_ref}\".");
        let mut ids = options
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        eprintln!("Providers: {}", ids.join(", "));
        return 1;
    };
    if !std::io::stdin().is_terminal() {
        eprintln!("\"{AUTH_LOGIN_USAGE}\" requires an interactive terminal");
        return 1;
    }
    let has_api_key_login = options
        .iter()
        .any(|row| row.id == provider.id && row.auth_type == "api_key");
    let has_oauth = provider.auth_type == "oauth"
        || options
            .iter()
            .any(|row| row.id == provider.id && row.auth_type == "oauth");
    if wants_api_key && !has_api_key_login {
        eprintln!(
            "{} has no API key login. Omit --api-key to sign in.",
            provider.name
        );
        return 1;
    }
    let use_oauth = !wants_api_key && has_oauth;
    if !use_oauth && !has_api_key_login {
        eprintln!(
            "{} is configured outside {APP_NAME} (ambient credentials).",
            provider.name
        );
        return 1;
    }
    let credential = if use_oauth {
        match run_oauth_login(provider, browser) {
            Ok(credential) => credential,
            Err(message) => {
                if message.eq_ignore_ascii_case("login cancelled") {
                    eprintln!("Login cancelled.");
                } else {
                    eprintln!("Failed to log in to {}: {message}", provider.name);
                }
                return 1;
            }
        }
    } else {
        print!("API key: ");
        let _ = std::io::stdout().flush();
        let mut key = String::new();
        if std::io::stdin().read_line(&mut key).is_err() || key.trim().is_empty() {
            eprintln!("Login cancelled.");
            return 1;
        }
        Credential::ApiKey {
            key: Some(key.trim().to_string()),
            env: None,
            unknown: serde_json::Map::new(),
        }
    };
    let storage = AuthStorage::create(&auth_path);
    if let Err(error) = storage.modify(&provider.id, |_| Ok(Some(credential))) {
        eprintln!("Login failed: {error}");
        return 1;
    }
    println!(
        "{}. Credentials saved to {}",
        if use_oauth {
            format!("Logged in to {}", provider.name)
        } else {
            format!("Saved API key for {}", provider.name)
        },
        auth_path.display()
    );
    0
}

/// Drive pi-ai's built-in OAuth flow from a line terminal. The notifications
/// and prompt shapes are the same ones `createLoginInteraction`
/// (`auth-login.ts:136-232`) adapts to the TUI.
fn run_oauth_login(
    provider: &crate::oauth::AuthSelectorProvider,
    browser: bool,
) -> Result<Credential, String> {
    let flow = BuiltinLoginFlow::start(provider)
        .ok_or_else(|| format!("{} has no ported OAuth flow", provider.name))?;
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut browser_opened = false;
    loop {
        if Instant::now() > deadline {
            return Err("OAuth login timed out".to_string());
        }
        match flow.poll() {
            BuiltinFlowPoll::Idle => sleep(Duration::from_millis(100)),
            BuiltinFlowPoll::PromptClosed { .. } => {}
            BuiltinFlowPoll::Notify(event) => {
                let url = match event {
                    AuthEvent::AuthUrl { url, instructions } => {
                        println!("Open this URL to sign in:\n  {url}");
                        if let Some(instructions) = instructions {
                            println!("{instructions}");
                        }
                        Some(url)
                    }
                    AuthEvent::DeviceCode {
                        user_code,
                        verification_uri,
                        ..
                    } => {
                        println!("Open {verification_uri} and enter code: {user_code}");
                        println!("Waiting for authentication...");
                        Some(verification_uri)
                    }
                    AuthEvent::Info { message, links } => {
                        println!("{message}");
                        for link in links {
                            println!(
                                "  {}: {}",
                                link.label.as_deref().unwrap_or(&link.url),
                                link.url
                            );
                        }
                        None
                    }
                    AuthEvent::Progress { message } => {
                        println!("{message}");
                        None
                    }
                };
                if browser
                    && !browser_opened
                    && let Some(url) = url
                {
                    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
                    browser_opened = true;
                }
            }
            BuiltinFlowPoll::Prompt { id, prompt } => {
                let answer = answer_prompt(&prompt, browser)?;
                flow.answer(id, answer);
            }
            BuiltinFlowPoll::Settled(result) => return result,
        }
    }
}

/// Prompt shapes of `AuthPrompt` (`auth/types.ts:125-130`): select chooses by
/// number, all others read one line.
fn answer_prompt(prompt: &AuthPrompt, browser: bool) -> Result<String, String> {
    match prompt {
        AuthPrompt::Select { message, options } => {
            let mut choices: Vec<&pi_ai::auth::AuthPromptOption> = options.iter().collect();
            if browser && choices.len() > 1 {
                // Upstream takes the browser option without asking
                // (`selectMethod`, `auth-login.ts:145-161`).
                return Ok(choices.remove(0).id.clone());
            }
            println!("{message}");
            for (index, option) in choices.iter().enumerate() {
                println!("  {}. {}", index + 1, option.label);
            }
            let selection = read_line("Select 1-")?;
            let index = selection
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|number| number.checked_sub(1));
            choices
                .get(index.unwrap_or(usize::MAX))
                .map(|option| option.id.clone())
                .ok_or_else(|| "Login cancelled".to_string())
        }
        AuthPrompt::Text {
            message,
            placeholder,
        }
        | AuthPrompt::ManualCode {
            message,
            placeholder,
        } => {
            let hint = placeholder
                .as_deref()
                .map_or(String::new(), |p| format!(" ({p})"));
            read_line(&format!("{message}{hint}: "))
        }
        AuthPrompt::Secret {
            message,
            placeholder,
        } => {
            let hint = placeholder
                .as_deref()
                .map_or(String::new(), |p| format!(" ({p})"));
            read_line(&format!("{message}{hint}: "))
        }
    }
}

fn read_line(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) => Err("Login cancelled".to_string()),
        Ok(_) => Ok(line.trim().to_string()),
        Err(error) => Err(error.to_string()),
    }
}

/// `runAuthLogout` (`auth-logout.ts:26-88`).
fn run_logout(args: &[String]) -> i32 {
    let (provider_ref, rest) = match args.split_first() {
        Some((provider, rest)) if !provider.starts_with('-') && rest.is_empty() => {
            (provider.as_str(), rest)
        }
        _ => {
            eprintln!("Usage: {AUTH_LOGOUT_USAGE}");
            return 1;
        }
    };
    let _ = rest;
    let auth_path = get_auth_path();
    let storage = AuthStorage::create(&auth_path);
    let stored = match storage.list() {
        Ok(stored) => stored,
        Err(error) => {
            eprintln!("Could not read stored credentials: {error}");
            return 1;
        }
    };
    let options = login_provider_options(&auth_path);
    let provider = find_matching_providers(&options, provider_ref)
        .first()
        .copied();
    let credential = stored.iter().find(|candidate| {
        candidate.provider_id == provider.map_or(provider_ref, |row| row.id.as_str())
    });
    if provider.is_none() && credential.is_none() {
        eprintln!("Unknown provider \"{provider_ref}\".");
        let mut ids = stored
            .iter()
            .map(|candidate| candidate.provider_id.as_str())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        let stored_ids = ids.join(", ");
        eprintln!(
            "Stored credentials: {}",
            if ids.is_empty() { "none" } else { &stored_ids }
        );
        return 1;
    }
    let name = provider.map_or(provider_ref, |row| row.name.as_str());
    let Some(credential) = credential else {
        println!(
            "No stored credentials for {name}. Environment variables and models.json config are unchanged."
        );
        return 0;
    };
    if let Err(error) = storage.delete(&credential.provider_id) {
        eprintln!("Logout failed: {error}");
        return 1;
    }
    if credential.credential_type == "oauth" {
        println!("Logged out of {name}.");
    } else {
        println!(
            "Removed stored API key for {name}. Environment variables and models.json config are unchanged."
        );
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_detection_matches_upstream() {
        assert!(is_auth_command_help(&["auth".to_string()]));
        assert!(is_auth_command_help(&[
            "auth".to_string(),
            "help".to_string()
        ]));
        assert!(is_auth_command_help(&[
            "auth".to_string(),
            "check".to_string(),
            "--help".to_string()
        ]));
        assert!(!is_auth_command_help(&[
            "auth".to_string(),
            "login".to_string()
        ]));
        assert!(!is_auth_command_help(&["mcp".to_string()]));
    }

    #[test]
    fn durations_and_unknown_commands() {
        assert_eq!(parse_duration("30m"), Some(Duration::from_mins(30)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration("30"), None);
        assert_eq!(parse_duration("1d"), None);
        assert!(
            parse_kind("bogus")
                .unwrap_err()
                .contains("Unknown auth command \"bogus\"")
        );
    }

    #[test]
    fn option_validation_errors_are_upstream_exact() {
        assert_eq!(
            parse_print_args(AuthKind::ApiKey, &["--json".to_string()]).unwrap_err(),
            "--json is only supported by auth check"
        );
        assert_eq!(
            parse_print_args(
                AuthKind::BearerToken,
                &["--min-expiry".to_string(), "x".to_string()]
            )
            .unwrap_err(),
            "--min-expiry must use a duration such as 30m or 1h"
        );
    }
}
