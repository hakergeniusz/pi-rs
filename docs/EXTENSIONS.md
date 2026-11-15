# Extensions host — recon and design decision

Upstream ground truth (pi 1.5.2): `packages/coding-agent/src/core/extensions/`
(`types.ts` 2353 lines, `runner.ts` 1572, `loader.ts` 888, `index.ts` 229,
`virtual-modules.ts`, `wrapper.ts`, jiti loaders) plus the user-facing contract
in `packages/coding-agent/docs/extensions.md` (3367 lines) and four bundled
extensions (`src/extensions/{codemode,llama,mcp,tool-search}`). The Rust seam
lives in `crates/pi-coding-agent/src/extensions/mod.rs`.

This document is the recon deliverable PLAN.md §2 asked for: what the extension
system actually is, what the first Rust host must support, and which runtime to
build it on.

## 1. What the system is

An extension is a TypeScript module (loaded with jiti, so no build step) whose
default export is a factory `(pi: ExtensionAPI) => void | Promise<void>`. The
factory registers tools, commands, shortcuts, flags, renderers, providers, and
MCP servers, and subscribes to ~35 event types. The `ExtensionRunner` owns
dispatch; the host (agent-session / sdk / modes) installs hooks that call into
the runner at fixed points. Everything the API touches crosses the extension
boundary as JSON-able data or strings — there are no opaque host objects in
the core surface (the TUI surface is method-call bridging, see §7).

Key structural facts (citations are upstream 1.5.2):

- **Runner state** — `ExtensionRunner` holds `extensions: Extension[]`, the
  shared mutable `ExtensionRuntime` action table, a `ExtensionUIContext`
  (defaults to a complete no-op object for non-TUI modes, `runner.ts:331-362`),
  mode (`print|tui|rpc|json`), cwd, `SessionManager`, `ModelRegistry`.
  `bindCore()` copies ~30 host action closures into the runtime and flushes
  provider registrations queued during loading (`runner.ts:417-551`).
- **Extension object** — per-extension mutable registries: `handlers:
  Map<type, Handler[]>`, `tools: Map<string, RegisteredTool>`, `commands`,
  `flags`, `shortcuts`, `messageRenderers`, `entryRenderers`,
  `toolRenderers`, `markdownTransformer`, `path` (`types.ts:2325` area).
- **Staleness** — every context accessor calls `assertActive()`; session
  replacement/reload invalidates the runtime with a descriptive error
  (`runner.ts:729-742`). This is a semantic the port must reproduce.
- **Error model** — handler throws are caught per handler, reported through
  `ExtensionErrorListener` as `{extensionPath, event, error, stack}`
  (`runner.ts:744-753`); they never crash the host. `tool_call` throws block
  the tool; `user_bash` throws block the command.
- **Aggregation rules** — first registration per tool name wins across
  extensions (`runner.ts:637-647`); duplicate command names get `name:occurrence`
  invocation names (`runner.ts:821-855`); extension shortcuts lose to 18
  reserved built-in keybinding ids, with diagnostics (`runner.ts:98-117, 680-723`).

## 2. Event catalogue and dispatch semantics

Handlers run in extension load order, then registration order. Dispatch shape
differs per event — the per-event semantics are the part most likely to be
lost in a naive port, so they are tabulated here.

| Event | Runner method | Chaining / result semantics |
|---|---|---|
| `project_trust` | `emitProjectTrustEvent` | first `{trusted: yes\|no}` wins; `"undecided"` falls through (`runner.ts:302-329`) |
| `session_before_switch/fork/compact/tree` | `emit` | last handler result wins; `cancel: true` short-circuits (`runner.ts:1103-1132`) |
| `session_shutdown`, `session_start`, `session_info_changed`, `agent_start/end/settled`, `turn_start`, `message_start/update`, `tool_execution_*`, `model_select`, `thinking_level_select`, `mcp_servers_change`, `after_provider_response`, `provider_stream_event`, `ui_prompt_start/end` | `emit` | notification-only |
| `before_agent_start` | `emitBeforeAgentStart` | returned `message`s collected; `systemPrompt` override chains via `forceSystemPrompt`; `event.systemPrompt` getter re-renders per handler (`runner.ts:1434-1484`) |
| `agent_before_settle`, `turn_end` | `emitBoundary` | each handler sees prior `entries` draft + `continue`; context preview rebuilt after each handler; invalid preview discards everything (`runner.ts:1043-1092`) |
| `message_end` | `emitMessageEnd` | message rewrite chain; replacement must keep same role (`runner.ts:1158-1195`) |
| `tool_call` | `emitToolCall` | in-place `event.input` mutation visible to later handlers; `{block: true, terminate?}` short-circuits (`runner.ts:1256-1274`) |
| `tool_result` | `emitToolResult` | middleware chain over content/details/structuredContent/isError/usage; replacing content without structuredContent drops it (`runner.ts:1197-1254`) |
| `context` | `emitContext` phase 1 | sees non-system messages only (deep-copied); changed list gets prompt+tools replayed as one leading system message (`restoreSystemMessages`, `runner.ts:280-300, 1312-1341`) |
| `context_with_system` | `emitContext` phase 2 | sees full transcript; owns prompt+tools; dropping leading system message is reported but honored (`runner.ts:1343-1373`) |
| `before_provider_headers` | `emitBeforeProviderHeaders` | in-place header mutation; `null` deletes (`runner.ts:1406-1432`) |
| `before_provider_request` | `emitBeforeProviderRequest` | returns replacement payload (unknown JSON shape) (`runner.ts:1375-1404`) |
| `input` | `emitInput` | transform chain over text/images; `handled` short-circuits (`runner.ts:1532-1572`) |
| `user_bash` | `emitUserBash` | first valid result wins; `{operations}` XOR `{result}` shape-validated (`runner.ts:1276-1305`) |
| `cache_warming_decision` | `emitCacheWarmingDecision` | last `{action}` override wins (`runner.ts:1135-1156`) |
| `resources_discover` | `emitResourcesDiscover` | collects skill/prompt/theme paths with attribution (`runner.ts:1486-1529`) |

