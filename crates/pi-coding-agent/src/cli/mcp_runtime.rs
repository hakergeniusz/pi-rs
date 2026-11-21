//! CLI MCP runtime seam (`extensions/mcp/runtime.ts:212-214`, `:361-409`,
//! `:444-455`, `cli.ts:299`, `:627-656`). Bridges the `pi mcp` CLI's
//! [`McpRuntime`] trait to the real `pi-mcp` client for connection/tool
//! listing and to the `mcp-auth.json` store for credential removal.
//!
//! Deviations: the OAuth sign-in flow (`signInMcpServer`) is not driven from
//! the CLI — it stays interactive-mode work — so [`CliMcpRuntime::sign_in`]
//! returns a typed failure. Header/env `!cmd` config values resolve through
//! [`crate::auth::resolve_config_value`]; a failed resolution is reported as
//! a failed connection rather than aborting the whole `list` command.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use pi_mcp::{McpClient, McpError, McpServerConfig, McpTransportConfig};
use serde_json::Value as Json;

use super::mcp_command::{ConnectionSnapshot, McpRuntime, McpServerEntry, SignInError};

/// Wall-clock ceiling for a CLI connection handshake and `tools/list`
/// (`runtime.ts:212-214` uses `(config.timeout ?? 60) * 1000`).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The real runtime used by `pi mcp` in the binary.
#[derive(Debug, Default, Clone, Copy)]
pub struct CliMcpRuntime;

