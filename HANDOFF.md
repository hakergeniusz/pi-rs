# Handoff: Pi in Rust (`pi-rs`)

> Written 2026-11-11, updated same day after gate attempts. Repo: `/home/hakergeniusz/Projects/pi-rs` on `acer`.
> **GitHub: https://github.com/hakergeniusz/pi-rs (private)**, remote `origin` (ssh). `main` = P0–P3 gated history; **`wip/p4-pre-gate`** = snapshot branch with ALL uncommitted P4 work committed (`4853906 chore(pi-coding-agent): pre-gate snapshot of P4 work`, Assisted-by trailer). Local checkout is ON `wip/p4-pre-gate`. CI (GitHub Actions) queued on both pushes — run `gh run list` / `gh run watch --exit-status` to verify; unverified at handoff time.
> ALL development happens on `acer` (per user). Reference checkout: `/tmp/ai-agent/pi` on `acer` — **WIPED (tmpfs); re-clone before P5 work: `gh repo clone earendil-works/pi /tmp/ai-agent/pi`** (was @ `b22e525b3`).

## Goal
Rewrite Pi (earendil-works/pi — TypeScript coding-agent harness monorepo, ~236K LOC) in Rust, following `PLAN.md` in the repo root. Phases P0→P7, each ends with a committed, gated milestone.

## Current Progress

| Phase | Scope | State |
|---|---|---|
| P0 | Cargo workspace skeleton, lints, CI | ✅ committed `ecf7287` |
| P1 | `pi-ai` wire types, SSE, adapters, catalog (43 providers) | ✅ committed |
| P2 | `pi-agent` loop, tools seam, queues, hooks | ✅ committed `4ad169d` |
| P3 | `pi-tui` full TUI crate (~40.5K lines, 17 components, themes, keys, alt screen) | ✅ committed `39b22ad` — 740 workspace tests, clippy all+pedantic clean, fmt clean |
| P4 | `pi-coding-agent`: config, auth, sessions (JSONL v3 byte-compat), built-in tools, system prompt, prompt templates, skills, CLI args, print mode (`pi -p`) | ⚠️ code complete, in working tree, uncommitted — **gate blocked by machine OOM storms; 2 test fixes applied, final gate run still pending** (see Next steps) |

P4 detail (uncommitted, in working tree):
- `src/session/` (`records.rs` byte-exact Record envelope + V8-exact JSON writer in `session/json.rs`, `store.rs` SessionManager, `migrate.rs` v1→v3, `mod.rs`), `src/auth.rs` (auth.json store, lock protocol, `$VAR`/`!cmd` config-value resolution), `src/config.rs` + `src/config/settings.rs` (SettingsManager).
- `src/tools/` (read/bash/edit/edit-diff/write/ls/find/grep/truncate + renderers), `src/prompt.rs`, `src/prompt_templates.rs`, `src/skills.rs`.
- `src/cli/args.rs` (hand-rolled parser port with diagnostics + unknown-flag capture; includes `tool_list_error` port — mixed names+modifiers rejected at parse time like TS `getToolListError`), `src/modes/print_mode.rs` (text-mode `pi -p`, model resolution, api-key ladder flag → auth.json → env, `ProviderStreamFn` bridge pi-ai→pi-agent), `src/main.rs` wired (help/version/list-models/-p; json/rpc/interactive exit with phase notes).
- Test totals at last full run: 426 tests, 2 failures **fixed this session** (below), rest green. `cli`/`modes` tests now validated.

### P4 gate — state when handing off
- Two test failures were found and **fixed** (both were test bugs, implementations are TS-faithful; verified against TS reference before editing):
  1. `resolve_model_finds_unique_and_ambiguous`: used stale `anthropic/claude-sonnet-4`, which is NOT in `pi-ai/data/models.json`. Now uses `anthropic/claude-sonnet-4-6` (exact pair exists; bare id spans 3 providers → real ambiguity case).
  2. `tool_selection_honors_flags`: asserted "names win" (len 1) for a mixed names+modifiers `--tools` list. TS semantics: parse-time error via `getToolListError` (already ported in `cli/args.rs:120`), and `applyToolModifiers` doc says "Other entries are ignored" — so if mixed ever reached `select_builtin_tools`, bare names are ignored → full coding defaults. Test now asserts that (len 4, names `[read, bash, edit, write]`).
- After the fixes, every gate run that needed a **fresh rustc compile of pi-coding-agent** was killed (SIGTERM) by machine OOM fallout — see "What didn't work". Test runs on fully cached artifacts were green except the 2 failures above.
- `test-log.txt` in repo root is **stale** (documents an already-fixed E0308 in `session/store.rs`) — safe to delete before commit.

## What worked
- Reusing mature spike modules (`pi-rs-spike` on acer): session records/store/migrate, pi-tools, rebased onto typed wire APIs.
- Byte-compat strategy: ordered-map `Record` envelope + V8-exact float/UTF-8 JSON writer (`session/json.rs`); load→append→rewrite stays byte-identical.
- Two parallel subagent workstreams (data layer / tool layer) with disjoint file sets; then a lead-owned integration pass (CLI + main.rs).
- Before changing a "failing" test, verifying the TS reference (`getToolListError` / `applyToolModifiers` in `settings-manager.ts`) — both failures were test bugs, not impl bugs.

