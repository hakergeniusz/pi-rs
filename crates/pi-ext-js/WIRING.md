# Wiring the remaining `@earendil-works/*` host packages (E4)

This slice was prepared on branch `feat/host-packages`. **It does not touch
`resolver.rs`, `engine.rs`, `node/mod.rs`, `lib.rs`, any existing `node/*.js`,
`Cargo.toml` or `Cargo.lock`.** The coordinator applies the small registration
patch below, then runs the normal gate.

## Files added

| Path | Purpose |
|---|---|
| `src/hostpkg/mod.rs` | `host_package_modules()`, `HOST_PACKAGE_SPECIFIERS`, `source()` + 6 unit tests |
| `src/node/host/pi-ai.js` | `@earendil-works/pi-ai` extension view (root `/compat` `/models` `/oauth` `/providers/all` `/utils/model-operations` `/utils/oauth-page`) |
| `src/node/host/pi-tui.js` | `Container`, `Spacer`, `Text`, `Input`, `SelectList`, `Box`, `HStack`, `Markdown`, `ScrollView`, `Loader`, `visibleWidth`, `truncateToWidth`, `Key`, … |
| `src/node/host/pi-agent-core.js` | `toolResult`, `textContent`, `imageContent`, `isToolResult`, `createToolResultMessage`, … (type-only surfaces absent) |
| `src/node/host/pi-codemode-source.js` | `CODEMODE_SOURCE_GRAMMAR` (byte-identical to `pi-codemode/src/source.rs:17-25`) |

## Specifier table

Every specifier below is served by `host_package_modules()`; the
`@mariozechner/*` aliases from upstream `virtual-modules.ts:34-41` are included.

```
@earendil-works/pi-ai                              -> pi-ai.js
@earendil-works/pi-ai/compat                       -> pi-ai.js
@earendil-works/pi-ai/models                       -> pi-ai.js
@earendil-works/pi-ai/oauth                        -> pi-ai.js
@earendil-works/pi-ai/providers/all                -> pi-ai.js
@earendil-works/pi-ai/utils/model-operations       -> pi-ai.js
@earendil-works/pi-ai/utils/oauth-page             -> pi-ai.js
@earendil-works/pi-tui                             -> pi-tui.js
@earendil-works/pi-agent-core                      -> pi-agent-core.js
@earendil-works/pi-codemode/source                 -> pi-codemode-source.js
@mariozechner/{pi-ai*,pi-tui,pi-agent-core,pi-codemode/source} -> same
```

## Exact registration steps

1. **`crates/pi-ext-js/src/lib.rs`** — declare the new module:

   ```rust
   pub mod hostpkg;
   ```

2. **`crates/pi-ext-js/src/resolver.rs`, `virtual_module_source`** — add the
   new table to the fallback chain, before `node::source`:

   ```rust
   _ => crate::hostpkg::source(specifier)
       .or_else(|| node::source(specifier)),
   ```

   (Equivalently, iterate `crate::hostpkg::host_package_modules()`; the
   `source()` adapter is provided so the existing `Option<&'static str>`
   signature needs no reshaping.)

3. **`crates/pi-ext-js/src/resolver.rs`, `is_unavailable_host_module`** — the
   new specifiers must no longer take the E1 rejection path:

   ```rust
   pub fn is_unavailable_host_module(specifier: &str) -> bool {
       !node::HOST_PACKAGES.contains(&specifier)
           && !crate::hostpkg::HOST_PACKAGE_SPECIFIERS.contains(&specifier)
           && (specifier.starts_with("@earendil-works/") || /* …unchanged… */)
   }
   ```

   If the coordinator instead extends `node::HOST_PACKAGES` with the new
   names, step 3 is unnecessary (but step 2 still is, because `node::source`
   does not serve them).

4. **`crates/pi-ext-js/tests/node_builtins.rs`** — two assertions pin the old
   E1 behaviour and must be updated when the packages become available:

   - `genuinely_unavailable_host_packages_keep_the_e1_error` uses
     `@earendil-works/pi-tui` as its example; switch it to a genuinely
     unserved specifier (e.g. `@earendil-works/pi-ai/providers/openrouter`
     or `@earendil-works/pi-codemode`).
   - `resolver_table_matches_the_documented_coverage` asserts
     `is_unavailable_host_module("@earendil-works/pi-tui")` and
     `virtual_module_source("@earendil-works/pi-ai").is_none()`; invert both
     (or iterate `hostpkg::HOST_PACKAGE_SPECIFIERS` and assert they are all
     served and not unavailable).