impl CliMcpRuntime {
    /// Build a `pi-mcp` spawn config from a validated CLI entry.
    fn spawn_config(entry: &McpServerEntry, cwd: &Path) -> Result<McpServerConfig, String> {
        let resolve = |key: &str, raw: &str| -> Result<String, String> {
            crate::auth::resolve_config_value(raw, None)
                .ok_or_else(|| format!("MCP server \"{}\": unresolved {key}", entry.name))
        };
        if let Some(url) = entry.config.get("url").and_then(Json::as_str) {
            let mut headers = HashMap::new();
            if let Some(record) = entry.config.get("headers").and_then(Json::as_object) {
                for (key, value) in record {
                    let Some(raw) = value.as_str() else { continue };
                    headers.insert(key.clone(), resolve("header", raw)?);
                }
            }
            return Ok(McpServerConfig {
                name: entry.name.clone(),
                transport: McpTransportConfig::Http {
                    url: url.to_string(),
                    headers,
                },
            });
        }
        let command = entry
            .config
            .get("command")
            .and_then(Json::as_str)
            .ok_or_else(|| format!("MCP server \"{}\": config has no \"command\"", entry.name))?;
        let args = entry
            .config
            .get("args")
            .and_then(Json::as_array)
            .map(|args| {
                args.iter()
                    .filter_map(Json::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut env = HashMap::new();
        if let Some(record) = entry.config.get("env").and_then(Json::as_object) {
            for (key, value) in record {
                let Some(raw) = value.as_str() else { continue };
                env.insert(key.clone(), resolve("env", raw)?);
            }
        }
        let working_dir = entry
            .config
            .get("cwd")
            .and_then(Json::as_str)
            .map_or_else(|| cwd.to_path_buf(), |raw| cwd.join(raw));
        Ok(McpServerConfig {
            name: entry.name.clone(),
            transport: McpTransportConfig::Stdio {
                command: command.to_string(),
                args,
                env,
                working_dir: Some(working_dir),
            },
        })
    }
}

impl McpRuntime for CliMcpRuntime {
    fn oauth_url(&self, entry: &McpServerEntry, _cwd: &Path, _agent_dir: &Path) -> Option<String> {
        let url = entry.config.get("url")?.as_str()?;
        let has_authorization = entry
            .config
            .get("headers")
            .and_then(Json::as_object)
            .is_some_and(|headers| {
                headers
                    .keys()
                    .any(|key| key.eq_ignore_ascii_case("authorization"))
            });
        (!has_authorization).then(|| url.to_string())
    }

    fn connect(&self, entry: &McpServerEntry, cwd: &Path, _agent_dir: &Path) -> ConnectionSnapshot {
        let base = || ConnectionSnapshot {
            state: String::new(),
            tools: Vec::new(),
            has_resources: false,
            error: None,
        };
        if entry.config.get("enabled") == Some(&Json::Bool(false)) {
            let mut result = base();
            result.state = "disabled".to_string();
            return result;
        }
        let spawn = match Self::spawn_config(entry, cwd) {
            Ok(spawn) => spawn,
            Err(error) => {
                let mut result = base();
                result.state = "failed".to_string();
                result.error = Some(error);
                return result;
            }
        };
        crate::runtime::block_on(async {
            let mut result = base();
            match McpClient::connect_with_timeout(&spawn, CONNECT_TIMEOUT).await {
                Ok(client) => match client.tools_list_with_timeout(CONNECT_TIMEOUT).await {
                    Ok(list) => {
                        result.state = "connected".to_string();
                        result.tools = list.tools.into_iter().map(|tool| tool.name).collect();
                        let _ = client.shutdown().await;
                    }
                    Err(error) => {
                        result.state = "failed".to_string();
                        result.error = Some(error.to_string());
                        let _ = client.shutdown().await;
                    }
                },
                Err(error) => {
                    result.state = if matches!(error, McpError::AuthRequired { .. }) {
                        "needs-auth".to_string()
                    } else {
                        "failed".to_string()
                    };
                    result.error = Some(error.to_string());
                }
            }
            result
        })
    }

    fn close(&self, _entry: &McpServerEntry, _cwd: &Path, _agent_dir: &Path) {
        // Each CLI `connect` owns and shuts down its own one-shot client.
    }

    fn remove_credentials(&self, name: &str, url: &str) -> bool {
        let agent_dir = crate::config::get_agent_dir();
        let store = pi_mcp::oauth::McpOAuthCredentialStore::with_path(
            agent_dir.join(pi_mcp::oauth::MCP_AUTH_FILE_NAME),
        );
        store.remove(name, url).unwrap_or(false)
    }

    fn sign_in(
        &self,
        _name: &str,
        _url: &str,
        _timeout_ms: u64,
        _show_authorization_url: &mut dyn FnMut(&str),
        _prompt_redirect_url: &mut dyn FnMut() -> Option<String>,
    ) -> Result<usize, SignInError> {
        // Interactive-mode work: the CLI has no event loop to drive the
        // loopback callback / browser flow. Keep the typed failure so the
        // command prints a truthful message instead of hanging.
        Err(SignInError::Failed(
            "MCP sign-in is only available in interactive mode (`pi` then `/mcp`)".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(text: &str) -> Json {
        serde_json::from_str(text).expect("valid test JSON")
    }

    fn entry(value: Json) -> McpServerEntry {
        McpServerEntry {
            name: "srv".to_string(),
            config: value,
            source: "global".to_string(),
            scope: None,
            override_path: None,
        }
    }

    #[test]
    fn oauth_url_requires_http_without_authorization() {
        let runtime = CliMcpRuntime;
        let http = entry(json("{\"url\":\"http://x\"}"));
        assert_eq!(
            runtime
                .oauth_url(&http, Path::new("/"), Path::new("/"))
                .as_deref(),
            Some("http://x")
        );
        let authed = entry(json(
            "{\"url\":\"http://x\",\"headers\":{\"Authorization\":\"Bearer x\"}}",
        ));
        assert_eq!(
            runtime.oauth_url(&authed, Path::new("/"), Path::new("/")),
            None
        );
        let stdio = entry(json("{\"command\":\"x\"}"));
        assert_eq!(
            runtime.oauth_url(&stdio, Path::new("/"), Path::new("/")),
            None
        );
    }

    #[test]
    fn disabled_servers_report_disabled_without_spawning() {
        let runtime = CliMcpRuntime;
        let disabled = entry(json("{\"command\":\"x\",\"enabled\":false}"));
        let snapshot = runtime.connect(&disabled, Path::new("/"), Path::new("/"));
        assert_eq!(snapshot.state, "disabled");
        assert_eq!(snapshot.tools, Vec::<String>::new());
    }
}
