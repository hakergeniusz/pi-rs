# Codemode host wiring — coordinator checklist

Slice: `feat/codemode-host`. Upstream surface ported:
`packages/coding-agent/src/extensions/codemode/{tool,execute,index,renderer}.ts`
(declarations, nested tool adapters, store folding, output formatting, error
text, discovery globals, exposure-gated registration).

## What landed here

- `crates/pi-coding-agent/src/codemode_host.rs` — self-contained host:
  - `CallableTool` / `Exposure` / `CodemodeMode` / `NamespaceInfo` /
    `CodemodeOptions`, plus `create_codemode_description` (budgeted,
    namespace-grouped tool sections) and `read_store` /
    `store_entry_data`;
  - `ToolInvoker` bridge and `execute_via_executor` adapter to pi-agent's
    `ToolExecutor` (blocking on the process-wide runtime, per the crate's
    synchronous VM seam);
  - `CodemodeHost::{new, tool_definition, description, execute_input,
    execute_code}` and minimal `ToolRenderers`;
  - 15 unit tests with synthetic tools/JS scripts.
- `crates/pi-coding-agent/src/lib.rs` — one added line:
  `pub mod codemode_host;`. This was necessary for the module to compile in the
  library crate (it uses `crate::session`, `crate::tools`, `crate::runtime`).
  It is the only edit outside the explicitly permitted file list.
- No edits to `Cargo.toml`, `Cargo.lock`, `main.rs`, `tools/index.rs`, or
  `extensions/mod.rs`.

## Dependency check (no update expected)

On branch base `ab6059b` both are **already** correct:

- `crates/pi-coding-agent/Cargo.toml:22` —
  `pi-codemode = { workspace = true }`;
- `Cargo.lock`, `pi-coding-agent` dependency list — contains `pi-codemode`.

The coordinator should re-check both after any merge/rebase. If the dep is
missing, add the workspace dep line and regenerate the lock entry (do not
hand-edit the lock's checksum block).

## What the coordinator must still verify / wire

The module is deliberately **not yet live** in the agent host: registering a
tool definition without an executor arm would make a `codemode` call fail with
`Tool codemode not found`. Complete these steps, then gate the full suite.

1. **Build a host in `build_runtime` (`src/main.rs:~335`)** after
   `select_tools` resolves the active set:
   - `CallableTool::from_definition` for each selected tool definition; set
     `exposure` from the MCP/tool exposure (`Exposure::parse`) and `namespace`
     from the MCP namespace where available.
   - `CodemodeHost::new(tools, invoker)` where `invoker` is an
     `impl ToolInvoker` that calls
     `pi_codemode_host::execute_via_executor(executor, name, args, call_id, signal)`
     on the **same** `Arc<dyn ToolExecutor>` the agent loop uses (so
     validation/hooks/permissions match direct calls).
   - Bind the session seams: `.with_store_reader(|| read_store(&session.get_branch(None)...))`
     and `.with_append_entry(|kind, data| session.append_custom_entry(kind,
     Some(data)))`. `SessionManager` needs `&mut` for the append, so use the
     host's existing session handle/borrow pattern; a `Mutex<SessionManager>`
     is the simplest fit.
   - Apply `CodemodeOptions`: `mode` from `codemode.mode` setting (upstream
     `on`/`only`, default `on`), `inline_budget` from
     `codemode.inlineBudget` (default 3000), `enabled` from the CLI tool gate.
2. **Tool registry (`src/tools/index.rs`) / executor dispatch**:
   - Add `host.tool_definition()` to the tool universe passed to
     `select_tools` **only** when the CLI activates it (`--tools +codemode`
     or an all-names allowlist containing `codemode`, or MCP
     `auto_enable_codemode`). Upstream registers it
     `defaultActive: false`, so it must not appear in the default set.
   - Teach `BuiltinToolExecutor::run` / `ExtensionAwareExecutor::execute`
     (`src/extensions/mod.rs:~542`) to short-circuit `name == "codemode"`
     into `CodemodeHost::execute_input(call.id, &arguments, None)` before the
     normal builtin dispatch. The host is `Clone`, so an `Arc<CodemodeHost>`
     can be shared with the runtime/UI.
3. **Exposure gating**: no `--no-codemode` flag exists upstream; the gates are
   `--no-tools`, `--no-builtin-tools`, `--tools`, `--exclude-tools` (already
   implemented in `modes/print_mode.rs::select_tools`) and MCP per-server
   `exposure` (`codemode`/`deferred`/`direct`/`hidden`, already normalized in
   `mcp_config.rs`). Feed the resolved `enabled` flag into `CodemodeOptions`,
   and rely on `CallableTool::is_callable()` / `Exposure` for which nested
   tools scripts can reach.
4. **Settings**: the port has no `codemode` settings block. Either add
   `codemode.mode` / `codemode.inlineBudget` to `config/settings.rs` (matching
   `settings-defaults.ts:57-60`) or accept the hard-coded defaults
   (`CodemodeMode::On`, 3000 tokens). This is a deliberate 1:1 gap until then.
5. **Run the gate** (no cargo was run in this slice):
   - `cargo test -p pi-coding-agent codemode_host` (15 tests),
   - then the full RAM-first gate from `AGENTS.md` (clippy all+pedantic
     `-D warnings`, fmt check, workspace tests),
   - fix any coordinator-side compile issues; the process note in
     `HANDOFF.md` expects one fix round per subagent slice.

## Intentional deviations from upstream 1.5.2

- **Synchronous VM / blocking invoker.** `pi_codemode::Tool::execute` is sync;
  nested calls block on the process runtime via `execute_via_executor`.
- **No worker pool / admission / `onUpdate` progress.** The in-process VM
  cannot stream nested-call rows; the final `details.calls` snapshot is
  equivalent. `PI_CODEMODE_MAX_WORKERS` and the 60 s worker wait do not apply.
- **No `models.*` globals.** The port has no classifier/image model registry
  seam; `CodemodeOptions.models` defaults to `false`. Do not declare it until
  a model runtime is wired.
- **No grammar-constrained sampling** for the codemode input (the port's
  `ConstrainedSampling` enum has no Lark variant).
- **Simplified `searchTools` ranking** — deterministic token overlap instead
  of upstream's BM25 (`tool-search/tool.ts`); names/validation/return shape
  match.
- **Minimal renderers** — plain-text previews instead of upstream's
  component/TUI renderers; the TUI bridging slice can replace
  `CODEMODE_RENDERERS` in one place.
- **Temp-file naming** uses `uuid` v4 with the requested extension (upstream
  uses `writeOutputFile`); both spill to the OS temp dir.
- **`memory_limit_bytes` = 256 MiB** (matches upstream) enforced by the
  in-process `rquickjs` runtime.
