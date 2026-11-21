//! MCP server configuration: `mcp.json` discovery, validation, and the
//! snapshot consumed by `/mcp` and `pi mcp`.
//!
//! Ground truth (upstream 1.5.2):
//! * `packages/coding-agent/src/extensions/mcp/config.ts:1-234` — the primary
//!   port: [`LoadedMcpConfig`], [`McpServerEntry`], `readConfigFile`,
//!   `loadMcpConfig`, and the `editMcpServers` write helpers.
//! * `packages/mcp/src/config.ts:19-243` — the shared `mcpServers` fields,
//!   `validateMcpServerConfig`, `isMcpServerOverride`, and the OAuth checks.
//! * `packages/coding-agent/src/core/mcp-servers.ts:61-227` — pi's own fields
//!   (`exposure`, `toolExposure`, `auth`), alias resolution, and the wrapper
//!   validator the loader calls.
//! * `packages/mcp/src/naming.ts:29-35` — `mcpNamespace` (collision detection).
//! * `packages/coding-agent/src/utils/text.ts:1-9` — `stripBom`.
//! * `packages/coding-agent/src/config.ts:603-681` — `CONFIG_DIR_NAME` and
//!   `getAgentDir`, reached through [`crate::config`].
//!
//! # Deviation: XDG config root
//!
//! Upstream reads the global file from `<agentDir>/mcp.json`, where
//! `agentDir` is `~/.pi/agent`. pi-rs keeps the *same* `<agentDir>/mcp.json`
//! layout but resolves `agentDir` through [`crate::config::get_agent_dir`]:
//! `$PI_CODING_AGENT_DIR`, else `$XDG_CONFIG_HOME/pi-rs`, else
//! `~/.config/pi-rs`. The project file is `<cwd>/.pi/mcp.json` in both ports
//! (`.pi` is upstream-compatible). All path inputs are injectable through
//! [`LoadMcpConfigOptions`] so tests never touch a real config root.
//!
//! Two smaller implementation deviations, forced by the crate's dependency
//! set: JSON parse errors carry `serde_json`'s wording rather than V8's, and
//! URL validation uses the small [`parse_url`] helper instead of the WHATWG
//! `URL` class (no `url` crate is a dependency of `pi-coding-agent`).

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};

/// One `mcpServers` entry as given in `mcp.json` (unknown fields kept, like
/// upstream's `McpServerConfig & Record<string, unknown>`).
pub type McpConfigObject = Map<String, Value>;

/// Where an entry came from. Upstream models this as an optional
/// `"global" | "project" | "extension"` tag; the port makes it explicit and
/// adds `Extension` for servers registered by an extension (the loader only
/// ever produces `Global` or `Project`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpServerScope {
    Global,
    Project,
    Extension,
}

/// One entry of [`LoadedMcpConfig::servers`].
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerEntry {
    pub name: String,
    pub config: McpConfigObject,
    /// Config file that defined the entry, or the extension path for
    /// extension-registered servers.
    pub source: String,
    pub scope: McpServerScope,
    /// Project `mcp.json` that overrides a global server's `enabled`,
    /// `exposure`, or `toolExposure`.
    pub override_path: Option<String>,
}

/// The snapshot the `/mcp` selector and `pi mcp` CLI consume
/// (`LoadedMcpConfig`, `config.ts:62-69`).
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedMcpConfig {
    pub servers: Vec<McpServerEntry>,
    /// Activate the codemode tool when `codemode` servers connect. Upstream
    /// defaults this to `true` at the use site; the loader keeps it unset when
    /// no file specifies it.
    pub auto_enable_codemode: Option<bool>,
    pub errors: Vec<String>,
    /// The trusted project's `mcp.json`, where `/mcp` saves project overrides.
    pub project_config: Option<String>,
}

/// Injectable path inputs for [`load_mcp_config`] (upstream's
/// `{ agentDir, cwd, projectTrusted }` options object).
#[derive(Debug, Clone, PartialEq)]
pub struct LoadMcpConfigOptions {
    pub agent_dir: PathBuf,
    pub cwd: PathBuf,
    pub project_trusted: bool,
}

/// Exposure of a server's tools (`McpExposure`, `mcp-servers.ts:61`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpExposure {
    Codemode,
    Deferred,
    Direct,
    Hidden,
}

impl McpExposure {
    /// The config-file spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codemode => "codemode",
            Self::Deferred => "deferred",
            Self::Direct => "direct",
            Self::Hidden => "hidden",
        }
    }
}

/// Patch applied by `/mcp` (`McpServerConfigPatch`, `config.ts:150-154`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct McpServerConfigPatch {
    pub enabled: Option<bool>,
    pub exposure: Option<McpExposure>,
}

/// Options for [`update_mcp_server_config`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UpdateMcpServerConfigOptions {
    /// When the entry is missing, add it as a project override instead of
    /// failing (upstream's `{ override: true }`).
    pub create_override: bool,
}

const MCP_JSON: &str = "mcp.json";
const OVERRIDE_KEYS: [&str; 3] = ["enabled", "exposure", "toolExposure"];
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn shape_error(path: &Path) -> String {
    format!(
        "{}: expected an object with an \"mcpServers\" object",
        path_string(path)
    )
}

fn is_record(value: &Value) -> bool {
    matches!(value, Value::Object(_))
}

fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{feff}').unwrap_or(content)
}

/// Namespace of a server's tools (`mcpNamespace`, `naming.ts:33-35`): server
/// names differing only in `-` and `_` collide and must be rejected.
pub fn mcp_namespace(server: &str) -> String {
    format!("mcp__{}", server.replace('-', "_"))
}

/// Whether an entry defines a server or only overrides one from a less
/// trusted file (`isMcpServerOverride`, `mcp/src/config.ts:241-243`).
pub fn is_mcp_server_override(entry: &McpConfigObject) -> bool {
    !entry.contains_key("command") && !entry.contains_key("url") && !entry.contains_key("type")
}

