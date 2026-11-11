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
   - `cargo test -p pi-coding-agent` (expect 426+ green; the 2 fixes are in)
   - `cargo clippy --workspace --all-targets -- -D warnings` (same env)
   - `cargo fmt --all -- --check`
   - The capped run started in the writing session is **dead** (killed with the session; log empty, no rustc left running) — just start it fresh with the capped env above.
   - Free memory first: llama-swap is stopped (4-6G available should hold); if horizon-client (2.8G RSS) is also spiking, ask the user before proceeding.
2. `test-log.txt` already deleted; the P4 code is already committed on branch `wip/p4-pre-gate` (snapshot `4853906`). After the gate passes: land P4 on `main` with
   `feat(pi-coding-agent): config, auth, sessions, tools, prompts, skills, print mode (P4)`
   (either re-commit on main or fast-forward), push, and watch CI per the git workflow rules.
3. **Re-clone the reference** before any P5 porting: `gh repo clone earendil-works/pi /tmp/ai-agent/pi` (tmpfs; was @ `b22e525b3`).
4. **Phase P5 — Modes & Extensions**: JSON mode (`modes/json-event.ts`, ~61 lines), RPC mode (stdin/stdout JSONL, command schemas), extension host (decide: deno_core embedded JS vs Rust-native trait+WASM vs subprocess — plan recommendation: deno_core), MCP client (`pi-mcp` crate is a stub), codemode sandbox (`pi-codemode` stub), package-manager subset (`package-manager-cli.ts`, 41.6K), project trust gating (`cli/project-trust.ts`), `@file` args (`cli/file-processor.ts`), full per-provider env-key table (`packages/ai/src/env-api-keys.ts`; partial table already in `print_mode.rs::env_key_candidates`), OAuth bearer auth in adapters. Exit: RPC driven session matches TS behavior on the documented command set.
5. **P6 — Interactive mode** (chat viewport, editor, slash commands, themes, /resume).
6. **P7 — Hardening** (compaction, cache warmer, crash log, signals/backpressure, release).

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
