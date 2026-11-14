# pi-rs

A Rust rewrite of [Pi](https://github.com/earendil-works/pi) — the minimal,
extensible agent harness — aiming for behavioral and on-disk compatibility
(session JSONL v3, print/JSON/RPC output schemas, `~/.pi` config directories).

**Status: pre-alpha.** The core pipeline is proven end-to-end against a live
provider (`pi -p` prompt → answer, tool round-trips, JSON event mode), and bare
`pi` renders the interactive chat TUI ported from upstream. Not at feature
parity yet: selector dialogs beyond `/model`, extensions, compaction, and the
MCP tool bridge are still landing. See [PLAN.md](PLAN.md) for the phased plan
and [HANDOFF.md](HANDOFF.md) for the detailed, dated status.

## What works

- **`pi -p "prompt"`** — print mode over the real agent loop: streaming
  providers (anthropic, openai-completions, openai-codex-responses), the
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
  bash mode, and session persistence (shutdown prints the resume command).
- **Sessions** — JSONL v3 transcript store under `~/.pi`, byte-compatible on
  the synthetic-fixture surface (golden fixtures from the TS pi still pending).

## Crates

| Crate | Port of | Status |
|---|---|---|
| `pi-schema` | `@earendil-works/pi-schema` | wire types |
| `pi-ai` | `@earendil-works/pi-ai` | types, model catalog, provider adapters (SSE streaming) |
| `pi-agent` | `@earendil-works/pi-agent-core` | agent loop with steering/follow-up queues, transcript, events |
| `pi-tui` | `@earendil-works/pi-tui` | differential renderer, editor, fuzzy/autocomplete, mouse, alt screen, theme |
| `pi-mcp` | `@earendil-works/pi-mcp` | JSON-RPC types + stdio transport client (HTTP transport + tool bridge pending) |
| `pi-codemode` | `@earendil-works/pi-codemode` | stub |
| `pi-coding-agent` (`pi` binary) | `@earendil-works/pi-coding-agent` | CLI, print/JSON/RPC/interactive modes, session store, tools, auth |

Deferred (not in the core scope): `chord`, `pi-durable`, `apps/p2`.

## Build

Requires Rust 1.85 (MSRV).

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

## Ground rules

- MIT licensed (original Pi by Mario Zechner is MIT; attribution preserved).
- Session files, CLI output schemas and config dirs stay byte/shape-compatible
  with the TypeScript implementation.
- Provider integration tests run only against free or local endpoints; test
  fixtures are synthetic and never touch real `~/.pi` credentials.
