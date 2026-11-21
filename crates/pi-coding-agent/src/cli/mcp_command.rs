//! The `pi mcp` command: `packages/coding-agent/src/extensions/mcp/cli.ts:1-677`,
//! loaded lazily by `cli.lazy.ts:1-2` and dispatched from `main.ts:665-667`
//! (`runMcpCommand(args.slice(1), { cwd, agentDir })`).
//!
//! The `mcp.json` helpers are ported from `extensions/mcp/config.ts:71-234`
//! (`readConfigFile`/`loadMcpConfig`/`addMcpServerConfig`/
//! `removeMcpServerConfig`/`editMcpServers`); the config validator mirrors
//! `core/mcp-servers.ts:213-227` as ported in
//! `crates/pi-ext-js/src/mcp_registry.rs:162-499`. They live here instead of
//! calling `pi-ext-js` because that crate is an optional feature and this
//! command must build under `--no-default-features` too.
//!
//! Everything that needs the coordinator's wiring — connections, OAuth
//! credentials, the browser, and the terminal prompt — sits behind the
//! [`McpRuntime`] / [`McpOutput`] traits, so this module compiles and tests
//! standalone. Upstream's `chalk` styling is dropped: these commands run
//! before any TUI exists, where chalk degrades to plain text.
//!
//! Deviations (all mechanical): `serde_json::Map` sorts object keys instead
//! of preserving insertion order (upstream's `JSON.stringify` preserves it),
//! JSON parse error wording comes from `serde_json` rather than V8, the
//! project config path uses [`PROJECT_CONFIG_DIR_NAME`] (the port keeps `.pi`
//! for project-local files while the user root moved to XDG), and the
//! non-interactive `waitForRedirectUrl` arm returns immediately instead of
//! waiting for the signal — the runtime's own timeout bounds the sign-in.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value as Json, json};
use serde_json::ser::{PrettyFormatter, Serializer};

use crate::cli::project_trust::ProjectTrustStore;
use crate::config::{APP_NAME, CONFIG_DIR_NAME, PROJECT_CONFIG_DIR_NAME};

const DEFAULT_LOGIN_TIMEOUT_SECONDS: f64 = 300.0;
/// `cli.ts:78` (`chalk.dim` is plain here).
const HELP_HINT: &str = "Use \"pi mcp --help\" for usage.";
/// `config.ts:71`.
const OVERRIDE_KEYS: [&str; 3] = ["enabled", "exposure", "toolExposure"];
const EXPOSURES: [&str; 4] = ["codemode", "deferred", "direct", "hidden"];
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// Where a configured server was defined (`config.ts:54-59`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpScope {
    Global,
    Project,
    Extension,
}

impl McpScope {
    fn as_str(self) -> &'static str {
        match self {
            McpScope::Global => "global",
            McpScope::Project => "project",
            McpScope::Extension => "extension",
        }
    }
}

/// `config.ts:48-60`.
#[derive(Debug, Clone)]
pub struct McpServerEntry {
    pub name: String,
    pub config: Json,
    pub source: String,
    pub scope: Option<McpScope>,
    /// Project `mcp.json` overriding `enabled`/`exposure`/`toolExposure`.
    pub override_path: Option<String>,
}

/// `config.ts:62-69`.
#[derive(Debug, Default)]
pub struct LoadedMcpConfig {
    pub servers: Vec<McpServerEntry>,
    pub auto_enable_codemode: Option<bool>,
    pub errors: Vec<String>,
    pub project_config: Option<PathBuf>,
}

/// `runtime.ts:141-456`, reduced to the fields the CLI reads.
#[derive(Debug, Clone, Default)]
pub struct ConnectionSnapshot {
    /// One of `disabled`, `connecting`, `connected`, `needs-auth`, `failed`,
    /// `disconnected`, `closed` (`runtime.ts:328-447`).
    pub state: String,
    pub tools: Vec<String>,
    pub has_resources: bool,
    pub error: Option<String>,
}

/// `McpSignInCancelledError` vs any other sign-in failure (`cli.ts:641-647`).
#[derive(Debug)]
pub enum SignInError {
    Cancelled,
    Failed(String),
}

/// The coordinator's wiring seam: connections, credentials, browser, and
/// the sign-in flow. `cli.ts` injects these through `McpCommandOptions`
/// (`credentials`, `providerToken`, `openUrl`).
pub trait McpRuntime {
    /// `connection.oauthUrl` (`runtime.ts:217`): an HTTP server with no
    /// `Authorization` header.
    fn oauth_url(&self, entry: &McpServerEntry, cwd: &Path, agent_dir: &Path) -> Option<String>;

    /// Opens a connection and lists tools (`getClient`, `runtime.ts:361-409`).
    fn connect(&self, entry: &McpServerEntry, cwd: &Path, agent_dir: &Path) -> ConnectionSnapshot;

    /// `connection.close()` (`runtime.ts:444-455`).
    fn close(&self, entry: &McpServerEntry, cwd: &Path, agent_dir: &Path);

    /// `credentials.remove(name, url)` (`cli.ts:299`).
    fn remove_credentials(&self, name: &str, url: &str) -> bool;

    /// `signInMcpServer` plus the reconnect (`cli.ts:627-656`); returns the
    /// tool count reported after reconnecting.
    fn sign_in(
        &self,
        name: &str,
        url: &str,
        timeout_ms: u64,
        show_authorization_url: &mut dyn FnMut(&str),
        prompt_redirect_url: &mut dyn FnMut() -> Option<String>,
    ) -> Result<usize, SignInError>;
}

/// `cli.ts:226-227`: `log`/`error` as a trait so options stay immutable and
/// the command needs no shared mutable borrows.
pub trait McpOutput {
    fn log(&self, line: &str);
    fn error(&self, line: &str);
}

/// Plain stdout/stderr implementation for the binary.
pub struct StdoutOutput;

impl McpOutput for StdoutOutput {
    fn log(&self, line: &str) {
        println!("{line}");
    }

    fn error(&self, line: &str) {
        eprintln!("{line}");
    }
}

/// `questionTerminal` redirect prompt (`cli.ts:82-94`).
type RedirectPrompt<'a> = &'a dyn Fn(&str) -> Option<String>;

/// `McpCommandOptions` (`cli.ts:82-94`), with the injected dependencies
/// collected behind [`McpRuntime`] and [`McpOutput`].
pub struct McpCommandOptions<'a> {
    pub cwd: &'a Path,
    pub agent_dir: &'a Path,
    pub runtime: &'a dyn McpRuntime,
    /// `openUrl` (`cli.ts:90`, 624): the platform browser by default.
    pub open_url: &'a dyn Fn(&str),
    /// Upstream's `process.stdin.isTTY === true && openUrl === undefined`.
    pub interactive: bool,
    /// Terminal pasted-URL prompt (`questionTerminal`); absent when the
    /// coordinator has no interactive stdin.
    pub prompt_redirect: Option<RedirectPrompt<'a>>,
    pub output: &'a dyn McpOutput,
}

/// `HELP` (`cli.ts:34-76`) with chalk styling dropped.
fn help_text() -> String {
    format!(
        r#"Usage:
  {APP_NAME} mcp add <server> [options] -- <command> [args...]
  {APP_NAME} mcp add <server> [options] --url <url>
  {APP_NAME} mcp remove <server> [-l]
  {APP_NAME} mcp list [--json]
  {APP_NAME} mcp login <server> [-l] [--timeout <seconds>]
  {APP_NAME} mcp logout <server> [-l]

Configure and check MCP servers and sign in to OAuth servers without starting a session.
Reads ~/{CONFIG_DIR_NAME}/agent/mcp.json and, in trusted projects, {PROJECT_CONFIG_DIR_NAME}/mcp.json.

Commands:
  add <server>            Add or replace a server in mcp.json
  remove <server>         Remove a server from mcp.json
  list                    Show state, tools, and errors (exits 1 on failure)
  login <server>          Sign in through the browser
  logout <server>         Delete the stored OAuth credentials

Options for add, remove, login, and logout:
  -l, --local             Use {PROJECT_CONFIG_DIR_NAME}/mcp.json in the current project instead of the global file

Options for add:
  --url <url>             Streamable HTTP server URL (instead of a command)
  --env <KEY=VALUE>       Environment variable for a stdio server (repeatable)
  --cwd <dir>             Working directory for a stdio server
  --header <KEY=VALUE>    HTTP header (repeatable)
  --bearer-token-env-var <NAME>
                          Send "Authorization: Bearer ${{NAME}}"
  --oauth-client-id <id>  Pre-registered OAuth client id
  --oauth-client-secret <secret>
                          OAuth client secret (may be ${{NAME}} or !command)
  --oauth-callback-port <port>
                          Fixed OAuth callback port
  --oauth-client-name <name>
                          Client name sent when registering with the OAuth server
  --oauth-client-registration <mode>
                          auto (default), cimd, or dcr
  --exposure <mode>       codemode (default), deferred, direct, or hidden
  --description <text>    What the server offers, shown in the system prompt

Other options:
  --json                  Print the list as JSON
  --timeout <seconds>     Time limit for the whole sign-in (default: 300)"#
    )
}

/// `cli.ts:169-176`: alias table and the parser result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptionKind {
    Flag,
    Value,
    List,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OptValue {
    Flag,
    Value(String),
}

#[derive(Debug, Default)]
struct ParsedOptions {
    positional: Vec<String>,
    values: BTreeMap<String, OptValue>,
    lists: BTreeMap<String, Vec<String>>,
}