UI prompts (`ctx.ui.select/confirm/input/editor/custom`) are wrapped so that
`ui_prompt_start`/`ui_prompt_end` bracket nested prompts coalesced by depth
(`runner.ts:577-622`).

## 3. Host integration map

Where the host calls into the runner (all `packages/coding-agent/src`):

| Hook point | File:line | Event(s) |
|---|---|---|
| `agent.beforeToolCall` / `afterToolCall` | `core/agent-session.ts:717-788` (emit 731, 753) | `tool_call`, `tool_result` |
| turn boundary | `core/agent-session.ts:929, 2105` | `turn_end`, `agent_before_settle` |
| message persist | `core/agent-session.ts:1406` | `message_end` |
| run start | `core/agent-session.ts:2018` | `before_agent_start` |
| user input | `core/agent-session.ts:2134` | `input` |
| resources | `core/agent-session.ts:3457` | `resources_discover` |
| reload teardown | `core/agent-session.ts:3838` | `session_shutdown(reload)` |
| session switch/fork/compact/tree + teardown | `core/agent-session-runtime.ts:142,159,171,417` | `session_before_*`, `session_shutdown` |
| cache warmer / HTTP | `core/sdk.ts:338,355,377,382,394` | `cache_warming_decision`, `before_provider_headers`, `before_provider_request`, `after_provider_response`, `provider_stream_event` |
| context transform | `core/sdk.ts:431` (`transformContext`) | `context`, `context_with_system` |
| trust | `core/project-trust.ts:56` | `project_trust` |
| `!`/`!!` bash | `modes/interactive/interactive-mode.ts:7184`, `modes/rpc/rpc-mode.ts:583` | `user_bash` |

Extension-registered tools become agent tools through the same
`wrapToolDefinition` used by built-ins, with `createToolContext(toolCallId,
signal)` supplying the extension context + nested `executeTool`
(`core/extensions/wrapper.ts:17-29`, `runner.ts:975-1002`). Nested calls get
`<parentToolCallId>/<n>` ids, pass through `tool_call`/`tool_result` handlers,
do not add transcript entries, and roll their usage up to the calling tool.

## 4. The `pi` API surface

Inventory from `types.ts` (2353 lines; citations `T:` = types.ts, `VM:` =
virtual-modules.ts, `R:` = runner.ts).

### 4.1 `ExtensionAPI` methods (`T:1638–1967`)

