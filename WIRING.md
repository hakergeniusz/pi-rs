# Wiring: per-path extension unload on `/reload`

This document is for the coordinator integrating the `feat/reload-unload`
slice. The slice is confined to `crates/pi-ext-js/src/runner.rs` (+ its tests);
no other crate file was changed.

## What landed

`ExtensionRunner::reload(&mut self, paths: &[PathBuf]) -> LoadExtensionsResult`
in `crates/pi-ext-js/src/runner.rs` implements upstream's `/reload` extension
half:

- `clearExtensionCache()` + fresh-runner semantics
  (`core/resource-loader.ts:541-545`, `core/agent-session.ts:3826-3849`,
  `core/extensions/loader.ts:145-151`): every discovered path is
  **re-evaluated from source**, and no registration from the previous
  evaluation survives — handlers, tools, commands, flags, shortcuts,
  markdown/entry/message/tool renderers, provider/virtual-model registrations,
  and MCP servers/registrations.
- The stale-ctx protocol is applied to the outgoing runtime first
  (`core/extensions/loader.ts:200-205`, `runner.ts:729-737`).
- MCP connections are shut down and the provider/MCP registries are cleared,
  so the caller's existing post-reload `bind()` + `session_start(reload)` +
  `connect_mcp_servers()` sequence rebuilds them from the new evaluation.
- Host-facing state carries over: settings, cwd, mode, flag values, active
  tool names, host actions, error listeners, UI sink, and the MCP manager
  (with its auth dir).

**Host-facing API is stable**: the return type is the already-exported
`pi_ext_js::loader::LoadExtensionsResult` (same type `load_paths` returns), so
no `lib.rs` re-export is needed. `reload_with_sources` is a private helper used
only by unit tests.

## Exact call-site change in `pi-coding-agent`

Only **one** function needs editing: `crates/pi-coding-agent/src/extensions/mod.rs`,
`pub fn reload(...)` (the `#[cfg(feature = "ext-js")]` one). Replace the
"already-loaded minus" filter and `load_paths(&remaining)` with a full-path
`runner.reload(&paths)`:

```diff
-    // The final pass: everything trusted allows, minus already-loaded
-    // modules (`loadFinalExtensionSet`, `resource-loader.ts:791-833`).
-    let loaded: HashSet<String> = runner.extension_paths().into_iter().collect();
-    let remaining: Vec<PathBuf> = discovered
+    // Upstream clears the extension factory cache and re-evaluates every
+    // file on /reload (`resource-loader.ts:541-545`); no loaded-set filter.
+    let paths: Vec<PathBuf> = discovered
         .iter()
         .filter(|entry| project_trusted || entry.origin != pi_ext_js::ExtensionOrigin::Project)
         .map(|entry| entry.path.clone())
-        .filter(|path| !loaded.contains(&path.to_string_lossy().into_owned()))
         .collect();
-    let result = load_paths(&mut runner, &remaining);
+    let result = runner.reload(&paths);
     for load_error in &result.errors {
         diagnostics.push(error(format!(
             "Failed to load extension \"{}\": {}",
             load_error.path, load_error.error
         )));
     }
```

Notes:

- Keep `use pi_ext_js::loader::{discover_cli_paths, discover_extensions,
  load_paths};` as-is — `load_paths` is still used by `load_with_engine` in the
  same file. (`HashSet` may become unused in this function; check the rest of
  the file before removing its import.)
- **Leave the rest of the sequence unchanged.** Specifically:
  - `runner.core().lock().settings = Value::Object(settings.get_settings());`
    before the reload is fine — `reload()` carries the core (and its settings)
    into the fresh runtime.
  - `runner.bind()`, `runner.emit(session_start reason:"reload")`,
    `runner.connect_mcp_servers()`, and the provider-registration-error report
    all stay after the reload exactly as they are today.
  - `interactive_mode.rs` already calls `runner.invalidate(None)` before
    `extensions::reload` (lines 8213-8221). `reload()` also invalidates
    (idempotent), so no change is needed there.
- The coordinator must **not** filter by `runner.extension_paths()` anymore:
  the whole point is that every path re-evaluates, including ones that were
  already loaded before `/reload`.

## Deviations / implementation notes

1. **Runtime rebuild instead of a jiti module-cache clear.** The port has no
   jiti `extensionCache`; it reproduces upstream's behaviour by rebuilding the
   `JsHost` (fresh QuickJS runtime/context, hence empty `__piExtensions` and
   module registry) over the carried `HostCore`, then re-evaluating each path.
   Upstream builds a brand-new `ExtensionRunner`; the port rebuilds the host in
   place and re-attaches the host-facing bindings.
2. **Old captured contexts are unreachable, not merely stale.** Upstream keeps
   the old runner object alive with a stale message; the port drops the old JS
   heap, so any captured `pi`/ctx from the previous evaluation cannot be
   invoked at all. Observable behaviour is the same (using it throws / does
   nothing), but the mechanism differs.
3. **Provider + MCP registries are cleared.** Upstream's fresh runner starts
   with neither; the re-evaluated extensions rebuild them at load/bind. If a
   future host registers providers directly into `HostCore.providers` (none do
   today — all writes go through extension-driven engine ops), the coordinator
   must re-register them after `reload()`.
4. **`session_shutdown(reason:"reload")` is still emitted by the caller**, not
   by `reload()`; this matches upstream's split (`agent-session.ts:3828`).
5. **`mem::take` on `HostCore`**: `HostCore` is not `Clone`, so the core value is
   moved out of the shared mutex into the fresh runtime. Runtime-creation
   failure is OOM-class; in that case `reload()` returns per-path error values
   and leaves the runner headless (with a default core) rather than panicking.
6. **No signature change for callers**: `reload(&[PathBuf])` returns the same
   `LoadExtensionsResult` used by `load_paths`. No `lib.rs`, `Cargo.toml`, or
   other-crate change is required.

## Verification

Four unit tests were added to `runner.rs`'s `mod tests`:

- `reload_drops_handlers_removed_from_source` — a handler present in v1 is
  absent after re-evaluating v2; no stale empty records remain.
- `reload_re_registers_changed_metadata` — tool/command names and descriptions
  from the new source replace the old ones; no stale tools stay active.
- `reload_preserves_stale_ctx_protocol_and_fresh_contexts` — a captured ctx
  throws after `invalidate`, while the re-evaluated handler runs normally.
- `reload_drops_mcp_registrations_and_connections` — a registered MCP server,
  its connection, and its bridged tools are all gone after re-evaluation.