impl ParsedOptions {
    /// `values.has(option) || lists.has(option)` (`cli.ts:400-401`).
    fn has(&self, option: &str) -> bool {
        self.values.contains_key(option) || self.lists.contains_key(option)
    }

    /// The string value of a non-list option (`value()` in `cli.ts:384-387`).
    fn value(&self, option: &str) -> Option<&str> {
        match self.values.get(option) {
            Some(OptValue::Value(value)) => Some(value),
            _ => None,
        }
    }
}

/// `parseOptions` (`cli.ts:183-222`), verbatim semantics: `--` and reaching
/// `max_positionals` end option parsing; `-l` aliases `--local`.
fn parse_options(
    args: &[String],
    known: &[(&str, OptionKind)],
    output: &dyn McpOutput,
    max_positionals: usize,
) -> Option<ParsedOptions> {
    let mut parsed = ParsedOptions::default();
    let mut index = 0usize;
    while index < args.len() {
        let typed = &args[index];
        let arg = if typed == "-l" { "--local" } else { typed.as_str() };
        if arg == "--" || parsed.positional.len() >= max_positionals {
            let from = if arg == "--" { index + 1 } else { index };
            parsed.positional.extend_from_slice(&args[from..]);
            break;
        }
        if !arg.starts_with("--") {
            parsed.positional.push(typed.clone());
            index += 1;
            continue;
        }
        let name = &arg[2..];
        let Some(kind) = known.iter().find(|(key, _)| *key == name).map(|(_, kind)| *kind) else {
            output.error(&format!("Unknown option {typed}.\n{HELP_HINT}"));
            return None;
        };
        if kind == OptionKind::Flag {
            parsed.values.insert(name.to_string(), OptValue::Flag);
            index += 1;
            continue;
        }
        let Some(value) = args.get(index + 1) else {
            output.error(&format!("{arg} needs a value."));
            return None;
        };
        index += 1;
        if kind == OptionKind::List {
            parsed
                .lists
                .entry(name.to_string())
                .or_default()
                .push(value.clone());
        } else {
            parsed
                .values
                .insert(name.to_string(), OptValue::Value(value.clone()));
        }
        index += 1;
    }
    Some(parsed)
}

/// `editDistance` (`cli.ts:321-331`): Levenshtein distance for suggestions.
fn edit_distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b_chars.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut current = vec![i + 1];
        for (j, cb) in b_chars.iter().enumerate() {
            current.push(
                (previous[j + 1] + 1)
                    .min(current[j] + 1)
                    .min(previous[j] + usize::from(ca != *cb)),
            );
        }
        previous = current;
    }
    previous[b_chars.len()]
}

/// `parsePairs` (`cli.ts:334-346`): repeatable `KEY=VALUE` options.
fn parse_pairs(
    option: &str,
    pairs: Option<&Vec<String>>,
    output: &dyn McpOutput,
) -> Option<BTreeMap<String, String>> {
    let mut record = BTreeMap::new();
    let Some(pairs) = pairs else {
        return Some(record);
    };
    for pair in pairs {
        let Some(separator) = pair.find('=') else {
            output.error(&format!("--{option} expects KEY=VALUE, got \"{pair}\"."));
            return None;
        };
        if separator == 0 {
            output.error(&format!("--{option} expects KEY=VALUE, got \"{pair}\"."));
            return None;
        }
        record.insert(pair[..separator].to_string(), pair[separator + 1..].to_string());
    }
    Some(record)
}

/// `describeTransport` (`cli.ts:120-123`).
fn describe_transport(config: &Json) -> String {
    if let Some(url) = config.get("url").and_then(Json::as_str) {
        return url.to_string();
    }
    let command = config.get("command").and_then(Json::as_str).unwrap_or_default();
    let args = config
        .get("args")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(Json::as_str);
    std::iter::once(command).chain(args).collect::<Vec<_>>().join(" ")
}

/// `getAuthProvider` (`cli.ts:125-127`).
fn auth_provider(config: &Json) -> Option<String> {
    config
        .get("auth")
        .and_then(Json::as_object)?
        .get("provider")
        .and_then(Json::as_str)
        .map(str::to_string)
}

/// `ServerReport` (`cli.ts:96-114`).
#[derive(Debug, Clone)]
struct ServerReport {
    name: String,
    scope: String,
    source: String,
    override_path: Option<String>,
    enabled: bool,
    exposure: String,
    transport: String,
    auth_provider: Option<String>,
    state: String,
    tools: Vec<String>,
    tool_exposure: BTreeMap<String, String>,
    resources: bool,
    error: Option<String>,
}

impl ServerReport {
    fn to_json(&self) -> Json {
        let mut fields = Map::new();
        fields.insert("name".to_string(), json!(self.name));
        fields.insert("scope".to_string(), json!(self.scope));
        fields.insert("source".to_string(), json!(self.source));
        if let Some(path) = &self.override_path {
            fields.insert("override".to_string(), json!(path));
        }
        fields.insert("enabled".to_string(), json!(self.enabled));
        fields.insert("exposure".to_string(), json!(self.exposure));
        fields.insert("transport".to_string(), json!(self.transport));
        if let Some(provider) = &self.auth_provider {
            fields.insert("authProvider".to_string(), json!(provider));
        }
        fields.insert("state".to_string(), json!(self.state));
        fields.insert("tools".to_string(), json!(self.tools));
        if !self.tool_exposure.is_empty() {
            fields.insert(
                "toolExposure".to_string(),
                json!(self.tool_exposure.iter().map(|(k, v)| (k.clone(), json!(v))).collect::<Map<String, Json>>()),
            );
        }
        if self.resources {
            fields.insert("resources".to_string(), json!(true));
        }
        if let Some(error) = &self.error {
            fields.insert("error".to_string(), json!(error));
        }
        Json::Object(fields)
    }
}

/// `getMcpToolExposure` (`core/mcp-servers.ts:170-178`, ported in
/// `pi-ext-js/src/mcp_registry.rs:335-356`). Exact names win; the first
/// matching `*`-pattern wins; absent keys default to `codemode`.
fn tool_exposure(config: &Json, tool: &str) -> String {
    if let Some(overrides) = config.get("toolExposure").and_then(Json::as_object) {
        if let Some(exposure) = overrides.get(tool).and_then(Json::as_str) {
            return exposure.to_string();
        }
        for (pattern, exposure) in overrides {
            if pattern.contains('*')
                && pattern_matches(pattern, tool)
                && let Some(exposure) = exposure.as_str()
            {
                return exposure.to_string();
            }
        }
    }
    config
        .get("exposure")
        .and_then(Json::as_str)
        .unwrap_or("codemode")
        .to_string()
}

fn pattern_matches(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rest = name;
    let last = parts.len() - 1;
    for (index, part) in parts.iter().enumerate() {
        if index == 0 {
            let Some(stripped) = rest.strip_prefix(part) else {
                return false;
            };
            rest = stripped;
        } else if index == last {
            return rest.ends_with(part);
        } else if let Some(position) = rest.find(part) {
            rest = &rest[position + part.len()..];
        } else {
            return false;
        }
    }
    true
}

/// `config.ts:120-130`: an override entry defines no transport of its own.
fn is_server_override(entry: &Json) -> bool {
    entry.get("command").is_none() && entry.get("url").is_none() && entry.get("type").is_none()
}

fn is_record(value: &Json) -> bool {
    value.is_object()
}

fn mcp_namespace(name: &str) -> String {
    format!("mcp__{}", name.replace('-', "_"))
}

fn strip_bom(text: &str) -> &str {
    text.trim_start_matches('\u{feff}')
}

/// `mcp-servers.ts:213-227` / `pi-mcp config.ts:110-139`, as ported in
/// `pi-ext-js/src/mcp_registry.rs:162-499`. Returns the alias-resolved copy
/// upstream stores, or the message `validateMcpServerConfig` returns.
#[allow(clippy::too_many_lines)] // mirrors the TS validator; split when extending
fn validate_mcp_server_config(name: &str, raw: &Json) -> Result<Json, String> {
    let invalid = |error: String| format!("server \"{name}\": {error}");
    if !server_name_is_valid(name) {
        return Err(format!(
            "invalid server name \"{name}\" (use letters, digits, \"_\" and \"-\")"
        ));
    }
    let entry = raw
        .as_object()
        .ok_or_else(|| format!("server \"{name}\" must be an object"))?;
    // `resolveExposureAliases` (`mcp-servers.ts:157-167`): the resolved copy
    // is what gets stored.
    let mut resolved = entry.clone();
    if resolved.get("exposure").and_then(Json::as_str) == Some("codemode-deferred") {
        resolved.insert("exposure".to_string(), json!("codemode"));
    }
    if let Some(Json::Object(tool_exposure)) = resolved.get_mut("toolExposure") {
        for value in tool_exposure.values_mut() {
            if value.as_str() == Some("codemode-deferred") {
                *value = json!("codemode");
            }
        }
    }
    if let Some(error) = validate_base_fields(&resolved) {
        return Err(invalid(error));
    }
    let url = resolved.get("url").and_then(Json::as_str);
    let command = resolved.get("command").and_then(Json::as_str);
    let transport_type = resolved.get("type").and_then(Json::as_str);
    if transport_type == Some("sse") {
        return Err(invalid(
            "legacy SSE transport is not supported; use the streamable HTTP URL".to_string(),
        ));
    }
    if let Some(url) = url
        && matches!(transport_type, None | Some("http" | "streamable-http"))
    {
        return validate_http_fields(&resolved, url)
            .and_then(|()| check_exposures(&resolved).map_or(Ok(()), Err))
            .and_then(|()| check_auth(&resolved, url).map_or(Ok(()), Err))
            .map(|()| Json::Object(resolved))
            .map_err(invalid);
    }
    if command.is_some() && matches!(transport_type, None | Some("stdio")) {
        return validate_stdio_fields(&resolved)
            .and_then(|()| check_exposures(&resolved).map_or(Ok(()), Err))
            .map(|()| Json::Object(resolved))
            .map_err(invalid);
    }
    Err(invalid(
        "needs either \"command\" (stdio) or \"url\" (streamable HTTP)".to_string(),
    ))
}

