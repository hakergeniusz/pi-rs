# pi-rs

A Rust rewrite of [Pi](https://github.com/earendil-works/pi) — the minimal,
extensible agent harness — aiming for behavioral and on-disk compatibility
(session JSONL v3, print/JSON/RPC output schemas, project `.pi/` layout).

**Status: `v0.1.0`.** The core pipeline is proven
end-to-end against a live provider (`pi -p` prompt → answer, tool round-trips,
JSON event mode), and bare `pi` renders the interactive chat TUI ported from
upstream, including the `/model`, `/trust`, `/settings`, and `/resume`
selector dialogs and fullscreen mode (`--tui-mode regular` opts out).
Not at feature parity yet: the extensions headless core has landed
(`pi-ext-js` — embedded QuickJS host, TypeScript transpile, the extension
runner with upstream dispatch semantics; upstream's bundled `tool-search`
extension runs identically) together with the UI-over-protocol wiring
(dialogs, `user_bash`, context transforms, RPC dialog subprotocol),
virtual-model routing, models.json custom providers, and the composed
extension-provider catalog, extension-registered keyboard shortcuts
(`pi.registerShortcut`, resolved against the built-in keybindings like
upstream and listed under `/hotkeys`), extension markdown
transformers (`pi.registerMarkdownTransformer`, running per render on the
user/assistant/summary/changelog surfaces), extension entry
renderers (`pi.registerEntryRenderer` — custom session entries rendered
by extension components over the handle bridge, with the live theme
surface and keybinding hints), and extension message renderers
(`pi.registerMessageRenderer` — custom messages with a renderer-supplied
component or the default label+markdown box, also over the handle
bridge), and extension tool renderers (`pi.registerToolRenderer` —
resolver chains over the built-in renderers, custom components over
the handle bridge with `renderShell: "self"` support), and the
interactive `ctx.ui` dialogs with the real dialog stack
(`ctx.ui.select`/`confirm`/`input`/`editor` mount over the bridge with
countdowns, and `ctx.ui.custom` invokes its factory — the adopted
component stacks in the editor slot, `done()` resolves the promise),
the overlay arm (`ctx.ui.custom {overlay:true}` and the factory's
`tui.showOverlay` surface with the `OverlayHandle` adapter) over the
pi-tui overlay machinery, the OSC 7501 program-status reporter
(working/blocked/done/error/idle with dialog and overlay sources,
terminal titles) and `ctx.ui.setStatus` footer statuses, and the small
`ctx.ui` surfaces — widgets above/below the editor (`setWidget` lines
or component factories), custom footer/header factories with the live
footer-data adapter, the working-status family (`setWorkingMessage`/
`setWorkingVisible`/`setWorkingIndicator`/`setHiddenThinkingLabel`),
`pasteToEditor`/`setEditorText`/`getEditorText`, tool-output expansion,
`onTerminalInput`, and autocomplete wrappers (`addAutocompleteProvider`
chains over the built-in provider with the trigger union), and the
editor component (`setEditorComponent` — a factory-built component
takes the editor slot with the submit/change/autocomplete/action
wiring and the working status moving to the status row), and the
`ctx.ui` themes API (`ctx.ui.theme` styling, `getAllThemes`/`getTheme`
with by-handle styling adapters, `setTheme` by name — persisting
`settings.theme` — or by Theme instance as `<in-memory>`) — E3 TUI
bridging, the MCP streamable-HTTP transport and OAuth module, the npm
package-manager arm and the P7 tail have landed. The 2026-11-22 gap wave
added seven provider adapters (Google GenAI/Vertex, Bedrock Converse,
OpenAI/Azure Responses, Mistral Conversations, pi-messages), built-in
OAuth login/refresh for Anthropic/Codex/Copilot, the new `pi-env` crate,
the telemetry ping/exporter/conformance surface and a registered image
attachment module. A follow-up host-wiring wave made stored built-in OAuth
credentials refresh at wire time, attached images from `read`/`@file` and the
interactive `convertToLlm` filter, fired the telemetry ping/exporter, and drove
MCP startup/list-changed ticks. Remaining work is `pi-env` host wiring and
mechanism-level deviations; see
[`docs/GAP-STATUS.md`](docs/GAP-STATUS.md) for the coverage matrix,
[PLAN.md](PLAN.md) for the phased plan and [HANDOFF.md](HANDOFF.md) for the
detailed, dated status.

## What works

- **`pi -p "prompt"`** — print mode over the real agent loop: streaming
  providers (anthropic, openai-completions, openai-codex-responses, Google
  GenAI/Vertex, Bedrock Converse, OpenAI/Azure Responses, Mistral
  Conversations, pi-messages), the
  built-in tools (`read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`) with
  themed renderers, and the credential ladder (`--api-key` → stored `auth.json`
  → provider env vars).
- **`pi --mode json`** — JSONL event stream matching `json-event.ts`'s wire
  shape; **`--mode rpc`** — a typed request/response envelope (reconciliation
  against `rpc-entry.ts` pending).
- **`pi` (interactive)** — the chat TUI: pi logo header with ctrl+o
  expansion, streaming markdown, tool execution rows, footer usage/context
  stats, editor autocomplete (`/` commands, `@file` fuzzy search via `fd`,
  path completion), the `/model` selector dialog, ctrl+p model cycling,
  steering/follow-up message queues (enter / alt+enter / alt+up), `!`/`!!`
  bash mode, `/compact` context compaction (status indicator, queued
  submissions, ctrl+o expansion, billed-tokens notice), the `/trust`
  project-trust dialog (trust / trust parent / distrust, persisted to
  trust.json), the full `/settings` dialog (the complete upstream row
  list — auto-compact, image rows per terminal capability, editor/output
  padding, autocomplete, transport, HTTP idle timeout, cache warming,
  telemetry, trust, tree filter, warnings sub-menu, per-model thinking
  levels, TUI/fullscreen rows, theme — persisted to settings.json), the
  OSC
  terminal-color theme controller (grayscale system theme while the query is
  pending, re-theming on the reply, mode-2031 light/dark reports, `/theme`
  switcher), the `/resume` session selector (current-folder/all scopes,
  search with `re:` regex and `"phrase"` support, fork-tree threading,
  rename and trash-delete) with resumed transcripts replayed into the chat,
  the `/name`, `/session`, `/hotkeys`, `/changelog`, and `/copy` commands,
  and session
  persistence (shutdown prints the resume command).
- **Sessions** — JSONL v3 transcript store under the XDG data root
  (`~/.local/share/pi-rs/sessions` by default), byte-compatible and
  verified against golden fixtures generated by the real TypeScript pi.
- **Wired library slices** — stored built-in OAuth credentials (Anthropic,
  Codex, Copilot) refresh at request time; `read`/`@file` attach image content
  with upstream's notes and the interactive `blockImages` filter; the anonymous
  install/update ping and exporter flush run at interactive startup/shutdown
  (default transports are inert); MCP startup/list-changed ticks are host-driven.
  Print/JSON/RPC still lack the `blockImages` filter.

## Crates

| Crate | Port of | Status |
|---|---|---|
| `pi-schema` | `@earendil-works/pi-schema` | wire types |
| `pi-ai` | `@earendil-works/pi-ai` | types, model catalog, provider adapters (SSE streaming, incl. Google GenAI/Vertex, Bedrock Converse, OpenAI/Azure Responses, Mistral Conversations, pi-messages), built-in OAuth login/refresh |
| `pi-agent` | `@earendil-works/pi-agent-core` | agent loop with steering/follow-up queues, transcript, events |
| `pi-tui` | `@earendil-works/pi-tui` | differential renderer, editor, fuzzy/autocomplete, mouse, alt screen, theme, input listeners |
| `pi-mcp` | `@earendil-works/pi-mcp` | JSON-RPC types + stdio + streamable-HTTP transport clients (session/protocol-version headers, SSE parser + resumption, GET stream), the OAuth module (discovery, PKCE flows, sign-in, loopback callback), resources and fidelity APIs (startup wait, `list_changed` tool refresh, `_meta` preservation; ticks driven by the coding-agent host) |
| `pi-ext-js` | extensions runtime (`core/extensions`) | embedded QuickJS host: TS transpile, loader, runner, UI dialogs over protocol, `user_bash`, context transforms, RPC dialog subprotocol, virtual-model routing, extension keyboard shortcuts + markdown transformers + entry/message/tool renderers and components over the handle bridge, interactive `ctx.ui.custom` + the dialog stack + overlay handles, OSC 7501 program status + `ctx.ui.setStatus`/setTitle, the small `ctx.ui` surfaces (widgets, footer/header factories, working-status family, editor text ops, toolsExpanded, onTerminalInput), autocomplete wrappers (`addAutocompleteProvider` chain + trigger union), the editor component (`setEditorComponent`/`getEditorComponent` — the delegate editor slot + the status row), the `ctx.ui` themes API (`theme`/`getAllThemes`/`getTheme`/`setTheme` — the engine-side sync half, `themeChanged` crossings for the mode-side visuals + persistence, RPC/headless stubs verbatim) (`tool-search` exit gate green; E3 TUI bridging complete) |
| `pi-codemode` | `@earendil-works/pi-codemode` | inline VM, deterministic runs, `models.*` globals, grammar-constrained sampling hook; Node/Workers/wasm transports are typed stubs |
| `pi-env` | `@earendil-works/pi-env` | daemon frame transport, remote env/files/process/PTY/watch, SSH; library only, not yet consumed by the binary |
| `pi-coding-agent` (`pi` binary) | `@earendil-works/pi-coding-agent` | CLI, print/JSON/RPC/interactive modes, session store, tools, auth, images and telemetry modules |

Deferred (not in the core scope): `chord`, `pi-durable`, `evals`, `apps/p2`.
`/share` and `/bug` are omitted by policy (Radius upload); macOS release assets
await a hosted runner.

## Build

Requires Rust 1.97 (MSRV) — pinned by the oxc 0.158 transpile engine in
`pi-ext-js` (`rust-version` in the workspace `Cargo.toml`).

```sh
cargo build
cargo test
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

The workspace gate (fmt + clippy all+pedantic `-D warnings` + full test suite)
runs in CI on every push to `main`. Heavy builds should run in a
memory-capped container; the interactive-mode tests need `fd` and `rg` on
`PATH` (installed by CI, used via `which()` like upstream's `ensureTool`).

## Paths

pi-rs follows the [XDG Base Directory spec](https://specifications.freedesktop.org/basedir-spec/latest/)
under a `pi-rs` leaf, so it never touches a real TypeScript pi install
(`~/.pi`):

| Role | Default | Contents |
|---|---|---|
| config | `~/.config/pi-rs` | `settings.json`, `auth.json`, `models.json`, `themes/`, `prompts/`, `tools/`, skills, `trust.json` |
| data | `~/.local/share/pi-rs` | `sessions/` |
| cache | `~/.cache/pi-rs` | managed binaries (fd, rg) |
| state | `~/.local/state/pi-rs` | `pi-debug.log` |

Each root honors its `XDG_*` env var when absolute. `PI_CODING_AGENT_DIR`
(upstream's override) collapses every role into one directory — with it set,
the layout is exactly upstream's. Project-local `.pi/` directories
(`.pi/settings.json`, `.pi/skills/`, …) are upstream-compatible and
unaffected.

## Ground rules

- MIT licensed (original Pi by Mario Zechner is MIT; attribution preserved).
- Session files and CLI output schemas stay byte/shape-compatible with the
  TypeScript implementation; the user-level directory root is the one
  deliberate deviation (XDG, see [Paths](#paths)).
- Provider integration tests run only against free or local endpoints; test
  fixtures are synthetic and never touch real `~/.pi` credentials.