| Group | Members | Notes |
|---|---|---|
| subscription | `on(event, handler)` — 37 typed overloads, sync or async, returns unsubscribe | handlers in load order then registration order |
| messaging | `sendMessage(msg, {triggerTurn?, deliverAs: "steer"\|"followUp"\|"nextTurn"})`, `sendUserMessage(content, {deliverAs?, expandPromptTemplates?})`, `appendEntry(customType, data?)` | appendEntry never reaches the LLM |
| tools | `registerTool(def)`, `getActiveTools()`, `getAllTools()`, `setActiveTools(names)`, `registerToolRenderer(resolver)` | setActiveTools ignores unknown/`hidden`; codemode/deferred stay callable |
| commands/flags | `registerCommand(name, opts)`, `getCommands()`, `registerShortcut(keyId, opts)`, `registerFlag(name, opts)`, `getFlag(name)` | duplicate command names get `name:occurrence`; flags carry values (default → CLI) |
| rendering | `registerMessageRenderer(customType, renderer)`, `registerEntryRenderer(customType, renderer)`, `registerMarkdownTransformer(fn, {messageTypes?})` | one markdown transformer per extension; surfaces: user/assistant/assistant-thinking/custom/summary/changelog |
| session metadata | `setSessionName`, `getSessionName`, `setLabel(entryId, label?)` | |
| process | `exec(cmd, args, {signal?, timeout?, cwd?, maxBuffer?})`, `getSettings()` | exec is the only process primitive; 256 MiB default buffer, truncation semantics |
| model | `setModel(model) → Promise<boolean>` (false = no auth), `getThinkingLevel()`, `setThinkingLevel(level)` (clamped) | session-only, recorded in history |
| providers | `registerProvider(name, config)` / `registerProvider(provider)`, `unregisterProvider(name)`, `registerVirtualModel(def)`, `unregisterVirtualModel(provider, id)` | queued during load, flushed at bindCore, immediate after; config supports `baseUrl` overrides, `$ENV` keys, `refreshModels`, OAuth, native `streamSimple` |
| MCP | `registerMcpServer(name, config)`, `unregisterMcpServer(name)`, `getMcpServers()` | session-scoped, not persisted; mcp.json same-name wins; a `mcp_servers_change` handler becomes the connector |
| misc | `events: EventBus` (extension-to-extension pub/sub) | |
| **absent** | **no `complete()`/sampling surface**; no session mutation outside command ctx; sessionManager on ctx is read-only | sampling-side power = providers/virtual models only |

### 4.2 Event payloads and results

37 event types in the `ExtensionEvent` union (`T:1426–1458`); the dispatch
semantics table is §2. Payload highlights the port must reproduce:
`tool_call`/`tool_result` are discriminated per tool name with typed `input`
and mutable in place (no re-validation after mutation, later handlers see it);
`context` excludes system messages (the host replays prompt+tools as one
leading system message after a changed list); `before_provider_request`
carries the raw provider payload as `unknown`; `input` carries `source:
"interactive"\|"rpc"\|"extension"` and `streamingBehavior`. Nested tool calls
get ids `<parentToolCallId>/<n>`, never enter the transcript, and are recorded
in the parent result's `nestedCalls` (bounded: 8 KiB args / 32 KiB result per
call, ≤256 records, `complete: false` when lossy).

### 4.3 Factory and loaded-extension shape

`ExtensionFactory = (pi: ExtensionAPI) => void | Promise<void>` (`T:2094`) —
awaited before `session_start`/`resources_discover`; provider registrations
queued in the factory are flushed at bind. Activation state lives in closures.
`InlineExtension` (`T:2096–2124`) adds `{name, hidden?, replaceable?, builtin?}`
— `replaceable` yields to same-name registrations (used by the bundled
extensions); `builtin:` loads after project trust and cannot handle
`project_trust`. Loaded record `Extension` (`T:2317–2333`): path, sourceInfo,
`handlers: Map<string, HandlerFn[]>`, `tools`, `commands`, `flags`,
`shortcuts`, `messageRenderers`, `entryRenderers`, `toolRenderers?`, exactly
one `markdownTransformer?`. No API-version constant exists anywhere —
compatibility is by convention plus module aliasing.

### 4.4 `ToolDefinition` (`T:600–689`)

`name, label, description, promptSnippet?, promptGuidelines?, parameters
(TSchema), constrainedSampling?, renderShell?: "default"\|"self",
prepareArguments?(pre-validation), outputSchema?, exposure?, namespace?,
reachesTools?(input) → "all"\|words, annotations? (MCP-style hints),
defaultActive?, prepareLoadout?(loadout) → {descriptions?, hiddenDeclarations?},
executionMode?: "sequential"\|"parallel", execute(toolCallId, params, signal,
onUpdate, ctx), renderCall?, renderResult?`.

`ToolExposure` (`T:526–537`): `direct` (declared+callable) | `model-only`
(declared, never callable) | `codemode` (always callable, declared only when
activated, listed by codemode) | `deferred` (callable, tool_search-findable,
not listed) | `hidden` (registered but unreachable — the only way to
"unregister", since tools cannot be removed).

### 4.5 Context hierarchy (`T:346–473`)

