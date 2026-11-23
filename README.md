# pi-rs

A Rust rewrite of [Pi](https://github.com/earendil-works/pi), the minimal,
extensible agent harness. The goal is **almost 1:1 parity** with upstream:
session JSONL v3, print/JSON/RPC output schemas and the project `.pi/` layout
stay compatible. One deliberate deviation keeps pi-rs safe to run beside a real
TypeScript Pi install: user-level files live under XDG directories with a
`pi-rs` leaf instead of `~/.pi`.

**Status: `v0.1.1`.** The core pipeline is proven end-to-end against a live
provider (`pi -p` prompt to answer, tool round-trips, JSON event mode), and bare
`pi` renders the upstream chat TUI, including the `/model`, `/trust`,
`/settings` and `/resume` dialogs and fullscreen mode (`--tui-mode regular`
opts out). The extensions arc (E0 to E4), MCP, OAuth login/refresh, telemetry,
image handling, codemode, `pi-env`, virtual-model routing, models.json custom
providers and the P7 tail have landed. What remains is mechanism-level
deviations, not missing headline features:

- Coverage matrix: [`docs/GAP-STATUS.md`](docs/GAP-STATUS.md)
- Phased plan and scope: [PLAN.md](PLAN.md)
- Dated detail and gotchas: [HANDOFF.md](HANDOFF.md)

## Highlights

- **A mode for every workflow:** streaming print mode (`-p`), JSONL events
  (`--mode json`), typed RPC (`--mode rpc`) and a full interactive TUI.
- **Upstream-shaped extensions:** JS/TS extensions run on an embedded QuickJS
  host with upstream dispatch semantics, including UI dialogs, renderers,
  tools, shortcuts, context transforms and the RPC dialog subprotocol.
- **Native Rust extensions:** a pi-rs addition that lets compiled plugins
  change almost any part of the host. See
  [Native Rust extensions](#native-rust-extensions).
- **Broad provider support:** Anthropic, OpenAI completions and Codex
  Responses, Google GenAI/Vertex, Bedrock Converse, Azure Responses, Mistral
  Conversations and pi-messages, plus built-in OAuth login/refresh for
  Anthropic, Codex and Copilot.
- **Compatible sessions:** JSONL v3 transcripts, byte-checked against golden
  fixtures from the real TypeScript Pi.
- **Safe coexistence:** XDG roots mean pi-rs never touches `~/.pi`.

## Install

### One-liner (Linux x86-64 / ARM64)

The repo is **public**, so no token is needed. One-liner (works in `sh`, `bash`
and `zsh` on Linux and macOS):

```sh
curl -fsSL https://raw.githubusercontent.com/hakergeniusz/pi-rs/main/scripts/install.sh | sh
```

Safer alternative - download, review, then run:

```sh
curl -fsSLO https://raw.githubusercontent.com/hakergeniusz/pi-rs/main/scripts/install.sh
sh ./install.sh --help
sh ./install.sh v0.1.1
```

The binary lands at `~/.local/bin/pi-rs` by default; override with `--prefix DIR`
(or `PI_INSTALL_DIR`). Each download is SHA-256 verified before install. No
`curl | sh`, no writes outside the prefix, and `sudo` only with `--sudo`. Use
`--force` to replace an existing install.

Current release: **v0.1.1**. Assets: `pi-linux-x86_64`, `pi-linux-aarch64`,
`pi-darwin-x86_64`, `pi-darwin-aarch64` and `pi-windows-x86_64`, each with a
`.sha256`. The tag workflow builds and verifies all five targets - the repo is
public, so the installer downloads anonymously.

### npm / Bun (planned, not published yet)

There is no pi-rs package on npm today, so these commands do **not** work yet.
The intended forms once a package is published are:

```sh
npx pi-rs             # planned
bunx pi-rs            # planned
npm install -g pi-rs  # planned
bun install -g pi-rs  # planned
```

Until then, use the installer above or [build from source](#build-from-source).
The upstream TypeScript package `@earendil-works/pi-coding-agent` is a
different program and is not pi-rs.

## Performance

Measured on the operator's box; treat these as indicative, not as a benchmark
suite.

| | pi-rs (release) | pi-rs (debug, PLT) | upstream pi (Node, v0.12.26) |
|---|---|---|---|
| Median time to first TUI frame | ~342 ms | ~694 ms | ~65 ms |
| Resident set size (RSS) | ~32 MiB | ~40 MiB | ~41 MiB |
| Binary | 26 MB, stripped | debug build | Node runtime |

pi-rs uses roughly **24% less memory** than upstream Node pi, but starts about
**5x slower**. These are different trades, not a speed win: Node benefits from
a warm V8 startup, while pi-rs spends its first few hundred milliseconds on
config, model catalog and session setup rather than on drawing the frame. The
release binary is stripped and takes about five minutes to build here.

## What works

- **`pi -p "prompt"`** prints a full agent-loop answer: streaming providers
  (anthropic, openai-completions, openai-codex-responses, Google GenAI/Vertex,
  Bedrock Converse, OpenAI/Azure Responses, Mistral Conversations,
  pi-messages), built-in tools (`read`, `write`, `edit`, `bash`, `grep`,
  `find`, `ls`) with themed renderers, and the credential ladder (`--api-key`
  to stored `auth.json` to provider env vars).
- **`pi --mode json`** emits an upstream-shaped JSONL event stream;
  **`--mode rpc`** offers a typed request/response envelope.
- **`pi` (interactive)** is the upstream chat TUI: logo header, streaming
  markdown, tool rows, usage/context footer, editor autocomplete (`/`
  commands, `@file` fuzzy search via `fd`, path completion), `/model`,
  ctrl+p model cycling, steering/follow-up message queues, `!`/`!!` bash mode,
  `/compact` context compaction, `/trust` project-trust dialog, the full
  `/settings` dialog, the OSC terminal-color theme controller, `/resume`
  (search, fork tree, rename, trash), `/name`, `/session`, `/hotkeys`,
  `/changelog`, `/copy`, and session persistence (shutdown prints the resume
  command).
- **Sessions** are JSONL v3 under the XDG data root
  (`~/.local/share/pi-rs/sessions` by default), byte-compatible with golden
  fixtures from the real TypeScript pi.
- **Wired library slices:** stored built-in OAuth credentials refresh at
  request time; `read`/`@file` attach images and resize/convert oversized and
  BMP images; `blockImages` is re-read per provider call in interactive and
  RPC (print/JSON snapshot it at startup); the anonymous install/update ping
  and exporter flush run at interactive startup/shutdown with opt-in HTTP
  transports; MCP startup/list-changed ticks are host-driven.

## Native Rust extensions

A pi-rs addition on top of upstream's JS/TS extension host: compiled `cdylib`
plugins can change almost any part of pi-rs, from tools to prompt and message
handling. This is deliberately **additive**; upstream pi has JS/TS extensions
only, and the stable **C ABI** is the boundary that keeps native plugins
decoupled from Rust internals.

- **Discovery:** point `PI_NATIVE_EXTENSIONS` at your plugin, or drop one in
  the project's `.pi/native/` directory.
- **Interface:** a small C ABI carrying JSON payloads in and out. No Rust
  types cross the boundary.
- **Registration:** plugins register tools and hooks, including
  `on_session_start`, `on_message`, `on_tool_result` and `transform_prompt`,
  so an extension can alter session startup, message flow, tool results and
  the prompt sent to the model.
- **Scope:** combined with the JS/TS extension host, this reaches tools, UI,
  providers, sessions and prompts, i.e. almost every part of pi-rs.

## Crates

| Crate | Port of | Status |
|---|---|---|
| `pi-schema` | `@earendil-works/pi-schema` | wire types |
| `pi-ai` | `@earendil-works/pi-ai` | types, model catalog, provider adapters (SSE streaming, incl. Google GenAI/Vertex, Bedrock Converse, OpenAI/Azure Responses, Mistral Conversations, pi-messages), built-in OAuth login/refresh |
| `pi-agent` | `@earendil-works/pi-agent-core` | agent loop with steering/follow-up queues, transcript, events |
| `pi-tui` | `@earendil-works/pi-tui` | differential renderer, editor, fuzzy/autocomplete, mouse, alt screen, theme, input listeners |
| `pi-mcp` | `@earendil-works/pi-mcp` | JSON-RPC types + stdio + streamable-HTTP transport clients (session/protocol-version headers, SSE parser + resumption, GET stream), the OAuth module (discovery, PKCE flows, sign-in, loopback callback), resources and fidelity APIs (startup wait, `list_changed` tool refresh, `_meta` preservation; ticks driven by the coding-agent host) |
| `pi-ext-js` | extensions runtime (`core/extensions`) | embedded QuickJS host: TS transpile, loader, runner, UI dialogs over protocol, `user_bash`, context transforms, RPC dialog subprotocol, virtual-model routing, extension keyboard shortcuts + markdown transformers + entry/message/tool renderers and components over the handle bridge, interactive `ctx.ui.custom` + the dialog stack + overlay handles, OSC 7501 program status + `ctx.ui.setStatus`/setTitle, the small `ctx.ui` surfaces (widgets, footer/header factories, working-status family, editor text ops, toolsExpanded, onTerminalInput), autocomplete wrappers (`addAutocompleteProvider` chain + trigger union), the editor component (`setEditorComponent`/`getEditorComponent` - the delegate editor slot + the status row), the `ctx.ui` themes API (`theme`/`getAllThemes`/`getTheme`/`setTheme` - the engine-side sync half, `themeChanged` crossings for the mode-side visuals + persistence, RPC/headless stubs verbatim) (`tool-search` exit gate green; E3 TUI bridging complete) |
| `pi-codemode` | `@earendil-works/pi-codemode` | inline VM, deterministic runs, `models.*` globals, grammar-constrained sampling hook, real local thread/client transports; true Node/Workers/wasm transports remain typed stubs |
| `pi-env` | `@earendil-works/pi-env` | daemon frame transport, remote env/files/process/PTY/watch, SSH; host-wired as `pi remote` (ticks/PTY/SSH onboarding are host-driven) |
| `pi-coding-agent` (`pi-rs` binary) | `@earendil-works/pi-coding-agent` | CLI, print/JSON/RPC/interactive modes, session store, tools, auth, images and telemetry modules |

Deferred (not in the core scope): `chord`, `pi-durable`, `evals`, `apps/p2`.
`/share` and `/bug` are omitted by policy (Radius upload).

## Build from source

Requires Rust 1.97 (MSRV), pinned by the oxc 0.158 transpile engine in
`pi-ext-js` (the workspace `Cargo.toml` `rust-version`).

```sh
cargo build
cargo test
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

The workspace gate (fmt, clippy with all targets and pedantic `-D warnings`,
full test suite) runs in CI on every push to `main`. Heavy builds should run
with low parallelism on memory-capped machines. The cargo build cache lives
under `/var/cache/zstd5` (`CARGO_TARGET_DIR=/var/cache/zstd5/cargo-target`);
`scripts/host-build.sh` defaults to it so recompiles stay fast without
polluting the repo. Interactive-mode tests need
`fd` and `rg` on `PATH` (installed by CI, resolved via `which()` like upstream's
`ensureTool`).

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
(upstream's override) collapses every role into one directory. With it set,
the layout is exactly upstream's. Project-local `.pi/` directories
(`.pi/settings.json`, `.pi/skills/`, ...) are upstream-compatible and
unaffected.

## Ground rules

- MIT licensed (original Pi by Mario Zechner is MIT; attribution preserved).
- Session files and CLI output schemas stay byte/shape-compatible with the
  TypeScript implementation; the user-level directory root is the one
  deliberate deviation (XDG, see [Paths](#paths)).
- Provider integration tests run only against free or local endpoints; test
  fixtures are synthetic and never touch real `~/.pi` credentials.
- Native Rust extensions are an additive capability; the C ABI is a deliberate
  stable boundary, and upstream pi has no equivalent.
