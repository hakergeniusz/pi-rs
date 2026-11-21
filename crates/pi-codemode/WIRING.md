# Wiring `pi-codemode` into `pi-coding-agent`

This crate is now a real port of `@earendil-works/pi-codemode` 1.5.2
(`/tmp/pi/packages/codemode/src`, the pinned upstream at
`~/tmp-11-18/pi-152/packages/codemode/src`). It is self-contained: `cargo test
-p pi-codemode` runs the engine and the bridge round-trip tests with no host
wiring at all. This document lists what the coordinator still needs to do to
expose the codemode *tool* to the agent.

## 1. Dependency changes

**Root `Cargo.toml`: none.** `pi-codemode` was already a workspace member and
`rquickjs` was already a workspace dependency (added for `pi-ext-js`). This
crate only adds `rquickjs = { workspace = true }` to
`crates/pi-codemode/Cargo.toml` and drops the unused `pi-schema` / `tokio`
entries from the stub.

**`Cargo.lock`: none.** `rquickjs 0.14` (plus `rquickjs-core`, `-macro`,
`-sys`) is already locked through `pi-ext-js`. No new package or version is
introduced.

When the codemode tool is registered in `pi-coding-agent`, add
`pi-codemode = { workspace = true }` to that crate's `Cargo.toml`. The stub's
public constant `SANDBOX_VERSION` was removed; no current code references it
(`rg -n pi_codemode crates/pi-coding-agent/src` shows only MCP-config strings
named `codemode`, not Rust API uses).

## 2. What is exported

`crates/pi-codemode/src/lib.rs` re-exports the whole upstream-facing surface:

| Upstream | Rust |
|---|---|
| `types.ts` | `CodemodeTool` → `Tool` trait + `FnTool`/`tool()`; `CodemodeResult`, `CodemodeError`, `OutputItem`, `Call`, `CodemodeStoreWrites`, `CodemodeRun*`, `CodemodeOutcome`, … |
| `identifier.ts` | `to_codemode_identifier` |
| `declarations.ts` | `render_declarations`, `render_tool_signature`, `render_tool_sample`, `render_tool_output_type`, `schema_to_type`, `resolve_schema_ref`, `RenderTool`, `RenderToolOptions`, `DEFAULT_INPUT_SCHEMA_MAX_CHARS` |
| `source.ts` | `parse_codemode_source`, `CodemodeSourceError`, `CODEMODE_OPTIONS_PREFIX`, `CODEMODE_SOURCE_GRAMMAR` |
| `runtime/protocol.ts` | `VmStartData`, `WorkerToHostMessage`, `HostToWorkerMessage`, validators |
| `runtime/prelude-source.ts` | `PRELUDE_SOURCE` + the four `MAX_*` constants |
| `runtime/execution.ts`, `runtime/inline.ts` | `Sandbox`, `InlineCodemodeSandbox`, `SandboxOptions`, `ToolExecutor` |
| `runtime/vm.ts`, `runtime/inline.ts` | `run_vm`, `VmOptions`, `DEFAULT_INTERRUPT_BUDGET` |

## 3. Host wiring the coordinator must supply

The agent-side code that upstream keeps in
`packages/coding-agent/src/extensions/codemode/{tool,execute}.ts` is **not**
in this crate. It needs a live agent session (`ctx.executeTool`, model
registry, session store), so it remains host code. Concretely:

1. **Tool adapter.** For each callable agent tool, build a `pi_codemode::Tool`
   (or `tool(name, |args, ctx| …)` closure) whose `execute` calls
   `ctx.executeTool(name, args)`, maps `structuredContent`/text content to the
   returned `serde_json::Value`, and throws `ToolError` on failure. This is the
   `sandboxTools` mapping at upstream `execute.ts:525-552`.
2. **Globals.** Register `searchTools`, `describeTool`, `describeNamespace`,
   and (optionally) `models.*` as `Tool`s with `spread: true` and names like
   `models.classify` (upstream `execute.ts:669-850`). Namespaces are created
   automatically by `check_globals`.
3. **Store.** Pass the branch's `codemode-store` values as
   `ExecuteOptions.store` and persist `CodemodeResult::Ok { store_writes, .. }`
   as a session custom entry (upstream `execute.ts:292-303, 619-625`).
