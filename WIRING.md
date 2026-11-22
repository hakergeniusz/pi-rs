# E4 extension-host wiring — `node:*` builtins and host packages

This document records what the `feat/node-builtins` slice added, what still
needs host wiring, and the E4 gaps deliberately left open. It is written for
the next agent with fresh context; `HANDOFF.md` remains the dated status log.

## What landed

- **Virtual modules** in `crates/pi-ext-js/src/node/` served through
  `resolver::virtual_module_source`:
  - `node:fs`, `node:fs/promises`, `node:path`, `node:os`, `node:url`,
    `node:child_process`, `node:buffer`, `node:util`, `node:process`,
    `node:crypto`.
  - `@earendil-works/pi-coding-agent` (alias `@mariozechner/...`) and
    `@earendil-works/pi-mcp` (alias `@mariozechner/...`).
- **Rust host ops** installed by `resolver::node::install_ops` from
  `engine::JsHost::install_ops`, all reading the same `HostCore` the `pi`
  API uses. No Node process is embedded and no extension gets direct
  filesystem access from JS.
- **`@earendil-works/pi-codemode/declarations` is untouched** — it remains
  the E1 shim in `glue.rs`.
- Unknown specifiers, including `@earendil-works/pi-ai*`, `pi-tui`,
  `pi-agent-core`, `pi-codemode/source` and `pi-*`, keep the E1 rejection
  string verbatim.

## Host wiring obligations

1. **cwd is authoritative in `HostCore`.** `node:process.cwd()`,
   `node:path.resolve` and `node:child_process` read `HostCore::cwd`, not
   `std::env::current_dir()`. The embedder must keep `cwd` in sync with the
   session's cwd (the `cwd` field is already synced by `ExtensionRunner`).
2. **Session metadata is a snapshot.** `getSessionMetadata()` in the
   `@earendil-works/pi-coding-agent` shim reports `session_name`, `cwd`,
   `mode`, `thinking_level`, entry/label counts and tool counts from
   `HostCore`. Re-run `bind()`/snapshot sync after a `newSession`/`fork`/
   `switchSession` so a captured import sees fresh values.
3. **Tools come from the runner's aggregated state.** `getAllTools()` and
   `getActiveTools()` read `HostCore::all_tools`/`active_tools`; `setActiveTools`
   applies the existing registered/non-hidden filter. No new tool registry
   was introduced.
4. **Settings are live.** `getSettings()` reuses the existing `getSettings`
   op, so a settings change after load is visible without reload.
5. **Filesystem permissions mirror upstream.** Extensions run with the
   host's privileges; the trust gate in `loader.rs` still decides whether an
   extension loads. The new `node:fs` shim does not add a second sandbox.

## Deliberate deviations (add to `HANDOFF.md` when merging)

- `node:child_process`: only the sync calls and a shape-compatible `spawn`
  whose events fire after the engine drains its job queue; no real
  concurrent subprocess streams. `fork` throws.
- `node:path.win32` uses POSIX lexical semantics.
- `node:process.exit()` is a no-op and `chdir()` throws; the embedded host
  owns process lifetime and cwd.
- `process.env` filters keys containing `key`, `token`, `secret`, `password`,
  `credential` or `auth` so a fixture cannot leak a live credential into a
  transcript.
- `node:crypto`: `sha256`/`sha512` only; `createHmac` throws and
  `randomBytes` uses a non-cryptographic PRNG (only `randomUUID` is used by
  the bundled MCP flow).
- `node:util.TextEncoder`/`TextDecoder` delegate to the Buffer shim; no
  full-stream decode semantics.
- `Stats` exposes the common fields (`isFile`, `isDirectory`, `size`,
  `mtimeMs`, `mode`) and methods; other Node `Stats` members are absent.
- `node:url` provides a small WHATWG-shaped `URL`/`URLSearchParams`; it is
  not spec-complete.

## Remaining E4 gaps

1. **CJS interop** — ESM-only transpile path remains (`docs/EXTENSIONS.md`
   §6.3); `require()` in an extension still fails.
2. **`/reload` per-path unload** — `ctx.reload()` stales the runtime but the
   JS-side registries are not unloaded per extension path; module-cache
   invalidation is still process-lifetime.
3. **`node_modules` resolution** — bare specifiers outside the virtual
   table are rejected; no npm dependency resolution inside extensions.
4. **Host packages not bundled by the port** — `pi-ai`, `pi-tui`,
   `pi-agent-core`, `pi-codemode/source`, `pi-codemode/worker` keep the E1
   error. The Rust-native MCP/codemode/TUI hosts do not need them today.
5. **`Intl.Segmenter`** — still the E1 regex shim; unspaced scripts segment
   as one run (HANDOFF E1 deviation §1).
6. **Async `spawn` streaming**, real TTY flags, full `crypto`/`url`/`os`
   surface, Windows path semantics.
7. **Golden fixture parity** — run the upstream-generated extension
   fixtures through these shims when the P4 fixture set is extended for E4.

## Tests

- `crates/pi-ext-js/tests/node_builtins.rs` (3 tests): loads
  `tests/fixtures/node-builtins/index.ts` and asserts one representative
  result per module/package; pins the E1 rejection string for unavailable
  packages; checks the resolver table matches the documented coverage.
- `crates/pi-ext-js/src/node/mod.rs` (4 unit tests): lexical path semantics,
  Buffer encoding round-trips, Node-shaped `fs` error codes, and shim table
  completeness.
- The E1 engine test that expected `node:` imports to fail was replaced with
  one asserting supported imports resolve and unsupported ones reject.

Total: 7 new tests + 1 replaced test.