| Context | Grants |
|---|---|
| `ExtensionContext` (base) | `ui`, `mode`, `hasUI`, `cwd`, read-only `sessionManager`, `modelRegistry`, `model`, `scopedModels`, `thinkingLevel?`, `isIdle()`, `isProjectTrusted()`, `signal`, `abort()`, `hasPendingMessages()`, `shutdown()`, `getContextUsage()`, `compact(opts?)` (fire-and-forget, `onComplete/onError`), `getSystemPrompt()` |
| `ExtensionToolContext` | `+ tools: readonly AgentTool[]`, `executeTool(name, args, {signal?, onUpdate?})` — full validation/hooks path, never rejects (errors → `isError: true`) |
| `ExtensionCommandContext` | `+ getSystemPromptOptions()`, `waitForIdle()`, `newSession({parentSession?, setup?, withSession?})`, `fork(entryId, {position?, withSession?})`, `navigateTree(targetId, {summarize?, customInstructions?, replaceInstructions?, label?})`, `switchSession(path, {withSession?})`, `reload()` — session mutation is deliberately command-only (deadlock safety) |
| `ReplacedSessionContext` | `+ sendMessage/sendUserMessage` bound to the replacement session |
| `ProjectTrustContext` | reduced: `{cwd, mode, hasUI, ui: select/confirm/input/notify}` |

All contexts share the `assertActive()` staleness protocol: session
replacement or `/reload` invalidates the runtime and every captured `pi`/ctx
throws a descriptive error (`T:2213–2218`).

### 4.6 UI surface and mode matrix (`ExtensionUIContext`, `T:164–321`)

| Availability | Members |
|---|---|
| all modes (no-op headless) | `notify`, `setTitle`, `setStatus`, `setWidget` (string arrays in RPC; component factories TUI-only), `setEditorText`/`pasteToEditor` |
| TUI + RPC dialogs | `select(title, options, {signal?, timeout?})`, `confirm → boolean`, `input → string?`, `editor(title, prefill?)` — timeout auto-dismisses (undefined/false), `ui_prompt_start/end` bracket them |
| TUI + RPC no-op-ish | `setTheme` (RPC: `{success:false, "Theme switching not supported"}`), `getAllThemes → []` in RPC |
| **TUI-only** | `custom<T>(factory, {overlay?, overlayOptions?, onHandle?, blocked?})`, `onTerminalInput(handler)`, `setFooter/setHeader`, `setWorkingMessage/Visible/Indicator`, `setHiddenThinkingLabel`, `addAutocompleteProvider`, `setEditorComponent/getEditorComponent`, `getToolsExpanded/setToolsExpanded`, `theme` (readable anywhere, real only in TUI) |

RPC mode specifies the degradation contract as a JSON request/response
subprotocol (`docs/rpc-extension-ui.md`): dialogs become RPC round-trips,
`custom()` returns `undefined`, component factories are ignored. The Rust host
adopts this matrix verbatim rather than inventing one.

### 4.7 TS-only idioms with porting consequences

- **Virtual modules** (`VM:14–42`): extensions import `typebox` (+`/compile`,
  `/value`), `@earendil-works/pi-agent-core`, `pi-ai` (+`/compat`, `/models`,
  `/oauth`, `/providers/all`), `pi-tui`, `pi-coding-agent`, and legacy
  `@mariozechner/*`/`@sinclair/typebox` aliases; the host maps them all to
  bundled copies so extensions share class identity with the host. A JS host
  must reproduce this exact module table; module identity bugs are already a
  known hazard upstream (`KeybindingHints` exists because a second copy of
  pi-coding-agent misses host keybindings, `T:147–151`).
- **In-place mutation contracts** (headers, `tool_call.input`,
  `systemPromptOptions`): Rust needs `&mut` event plumbing or
  copy-in/copy-out semantics per event.
- **Dynamic typing edges**: `payload: unknown` replaceable wholesale,
  `details: unknown`, renderer `state: any` — fidelity requires keeping values
  as engine-side objects, not forcing everything through `serde_json::Value`.
- **Mixed sync/async handlers** and live streams (`streamSimple` returns a
  push-iterator `AssistantMessageEventStream`).
- **Power-user re-exports**: `pi-coding-agent` re-exports ~150 types plus
  runtime helpers and type guards; extensions reach deep host internals by
  import (`IX:7–229`).

## 5. Loading pipeline and the bundled extensions

Survey from `loader.ts` (888 lines), `package-manager.ts`, `resource-loader.ts`,
and the four bundled extensions (citations are 1.5.2 paths under
`packages/coding-agent/src`).

### 5.1 Discovery order (the real CLI path)

1. `--extension/-e <path|builtin:<name>|npm|git>` flags, repeatable
   (`cli/args.ts:186-189`); non-`builtin:` sources install into
   `<agentDir>/tmp/extensions` first (`package-manager.ts:271, 1061-1076`).
   `--no-extensions` disables discovery + built-ins but not explicit `-e`.
2. settings.json `packages` (project scope first, then user; npm/git installs,
   `package-manager.ts:995-1006`).
