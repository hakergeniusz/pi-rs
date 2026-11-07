# pi-rs — Rust Rewrite of earendil-works/pi

Source surveyed: `/tmp/ai-agent/pi` @ `085c84e` (GitHub `earendil-works/pi`, MIT).
This document is the working plan. Implementation starts only after user approval.

## 1. Survey of the original

Pi is a minimal, extensible agent harness: multi-provider LLM API, agent loop with
tool calling, JSONL session tree, built-in coding tools, extensions/skills/themes,
and four interfaces (interactive TUI, print, JSON, RPC).

npm workspaces, TypeScript, ~236K LOC (`*/src`, ts/tsx):

| Package | LOC | Role |
|---|---|---|
| `coding-agent` | 78.5K | CLI: config, sessions, tools, extensions, modes (two monster files: `agent-session.ts` 166 KB, `interactive-mode.ts` 253 KB) |
| `chord` | 56.3K | Object runtime across processes (plugins/hot-reload, store, links, HTTP, workers) |
| `durable` | 34.1K | Durable agent harness on chord (experimental) |
| `ai` | 28.9K | Unified multi-provider LLM API + model catalog (~60 provider files) |
| `tui` | 20.0K | Terminal UI framework, differential rendering, incl. native modules |
| `schema` | 4.9K | JSON-schema builders/validators |
| `mcp` | 4.7K | MCP client support |
| `codemode` | 3.0K | JS-in-sandbox tool execution |
| `agent` | 2.6K | Agent loop, transport, state |
| `env` | 2.6K | misc |
| `telemetry` | 1.6K | vendor-neutral telemetry contracts |
| `apps/p2` | 31.2K | Separate app built on chord (server/web/tui hosts) |

Dependency order (build order): telemetry → schema → chord → tui → codemode → mcp
→ ai → durable → env → agent → coding-agent.

Core concepts that must survive the rewrite (from `docs/how-pi-works.md`,
`session-format.md`): session = JSONL tree (`id`/`parentId`, v3 format, migration
v1→v3); active branch = model history; agent loop = request → stream → tool calls →
results → next turn; steering vs follow-up queue; compaction as summary entries;
extensions register tools/commands/shortcuts/providers/event handlers/renderers;
prompt templates, skills, themes, packages; project trust before project resources.

## 2. Scope decision (needs user input)

Full 1:1 parity of all 13 workspaces is a multi-month effort. Recommended:

- **In scope (core product)**: `schema`, `ai`, `agent`, `tui`, `mcp`, `codemode`,
  `env`, `telemetry` (subset), `coding-agent` — the thing users install as `pi`.
- **Deferred, decide later**: `chord` + `durable` (would be a separate Rust
  runtime project; only needed for p2 and durable conversations), `apps/p2`.
- Node-based **extension system**: TypeScript extensions cannot run in a Rust
  process natively. Options (decide by Phase 5): (a) embedded JS engine (deno_core
  / boa / quickjs) running the same extension API, (b) Rust-native plugin trait +
  WASM, (c) subprocess extension host. Recommendation: (a) via `deno_core` for
  compatibility with the existing extension ecosystem.

## 3. Rust architecture (cargo workspace `pi-rs`)

```
crates/
  pi-schema      JSON value + schema builders + validation (serde_json, custom)
  pi-ai          types, streaming, providers, model catalog (serde, reqwest+rustls,
                 tokio, eventsource-stream, futures)
  pi-agent       agent loop, tool trait, state, steering/queue (tokio)
  pi-tui         terminal backend + diff renderer + components (crossterm,
                 own diff renderer mirroring pi-tui semantics)
  pi-mcp         MCP client (rmcp or hand-rolled JSON-RPC over stdio/HTTP)
  pi-codemode    JS sandbox for code mode (deno_core / quickjs)
  pi-coding-agent binary `pi`: config, sessions, tools, extensions, modes,
                 package manager (clap, serde_json, JSONL, syntect, similar,
                 ignore+grep-searcher for grep tool, globset, walkdir, uuid,
                 chrono, dirs, minimal-lexical…)
```

