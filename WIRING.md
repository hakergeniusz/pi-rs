# CJS interop slice — wiring notes (branch `feat/cjs-interop`)

## What landed

- **`crates/pi-ext-js/src/cjs.rs`** (new): format detection, the JS shim
  generator, and upstream's invalid-factory diagnostic.
- **`crates/pi-ext-js/src/resolver.rs`**: `ExtensionLoader::load` wraps CJS
  dependency files before `Module::declare`; `unavailable_module_message` is
  shared with the shim so a failed `require` and a failed `import` produce the
  identical error string.
- **`crates/pi-ext-js/src/loader.rs`**: `load_paths` prepares each entry via
  `cjs::entry_source` and maps `MissingFactory` to upstream's exact
  `Extension does not export a valid factory function: {path}`
  (`loader.ts:659`) — the engine's own error is lowercase (`error.rs:23`).
- **`crates/pi-ext-js/src/lib.rs`**: registers `pub mod cjs;`.
- **`crates/pi-ext-js/tests/cjs_interop.rs`** (new): 3 integration tests.
  `cjs.rs` adds 4 unit tests. Total: **7 new tests**.

## Host wiring needed

None for the normal CLI path: `pi-coding-agent` already loads extensions
through `pi_ext_js::loader::load_paths` (and `discover_and_load`), which now
handles CJS entries. **A direct `runner.load_extension(path, None)` bypasses
the entry wrapper** — `engine.rs` is off-limits to this slice, so that entry
point stays ESM-only. Callers using it directly must either:

```rust
let source = pi_ext_js::cjs::entry_source(path); // Some(wrapped) for CJS
runner.load_extension(&path.to_string_lossy(), source.as_deref())?;
```

or route through `loader::load_paths`. No change is needed in `engine.rs`,
`runner.rs`, `error.rs`, `Cargo.toml`, or `Cargo.lock`.

## Semantics covered

- `module.exports = function (pi) { … }` as an extension entry, including
  `require("typebox")` (virtual module) and `pi.registerTool` /
  `pi.registerCommand`.
- `exports.x = …` in CJS dependencies: `require("./helper.js")` returns the
  raw `module.exports` object through the wrapper's `__piCjsExports`
  binding (a `__piCjs` marker drives the interop); requiring an ESM
  dependency returns its namespace object. The `default` export stays
  factory-only so object exports cannot be mistaken for an extension
  entry.
- Static `require("literal")` calls are lowered to ESM imports, so they use
  the **same** resolver and virtual-module table as `import` statements,
  including relative paths and `node:*` shims.
- `__filename`, `__dirname`, `module.filename`, `module.paths`, and
  `module.require` are provided by the shim; a shebang is stripped.
- Non-function exports become `undefined`, so the engine's missing-factory
  path reports upstream's invalid-factory string via `load_paths`.
- Unresolvable `require` specifiers throw the resolver's exact ESM error
  text (including the `node:` branch).
- One failing CJS extension does not stop later ESM extensions
  (errors-as-values).

## Deviations / remaining gaps

- **Dynamic/computed `require(name)`** is not backed by a host `require` op
  (adding ops requires editing `engine.rs`, out of scope); it throws the
  resolved-module error. Only literal specifiers are lowered to imports.
- **`node_modules` resolution** remains unsupported for both formats, as in
  E1 (documented in `docs/EXTENSIONS.md` §6.3).
- **Circular CJS↔ESM requires** can hit ESM TDZ errors instead of Node's
  partial-exports behavior; jiti's full CJS cycle semantics are not ported.
- **Mixed modules** (both `export` and `module.exports`) are treated as ESM,
  matching jiti's native-ESM preference; a `require()` inside such a module
  is not shimmed.
- CJS modules run in ESM strict mode; sloppy-mode CJS side effects (implicit
  globals, `this === globalThis`) differ from Node.
- `.cjs` files are still not discovered — upstream's entry rules allow only
  `.ts`/`.js` (`loader.ts:748-750`).
- `JsHost::load_extension_module` with inline CJS source does not wrap
  automatically (see "Host wiring needed").
- `entry_source` transpiles once for detection and the ESM path transpiles
  again (the TS disk cache makes this cheap); a pure-JS cache for the
  wrapped form is a future optimization.

## Verification

Per slice rules no `cargo` command was run here. The coordinator's serial
host gate (`scripts/host-build.sh`) must run `cargo fmt --check`,
`cargo clippy --workspace --all-targets -- -D warnings` and
`cargo test --workspace -- --test-threads=4`; files were formatted with
single-file `rustfmt --edition 2024`.