#[derive(Default)]
struct McpConfigState {
    servers: Vec<McpServerEntry>,
    auto_enable_codemode: Option<bool>,
    errors: Vec<String>,
}

impl McpConfigState {
    fn position(&self, name: &str) -> Option<usize> {
        self.servers.iter().position(|entry| entry.name == name)
    }
}

/// Read one `mcp.json` into `state` (`readConfigFile`, `config.ts:83-131`).
#[allow(clippy::too_many_lines)] // mirrors the TS loader; split when extending
fn read_config_file(path: &Path, scope: McpServerScope, state: &mut McpConfigState) {
    let path_str = path_string(path);
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            state.errors.push(format!("{path_str}: {error}"));
            return;
        }
    };

    let parsed: Value = match serde_json::from_str(strip_bom(&content)) {
        Ok(parsed) => parsed,
        Err(error) => {
            state.errors.push(format!("{path_str}: {error}"));
            return;
        }
    };
    if !is_record(&parsed)
        || parsed
            .get("mcpServers")
            .is_some_and(|value| !is_record(value))
    {
        state.errors.push(shape_error(path));
        return;
    }

    if let Some(value) = parsed.get("autoEnableCodemode") {
        if let Some(flag) = value.as_bool() {
            state.auto_enable_codemode = Some(flag);
        } else {
            state
                .errors
                .push(format!("{path_str}: autoEnableCodemode must be a boolean"));
        }
    }

    let Some(servers) = parsed.get("mcpServers").and_then(Value::as_object) else {
        return;
    };
    for (name, value) in servers {
        if scope == McpServerScope::Project
            && is_record(value)
            && is_mcp_server_override(value.as_object().unwrap())
        {
            let extra: Vec<&String> = value
                .as_object()
                .unwrap()
                .keys()
                .filter(|key| !OVERRIDE_KEYS.contains(&key.as_str()))
                .collect();
            match state.position(name) {
                None => state.errors.push(format!(
                    "{path_str}: server \"{name}\" needs \"command\" or \"url\", or a global server to override"
                )),
                Some(_) if !extra.is_empty() => state.errors.push(format!(
                    "{path_str}: server \"{name}\": an override can only set {}",
                    OVERRIDE_KEYS.join(", ")
                )),
                Some(position) => {
                    let mut merged = state.servers[position].config.clone();
                    for (key, value) in value.as_object().unwrap() {
                        merged.insert(key.clone(), value.clone());
                    }
                    match validate_mcp_server_config(name, &Value::Object(merged.clone())) {
                        Ok(config) => {
                            let mut entry = state.servers[position].clone();
                            entry.config = config;
                            entry.override_path = Some(path_str.clone());
                            state.servers[position] = entry;
                        }
                        Err(error) => state.errors.push(format!("{path_str}: {error}")),
                    }
                }
            }
            continue;
        }

        let config = match validate_mcp_server_config(name, value) {
            Ok(config) => config,
            Err(error) => {
                state.errors.push(format!("{path_str}: {error}"));
                continue;
            }
        };
        let clash = state
            .servers
            .iter()
            .find(|other| other.name != *name && mcp_namespace(&other.name) == mcp_namespace(name))
            .map(|other| other.name.clone());
        if let Some(clash) = clash {
            state.errors.push(format!(
                "{path_str}: server \"{name}\" conflicts with \"{clash}\""
            ));
            continue;
        }
        if scope == McpServerScope::Project
            && config.contains_key("url")
            && config.contains_key("auth")
        {
            state.errors.push(format!(
                "{path_str}: server \"{name}\": auth is only allowed in the global mcp.json"
            ));
            continue;
        }
        // Upstream uses `servers.set(name, ...)`: a same-name entry from a
        // later (project) file replaces the global one in place.
        let entry = McpServerEntry {
            name: name.clone(),
            config,
            source: path_str.clone(),
            scope,
            override_path: None,
        };
        if let Some(position) = state.position(name) {
            state.servers[position] = entry;
        } else {
            state.servers.push(entry);
        }
    }
}

/// Load the global and (when trusted) project MCP configuration
/// (`loadMcpConfig`, `config.ts:137-148`). Disabled servers stay in the
/// snapshot with `enabled: false` so `/mcp` can enable them again.
pub fn load_mcp_config(options: &LoadMcpConfigOptions) -> LoadedMcpConfig {
    let mut state = McpConfigState::default();
    read_config_file(
        &options.agent_dir.join(MCP_JSON),
        McpServerScope::Global,
        &mut state,
    );
    let project_config = options.project_trusted.then(|| {
        options
            .cwd
            .join(crate::config::PROJECT_CONFIG_DIR_NAME)
            .join(MCP_JSON)
    });
    if let Some(project_config) = &project_config {
        read_config_file(project_config, McpServerScope::Project, &mut state);
    }
    LoadedMcpConfig {
        servers: state.servers,
        auto_enable_codemode: state.auto_enable_codemode,
        errors: state.errors,
        project_config: project_config.as_deref().map(path_string),
    }
}

