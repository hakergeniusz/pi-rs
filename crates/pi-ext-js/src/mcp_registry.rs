//! MCP server registration: the registry the `pi.registerMcpServer` op
//! serves from, plus the config validation it runs first.
//!
//! Ports `packages/mcp/src/naming.ts:33` (`mcpNamespace`), the validation
//! half of `packages/mcp/src/config.ts:113-160` (`validateMcpServerConfig`
//! and its field validators; the OAuth CIMD hook is a host option upstream
//! and stays unbound in E1), and the registration rules of the
//! `registerMcpServer` API method (`core/extensions/loader.ts:483-505`):
//! an extension may not re-register another extension's server, and names
//! that differ only in `-`/`_` share a namespace.

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

/// The session-scoped MCP registry (`McpServerRegistry`).
#[derive(Debug, Default)]
pub struct McpRegistry {
    servers: Vec<RegisteredMcpServer>,
}

impl McpRegistry {
    /// Registers one server, enforcing the loader's ownership and namespace
    /// rules. The error strings are the ones `pi.registerMcpServer` throws.
    pub fn register(
        &mut self,
        name: &str,
        extension_path: &str,
        config: Value,
    ) -> Result<(), String> {
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
        self.servers.retain(|server| server.name != name);
        self.servers.push(RegisteredMcpServer {
            name: name.to_string(),
            extension_path: extension_path.to_string(),
            config,
        });
        Ok(())
    }

    /// Unregisters one server; only its owner may (`loader.ts:502-505`).
    pub fn unregister(&mut self, name: &str, extension_path: &str) -> Result<(), String> {
        match self.servers.iter().find(|server| server.name == name) {
            None => Err(format!("MCP server \"{name}\" is not registered")),
            Some(server) if server.extension_path != extension_path => Err(format!(
                "MCP server \"{name}\" is registered by extension \"{}\"",
                server.extension_path
            )),
            Some(_) => {
                self.servers.retain(|server| server.name != name);
                Ok(())
            }
        }
    }

    /// All registered servers in registration order.
    pub fn list(&self) -> Vec<&RegisteredMcpServer> {
        self.servers.iter().collect()
    }
}

/// Validates one entry of the `mcpServers` shape (a trust boundary, so the
/// error strings are exact). Returns `Ok(())` or the message
/// `pi.registerMcpServer` prepends `server "{name}": ` to.
pub fn validate_mcp_server_config(name: &str, raw: &Value) -> Result<(), String> {
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
            return match validate_http_fields(entry, url) {
                Ok(()) => Ok(()),
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
        return match validate_stdio_fields(entry) {
            Ok(()) => Ok(()),
            Err(error) => invalid(error),
        };
    }
    invalid("needs either \"command\" (stdio) or \"url\" (streamable HTTP)".to_string())
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
        assert_eq!(
            registry.unregister("app", "b.ts").unwrap_err(),
            "MCP server \"app\" is registered by extension \"a.ts\""
        );
        registry
            .unregister("app", "a.ts")
            .expect("owner unregisters");
        assert!(registry.list().iter().all(|server| server.name != "app"));
    }
}