5. **No `engine.rs` change is required.** The shims use only the existing
   `getModel` op (installed by `HostActions::get_model`); they do not need new
   Rust ops and never reach into the process environment or filesystem.

6. Run the gate (host-side, `scripts/host-build.sh` per the current AGENTS.md).

## Tests

`src/hostpkg/mod.rs` contains six self-contained unit tests that build a
standalone QuickJS runtime with a loader serving *only* this table (plus the
E1 `typebox` shim), transpile a TypeScript fixture, and assert real runtime
behaviour:

1. `table_covers_every_documented_specifier` — table/source consistency.
2. `pi_ai_exports_model_helpers_and_typebox_view` — `getModel()` (from a stub
   `__piOps.getModel`), thinking levels, `modelsAreEqual`, `contentText`,
   `retryDelayMs`, `StringEnum`, `Type`, error results, OAuth HTML.
3. `pi_tui_constructors_render_and_helpers_work` — `Container.render`,
   `Spacer`, `Text`, `SelectList.onSelect`, `Input`, `Box`, `visibleWidth`,
   `truncateToWidth`, `Key`.
4. `pi_agent_core_tool_result_shapes_match_upstream` — result/content shapes.
5. `codemode_source_grammar_matches_the_rust_crate` — byte-for-byte grammar.
6. `type_only_imports_are_erased_and_harmless` — `import type` from all three
   packages disappears in transpile and leaves no runtime import.

They are independent of steps 1-4, so they pass even before the coordinator
wires the resolver. After wiring, add a host-level fixture test if desired
(the `tests/fixtures/node-builtins/index.ts` pattern is a good template).

## Remaining gaps and deviations from upstream 1:1

- **`@earendil-works/pi-ai`**: the composed model/provider catalog is
  Rust-owned, so only the *current* model crosses (`getModel()`); there is no
  JS catalog enumeration. `stream`, `streamSimple`, `completeSimple` and
  `lazyStream` are documented throwing stubs — a live model call must go
  through the port's Rust provider registry. Type-only exports (`Static`,
  `TSchema`, `Model`, `Message`, …) are intentionally absent at runtime, just
  as upstream's transpiler erases them. One JS file serves every `pi-ai`
  subpath (a superset of each), rather than upstream's separate entrypoints.
  Subpaths still unserved: `/api/*.lazy`, `/providers/openrouter`,
  `/providers/radius-config`, `/bedrock-provider`, `/bun-oauth`. The OAuth
  success/error pages carry upstream's titles and message text but not its
  exact CSS. `uuidv7` uses `Math.random` (no WebCrypto in this QuickJS).
- **`@earendil-works/pi-tui`**: data-shape constructors only. There is no
  terminal, no event loop and no real key dispatch in QuickJS; `render(width)`
  returns plain ANSI-preserving rows for a host-side painter. `Component` is
  exported as a concrete base class for convenience. Not ported: `Editor`,
  `SettingsList`, `TruncatedText`, theme objects (`getSelectListTheme`, …),
  `MouseRegion`, `ProcessTerminal`, native clipboard, markdown/latex
  internals, full `SelectList` mouse/theme behaviour. The bundled MCP UI also
  imports local components (`DynamicBorder`, `AuthUrlComponent`,
  `VisualLinePreview`) that are not part of `pi-tui` and stay host-owned.
- **`@earendil-works/pi-agent-core`**: no live `Agent`, loop or `runToolCall`;
  only result/content shapes and a local `setDefaultStreamFn` holder. All
  upstream type-only imports are harmless no-ops.
- **`@earendil-works/pi-codemode/source`**: only `CODEMODE_SOURCE_GRAMMAR`
  and `CODEMODE_OPTIONS_PREFIX`; `parseCodemodeSource` and
  `CodemodeSourceError` remain Rust-only in the `pi-codemode` crate.
- The `@mariozechner/*` aliases are included even though the task focused on
  `@earendil-works/*`, matching upstream `virtual-modules.ts:34-41`.
