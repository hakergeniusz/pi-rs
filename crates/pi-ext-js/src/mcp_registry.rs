//! MCP server registration: the registry the `pi.registerMcpServer` op
//! serves from, plus the config validation it runs first, and the tool
//! naming the bridge uses.
//!
//! Ports `packages/mcp/src/naming.ts:18-35` (`createMcpToolName` with the
//! 64-character cap and the sha256 suffix, `mcpNamespace`), the validation
//! of `core/mcp-servers.ts:213-227` (`validateMcpServerConfig`: pi-mcp's
//! field validators plus pi's exposure aliases (`:63-66`), exposure checks
//! (`:181-192`), and the HTTP `auth` check (`:196-207`); the config stored
//! is the alias-resolved copy) and of `packages/mcp/src/config.ts:113-160`
//! (the shared `mcpServers` fields; the OAuth CIMD hook is a host option
//! upstream and stays unbound), and the registration rules of the
//! `registerMcpServer`/`unregisterMcpServer` API methods
//! (`core/extensions/loader.ts:483-505` + `core/mcp-servers.ts:242-253`):
//! an extension may not re-register another extension's server, names that
//! differ only in `-`/`_` share a namespace, and unregistering an unknown
//! or foreign server is a silent no-op.

use serde_json::Value;

/// A registered MCP server (`{name, config, extensionPath}`).
#[derive(Debug, Clone)]
pub struct RegisteredMcpServer {
    /// The server name as registered.
    pub name: String,
    /// The extension that registered it.
    pub extension_path: String,
    /// The validated server config.
    pub config: Value,
}

/// Namespace of a server's tools: `mcp__<server>` with `-` replaced by `_`,
/// like the tool names (`naming.ts:29-34`).
pub fn mcp_namespace(server: &str) -> String {
    format!("mcp__{}", server.replace('-', "_"))
}

/// Provider tool names are limited to 64 characters of `[A-Za-z0-9_-]`
/// (`naming.ts:10`).
const MAX_TOOL_NAME_LENGTH: usize = 64;
/// Hex characters of the disambiguating hash suffix (`naming.ts:25`).
const TOOL_NAME_HASH_CHARS: usize = 8;

/// `mcp__<server>__<tool>`, sanitized and shortened with a hash suffix when
/// too long or taken (`naming.ts:18-27`). Like Codex, everything but
/// `[A-Za-z0-9_]` becomes `_`; `is_taken` reports names used by a different
/// MCP tool, so sanitizing collisions (`a-b` vs `a_b`) get the suffix.
pub fn create_mcp_tool_name(server: &str, tool: &str, is_taken: impl Fn(&str) -> bool) -> String {
    let name = format!("mcp__{server}__{tool}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    if name.len() <= MAX_TOOL_NAME_LENGTH && !is_taken(&name) {
        return name;
    }
    let hash = tool_name_hash(server, tool);
    // The truncation cap upstream's `name.slice(0, 64 - 8 - 1)` clamps at —
    // a short-but-taken name keeps its full form (JS slice never panics).
    let cap = MAX_TOOL_NAME_LENGTH - TOOL_NAME_HASH_CHARS - 1;
    let truncated = if name.len() > cap {
        &name[..cap]
    } else {
        &name[..]
    };
    format!("{truncated}_{hash}")
}