3. settings.json `extensions` as local paths relative to project `.pi/` or
   `~/.pi/agent/` (`package-manager.ts:1011-1037`).
4. Auto-discovery: project `.pi/extensions` **only after the project is
   trusted** (`package-manager.ts:2666, 2685-2693`); user
   `~/.pi/agent/extensions` always; one level deep, skip dotfiles and
   `node_modules` (`package-manager.ts:602-678`).
5. Built-ins registered as `builtin:<name>` synthetic paths unless the
   `extensions` setting has `-builtin:<name>` overrides
   (`package-manager.ts:1041-1056`).

Entry rules: `.ts`/`.js` only (no `.mjs`/`.cjs`); a directory is a package.json
`pi.extensions: string[]` manifest if present, else `index.ts` → `index.js`
(`pi-manifest.ts:4-42`). Pre-trust bootstrap pass forces `projectTrusted=false`
so only global/CLI extensions can handle `project_trust`
(`resource-loader.ts:533-553`).

### 5.2 Loading mechanics

- **jiti 2.7.0** — runtime Babel TS/ESM transform; lazily imported as
  `"jiti"` (normal Node) or `"jiti/static"` (compiled/bundled), selected by
  runtime detection: Bun / Node SEA / `PI_BUNDLED_NODE`
  (`loader.ts:44-57`). Instance: `createJiti(import.meta.url,
  {moduleCache: false, ...})` (`loader.ts:588-591`).
- **Three resolution modes** (`loader.ts:583-587`): embedded runtime →
  `virtualModules` table; TS-source runtime → `virtualModules` +
  `tsconfigPaths`; unbundled dist → `alias` map (same specifiers resolved to
  real paths). The embedded mode is the proof that a non-Node host can serve
  the exact same extension import surface.
- **Module cache**: in-memory, keyed by cwd + generation token; cleared on
  reload; no fs watchers anywhere in the loading path — `/reload` is manual
  (`loader.ts:136-158`; zero `fs.watch` hits).
- **Factory lifecycle** (`loader.ts:253-277, 542-560`): per-extension API with
  loading → active/failed states; commit flushes queued flag defaults and
  provider/MCP/virtual-model registrations; discard unsubscribes load-time
  event-bus subscriptions. Runtime action stubs throw "Extension runtime not
  initialized" until `bindCore`.
- **Load errors are values, not throws**: per-path failures return in
  `errors[]` and surface as CLI diagnostics; runtime handler errors fan out
  through `ExtensionError` listeners (`loader.ts:646-668, 712-715`;
  `main.ts:861-864`).
- **Replaceable built-ins**: if a non-replaceable extension already registered
  the same tool/command/flag name, a `replaceable` built-in (codemode,
  tool-search, mcp) is dropped with a warning (`resource-loader.ts:143-179`)
  — that is the extension-overrides-builtin mechanism; tools themselves
  cannot be unregistered, only withdrawn via `exposure: "hidden"`.
- **Hot reload** (`interactive-mode.ts:6617-6700`, `agent-session.ts:3828-3875`):
  `session_shutdown(reload)` → `oldRunner.invalidate()` → settings reload →
  re-discovery → new ExtensionRunner → `session_start(reload)`.

### 5.3 Bundled extensions (`src/extensions/index.ts:7-14`)

| Ext | Purpose | `pi.*` used | Events | TUI |
|---|---|---|---|---|
| codemode | `codemode` tool: the model runs TS scripts against tools in a **worker subprocess**; inactive by default | `registerTool`, `appendEntry`, `getAllTools`, `getSettings` | none | no |
| llama.cpp | provider router + `/llama` model manager (HF download/load/unload) | `registerProvider`, `registerCommand` | none | **yes** (guarded `ctx.mode !== "tui"` early-return) |
| mcp | connects `mcp.json` + `pi.registerMcpServer` servers; tools `mcp__server__tool` with exposure variants; OAuth; `/mcp` manager; resource tools | `registerToolRenderer`, `registerTool`, `getMcpServers`, `getActiveTools`/`setActiveTools`, `getAllTools`, `registerCommand` | `session_start`, `before_agent_start`, `tool_call`, `turn_start`, `mcp_servers_change`, `session_shutdown` | optional (manager TUI-only; connect/report headless) |
| tool-search | `tool_search` tool: BM25 over deferred tool metadata, activates matches; inactive by default | `registerTool` (+ uses `getActiveTools`/`getAllTools`/`setActiveTools` in-body) | none | no |