## What didn't work (machine incidents — read before running builds)
- **The machine OOM-stormed repeatedly** (20:28, ~20:43, 21:05 kernel OOM events; victims: llama-server, zcode desktop, chrome). **This session's probes were confirmed at fault for part of it** (user confirmed): a 4GB python memory-hog test and an extra rustc probe added pressure on a box with ~2-3G available. Do NOT run memory stress tests here.
- **The rustc "SIGTERM mystery" explained**: no process kills rustc directly. Kernel OOM SIGKILLs the highest `oom_score_adj` victim (zcode/chrome), then systemd scope teardown SIGTERMs remaining processes in `app-zcode-*.scope` — including cargo/rustc. Evidence: no kernel log line at rustc death; rustc killed mid-compile; sleep 130 and a 4GB hog survived (no OOM fired during those windows); small-crate compiles (pi-tui, 6.5s) survived.
- rtk is NOT the killer: no timeout in its config; `rtk bypass` made no difference. Don't waste time re-deriving this.
- Builds WITH debuginfo (`debuginfo=2`, the default dev profile) on the big pi-coding-agent unit spike several GB and overlap OOM windows; small crates are fine.

## User rules (additions from this session)
- **Limit cargo concurrency and max RAM usage** (user order after OOM): run builds as
  `nice -n 19 env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUGINFO=0 cargo <cmd>`
  The `DEBUGINFO=0` override splits the target cache from default-profile builds — **use the same env for every gate step** (test, then clippy) so the cache is shared.
- `llama-swap.service` (user-level systemd) was **stopped with user approval** for the gate and **must be restarted afterwards**: `systemctl --user start llama-swap.service`. Don't leave it stopped; don't kill its processes without asking.
- Never run memory-hog/stress probes on this machine again.
- Standing rules: run everything on `acer`; `nice -n 19` for all rustc/cargo; never read real `~/.pi` auth/session data; test fixtures must be synthetic.

## Next steps (in order)
1. **Finish the P4 gate** with the capped env from User rules:
## BUILD IN CONTAINER — the gate IS now green (2026-11-12)

All builds/tests run inside the **`pi-rs-build` podman container** (rootless, hard caps: `--memory=4g --memory-swap=4g --cpus=4`) — container OOM kills the BUILD, never the host. It runs rustc 1.99.1 matching the host; mounts repo at `/work`, host `~/.cargo/registry` (rw), volume `pi-rs-build-target` at `/build-target` (CARGO_TARGET_DIR). Needs `rg`+`fd` installed (done) for the grep/find tests.

Run the gate (all envs inside the container via `sh -c` — NEVER pass env via a zsh `$VAR` to `podman exec -e`; zsh does not word-split and cargo silently gets garbage, which caused a misleading OOM):

```bash
podman exec pi-rs-build sh -c 'export CARGO_TARGET_DIR=/build-target CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUGINFO=0; cd /work; nice -n 19 cargo fmt --all && nice -n 19 cargo clippy --workspace --all-targets -- -D warnings && nice -n 19 cargo test --workspace -- --test-threads=4 && nice -n 19 cargo fmt --all -- --check'
```

State when this handoff was updated: **workspace clippy-clean; full workspace test suite green (528 tests in the coding-agent lib alone; every integration target green; MCP unit tests green)** — this includes P4 (gate passed in-container) plus the P5 modules below. Remaining before landing on `main`:
1. **Re-clone the TS reference** (`gh repo clone earendil-works/pi /tmp/ai-agent/pi`) and reconcile the `TODO(verify-vs-TS)` markers written without the reference: `modes/json_event.rs` (event wire casing/shapes), `modes/rpc.rs` (request/response envelope), `cli/file_processor.rs` (block format, size cap), `cli/project_trust.rs` (marker-file name), `pi-ai/src/providers/anthropic.rs` (oauth beta header `anthropic-beta: oauth-2025-04-20`), env-key table additions in `print_mode.rs::env_key_candidates`.
2. Land P4+P5-part-1 on `main` (branch `wip/p4-pre-gate` has snapshot `4853906`; local checkout is on it), push, watch CI.
3. EACCES test note: `tools_write_edit.rs::includes_eacces_for_read_only_files` skips when running as root (container) — documented in the test.