/// First 8 hex characters of sha256 over `server\0tool` (`naming.ts:25`).
fn tool_name_hash(server: &str, tool: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(server.as_bytes());
    hasher.update([0]);
    hasher.update(tool.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::new();
    for byte in digest.iter().take(4) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The session-scoped MCP registry (`McpServerRegistry`).
#[derive(Debug, Default)]
pub struct McpRegistry {
    servers: Vec<RegisteredMcpServer>,
}

impl McpRegistry {
    /// The ownership and namespace pre-checks `pi.registerMcpServer` runs
    /// before applying (`loader.ts:489-504`). The error strings are the
    /// ones the API method throws.
    pub fn check_register(&self, name: &str, extension_path: &str) -> Result<(), String> {
        if let Some(existing) = self.servers.iter().find(|server| server.name == name) {
            if existing.extension_path != extension_path {
                return Err(format!(
                    "MCP server \"{name}\" is already registered by extension \"{}\"",
                    existing.extension_path
                ));
            }
        }
        if let Some(clash) = self.servers.iter().find(|server| {
            server.name != name && mcp_namespace(&server.name) == mcp_namespace(name)
        }) {
            return Err(format!(
                "MCP server \"{name}\" conflicts with registered server \"{}\"",
                clash.name
            ));
        }
        Ok(())
    }

    /// Registers one server, enforcing the loader's ownership and namespace
    /// rules. The error strings are the ones `pi.registerMcpServer` throws.
    pub fn register(
        &mut self,
        name: &str,
        extension_path: &str,
        config: Value,
    ) -> Result<(), String> {
        self.check_register(name, extension_path)?;
        self.servers.retain(|server| server.name != name);
        self.servers.push(RegisteredMcpServer {
            name: name.to_string(),
            extension_path: extension_path.to_string(),
            config,
        });
        Ok(())
    }

    /// Unregisters one server when this extension owns it; unknown names
    /// and foreign owners are silent no-ops (`mcp-servers.ts:249-253` via
    /// `loader.ts:502-505` — the API method never throws here).
    pub fn unregister(&mut self, name: &str, extension_path: &str) {
        let owned = self
            .servers
            .iter()
            .find(|server| server.name == name)
            .is_some_and(|server| server.extension_path == extension_path);
        if owned {
            self.servers.retain(|server| server.name != name);
        }
    }

    /// All registered servers in registration order.
    pub fn list(&self) -> Vec<&RegisteredMcpServer> {
        self.servers.iter().collect()
    }
}

/// Validates one entry of the `mcpServers` shape (a trust boundary, so the
/// error strings are exact). Returns the alias-resolved config copy
/// upstream stores (`mcp-servers.ts:226` via `loader.ts:497`), or the
/// message `pi.registerMcpServer` throws.
pub fn validate_mcp_server_config(name: &str, raw: &Value) -> Result<Value, String> {
    let invalid = |error: String| Err(format!("server \"{name}\": {error}"));
    if !server_name_is_valid(name) {
        // Upstream returns this one unwrapped: the name is unknown, so the
        // `server "{name}":` prefix would repeat it.
        return Err(format!(
            "invalid server name \"{name}\" (use letters, digits, \"_\" and \"-\")"
        ));
    }
    let Some(entry) = raw.as_object() else {
        return Err(format!("server \"{name}\" must be an object"));
    };
    // Exposure aliases resolve before validation and the resolved copy is
    // what gets stored (`resolveExposureAliases`, `mcp-servers.ts:157-167`).
    let resolved = resolve_exposure_aliases(entry);
    let entry = resolved
        .as_object()
        .expect("resolve_exposure_aliases returns an object");
    if let Some(error) = validate_base_fields(entry) {
        return invalid(error);
    }
    let url = entry.get("url").and_then(Value::as_str);
    let command = entry.get("command").and_then(Value::as_str);
    if entry.get("type").and_then(Value::as_str) == Some("sse") {
        return invalid(
            "legacy SSE transport is not supported; use the streamable HTTP URL".to_string(),
        );
    }
    if let Some(url) = url {
        if matches!(
            entry.get("type").and_then(Value::as_str),
            None | Some("http" | "streamable-http")
        ) {
            return match validate_http_fields(entry, url)
                .and_then(|()| check_exposures(entry).map_or(Ok(()), Err))
                .and_then(|()| check_auth(entry, url).map_or(Ok(()), Err))
            {
                Ok(()) => Ok(resolved),
                Err(error) => invalid(error),
            };
        }
    }
    if command.is_some()
        && matches!(
            entry.get("type").and_then(Value::as_str),
            None | Some("stdio")
        )
    {
        return match validate_stdio_fields(entry)
            .and_then(|()| check_exposures(entry).map_or(Ok(()), Err))
        {
            Ok(()) => Ok(resolved),
            Err(error) => invalid(error),
        };
    }
    invalid("needs either \"command\" (stdio) or \"url\" (streamable HTTP)".to_string())
}

/// The MCP exposures and the older alias accepted in configs
/// (`MCP_EXPOSURES`/`MCP_EXPOSURE_ALIASES`, `mcp-servers.ts:63-66`).
const MCP_EXPOSURES: [&str; 4] = ["codemode", "deferred", "direct", "hidden"];
const CODEMODE_DEFERRED_ALIAS: &str = "codemode-deferred";

/// A copy of the server entry with exposure aliases replaced by their
/// current names (`resolveExposureAliases`, `mcp-servers.ts:157-167`).
fn resolve_exposure_aliases(entry: &serde_json::Map<String, Value>) -> Value {
    let mut resolved = entry.clone();
    if let Some(exposure) = resolved.get_mut("exposure") {
        if exposure.as_str() == Some(CODEMODE_DEFERRED_ALIAS) {
            *exposure = Value::String("codemode".to_string());
        }
    }
    if let Some(Value::Object(tool_exposure)) = resolved.get_mut("toolExposure") {
        for value in tool_exposure.values_mut() {
            if value.as_str() == Some(CODEMODE_DEFERRED_ALIAS) {
                *value = Value::String("codemode".to_string());
            }
        }
    }
    Value::Object(resolved)
}

/// Whether a value names an MCP exposure (`isExposure`, `mcp-servers.ts:147-149`).
fn is_exposure(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|text| MCP_EXPOSURES.contains(&text))
}

/// Checks `exposure` and `toolExposure` (after aliases were resolved);
/// absent keys are valid (`checkExposures`, `mcp-servers.ts:181-192` — an
/// explicit `null` is present and invalid, like `undefined` vs `null` in
/// JS).
fn check_exposures(entry: &serde_json::Map<String, Value>) -> Option<String> {
    let exposures = MCP_EXPOSURES
        .iter()
        .map(|value| format!("\"{value}\""))
        .collect::<Vec<_>>()
        .join(", ");
    if let Some(exposure) = entry.get("exposure") {
        if !is_exposure(exposure) {
            return Some(format!("exposure must be one of {exposures}"));
        }
    }
    if let Some(tool_exposure) = entry.get("toolExposure") {
        let Some(tool_exposure) = tool_exposure.as_object() else {
            return Some("toolExposure must map tool names to exposures".to_string());
        };
        for (tool, value) in tool_exposure {
            if !is_exposure(value) {
                return Some(format!(
                    "toolExposure \"{tool}\" must be one of {exposures}"
                ));
            }
        }
    }
    None
}

/// Loopback hosts an http `auth` URL may use (`LOOPBACK_HOSTS`,
/// `mcp-servers.ts:194`).
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// Checks `auth` of an HTTP server, which sends a provider's credential to
/// `url` (`checkAuth`, `mcp-servers.ts:196-207`; absent is valid, an
/// explicit `null` is present and invalid).
fn check_auth(entry: &serde_json::Map<String, Value>, url: &str) -> Option<String> {
    let auth = entry.get("auth")?;
    let Some(auth) = auth.as_object() else {
        return Some("auth.provider must be a provider name".to_string());
    };
    let provider_ok = auth
        .get("provider")
        .is_some_and(|provider| provider.as_str().is_some_and(|text| !text.is_empty()));
    if !provider_ok {
        return Some("auth.provider must be a provider name".to_string());
    }
    let host = url_host(url);
    if !url.starts_with("https://") && !LOOPBACK_HOSTS.contains(&host.as_str()) {
        return Some(
            "auth requires an https URL, or http on localhost, 127.0.0.1, or [::1]".to_string(),
        );
    }
    None
}

/// The `host[:port]` of an http(s) URL (`new URL(url).hostname`), empty for
/// anything else.
fn url_host(url: &str) -> String {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return String::new();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    // Strip the port like URL.hostname does ([::1]:8080 → [::1]).
    if authority.starts_with('[') {
        return authority.find(']').map_or_else(
            || authority.to_string(),
            |end| authority[..=end].to_string(),
        );
    }
    authority.split(':').next().unwrap_or_default().to_string()
}

/// Exposure of one tool of a server: its `toolExposure` entry, else the
/// server's `exposure` (`getMcpToolExposure`,
/// `core/mcp-servers.ts:170-178`). Exact names win over patterns; among
/// patterns the first match in the object wins. The pattern matcher
/// approximates upstream's `*`-glob regex with an in-order literal scan.
#[must_use]
pub fn get_mcp_tool_exposure(config: &Value, tool_name: &str) -> String {
    if let Some(overrides) = config.get("toolExposure").and_then(Value::as_object) {
        if let Some(exact) = overrides.get(tool_name) {
            if let Some(exposure) = exact.as_str() {
                return exposure.to_string();
            }
        }
        for (pattern, exposure) in overrides {
            if pattern.contains('*')
                && tool_pattern_matches(pattern, tool_name)
                && let Some(exposure) = exposure.as_str()
            {
                return exposure.to_string();
            }
        }
    }
    config
        .get("exposure")
        .and_then(Value::as_str)
        .unwrap_or("codemode")
        .to_string()
}

/// Whether `name` matches a `*`-pattern (`toolPatternRegExp`,
/// `core/mcp-servers.ts:21-27`): literal segments match in order, the first
/// anchored to the start and the last to the end.
fn tool_pattern_matches(pattern: &str, name: &str) -> bool {
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
        } else {
            let Some(position) = rest.find(part) else {
                return false;
            };
            rest = &rest[position + part.len()..];
        }
    }
    true
}

