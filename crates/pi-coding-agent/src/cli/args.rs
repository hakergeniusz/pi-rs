//! CLI argument parsing (`cli/args.ts`).
//!
//! A hand-rolled parser, like upstream: clap cannot express the exact
//! semantics this must preserve — `-p` consuming the following prompt,
//! unknown long flags captured for extensions (P5), `--` ending option
//! parsing, and diagnostics that keep parsing instead of aborting.

/// Output mode (`Mode`, `args.ts:11`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Text,
    Json,
    Rpc,
}

/// TUI mode (`TuiMode` from `core/settings-manager.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiMode {
    Regular,
    Fullscreen,
}

/// A parse diagnostic (`args.ts:58`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticKind {
    Warning,
    Error,
}

/// One registered extension flag (`ExtensionFlag` in
/// `core/extensions/types.ts`), collected from the loaded host for the
/// `Extension CLI Flags:` section of `--help` (`args.ts:273-283`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionFlagInfo {
    pub name: String,
    pub description: String,
    /// `"boolean"` or `"string"`.
    pub kind: String,
}

/// Parsed CLI arguments (`Args`, `args.ts:13-59`).
///
/// Fields stay `None`/empty exactly where upstream leaves them undefined;
/// tests pin the shape. `unknown_flags` captures unrecognized long flags so
/// the extension host (P5) can claim them.
#[derive(Debug, Clone, Default)]
// Upstream's `Args` interface carries the same boolean flags 1:1.
#[allow(clippy::struct_excessive_bools)]
pub struct Args {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<Vec<String>>,
    /// The raw `--thinking` value; validity is checked here, applied later.
    pub thinking: Option<String>,
    pub r#continue: bool,
    pub resume: bool,
    pub help: bool,
    pub version: bool,
    pub mode: Option<Mode>,
    pub name: Option<String>,
    pub no_session: bool,
    pub session: Option<String>,
    pub session_id: Option<String>,
    pub fork: Option<String>,
    pub session_dir: Option<String>,
    pub models: Option<Vec<String>>,
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Option<Vec<String>>,
    pub no_tools: bool,
    pub no_builtin_tools: bool,
    pub extensions: Option<Vec<String>>,
    pub no_extensions: bool,
    pub no_mcp: bool,
    pub print: bool,
    pub export: Option<String>,
    pub no_skills: bool,
    pub skills: Option<Vec<String>>,
    pub prompt_templates: Option<Vec<String>>,
    pub no_prompt_templates: bool,
    pub themes: Option<Vec<String>>,
    pub use_theme: Option<String>,
    pub no_themes: bool,
    pub no_context_files: bool,
    /// `None` = flag absent; `Some(None)` = flag with no search pattern.
    pub list_models: Option<Option<String>>,
    pub offline: bool,
    pub tui_mode: Option<TuiMode>,
    pub verbose: bool,
    /// `true` = `--approve`, `false` = `--no-approve`, `None` = unset.
    pub project_trust_override: Option<bool>,
    pub messages: Vec<String>,
    pub file_args: Vec<String>,
    /// Unknown flags (`args.ts:57`): flag name → value (`None` = boolean).
    pub unknown_flags: std::collections::BTreeMap<String, Option<String>>,
    pub diagnostics: Vec<Diagnostic>,
}

/// The valid `--thinking` levels (`VALID_THINKING_LEVELS`, `args.ts:61`).
pub const VALID_THINKING_LEVELS: [&str; 7] =
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// `isValidThinkingLevel` (`args.ts:63-65`).
pub fn is_valid_thinking_level(level: &str) -> bool {
    VALID_THINKING_LEVELS.contains(&level)
}

