//! CLI MCP runtime seam (`extensions/mcp/runtime.ts:212-214`, `:361-409`,
//! `:444-455`, `cli.ts:299`, `:627-656`). Bridges the `pi mcp` CLI's
//! [`McpRuntime`] trait to the real `pi-mcp` client for connection/tool
//! listing and to the `mcp-auth.json` store for credential removal.
//!
//! [`CliMcpRuntime::sign_in`] drives the two sign-in halves directly
//! (`extensions/mcp/oauth.ts:207-250`, `cli.ts:627-656`): a loopback callback
//! server, `start_mcp_sign_in` (refresh or browser URL), and
//! `complete_mcp_sign_in` after the redirect. A `needs-auth` connect result
//! is produced by the CLI itself for HTTP servers with an auth challenge;
//! the interactive `/mcp` selector needs the same state from its own host
//! seam (see the report to the caller).
//!
//! Deviations: the pasted-redirect-URL arm (`promptForRedirectUrl`) is not
//! wired, because the pi-mcp prompt callback needs a `'static` future while
//! this runtime's prompt is a borrowed `FnMut`; browser loopback works.
//! Header/env `!cmd` config values resolve through
//! [`crate::auth::resolve_config_value`]; a failed resolution is reported as
//! a failed connection rather than aborting the whole `list` command.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pi_mcp::oauth::{
    CancellationToken, ListenForLoopbackSignInOptions, MCP_AUTH_FILE_NAME, McpOAuthCredentialStore,
    McpOAuthServerStore, McpOAuthSettings, McpSignInOptions, McpSignInStart, OAuthError,
    WaitForLoopbackSignInOptions, complete_mcp_sign_in, listen_for_loopback_sign_in,
    start_mcp_sign_in, wait_for_loopback_sign_in,
};
use pi_mcp::{McpClient, McpError, McpServerConfig, McpTransportConfig};
use serde_json::Value as Json;

use super::mcp_command::{ConnectionSnapshot, McpRuntime, McpServerEntry, SignInError};

/// Wall-clock ceiling for a CLI connection handshake and `tools/list`
/// (`runtime.ts:212-214` uses `(config.timeout ?? 60) * 1000`).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Cancellation vs. failure, as `cli.ts:641-647` distinguishes them.
fn sign_in_error(error: &McpError) -> SignInError {
    if matches!(error, McpError::OAuth(OAuthError::SignInCancelled)) {
        SignInError::Cancelled
    } else {
        SignInError::Failed(error.to_string())
    }
}

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
                    result.state = if error.needs_auth() {
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
        name: &str,
        url: &str,
        timeout_ms: u64,
        show_authorization_url: &mut dyn FnMut(&str),
        _prompt_redirect_url: &mut dyn FnMut() -> Option<String>,
    ) -> Result<usize, SignInError> {
        // `signInMcpServer` (oauth.ts:207-250) plus the reconnect and tool
        // count of `cli.ts:627-656`. The CLI has no TUI event loop, but the
        // loopback callback and the two sign-in halves are plain async work
        // the CLI's tokio runtime can drive.
        crate::runtime::block_on(async {
            let agent_dir = crate::config::get_agent_dir();
            let credentials =
                McpOAuthCredentialStore::with_path(agent_dir.join(MCP_AUTH_FILE_NAME));
            let store: Arc<dyn McpOAuthServerStore> = Arc::new(
                credentials
                    .for_server(name, url)
                    .map_err(|error| sign_in_error(&error))?,
            );
            let settings = McpOAuthSettings::default();
            let callback = listen_for_loopback_sign_in(ListenForLoopbackSignInOptions {
                server_url: url,
                store: store.clone(),
                settings: &settings,
                client_metadata_documents: None,
                render_page: None,
            })
            .await
            .map_err(|error| sign_in_error(&error))?;

            // `AbortSignal.timeout(timeoutMs)` (cli.ts:639): one timer bounds
            // every half of the flow, including the browser wait.
            let signal = CancellationToken::new();
            let timer_signal = signal.clone();
            let timer = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
                timer_signal.cancel();
            });
            let options = McpSignInOptions {
                server_url: url.to_string(),
                store: store.clone(),
                settings,
                client_name: "pi".to_string(),
                client_metadata_documents: None,
                redirect_url: callback.redirect_url.clone(),
                challenge: None,
                signal: Some(signal.clone()),
            };
            let outcome = async {
                match start_mcp_sign_in(&options)
                    .await
                    .map_err(|error| sign_in_error(&error))?
                {
                    McpSignInStart::Authorized => Ok(()),
                    McpSignInStart::Redirect(authorization) => {
                        show_authorization_url(&authorization.authorization_url);
                        let response = wait_for_loopback_sign_in(WaitForLoopbackSignInOptions {
                            callback: callback.callback,
                            authorization: authorization.clone(),
                            prompt_for_redirect_url: None,
                            signal: Some(signal.clone()),
                        })
                        .await
                        .map_err(|error| sign_in_error(&error))?;
                        complete_mcp_sign_in(&options, &authorization, &response)
                            .await
                            .map_err(|error| sign_in_error(&error))
                    }
                }
            }
            .await;
            timer.abort();
            outcome?;

            // The reconnect (`cli.ts:649-656`): the command reports the
            // tool count, so open one fresh client against the same URL.
            let spawn = McpServerConfig {
                name: name.to_string(),
                transport: McpTransportConfig::Http {
                    url: url.to_string(),
                    headers: HashMap::new(),
                },
            };
            let client = McpClient::connect_with_timeout(&spawn, CONNECT_TIMEOUT)
                .await
                .map_err(|error| sign_in_error(&error))?;
            let tools = client
                .tools_list_with_timeout(CONNECT_TIMEOUT)
                .await
                .map_err(|error| sign_in_error(&error))?
                .tools
                .len();
            let _ = client.shutdown().await;
            Ok(tools)
        })
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