## P5 progress (written 2026-11-12, all compiling + tested)
- **`modes/json_event.rs`** — JSON mode: per-event JSONL wire serialization (`event_to_json`), `JsonEventSink`, `run_json_mode` on the print-runtime seams. Wired into `main.rs` (`--mode json`).
- **`modes/rpc.rs`** — RPC mode: typed request/response envelope (`RpcRequest`/`RpcResponse`/`RpcCommand` with serde tags), `command_schemas()` table, stdin/stdout loop, per-command handlers (`prompt`/`steerable_prompt`/`followup_prompt`/`abort`/`set_model`/`set_thinking_level`/`get_state`/`get_messages`/`shutdown`). Steer/abort degrade to plain prompts until P6 queue plumbing. Wired into `main.rs` (`--mode rpc`).
- **`cli/file_processor.rs`** — `@file` expansion: inline tokens expand only when the file exists (prose stays prose), bare `@file` args error on missing; binary files noted, 1MB cap, 6 unit tests.
- **`cli/project_trust.rs`** — trust gating with agent-dir marker files, `_in` variants for dependency injection (no env mutation in tests), stdin/stdout confirm prompt, 5 unit tests. Not yet called from `main.rs` (wire with settings trust plumbing).
- **`extensions/mod.rs`** — runtime-agnostic extension-host seam: `Extension` trait (tools/commands/flags/events), `ExtensionRegistry` aggregation + unknown-flag claiming, 3 unit tests. deno_core-vs-native decision still open; this seam serves either.
- **`pi-mcp`** — real MCP client (was a stub): JSON-RPC 2.0 types + line classification, stdio subprocess transport (spawn, id correlation, notifications, EOF handling), `McpClient` with initialize handshake + `tools/list`/`tools/call`/`ping`, `McpServerConfig`. Unit tests on types/classification/serialization; HTTP transport + server-manager + tool-bridge into the agent registry still open.
- **OAuth bearer pass-through** — `StreamOptions.access_token` (pi-ai) → `StreamRequest.access_token` (pi-agent) → `AgentLoopConfig.access_token` → `PrintRuntime.access_token`; auth ladder returns `WireCredential::{ApiKey, AccessToken}` (`resolve_wire_credential`); anthropic adapter sends `Authorization: Bearer` + `anthropic-beta: oauth-2025-04-20` for tokens, `x-api-key` for keys.

## Next steps (in order)
1. Re-clone reference; reconcile TODO markers above; run the container gate again.
2. Land on `main` with `feat(pi-coding-agent): config, auth, sessions, tools, prompts, skills, print mode (P4) + P5 part 1 (json/rpc, extensions seam, mcp client, trust, @file, oauth)`, push, watch CI.
3. **Real `pi -p` smoke test against a live provider** — still the first true end-to-end proof (never done).
4. Golden session fixtures from the TS pi (byte-compat proof beyond synthetic fixtures).
5. P5 remainder: MCP server-manager + tool bridge into the agent registry, package-manager subset (`package-manager-cli.ts`, 41.6K), codemode sandbox (`pi-codemode` stub, blocked on host decision), extension runtime host (decision pending).
6. **P6 — Interactive mode** (chat viewport, editor, slash commands, themes, /resume).
7. **P7 — Hardening** (compaction, cache warmer, crash log, signals/backpressure, release).

## Gotchas / decisions
- Ported code once arrived with a systematic corruption: `map_or((default,` (extra `(` after `map_or(`) — fixed crate-wide by rewriting to `map_or(`. (Store.rs E0308 in old `test-log.txt` was another artifact of that batch, already fixed.)
- Workspace lints: clippy `all+pedantic`, CI `-D warnings`, `cargo fmt --check`, MSRV 1.85 (`rust-version = "1.85"`). Newer-API fixes must respect MSRV (e.g. `f64::midpoint` 1.87 → scoped `#[allow]`).
- `ToolDefinition.name` is `&'static str`; `Mode`/`PrintModeOptions` have no `Default`.
- pi-ai `StreamOptions.reasoning` has only 5 levels; `xhigh`/`max` clamp to `High` in `ProviderStreamFn` until P5.
- Catalog reality check for tests: `models.json` has `claude-sonnet-4-5/4-6/5/5-5` but NOT plain `claude-sonnet-4`; bare `claude-sonnet-4-6` is ambiguous (anthropic + cloudflare-ai-gateway + opencode).
- Deviations documented in code: NFKC subset for fuzzy edit matching (no `unicode-normalization` dep yet), no revision-cache in auth storage, no `powershell` tool, bash tool description hardcodes "bash".
- Local dev machine only (if ever resumed): plain `cargo` is broken there (rustup proxy misreads argv[0]); use `/tmp/ai-agent/cargo` wrapper (`exec -a cargo nice -n 19 ionice -c 2 -n 7 /usr/sbin/cargo`). On acer no wrapper needed — just prefix `nice -n 19`.

## Deferred items backlog (from P4 reports)
- `unicode-normalization` dep for byte-exact NFKC (edit tool fuzzy match).
- `serde_json` workspace feature `float_roundtrip` (exotic >17-digit literals, 1 ULP rewrites in session JSONL).
- `SessionListProgress` callback, `ReadonlySessionManager` (TS type-level surface).
- Node-runtime-specific parts of `config.ts` (Bun/npm install detection, self-update, QuickJS paths).
- JSON/RPC modes, extension binding, signals + stdout backpressure in print mode.