fn server_name_is_valid(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn validate_base_fields(entry: &Map<String, Json>) -> Option<String> {
    match entry.get("enabled") {
        None | Some(Json::Bool(_)) => {}
        Some(_) => return Some("enabled must be a boolean".to_string()),
    }
    match entry.get("description") {
        None | Some(Json::String(_)) => {}
        Some(_) => return Some("description must be a string".to_string()),
    }
    match entry.get("timeout") {
        None => {}
        Some(Json::Number(number)) if number.as_f64().is_some_and(|seconds| seconds > 0.0) => {}
        Some(_) => return Some("timeout must be a positive number of seconds".to_string()),
    }
    None
}

fn validate_http_fields(entry: &Map<String, Json>, url: &str) -> Result<(), String> {
    if !is_http_url(url) {
        return Err("url must be an http or https URL".to_string());
    }
    if let Some(headers) = entry.get("headers")
        && !is_string_record(headers)
    {
        return Err("headers must map names to strings".to_string());
    }
    validate_oauth(entry.get("oauth"))
}

fn is_http_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.is_empty()
}

fn validate_stdio_fields(entry: &Map<String, Json>) -> Result<(), String> {
    if let Some(args) = entry.get("args")
        && !args.as_array().is_some_and(|args| args.iter().all(Json::is_string))
    {
        return Err("args must be an array of strings".to_string());
    }
    if let Some(env) = entry.get("env")
        && !is_string_record(env)
    {
        return Err("env must map names to strings".to_string());
    }
    if let Some(cwd) = entry.get("cwd")
        && !cwd.is_string()
    {
        return Err("cwd must be a string".to_string());
    }
    Ok(())
}

fn is_string_record(value: &Json) -> bool {
    value
        .as_object()
        .is_some_and(|record| record.values().all(Json::is_string))
}

fn validate_oauth(value: Option<&Json>) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    let Some(oauth) = value.as_object() else {
        return Err("oauth must be an object".to_string());
    };
    if let Some(client_id) = oauth.get("clientId")
        && !client_id.is_string()
    {
        return Err("oauth.clientId must be a string".to_string());
    }
    if let Some(client_secret) = oauth.get("clientSecret")
        && !client_secret.is_string()
    {
        return Err("oauth.clientSecret must be a string".to_string());
    }
    if let Some(port) = oauth.get("callbackPort")
        && !port.as_i64().is_some_and(|port| (1..=65535).contains(&port))
    {
        return Err("oauth.callbackPort must be a port number".to_string());
    }
    Ok(())
}

fn check_exposures(entry: &Map<String, Json>) -> Option<String> {
    let choices = EXPOSURES
        .iter()
        .map(|value| format!("\"{value}\""))
        .collect::<Vec<_>>()
        .join(", ");
    if let Some(exposure) = entry.get("exposure")
        && !exposure.as_str().is_some_and(|text| EXPOSURES.contains(&text))
    {
        return Some(format!("exposure must be one of {choices}"));
    }
    if let Some(tool_exposure) = entry.get("toolExposure") {
        let Some(tool_exposure) = tool_exposure.as_object() else {
            return Some("toolExposure must map tool names to exposures".to_string());
        };
        for (tool, value) in tool_exposure {
            if !value.as_str().is_some_and(|text| EXPOSURES.contains(&text)) {
                return Some(format!("toolExposure \"{tool}\" must be one of {choices}"));
            }
        }
    }
    None
}

fn check_auth(entry: &Map<String, Json>, url: &str) -> Option<String> {
    let auth = entry.get("auth")?;
    let Some(auth) = auth.as_object() else {
        return Some("auth.provider must be a provider name".to_string());
    };
    let provider_ok = auth
        .get("provider")
        .and_then(Json::as_str)
        .is_some_and(|provider| !provider.is_empty());
    if !provider_ok {
        return Some("auth.provider must be a provider name".to_string());
    }
    if !url.starts_with("https://") && !LOOPBACK_HOSTS.contains(&url_host(url).as_str()) {
        return Some(
            "auth requires an https URL, or http on localhost, 127.0.0.1, or [::1]".to_string(),
        );
    }
    None
}

fn url_host(url: &str) -> String {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return String::new();
    };
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    if authority.starts_with('[') {
        return authority
            .find(']')
            .map_or_else(|| authority.to_string(), |end| authority[..=end].to_string());
    }
    authority.split(':').next().unwrap_or_default().to_string()
}

/// `config.ts:77-81`.
#[derive(Default)]
struct McpConfigState {
    servers: Vec<McpServerEntry>,
    auto_enable_codemode: Option<bool>,
    errors: Vec<String>,
}

/// `readConfigFile` (`config.ts:83-131`).
#[allow(clippy::too_many_lines)] // mirrors the TS reader; split when extending
fn read_config_file(path: &Path, scope: McpScope, state: &mut McpConfigState) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    let path_text = path.display().to_string();
    let parsed: Json = match serde_json::from_str(strip_bom(&raw)) {
        Ok(parsed) => parsed,
        Err(error) => {
            state.errors.push(format!("{path_text}: {error}"));
            return;
        }
    };
    let Some(root) = parsed.as_object() else {
        state
            .errors
            .push(format!("{path_text}: expected an object with an \"mcpServers\" object"));
        return;
    };
    if let Some(servers) = root.get("mcpServers")
        && !is_record(servers)
    {
        state
            .errors
            .push(format!("{path_text}: expected an object with an \"mcpServers\" object"));
        return;
    }
    match root.get("autoEnableCodemode") {
        Some(Json::Bool(value)) => state.auto_enable_codemode = Some(*value),
        Some(_) => state
            .errors
            .push(format!("{path_text}: autoEnableCodemode must be a boolean")),
        None => {}
    }
    let Some(servers) = root.get("mcpServers").and_then(Json::as_object) else {
        return;
    };
    for (name, value) in servers {
        if scope == McpScope::Project && is_record(value) && is_server_override(value) {
            let base = state
                .servers
                .iter()
                .find(|server| server.name.as_str() == name.as_str())
                .cloned();
            let extra: Vec<&String> = match value.as_object() {
                Some(object) => object
                    .keys()
                    .filter(|key| !OVERRIDE_KEYS.contains(&key.as_str()))
                    .collect(),
                None => Vec::new(),
            };
            let Some(base) = base else {
                state.errors.push(format!(
                    "{path_text}: server \"{name}\" needs \"command\" or \"url\", or a global server to override"
                ));
                continue;
            };
            if !extra.is_empty() {
                state.errors.push(format!(
                    "{path_text}: server \"{name}\": an override can only set {}",
                    OVERRIDE_KEYS.join(", ")
                ));
                continue;
            }
            let mut merged = base.config.as_object().cloned().unwrap_or_default();
            if let Some(value) = value.as_object() {
                for (key, field) in value {
                    merged.insert(key.clone(), field.clone());
                }
            }
            match validate_mcp_server_config(name, &Json::Object(merged)) {
                Ok(config) => {
                    state.servers.retain(|server| server.name.as_str() != name.as_str());
                    state.servers.push(McpServerEntry {
                        name: name.clone(),
                        config,
                        source: base.source.clone(),
                        scope: base.scope,
                        override_path: Some(path_text.clone()),
                    });
                }
                Err(error) => state.errors.push(format!("{path_text}: {error}")),
            }
            continue;
        }
        let config = match validate_mcp_server_config(name, value) {
            Ok(config) => config,
            Err(error) => {
                state.errors.push(format!("{path_text}: {error}"));
                continue;
            }
        };
        if let Some(clash) = state.servers.iter().find(|server| {
            server.name.as_str() != name.as_str()
                && mcp_namespace(&server.name) == mcp_namespace(name)
        }) {
            state.errors.push(format!(
                "{path_text}: server \"{name}\" conflicts with \"{}\"",
                clash.name
            ));
            continue;
        }
        if scope == McpScope::Project
            && config.get("url").is_some()
            && config.get("auth").is_some_and(|auth| !auth.is_null())
        {
            state.errors.push(format!(
                "{path_text}: server \"{name}\": auth is only allowed in the global mcp.json"
            ));
            continue;
        }
        state.servers.push(McpServerEntry {
            name: name.clone(),
            config,
            source: path_text.clone(),
            scope: Some(scope),
            override_path: None,
        });
    }
}

/// `loadMcpConfig` (`config.ts:137-148`).
fn load_mcp_config(agent_dir: &Path, cwd: &Path, project_trusted: bool) -> LoadedMcpConfig {
    let mut state = McpConfigState::default();
    read_config_file(&agent_dir.join("mcp.json"), McpScope::Global, &mut state);
    let project_config = project_trusted.then(|| cwd.join(PROJECT_CONFIG_DIR_NAME).join("mcp.json"));
    if let Some(project_config) = &project_config {
        read_config_file(project_config, McpScope::Project, &mut state);
    }
    LoadedMcpConfig {
        servers: state.servers,
        auto_enable_codemode: state.auto_enable_codemode,
        errors: state.errors,
        project_config,
    }
}

