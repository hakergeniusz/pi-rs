# Wiring `pi mcp` into the binary

The module `crates/pi-coding-agent/src/cli/mcp_command.rs` is complete and
stands alone. It deliberately does **not** edit shared wiring files. This note
is the exact set of changes the coordinator must apply.

## 1. Module registration — `crates/pi-coding-agent/src/cli/mod.rs`

Add one line (alphabetical between `file_processor` and `package_command`):

```rust
pub mod mcp_command;
```

## 2. `crates/pi-coding-agent/src/cli/args.rs` — no change needed

`print_help()` already advertises `pi mcp <command>` (lines 474-480 of the
current file), and `parse_args` must **not** see the `mcp` argv: dispatch
happens before `parse_args`, exactly like `pi install|remove|list|update`
(`main.rs:41-45`). Do not add a `mcp` flag or an `Args` field.

## 3. `crates/pi-coding-agent/src/main.rs`

### 3a. Dispatch (place immediately after the `handle_package_command` block,
before `let args = parse_args(&raw);`):

```rust
// `pi mcp` routes before mode startup like the package commands
// (upstream `main.ts:665-667`): runMcpCommand(args.slice(1), { cwd, agentDir }).
if raw.first().is_some_and(|word| word == "mcp") {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let agent_dir = pi_coding_agent::config::get_agent_dir();
    let runtime = <your McpRuntime impl>;
    let open_browser = |url: &str| { /* platform opener, e.g. `xdg-open` */ };
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let prompt = |text: &str| -> Option<String> { /* questionTerminal equivalent */ };
    let status = pi_coding_agent::cli::mcp_command::run_mcp_command(
        &raw[1..], // the leading "mcp" is stripped, as upstream does
        &pi_coding_agent::cli::mcp_command::McpCommandOptions {
            cwd: &cwd,
            agent_dir: &agent_dir,
            runtime: &runtime,
            open_url: &open_browser,
            interactive,
            prompt_redirect: Some(&prompt),
            output: &pi_coding_agent::cli::mcp_command::StdoutOutput,
        },
    );
    std::process::exit(status);
}
```

Imports (only if not already present):

```rust
use pi_coding_agent::cli::mcp_command::{McpCommandOptions, StdoutOutput, run_mcp_command};
```

`StdoutOutput` maps `log`→`println!` and `error`→`eprintln!`. A TUI-aware
caller can implement `McpOutput` instead; the trait has only `log(&self, &str)`
and `error(&self, &str)`.

### 3b. The `McpRuntime` implementation

The module defines the seam; the coordinator implements it with the real
wiring. Signatures (all methods take `&self`):

```rust
pub trait McpRuntime {
    fn oauth_url(&self, entry: &McpServerEntry, cwd: &Path, agent_dir: &Path) -> Option<String>;
    fn connect(&self, entry: &McpServerEntry, cwd: &Path, agent_dir: &Path) -> ConnectionSnapshot;
    fn close(&self, entry: &McpServerEntry, cwd: &Path, agent_dir: &Path);
    fn remove_credentials(&self, name: &str, url: &str) -> bool;
    fn sign_in(
        &self,
        name: &str,
        url: &str,
        timeout_ms: u64,
        show_authorization_url: &mut dyn FnMut(&str),
        prompt_redirect_url: &mut dyn FnMut() -> Option<String>,
    ) -> Result<usize, SignInError>;
}
```

Public data types the implementation receives/returns:

- `McpServerEntry { name: String, config: serde_json::Value, source: String,
  scope: Option<McpScope>, override_path: Option<String> }`
- `ConnectionSnapshot { state: String, tools: Vec<String>,
  has_resources: bool, error: Option<String> }` — `state` is one of the
  upstream strings `connected`, `needs-auth`, `failed`, `disabled`,
  `disconnected`, `closed`, `connecting` (`runtime.ts:328-447`).
- `SignInError::{Cancelled, Failed(String)}` — `Cancelled` must mean
  upstream's `McpSignInCancelledError` so the CLI prints the timeout message.

Implementation notes:

- **Config loading / add / remove are already ported inside `mcp_command.rs`**
  (`load_mcp_config`, `add_mcp_server_config`, `remove_mcp_server_config` from
  `config.ts`). The runtime does not need to touch `mcp.json` at all.
- `oauth_url` is purely derived from `entry.config`: an HTTP `url` with no
  `Authorization` header (case-insensitive) → `Some(url)`, else `None`
  (`runtime.ts:217`). `connection_snapshot`'s `oauth_url` field is not used by
  the CLI — it calls this method before connecting, like upstream.
- `connect` should use `pi_ext_js::mcp_manager::McpManager` (already wired for
  sessions) with the same cwd/agent_dir; `tools` are the discovered tool names,
  `error` is the connect failure text. Return `state: "needs-auth"` when the
  server answers 401 without `Authorization` (MCP OAuth). `close` calls
  `drop_server`.
- `remove_credentials(name, url)` deletes the stored OAuth credentials for
  that server/URL (`McpOAuthCredentialStore::remove`) and returns whether a
  record existed.
- `sign_in` runs `pi_mcp::oauth::sign_in::{start_mcp_sign_in,
  complete_mcp_sign_in}` against the server's challenge, calls
  `show_authorization_url(&href)` for the browser step (the CLI already logs
  `Sign in to MCP server "<name>" in your browser:\n<href>` and calls
  `open_url`), uses `prompt_redirect_url()` for the pasted redirect URL, then
  reconnects and returns the tool count. Map cancellation to
  `SignInError::Cancelled`, everything else to `SignInError::Failed(msg)`.
  A simpler first wiring may return `Err(SignInError::Failed("not
  implemented"))`; the CLI still prints upstream's exact failure string.

`providerToken` (upstream's `McpCommandOptions.providerToken`, used only to
read provider credentials for `auth.provider` servers) is not a separate
argument: login/logout reject `auth.provider` servers before connecting, and
`list` reports the provider name via `authProvider`. A coordinator that wants
provider-backed `list` connections can resolve tokens inside `connect`.

## 4. Deviation checklist for `HANDOFF.md`

- chalk → plain output (same convention as the package CLI).
- Project config path uses `.pi/mcp.json` (`PROJECT_CONFIG_DIR_NAME`), not the
  XDG leaf, matching the on-disk layout the port already uses.
- `serde_json::Map` sorts object keys; upstream's `JSON.stringify` preserves
  insertion order (file bytes and `--json` key order differ, semantics don't).
- JSON parse errors use `serde_json`'s wording, not V8's.
- The non-interactive `waitForRedirectUrl` arm returns immediately instead of
  waiting for the abort signal; the runtime's `timeout_ms` bounds the flow.
- The config validator and tool-exposure helpers are duplicated locally from
  `pi-ext-js::mcp_registry` because `pi-ext-js` is an optional feature and the
  command must compile under `--no-default-features`.