4. **Source options.** Call `parse_codemode_source(input.code)` first; apply
   `options.timeout_ms` to `ExecuteOptions.timeout_ms` and `max_output_tokens`
   to the host's output truncation.
5. **Result rendering.** `formatOutput`/`joinAdjacentText`/`truncateOutput`
   and the "Script completed/failed" header stay in the agent (upstream
   `execute.ts:322-472, 626-651`). `CodemodeResult` gives them `output`,
   `calls`, `storeWrites`, and `value`.
6. **Declarations for the tool description.** Feed each agent tool's metadata
   into `RenderTool` and call `render_tool_sample` / `render_declarations`;
   `CODEMODE_SOURCE_GRAMMAR` is the grammar-constrained sampling hint.

### Async tools

`Tool::execute` is synchronous because the embedded QuickJS engine runs
synchronously (the same constraint upstream solves with a worker thread).
Adapt an async agent tool with a blocking bridge, e.g.:

```rust
let runtime = tokio::runtime::Handle::current();
let tool = pi_codemode::tool("read", move |args, ctx| {
    let future = async { ctx_agent.execute_tool("read", args, signal).await };
    let value = tokio::task::block_in_place(|| runtime.block_on(future))?;
    Ok(value)
});
```

Run `Sandbox::execute` on a dedicated blocking thread if the agent loop must
stay responsive; that is the Rust analogue of upstream's worker thread.

## 4. Deviations from upstream (1:1 fidelity notes)

- **Engine/in-process.** Upstream runs `quickjs-wasi` in a Node worker (or as
  a wasm module). Reusing the `rquickjs` engine already embedded by
  `pi-ext-js` avoids a second JS runtime. The result is upstream's
  `InlineCodemodeSandbox` execution model: synchronous on the calling thread,
  with an interrupt budget (default 100,000 polls) and a wall-clock deadline
  that also covers synchronous compute. `pi-ext-js` does not re-export its
  engine, so this crate depends on the same workspace `rquickjs` crate rather
  than refactoring E1 (out of scope for this slice).
- **Bridge handles.** rquickjs ties JS handles to `Context::with`, but the
  job queue (`Runtime::execute_pending_job`) must be driven outside that lock.
  The prelude's `settle`/`stalled` callbacks are therefore stashed on
  `globalThis` under `__codemodeSettle`/`__codemodeStalled` for the duration
  of one execution and removed afterwards. Upstream captures them in a
  closure. A script that guesses those names could settle its own calls; the
  engine is already a tool-calling sandbox, not a cross-tenant security
  boundary, and the names are not enumerable in the declarations.
- **Omitted transports.** `CodemodeSandbox` (worker threads),
  `RemoteCodemodeSandbox`/`serveCodemodeRemote` (RPC isolates),
  `CodemodeSandboxBase::start()` (deterministic replay), wasm loading, and the
  Cloudflare/portable entry points are not ported. Their data types are kept
  so downstream code compiles, but no transport implementation exists. These
  require Node/`cloudflare:workers` hosts that do not apply to pi-rs.
- **Tool errors.** `ToolError` mirrors upstream's thrown `Error`; the
  `properties` map becomes JSON on the script's `Error` (except `name`,
  `message`, `stack`, which the prelude filters).
- **Unicode in identifiers / `$ref` decoding.** `to_codemode_identifier` uses
  Rust `char` iteration (Unicode scalar values), matching TS code-point
  iteration. `$ref` percent-decoding rejects malformed escapes in the same
  cases as `decodeURIComponent`.
- **`CodemodeResult` shape.** Rust uses an `Ok`/`Err` enum with the same
  fields; it is not serde-tagged as `{ ok: true|false }`. Host-side renderers
  pattern-match the enum.

## 5. Tests

`cargo test -p pi-codemode` (run by the coordinator's RAM gate) covers:

- identifier normalization and source-option parsing;
- schema → TypeScript rendering, `$ref` resolution, constraint tags, limits;
- protocol message validators and store/script-error decoding;
- **bridge round trips**: tool calls by normalized and raw name, `ALL_TOOLS`,
  globals (including spread and namespaces), `text`/`console`/`image`/
  `exit`, `store`/`load`, error propagation with properties, missing tools,
  syntax errors, interrupt-budget timeouts, abort, and closed sandboxes;
- tool registration validation (duplicates, reserved globals, namespace
  conflicts).

All tool definitions and scripts are synthetic; no real network, config, or
credentials are touched.