/// `editMcpServers` (`config.ts:220-234`): rewrite `mcpServers` with the
/// file's own indentation, preserving all other top-level content.
fn edit_mcp_servers<F>(path: &Path, edit: F) -> Result<(), String>
where
    F: FnOnce(&mut Option<Map<String, Json>>, &mut Map<String, Json>) -> bool,
{
    let error_text = path.display().to_string();
    let raw = if path.exists() {
        Some(std::fs::read_to_string(path).map_err(|error| format!("{error_text}: {error}"))?)
    } else {
        None
    };
    let text = raw.as_deref().map(strip_bom);
    let mut parsed: Json = match &text {
        Some(text) => serde_json::from_str(text).map_err(|error| format!("{error_text}: {error}"))?,
        None => Json::Object(Map::new()),
    };
    let should_write = {
        let root = parsed
            .as_object_mut()
            .ok_or_else(|| format!("{error_text}: expected an object with an \"mcpServers\" object"))?;
        if let Some(servers) = root.get("mcpServers")
            && !is_record(servers)
        {
            return Err(format!(
                "{error_text}: expected an object with an \"mcpServers\" object"
            ));
        }
        let mut servers = root
            .remove("mcpServers")
            .and_then(|value| value.as_object().cloned());
        let changed = edit(&mut servers, root);
        if changed {
            root.insert(
                "mcpServers".to_string(),
                Json::Object(servers.unwrap_or_default()),
            );
        }
        changed
    };
    if !should_write {
        return Ok(());
    }
    let indent = detect_indent(text);
    let mut bytes = Vec::new();
    let formatter = PrettyFormatter::with_indent(indent.as_bytes());
    let mut serializer = Serializer::with_formatter(&mut bytes, formatter);
    parsed
        .serialize(&mut serializer)
        .map_err(|error| error.to_string())?;
    let mut output = String::from_utf8(bytes).map_err(|error| error.to_string())?;
    output.push('\n');
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(path, output).map_err(|error| error.to_string())
}

/// The indentation of the first indented non-blank line (`config.ts:231`).
fn detect_indent(text: Option<&str>) -> String {
    if let Some(text) = text {
        for line in text.lines() {
            let leading: String = line
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect();
            if !leading.is_empty()
                && line[leading.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| !c.is_whitespace())
            {
                return leading;
            }
        }
    }
    "  ".to_string()
}

/// `addMcpServerConfig` (`config.ts:191-201`).
fn add_mcp_server_config(path: &Path, name: &str, config: &Json) -> Result<bool, String> {
    let mut replaced = false;
    edit_mcp_servers(path, |servers, _| {
        let target = servers.get_or_insert_with(Map::new);
        replaced = target.contains_key(name);
        target.insert(name.to_string(), config.clone());
        true
    })?;
    Ok(replaced)
}

/// `removeMcpServerConfig` (`config.ts:204-214`).
fn remove_mcp_server_config(path: &Path, name: &str) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let mut removed = false;
    edit_mcp_servers(path, |servers, _| {
        let Some(servers) = servers else {
            return false;
        };
        if servers.remove(name).is_some() {
            removed = true;
            true
        } else {
            false
        }
    })?;
    Ok(removed)
}

/// `McpServerConnection` creation (`cli.ts:129-144`) plus the snapshot the
/// `list`/`login` arms read from it.
fn connection_snapshot(
    entry: &McpServerEntry,
    options: &McpCommandOptions<'_>,
) -> ConnectionSnapshot {
    options.runtime.connect(entry, options.cwd, options.agent_dir)
}

/// Whether the project is trusted (`cli.ts:240`): upstream's
/// `get(cwd) === true`; a malformed store is treated as untrusted here.
fn project_trusted(agent_dir: &Path, cwd: &Path) -> bool {
    ProjectTrustStore::new(agent_dir).get(cwd) == Ok(Some(true))
}

/// Closes a connection on every return path (`cli.ts:310-312`'s `finally`).
struct CloseGuard<'a> {
    entry: &'a McpServerEntry,
    runtime: &'a dyn McpRuntime,
    cwd: &'a Path,
    agent_dir: &'a Path,
}

impl Drop for CloseGuard<'_> {
    fn drop(&mut self) {
        self.runtime.close(self.entry, self.cwd, self.agent_dir);
    }
}

/// `runMcpCommand` (`cli.ts:225-318`). `args` excludes the leading `mcp`
/// (upstream `main.ts:667` passes `args.slice(1)`); the return is the exit
/// code.
#[allow(clippy::too_many_lines)] // mirrors the TS dispatch; split when extending
pub fn run_mcp_command(args: &[String], options: &McpCommandOptions<'_>) -> i32 {
    let output = options.output;
    let Some((command, rest)) = args.split_first() else {
        output.log(&help_text());
        return 0;
    };
    if command == "help" || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        output.log(&help_text());
        return 0;
    }

    let project_config = options.cwd.join(PROJECT_CONFIG_DIR_NAME).join("mcp.json");
    if command == "add" || command == "remove" {
        return if command == "add" {
            add(rest, &project_config, options)
        } else {
            remove(rest, &project_config, options)
        };
    }

    let trusted = project_trusted(options.agent_dir, options.cwd);
    let loaded = load_mcp_config(options.agent_dir, options.cwd, trusted);
    let untrusted_note = (!trusted && project_config.exists()).then(|| {
        format!(
            "{} is ignored because the project is not trusted. Start {APP_NAME} in the project to trust it.",
            project_config.display()
        )
    });

    match command.as_str() {
        "list" => {
            let Some(parsed) = parse_options(rest, &[("json", OptionKind::Flag)], output, usize::MAX) else {
                return 1;
            };
            if !parsed.positional.is_empty() {
                output.error(&format!("Usage: {APP_NAME} mcp list [--json]\n{HELP_HINT}"));
                return 1;
            }
            list(
                &loaded,
                parsed.has("json"),
                untrusted_note.as_deref(),
                options,
            )
        }
        "login" | "logout" => {
            let known: &[(&str, OptionKind)] = if command == "login" {
                &[("local", OptionKind::Flag), ("timeout", OptionKind::Value)]
            } else {
                &[("local", OptionKind::Flag)]
            };
            let Some(parsed) = parse_options(rest, known, output, usize::MAX) else {
                return 1;
            };
            let name = parsed.positional.first();
            let extra = parsed.positional.get(1..).unwrap_or_default();
            let Some(name) = name else {
                usage_error(command, output);
                return 1;
            };
            if !extra.is_empty() {
                usage_error(command, output);
                return 1;
            }
            let Some(entry) = loaded
                .servers
                .iter()
                .find(|server| server.name.as_str() == name.as_str())
            else {
                let names: Vec<&str> = loaded.servers.iter().map(|server| server.name.as_str()).collect();
                let suggestion = names
                    .iter()
                    .copied()
                    .find(|candidate| edit_distance(candidate, name) <= 2);
                let mut message = format!("No MCP server named \"{name}\".");
                if let Some(suggestion) = suggestion {
                    let _ = write!(message, " Did you mean \"{suggestion}\"?");
                }
                if let Some(note) = &untrusted_note {
                    let _ = write!(message, " {note}");
                }
                // `names.join(", ") || "none"` (`cli.ts:277`).
                let configured = if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                };
                let _ = write!(message, " Configured: {configured}.");
                output.error(&message);
                return 1;
            };
            if parsed.has("local") && entry.scope != Some(McpScope::Project) {
                output.error(&format!(
                    "MCP server \"{name}\" is defined in {}, not in the project; omit --local.",
                    entry.source
                ));
                return 1;
            }
            if let Some(provider) = auth_provider(&entry.config) {
                output.error(&format!(
                    "MCP server \"{name}\" uses the credentials of provider \"{provider}\". Run \"{APP_NAME} auth {command} {provider}\"."
                ));
                return 1;
            }
            let Some(url) = options.runtime.oauth_url(entry, options.cwd, options.agent_dir) else {
                output.error(&format!(
                    "MCP server \"{name}\" does not use OAuth. Only HTTP servers without an Authorization header do."
                ));
                return 1;
            };
            if command == "logout" {
                let removed = options.runtime.remove_credentials(name, &url);
                let message = if removed {
                    format!("Signed out of MCP server \"{name}\".")
                } else {
                    format!("No stored credentials for MCP server \"{name}\".")
                };
                output.log(&message);
                return 0;
            }
            let timeout = match parsed.value("timeout") {
                Some(value) => value.parse::<f64>().unwrap_or(f64::NAN),
                None => DEFAULT_LOGIN_TIMEOUT_SECONDS,
            };
            if !timeout.is_finite() || timeout <= 0.0 {
                output.error("--timeout must be a positive number of seconds.");
                return 1;
            }
            login(entry, &url, timeout * 1000.0, options)
        }
        other => {
            output.error(&format!("Unknown mcp command \"{other}\".\n{HELP_HINT}"));
            1
        }
    }
}

fn usage_error(command: &str, output: &dyn McpOutput) {
    output.error(&format!(
        "Usage: {APP_NAME} mcp {command} <server> [-l]\n{HELP_HINT}"
    ));
}