/// Tool exposure of an MCP exposure (`toToolExposure`,
/// `extensions/mcp/tools.ts:46-48`): `codemode` and `deferred` both leave
/// tools out of the codemode description, so `codemode` registers as the
/// `deferred` tool exposure.
#[must_use]
pub fn to_tool_exposure(exposure: &str) -> &'static str {
    match exposure {
        "codemode" | "deferred" => "deferred",
        "direct" => "direct",
        // Configs carry only validated exposures.
        _ => "hidden",
    }
}

fn server_name_is_valid(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn validate_base_fields(entry: &serde_json::Map<String, Value>) -> Option<String> {
    match entry.get("enabled") {
        None | Some(Value::Bool(_)) => {}
        Some(_) => return Some("enabled must be a boolean".to_string()),
    }
    match entry.get("description") {
        None | Some(Value::String(_)) => {}
        Some(_) => return Some("description must be a string".to_string()),
    }
    match entry.get("timeout") {
        None => {}
        Some(Value::Number(number)) if number.as_f64().is_some_and(|seconds| seconds > 0.0) => {}
        Some(_) => return Some("timeout must be a positive number of seconds".to_string()),
    }
    None
}

fn validate_http_fields(entry: &serde_json::Map<String, Value>, url: &str) -> Result<(), String> {
    if !is_http_url(url) {
        return Err("url must be an http or https URL".to_string());
    }
    if let Some(headers) = entry.get("headers") {
        if !is_string_record(headers) {
            return Err("headers must map names to strings".to_string());
        }
    }
    validate_oauth(entry.get("oauth"))
}

/// `URL.canParse(url) && /^https?:$/.test(new URL(url).protocol)`.
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

fn validate_stdio_fields(entry: &serde_json::Map<String, Value>) -> Result<(), String> {
    if let Some(args) = entry.get("args") {
        let ok = args
            .as_array()
            .is_some_and(|args| args.iter().all(Value::is_string));
        if !ok {
            return Err("args must be an array of strings".to_string());
        }
    }
    if let Some(env) = entry.get("env") {
        if !is_string_record(env) {
            return Err("env must map names to strings".to_string());
        }
    }
    if let Some(cwd) = entry.get("cwd") {
        if !cwd.is_string() {
            return Err("cwd must be a string".to_string());
        }
    }
    Ok(())
}

fn is_string_record(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|record| record.values().all(Value::is_string))
}