**De-facto core subset** (the minimum a Rust host must expose to host the
bundled set 1:1): `on` (+`events` bus), `registerTool`, `registerCommand`,
`registerToolRenderer`, `registerProvider`, `registerMcpServer`,
`getMcpServers`, `getAllTools`, `getActiveTools`, `setActiveTools`,
`getSettings`, `appendEntry`; ctx: `ui.notify/select/confirm/input/custom/
setStatus`, `modelRegistry.getProviderAuth/refresh`, `cwd`, `signal`,
`isProjectTrusted`, `mode`, `hasUI`.

### 5.4 Node-only dependencies the Rust host replaces

| Dep | Replacement |
|---|---|
| jiti (Babel TS transform + ESM/CJS interop + node_modules resolution) | **oxc TS strip + engine module loader (E1: landed, `pi-ext-js/transpile.rs` + `resolver.rs`)** |
| `createRequire`/`import.meta.resolve`/specifier canonicalization | path resolution in the module loader |
| Bun/SEA/bundled-Node runtime detection | build-mode constant (we are always the embedded case) |
| sync fs discovery (`readdirSync`/`statSync`/`existsSync`) | `std::fs` |
| `setImmediate`, `AbortSignal.timeout/any`, `structuredClone` | tokio timers, `AbortSignal` port, serde clone |

## 6. Runtime decision

**Decision: embedded JS engine — `rquickjs` 0.14 (QuickJS-NG) — behind a
feature-gated host crate, phased per §8. Originally `deno_core` (V8) with
`rquickjs` as the fallback; flipped at the E0 spike (2026-11-15) when
Cloudflare's acquisition deprecated the Deno runtime and Bun proved
non-embeddable (§6.2). The spike validated the engine in-container
(measurements in HANDOFF.md).**

### 6.1 Why an embedded engine at all

- The ecosystem is TypeScript by contract: discovery loads `.ts`/`.js`
  (§5.1), settings accept npm/git **packages** (§5.1 step 2), and upstream
  ships its own built-in extensions as TS factories (§5.3). A Rust-native
  plugin trait cannot load any of that — it is a different product.
- Upstream already runs this exact architecture in production: the
  compiled-binary runtime serves extensions from the `virtualModules` table
  with jiti-static — "Modules available to extensions in source and compiled
  binary runtimes" (`virtual-modules.ts:14`). A Rust host with an embedded
  engine is the same shape: native core + bundled virtual modules + TS
  transpile at the edge.
- `codemode` is JS-in-sandbox by definition (§5.3); without an engine it has
  no 1:1 port. The MCP extension's tool-bridge and `tool_search` likewise
  assume a JS host for their tool bodies.
- The API surface is boundary-friendly (verified across §2/§4): event
  payloads, results, tool args/results, provider payloads are JSON-shaped;
  UI components cross as strings (`render(width) → string[]`,
  `handleInput(data: string)`, `done(value)`); the RPC degradation matrix
  already specifies a JSON protocol for everything that is not a live
  component (§4.6).

### 6.2 Why `rquickjs` over the alternatives

| Option | Verdict | Reason |
|---|---|---|
| **`rquickjs` 0.14 (QuickJS-NG) + TS transpiler** | **chosen** | Embeddable as a library (the gate Bun fails), MIT, active (0.14.0, 2026-09), QuickJS-NG backend. Light where it matters for this port: E0 measured a 29 s cold release build at `-j2` on the memory-capped machine, ~4 MiB runtime RSS, and 1.7 MiB of binary weight (`libquickjs.a` 2.3 MiB) versus V8's projected 30–60 MiB. Slower than V8 and needs hand-written glue (bootstrap globals, job draining, value marshalling), but the extension API crosses the boundary as JSON/strings (§6.1), so the glue is written once, not per event. |
| `deno_core` (V8) | fallback (was chosen at recon) | tokio-native, mature op framework, prebuilt V8 static libs — but Cloudflare acquired Deno (2026-10-09) and deprecated the runtime (monthly fixes until ~2027-10, then EOL), removing the maintenance guarantee the recon relied on. Remains the escape hatch if QuickJS hits a fidelity wall (debugger story, ES2023+ gaps). |
| Bun (embedded) | rejected | No embeddable library API exists: oven-sh/bun#12017 is open, the Anthropic Rust rewrite (merged 2026-05) ships none, and the only embedding crates (`bun_runtime` 0.6.0) are weeks-old community projects. Embedding Bun today would mean the subprocess model rejected below. Watch item: an official Bun embedding API would be revisited before E2. |
| Rust-native trait (seam as-is) | rejected | incompatible with the TS ecosystem and with 1:1 fidelity; survives only as the seam the JS host plugs into (§7). |
| Subprocess host (node/deno child) | rejected | ships a JS runtime anyway, adds a second IPC protocol over a deeply bidirectional API (sync-ish UI calls, streaming, component bridging), and buys isolation upstream explicitly does not have (extensions run with full user permissions; trust is decided before load). |