/// `add` (`cli.ts:348-470`).
#[allow(clippy::too_many_lines)] // mirrors the TS add arm; split when extending
fn add(args: &[String], project_config: &Path, options: &McpCommandOptions<'_>) -> i32 {
    let output = options.output;
    let usage = format!(
        "Usage: {APP_NAME} mcp add <server> [options] (--url <url> | -- <command> [args...])\n{HELP_HINT}"
    );
    let known: &[(&str, OptionKind)] = &[
        ("local", OptionKind::Flag),
        ("url", OptionKind::Value),
        ("env", OptionKind::List),
        ("cwd", OptionKind::Value),
        ("header", OptionKind::List),
        ("bearer-token-env-var", OptionKind::Value),
        ("oauth-client-id", OptionKind::Value),
        ("oauth-client-secret", OptionKind::Value),
        ("oauth-callback-port", OptionKind::Value),
        ("oauth-client-name", OptionKind::Value),
        ("oauth-client-registration", OptionKind::Value),
        ("exposure", OptionKind::Value),
        ("description", OptionKind::Value),
    ];
    let Some(parsed) = parse_options(args, known, output, 2) else {
        return 1;
    };
    let name = parsed.positional.first();
    let command = parsed.positional.get(1..).unwrap_or_default();
    let url = parsed.value("url");
    let url_absent = url.is_none();
    let command_absent = command.is_empty();
    if name.is_none() || url_absent == command_absent {
        output.error(&usage);
        return 1;
    }
    let name = name.expect("checked above");
    let http_only = [
        "header",
        "bearer-token-env-var",
        "oauth-client-id",
        "oauth-client-secret",
        "oauth-callback-port",
        "oauth-client-name",
        "oauth-client-registration",
    ];
    let stdio_only = ["env", "cwd"];
    let candidates: &[&str] = if url_absent { &http_only } else { &stdio_only };
    if let Some(misplaced) = candidates.iter().find(|option| parsed.has(option)) {
        output.error(&format!(
            "--{misplaced} only applies to {}.",
            if url_absent { "HTTP servers (--url)" } else { "stdio servers" }
        ));
        return 1;
    }

    let mut config = Map::new();
    if let Some(url) = url {
        let Some(mut headers) = parse_pairs("header", parsed.lists.get("header"), output) else {
            return 1;
        };
        if let Some(bearer) = parsed.value("bearer-token-env-var") {
            headers.insert(
                "Authorization".to_string(),
                format!("Bearer ${{{bearer}}}"),
            );
        }
        let mut oauth = Map::new();
        for (option, field) in [
            ("oauth-client-id", "clientId"),
            ("oauth-client-secret", "clientSecret"),
            ("oauth-client-name", "clientName"),
            ("oauth-client-registration", "clientRegistration"),
        ] {
            if let Some(value) = parsed.value(option) {
                oauth.insert(field.to_string(), json!(value));
            }
        }
        if let Some(port) = parsed.value("oauth-callback-port") {
            let number = port
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map_or(Json::Null, Json::Number);
            oauth.insert("callbackPort".to_string(), number);
        }
        config.insert("url".to_string(), json!(url));
        if !headers.is_empty() {
            config.insert(
                "headers".to_string(),
                json!(headers.into_iter().map(|(k, v)| (k, json!(v))).collect::<Map<String, Json>>()),
            );
        }
        if !oauth.is_empty() {
            config.insert("oauth".to_string(), Json::Object(oauth));
        }
    } else {
        let Some(env) = parse_pairs("env", parsed.lists.get("env"), output) else {
            return 1;
        };
        let (executable, command_args) = command.split_first().expect("checked above");
        config.insert("command".to_string(), json!(executable));
        if !command_args.is_empty() {
            config.insert(
                "args".to_string(),
                json!(command_args.iter().map(|arg| json!(arg)).collect::<Vec<Json>>()),
            );
        }
        if !env.is_empty() {
            config.insert(
                "env".to_string(),
                json!(env.into_iter().map(|(k, v)| (k, json!(v))).collect::<Map<String, Json>>()),
            );
        }
        if let Some(cwd) = parsed.value("cwd") {
            config.insert("cwd".to_string(), json!(cwd));
        }
    }
    if let Some(exposure) = parsed.value("exposure") {
        config.insert("exposure".to_string(), json!(exposure));
    }
    if let Some(description) = parsed.value("description") {
        config.insert("description".to_string(), json!(description));
    }
    let validated = match validate_mcp_server_config(name, &Json::Object(config)) {
        Ok(config) => config,
        Err(error) => {
            output.error(&error);
            return 1;
        }
    };
    let project = parsed.has("local");
    let path = if project {
        project_config.to_path_buf()
    } else {
        options.agent_dir.join("mcp.json")
    };
    let scope = if project { "project" } else { "global" };
    let replaced = match add_mcp_server_config(&path, name, &validated) {
        Ok(replaced) => replaced,
        Err(error) => {
            output.error(&format!("Could not update {}: {error}", path.display()));
            return 1;
        }
    };
    output.log(&format!(
        "{} {scope} MCP server \"{name}\" in {}.",
        if replaced { "Replaced" } else { "Added" },
        path.display()
    ));
    if project && !project_trusted(options.agent_dir, options.cwd) {
        output.log(&format!(
            "The project is not trusted, so {} is ignored until you start {APP_NAME} in the project and trust it.",
            path.display()
        ));
    }
    let may_need_sign_in = validated.get("url").is_some()
        && !validated
            .get("headers")
            .and_then(Json::as_object)
            .is_some_and(|headers| headers.keys().any(|key| key.eq_ignore_ascii_case("authorization")));
    let hint = if may_need_sign_in {
        format!(". If it requires sign-in: {APP_NAME} mcp login {name}")
    } else {
        String::new()
    };
    output.log(&format!("Check it with: {APP_NAME} mcp list{hint}"));
    0
}

/// `remove` (`cli.ts:472-508`).
fn remove(args: &[String], project_config: &Path, options: &McpCommandOptions<'_>) -> i32 {
    let output = options.output;
    let Some(parsed) = parse_options(args, &[("local", OptionKind::Flag)], output, usize::MAX) else {
        return 1;
    };
    let name = parsed.positional.first();
    let extra = parsed.positional.get(1..).unwrap_or_default();
    let (Some(name), true) = (name, extra.is_empty()) else {
        output.error(&format!(
            "Usage: {APP_NAME} mcp remove <server> [-l]\n{HELP_HINT}"
        ));
        return 1;
    };
    let project = parsed.has("local");
    let path = if project {
        project_config.to_path_buf()
    } else {
        options.agent_dir.join("mcp.json")
    };
    let scope = if project { "project" } else { "global" };
    let removed = match remove_mcp_server_config(&path, name) {
        Ok(removed) => removed,
        Err(error) => {
            output.error(&format!("Could not update {}: {error}", path.display()));
            return 1;
        }
    };
    if removed {
        output.log(&format!(
            "Removed {scope} MCP server \"{name}\" from {}.",
            path.display()
        ));
        return 0;
    }
    let other = load_mcp_config(options.agent_dir, options.cwd, true)
        .servers
        .into_iter()
        .find(|server| server.name.as_str() == name && server.scope.map(McpScope::as_str) != Some(scope));
    let mut message = format!("No {scope} MCP server named \"{name}\" in {}.", path.display());
    if let Some(other) = other {
        let _ = write!(
            message,
            " It is defined in {}{}.",
            other.source,
            if other.scope == Some(McpScope::Project) {
                "; use --local"
            } else {
                "; omit --local"
            }
        );
    }
    output.error(&message);
    1
}

/// `list` (`cli.ts:510-599`), with the report-building map of `:519-553`
/// folded into [`ServerReport::from_entry`].
fn list(
    loaded: &LoadedMcpConfig,
    json: bool,
    note: Option<&str>,
    options: &McpCommandOptions<'_>,
) -> i32 {
    let output = options.output;
    let reports: Vec<ServerReport> = loaded
        .servers
        .iter()
        .map(|entry| ServerReport::from_entry(entry, options))
        .collect();
    let failed = !loaded.errors.is_empty()
        || reports.iter().any(|report| report.enabled && report.state != "connected");

    if json {
        let mut fields = Map::new();
        fields.insert(
            "servers".to_string(),
            json!(reports.iter().map(ServerReport::to_json).collect::<Vec<Json>>()),
        );
        fields.insert("errors".to_string(), json!(loaded.errors));
        if let Some(note) = note {
            fields.insert("note".to_string(), json!(note));
        }
        output.log(&serde_json::to_string_pretty(&Json::Object(fields)).unwrap_or_default());
        return i32::from(failed);
    }
    if reports.is_empty() && loaded.errors.is_empty() {
        output.log(&format!(
            "No MCP servers configured. Add them to {} or {PROJECT_CONFIG_DIR_NAME}/mcp.json.",
            options.agent_dir.join("mcp.json").display()
        ));
    }
    for report in &reports {
        let state = match report.state.as_str() {
            "connected" => format!(
                "connected, {} tool{}",
                report.tools.len(),
                if report.tools.len() == 1 { "" } else { "s" }
            ),
            "needs-auth" => "needs sign-in".to_string(),
            other => other.to_string(),
        };
        output.log(&format!(
            "{}: {} ({}, {})",
            report.name, state, report.exposure, report.scope
        ));
        output.log(&format!("  {}", report.transport));
        if let Some(path) = &report.override_path {
            output.log(&format!("  project override: {path}"));
        }
        if report.state == "needs-auth" {
            let sign_in = match &report.auth_provider {
                Some(provider) => format!("  sign in with: /login {provider} in {APP_NAME}"),
                None => format!("  sign in with: {APP_NAME} mcp login {}", report.name),
            };
            output.log(&sign_in);
        }
        if !report.tools.is_empty() {
            let tools = report
                .tools
                .iter()
                .map(|tool| match report.tool_exposure.get(tool) {
                    Some(exposure) => format!("{tool} [{exposure}]"),
                    None => tool.clone(),
                })
                .collect::<Vec<_>>()
                .join(", ");
            output.log(&format!("  tools: {tools}"));
        }
        if report.resources {
            output.log("  offers resources");
        }
        if let Some(error) = &report.error {
            output.log(&format!("  {}", error.split('\n').collect::<Vec<_>>().join("\n  ")));
        }
    }
    for error in &loaded.errors {
        output.log(&format!("config error: {error}"));
    }
    if let Some(note) = note {
        output.log(note);
    }
    i32::from(failed)
}