/// Convenience for the upstream `defaultLoadConfig` (`index.ts:1344-1346`):
/// resolve the agent dir through the XDG-aware [`crate::config::get_agent_dir`].
pub fn load_mcp_config_for_cwd(cwd: impl Into<PathBuf>, project_trusted: bool) -> LoadedMcpConfig {
    load_mcp_config(&LoadMcpConfigOptions {
        agent_dir: crate::config::get_agent_dir(),
        cwd: cwd.into(),
        project_trusted,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerKind {
    Stdio,
    Http,
}

/// Validate one `mcpServers` entry: pi-mcp's shared fields plus pi's exposure
/// and `auth` fields (`validateMcpServerConfig`, `mcp-servers.ts:213-227`).
/// Returns the config with exposure aliases resolved, or an error message
/// matching upstream's wording.
pub fn validate_mcp_server_config(name: &str, raw: &Value) -> Result<McpConfigObject, String> {
    let (kind, config) = validate_server_fields(name, raw)?;
    let error = check_exposures(&config).or_else(|| {
        if kind == ServerKind::Http {
            let url = config.get("url").and_then(Value::as_str).unwrap_or("");
            check_auth(config.get("auth"), url)
        } else {
            None
        }
    });
    error.map_or(Ok(config), |error| {
        Err(format!("server \"{name}\": {error}"))
    })
}

/// Port of `validateMcpServerFields` (`mcp/src/config.ts:113-139`) with pi's
/// alias resolution applied first (`mcp-servers.ts:214`).
fn validate_server_fields(
    name: &str,
    raw: &Value,
) -> Result<(ServerKind, McpConfigObject), String> {
    if !valid_server_name(name) {
        return Err(format!(
            "invalid server name \"{name}\" (use letters, digits, \"_\" and \"-\")"
        ));
    }
    let Some(object) = raw.as_object() else {
        return Err(format!("server \"{name}\" must be an object"));
    };
    let resolved = resolve_exposure_aliases(object);
    if let Some(error) = validate_base_fields(&resolved) {
        return Err(format!("server \"{name}\": {error}"));
    }

    let transport_type = resolved.get("type").and_then(Value::as_str);
    let url = resolved.get("url").and_then(Value::as_str);
    let has_command = resolved.get("command").is_some_and(Value::is_string);

    if transport_type == Some("sse") {
        return Err(format!(
            "server \"{name}\": legacy SSE transport is not supported; use the streamable HTTP URL"
        ));
    }
    if let Some(url) = url
        && (transport_type.is_none() || matches!(transport_type, Some("http" | "streamable-http")))
    {
        if let Some(error) = validate_http_fields(&resolved, url) {
            return Err(format!("server \"{name}\": {error}"));
        }
        return Ok((ServerKind::Http, resolved));
    }
    if has_command && (transport_type.is_none() || transport_type == Some("stdio")) {
        if let Some(error) = validate_stdio_fields(&resolved) {
            return Err(format!("server \"{name}\": {error}"));
        }
        return Ok((ServerKind::Stdio, resolved));
    }
    Err(format!(
        "server \"{name}\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)"
    ))
}

fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn resolve_exposure_aliases(value: &McpConfigObject) -> McpConfigObject {
    let mut resolved = value.clone();
    if resolve_exposure_alias(value.get("exposure")) == Some("codemode") {
        resolved.insert(
            "exposure".to_string(),
            Value::String("codemode".to_string()),
        );
    }
    if let Some(tool_exposure) = value.get("toolExposure").and_then(Value::as_object) {
        let mapped: McpConfigObject = tool_exposure
            .iter()
            .map(|(tool, exposure)| {
                let value = if resolve_exposure_alias(Some(exposure)) == Some("codemode") {
                    Value::String("codemode".to_string())
                } else {
                    exposure.clone()
                };
                (tool.clone(), value)
            })
            .collect();
        resolved.insert("toolExposure".to_string(), Value::Object(mapped));
    }
    resolved
}

/// The exposure an alias stands for; other values are returned unchanged
/// (`resolveExposureAlias`, `mcp-servers.ts:152-154`).
fn resolve_exposure_alias(value: Option<&Value>) -> Option<&str> {
    match value.and_then(Value::as_str) {
        Some("codemode-deferred") => Some("codemode"),
        other => other,
    }
}

fn validate_base_fields(entry: &McpConfigObject) -> Option<String> {
    if entry
        .get("enabled")
        .is_some_and(|value| !value.is_boolean())
    {
        return Some("enabled must be a boolean".to_string());
    }
    if entry
        .get("description")
        .is_some_and(|value| !value.is_string())
    {
        return Some("description must be a string".to_string());
    }
    if let Some(timeout) = entry.get("timeout")
        && !timeout.as_f64().is_some_and(|seconds| seconds > 0.0)
    {
        return Some("timeout must be a positive number of seconds".to_string());
    }
    None
}

fn validate_http_fields(entry: &McpConfigObject, url: &str) -> Option<String> {
    let Some(parts) = parse_url(url) else {
        return Some("url must be an http or https URL".to_string());
    };
    if !matches!(parts.scheme.as_str(), "http" | "https") {
        return Some("url must be an http or https URL".to_string());
    }
    if entry
        .get("headers")
        .is_some_and(|headers| !is_string_record(headers))
    {
        return Some("headers must map names to strings".to_string());
    }
    validate_oauth(entry.get("oauth"))
}

fn validate_stdio_fields(entry: &McpConfigObject) -> Option<String> {
    if let Some(args) = entry.get("args") {
        let ok = args
            .as_array()
            .is_some_and(|list| list.iter().all(Value::is_string));
        if !ok {
            return Some("args must be an array of strings".to_string());
        }
    }
    if entry.get("env").is_some_and(|env| !is_string_record(env)) {
        return Some("env must map names to strings".to_string());
    }
    if entry.get("cwd").is_some_and(|cwd| !cwd.is_string()) {
        return Some("cwd must be a string".to_string());
    }
    None
}

fn is_string_record(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|object| object.values().all(Value::is_string))
}

/// Port of `validateOAuth` (`mcp/src/config.ts:181-227`).
#[allow(clippy::too_many_lines)] // mirrors the TS validator; split when extending
fn validate_oauth(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let Some(object) = value.as_object() else {
        return Some("oauth must be an object".to_string());
    };
    if object.get("clientId").is_some_and(|v| !v.is_string()) {
        return Some("oauth.clientId must be a string".to_string());
    }
    if object.get("clientSecret").is_some_and(|v| !v.is_string()) {
        return Some("oauth.clientSecret must be a string".to_string());
    }
    if let Some(port) = object.get("callbackPort")
        && !port
            .as_u64()
            .is_some_and(|port| (1..=65_535).contains(&port))
    {
        return Some("oauth.callbackPort must be a port number".to_string());
    }
    if let Some(callback_url) = object.get("callbackUrl") {
        let Some(url) = callback_url
            .as_str()
            .filter(|url| is_loopback_redirect_uri(url))
        else {
            return Some(
                "oauth.callbackUrl must be an http URI on localhost, 127.0.0.1, or [::1] without query or fragment"
                    .to_string(),
            );
        };
        let url_port = parse_url(url).and_then(|parts| parts.port);
        let configured_port = object.get("callbackPort").and_then(Value::as_u64);
        if let (Some(url_port), Some(configured_port)) = (url_port, configured_port)
            && u64::from(url_port) != configured_port
        {
            return Some(
                "oauth.callbackUrl and oauth.callbackPort name different ports".to_string(),
            );
        }
    }
    if object.get("scope").is_some_and(|v| !v.is_string()) {
        return Some("oauth.scope must be a string".to_string());
    }
    if object
        .get("clientName")
        .is_some_and(|v| v.as_str().is_none_or(|name| name.trim().is_empty()))
    {
        return Some("oauth.clientName must be a non-empty string".to_string());
    }
    if let Some(registration) = object.get("clientRegistration") {
        let registration = registration.as_str();
        if !matches!(registration, Some("auto" | "dcr")) {
            if registration != Some("cimd") {
                return Some(
                    "oauth.clientRegistration must be \"auto\", \"dcr\", or \"cimd\"".to_string(),
                );
            }
            if object.contains_key("clientId") || object.contains_key("clientName") {
                return Some(
                    "oauth.clientRegistration \"cimd\" cannot be combined with oauth.clientId or oauth.clientName"
                        .to_string(),
                );
            }
            if !is_client_metadata_document_callback(
                object.get("callbackUrl").and_then(Value::as_str),
            ) {
                return Some(
                    "oauth.clientRegistration \"cimd\" requires oauth.callbackUrl on localhost or 127.0.0.1 with path /callback"
                        .to_string(),
                );
            }
        }
    }
    if let Some(metadata_url) = object.get("authServerMetadataUrl") {
        let valid = metadata_url
            .as_str()
            .and_then(parse_url)
            .is_some_and(|parts| {
                parts.scheme == "https"
                    || (parts.scheme == "http" && LOOPBACK_HOSTS.contains(&parts.host.as_str()))
            });
        if !valid {
            return Some(
                "oauth.authServerMetadataUrl must be an https URL, or http on localhost, 127.0.0.1, or [::1]"
                    .to_string(),
            );
        }
    }
    None
}

fn is_loopback_redirect_uri(value: &str) -> bool {
    parse_url(value).is_some_and(|parts| {
        parts.scheme == "http"
            && LOOPBACK_HOSTS.contains(&parts.host.as_str())
            && !parts.has_query
            && !parts.has_fragment
    })
}

fn is_client_metadata_document_callback(callback_url: Option<&str>) -> bool {
    let Some(url) = callback_url else {
        return true;
    };
    parse_url(url).is_some_and(|parts| parts.host != "[::1]" && parts.path == "/callback")
}

/// Port of `checkExposures` (`mcp-servers.ts:181-192`).
fn check_exposures(entry: &McpConfigObject) -> Option<String> {
    const EXPOSURES: [&str; 4] = ["codemode", "deferred", "direct", "hidden"];
    const LIST: &str = "\"codemode\", \"deferred\", \"direct\", \"hidden\"";
    if let Some(exposure) = entry.get("exposure")
        && !exposure
            .as_str()
            .is_some_and(|name| EXPOSURES.contains(&name))
    {
        return Some(format!("exposure must be one of {LIST}"));
    }
    if let Some(tool_exposure) = entry.get("toolExposure") {
        let Some(map) = tool_exposure.as_object() else {
            return Some("toolExposure must map tool names to exposures".to_string());
        };
        for (tool, exposure) in map {
            if !exposure
                .as_str()
                .is_some_and(|name| EXPOSURES.contains(&name))
            {
                return Some(format!("toolExposure \"{tool}\" must be one of {LIST}"));
            }
        }
    }
    None
}

/// Port of `checkAuth` (`mcp-servers.ts:197-207`).
fn check_auth(auth: Option<&Value>, url: &str) -> Option<String> {
    let auth = auth?;
    let provider = auth
        .as_object()
        .and_then(|object| object.get("provider"))
        .and_then(Value::as_str);
    if provider.is_none_or(str::is_empty) {
        return Some("auth.provider must be a provider name".to_string());
    }
    let parts = parse_url(url)?;
    if parts.scheme != "https" && !LOOPBACK_HOSTS.contains(&parts.host.as_str()) {
        return Some(
            "auth requires an https URL, or http on localhost, 127.0.0.1, or [::1]".to_string(),
        );
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UrlParts {
    scheme: String,
    host: String,
    port: Option<u16>,
    path: String,
    has_query: bool,
    has_fragment: bool,
}

/// Minimal absolute-URL parser covering the checks upstream makes with
/// `URL.canParse` / `new URL()` (`mcp/src/config.ts:160,196,221,232`). The
/// crate has no `url` dependency; this handles the http/https and loopback
/// cases the validator needs (documented deviation).
fn parse_url(value: &str) -> Option<UrlParts> {
    let (scheme, rest) = value.split_once(':')?;
    let valid_scheme = !scheme.is_empty()
        && scheme.as_bytes()[0].is_ascii_alphabetic()
        && scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'));
    if !valid_scheme {
        return None;
    }
    let (authority, tail) = if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        (&after[..end], &after[end..])
    } else {
        ("", rest)
    };
    let (host, port) = split_host_port(authority)?;
    let fragment_at = tail.find('#');
    let query_at = tail.find('?');
    let path_end = query_at
        .into_iter()
        .chain(fragment_at)
        .min()
        .unwrap_or(tail.len());
    Some(UrlParts {
        scheme: scheme.to_ascii_lowercase(),
        host,
        port,
        path: tail[..path_end].to_string(),
        has_query: query_at
            .is_some_and(|query| fragment_at.is_none_or(|fragment| query < fragment)),
        has_fragment: fragment_at.is_some(),
    })
}

fn split_host_port(authority: &str) -> Option<(String, Option<u16>)> {
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if host_port.is_empty() {
        return None;
    }
    if let Some(inner) = host_port.strip_prefix('[') {
        let close = inner.find(']')?;
        let host = format!("[{}]", &inner[..close]);
        let after = &inner[close + 1..];
        let port = match after.strip_prefix(':') {
            Some(port) => Some(port.parse::<u16>().ok()?),
            None if after.is_empty() => None,
            None => return None,
        };
        return Some((host.to_ascii_lowercase(), port));
    }
    if let Some((host, port)) = host_port.split_once(':') {
        return Some((host.to_ascii_lowercase(), Some(port.parse::<u16>().ok()?)));
    }
    Some((host_port.to_ascii_lowercase(), None))
}

/// Read an `mcp.json` (empty when missing), let `edit` change its
/// `mcpServers`, and write it back with its indentation when `edit` returns
/// `true` (`editMcpServers`, `config.ts:220-234`). Upstream throws; the port
/// returns `Err` with the same wording.
fn edit_mcp_servers<F>(path: &Path, edit: F) -> Result<(), String>
where
    F: FnOnce(Option<McpConfigObject>, McpConfigObject) -> Result<McpConfigObject, String>,
{
    let text = std::fs::read_to_string(path).ok();
    let mut root: McpConfigObject = match text.as_deref() {
        Some(text) => {
            let parsed: Value = serde_json::from_str(strip_bom(text))
                .map_err(|error| format!("{}: {error}", path_string(path)))?;
            parsed
                .as_object()
                .cloned()
                .ok_or_else(|| shape_error(path))?
        }
        None => McpConfigObject::new(),
    };
    if root
        .get("mcpServers")
        .is_some_and(|servers| !is_record(servers))
    {
        return Err(shape_error(path));
    }
    let servers = root.get("mcpServers").and_then(Value::as_object).cloned();
    root = edit(servers, root)?;

    let indent = detect_indent(text.as_deref());
    // serde_json's Serializer writes to an io::Write, not a String.
    let mut output = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut serializer = serde_json::Serializer::with_formatter(&mut output, formatter);
    Value::Object(root)
        .serialize(&mut serializer)
        .map_err(|error| error.to_string())?;
    output.push(b'\n');
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(path, output).map_err(|error| error.to_string())
}

/// The regex `/^([ \t]+)\S/m` from `config.ts:231`, falling back to two spaces.
fn detect_indent(text: Option<&str>) -> String {
    let Some(text) = text else {
        return "  ".to_string();
    };
    for line in text.lines() {
        let trimmed = line.trim_start_matches([' ', '\t']);
        if trimmed.len() != line.len() && trimmed.chars().next().is_some_and(|c| !c.is_whitespace())
        {
            return line[..line.len() - trimmed.len()].to_string();
        }
    }
    "  ".to_string()
}

/// Change one server's settings in the `mcp.json` that defines or overrides it
/// (`updateMcpServerConfig`, `config.ts:161-185`). Default values are removed
/// for real definitions and kept for overrides; other content is preserved.
///
/// # Errors
/// When `path` does not define `name`, or the file cannot be read/written.
pub fn update_mcp_server_config(
    path: &Path,
    name: &str,
    changes: &McpServerConfigPatch,
    options: &UpdateMcpServerConfigOptions,
) -> Result<(), String> {
    edit_mcp_servers(path, |servers, mut parsed| {
        let mut server = match servers.as_ref().and_then(|all| all.get(name)) {
            Some(Value::Object(map)) => map.clone(),
            _ if options.create_override => McpConfigObject::new(),
            _ => {
                return Err(format!(
                    "{} does not define MCP server \"{name}\"",
                    path_string(path)
                ));
            }
        };
        let keep_defaults = is_mcp_server_override(&server);
        if let Some(enabled) = changes.enabled {
            if enabled && !keep_defaults {
                server.remove("enabled");
            } else {
                server.insert("enabled".to_string(), Value::Bool(enabled));
            }
        }
        if let Some(exposure) = changes.exposure {
            if exposure == McpExposure::Codemode && !keep_defaults {
                server.remove("exposure");
            } else {
                server.insert(
                    "exposure".to_string(),
                    Value::String(exposure.as_str().to_string()),
                );
            }
        }
        let mut all = servers.unwrap_or_default();
        all.insert(name.to_string(), Value::Object(server));
        parsed.insert("mcpServers".to_string(), Value::Object(all));
        Ok(parsed)
    })
}

/// Add a server to an `mcp.json`, creating the file when missing
/// (`addMcpServerConfig`, `config.ts:191-201`). Returns whether an existing
/// entry was replaced.
///
/// # Errors
/// When the file is not an `mcpServers` object or cannot be written.
pub fn add_mcp_server_config(
    path: &Path,
    name: &str,
    config: &McpConfigObject,
) -> Result<bool, String> {
    let mut replaced = false;
    edit_mcp_servers(path, |servers, mut parsed| {
        let mut all = servers.unwrap_or_default();
        replaced = all.contains_key(name);
        all.insert(name.to_string(), Value::Object(config.clone()));
        parsed.insert("mcpServers".to_string(), Value::Object(all));
        Ok(parsed)
    })?;
    Ok(replaced)
}

/// Remove a server from an `mcp.json` (`removeMcpServerConfig`,
/// `config.ts:204-214`). Returns whether the file defined it.
///
/// # Errors
/// When the file is not an `mcpServers` object or cannot be written.
pub fn remove_mcp_server_config(path: &Path, name: &str) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let mut removed = false;
    edit_mcp_servers(path, |servers, mut parsed| {
        let Some(mut all) = servers else {
            return Ok(parsed);
        };
        if !all.contains_key(name) {
            return Ok(parsed);
        }
        all.remove(name);
        removed = true;
        parsed.insert("mcpServers".to_string(), Value::Object(all));
        Ok(parsed)
    })?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_config(root: &Path, relative: &str, content: &str) -> PathBuf {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    fn options(agent: &Path, cwd: &Path, trusted: bool) -> LoadMcpConfigOptions {
        LoadMcpConfigOptions {
            agent_dir: agent.to_path_buf(),
            cwd: cwd.to_path_buf(),
            project_trusted: trusted,
        }
    }

    fn entry<'a>(loaded: &'a LoadedMcpConfig, name: &str) -> &'a McpServerEntry {
        loaded
            .servers
            .iter()
            .find(|server| server.name == name)
            .unwrap()
    }

    #[test]
    fn missing_files_yield_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_mcp_config(&options(dir.path(), dir.path(), true));
        assert_eq!(loaded.servers, Vec::<McpServerEntry>::new());
        assert_eq!(loaded.errors, Vec::<String>::new());
        assert_eq!(loaded.auto_enable_codemode, None);
        // Upstream reports the project config path whenever the project is
        // trusted, even when the file does not exist (`config.ts:140-146`).
        let expected_project = dir.path().join(".pi/mcp.json");
        assert_eq!(
            loaded.project_config.as_deref(),
            expected_project.to_str()
        );
    }

    #[test]
    fn discovers_global_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            "mcp.json",
            r#"{"mcpServers":{"filesystem":{"command":"npx","args":["-y","server-fs"]}}}"#,
        );
        let loaded = load_mcp_config(&options(dir.path(), dir.path(), false));
        assert_eq!(loaded.errors, Vec::<String>::new());
        let server = entry(&loaded, "filesystem");
        assert_eq!(server.scope, McpServerScope::Global);
        assert_eq!(server.source, path.to_string_lossy().into_owned());
        assert_eq!(server.override_path, None);
        assert_eq!(
            server.config.get("command").and_then(Value::as_str),
            Some("npx")
        );
        assert_eq!(loaded.project_config, None);
    }

    #[test]
    fn project_config_loaded_only_when_trusted() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            "mcp.json",
            r#"{"mcpServers":{"global":{"command":"g"}}}"#,
        );
        let project_path = write_config(
            dir.path(),
            ".pi/mcp.json",
            r#"{"mcpServers":{"project":{"command":"p"}}}"#,
        );

        let untrusted = load_mcp_config(&options(dir.path(), dir.path(), false));
        assert_eq!(untrusted.servers.len(), 1);
        assert_eq!(untrusted.project_config, None);

        let trusted = load_mcp_config(&options(dir.path(), dir.path(), true));
        assert_eq!(trusted.errors, Vec::<String>::new());
        assert_eq!(trusted.servers.len(), 2);
        let server = entry(&trusted, "project");
        assert_eq!(server.scope, McpServerScope::Project);
        assert_eq!(server.source, project_path.to_string_lossy().into_owned());
        assert_eq!(
            trusted.project_config.as_deref(),
            Some(project_path.to_str().unwrap())
        );
    }

    #[test]
    fn project_entry_replaces_global_with_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            "mcp.json",
            r#"{"mcpServers":{"srv":{"command":"global"}}}"#,
        );
        let project = write_config(
            dir.path(),
            ".pi/mcp.json",
            r#"{"mcpServers":{"srv":{"command":"local"}}}"#,
        );
        let loaded = load_mcp_config(&options(dir.path(), dir.path(), true));
        assert_eq!(loaded.errors, Vec::<String>::new());
        assert_eq!(loaded.servers.len(), 1);
        let server = entry(&loaded, "srv");
        assert_eq!(
            server.config.get("command").and_then(Value::as_str),
            Some("local")
        );
        assert_eq!(server.source, project.to_string_lossy().into_owned());
        assert_eq!(server.scope, McpServerScope::Project);
        assert_eq!(server.override_path, None);
    }

    #[test]
    fn project_override_merges_allowed_fields_and_keeps_credentials() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            "mcp.json",
            r#"{"mcpServers":{"internal":{"url":"https://example.com/mcp","headers":{"Authorization":"[REDACTED:JSON Secret Field]"},"enabled":true,"exposure":"codemode"}}}"#,
        );
        let project = write_config(
            dir.path(),
            ".pi/mcp.json",
            r#"{"mcpServers":{"internal":{"enabled":false,"exposure":"direct"}}}"#,
        );
        let loaded = load_mcp_config(&options(dir.path(), dir.path(), true));
        assert_eq!(loaded.errors, Vec::<String>::new());
        let server = entry(&loaded, "internal");
        assert_eq!(
            server.config.get("enabled").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            server.config.get("exposure").and_then(Value::as_str),
            Some("direct")
        );
        assert_eq!(
            server
                .config
                .get("headers")
                .and_then(Value::as_object)
                .and_then(|h| h.get("Authorization"))
                .and_then(Value::as_str),
            Some("[REDACTED:JSON Secret Field]")
        );
        assert_eq!(
            server.override_path.as_deref(),
            Some(project.to_str().unwrap())
        );
    }

    #[test]
    fn project_override_without_global_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let project = write_config(
            dir.path(),
            ".pi/mcp.json",
            r#"{"mcpServers":{"srv":{"enabled":false}}}"#,
        );
        let loaded = load_mcp_config(&options(dir.path(), dir.path(), true));
        assert_eq!(loaded.servers, Vec::<McpServerEntry>::new());
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(
            loaded.errors[0],
            format!(
                "{}: server \"srv\" needs \"command\" or \"url\", or a global server to override",
                project.display()
            )
        );
    }

    #[test]
    fn project_override_rejects_extra_keys() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            "mcp.json",
            r#"{"mcpServers":{"srv":{"command":"g"}}}"#,
        );
        let project = write_config(
            dir.path(),
            ".pi/mcp.json",
            // An "override" must omit command/url/type; an extra key like
            // `description` is what upstream rejects (`mcp/src/config.ts:241`).
            r#"{"mcpServers":{"srv":{"enabled":false,"description":"sneaky"}}}"#,
        );
        let loaded = load_mcp_config(&options(dir.path(), dir.path(), true));
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(
            loaded.errors[0],
            format!(
                "{}: server \"srv\": an override can only set enabled, exposure, toolExposure",
                project.display()
            )
        );
        assert_eq!(
            entry(&loaded, "srv")
                .config
                .get("command")
                .and_then(Value::as_str),
            Some("g")
        );
    }

    #[test]
    fn project_cannot_set_auth() {
        let dir = tempfile::tempdir().unwrap();
        let project = write_config(
            dir.path(),
            ".pi/mcp.json",
            r#"{"mcpServers":{"srv":{"url":"https://example.com/mcp","auth":{"provider":"openai"}}}}"#,
        );
        let loaded = load_mcp_config(&options(dir.path(), dir.path(), true));
        assert_eq!(loaded.servers, Vec::<McpServerEntry>::new());
        assert_eq!(
            loaded.errors,
            vec![format!(
                "{}: server \"srv\": auth is only allowed in the global mcp.json",
                project.display()
            )]
        );
    }

    #[test]
    fn malformed_json_reports_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "mcp.json", "{ not json ");
        let loaded = load_mcp_config(&options(dir.path(), dir.path(), false));
        assert_eq!(loaded.errors.len(), 1);
        let prefix = format!("{}: ", path.display());
        assert!(loaded.errors[0].starts_with(prefix.as_str()));
    }

    #[test]
    fn wrong_shape_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "mcp.json", r#"{"mcpServers":5}"#);
        let loaded = load_mcp_config(&options(dir.path(), dir.path(), false));
        assert_eq!(
            loaded.errors,
            vec![format!(
                "{}: expected an object with an \"mcpServers\" object",
                path.display()
            )]
        );
        let root_path = write_config(dir.path(), "root.json", "[]");
        let parsed: Value = serde_json::from_str("[]").unwrap();
        assert!(!is_record(&parsed));
        assert_eq!(
            shape_error(&root_path),
            format!(
                "{}: expected an object with an \"mcpServers\" object",
                root_path.display()
            )
        );
    }

    #[test]
    fn auto_enable_codemode_is_parsed_and_project_wins() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            "mcp.json",
            r#"{"autoEnableCodemode":true,"mcpServers":{}}"#,
        );
        write_config(
            dir.path(),
            ".pi/mcp.json",
            r#"{"autoEnableCodemode":false,"mcpServers":{}}"#,
        );
        let untrusted = load_mcp_config(&options(dir.path(), dir.path(), false));
        assert_eq!(untrusted.auto_enable_codemode, Some(true));
        let trusted = load_mcp_config(&options(dir.path(), dir.path(), true));
        assert_eq!(trusted.auto_enable_codemode, Some(false));

        write_config(dir.path(), "mcp.json", r#"{"autoEnableCodemode":"yes"}"#);
        let bad = load_mcp_config(&options(dir.path(), dir.path(), false));
        assert_eq!(bad.auto_enable_codemode, None);
        assert_eq!(bad.errors.len(), 1);
        assert!(bad.errors[0].ends_with("autoEnableCodemode must be a boolean"));
    }

    #[test]
    fn names_differing_only_by_dash_and_underscore_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            "mcp.json",
            r#"{"mcpServers":{"a-b":{"command":"one"},"a_b":{"command":"two"}}}"#,
        );
        let loaded = load_mcp_config(&options(dir.path(), dir.path(), false));
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(
            loaded.errors,
            vec![format!(
                "{}: server \"a_b\" conflicts with \"a-b\"",
                path.display()
            )]
        );
        assert_eq!(mcp_namespace("a-b"), mcp_namespace("a_b"));
    }

    #[test]
    fn validation_errors_match_upstream_wording() {
        let cases = [
            (
                r#"{"description":"no transport"}"#,
                "server \"ok\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)",
            ),
            (r#""not an object""#, "server \"ok\" must be an object"),
            (
                r#"{"command":"x","timeout":0}"#,
                "server \"ok\": timeout must be a positive number of seconds",
            ),
            (
                r#"{"command":"x","args":[1]}"#,
                "server \"ok\": args must be an array of strings",
            ),
            (
                r#"{"type":"sse","url":"https://example.com"}"#,
                "server \"ok\": legacy SSE transport is not supported; use the streamable HTTP URL",
            ),
            (
                r#"{"url":"ftp://example.com"}"#,
                "server \"ok\": url must be an http or https URL",
            ),
            (
                r#"{"url":"not a url"}"#,
                "server \"ok\": url must be an http or https URL",
            ),
            (
                r#"{"url":"https://example.com","exposure":"loud"}"#,
                "server \"ok\": exposure must be one of \"codemode\", \"deferred\", \"direct\", \"hidden\"",
            ),
        ];
        for (raw, expected) in cases {
            let error =
                validate_mcp_server_config("ok", &serde_json::from_str(raw).unwrap()).unwrap_err();
            assert_eq!(error, expected, "raw = {raw}");
        }
        let error = validate_mcp_server_config("bad.name", &json!({"command": "x"})).unwrap_err();
        assert_eq!(
            error,
            "invalid server name \"bad.name\" (use letters, digits, \"_\" and \"-\")"
        );
    }

    #[test]
    fn validation_passes_http_and_resolves_exposure_aliases() {
        let config = validate_mcp_server_config(
            "docs",
            &json!({
                "url": "https://example.com/mcp",
                "headers": {"Authorization": "[REDACTED:JSON Secret Field]"},
                "exposure": "codemode-deferred",
                "toolExposure": {"search": "codemode-deferred"},
                "auth": {"provider": "openai"}
            }),
        )
        .unwrap();
        assert_eq!(
            config.get("exposure").and_then(Value::as_str),
            Some("codemode")
        );
        assert_eq!(
            config
                .get("toolExposure")
                .and_then(Value::as_object)
                .and_then(|tools| tools.get("search"))
                .and_then(Value::as_str),
            Some("codemode")
        );
        assert!(is_loopback_redirect_uri("http://localhost:8080/callback"));
        assert!(!is_loopback_redirect_uri("http://example.com/callback?x=1"));
    }

    #[test]
    fn oauth_and_auth_validation() {
        let bad_auth = validate_mcp_server_config(
            "srv",
            &json!({"url": "https://example.com/mcp", "auth": {}}),
        );
        assert_eq!(
            bad_auth.unwrap_err(),
            "server \"srv\": auth.provider must be a provider name"
        );
        let bad_auth = validate_mcp_server_config(
            "srv",
            &json!({"url": "http://example.com/mcp", "auth": {"provider": "p"}}),
        );
        assert_eq!(
            bad_auth.unwrap_err(),
            "server \"srv\": auth requires an https URL, or http on localhost, 127.0.0.1, or [::1]"
        );
        let oauth = validate_mcp_server_config(
            "srv",
            &json!({
                "url": "https://example.com/mcp",
                "oauth": {"clientRegistration": "cimd", "callbackUrl": "http://localhost/callback"}
            }),
        );
        assert!(oauth.is_ok(), "{oauth:?}");
        let oauth = validate_mcp_server_config(
            "srv",
            &json!({
                "url": "https://example.com/mcp",
                "oauth": {"clientRegistration": "cimd", "callbackUrl": "http://[::1]/callback"}
            }),
        );
        assert_eq!(
            oauth.unwrap_err(),
            "server \"srv\": oauth.clientRegistration \"cimd\" requires oauth.callbackUrl on localhost or 127.0.0.1 with path /callback"
        );
    }

    #[test]
    fn bom_is_stripped() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            "mcp.json",
            "\u{feff}{\"mcpServers\":{\"srv\":{\"command\":\"x\"}}}",
        );
        let loaded = load_mcp_config(&options(dir.path(), dir.path(), false));
        assert_eq!(loaded.errors, Vec::<String>::new());
        assert_eq!(loaded.servers.len(), 1);
    }

    #[test]
    fn add_update_and_remove_round_trip_preserves_indent() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "mcp.json", "{\n    \"other\": 1\n}\n");
        let mut config = McpConfigObject::new();
        config.insert("command".to_string(), Value::String("x".to_string()));
        assert!(!add_mcp_server_config(&path, "srv", &config).unwrap());
        assert!(add_mcp_server_config(&path, "srv", &config).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("    \"srv\""), "{text}");
        assert!(text.contains("\"other\": 1"), "{text}");

        update_mcp_server_config(
            &path,
            "srv",
            &McpServerConfigPatch {
                enabled: Some(false),
                exposure: Some(McpExposure::Direct),
            },
            &UpdateMcpServerConfigOptions::default(),
        )
        .unwrap();
        let updated = load_mcp_config(&options(dir.path(), dir.path(), false));
        let server = entry(&updated, "srv");
        assert_eq!(
            server.config.get("enabled").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            server.config.get("exposure").and_then(Value::as_str),
            Some("direct")
        );

        assert!(remove_mcp_server_config(&path, "srv").unwrap());
        assert!(!remove_mcp_server_config(&path, "srv").unwrap());
        assert_eq!(
            load_mcp_config(&options(dir.path(), dir.path(), false)).servers,
            Vec::<McpServerEntry>::new()
        );
    }

    #[test]
    fn update_can_create_a_project_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), ".pi/mcp.json", r#"{"mcpServers":{}}"#);
        update_mcp_server_config(
            &path,
            "srv",
            &McpServerConfigPatch {
                enabled: Some(false),
                exposure: None,
            },
            &UpdateMcpServerConfigOptions {
                create_override: true,
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "{\n  \"mcpServers\": {\n    \"srv\": {\n      \"enabled\": false\n    }\n  }\n}\n"
        );
        let error = update_mcp_server_config(
            &path,
            "missing",
            &McpServerConfigPatch::default(),
            &UpdateMcpServerConfigOptions::default(),
        )
        .unwrap_err();
        assert!(error.ends_with("does not define MCP server \"missing\""));
    }

    #[test]
    fn add_creates_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("nested");
        let path = agent.join("mcp.json");
        let mut config = McpConfigObject::new();
        config.insert(
            "url".to_string(),
            Value::String("https://example.com".to_string()),
        );
        assert!(!add_mcp_server_config(&path, "srv", &config).unwrap());
        let loaded = load_mcp_config(&options(&agent, dir.path(), false));
        assert_eq!(loaded.errors, Vec::<String>::new());
        assert_eq!(entry(&loaded, "srv").scope, McpServerScope::Global);
    }
}