Key tech choices: tokio runtime; `reqwest` with rustls (no openssl pain);
streaming via SSE decoding per provider; `serde` untagged enums for message
content blocks; session JSONL with `serde_json::Value` passthrough for unknown
fields (forward compat); crossterm + hand-rolled differential renderer (ratatui
does not match pi-tui's line-diff/main-screen model); `syntect` for highlight.js
equivalent; `similar` for the edit tool's diffs; `ignore`/`grep-searcher`
(ripgrep crates) for the grep tool; `arboard`-free, no clipboard dependency
unless needed.

Compat commitments: session format v3 byte-compatible (same JSONL), `--print`,
`--json`, `--rpc` output schemas identical, model catalog consumption of the same
pi.dev catalog data, config dirs `~/.pi/agent/...` shared.

## 4. Phases (each ends with a committed, working milestone)

- **P0 — Skeleton**: cargo workspace, lint CI, crate stubs, README, LICENSE (MIT),
  this plan archived. Exit: `cargo build && cargo clippy -D warnings` green.
- **P1 — pi-ai core**: types (messages, content blocks, usage, stop reasons),
  `StreamFn` contract, SSE plumbing, model catalog + `models.generated` data
  import (build-time codegen from the TS repo's JSON). Exit: stream a completion
  from ≥1 OpenAI-compatible and ≥1 Anthropic endpoint in a test.
- **P2 — pi-agent**: agent loop (turns, tool execution incl. sequential/parallel,
  steering/follow-up queues, abort), state management, hooks
  (before/afterToolCall, transforms). Exit: scripted fake-model test runs a
  multi-turn tool workflow.
- **P3 — pi-tui**: terminal, keys, diff renderer (main + alt screen), core
  components (Text, Input/Editor, Markdown, Loader, SelectList, Box/VStack…),
  autocomplete, keybindings. Exit: demo app renders + resizes flicker-free.
- **P4 — coding-agent core (non-interactive)**: config loading, session manager
  (JSONL v3 + migrations), built-in tools (read, bash, edit, write, ls, find,
  grep, truncate, renderers), system prompt assembly, context files, skills,
  prompt templates, auth storage. Exits: print mode `pi -p "task"` works;
  session file byte-compat test against the TS implementation's fixtures.
- **P5 — Modes & extensions**: JSON mode, RPC mode (stdin/stdout JSONL, command
  schemas), extension host (per scope decision), MCP client, codemode sandbox,
  package manager subset. Exit: RPC driven session matches TS behavior on the
  documented command set.
- **P6 — Interactive mode**: chat viewport, editor, slash commands, model
  switching, themes, /resume session picker, images, export-html. Exit: daily-driver
  parity checklist passes.
- **P7 — Hardening**: compaction, cache warmer, output guard, crash log, telemetry
  subset, docs port, release binary + install script. Exit: tagged v0.1.0.
- **P8 (optional, later)**: chord/durable/p2 port or interop — only on explicit
  request.

## 5. Testing strategy

- Port unit tests from vitest suites to `cargo test` per crate (ai/agent/session
  have the most valuable ones).
- Golden fixtures: generate session JSONL + RPC transcripts with the TS pi
  (already installed locally) and assert byte/schema compatibility in Rust tests.
- Provider integration tests run only against `:free`/local endpoints (rule G1).
- `cargo test` in CI + `cargo clippy -D warnings` + `cargo fmt --check`.

## 6. Risks

- `interactive-mode.ts` (253 KB) and `agent-session.ts` (166 KB) are the two
  hardest ports; mitigated by splitting into Rust modules early and locking
  behavior with golden tests first.
- Extension API compatibility is the largest design risk (decided at P5 gate).
- Provider coverage: ~60 provider files are mostly thin configs over a few API
  shapes; codegen from catalog data keeps this cheap.
- TUI fidelity (images, kitty protocol, native modifiers) may be reduced in v0.1.

## 7. Ground rules

- Work happens in `/home/hakergeniusz/Projects/pi-rs` (this dir).
- Original TS checkout stays in `/tmp/ai-agent/pi` for reference.
- No GitHub push until user explicitly approves (project-creation skill rules).
- Go/no-go from user before each phase gate; no-go = stop and wait.