impl ServerReport {
    /// `cli.ts:520-553`: disabled servers short-circuit with state `disabled`.
    fn from_entry(entry: &McpServerEntry, options: &McpCommandOptions<'_>) -> Self {
        let mut report = Self {
            name: entry.name.clone(),
            scope: entry
                .scope
                .map_or_else(|| "global".to_string(), |scope| scope.as_str().to_string()),
            source: entry.source.clone(),
            override_path: entry.override_path.clone(),
            enabled: entry.config.get("enabled") != Some(&Json::Bool(false)),
            exposure: entry
                .config
                .get("exposure")
                .and_then(Json::as_str)
                .unwrap_or("codemode")
                .to_string(),
            transport: describe_transport(&entry.config),
            auth_provider: auth_provider(&entry.config),
            state: "disabled".to_string(),
            tools: Vec::new(),
            tool_exposure: BTreeMap::new(),
            resources: false,
            error: None,
        };
        if !report.enabled {
            return report;
        }
        let snapshot = connection_snapshot(entry, options);
        report.state.clone_from(&snapshot.state);
        report.tools.clone_from(&snapshot.tools);
        for tool in &snapshot.tools {
            let exposure = tool_exposure(&entry.config, tool);
            if exposure != report.exposure {
                report.tool_exposure.insert(tool.clone(), exposure);
            }
        }
        report.resources = snapshot.has_resources;
        if report.state != "connected" {
            report.error = snapshot.error;
        }
        report
    }
}

/// `login` (`cli.ts:601-658`) including the `finally { connection.close() }`
/// from `cli.ts:310-312`.
fn login(
    entry: &McpServerEntry,
    url: &str,
    timeout_ms: f64,
    options: &McpCommandOptions<'_>,
) -> i32 {
    let output = options.output;
    let name = &entry.name;
    let guard = CloseGuard {
        entry,
        runtime: options.runtime,
        cwd: options.cwd,
        agent_dir: options.agent_dir,
    };
    let snapshot = connection_snapshot(entry, options);
    if snapshot.state == "connected" {
        output.log(&format!(
            "Already signed in to MCP server \"{name}\" ({} tools).",
            snapshot.tools.len()
        ));
        return 0;
    }
    if snapshot.state != "needs-auth" {
        output.error(&format!(
            "MCP server \"{name}\" failed to connect: {}",
            snapshot.error.as_deref().unwrap_or("unknown error")
        ));
        return 1;
    }

    let mut show_url = |authorization_url: &str| {
        output.log(&format!(
            "Sign in to MCP server \"{name}\" in your browser:\n{authorization_url}"
        ));
        (options.open_url)(authorization_url);
    };
    let mut redirect = || {
        let prompt = "If the browser cannot reach this machine, paste the URL it was redirected to: ";
        if options.interactive {
            options.prompt_redirect.and_then(|answer| answer(prompt))
        } else {
            None
        }
    };
    let result = options.runtime.sign_in(
        name,
        url,
        timeout_ms as u64,
        &mut show_url,
        &mut redirect,
    );
    if let Err(error) = result {
        match error {
            SignInError::Cancelled => output.error(&format!(
                "Sign-in to MCP server \"{name}\" was cancelled or not completed within {} seconds.",
                (timeout_ms / 1000.0).round() as u64
            )),
            SignInError::Failed(message) => output.error(&format!(
                "Sign-in to MCP server \"{name}\" failed: {message}"
            )),
        }
        return 1;
    }
    let snapshot = connection_snapshot(entry, options);
    if snapshot.state != "connected" {
        output.error(&format!(
            "Signed in, but {}",
            snapshot.error.as_deref().unwrap_or("unknown error")
        ));
        return 1;
    }
    output.log(&format!(
        "Signed in to MCP server \"{name}\" ({} tools).",
        snapshot.tools.len()
    ));
    // `drop(guard)` closes the connection before returning, like `finally`.
    drop(guard);
    0
}

#[cfg(test)]
mod tests {
    #![allow(clippy::assert_is_empty)]

