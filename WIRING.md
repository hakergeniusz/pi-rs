# Wiring the MCP config slice (`feat/mcp-config`)

The new module is `crates/pi-coding-agent/src/mcp_config.rs`. It is standalone:
it compiles against existing deps (`serde`, `serde_json`, `dirs` via
`crate::config`) and adds no `Cargo.toml` changes. I did **not** run any cargo
command (per the slice's hard rule); the coordinator must gate it.

## 1. `crates/pi-coding-agent/src/lib.rs`

Add the module export (the file already has `pub mod config;` and a
`//!` layout list — add one line to each):

```rust
/// MCP `mcp.json` discovery, validation and the `/mcp` + `pi mcp` snapshot
/// (`extensions/mcp/config.ts`, `mcp/src/config.ts`, `core/mcp-servers.ts`).
pub mod mcp_config;
```

## 2. `crates/pi-coding-agent/src/main.rs` — bootstrap

Two consumers, two insertion points (I did not touch this file):

### `pi mcp` CLI (early subcommand route)

Next to the existing `handle_package_command` call at `main.rs:43`, add an
early dispatch before `parse_args` / mode startup, mirroring upstream
`cli.ts:224-258`:

```rust
if let Some(code) = pi_coding_agent::cli::mcp_command::handle_mcp_command(&raw) {
    std::process::exit(code);
}
```

`cli::mcp_command` is a *new* module the coordinator owns (outside this
slice). Its `list` arm should build the snapshot from the already-resolved
`AgentDir` + `ProjectTrustStore`:

```rust
// upstream cli.ts:234-245
let agent_dir = pi_coding_agent::config::get_agent_dir();
let project_config_path = cwd.join(pi_coding_agent::config::PROJECT_CONFIG_DIR_NAME).join("mcp.json");
let project_trusted = pi_coding_agent::cli::ProjectTrustStore::new(&agent_dir).get(cwd) == Some(true);
let loaded = mcp_config::load_mcp_config(&mcp_config::LoadMcpConfigOptions {
    agent_dir: agent_dir.clone(),
    cwd: cwd.to_path_buf(),
    project_trusted,
});
// When !project_trusted && project_config_path.exists(), print upstream's note:
// "{path} is ignored because the project is not trusted. Start pi in the project to trust it."
```

For a plain `pi mcp list` from the current directory, the shortcut is
`mcp_config::load_mcp_config_for_cwd(cwd, project_trusted)` (it resolves the
agent dir through `config::get_agent_dir()`).

### Interactive session start (`/mcp` selector)

Upstream loads the snapshot on `session_start` (`mcp/index.ts:1115-1129,1344-1346`).
In this port, resolve it once in `build_runtime_with_ui` (`main.rs:295+`),
where `agent_dir` (line 302) and the final `project_trusted` are available,
then expose it through the mode options / `PrintRuntime` the same way other
session-scoped artifacts are threaded. The call:

```rust
let loaded_mcp = mcp_config::load_mcp_config(&mcp_config::LoadMcpConfigOptions {
    agent_dir: agent_dir.clone(),
    cwd: cwd.to_path_buf(),
    project_trusted: /* final trust ladder value */,
});
```

Keep `loaded_mcp.errors` as the MCP config error list (`configErrors`
upstream) and surface it through the same warning path other startup
diagnostics use. Default `auto_enable_codemode` to `true` at the use site when
`loaded_mcp.auto_enable_codemode` is `None` (upstream `index.ts:1119`).

## 3. Consuming the snapshot (selector + CLI `list`)

Public shape (all fields are public):

```rust
pub struct LoadedMcpConfig {
    pub servers: Vec<McpServerEntry>,
    pub auto_enable_codemode: Option<bool>,
    pub errors: Vec<String>,
    pub project_config: Option<String>,
}
pub struct McpServerEntry {
    pub name: String,
    pub config: McpConfigObject,          // serde_json::Map<String, Value>
    pub source: String,                   // defining mcp.json (or extension path)
    pub scope: McpServerScope,            // Global | Project | Extension
    pub override_path: Option<String>,    // project file overriding a global entry
}
```

- **Selector rows**: iterate `loaded.servers` in order (global first, project
  entries/overrides in place). A server is enabled unless
  `entry.config.get("enabled") == Some(Value::Bool(false))`. Use `scope` for
  the scope label (`global` / `project` / `extension`) and `source` /
  `override_path` for the file to show. Transport description is upstream's
  `cli.ts:120-123`: if `config.contains_key("url")` show the `url` string,
  else join `command` + `args`. Auth provider is `config["auth"]["provider"]`
  for HTTP entries.
- **Scope/precedence**: a project entry with the same name has *replaced* the
  global one in the vector (only one entry per name). A project-only override
  keeps the global `config` with only `enabled` / `exposure` /
  `toolExposure` replaced, and sets `override_path` to the project file.
- **`autoEnableCodemode`**: pass `loaded.auto_enable_codemode.unwrap_or(true)`
  to the codemode activation decision (`index.ts:499-532`).
- **`errors`** are already fully formatted with the offending path prefix;
  render them as-is.
- **`project_config`** is `Some` only for a trusted project; it is where
  `/mcp` writes project overrides (`index.ts:663,832`).

## 4. Saving from `/mcp` and `pi mcp add|remove`

The module also ports the `editMcpServers` write helpers (upstream throws;
the port returns `Result<_, String>` with the same message wording):

```rust
pub fn update_mcp_server_config(
    path: &Path,
    name: &str,
    patch: &McpServerConfigPatch,
    options: &UpdateMcpServerConfigOptions,
) -> Result<(), String>;

pub fn add_mcp_server_config(path: &Path, name: &str, config: &McpConfigObject)
    -> Result<bool, String>;          // bool = replaced an existing entry

pub fn remove_mcp_server_config(path: &Path, name: &str) -> Result<bool, String>;
```

`McpServerConfigPatch { enabled: Option<bool>, exposure: Option<McpExposure> }`;
`UpdateMcpServerConfigOptions { create_override: bool }` (set `true` for a
project override of a global server, upstream `index.ts:307`). Writes preserve
the file's detected indentation and unknown top-level keys, and create the
file/parent dirs when missing.

Validation before `add`: call
`mcp_config::validate_mcp_server_config(name, &serde_json::Value) -> Result<McpConfigObject, String>`;
`Err` carries the upstream error string verbatim.

## 5. Deviations to record in `HANDOFF.md` (coordinator's file)

1. Global file stays `<agentDir>/mcp.json`, but `agentDir` comes from
   `config::get_agent_dir()` (`$PI_CODING_AGENT_DIR`, else
   `$XDG_CONFIG_HOME/pi-rs`, else `~/.config/pi-rs`) instead of `~/.pi/agent`.
   Project file remains `<cwd>/.pi/mcp.json`.
2. JSON parse errors carry `serde_json` wording, not V8's `JSON.parse` wording.
3. URL checks use a minimal in-module `parse_url` (no `url` crate dependency).
4. `McpServerScope` is a non-optional enum (`Extension` added); upstream's
   `scope` is an optional string tag. The loader only emits `Global`/`Project`.
5. Every fallible write returns `Result<_, String>` where upstream throws.