### 6.3 Known costs and mitigations

- **Binary size / build**: measured at E0 — `libquickjs.a` is 2.3 MiB, the
  stripped spike example binary 1.7 MiB, cold release build 29 s at `-j2`
  under the 4 GiB container cap. Mitigation kept: `pi-ext-js` stays a
  workspace member, and `pi-coding-agent` gets the dependency behind the
  `ext-js` feature at E1 wiring so the lean build remains possible.
- **node built-ins**: extensions may import `node:fs`, `node:path`, …
  (upstream docs line 184). No embedded engine ships them (`rquickjs`
  included). Mitigation: start with the subset the bundled extensions and
  the top examples use (`node:fs`, `node:path`, `node:os`,
  `node:child_process` subset, `node:url`), grow on demand; record gaps as
  deviations.
- **MSRV/toolchain**: resolved at the spike — `rquickjs` 0.14 declares
  `rust-version = "1.87"`, so the workspace floor moved 1.85 → 1.87 (CI runs
  `@stable`; recorded in HANDOFF.md).
- **No built-in event loop**: `QuickJS` is single-threaded with a job queue;
  the host drains `Runtime::execute_pending_job()` after every engine entry
  (the E0 spike pattern). Upstream factories may return promises and
  handlers may be async — E1 decides between `rquickjs`'s futures support
  and an explicit job-queue shim.
- **jiti parity**: jiti does Babel TS/ESM/CJS interop + `tsconfigPaths` +
  node_modules resolution. Our loader needs a TS-stripping transpiler
  (`swc` or the lighter-to-build `oxc`; picked in E1) + a module
  resolver covering the virtual-module table (§4.7), relative/absolute specifiers,
  and extension `node_modules`. ESM-only first (upstream extensions are ESM;
  CJS interop deferred and recorded if it bites).
- **In-place mutation semantics** (§4.7): implement per-event as
  copy-in/copy-out with explicit "later handlers see earlier writes" chaining
  inside the engine boundary (JS objects stay live across the handler chain
  within one dispatch — the engine makes this natural, unlike a Rust trait).

### 6.4 Architecture

New crate `crates/pi-ext-js` (working name), feature `ext-js` on
`pi-coding-agent`:

```
┌─ pi-coding-agent (native core, all modes) ─────────────────────┐
│  ExtensionRegistry / ExtensionRunner (Rust port of runner.ts)  │
│        ▲                                                       │
│        │ impl Extension (one per loaded module)                │
│  pi-ext-js: JsExtension                                        │
│   - rquickjs Runtime (QuickJS-NG), single-threaded per host    │
│   - module loader (E1): swc/oxc TS strip + virtual-module table│
│   - ops: pi.* API (registration, exec, settings, events bus)   │
│   - host callbacks: events in → JSON results out; UI via       │
│     mode-gated bridge (no-op json/print, RPC subprotocol, TUI) │
└────────────────────────────────────────────────────────────────┘
```

The Rust `ExtensionRunner` port owns dispatch semantics (§2) for *all*
extensions — native `InlineExtension`-style factories included — so the JS
boundary only ever carries: registration calls, event dispatch (JSON payload
in, JSON result out), tool execution (async op with `signal` + `onUpdate`),
and UI bridge calls. Component-bearing calls (`renderCall`, widgets as
factories, `custom()`) are TUI-phase (§8) and cross as opaque component
handles whose `render/handleInput/invalidate` are engine round-trips.

## 7. Gap analysis vs the current Rust seam

**E1 status (2026-11-15):** the real runner now exists —
`crates/pi-ext-js/src/runner.rs` ports every §2 dispatch family, and the
items below describe what that port covers. The seam
(`crates/pi-coding-agent/src/extensions/mod.rs`) is still the *only*
extension surface the `pi` binary knows; replacing it with the E1 runner
(feature `ext-js`) is the first E2 item, so the gauntlets below remain as
written until that wiring.

The seam (`crates/pi-coding-agent/src/extensions/mod.rs`) models the easy 20%:
a static `Extension` trait (`tools()/commands()/flags()/handle_event()`), an
aggregating registry, and unknown-flag claiming. Gaps against §2/§3:

1. **Event model** — one `handle_event(&AgentEvent) -> EventOutcome` vs ~35
   typed events, each with its own chaining/result semantics (§2). `AgentEvent`
   is the agent-loop output stream, not the extension event set; the port needs
   its own `ExtensionEvent` enum with typed payloads and result types.
2. **Tool registration** — `ExtensionTool` lacks: `signal`/abort, `onUpdate`
   progress callback, nested `executeTool`, `tools` listing, `details`-typed
   results, `terminate`, `usage` rollup, exposure/annotations/namespace,
   `defaultActive`, `prepareArguments`, `executionMode`, first-wins by name.