    use std::cell::Cell;

    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_string()).collect()
    }

    struct Capture {
        lines: std::cell::RefCell<Vec<String>>,
        errors: std::cell::RefCell<Vec<String>>,
    }

    impl Capture {
        fn new() -> Self {
            Self {
                lines: std::cell::RefCell::new(Vec::new()),
                errors: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    impl McpOutput for Capture {
        fn log(&self, line: &str) {
            self.lines.borrow_mut().push(line.to_string());
        }

        fn error(&self, line: &str) {
            self.errors.borrow_mut().push(line.to_string());
        }
    }

    struct FakeRuntime {
        snapshots: BTreeMap<String, ConnectionSnapshot>,
        oauth_urls: BTreeMap<String, Option<String>>,
        removed: Cell<bool>,
        sign_in_tools: usize,
        sign_in_error: Option<String>,
        sign_in_cancelled: bool,
    }

    impl FakeRuntime {
        fn new() -> Self {
            Self {
                snapshots: BTreeMap::new(),
                oauth_urls: BTreeMap::new(),
                removed: Cell::new(false),
                sign_in_tools: 0,
                sign_in_error: None,
                sign_in_cancelled: false,
            }
        }

        fn with(mut self, name: &str, state: &str, tools: &[&str]) -> Self {
            self.snapshots.insert(
                name.to_string(),
                ConnectionSnapshot {
                    state: state.to_string(),
                    tools: tools.iter().map(|tool| (*tool).to_string()).collect(),
                    ..ConnectionSnapshot::default()
                },
            );
            self.oauth_urls.insert(name.to_string(), None);
            self
        }
    }

    impl McpRuntime for FakeRuntime {
        fn oauth_url(&self, entry: &McpServerEntry, _cwd: &Path, _agent_dir: &Path) -> Option<String> {
            self.oauth_urls
                .get(&entry.name)
                .cloned()
                .flatten()
        }

        fn connect(&self, entry: &McpServerEntry, _cwd: &Path, _agent_dir: &Path) -> ConnectionSnapshot {
            self.snapshots
                .get(&entry.name)
                .cloned()
                .unwrap_or_default()
        }

        fn close(&self, _entry: &McpServerEntry, _cwd: &Path, _agent_dir: &Path) {}

        fn remove_credentials(&self, _name: &str, _url: &str) -> bool {
            self.removed.replace(true);
            true
        }

        fn sign_in(
            &self,
            _name: &str,
            _url: &str,
            _timeout_ms: u64,
            show_authorization_url: &mut dyn FnMut(&str),
            _prompt_redirect_url: &mut dyn FnMut() -> Option<String>,
        ) -> Result<usize, SignInError> {
            show_authorization_url("https://example.com/authorize?state=x");
            if self.sign_in_cancelled {
                return Err(SignInError::Cancelled);
            }
            if let Some(error) = &self.sign_in_error {
                return Err(SignInError::Failed(error.clone()));
            }
            Ok(self.sign_in_tools)
        }
    }

    fn no_open(_url: &str) {}

    fn make_options<'a>(
        cwd: &'a Path,
        agent_dir: &'a Path,
        runtime: &'a FakeRuntime,
        capture: &'a Capture,
    ) -> McpCommandOptions<'a> {
        McpCommandOptions {
            cwd,
            agent_dir,
            runtime,
            open_url: &no_open,
            interactive: false,
            prompt_redirect: None,
            output: capture,
        }
    }

    fn config_file(dir: &Path, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("mkdir");
        let path = dir.join("mcp.json");
        std::fs::write(&path, body).expect("write");
        path
    }

    #[test]
    fn help_text_is_verbatim() {
        let help = help_text();
        assert!(help.starts_with("Usage:\n  pi mcp add <server>"), "{help}");
        assert!(help.contains("Reads ~/pi-rs/agent/mcp.json and, in trusted projects, .pi/mcp.json."));
        assert!(help.contains("Send \"Authorization: Bearer ${NAME}\""));
        assert!(help.contains("--exposure <mode>       codemode (default), deferred, direct, or hidden"));
        assert!(help.ends_with("--timeout <seconds>     Time limit for the whole sign-in (default: 300)"));
        assert_eq!(HELP_HINT, "Use \"pi mcp --help\" for usage.");
        // Help wins for the `help` word and for `-h`/`--help` anywhere.
        let dir = tempfile::tempdir().expect("tempdir");
        let capture = Capture::new();
        let runtime = FakeRuntime::new();
        let options = make_options(dir.path(), dir.path(), &runtime, &capture);
        for argv in [args(&["help"]), args(&["--help"]), args(&["list", "-h"])] {
            assert_eq!(run_mcp_command(&argv, &options), 0);
            assert_eq!(capture.lines.borrow().last().map(String::as_str), Some(help.as_str()));
        }
        assert_eq!(run_mcp_command(&[], &options), 0);
    }

    #[test]
    fn parse_options_handles_aliases_flags_lists_and_terminator() {
        let capture = Capture::new();
        let known = [
            ("local", OptionKind::Flag),
            ("url", OptionKind::Value),
            ("env", OptionKind::List),
        ];
        let parsed = parse_options(
            &args(&["-l", "--env", "A=1", "--env", "B=2", "--", "--url", "kept"]),
            &known,
            &capture,
            usize::MAX,
        )
        .expect("parse");
        assert!(parsed.has("local"));
        assert_eq!(parsed.value("url"), None);
        let expected_env = args(&["A=1", "B=2"]);
        assert_eq!(
            parsed.lists.get("env").map(Vec::as_slice),
            Some(expected_env.as_slice())
        );
        assert_eq!(parsed.positional, args(&["--url", "kept"]));

        // Reaching maxPositionals passes a command's own flags through.
        let parsed = parse_options(&args(&["srv", "run", "--flag"]), &known, &capture, 2).expect("parse");
        assert_eq!(parsed.positional, args(&["srv", "run", "--flag"]));

        let parsed = parse_options(&args(&["--url", "http://x"]), &known, &capture, usize::MAX).expect("parse");
        assert_eq!(parsed.value("url"), Some("http://x"));

        assert!(parse_options(&args(&["--bogus"]), &known, &capture, usize::MAX).is_none());
        assert_eq!(
            capture.errors.borrow().first().map(String::as_str),
            Some("Unknown option --bogus.\nUse \"pi mcp --help\" for usage.")
        );

        let parsed = parse_options(&args(&["--url"]), &known, &capture, usize::MAX);
        assert!(parsed.is_none());
        assert!(
            capture
                .errors
                .borrow()
                .iter()
                .any(|line| line.as_str() == "--url needs a value.")
        );
    }

    #[test]
    fn edit_distance_and_pair_parsing() {
        assert_eq!(edit_distance("filesystem", "filesyste"), 1);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("a", "xyz"), 3);
        let capture = Capture::new();
        let pairs = parse_pairs(
            "env",
            Some(&args(&["A=1", "EMPTY=", "X=a=b"])),
            &capture,
        )
        .expect("pairs");
        assert_eq!(pairs.get("A").map(String::as_str), Some("1"));
        assert_eq!(pairs.get("EMPTY").map(String::as_str), Some(""));
        assert_eq!(pairs.get("X").map(String::as_str), Some("a=b"));
        assert!(parse_pairs("header", Some(&args(&["NOEQUALS"])), &capture).is_none());
        assert_eq!(
            capture.errors.borrow().last().map(String::as_str),
            Some("--header expects KEY=VALUE, got \"NOEQUALS\".")
        );
        // `=value` is invalid: upstream requires `separator > 0`.
        assert!(parse_pairs("header", Some(&args(&["=value"])), &capture).is_none());
    }

    #[test]
    fn config_roundtrip_add_remove_and_override() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent = dir.path().join("agent");
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&agent).expect("mkdir");
        let global = agent.join("mcp.json");
        std::fs::write(
            &global,
            "{\n  \"mcpServers\": {\n    \"fs\": {\"command\": \"npx\"}\n  },\n  \"other\": 1\n}\n",
        )
        .expect("write");

        let loaded = load_mcp_config(&agent, &project, false);
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(loaded.servers[0].name, "fs");
        assert_eq!(loaded.servers[0].scope, Some(McpScope::Global));
        assert_eq!(describe_transport(&loaded.servers[0].config), "npx");
        assert_eq!(loaded.errors.len(), 0);
        assert!(loaded.project_config.is_none());

        // Add replaces and reports `replaced`, preserving indentation and
        // unrelated top-level keys.
        let added = add_mcp_server_config(&global, "fs", &json!({"command": "node"})).expect("add");
        assert!(added);
        assert!(!add_mcp_server_config(&global, "web", &json!({"url": "https://e.com/mcp"})).expect("add"));
        let raw = std::fs::read_to_string(&global).expect("read");
        assert!(raw.contains("\"other\": 1"), "{raw}");
        assert!(raw.contains("\"command\": \"node\""), "{raw}");
        assert_eq!(load_mcp_config(&agent, &project, false).servers.len(), 2);

        let removed = remove_mcp_server_config(&global, "fs").expect("remove");
        assert!(removed);
        assert!(!remove_mcp_server_config(&global, "fs").expect("remove"));
        assert_eq!(load_mcp_config(&agent, &project, false).servers.len(), 1);

        // A project override changes exposure without replacing the command.
        config_file(
            &project.join(".pi"),
            "{\"mcpServers\":{\"web\":{\"enabled\":false}}}",
        );
        let loaded = load_mcp_config(&agent, &project, true);
        assert!(loaded.project_config.is_some());
        let web = loaded.servers.iter().find(|server| server.name == "web").expect("web");
        assert_eq!(web.scope, Some(McpScope::Global));
        assert!(web.override_path.is_some());
        assert_eq!(web.config["command"], json!(null));
        assert_eq!(web.config["url"], "https://e.com/mcp");
        assert_eq!(web.config["enabled"], false);
    }

    #[test]
    fn config_errors_are_reported_per_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent = dir.path().join("agent");
        let project = dir.path().join("proj");
        config_file(&agent, "not json");
        config_file(&project.join(".pi"), "{\"mcpServers\":{\"bad name\":{}}}");
        let loaded = load_mcp_config(&agent, &project, true);
        assert_eq!(loaded.errors.len(), 2);
        assert!(loaded.errors[0].contains("mcp.json"), "{:?}", loaded.errors);
        assert!(
            loaded.errors[1].contains("needs \"command\" or \"url\""),
            "{:?}",
            loaded.errors
        );

        // A project override with no global base is an error too.
        let dir2 = tempfile::tempdir().expect("tempdir");
        let agent2 = dir2.path().join("agent");
        let project2 = dir2.path().join("proj");
        std::fs::create_dir_all(&agent2).expect("mkdir");
        config_file(&project2.join(".pi"), "{\"mcpServers\":{\"orphan\":{\"enabled\":false}}}");
        let loaded = load_mcp_config(&agent2, &project2, true);
        assert_eq!(loaded.errors.len(), 1);
        assert!(loaded.errors[0].contains("needs \"command\" or \"url\""), "{:?}", loaded.errors);
    }

    #[test]
    fn validate_config_and_transport_helpers() {
        assert_eq!(
            validate_mcp_server_config("bad name", &json!({"command": "x"})).unwrap_err(),
            "invalid server name \"bad name\" (use letters, digits, \"_\" and \"-\")"
        );
        assert_eq!(
            validate_mcp_server_config("x", &json!({"command": "x", "args": ["ok", 2]})).unwrap_err(),
            "server \"x\": args must be an array of strings"
        );
        assert_eq!(
            validate_mcp_server_config("x", &json!({"url": "ftp://x"})).unwrap_err(),
            "server \"x\": url must be an http or https URL"
        );
        let resolved = validate_mcp_server_config(
            "x",
            &json!({"command": "x", "exposure": "codemode-deferred"}),
        )
        .expect("alias");
        assert_eq!(resolved["exposure"], "codemode");

        let transport = json!({"command": "npx", "args": ["-y", "srv", "."]});
        assert_eq!(describe_transport(&transport), "npx -y srv .");
        assert_eq!(describe_transport(&json!({"url": "http://x"})), "http://x");
        assert_eq!(
            auth_provider(&json!({"url": "http://x", "auth": {"provider": "openai"}})).as_deref(),
            Some("openai")
        );
        assert_eq!(tool_exposure(&json!({"toolExposure": {"t*": "direct"}}), "tool"), "direct");
        assert_eq!(tool_exposure(&json!({}), "tool"), "codemode");
    }

    #[test]
    fn list_text_output_is_verbatim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent = dir.path().join("agent");
        config_file(&agent, "{\"mcpServers\":{\"fs\":{\"command\":\"npx\",\"exposure\":\"direct\"}}}");
        let runtime = FakeRuntime::new().with("fs", "connected", &["read", "write"]);
        let capture = Capture::new();
        let options = make_options(dir.path(), &agent, &runtime, &capture);
        let loaded = load_mcp_config(&agent, dir.path(), false);
        assert_eq!(list(&loaded, false, None, &options), 0);
        assert_eq!(
            *capture.lines.borrow(),
            args(&[
                "fs: connected, 2 tools (direct, global)",
                "  npx",
                "  tools: read, write",
            ])
        );
    }

    #[test]
    fn list_json_and_failed_states() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent = dir.path().join("agent");
        config_file(&agent, "{\"mcpServers\":{\"fs\":{\"command\":\"npx\"}}}");
        let runtime = FakeRuntime::new().with("fs", "needs-auth", &[]);
        let capture = Capture::new();
        let options = make_options(dir.path(), &agent, &runtime, &capture);
        let loaded = load_mcp_config(&agent, dir.path(), false);
        assert_eq!(list(&loaded, true, Some("a note"), &options), 1);
        let text = capture.lines.borrow().join("\n");
        assert!(text.contains("\"state\": \"needs-auth\""), "{text}");
        assert!(text.contains("\"note\": \"a note\""), "{text}");
        assert!(text.contains("\"servers\""), "{text}");

        let empty = LoadedMcpConfig::default();
        let capture = Capture::new();
        let options = make_options(dir.path(), &agent, &runtime, &capture);
        assert_eq!(list(&empty, false, None, &options), 0);
        assert_eq!(
            capture.lines.borrow().first().map(String::as_str),
            Some(format!("No MCP servers configured. Add them to {} or .pi/mcp.json.", agent.join("mcp.json").display()).as_str())
        );
    }

    #[test]
    fn add_and_remove_validate_arguments_and_write_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&agent).expect("mkdir");
        let runtime = FakeRuntime::new();
        let capture = Capture::new();
        let options = make_options(dir.path(), &agent, &runtime, &capture);

        // Missing name / both transports / neither transport.
        assert_eq!(run_mcp_command(&args(&["add"]), &options), 1);
        // A bare positional command IS a valid stdio add; both transports
        // together are what upstream rejects (`cli.ts:379`).
        assert_eq!(
            run_mcp_command(&args(&["add", "--url", "http://x", "srv", "cmd"]), &options),
            1
        );
        assert_eq!(run_mcp_command(&args(&["add", "srv"]), &options), 1);
        assert_eq!(
            capture.errors.borrow().first().map(String::as_str),
            Some(format!("Usage: pi mcp add <server> [options] (--url <url> | -- <command> [args...])\n{HELP_HINT}").as_str())
        );
        // A stdio-only flag on an HTTP add.
        assert_eq!(run_mcp_command(&args(&["add", "srv", "--env", "A=1", "--url", "http://x"]), &options), 1);
        assert_eq!(
            capture.errors.borrow().last().map(String::as_str),
            Some("--env only applies to stdio servers.")
        );
        // A malformed pair.
        assert_eq!(run_mcp_command(&args(&["add", "srv", "--cmd", "--env", "BAD"]), &options), 1);

        let before = capture.errors.borrow().len();
        assert_eq!(
            run_mcp_command(&args(&["add", "srv", "--", "npx", "-y", "tool"]), &options),
            0
        );
        let saved = std::fs::read_to_string(agent.join("mcp.json")).expect("read");
        assert!(saved.contains("\"command\": \"npx\""), "{saved}");
        assert!(saved.contains("\"args\""), "{saved}");
        assert_eq!(
            capture.lines.borrow().last().map(String::as_str),
            Some("Check it with: pi mcp list")
        );
        // Re-adding replaces.
        assert_eq!(run_mcp_command(&args(&["add", "srv", "--url", "https://x/mcp"]), &options), 0);
        let replaced = format!(
            "Replaced global MCP server \"srv\" in {}.",
            agent.join("mcp.json").display()
        );
        assert!(
            capture.lines.borrow().iter().any(|line| line == &replaced),
            "{:?}",
            capture.lines.borrow()
        );
        assert_eq!(
            capture.lines.borrow().last().map(String::as_str),
            Some("Check it with: pi mcp list. If it requires sign-in: pi mcp login srv")
        );
        assert_eq!(capture.errors.borrow().len(), before);

        assert_eq!(run_mcp_command(&args(&["remove", "srv"]), &options), 0);
        assert_eq!(run_mcp_command(&args(&["remove", "srv"]), &options), 1);
        assert_eq!(
            capture.errors.borrow().last().map(String::as_str),
            Some(format!("No global MCP server named \"srv\" in {}.", agent.join("mcp.json").display()).as_str())
        );
        let remove_usage_index = capture.errors.borrow().len();
        assert_eq!(run_mcp_command(&args(&["remove"]), &options), 1);
        assert_eq!(
            capture.errors.borrow().get(remove_usage_index).map(String::as_str),
            Some("Usage: pi mcp remove <server> [-l]\nUse \"pi mcp --help\" for usage.")
        );
    }

    #[test]
    fn dispatch_validates_list_login_logout_and_unknown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent = dir.path().join("agent");
        config_file(
            &agent,
            "{\"mcpServers\":{\"web\":{\"url\":\"https://e.com/mcp\"},\"fs\":{\"command\":\"npx\"}}}",
        );
        let mut runtime = FakeRuntime::new().with("web", "needs-auth", &[]);
        runtime.oauth_urls.insert("web".to_string(), Some("https://e.com/mcp".to_string()));
        let capture = Capture::new();
        let options = make_options(dir.path(), &agent, &runtime, &capture);

        assert_eq!(run_mcp_command(&args(&["bogus"]), &options), 1);
        assert_eq!(
            capture.errors.borrow().first().map(String::as_str),
            Some("Unknown mcp command \"bogus\".\nUse \"pi mcp --help\" for usage.")
        );
        assert_eq!(run_mcp_command(&args(&["list", "extra"]), &options), 1);
        assert_eq!(run_mcp_command(&args(&["list", "--wat"]), &options), 1);
        // Typo suggestion.
        assert_eq!(run_mcp_command(&args(&["logout", "webl"]), &options), 1);
        assert_eq!(
            capture.errors.borrow().last().map(String::as_str),
            Some("No MCP server named \"webl\". Did you mean \"web\"? Configured: web, fs.")
        );
        // --local on a global server.
        assert_eq!(run_mcp_command(&args(&["logout", "web", "-l"]), &options), 1);
        assert_eq!(
            capture.errors.borrow().last().map(String::as_str),
            Some(format!(
                "MCP server \"web\" is defined in {}, not in the project; omit --local.",
                agent.join("mcp.json").display()
            ).as_str())
        );
        // A stdio server has no OAuth URL.
        assert_eq!(run_mcp_command(&args(&["login", "fs"]), &options), 1);
        assert_eq!(
            capture.errors.borrow().last().map(String::as_str),
            Some("MCP server \"fs\" does not use OAuth. Only HTTP servers without an Authorization header do.")
        );
        // Invalid timeout.
        assert_eq!(run_mcp_command(&args(&["login", "web", "--timeout", "abc"]), &options), 1);
        assert_eq!(run_mcp_command(&args(&["login", "web", "--timeout", "0"]), &options), 1);
        assert_eq!(
            capture.errors.borrow().last().map(String::as_str),
            Some("--timeout must be a positive number of seconds.")
        );
        assert_eq!(run_mcp_command(&args(&["login", "web", "--timeout"]), &options), 1);
        assert_eq!(run_mcp_command(&args(&["logout", "web", "extra"]), &options), 1);

        // Logout removes credentials.
        assert_eq!(run_mcp_command(&args(&["logout", "web"]), &options), 0);
        assert!(runtime.removed.get());
        assert_eq!(
            capture.lines.borrow().last().map(String::as_str),
            Some("Signed out of MCP server \"web\".")
        );
    }

    #[test]
    fn login_flows_already_signed_in_and_sign_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent = dir.path().join("agent");
        config_file(&agent, "{\"mcpServers\":{\"web\":{\"url\":\"https://e.com/mcp\"}}}");
        let capture = Capture::new();

        let mut connected = FakeRuntime::new().with("web", "connected", &["a", "b"]);
        connected.oauth_urls.insert("web".to_string(), Some("https://e.com/mcp".to_string()));
        let options = make_options(dir.path(), &agent, &connected, &capture);
        assert_eq!(run_mcp_command(&args(&["login", "web"]), &options), 0);
        assert_eq!(
            capture.lines.borrow().last().map(String::as_str),
            Some("Already signed in to MCP server \"web\" (2 tools).")
        );

        let mut signing = FakeRuntime::new().with("web", "needs-auth", &[]);
        signing.oauth_urls.insert("web".to_string(), Some("https://e.com/mcp".to_string()));
        signing.sign_in_tools = 3;
        // The reconnect snapshot stays `needs-auth` (the fake does not
        // mutate), so this is the "Signed in, but ..." arm; assert the
        // browser step still logged the authorization URL.
        let capture2 = Capture::new();
        let options2 = make_options(dir.path(), &agent, &signing, &capture2);
        assert_eq!(run_mcp_command(&args(&["login", "web"]), &options2), 1);
        assert_eq!(
            capture2.lines.borrow().first().map(String::as_str),
            Some("Sign in to MCP server \"web\" in your browser:\nhttps://example.com/authorize?state=x")
        );
        assert!(
            capture2
                .errors
                .borrow()
                .last()
                .is_some_and(|line| line.starts_with("Signed in, but "))
        );

        let mut cancelled = FakeRuntime::new().with("web", "needs-auth", &[]);
        cancelled.oauth_urls.insert("web".to_string(), Some("https://e.com/mcp".to_string()));
        cancelled.sign_in_cancelled = true;
        let capture3 = Capture::new();
        let options3 = make_options(dir.path(), &agent, &cancelled, &capture3);
        assert_eq!(run_mcp_command(&args(&["login", "web", "--timeout", "10"]), &options3), 1);
        assert_eq!(
            capture3.errors.borrow().last().map(String::as_str),
            Some("Sign-in to MCP server \"web\" was cancelled or not completed within 10 seconds.")
        );
    }

    #[test]
    fn detect_indent_uses_the_files_own_indentation() {
        assert_eq!(detect_indent(None), "  ");
        assert_eq!(detect_indent(Some("{\n    \"mcpServers\": {}\n}")), "    ");
        assert_eq!(detect_indent(Some("{\n\t\"mcpServers\": {}\n}")), "\t");
        assert_eq!(detect_indent(Some("{\n\"mcpServers\": {}\n}")), "  ");
    }
}