fn validate_oauth(value: Option<&Value>) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    let Some(oauth) = value.as_object() else {
        return Err("oauth must be an object".to_string());
    };
    if let Some(client_id) = oauth.get("clientId") {
        if !client_id.is_string() {
            return Err("oauth.clientId must be a string".to_string());
        }
    }
    if let Some(client_secret) = oauth.get("clientSecret") {
        if !client_secret.is_string() {
            return Err("oauth.clientSecret must be a string".to_string());
        }
    }
    if let Some(port) = oauth.get("callbackPort") {
        let ok = port
            .as_i64()
            .is_some_and(|port| (1..=65535).contains(&port));
        if !ok {
            return Err("oauth.callbackPort must be a port number".to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn namespace_replaces_dashes() {
        assert_eq!(mcp_namespace("dev-app"), "mcp__dev_app");
        assert_eq!(mcp_namespace("app"), "mcp__app");
    }

    #[test]
    fn tool_names_sanitize_and_hash_on_collision() {
        let name = create_mcp_tool_name("docs", "search", |_| false);
        assert_eq!(name, "mcp__docs__search");
        // Everything but [A-Za-z0-9_] sanitizes, so `a-b` and `a_b` collide.
        assert_eq!(create_mcp_tool_name("s", "a-b", |_| false), "mcp__s__a_b");
        let taken = |candidate: &str| candidate == "mcp__s__a_b";
        let hashed = create_mcp_tool_name("s", "a-b", taken);
        assert!(hashed.starts_with("mcp__s__a_b_"), "{hashed}");
        assert_eq!(hashed.len(), "mcp__s__a_b_".len() + TOOL_NAME_HASH_CHARS);
        // Deterministic for a given server/tool pair.
        assert_eq!(hashed, create_mcp_tool_name("s", "a-b", taken));
        // Over-long names get the suffix even when not taken.
        let long_tool = "t".repeat(80);
        let long = create_mcp_tool_name("s", &long_tool, |_| false);
        assert_eq!(long.len(), MAX_TOOL_NAME_LENGTH, "{long}");
    }

    #[test]
    fn validates_stdio_and_http() {
        assert!(
            validate_mcp_server_config("fs", &json!({"command": "npx", "args": ["-y", "x"]}))
                .is_ok()
        );
        assert!(
            validate_mcp_server_config("web", &json!({"url": "https://example.com/mcp"})).is_ok()
        );
        assert_eq!(
            validate_mcp_server_config("bad name", &json!({"command": "x"})).unwrap_err(),
            "invalid server name \"bad name\" (use letters, digits, \"_\" and \"-\")"
        );
        assert_eq!(
            validate_mcp_server_config("x", &json!({"type": "sse", "url": "http://x"}))
                .unwrap_err(),
            "server \"x\": legacy SSE transport is not supported; use the streamable HTTP URL"
        );
        assert_eq!(
            validate_mcp_server_config("x", &json!({"command": "x", "args": ["ok", 2]}))
                .unwrap_err(),
            "server \"x\": args must be an array of strings"
        );
        assert_eq!(
            validate_mcp_server_config("x", &json!({"url": "ftp://x"})).unwrap_err(),
            "server \"x\": url must be an http or https URL"
        );
        assert_eq!(
            validate_mcp_server_config("x", &json!({})).unwrap_err(),
            "server \"x\": needs either \"command\" (stdio) or \"url\" (streamable HTTP)"
        );
    }

    #[test]
    fn validates_pi_exposure_fields() {
        assert_eq!(
            validate_mcp_server_config("x", &json!({"command": "c", "exposure": "loud"}))
                .unwrap_err(),
            "server \"x\": exposure must be one of \"codemode\", \"deferred\", \"direct\", \"hidden\""
        );
        assert_eq!(
            validate_mcp_server_config("x", &json!({"command": "c", "toolExposure": {"t": 3}}))
                .unwrap_err(),
            "server \"x\": toolExposure \"t\" must be one of \"codemode\", \"deferred\", \"direct\", \"hidden\""
        );
        assert_eq!(
            validate_mcp_server_config("x", &json!({"command": "c", "toolExposure": []}))
                .unwrap_err(),
            "server \"x\": toolExposure must map tool names to exposures"
        );
        // The alias resolves and the resolved copy is stored.
        let resolved = validate_mcp_server_config(
            "x",
            &json!({"command": "c", "exposure": "codemode-deferred"}),
        )
        .expect("alias is valid");
        assert_eq!(resolved["exposure"], "codemode");
    }

    #[test]
    fn validates_http_auth() {
        assert_eq!(
            validate_mcp_server_config("x", &json!({"url": "https://e.com", "auth": {}}))
                .unwrap_err(),
            "server \"x\": auth.provider must be a provider name"
        );
        assert_eq!(
            validate_mcp_server_config(
                "x",
                &json!({"url": "http://example.com/mcp", "auth": {"provider": "p"}})
            )
            .unwrap_err(),
            "server \"x\": auth requires an https URL, or http on localhost, 127.0.0.1, or [::1]"
        );
        assert!(
            validate_mcp_server_config(
                "x",
                &json!({"url": "http://localhost/mcp", "auth": {"provider": "p"}})
            )
            .is_ok()
        );
    }

    #[test]
    fn register_enforces_owner_and_namespace() {
        let mut registry = McpRegistry::default();
        registry
            .register("app", "a.ts", json!({"command": "x"}))
            .expect("first registration");
        assert_eq!(
            registry.register("app", "b.ts", json!({})).unwrap_err(),
            "MCP server \"app\" is already registered by extension \"a.ts\""
        );
        registry
            .register("x-y", "b.ts", json!({}))
            .expect("namespace-distinct registration");
        assert_eq!(
            registry.register("x_y", "c.ts", json!({})).unwrap_err(),
            "MCP server \"x_y\" conflicts with registered server \"x-y\""
        );
        // Unregistering an unknown or foreign server is a silent no-op
        // (mcp-servers.ts:249-253).
        registry.unregister("app", "b.ts");
        assert!(registry.list().iter().any(|server| server.name == "app"));
        registry.unregister("missing", "a.ts");
        registry.unregister("app", "a.ts");
        assert!(registry.list().iter().all(|server| server.name != "app"));
    }
}