3. **Commands** — no command context (`waitForIdle`, `newSession/fork/
   switchSession/navigateTree/reload`, `withSession` staleness semantics), no
   `name:occurrence` disambiguation.
4. **Missing registration surfaces** — shortcuts/keybindings, message/entry/
   tool renderers, markdown transformers, MCP server registry, provider and
   virtual-model registration, flags-with-values, `pi.events` bus, settings
   access, skills/prompt/theme discovery.
5. **Lifecycle** — no runner-level staleness invalidation, no error listener
   plumbing, no UI-context no-op/mode gating (`hasUI`, `ctx.mode`).
6. **What survives** — the `Arc<dyn Fn>` executor signature, the registry
   fan-out shape, and the flag-claiming flow are keepers; the rest of the seam
   is scaffolding to build the real thing on, not a constraint.

## 8. Phasing

Each phase is one or more topic branches with the full container gate; the
fidelity rule applies per phase (deviations recorded as they appear).

- **E0 — spike (no API): DONE** (2026-11-15, `feat/e0-js-host-spike`). The
  engine flipped to `rquickjs` 0.14 before the spike ran (Deno deprecation +
  Bun non-embeddable, §6.2); the spike proved the full round trip
  in-container — ESM default-export factory → `pi.on`/`registerCommand` →
  Rust-backed `getConfig` op → emit fan-out in registration order — with 3
  tests green, a 29 s cold release build at `-j2`, ~4 MiB runtime RSS, a
  1.7 MiB stripped example binary, and MSRV moved 1.85 → 1.87.
  *Notes: HANDOFF.md.*
- **E1 — headless core: DONE** (2026-11-15, `feat/e1-ext-core`, commits
  `42b3342` + `1b020f7`). The Rust `ExtensionRunner` port (every §2 family:
  notifications, `session_before_*`, `tool_call`, `tool_result`, `input`,
  `before_agent_start`, `message_end`, `turn_end`/`agent_before_settle`
  boundaries, `project_trust`), the loader (discovery order, `pi.extensions`
  manifest, trust split, errors-as-values), the de-facto core subset of §5.3
  (renderers/providers surface as documented E2/E3 errors), contexts (§4.5
  base/tool/command + the reduced trust context, headless UI matrix), tools
  with exposure and first-wins aggregation, `name:occurrence` commands,
  flags with values, `pi.exec`, and the MCP registration bridge
  (`mcpNamespace` + `validateMcpServerConfig` ported). TS transpile:
  **oxc** (§6.3 pick resolved — lighter to build under the 4 GB cap).
  Async: the job-queue shim (§6.3's other pick — async factories, async
  handlers, and async `pi.exec` settle exactly where upstream awaits them).
  *Exit met*: the bundled `tool-search` extension vendored **verbatim**
  loads through the full pipeline and runs identically — BM25 ranker,
  tokenizer, `$ref` schema text, and end-to-end execution all ported from
  upstream's own tests and green. The eight deviations (Intl shim, no
  node built-ins / node_modules, unbound nested `executeTool`, no
  `ctx.signal`, simplified `setActiveTools`, live-registry dispatch
  snapshots, reduced `ToolInfo`, ESM-only) are itemized in HANDOFF.md.
- **E2 — UI over protocol + host wiring**: dialogs/notify/setStatus/setWidget/setTitle via
  the no-op matrix headless and the RPC subprotocol in RPC mode (§4.6);
  `ui_prompt_start/end`; providers/virtual models incl. OAuth-config;
  `session_before_*` gates; `user_bash` interception. Exit: RPC-mode
  extension dialogs match upstream's rpc-extension-ui contract; codemode E1
  port decided (needs worker-subprocess JS execution).
- **E3 — TUI bridging**: shortcuts, message/entry/tool renderers, markdown
  transformers, widgets-as-components, `ctx.ui.custom` with overlay handles,
  editor component, autocomplete, themes; component handles crossing as
  `render/handleInput/invalidate` round-trips. Exit: llama.cpp `/llama`
  manager and a renderer extension behave as in upstream TUI.
- **E4 — completeness**: node built-in polyfill growth to the upstream
  documented set, npm-dependency resolution inside extensions, `/reload`
  lifecycle + staleness invalidation end-to-end, event-bus auto-unsubscribe
  semantics. Exit: golden extension fixtures generated by the real TS pi
  (shared with the P4 fixture work) pass.

Backlog order: **E2 UI over protocol + host wiring** next (E0 and E1 done
2026-11-15). The 69-item
doc remainder and P5 rpc-envelope reconciliation stay queued behind E1.