/// `normalizeSessionName` (`args.ts:67-70`).
pub fn normalize_session_name(value: &str) -> Option<String> {
    let name = value.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// `isToolModifier` (`core/settings-manager.ts:90-92`): a `+name`/`-name`
/// entry that adds to or removes from the defaults instead of listing.
fn is_tool_modifier(entry: &str) -> bool {
    entry.starts_with('+') || entry.starts_with('-')
}

/// `getToolListError` (`core/settings-manager.ts:97-104`): `--tools` entries
/// must be either all names or all modifiers, and modifiers take exact names.
pub fn tool_list_error(entries: &[&str]) -> Option<String> {
    let modifiers: Vec<&&str> = entries
        .iter()
        .filter(|entry| is_tool_modifier(entry))
        .collect();
    if modifiers.is_empty() {
        return None;
    }
    if modifiers.len() < entries.len() {
        return Some("tool names cannot be mixed with +name or -name entries".to_string());
    }
    if let Some(pattern) = modifiers.iter().find(|entry| entry.contains('*')) {
        return Some(format!(
            "+name and -name entries take exact tool names, not patterns: {pattern}"
        ));
    }
    None
}

fn split_comma_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

/// `parseArgs` (`args.ts:72-271`).
// Hand-rolled port of the upstream parser loop; kept 1:1 for
// parity-auditability rather than split into helpers.
#[allow(clippy::too_many_lines)]
pub fn parse_args(input: &[String]) -> Args {
    let mut result = Args::default();

    let mut index = 0usize;
    while index < input.len() {
        let arg = input[index].as_str();

        if arg == "--" {
            for positional in &input[index + 1..] {
                if let Some(stripped) = positional.strip_prefix('@') {
                    result.file_args.push(stripped.to_string());
                } else {
                    result.messages.push(positional.clone());
                }
            }
            break;
        } else if arg == "--help" || arg == "-h" {
            result.help = true;
        } else if arg == "--version" || arg == "-v" {
            result.version = true;
        } else if arg == "--mode" {
            let Some(mode) = input.get(index + 1).filter(|mode| !mode.starts_with('-')) else {
                result.diagnostics.push(Diagnostic {
                    kind: DiagnosticKind::Error,
                    message: "--mode requires text, json, or rpc".to_string(),
                });
                // Consume the flag itself; the missing/flag-looking value stays
                // for the next iteration (upstream's for-loop advances too).
                index += 1;
                continue;
            };
            index += 1;
            result.mode = match mode.as_str() {
                "text" => Some(Mode::Text),
                "json" => Some(Mode::Json),
                "rpc" => Some(Mode::Rpc),
                other => {
                    result.diagnostics.push(Diagnostic {
                        kind: DiagnosticKind::Error,
                        message: format!("Invalid mode \"{other}\". Valid values: text, json, rpc"),
                    });
                    None
                }
            };
        } else if arg == "--continue" || arg == "-c" {
            result.r#continue = true;
        } else if arg == "--resume" || arg == "-r" {
            result.resume = true;
        } else if arg == "--provider" && index + 1 < input.len() {
            index += 1;
            result.provider = Some(input[index].clone());
        } else if arg == "--model" && index + 1 < input.len() {
            index += 1;
            result.model = Some(input[index].clone());
        } else if arg == "--api-key" && index + 1 < input.len() {
            index += 1;
            result.api_key = Some(input[index].clone());
        } else if arg == "--system-prompt" && index + 1 < input.len() {
            index += 1;
            result.system_prompt = Some(input[index].clone());
        } else if arg == "--append-system-prompt" && index + 1 < input.len() {
            index += 1;
            result
                .append_system_prompt
                .get_or_insert_with(Vec::new)
                .push(input[index].clone());
        } else if arg == "--name" || arg == "-n" {
            if index + 1 < input.len() {
                index += 1;
                result.name = Some(input[index].clone());
            } else {
                result.diagnostics.push(Diagnostic {
                    kind: DiagnosticKind::Error,
                    message: "--name requires a value".to_string(),
                });
            }
        } else if arg == "--no-session" {
            result.no_session = true;
        } else if arg == "--session" && index + 1 < input.len() {
            index += 1;
            result.session = Some(input[index].clone());
        } else if arg == "--session-id" && index + 1 < input.len() {
            index += 1;
            result.session_id = Some(input[index].clone());
        } else if arg == "--fork" && index + 1 < input.len() {
            index += 1;
            result.fork = Some(input[index].clone());
        } else if arg == "--session-dir" && index + 1 < input.len() {
            index += 1;
            result.session_dir = Some(input[index].clone());
        } else if arg == "--models" && index + 1 < input.len() {
            index += 1;
            result.models = Some(split_comma_list(&input[index]));
        } else if arg == "--no-tools" || arg == "-nt" {
            result.no_tools = true;
        } else if arg == "--no-builtin-tools" || arg == "-nbt" {
            result.no_builtin_tools = true;
        } else if (arg == "--tools" || arg == "-t") && index + 1 < input.len() {
            index += 1;
            let tools = split_comma_list(&input[index]);
            let borrowed: Vec<&str> = tools.iter().map(String::as_str).collect();
            if let Some(error) = tool_list_error(&borrowed) {
                result.diagnostics.push(Diagnostic {
                    kind: DiagnosticKind::Error,
                    message: format!("{arg}: {error}"),
                });
            } else {
                result.tools = Some(tools);
            }
        } else if (arg == "--exclude-tools" || arg == "-xt") && index + 1 < input.len() {
            index += 1;
            result.exclude_tools = Some(split_comma_list(&input[index]));
        } else if arg == "--thinking" && index + 1 < input.len() {
            index += 1;
            let level = input[index].as_str();
            if is_valid_thinking_level(level) {
                result.thinking = Some(level.to_string());
            } else {
                result.diagnostics.push(Diagnostic {
                    kind: DiagnosticKind::Warning,
                    message: format!(
                        "Invalid thinking level \"{level}\". Valid values: {}",
                        VALID_THINKING_LEVELS.join(", ")
                    ),
                });
            }
        } else if arg == "--print" || arg == "-p" {
            result.print = true;
            // `args.ts:179-183`: the next argument is the prompt unless it is
            // a file arg or an option (`---`-prefixed values still count).
            if let Some(next) = input.get(index + 1) {
                let next = next.as_str();
                if !next.starts_with('@') && (!next.starts_with('-') || next.starts_with("---")) {
                    result.messages.push(next.to_string());
                    index += 1;
                }
            }
        } else if arg == "--export" && index + 1 < input.len() {
            index += 1;
            result.export = Some(input[index].clone());
        } else if (arg == "--extension" || arg == "-e") && index + 1 < input.len() {
            index += 1;
            result
                .extensions
                .get_or_insert_with(Vec::new)
                .push(input[index].clone());
        } else if arg == "--no-extensions" || arg == "-ne" {
            result.no_extensions = true;
        } else if arg == "--no-mcp" {
            result.no_mcp = true;
        } else if arg == "--skill" && index + 1 < input.len() {
            index += 1;
            result
                .skills
                .get_or_insert_with(Vec::new)
                .push(input[index].clone());
        } else if arg == "--prompt-template" && index + 1 < input.len() {
            index += 1;
            result
                .prompt_templates
                .get_or_insert_with(Vec::new)
                .push(input[index].clone());
        } else if arg == "--theme" && index + 1 < input.len() {
            index += 1;
            result
                .themes
                .get_or_insert_with(Vec::new)
                .push(input[index].clone());
        } else if arg == "--use-theme" {
            let Some(theme_name) = input.get(index + 1).filter(|name| !name.starts_with('-'))
            else {
                result.diagnostics.push(Diagnostic {
                    kind: DiagnosticKind::Error,
                    message: "--use-theme requires a theme name".to_string(),
                });
                // Same as `--mode`: consume the flag, re-check the next token.
                index += 1;
                continue;
            };
            result.use_theme = Some(theme_name.clone());
            index += 1;
        } else if arg == "--no-skills" || arg == "-ns" {
            result.no_skills = true;
        } else if arg == "--no-prompt-templates" || arg == "-np" {
            result.no_prompt_templates = true;
        } else if arg == "--no-themes" {
            result.no_themes = true;
        } else if arg == "--no-context-files" || arg == "-nc" {
            result.no_context_files = true;
        } else if arg == "--list-models" {
            // `args.ts:218-224`: an optional search pattern follows.
            if let Some(next) = input.get(index + 1)
                && !next.starts_with('-')
                && !next.starts_with('@')
            {
                result.list_models = Some(Some(next.clone()));
                index += 1;
                continue;
            }
            result.list_models = Some(None);
        } else if arg == "--tui-mode" {
            match input.get(index + 1).map(String::as_str) {
                Some("regular") => {
                    result.tui_mode = Some(TuiMode::Regular);
                    index += 1;
                }
                Some("fullscreen") => {
                    result.tui_mode = Some(TuiMode::Fullscreen);
                    index += 1;
                }
                None => {
                    result.diagnostics.push(Diagnostic {
                        kind: DiagnosticKind::Error,
                        message: "--tui-mode requires regular or fullscreen".to_string(),
                    });
                }
                Some(other) if other.starts_with('-') => {
                    result.diagnostics.push(Diagnostic {
                        kind: DiagnosticKind::Error,
                        message: "--tui-mode requires regular or fullscreen".to_string(),
                    });
                }
                Some(other) => {
                    index += 1;
                    result.diagnostics.push(Diagnostic {
                        kind: DiagnosticKind::Error,
                        message: format!(
                            "Invalid TUI mode \"{other}\". Valid values: regular, fullscreen"
                        ),
                    });
                }
            }
        } else if arg == "--verbose" {
            result.verbose = true;
        } else if arg == "--approve" || arg == "-a" {
            result.project_trust_override = Some(true);
        } else if arg == "--no-approve" || arg == "-na" {
            result.project_trust_override = Some(false);
        } else if arg == "--offline" {
            result.offline = true;
        } else if let Some(stripped) = arg.strip_prefix('@') {
            result.file_args.push(stripped.to_string());
        } else if let Some(long) = arg.strip_prefix("--") {
            // Unknown long flag (`args.ts:249-262`): captured for extensions.
            if let Some(eq_index) = long.find('=') {
                result.unknown_flags.insert(
                    long[..eq_index].to_string(),
                    Some(long[eq_index + 1..].to_string()),
                );
            } else {
                let flag_name = long.to_string();
                match input.get(index + 1) {
                    Some(next) if !next.starts_with('-') && !next.starts_with('@') => {
                        result.unknown_flags.insert(flag_name, Some(next.clone()));
                        index += 1;
                    }
                    _ => {
                        result.unknown_flags.insert(flag_name, None);
                    }
                }
            }
        } else if arg.starts_with('-') {
            result.diagnostics.push(Diagnostic {
                kind: DiagnosticKind::Error,
                message: format!("Unknown option: {arg}"),
            });
        } else {
            result.messages.push(arg.to_string());
        }

        index += 1;
    }

    result
}

/// `printHelp` (`args.ts:270-480`), minus chalk styling. The extension flag
/// section, Examples and Environment Variables follow upstream; paths and
/// variable descriptions are adjusted for pi-rs's XDG roots.
pub fn print_help(extension_flags: &[ExtensionFlagInfo]) {
    println!(
        "{} - AI coding assistant with read, bash, edit, write tools",
        crate::config::APP_NAME
    );
    // Keep the binary's own version discoverable: upstream's `--version`
    // prints only the package version (`main.ts:683-685`).
    println!(
        "pi-rs {} (upstream parity target: @earendil-works/pi-coding-agent 1.5.2)",
        env!("CARGO_PKG_VERSION")
    );
    println!();
    println!("Usage:");
    println!(
        "  {} [options] [--] [@files...] [messages...]",
        crate::config::APP_NAME
    );
    println!();
    println!("Commands:");
    println!(
        "  {} install <source> [-l]     Install extension source and add to settings",
        crate::config::APP_NAME
    );
    println!(
        "  {} remove <source> [-l]      Remove extension source from settings",
        crate::config::APP_NAME
    );
    println!(
        "  {} uninstall <source> [-l]   Alias for remove",
        crate::config::APP_NAME
    );
    println!(
        "  {} update [source|self|pi]   Update pi, extensions, or model catalogs",
        crate::config::APP_NAME
    );
    println!(
        "  {} list                      List installed extensions from settings",
        crate::config::APP_NAME
    );
    println!(
        "  {} config [-l]               Open TUI to enable/disable package resources (Tab switches scope)",
        crate::config::APP_NAME
    );
    println!(
        "  {} auth <command>            Log in or out, print credentials, or check provider readiness",
        crate::config::APP_NAME
    );
    println!(
        "  {} mcp <command>             Manage MCP servers",
        crate::config::APP_NAME
    );
    println!(
        "  {} <command> --help          Show help for install/remove/uninstall/update/list/config/auth/mcp",
        crate::config::APP_NAME
    );
    println!();
    println!("Options:");
    println!("  --provider <name>              Provider to search for --model (requires --model)");
    println!(
        "  --model <pattern>              Model pattern or ID (supports \"provider/id\" and optional \":<thinking>\")"
    );
    println!("  --api-key <key>                API key (defaults to env vars)");
    println!("  --system-prompt <text>         System prompt (default: coding assistant prompt)");
    println!(
        "  --append-system-prompt <text>  Append text or file contents to the system prompt (can be used multiple times)"
    );
    println!("  --mode <mode>                  Output mode: text (default), json, or rpc");
    println!("  --print, -p                    Non-interactive mode: process prompt and exit");
    println!(
        "  --continue, -c                 Continue the most recent session for the current directory"
    );
    println!("  --resume, -r                   Pick a session to resume");
    println!("  --session <path|id>            Open a session by file path or full/partial ID");
    println!(
        "  --session-id <id>              Use the current directory's existing session with the ID, creating it otherwise"
    );
    println!(
        "  --fork <path|id>               Fork a session by file path or full/partial ID into the current directory"
    );
    println!("  --session-dir <dir>            Directory for session storage and lookup");
    println!("  --no-session                   Don't save session (ephemeral)");
    println!("  --name, -n <name>              Set session display name");
    println!("  --models <patterns>            Comma-separated model patterns for Ctrl+P cycling");
    println!(
        "                                 Supports globs (anthropic/*, *sonnet*) and fuzzy matching"
    );
    println!(
        "  --no-tools, -nt                Disable all tools by default (built-in and extension)"
    );
    println!(
        "  --no-builtin-tools, -nbt       Disable built-in tools by default but keep extension/custom tools enabled"
    );
    println!(
        "  --tools, -t <tools>            Comma-separated allowlist of tool names or patterns (*) to enable"
    );
    println!("                                 Keeps MCP tools unless an entry starts with mcp__");
    println!(
        "                                 Only +name/-name entries add to or remove from the defaults"
    );
    println!(
        "  --exclude-tools, -xt <tools>   Comma-separated denylist of tool names or patterns (*) to disable"
    );
    println!("                                 Applies to all tools, MCP tools included");
    println!(
        "  --thinking <level>             Set thinking level: off, minimal, low, medium, high, xhigh, max"
    );
    println!(
        "  --extension, -e <path>         Load an extension file or builtin:<name> (can be used multiple times)"
    );
    println!(
        "  --no-extensions, -ne           Disable extension discovery and built-in extensions (explicit -e paths still work)"
    );
    println!(
        "  --no-mcp                       Disable built-in MCP support: no servers connect and no MCP tools"
    );
    println!(
        "  --skill <path>                 Load a skill file or directory (can be used multiple times)"
    );
    println!("  --no-skills, -ns               Disable skills discovery and loading");
    println!(
        "  --prompt-template <path>       Load a prompt template file or directory (can be used multiple times)"
    );
    println!("  --no-prompt-templates, -np     Disable prompt template discovery and loading");
    println!(
        "  --theme <path>                 Load a theme file or directory (can be used multiple times)"
    );
    println!("  --use-theme <name[/name]>      Set the initial interactive theme for this run");
    println!("  --no-themes                    Disable theme discovery and loading");
    println!(
        "  --no-context-files, -nc        Disable AGENTS.md and CLAUDE.md discovery and loading"
    );
    println!("  --export <file> [output]       Export session file to HTML and exit");
    println!("  --list-models [search]         List available models (with optional fuzzy search)");
    println!(
        "  --verbose                      Force verbose startup (overrides quietStartup setting)"
    );
    println!("  --tui-mode <mode>              TUI mode: fullscreen (default) or regular");
    println!("  --approve, -a                  Trust project-local files for this run");
    println!("  --no-approve, -na              Ignore project-local files for this run");
    println!(
        "  --offline                      Disable startup network operations (same as PI_OFFLINE=1)"
    );
    println!(
        "  --                             End option parsing; treat remaining arguments as messages/files"
    );
    println!("  --help, -h                     Show this help");
    println!("  --version, -v                  Show version number");
    println!();
    println!("Extensions can register additional flags (e.g., --plan from plan-mode extension).");
    if !extension_flags.is_empty() {
        println!();
        println!("Extension CLI Flags:");
        for flag in extension_flags {
            let value = if flag.kind == "string" {
                " <value>"
            } else {
                ""
            };
            let description = if flag.description.is_empty() {
                format!("Registered by {}", flag.name)
            } else {
                flag.description.clone()
            };
            println!(
                "  {:<28} {}",
                format!("--{}{}", flag.name, value),
                description
            );
        }
    }
    println!();
    println!("Examples:");
    let example = |description: &str, command: String| {
        println!("  # {description}");
        println!("  {command}");
        println!();
    };
    let pi = crate::config::APP_NAME;
    example(
        "Print a provider API key for an external client",
        format!("{pi} auth print-api-key --provider openai"),
    );
    example(
        "Print an OAuth bearer token for an external client (refreshes if expired)",
        format!("{pi} auth print-bearer-token --provider openai-codex"),
    );
    example("Interactive mode", pi.to_string());
    example(
        "Interactive mode with initial prompt",
        format!("{pi} \"List all .ts files in src/\""),
    );
    example(
        "Include files in initial message",
        format!("{pi} @prompt.md @image.png \"What color is the sky?\""),
    );
    example(
        "Non-interactive mode (process and exit)",
        format!("{pi} -p \"List all .ts files in src/\""),
    );
    example(
        "Prompt beginning with a dash",
        format!("{pi} -p -- \"- Summarize these points\""),
    );
    example(
        "Multiple messages (interactive)",
        format!("{pi} \"Read package.json\" \"What dependencies do we have?\""),
    );
    example(
        "Continue previous session",
        format!("{pi} --continue \"What did we discuss?\""),
    );
    example(
        "Start a named session",
        format!("{pi} --name \"Refactor auth module\""),
    );
    example(
        "Use different model",
        format!("{pi} --provider openai --model gpt-4o-mini \"Help me refactor this code\""),
    );
    example(
        "Use model with provider prefix (no --provider needed)",
        format!("{pi} --model openai/gpt-4o \"Help me refactor this code\""),
    );
    example(
        "Use model with thinking level shorthand",
        format!("{pi} --model sonnet:high \"Solve this complex problem\""),
    );
    example(
        "Limit model cycling to specific models",
        format!("{pi} --models claude-sonnet,claude-haiku,gpt-4o"),
    );
    example(
        "Limit to a specific provider with glob pattern",
        format!("{pi} --models \"github-copilot/*\""),
    );
    example(
        "Cycle models with fixed thinking levels",
        format!("{pi} --models sonnet:high,haiku:low"),
    );
    example(
        "Start with a specific thinking level",
        format!("{pi} --thinking high \"Solve this complex problem\""),
    );
    example(
        "Read-only mode (no file modifications possible)",
        format!("{pi} --tools read,grep,find,ls -p \"Review the code in src/\""),
    );
    example(
        "Add codemode to the default tools",
        format!("{pi} --tools +codemode"),
    );
    example(
        "Codemode with only the tools of one MCP server",
        format!("{pi} --tools read,bash,codemode,'mcp__radius__*'"),
    );
    example(
        "Disable one tool while keeping the rest available",
        format!("{pi} --exclude-tools ask_question"),
    );
    example(
        "Export a session file to HTML",
        format!(
            "{pi} --export ~/{}/agent/sessions/--path--/session.jsonl",
            crate::config::CONFIG_DIR_NAME
        ),
    );
    println!("  {pi} --export session.jsonl output.html");
    println!();
    println!("Environment Variables:");
    for (name, description) in [
        ("ANTHROPIC_AUTH_TOKEN", "Anthropic bearer auth token"),
        ("ANTHROPIC_API_KEY", "Anthropic Claude API key"),
        (
            "ANTHROPIC_OAUTH_TOKEN",
            "Anthropic OAuth token (alternative to API key)",
        ),
        ("ANT_LING_API_KEY", "Ant Ling API key"),
        ("OPENAI_API_KEY", "OpenAI GPT API key"),
        ("AZURE_OPENAI_API_KEY", "Azure OpenAI API key"),
        (
            "AZURE_OPENAI_BASE_URL",
            "Azure OpenAI/Cognitive Services base URL",
        ),
        (
            "AZURE_OPENAI_RESOURCE_NAME",
            "Azure OpenAI resource name (alternative to base URL)",
        ),
        (
            "AZURE_OPENAI_API_VERSION",
            "Azure OpenAI API version (default: v1)",
        ),
        (
            "AZURE_OPENAI_DEPLOYMENT_NAME_MAP",
            "Azure OpenAI model=deployment map",
        ),
        ("DEEPSEEK_API_KEY", "DeepSeek API key"),
        ("NVIDIA_API_KEY", "NVIDIA NIM API key"),
        ("GEMINI_API_KEY", "Google Gemini API key"),
        ("GROQ_API_KEY", "Groq API key"),
        ("CEREBRAS_API_KEY", "Cerebras API key"),
        ("XAI_API_KEY", "xAI Grok API key"),
        ("FIREWORKS_API_KEY", "Fireworks API key"),
        ("TOGETHER_API_KEY", "Together AI API key"),
        ("BASETEN_API_KEY", "Baseten API key"),
        ("OPENROUTER_API_KEY", "OpenRouter API key"),
        ("AI_GATEWAY_API_KEY", "Vercel AI Gateway API key"),
        ("ZAI_API_KEY", "ZAI Coding Plan API key (Global)"),
        ("ZAI_CODING_CN_API_KEY", "ZAI Coding Plan API key (China)"),
        ("MISTRAL_API_KEY", "Mistral API key"),
        ("MINIMAX_API_KEY", "MiniMax API key"),
        ("MOONSHOT_API_KEY", "Moonshot AI API key"),
        ("OPENCODE_API_KEY", "OpenCode Zen/OpenCode Go API key"),
        ("KIMI_API_KEY", "Kimi For Coding API key"),
        ("META_API_KEY", "Meta Model API key"),
        (
            "CLOUDFLARE_API_KEY",
            "Cloudflare API token (Workers AI and AI Gateway)",
        ),
        (
            "CLOUDFLARE_ACCOUNT_ID",
            "Cloudflare account id (required for both)",
        ),
        (
            "CLOUDFLARE_GATEWAY_ID",
            "Cloudflare AI Gateway slug (required for AI Gateway)",
        ),
        (
            "QWEN_TOKEN_PLAN_API_KEY",
            "Qwen Token Plan API key (international)",
        ),
        (
            "QWEN_TOKEN_PLAN_CN_API_KEY",
            "Qwen Token Plan API key (China)",
        ),
        ("XIAOMI_API_KEY", "Xiaomi MiMo API key"),
        (
            "XIAOMI_TOKEN_PLAN_CN_API_KEY",
            "Xiaomi MiMo Token Plan API key (China)",
        ),
        (
            "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
            "Xiaomi MiMo Token Plan API key (Amsterdam)",
        ),
        (
            "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
            "Xiaomi MiMo Token Plan API key (Singapore)",
        ),
        ("AWS_PROFILE", "AWS profile for Amazon Bedrock"),
        ("AWS_ACCESS_KEY_ID", "AWS access key for Amazon Bedrock"),
        ("AWS_SECRET_ACCESS_KEY", "AWS secret key for Amazon Bedrock"),
        ("AWS_BEARER_TOKEN_BEDROCK", "Bedrock API key (bearer token)"),
        (
            "AWS_REGION",
            "AWS region for Amazon Bedrock (e.g., us-east-1)",
        ),
    ] {
        println!("  {name:<32} - {description}");
    }
    println!(
        "  {:<32} - Agent dir override: all state under one dir (default: XDG roots like ~/.config/{})",
        crate::config::ENV_AGENT_DIR,
        crate::config::CONFIG_DIR_NAME
    );
    println!(
        "  {:<32} - Session storage directory (overridden by --session-dir)",
        crate::config::ENV_SESSION_DIR
    );
    println!(
        "  {:<32} - Override package directory (for Nix/Guix store paths)",
        "PI_PACKAGE_DIR"
    );
    println!(
        "  {:<32} - Disable startup network operations when set to 1/true/yes",
        "PI_OFFLINE"
    );
    println!(
        "  {:<32} - Override install telemetry when set to 1/true/yes or 0/false/no",
        "PI_TELEMETRY"
    );
    println!(
        "  {:<32} - pi-rs OTLP endpoint for the opt-in telemetry exporter",
        "PI_RS_OTLP_ENDPOINT"
    );
    println!();
    println!("Built-in Tool Names:");
    println!("  read       - Read file contents");
    println!("  bash       - Execute bash commands");
    println!("  powershell - Execute PowerShell commands on Windows");
    println!("  edit       - Edit files with find/replace");
    println!("  write      - Write files (creates/overwrites)");
    println!("  grep       - Search file contents (read-only, off by default)");
    println!("  find       - Find files by glob pattern (read-only, off by default)");
    println!("  ls         - List directory contents (read-only, off by default)");
}

#[cfg(test)]
mod tests {
    // `assert!(v.is_empty())` stays: clippy's `assert_eq!(v, [])` alternative
    // is noisier for these port-fixture assertions.
    #![allow(clippy::assert_is_empty)]
    use super::*;

    fn parse(items: &[&str]) -> Args {
        let owned: Vec<String> = items.iter().map(|item| (*item).to_string()).collect();
        parse_args(&owned)
    }

    #[test]
    fn version_and_help_flags() {
        assert!(parse(&["--version"]).version);
        assert!(parse(&["-v"]).version);
        let both = parse(&["--version", "--help", "some message"]);
        assert!(both.version && both.help);
        assert_eq!(both.messages, vec!["some message"]);
        assert!(parse(&["--help"]).help);
        assert!(parse(&["-h"]).help);
    }

    #[test]
    fn print_flag_consumes_prompt() {
        assert!(parse(&["--print"]).print);
        assert!(parse(&["-p"]).print);
        // `args.test.ts:47-53`: a prompt starting with YAML frontmatter.
        let prompt = "---\ntitle: hello\n---\nSay hi.";
        let result = parse(&["-p", prompt]);
        assert!(result.print);
        assert_eq!(result.messages, vec![prompt]);
        assert!(result.unknown_flags.is_empty());
        // `args.test.ts:55-60`: options after -p are not prompts.
        let result = parse(&["-p", "--provider", "openai", "Say hi."]);
        assert!(result.print);
        assert_eq!(result.provider.as_deref(), Some("openai"));
        assert_eq!(result.messages, vec!["Say hi."]);
    }

    #[test]
    fn mode_flag_validation() {
        for (raw, expected) in [
            ("text", Mode::Text),
            ("json", Mode::Json),
            ("rpc", Mode::Rpc),
        ] {
            let result = parse(&["--mode", raw]);
            assert_eq!(result.mode, Some(expected));
            assert!(result.diagnostics.is_empty());
        }
        for mode in ["yaml", ""] {
            let result = parse(&["--mode", mode, "--version"]);
            assert_eq!(result.mode, None);
            assert!(result.version);
            assert!(result.messages.is_empty());
            assert!(result.unknown_flags.is_empty());
            assert_eq!(
                result.diagnostics,
                vec![Diagnostic {
                    kind: DiagnosticKind::Error,
                    message: format!("Invalid mode \"{mode}\". Valid values: text, json, rpc"),
                }]
            );
        }
        // `args.test.ts:175-188`: missing value and option-as-value.
        let result = parse(&["--mode"]);
        assert!(result.mode.is_none());
        assert_eq!(result.diagnostics.len(), 1);
        let result = parse(&["--mode", "--version"]);
        assert!(result.mode.is_none());
        assert!(result.version);
        assert_eq!(result.diagnostics.len(), 1);
    }

    #[test]
    fn name_flag() {
        assert_eq!(
            parse(&["--name", "my-session"]).name.as_deref(),
            Some("my-session")
        );
        assert_eq!(
            parse(&["-n", "quick-session"]).name.as_deref(),
            Some("quick-session")
        );
        // Empty values are preserved for main validation (`args.test.ts:209-212`).
        assert_eq!(parse(&["--name", ""]).name.as_deref(), Some(""));
        assert_eq!(
            normalize_session_name("  named session  ").as_deref(),
            Some("named session")
        );
        assert!(normalize_session_name("   ").is_none());
        let result = parse(&["--name"]);
        assert_eq!(
            result.diagnostics,
            vec![Diagnostic {
                kind: DiagnosticKind::Error,
                message: "--name requires a value".to_string(),
            }]
        );
    }

    #[test]
    fn tool_flag_validation() {
        assert!(parse(&["--no-tools"]).no_tools);
        assert!(parse(&["-nt"]).no_tools);
        assert!(parse(&["-nbt"]).no_builtin_tools);
        assert_eq!(
            parse(&["--tools", "read,bash"]).tools,
            Some(vec!["read".to_string(), "bash".to_string()])
        );
        assert_eq!(
            parse(&["-t", "read,bash"]).tools,
            Some(vec!["read".to_string(), "bash".to_string()])
        );
        // `args.test.ts:468-472`: modifiers parse fine.
        let result = parse(&["-t", "+codemode,-write"]);
        assert_eq!(
            result.tools,
            Some(vec!["+codemode".to_string(), "-write".to_string()])
        );
        assert!(result.diagnostics.is_empty());
        // `args.test.ts:474-480`: names mixed with modifiers are rejected.
        let result = parse(&["--tools", "read,+codemode"]);
        assert!(result.tools.is_none());
        assert_eq!(
            result.diagnostics,
            vec![Diagnostic {
                kind: DiagnosticKind::Error,
                message: "--tools: tool names cannot be mixed with +name or -name entries"
                    .to_string(),
            }]
        );
        // `args.test.ts:482-491`: patterns in modifiers are rejected.
        let result = parse(&["-t", "+mcp__radius__*"]);
        assert!(result.tools.is_none());
        assert!(result.diagnostics[0].message.contains(
            "+name and -name entries take exact tool names, not patterns: +mcp__radius__*"
        ));
        assert_eq!(
            parse(&["--exclude-tools", "read,bash"]).exclude_tools,
            Some(vec!["read".to_string(), "bash".to_string()])
        );
        assert_eq!(
            parse(&["-xt", "read,bash"]).exclude_tools,
            Some(vec!["read".to_string(), "bash".to_string()])
        );
    }

    #[test]
    fn thinking_flag() {
        let result = parse(&["--thinking", "high"]);
        assert_eq!(result.thinking.as_deref(), Some("high"));
        let result = parse(&["--thinking", "bogus"]);
        assert!(result.thinking.is_none());
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].kind, DiagnosticKind::Warning);
    }

    #[test]
    fn file_args_and_unknown_flags() {
        let result = parse(&["@README.md", "@src/main.ts"]);
        assert_eq!(result.file_args, vec!["README.md", "src/main.ts"]);
        let result = parse(&["@file.txt", "explain this", "@image.png"]);
        assert_eq!(result.file_args, vec!["file.txt", "image.png"]);
        assert_eq!(result.messages, vec!["explain this"]);
        // `args.test.ts:533-547`: unknown long flags captured.
        let result = parse(&["--unknown-flag", "message"]);
        assert!(result.messages.is_empty());
        assert_eq!(
            result.unknown_flags.get("unknown-flag"),
            Some(&Some("message".into()))
        );
        let result = parse(&["--unknown-flag"]);
        assert_eq!(result.unknown_flags.get("unknown-flag"), Some(&None));
        let result = parse(&["--unknown-flag=value"]);
        assert_eq!(
            result.unknown_flags.get("unknown-flag"),
            Some(&Some("value".into()))
        );
    }

    #[test]
    fn session_flags() {
        assert_eq!(
            parse(&["--session", "/path/to/session.jsonl"])
                .session
                .as_deref(),
            Some("/path/to/session.jsonl")
        );
        assert_eq!(
            parse(&["--session-id", "orchestrated-session"])
                .session_id
                .as_deref(),
            Some("orchestrated-session")
        );
        let result = parse(&["--fork", "1234abcd"]);
        assert_eq!(result.fork.as_deref(), Some("1234abcd"));
        assert!(result.messages.is_empty());
        assert!(parse(&["--no-session"]).no_session);
        assert!(parse(&["--continue"]).r#continue);
        assert!(parse(&["-c"]).r#continue);
        assert!(parse(&["--resume"]).resume);
        assert!(parse(&["-r"]).resume);
    }

    #[test]
    fn tui_mode_flag() {
        assert_eq!(
            parse(&["--tui-mode", "regular"]).tui_mode,
            Some(TuiMode::Regular)
        );
        assert_eq!(
            parse(&["--tui-mode", "fullscreen"]).tui_mode,
            Some(TuiMode::Fullscreen)
        );
        let result = parse(&["--tui-mode", "other"]);
        assert!(result.tui_mode.is_none());
        assert_eq!(
            result.diagnostics,
            vec![Diagnostic {
                kind: DiagnosticKind::Error,
                message: "Invalid TUI mode \"other\". Valid values: regular, fullscreen"
                    .to_string(),
            }]
        );
        assert_eq!(parse(&["--tui-mode"]).diagnostics.len(), 1);
        // `args.test.ts:430-434`: the old --ui-mode is an unknown flag.
        let result = parse(&["--ui-mode", "fullscreen"]);
        assert!(result.tui_mode.is_none());
        assert_eq!(
            result.unknown_flags.get("ui-mode"),
            Some(&Some("fullscreen".into()))
        );
    }

    #[test]
    fn trust_and_misc_flags() {
        assert_eq!(parse(&["--approve"]).project_trust_override, Some(true));
        assert_eq!(parse(&["-a"]).project_trust_override, Some(true));
        assert_eq!(parse(&["--no-approve"]).project_trust_override, Some(false));
        assert_eq!(parse(&["-na"]).project_trust_override, Some(false));
        assert!(parse(&["--verbose"]).verbose);
        assert!(parse(&["--offline"]).offline);
        assert!(parse(&["--no-mcp"]).no_mcp);
        assert!(parse(&["--no-context-files"]).no_context_files);
        assert!(parse(&["-nc"]).no_context_files);
        assert!(parse(&["--no-skills"]).no_skills);
        assert!(parse(&["-ns"]).no_skills);
        assert!(parse(&["--no-prompt-templates"]).no_prompt_templates);
        assert!(parse(&["-np"]).no_prompt_templates);
        assert!(parse(&["--no-themes"]).no_themes);
        assert_eq!(parse(&["--list-models"]).list_models, Some(None));
        assert_eq!(
            parse(&["--list-models", "sonnet"]).list_models,
            Some(Some("sonnet".into()))
        );
    }

    #[test]
    fn models_list_ignores_empty_entries() {
        // Issue #10334 (`args.test.ts:150-153`).
        let result = parse(&["--models", "gpt-4o, ,claude-sonnet,"]);
        assert_eq!(
            result.models,
            Some(vec!["gpt-4o".to_string(), "claude-sonnet".to_string()])
        );
    }

    #[test]
    fn complex_combination() {
        // `args.test.ts:551-569`.
        let result = parse(&[
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet",
            "--print",
            "--thinking",
            "high",
            "@prompt.md",
            "Do the task",
        ]);
        assert_eq!(result.provider.as_deref(), Some("anthropic"));
        assert_eq!(result.model.as_deref(), Some("claude-sonnet"));
        assert!(result.print);
        assert_eq!(result.thinking.as_deref(), Some("high"));
        assert_eq!(result.file_args, vec!["prompt.md"]);
        assert_eq!(result.messages, vec!["Do the task"]);
    }

    #[test]
    fn double_dash_ends_parsing() {
        // `args.ts:83-92`: everything after `--` is messages/files. This also
        // lets a prompt start with a dash (`pi -p -- "- Summarize"`).
        let result = parse(&["-p", "--", "- Summarize these points", "@notes.md"]);
        assert!(result.print);
        assert_eq!(result.messages, vec!["- Summarize these points"]);
        assert_eq!(result.file_args, vec!["notes.md"]);
    }
}
