# Handoff: Pi in Rust (`pi-rs`)

> Written 2026-11-11, updated same day after gate attempts. Repo: `/home/hakergeniusz/Projects/pi-rs` on `acer`.
> **GitHub: https://github.com/hakergeniusz/pi-rs (private)**, remote `origin` (ssh). **`main` = P0–P5-part-1 + codex adapter + interactive TUI (parts 1+2a+2b fullscreen) + XDG paths; E2E PROVEN LIVE** — `pi -p` smoke test PASSED 2026-11-14 (PONG / read-tool round-trip ALBATROSS / JSON event mode) with the openai-codex stored OAuth credential and default model `openai-codex/gpt-6-luna`; required (1) a new `openai-codex-responses` adapter in pi-ai (`8efb00b`) and (2) the agent loop driven on a process-wide tokio runtime (`4c4d62c`, was futures::executor::block_on → "no reactor running"). — landing fast-forwarded `39b22ad..37782d1` on 2026-11-14; first landing CI failed only because ubuntu-latest ships **neither rg nor fd** (12 grep tests: `which("rg")` failed) → fixed in ci.yml (`8c6d178`, installs `ripgrep` + `fd-find`); CI run `43578686739` on `8c6d178` = **ok**. Interactive TUI landed `27769a9..500302b` + part 2a `8ae806d` on 2026-11-14 (see P6 sections). `wip/p4-pre-gate` == `main`; local checkout is ON `wip/p4-pre-gate`.
> ALL development happens on `acer` (per user). Reference checkout: **`/tmp/pi`** on `acer` (re-cloned 2026-11-15 @ 1.5.2; tmpfs — re-clone if gone: `gh repo clone earendil-works/pi /tmp/pi`).

## Goal
Rewrite Pi (earendil-works/pi — TypeScript coding-agent harness monorepo, ~236K LOC) in Rust, following `PLAN.md` in the repo root. Phases P0→P7, each ends with a committed, gated milestone.

## Current Progress

| Phase | Scope | State |
|---|---|---|
| P0 | Cargo workspace skeleton, lints, CI | ✅ committed `ecf7287` |
| P1 | `pi-ai` wire types, SSE, adapters, catalog (43 providers) | ✅ committed |
| P2 | `pi-agent` loop, tools seam, queues, hooks | ✅ committed `4ad169d` |
| P3 | `pi-tui` full TUI crate (~40.5K lines, 17 components, themes, keys, alt screen) | ✅ committed `39b22ad` — 740 workspace tests, clippy all+pedantic clean, fmt clean |
| P4 | `pi-coding-agent`: config, auth, sessions (JSONL v3 byte-compat), built-in tools, system prompt, prompt templates, skills, CLI args, print mode (`pi -p`) | ✅ committed — gate green in-container (1205 tests, 44 targets), landed on `main` 2026-11-14 |
| P5 | JSON/RPC modes, extensions seam, MCP client, trust, `@file`, OAuth | ⚠️ part 1 landed on `main` (`c8e1818` + reconciliation `a6b9934`); remainder open (rpc envelope, MCP server-manager/tool-bridge, package-manager, codemode, ext host, `defaultProjectTrust` getter) |
| P6 | Interactive mode | ⚠️ **parts 1+2a+2b+2c landed on `main`** — regular chat TUI 1:1 (theme, components, orchestrator, themed renderers, CaptureTerminal e2e, real-PTY smoke) + autocomplete/model/trust/settings/session selectors + compaction + queues + mouse toggles + OSC theme + fullscreen mode (alt screen + chat viewport + switch) + **3D easter egg + `/arminsayshi`**; remainder open (extensions host, rpc/MCP hardening) |

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
1. ~~Finish the P4 gate~~ — **DONE 2026-11-14**: gate green in-container after reconciliation commit `a6b9934` (clippy fallout fixed: `assert_is_empty` module allows, `single_match_else`→if/else, `format_push_string`→`writeln!`, `map_unwrap_or`, `match_same_arms`, `implicit_clone`, `type_complexity` alias).
## BUILD IN CONTAINER — the gate IS now green (2026-11-12)

All builds/tests run inside the **`pi-rs-build` podman container** (rootless, hard caps: `--memory=4g --memory-swap=4g --cpus=4`) — container OOM kills the BUILD, never the host. It runs rustc 1.99.1 matching the host; mounts repo at `/work`, host `~/.cargo/registry` (rw), volume `pi-rs-build-target` at `/build-target` (CARGO_TARGET_DIR). Needs `rg`+`fd` installed (done) for the grep/find tests.

Run the gate (all envs inside the container via `sh -c` — NEVER pass env via a zsh `$VAR` to `podman exec -e`; zsh does not word-split and cargo silently gets garbage, which caused a misleading OOM):

```bash
podman exec pi-rs-build sh -c 'export CARGO_TARGET_DIR=/build-target CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUGINFO=0; cd /work; nice -n 19 cargo fmt --all && nice -n 19 cargo clippy --workspace --all-targets -- -D warnings && nice -n 19 cargo test --workspace -- --test-threads=4 && nice -n 19 cargo fmt --all -- --check'
```

State when this handoff was updated: **workspace clippy-clean; full workspace test suite green (1202 tests, 44 targets; every integration target green; MCP unit tests green)** — P4 gate passed in-container, P5 part 1 committed as `c8e1818` on `wip/p4-pre-gate` and pushed. **TODO(verify-vs-TS) markers: all reconciled (2026-11-14, reference re-cloned to /tmp/ai-agent/pi, shallow).** What the reconciliation changed:
- **`cli/file_processor.rs` rewritten to upstream** (`file-processor.ts`): NO size cap, `<file name="{abs}">\n{content}\n</file>\n` blocks, empty files skipped, BOM stripped, invalid UTF-8 → U+FFFD (Node semantics), image sniff (jpeg/png/gif/webp/bmp magic bytes incl. animated-png + isBmp) → `ImageContent` attachments + `<file name="..."></file>` refs, bmp → upstream's could-not-convert text (no sharp pipeline). **No inline `@token` scanning exists upstream** — prompt text is never scanned; only bare `@file` args. RPC mode rejects file args (`main.ts:708`). `buildInitialMessage` joins stdin+fileText+first message with `""` (`initial-message.ts:40`) — `resolve_prompt_messages` now mirrors it; stdin piping is a P7 note. New dep: `base64`.
- **`modes/json_event.rs`**: `message_update` wire shape fixed to `json-event.ts` — `{type, usage, assistantMessageEvent}` with `partial` stripped and `toolcall_start` gaining `id`/`toolName`. Remaining honest deviations (doc'd): `agent_end` without `willRetry`, synchronous sink vs mid-loop subscription.
- **`cli/project_trust.rs` rewritten**: upstream uses **`<agentDir>/trust.json`** (map canonical cwd → bool, `null` deletes, sorted keys, nearest-ancestor lookup, `findNearestTrustEntry`) + `hasTrustRequiringProjectResources` (`.pi/` entries list + `.agents/skills` ancestor walk excluding `$HOME/.agents/skills`) + `resolveProjectTrusted` ladder (override → resources??true → store → `defaultProjectTrust` always/never/ask → non-UI = **false**, no prompting). Marker files + stdin confirm were invented; gone. Wired into `build_runtime` (`project_trusted: Some(...)`. The `defaultProjectTrust` setting getter is still open).
- **`print_mode.rs::env_key_candidates`**: verified against `env-api-keys.ts:73-129`; hallucinated arms removed (grok/aihubmix/dmxapi/gitee-ai/novita/ppio/scaleway/targon/cohere/perplexity/siliconflow/modelscope/bailian/stepfun had NO TS entries); `minimax-cn`→`MINIMAX_CN_API_KEY`; `huggingface`→`HF_TOKEN` only; added `google-vertex`, `github-copilot`, `radius`, `typesafe`, full qwen pairs. Auth ladder semantics fixed: `ANTHROPIC_AUTH_TOKEN` → Bearer; `ANTHROPIC_OAUTH_TOKEN` → **x-api-key** (`anthropic.ts:43-45`); stored OAuth creds → `toAuth().apiKey` = x-api-key (`auth/oauth/anthropic.ts:304-306`) EXCEPT kimi-coding (Bearer headers, `kimi-coding.ts:275-277`).
- **`pi-ai/src/providers/anthropic.rs`**: the `anthropic-beta: oauth-2025-04-20` header **does not exist** in the current reference — removed. Bearer = plain `Authorization` (no beta); key wins when both set; env fallbacks removed from the adapter (the caller's ladder owns env). `openai_completions.rs` now honors `access_token` (kimi-style Bearer) ahead of api_key.
- **rpc.rs** request/response envelope is NOT yet reconciled against `rpc-entry.ts` (no marker; doc note says before P5 exit gate).

Remaining before landing on `main` — **ALL DONE 2026-11-14**:
1. ~~Container gate~~ green (1205 tests, 44 targets, clippy+fmt clean) → committed `a6b9934`, pushed.
2. ~~Land on `main`~~ fast-forwarded `39b22ad..37782d1`, pushed; ci.yml needed **ripgrep AND fd** (first run `43578349132` FAILED — 12 grep tests, `rg` missing on the runner) → `8c6d178` → CI run `43578686739` **GREEN**.
3. EACCES test note (kept): `tools_write_edit.rs::includes_eacces_for_read_only_files` skips when running as root (container) — documented in the test.

## P5 progress (written 2026-11-12, all compiling + tested)
- **`modes/json_event.rs`** — JSON mode: per-event JSONL wire serialization (`event_to_json`), `JsonEventSink`, `run_json_mode` on the print-runtime seams. Wired into `main.rs` (`--mode json`).
- **`modes/rpc.rs`** — RPC mode: typed request/response envelope (`RpcRequest`/`RpcResponse`/`RpcCommand` with serde tags), `command_schemas()` table, stdin/stdout loop, per-command handlers (`prompt`/`steerable_prompt`/`followup_prompt`/`abort`/`set_model`/`set_thinking_level`/`get_state`/`get_messages`/`shutdown`). Steer/abort degrade to plain prompts until P6 queue plumbing. Wired into `main.rs` (`--mode rpc`).
- **`cli/file_processor.rs`** — `@file` expansion: inline tokens expand only when the file exists (prose stays prose), bare `@file` args error on missing; binary files noted, 1MB cap, 6 unit tests.
- **`cli/project_trust.rs`** — trust gating with agent-dir marker files, `_in` variants for dependency injection (no env mutation in tests), stdin/stdout confirm prompt, 5 unit tests. Not yet called from `main.rs` (wire with settings trust plumbing).
- **`extensions/mod.rs`** — runtime-agnostic extension-host seam: `Extension` trait (tools/commands/flags/events), `ExtensionRegistry` aggregation + unknown-flag claiming, 3 unit tests. deno_core-vs-native decision still open; this seam serves either.
- **`pi-mcp`** — real MCP client (was a stub): JSON-RPC 2.0 types + line classification, stdio subprocess transport (spawn, id correlation, notifications, EOF handling), `McpClient` with initialize handshake + `tools/list`/`tools/call`/`ping`, `McpServerConfig`. Unit tests on types/classification/serialization; HTTP transport + server-manager + tool-bridge into the agent registry still open.
- **OAuth bearer pass-through** — `StreamOptions.access_token` (pi-ai) → `StreamRequest.access_token` (pi-agent) → `AgentLoopConfig.access_token` → `PrintRuntime.access_token`; auth ladder returns `WireCredential::{ApiKey, AccessToken}` (`resolve_wire_credential`); anthropic adapter sends `Authorization: Bearer` + `anthropic-beta: oauth-2025-04-20` for tokens, `x-api-key` for keys.

## Next steps (in order)
1. ~~Container gate after the reconciliation~~ DONE (`a6b9934`); ~~land on `main` with fd in ci.yml~~ DONE (`37782d1`).
2. ~~Real `pi -p` smoke test~~ **DONE 2026-11-14** — first true end-to-end proof PASSED: `pi -p "Reply with exactly: PONG"` → `PONG`; read-tool round-trip → `ALBATROSS`; `--mode json` → correct JSONL wire. (Working binary: build in the container, `podman cp` out, run from a temp dir with `--no-session`.)
3. Golden session fixtures from the TS pi (byte-compat proof beyond synthetic fixtures).
4. P5 remainder: reconcile `rpc.rs` against `rpc-entry.ts`; MCP server-manager + tool bridge into the agent registry, package-manager subset (`package-manager-cli.ts`, 41.6K), codemode sandbox (`pi-codemode` stub, blocked on host decision), extension runtime host (decision pending), `defaultProjectTrust` settings getter.
5. ~~**P6 — Interactive mode**~~ **parts 1+2a DONE 2026-11-14, part 2b (fullscreen) DONE 2026-11-15, part 2c (3D easter egg) DONE 2026-11-15** (see the P6 sections below); the extension host remains.
6. **P7 — Hardening** (compaction, cache warmer, crash log, signals/backpressure + stdin piping, release).

## P6 progress — interactive chat TUI (part 1, landed `500302b` 2026-11-14)

Bare `pi` (no args, stdout a TTY) now renders the 1:1 port of upstream's
regular chat mode from `packages/coding-agent/src/modes/interactive/`
(~4.4K lines added). The pi-tui framework and theme engine were already
ported (P3) — the gap was entirely the coding-agent layer.

- **`modes/interactive/theme.rs`** — global theme (`THEME` RwLock; see the
  deadlock gotcha below), `init_theme`/`set_theme` with silent system
  fallback, markdown/select-list/editor theme getters, chalk-equivalent
  bold/italic/inverse helpers, `app_keys` module = `APP_KEYBINDINGS`
  subset (escape interrupt, ctrl+c clear/×2-exit, ctrl+d exit-empty,
  ctrl+z suspend, shift+tab thinking cycle, ctrl+p/ctrl+shift+p model
  cycle, ctrl+o tools expand, ctrl+t thinking toggle), `keyHint` helpers.
- **`modes/interactive/components/`** — `user_message` (Markdown on
  UserMessageBg), `assistant_message` (streaming markdown-slot reuse,
  thinking runs, hidden label, stop-reason footers), `tool_execution`
  (pending/success/error backgrounds, `toolTimerText` port: final
  duration wins, `Elapsed` while partial; `formatToolDuration` 4.2s /
  1m 30s / 2h 3m 4s), `footer` (↑↓R W CH% $ / context% color thresholds
  70/90 / right-aligned model ` • thinking level`), `header` (half-block
  pi logo, BuiltInHeader compact↔expanded via ctrl+o), `status_indicator`
  (Loader spinner in the editor's top border).
- **`modes/interactive/interactive_mode.rs`** — the orchestrator: tree
  [header, chat container, idle status, ChatEditor, footer]; agent run on
  a worker thread (`run_agent_loop` + mpsc sink), editor-event queue for
  submits/app-actions, spinner host-driven via `Loader::tick` every 80ms,
  crossterm poll(25ms) input loop with `key_to_terminal_data`; commands
  `/new /clear /exit /quit /model /thinking /theme` and `!`/`!!` bash;
  shutdown prints `To resume this session: pi --session <id>`.
- **`tools/renderers/`** — themed ports with `CallContext`/`ResultContext`
  fn-pointer `ToolRenderers`: compact read classification (SKILL.md →
  `[skill] dir`, AGENTS/CLAUDE.md → `read resource`), bash keep-end-5
  preview, edit diff with LCS word diff + inverse highlighting
  (`parse_diff_line` ports the upstream regex `^([+-\s])(\s*\d*)\s(.*)$`
  WITH backtracking), write/grep/find/ls previews + truncation warnings.
- **pi-tui seams** — `Editor.top_border_decorator` (the
  `renderTopBorder` override), `Container::as_any_mut` opt-in so hosts
  downcast typed children out of the tree.
- **Tests** — CaptureTerminal e2e in `interactive_mode.rs`: real agent
  loop + scripted `StreamFn` (tool-use turn calling `read` on a temp
  file, then a final turn), driving submit→pump()→render; asserts header
  hints, user message, both assistant turns, tool row call+result,
  footer usage totals, session persistence. Plus a mid-run working-status
  test and an app-actions (ctrl+o/ctrl+d) test.
- **Real-PTY smoke PASSED 2026-11-14** — `/tmp/pi-rs-pty-smoke.py`
  (openpty, winsize set BEFORE fork; `pty.fork` raced the size and
  hung): temp HOME, synthetic `--api-key`, asserts logo half-blocks +
  ctrl+o hint + interrupt hint, then ctrl+d exit + `To resume this
  session:` line. Driver kept in /tmp, not committed.
- **Gate green + CI green** (clippy all+pedantic -D warnings, 44 test
  targets, fmt --check); landing CI run `43602314909` = **ok**.

## P6 progress — part 2a: autocomplete, model selector, queues (landed `8ae806d` 2026-11-14)

- **pi-tui `fuzzy.rs`** — port of `packages/tui/src/fuzzy.ts`:
  `fuzzyMatch` (greedy alignment + best contiguous-occurrence rescoring,
  word-boundary/gap/consecutive scoring, exact-match −100,
  letters↔digits swap retry +5) and `fuzzyFilter` (whitespace/slash
  tokens, all must match, stable best-first sort). Positions are char-,
  not UTF-16-index-based (identical for ASCII).
- **Editor trait fidelity** — `AutocompleteProvider::suggest` gained the
  `force` flag and a defaulted `should_trigger_file_completion`; the
  editor's Tab handler gates forced completion through it
  (`editor.ts:2300-2309`, `:2388`).
- **`modes/interactive/autocomplete.rs`** — port of `autocomplete.ts`
  `CombinedAutocompleteProvider`: `@`-prefix quoted/unquoted extraction
  (`findUnclosedQuoteStart`, `stripLeadingWrappers`, `isTokenStart`),
  fuzzy file search via an `fd` subprocess (base-dir depth-1 + recursive,
  dedupe, score 100/80/50/30 +10 dir bonus, top 20, scoped `dir/query`
  display), readdir path completion (dirs-first, `~`/`./`/absolute
  preserving), `applyCompletion`'s four branches (slash command `/x `,
  @-attachment, argument context, plain path) and the quoted-value
  requote. Synchronous — an `fd` run blocks for its few-ms duration
  (upstream: 20ms debounce + abort tokens). CJK-punctuation separators
  approximated by the fullwidth class (no unicode-script dep).
- **Autocomplete command list** (`autocomplete_commands`) — model /
  thinking / new / quit with upstream descriptions; only implemented
  builtins are listed (advertising unimplemented ones would make the
  picker lie). `/model` args fuzzy-search the whole catalog
  (`getModelSearchText`); `/thinking` args = `VALID_THINKING_LEVELS`.
- **Model cycling** — ctrl+p / ctrl+shift+p now cycle
  (`getNextCycleIndex` + `_cycleAvailableModel`): credential-filtered
  catalog models (`resolve_wire_credential` per provider; the `--api-key`
  flag counts only for the initial provider — upstream auth-filters via
  provider `checkAuth`), status `Switched to {name||id}` + thinking
  suffix. `/model <pattern>`: exact resolve switches (status
  `Model: {id}`, fixing part 1's "Switched to"), otherwise the pattern
  prefills the selector.
- **`components/model_selector.rs`** — `ModelSelectorComponent` port
  minus the parts that need the runtime catalog refresh (embedded static
  catalog) and scoped models (land with settings): search Input, fuzzy
  filter (`getModelSelectorSearchText`), current-model-first sort,
  `getDistinctModelName` annotation, 10-row window + scroll indicator,
  enter/escape/page/tab semantics, published events drained by `pump`.
  Mounted via `Tui::replace_child` in the editor's tree slot
  (`showSelector`), editor parked in `suspended_editor`.
- **Steering/follow-up queues** — `Queues` now shared orchestrator↔loop
  via `Arc<Mutex<Queues>>` + a `QueueSource` adapter; Enter during a run
  steers, alt+enter (`app.message.followUp`) queues a follow-up (or
  submits when idle), alt+up (`app.message.dequeue`) restores all queued
  messages into the editor, the pending-messages container (new tree slot
  between chat and status) renders `Steering:`/`Follow-up:` rows + the
  dequeue hint. Deliveries persist at loop-declaration time — the
  `active_prompt` marker (timestamp+text) keeps the run's own prompt
  (persisted at submit) from double-appending when a steer shares its
  millisecond. Tree is now [header, chat, pending, status, editor,
  footer] — the index consts moved.
- **pi-tui fidelity fixes found while porting** — `Spacer::new(lines)`
  renders its empty lines (was a no-op; upstream `Spacer(1)`), and
  `TruncatedText` is a real Component (paddingX/Y, ANSI-aware truncation,
  first line only) instead of a byte-slicing helper that would panic on
  UTF-8.
- **Tests** — 14 new: fuzzy engine (8), provider (8 incl. fd-gated),
  model selector unit (6), cycle indices/step, selector mount/restore
  e2e, steer/follow-up e2e (transcript order + pending display),
  dequeue e2e. Workspace 1269 tests; PTY smokes: base header/exit +
  autocomplete dropdown + model selector open/cancel — all pass.
- **CI on the landing**: run `43606894650` on `8ae806d` = **ok**.

### P6 known deviations from upstream (documented in module docs)
- No OSC terminal-color query yet (COLORFGBG → dark fallback); theme
  controller lands in part 2b.
- Autocomplete lists only implemented builtins; no extension/prompt-
  template/skill commands in the picker (they land with the extension
  host), no `#` resource trigger, no `autocompleteMaxVisible` setting
  wiring (default 5). Selector: no catalog refresh, no scoped models /
  scope toggle / save-as-default / default badge, no `/login` hint
  action. fd subprocess is synchronous (upstream debounces + aborts).
- No extension statuses in footer, no mouse toggles, no compaction UI.
- Queue: no bash-pending row in the pending display (`!` is synchronous),
  dequeue does not abort the run (upstream's abort variant is used by the
  extension/abort paths, not the alt+up handler).
- `!` bash runs synchronously (upstream streams via PTY) — P7.
- No syntax highlighting in tool output (no grammars); edit diffs render
  on result only (no live streaming preview).
- `Tui::do_render` already contained the main-screen differential
  renderer (P3 port of `tui-main-screen.ts`) — no new main-screen port
  was needed.

## XDG paths (landed on `feat/xdg-paths`, 2026-11-14)

User request: "Make it use its own path that follows XDG rules" — pi-rs must
not touch the real TypeScript pi's `~/.pi` (the user runs real pi heavily).

- `config.rs` rewritten as the single XDG resolver. Roles: **config**
  `$XDG_CONFIG_HOME/pi-rs` else `~/.config/pi-rs` (settings, auth, models,
  themes, prompts, tools, skills, trust); **data** `~/.local/share/pi-rs`
  (sessions); **cache** `~/.cache/pi-rs` (managed fd/rg); **state**
  `~/.local/state/pi-rs` (debug log). XDG values must be absolute + non-empty
  else the default applies (per spec). `PI_CODING_AGENT_DIR` collapses every
  role into one dir — upstream's exact single-dir layout when set.
  `PI_CODING_AGENT_SESSION_DIR` unchanged. Pure cores (`xdg_base_dir_for`,
  `config_dir_for`, `agent_dir_for(env, xdg, home)`, `sessions_dir_for`,
  `bin_dir_for`, `debug_log_path_for`) take env as `Option<&str>` — no
  `set_var` in tests (edition 2024 / cross-thread races).
- `CONFIG_DIR_NAME` = `"pi-rs"` (user XDG leaf); NEW
  `PROJECT_CONFIG_DIR_NAME` = `".pi"` for project-local dirs. Project-scope
  users of the old constant switched: `settings.rs` (×2), `project_trust.rs`;
  `tools::CONFIG_DIR_NAME` now aliases `PROJECT_CONFIG_DIR_NAME`.
  `args.rs --help` env table updated.
- `tools/mod.rs`: `agent_dir()` delegates to `config::get_agent_dir()`;
  `bin_dir()` to `config::get_bin_dir()`; local `expand_tilde` deleted
  (duplicate of config's). Session layout: `default_session_dir_path(cwd,
  sessions_dir)` now takes the SESSIONS ROOT (was agent dir + internal
  `join("sessions")`); all callers pass `get_sessions_dir()`;
  `SessionLayout::from_env` removed (zero callers; semantics didn't survive
  the role split — sessions no longer hang off the agent dir).
- **Deliberate deviation, no migration**: pi-rs will not read or write `~/.pi`
  at all. File formats stay byte-compatible; the root moves. README "Paths"
  section documents the table.
- Verified: workspace gate green (clippy -D warnings, all tests, fmt);
  real-PTY smoke (`/tmp/pi-rs-xdg-smoke.py`, driver outside the repo) with a
  synthetic HOME and all XDG_*/PI_CODING_AGENT_* unset — session dir created
  under `<home>/.local/share/pi-rs/sessions/--<cwd>--/`, **no `<home>/.pi`
  created**. (Session *file* is created lazily on first append — empty exit
  leaves only the dir; pre-existing part-1 behavior, unchanged.)

## Compaction (landed on `feat/p6b-compaction`, 2026-11-14)

Full port of `core/compaction/*` plus the interactive `/compact` flow.

- `session/compaction.rs` (~1100 lines, 12 tests): settings
  (`reserve_tokens` 16384 / `keep_recent_tokens` 20000, chars/4 estimate,
  image = 4800 chars), `should_compact`, `prepare_compaction` (projection →
  prev-compaction boundary → `find_projected_cut_point` with the backward
  budget walk, recovery-omission suffix advance, split-turn handling), the
  three verbatim prompts + `SUMMARIZATION_SYSTEM_PROMPT`,
  `serialize_conversation` ([User]/[Assistant]/[Tool result] with 2000-char
  truncation, thinking omitted), `compact()` as a single summarization call
  over the `StreamFn` seam (convert_to_llm FIRST, AbortSignal honored
  between events), the read-files/modified-files XML appendix (`\n\n` join,
  `formatFileOperations` verbatim), `details_json`.
  `SessionManager::compact` in store.rs orchestrates prepare → summarize →
  append (the print/RPC/tests entry point).
- Interactive wiring (`interactive_mode.rs`): `/compact [instructions]`
  dispatch (after /model, before `!`), "Compacting context... (ctrl+c to
  cancel)" via `StatusIndicator::compaction` in the editor border,
  submissions during compaction queue ("Queued message for after
  compaction"; built-in slash commands still dispatch), Interrupt aborts
  compaction first, the outcome drain in `pump()` appends the compaction
  entry on the UI thread, renders the `[compaction]` row (collapsed
  "Compacted from N tokens (ctrl+o to expand)", ctrl+o expands to markdown)
  plus the "Compaction: N tokens billed (~$X)" notice, then flushes the
  queue (first message starts a run, the rest ride steer/follow-up).
  "Compaction failed: {message}" wraps every error; "Compaction cancelled"
  on aborts; "Already compacted" / "Nothing to compact (session too small)"
  for prepare rejections.
- **Deviation — UI split flow**: the worker thread can't own `&mut
  SessionManager`, so the UI does prepare-on-main → LLM-on-worker →
  append-on-main at outcome, NOT `SessionManager::compact`. Also:
  single-attempt summarization (no retry), no session_before_compact /
  session_compact extension hooks, cacheRetention left to the adapter, the
  chat keeps rendered history instead of re-rendering from entries (the
  renderSessionEntries port is pending), and DEFAULT_COMPACTION_SETTINGS is
  used in interactive mode (no settings handle yet; the getter is ported
  and tested).
- **Pump-order gotcha**: retire the compaction run BEFORE
  `handle_compaction_outcome` — the queue flush re-enters `submit`, whose
  guard would re-queue the flushed message while the run still looks live
  (upstream clears the promise before `flushCompactionQueue`). The e2e
  caught this.
- Verified: workspace gate green; two CaptureTerminal e2e tests with a
  content-aware scripted stream keyed on the summarization system prompt
  (full run → /compact → status → `[compaction]` row → cost notice →
  persisted entry with the read-files appendix → "Already compacted" on
  repeat; and queue-during-compaction → flush runs the queued message and
  persists it right after the compaction entry). Real-PTY smoke
  (`/tmp/pi-rs-compact-smoke.py`): /compact on an empty session prints
  "Nothing to compact (session too small)" and the TUI survives.
- **Test note**: the read tool caps its result at 50KB (≈12.5k estimated
  tokens), so a read alone can never exceed the 20000-token keep-recent
  budget — the e2e uses a 100KB second user prompt to push the backward
  walk past it.

## OSC theme controller (landed on `feat/p6b-osc-theme`, 2026-11-14)

The `theme-controller.ts` port: OSC 10/11/4 color queries, mode-2031 scheme
reports, and auto light/dark pair settings, wired into the interactive mode.

- pi-tui already carried the query machinery (`Tui::query_terminal_colors` /
  `service_terminal_queries` / `take_terminal_colors`, scheme listeners);
  this slice added the controller: `theme.rs` module state (reported colors,
  pending flag, scheme — upstream keeps these module-level too) plus
  `record_reported_colors`, and the mode-side controller fields
  (`theme_setting`/`theme_active_name`/`theme_auto_sync`, late-report +
  scheme-report queues, `theme_query_start` as the deadline clock) drained by
  `pump_theme()` at the end of every pump.
- **Two `TerminalColors` types**: the TUI query API reports
  `pi_tui::tui::TerminalColors` (`RgbColor`, u8 channels, Copy) while the
  theme loader consumes `pi_tui::color::TerminalColors` (`Rgb`, f64, palette
  `Vec`). Upstream has one type; the port converts once at
  `theme::record_reported_colors`. Controller queues/signatures use the tui
  type; theme.rs stores the color type. `parse_auto_theme_setting` /
  `resolve_theme_setting` are NOT duplicated — pi-tui's theme module already
  exports them; the pi-coding-agent wrapper folds in
  `terminal_appearance()`.
- pi-tui's `CaptureTerminal` is now `Clone` (response store Arc-backed) so a
  test keeps a handle to the terminal a mode owns — the affordance upstream
  TS gets for free. `themed_fixture` builds on it.
- Verified: theme.rs unit tests (merge/change/u8→f64 conversion,
  scheme-vs-background precedence), three CaptureTerminal e2e (early reply
  re-themes; reply arriving after the deadline rides the late sink;
  mode-2031 report applies only when auto-sync), real-PTY smoke
  (`/tmp/pi-rs-theme-smoke.py`): answers the queries with a full palette +
  DA1 + a 997 report, asserts no reply text leaks into the editor and /exit
  is clean.
- **Deviations**: `/theme` persists nothing yet — the settings.json `theme`
  write lands with the settings selector (upstream `/theme` opens that
  selector); the setting IS read at startup (`--use-theme` > settings.json).
  One shared deadline clock per mode instead of upstream's per-query timers
  (the crate owns no timer; the host passes elapsed ms). Extension themes'
  "<in-memory>" sentinel is unreachable (no extension host).
- **Smoke gotcha**: a PTY smoke that typed into the editor must ctrl+u
  before `/exit`, or the Enter submits the typed text as a user message (the
  theme smoke's first run shipped a 401 to the real API).

## Mouse-driven toggles (landed on `feat/p6b-mouse-toggles`, 2026-11-14)

The component-side mouse handlers, ported ahead of the fullscreen renderer
that actually delivers mouse events.

- **Upstream scoping finding**: in pi 1.5.2 the main-screen chat TUI NEVER
  enables terminal mouse tracking (`TuiMainScreen` takes no mouse option;
  `TuiAltScreen` does, `mouse ?? true`). The click-to-expand and logo-click
  handlers live on shared components and are inert in the main screen — the
  port reproduces exactly that. There is no keybinding toggle upstream.
- `ToolExecutionComponent` wraps every displayed child (call line, result
  body) in pi-tui's already-ported `MouseRegion`; a left click toggles the
  row's expansion (`tool-execution.ts:116-124`, wrapping at
  `:228`/`:235`/`:240`/`:249`/`:257`/`:264`). Rust ownership note: the
  region callbacks cannot borrow the component, so they flip a shared
  `Arc<AtomicBool>` that `handle_mouse` reconciles into `set_expanded`.
- `BuiltInHeader::handle_mouse` recognizes clicks on the four logo cells
  (y ≤ 1, x within this component's padding offset — upstream's header pads
  one column; `interactive-mode.ts:249-256`) and hands the logo's screen
  position to `on_logo_click` — unset, because the 3D easter egg
  (`easter-egg-3d.ts`) is deferred with fullscreen.
- Verified: unit tests for the click toggle (left click expands/collapses;
  middle click and press pass through) and the logo-cell geometry (screen
  coordinate math, outside-clip, unset callback). The alt-screen Tui's mouse
  state machine is already ported in pi-tui; its delivery lands with the
  fullscreen mode slice.

## Project trust selector (landed on `feat/p6b-trust-selector`, 2026-11-14)

The `/trust` dialog: the third selector following the established mount
pattern (`Tui::replace_child(EDITOR_INDEX, selector)` + suspended editor +
per-selector `Arc<Mutex<Vec<Event>>>` queue drained in the pump).

- `cli/project_trust.rs` gained the option builder: `project_trust_options`
  ports `getProjectTrustOptions` (Trust = project entry set true; Trust
  parent folder = parent entry set true + project entry DELETED, offered
  whenever the parent exists — only the filesystem root has none; Do not
  trust = project entry set false), and `project_trust_parent_path` ports
  `getProjectTrustParentPath` (canonicalized parent, `None` for `/`).
- `TrustSelectorComponent` (`components/trust-selector.ts`) renders the
  saved decision (`formatDecision`: "none" / "{label} (inherited from
  {path})" when the saved path differs from the project's) and the current
  session state, preselects the option matching the saved decision, and
  keeps the upstream `j`/`k` literal fallbacks beside the `tui.select.*`
  keybindings. Confirm emits `{ trusted, updates }`; the mode persists via
  `ProjectTrustStore::set_many` and prints "Saved trust decision:
  trusted|untrusted. Restart pi for this to take effect." (upstream's
  restart note — the port likewise does not re-resolve trust live).
- `main.rs::run_interactive` resolves the trust ladder once and threads
  `agent_dir`/`project_trusted` through `InteractiveOptions` (test seams
  default them), so the selector displays real state without re-reading
  the store.
- Gotcha (gate tooling): appending `| tail -3` to the clippy step MASKS its
  exit status — a failing chain still printed "GATE-GREEN". Keep clippy
  unpiped in the gate, or check `${PIPESTATUS}`.
- Not ported (documented, upstream-copied text): the `pi auth login`
  subcommand. The "Run `pi auth login --provider X`" error hint in print
  mode is upstream's own wording and is misleading in the port until auth
  login lands — the working path is copying auth.json into
  `~/.config/pi-rs/`.

## Settings selector (landed on `feat/p6b-settings-selector`, 2026-11-15)

The `/settings` dialog (`settings-selector.ts`, 752 lines): the fourth
selector, same mount pattern. Landed as three component commits + one
wiring commit + the version cut (`v0.1.0-alpha.1`).

- **Honest row scoping** ("the picker must not lie"): the dialog carries only
  the rows that work end-to-end — steering mode, follow-up mode, hide
  thinking, default project trust, per-model thinking level, theme. Upstream
  rows whose backend the port doesn't have yet are omitted (autocompact is
  display-only here, images/transport/share/http-timeout/cache-warming/
  mermaid/telemetry/etc. have no ported backend). The component test asserts
  the absence of unported rows so the scope can't silently drift.
- **pi-tui settings-list debt** (`f066f46`): `with_search()` (Input row +
  fuzzy filter + row-offset mouse) and `with_submenu` factories. Rust
  divergence fixed: upstream's `filteredItems` alias the same item objects,
  so the port filters `Vec<usize>` indices into `items` instead of cloning —
  mutations (e.g. the model-thinking summary) stay visible through both
  views. `with_values` no longer clobbers `current_value`.
- **Submenu callback shape** (`73f8554`): Rust can't call back into the list
  from a factory closure, so `SubmenuDone` is a shared mailbox drained right
  after input forwarding (`SubmenuClose { value, navigate_to }`; navigate_to
  re-selects + re-activates, close-without-value restores the opener's
  selection). `settings_submenu.rs` ports `SelectSubmenu` (rebuilds its
  SelectList on filter — upstream does too) and `SteppedSubmenu`
  (`SubmenuContext` = BTreeMap, loop=true for the model-thinking steps).
- **ThemeSubmenu**: single menu (system/automatic/themes, "automatic" does
  NOT close — `with_done_manual`) + the automatic menu (light/dark submenus,
  apply row); previews resolve + `set_theme` + render WITHOUT persisting;
  cancel restores the original. `/theme` persistence deviation CLOSED: the
  selector writes `theme` via `SettingsManager::set`.
- **Model thinking**: `getSupportedThinkingLevels` ported into pi-ai
  (`models.js:702-712`: non-reasoning → off; xhigh/max only when the
  `thinkingLevelMap` entry exists and is non-null). The stepped submenu
  persists `modelThinkingLevels[provider/id]` (clear-override row reverts to
  the global default); the orchestrator live-re-applies when the current
  model is touched.
- **Read sides** (`dae34d2`): interactive startup reads hideThinkingBlock +
  steering/follow-up modes (persisting upstream-style via settings);
  `build_runtime` bootstraps a settings manager BEFORE trust resolution to
  feed `defaultProjectTrust` into the ladder (main.js:583/669) and resolves
  the initial thinking level flag > per-model override >
  `defaultThinkingLevel` > off (model-resolver.js:386-421) — previously
  always "off" for non-interactive runs.

## Session selector (landed on `feat/p6b-session-selector`, 2026-11-15)

The `/resume` dialog (`session-selector.ts`, 884 lines) + the prerequisite
transcript replay, on one branch (4 commits + this doc).

- **Transcript replay first** (`a06ed45`): `renderSessionEntries` +
  `renderSessionItems` (interactive-mode.ts:3437/:3340) — startup walk of the
  compaction-aware context entries re-creating the chat rows (user prompts
  also populate the editor history, assistant tool calls open rows completed
  by replayed toolResults with durations/error flags, aborted/errored rows
  carry upstream's texts, compaction entries render summary + billed-tokens
  notice). `run()` calls it after terminal start, so `pi --continue` /
  `--session` now show the prior transcript — the empty-resume deviation is
  CLOSED, and in-place switching had a truthful display to build on.
  Skipped (documented): custom messages, branch summaries, bashExecution
  rows, cache-miss notices.
- **Search** (`058e5d5`): `session_selector_search.rs` ports
  `session-selector-search.ts` — tiered matching (name=0, id/messages/cwd
  exact=1, fuzzy name/cwd=2; tiers dominate the fuzzy scores they carry),
  `re:` case-insensitive regex mode (the `regex` crate was already a
  dependency), `"phrase"` with whitespace normalization, unbalanced-quote
  fallback, Recent keeps listing order while other modes sort by score with
  modified-desc tiebreak.
- **Selector** (`209ed86`): header (title left; scope/name/sort right; hint
  lines swap for delete-confirmation/status), one-line rows in a scrolling
  window with fork-tree prefixes (`parentSessionPath` grouping, latest
  subtree activity sort), Tab scope toggle with lazy all-load, ctrl+s sort
  cycle, ctrl+n named filter, ctrl+p path display, ctrl+d/ctrl+backspace
  delete confirmation (`trash` CLI → unlink fallback; the active session is
  protected), ctrl+r rename via `SessionManager::append_session_info`.
  Deviations: loaders synchronous (no progress/abort plumbing), status
  messages persist until the next state change (no timer), the upstream
  loader/confirmation callbacks collapse into direct calls.
- **Wiring** (`e001452`): `/resume` opens the selector on the standard
  mount; Select swaps the session store (`SessionManager::open`), clears run
  state, replays the transcript, and shows "Resumed session".
  **Deviation**: upstream rebuilds the whole runtime for the target's cwd
  (extensions, tools, trust); the port keeps the startup runtime and only
  swaps the session, so a foreign-cwd session keeps this cwd's tool
  bindings.
- Gate gotchas: pi-tui's `truncate_to_width(text, width, ellipsis, pad)` —
  upstream's `truncateToWidth` defaults `pad=false`; passing `true` silently
  pads lines to full width and broke the header layout math (right side
  measured as width → left side squeezed to nothing). And `Input` has BOTH
  `Focusable::set_focused` and `Component::set_focused` — call as
  `Focusable::set_focused(&mut input, …)` to disambiguate.

## Fullscreen mode (landed on `feat/p6b-fullscreen`, 2026-11-15)

P6 part 2b: the alt-screen renderer + chat viewport + the mode switch, on
one branch (7 commits + this doc). Upstream has NO fullscreen-mode.ts — it
is `createInteractiveTui({tuiMode})` (tui-renderer.js) choosing
`TuiAltScreen` vs `TuiMainScreen`, `createChatViewport`
(chat-viewport.js) building the transcript ScrollView + dock, and
`switchTuiMode` (interactive-mode.ts:641-700) swapping renderers under one
shared component tree. The port's pi-tui already contained the full
alt-screen machinery (P3); this slice is the pi-coding-agent integration.

- **pi-tui groundwork** (`efb8bde`, `5791096`): `FocusTarget::Id(ComponentId)`
  names nested components (component ids are heap addresses — taken AFTER
  boxing); `Tui::component_by_id_deep_mut` walks the tree through
  `layout_children_mut`; `Stack/VStack::replace_child` (swap in place,
  entries untouched) + `entry_options`; `ScrollView::set_styles` +
  `take_child` + public `state`; `set_copy_on_select`/`set_wheel_scroll_lines`
  runtime setters; `clear_alt_screen_root`.
- **Chat viewport** (`1ac55fc`): `chat_viewport.rs` ports
  chat-viewport.js — `mount` moves the six flat hosts into
  Container[header,chat] → ScrollView (follow_end, primary, overscroll
  chain, themed scrollbar) + dock VStack[pending, status, editor(min 3),
  footer] → root VStack[transcript grow, dock] → `set_alt_screen_root`;
  `disassemble` hands them back in flat order. Ownership is THE structural
  deviation: JS shares the tree by reference; Rust needs one owner, so the
  hosts MOVE and the recorded `HostSlot` ids find them in either
  arrangement.
- **Settings** (`e68434b`): getTuiMode (fullscreen default; only "regular"
  opts out), getFullscreenExitOutput, getFullscreenScrollbar (+ From into
  pi-tui Scrollbar), getFullscreenCopyOnSelect (default on),
  getFullscreenWheelScrollLines (clamp 1-100 floor, "auto" else), + setters.
- **Exit API** (`2fd47af`): `Tui::exit_alt_screen(TuiStopOptions)` runs the
  before/after stop hooks without stopping the terminal, drops the alt
  state (root children route flat again), resets the render state like the
  fresh TuiMainScreen a switch builds, and returns leftover root children.
- **Wiring** (`41f1e10`): InteractiveOptions.tui_mode (--tui-mode wins,
  else settings); `run()` enters fullscreen before the first frame;
  `switch_tui_mode` refuses while a selector occupies the editor slot
  (upstream's overlay guard) and moves the hosts via
  take_root_children/mount ↔ disassemble/exit_alt_screen; `downcast_tree`
  takes a HostSlot and reaches the hosts by identity in either arrangement;
  `replace_editor_slot` swaps selectors into the flat index or the dock
  slot; focus rides `FocusTarget::Id(editor_id)` across switches (upstream
  preserves the focused component). `fullscreen_options` ports
  tui-renderer.js:9-29 (theme-styled search matches + navigation buttons,
  the themed "↓ Jump to latest message · End" label, copy-on-select + wheel
  settings). `shutdown` ports stopInteractiveTui (:632-639): transcript
  exit → drain (restore editor, drop selector queues) → switch to regular →
  stop(preserveScreen:false) so the stop repaints the transcript into the
  scrollback; resume-hint exit → stop(preserveScreen:true). e2e test drives
  enter → viewport root mounted → selector blocks switch → exit → flat
  tree renders.
- **Settings selector rows** (`a94a262`): the five fullscreen items verbatim
  (settings-selector.ts:489-525) + their events + handlers
  (:4374-4402) — TuiModeChange reverts the row and shows "Close active
  overlays before changing TUI mode" when refused; scrollbar re-applies to
  the live transcript; copy-on-select/wheel push to the live alt screen.
- **Deviations**: the viewport is REBUILT per switch (upstream reuses the
  same viewport object), so the transcript scroll offset resets to
  follow-the-end and the scrollbar setting is re-read at mount —
  documented in chat_viewport.rs; `open_url`/`on_right_click_paste`
  unset (no browser opener; Windows-only upstream); `keyDisplayText`
  reduced to a one-key capitalized helper ("end"→"End"); `copySelection`
  is the alt screen's internal OSC-52 path (upstream passes a clipboard
  callback); upstream's `restoreProgress` (terminal progress bar on switch)
  has no port counterpart.

## 3D easter egg + /arminsayshi (landed on `feat/easter-egg-3d`, 2026-11-15)

P6 part 2c: `easter-egg-3d.js` (1138 lines) + `armin.js` + the lazy wrapper,
ported host-driven (the port has no timers — the interactive loop's ~25ms
pump cadence stands in for upstream's `setInterval`).

- **armin.rs** (`d6368a6`): the inline component — the 31×36 XBM bitmap
  (LSB-first, 1 = background; art pixels are the 0s, 245 of them) and the
  seven reveal effects (typewriter, scanline, rain, fade, dissolve, CRT
  wipe, glitch) keyed by a fresh splitmix64 seed per mount. Pure
  `grid_at(frame)` per effect; the host derives the frame from elapsed
  time × fps ("pi owns an interval; this port owns a frame index").
- **easter_egg_3d.rs** (`7374b40`): the fullscreen 3D animation. SGR
  `parseScreen` (inverse video: `cellFg = bg ?? background`); `BlockRaster`
  braille ray-caster — per-dot ray/plane intersection with a depth buffer,
  face culling including neighbor-cover (exact `==` positions while blocks
  rest, as upstream's `===`), diffuse/rim/specular lighting, fog, and the
  light-background halo as a separable tent filter with quantized
  amounts/levels; timeline — fly-off (0.1/1.3s), puzzle cycles (0.3s × 12
  shuffle steps + 1s return + hold) with the spin locked to them
  (SPIN_RAMP/SPIN_SPEED), dust dissolve (WAVE_SPREAD/JITTER), quantized
  starfield (STARS_START 2.5), and the 1.1s exit that runs the whole
  timeline backwards; models pi-logo (4×4, 10 blocks, camera 10,
  widthShare 0.35) and Armin (31×36, camera 80, widthShare 0.45);
  `Math.imul`-style hash (`u32::wrapping_mul`) keeps every random choice
  deterministic per cell.
- **Host wiring** (`7374b40`): `play_easter_egg_3d` mirrors the lazy
  wrapper's guards (`alt_screen_active()` ≡ `tui instanceof TuiAltScreen`;
  `has_overlay()` returns **true** — absorbed, no fallback — exactly
  upstream) and captures `last_lines()`; the animation uses the stored
  startup terminal colors (`theme::terminal_colors`, made `pub(crate)`)
  instead of a fresh query; the exit's completion raises `Arc<AtomicBool>`
  and the pump hides the overlay. The header's `on_logo_click` callback
  queues the logo position into an `Arc<Mutex<Vec<(i64,i64)>>>` (the
  callback cannot borrow the mode mid-dispatch) and the pump replays it as
  the 3D pi logo. `/arminsayshi` plays the 3D Armin fullscreen and falls
  back to `Spacer(1)` + the inline component in the flat tree; the command
  is deliberately absent from autocomplete (upstream hides it too).
- **Deviations**: no timers (host-driven frames + pump-polled completion,
  documented at the type); terminal rows via `Arc<AtomicU16>` refreshed by
  the pump instead of a per-frame `process.stdout.rows` read; stored
  startup colors instead of a live color query. Everything else — layout,
  math, constants, easing, quantization, guards — is 1:1.
- **Tests**: 17 new (armin 5, egg 12) + 3 interactive-mode e2e
  (fallback in the flat tree, fullscreen play + pump retirement, logo
  click drain). Gate green: fmt, clippy --workspace --all-targets -D
  warnings, workspace tests.

## Agent docs (landed on `docs/agent-docs`, 2026-11-15)
- **`AGENTS.md`** (repo root): the operating manual for AI agents — fidelity
  directive, ground-truth locations, the `~/.pi` privacy rule, docs-where-you-go
  rule, build/test suite, git workflow, code conventions, and the current
  backlog. Machine-specific constraints (podman container `pi-rs-build`, OOM
  caps, `-j 2`, no stress probes) are quarantined under a
  "workstation-specific" heading so agents without that container can still
  use the file; only the plain cargo suite is universal.
- **`docs/PORTING.md`**: the porting craft — ground-truth locations, the
  crate↔package map (including the `pi-coding-agent` module map against
  upstream `cli/core/extensions/modes/utils`), the six-step porting workflow,
  the TS→Rust pattern dictionary (timers→pump, callback queues, aliasing→ids,
  `Math.imul`→`wrapping_mul`, `===`→float_cmp allow, emitter→set_on_*, Map
  order→ordered containers, let-chains), testing conventions (CaptureTerminal,
  PTY smokes, synthetic fixtures, `#[ignore]` providers), condensed durable
  gotchas, and the deviations ledger.
- Every `.rs` file was verified to carry a `//!` module doc already — the
  per-code doc rule has held since P0; these two files add the repo-level
  layer. `/tmp/ai-agent/pi` re-clone CONFIRMED present at `827383a`
  (`packages/coding-agent` 1.5.2) — the "WIPED (tmpfs)" warning in the header
  above is stale.

## Item-level doc pass (landed on `docs/code-docs`, 2026-11-15)
- Workspace scan found **521 pub items without `///` docs**; this pass landed
  ~470 of them, leaving **69** (almost all `pi-coding-agent`'s smaller files —
  the tier whose helper agent was stopped mid-flight — plus a few
  `//`-comment-separated false positives a naive scanner counts).
  Covered: lib.rs per-module one-liners ×5 crates; `pi-agent` complete
  (agent builder/setters, `ThinkingLevel`, `AbortSignal`, `PendingMessageQueue`,
  state setters); `pi-mcp` types; `pi-ai` provider module docs;
  `pi-tui` keybindings/terminal/keys/mouse/undo/kill-ring/search/ansi/color/
  oklab/layout + framework core (theme, tui, tui_alt_screen, component,
  system_theme, frame) + all of `components/`; `pi-coding-agent`
  tools/mod.rs one-liners + interactive/theme.rs app_keys + style helpers.
- The pass ran as three helper agents (pi-tui plumbing 174 docs, framework
  core 45, components 163) plus main-agent work (94 + 37 + fixes); every
  agent diff was verified docs-only (0 removed lines, only `///` additions).
- **Fixed pre-existing doc bug**: `ansi.rs` `flatten_lines` carried the first
  sentence + citation of `apply_background_to_line` (docs merged onto the
  wrong item); `flatten_lines` now cites only `utils.ts:896-898`.
- **Incident, caught by the gate**: a batch edit replaced `theme()`'s
  signature line instead of inserting a doc above it (syntax error at
  `theme.rs:216`); restored, gate re-run green with unpiped exit codes
  (CLIPPY/TEST/FMT all 0, 0 failed suites) — piped `| tail` again masked
  exit codes on the first attempt, so the log+exit-code pattern stays.
- **Findings from the pass worth future attention** (reported, not fixed):
  `components/input.rs` and `line_input.rs` are two parallel ports of the
  same upstream `input.ts` (byte-offset vs char-index cursors) — hosts must
  not mix them; `input.rs` redeclares `CURSOR_MARKER` locally;
  `SelectListTheme::selected_prefix` is declared/defaulted but never read
  (`render_item` hardcodes `"→ "`); `terminal.rs` `KEY_PROTOCOLS` is
  referenced nowhere; `LayoutBox::layer` is always written 0 so the
  hit-test z-order key is constant; `LayoutNode::VStack` ignores `align`;
  stale citations `editor.ts:2665-2282` (reversed range) and the module doc's
  "2,472 lines" claim.

## Extensions-host recon (landed on `docs/extensions-recon`, 2026-11-15)
- **Deliverable: `docs/EXTENSIONS.md`** — the recon + design decision PLAN.md
  §2 required at the P5 gate. No code changed; the seam
  (`crates/pi-coding-agent/src/extensions/mod.rs`) is untouched scaffolding.
- **Decision: embedded JS engine — `deno_core` (V8)** behind a feature-gated
  `pi-ext-js` crate, phased E0→E4 (spike → headless core → UI-over-protocol →
  TUI bridging → completeness). `rquickjs`+swc recorded as fallback if the
  spike fails on the memory-capped machine. Rust-native trait and subprocess
  host rejected (evidence in §6.2).
- **Evidence base** (all cited in the doc): full read of upstream
  `core/extensions/{runner,types}.ts` (3925 lines) + `docs/extensions.md`
  (3367 lines) + two agent surveys (API-surface inventory; loader + bundled
  extensions). Key facts: 37 typed events, each with distinct dispatch
  semantics (§2 table); no LLM-sampling surface in `pi` (providers/virtual
  models only); tool exposure has 5 values incl. `hidden` (the only
  "unregister"); session mutation is command-context-only; upstream already
  serves extensions from a bundled virtual-module table when the host is a
  compiled binary (`virtual-modules.ts:14`) — the exact shape a Rust+V8 host
  replicates; the whole boundary is JSON/strings, incl. TUI components
  (`render(width) → string[]`); bundled-extension API usage defines the
  de-facto E1 core subset (§5.3).
- **Seam gap analysis** (§7): current trait models the easy 20%; six gap
  classes (event model, tool registration richness, command context, missing
  registration surfaces, lifecycle/staleness, error plumbing). Keepers: the
  `Arc<dyn Fn>` executor signature, registry fan-out shape, flag claiming.
- **Spike checklist for E0** (next unit): deno_core builds in-container under
  memory caps (prebuilt v8), MSRV vs 1.85, tokio alignment, binary growth +
  RSS, hello-world op round-trip.

## E0 spike: JS host on rquickjs (landed on `feat/e0-js-host-spike`, 2026-11-15)
- **Engine decision flipped before the spike ran: `deno_core` → `rquickjs`
  0.14 (QuickJS-NG).**
  - Cloudflare acquired Deno (2026-10-09): the Deno runtime is deprecated —
    monthly bug/security releases until ~2027-10, then EOL. The maintenance
    guarantee the recon relied on for `deno_core` is gone.
  - Bun (the operator's suggested alternative) was assessed and rejected for
    embedding: no embeddable library API exists (oven-sh/bun#12017 still
    open; the Anthropic Rust rewrite — acquired 2025-12, Zig→Rust merged
    2026-05 — ships none; the only embedding crates are weeks-old community
    projects). Embedding Bun today would reopen the subprocess-host model
    rejected in docs/EXTENSIONS.md §6.2. Watch item: an official Bun
    embedding API would be revisited before E2.
  - `rquickjs` 0.14 was the recon's recorded fallback and is now primary:
    embedded library, MIT, active, QuickJS-NG backend. docs/EXTENSIONS.md §6
    carries the updated decision.
- **Spike crate `crates/pi-ext-js`** (E0 scope, deliberately no API):
  ESM module load → default-export `(pi)` factory → `pi.on` /
  `pi.registerCommand` into JS-side registries → Rust-backed `getConfig` op
  → `emit` fan-out in registration order with stringified results back in
  Rust; a module without a default export fails as a typed
  `JsHostError::MissingDefaultExport`. Three tests, all green. The real
  `ExtensionAPI` (loader, TS transpile, async handlers, runner) is E1.
- **Measurements** (container 4 cores / 4 GB, `-j2`, `nice 19`,
  `/build-target`):
  - cold release build (quickjs-ng + rquickjs + crate + example): **29 s**;
  - runtime peak RSS of the spike example: **~4.0 MiB** release (4.5 MiB dev);
  - release example binary, stripped + thin-LTO: **1.7 MiB**;
    `libquickjs.a`: **2.3 MiB** — versus deno_core's projected +30–60 MiB.
  The OOM-risk gate that motivated the go/no-go is retired.
- **MSRV bumped 1.85 → 1.87**: rquickjs 0.14 declares
  `rust-version = "1.87"`; the workspace floor moved (CI runs
  `dtolnay/rust-toolchain@stable`, unaffected; 1.87 shipped 2025-05). The
  bump also surfaced `manual_is_multiple_of` in `pi-tui/word_navigation.rs`
  (WB15/WB16 even-run check now `.is_multiple_of(2)`; `is_multiple_of`
  stabilized exactly in 1.87).
- **rquickjs 0.14 API notes banked for E1**: `Runtime::set_memory_limit`
  takes `usize`; `Array::len()` returns `usize`; `Array::get<V>(idx)` is
  single-generic; modules go `Module::declare(..)?.eval()` →
  `(Module, Promise)`; drain jobs with `Runtime::execute_pending_job()` →
  `Result<bool>` (its `JobException` is not publicly nameable — flatten to
  a string); `JSON.stringify` quote-wraps strings, so the glue keeps an
  identity helper for string values.
- **Next: E1 headless core** — Rust `ExtensionRunner` port with §2 dispatch
  semantics, loader (§5), de-facto core API subset (§5.3); exit = the
  bundled `tool-search` extension runs identically to upstream.

## E1 headless extensions core (landed on `feat/e1-ext-core`, 2026-11-15)

- **What landed** (commits `42b3342` headless core + `1b020f7` tests/exit
  gate; all in `crates/pi-ext-js`, ~6,900 lines):
  - `transpile.rs` — the jiti transform replacement: **oxc 0.158** (parse →
    TS strip → codegen; enums/namespaces lowered via
    `SemanticBuilder::with_enum_eval(true)`, module syntax untouched, ESM
    out). §6.3's swc-vs-oxc pick resolved to **oxc** (lighter build under
    the 4 GB cap; the whole oxc stack cold-builds in ~24 s at `-j2`).
  - `resolver.rs` — virtual-module table (`typebox`,
    `@earendil-works/pi-codemode/declarations`), relative/absolute
    resolution with the entry rules, `pi.extensions` manifest reading
    (`pi-manifest.ts`), and the E1 rejection messages for `node:*` and
    host packages (E4).
  - `engine.rs` + `glue.rs` — the QuickJS runtime, the Rust op table (the
    `pi` API: registration with `createExtensionAPI`'s exact error
    strings, action methods guarded by the not-initialized-throw until
    `bind`, `exec`, MCP, flags, settings, session metadata), the headless
    contexts (base/tool/command/trust + the `noOpUIContext` matrix), the
    **dispatch-session protocol** (event objects stay live across
    handlers so in-place mutation chains, results land in a slot the
    runner reads after draining jobs), the `Intl.Segmenter` shim, the
    console shim, and the extension event bus.
  - `runner.rs` — the `ExtensionRunner` port: `emit`,
    `emitToolCall` (in-place input mutation, `block` short-circuit,
    errors propagate), `emitToolResult` (middleware chain, content
    replacement drops `structuredContent`), `emitInput` (transform chain,
    `handled`), `emitBeforeAgentStart` (message collection +
    `forceSystemPrompt` chain, per-handler `systemPrompt` render),
    `emitMessageEnd` (role guard), `emitBoundary`
    (entries/continue threading, preview rebuild, invalid ⇒ discard),
    `emitProjectTrustEvent` (first decided wins), first-wins tools/flags,
    `name:occurrence` commands with the collision loop, tool execution,
    staleness invalidation.
  - `loader.rs` — discovery order (project `.pi/extensions` → user
    `extensions` → explicit; one level deep, manifest/index entries),
    the pre-trust split (`split_for_trust`), errors-as-values.
  - `mcp_registry.rs` — `mcpNamespace` + `validateMcpServerConfig` port
    (verbatim error strings), owner/namespace-clash registration rules.
  - `exec.rs` — `execCommand` port: combined `maxBuffer` cap with
    truncation-kill, timeout SIGTERM → SIGKILL after the 5 s grace,
    `128+signal` exit codes, ENOENT as a failed execution (code 1).
- **Exit gate met**: upstream's bundled `tool-search` extension vendored
  **verbatim** (`tests/fixtures/tool-search/`, MIT attribution headers)
  loads unmodified through the full pipeline — oxc transpile, relative
  import, both virtual modules, the `Intl.Segmenter` shim — and behaves
  identically: the BM25 ranker/tokenizer/$ref schema-text cases ported
  from `test/tool-search.test.ts` all pass (one documented `#[ignore]`
  for ICU-dependent segmentation), and the end-to-end gate registers it
  inactive, activates it, executes it (relevance/name/namespace queries,
  exact-name-first, limits, activation bookkeeping, validation errors,
  upstream result text). 56 tests green.
- **E1 deviations** (each recorded where it is implemented):
  1. `Intl.Segmenter` is a shim (QuickJS-NG ships no `Intl`); unspaced
     scripts (CJK/Thai) segment as one run. Upstream reaches the
     segmenter only outside its ASCII fast path, so ASCII behavior is
     byte-identical. E4 replaces the shim.
  2. `node:*` built-ins and host packages (`@earendil-works/*`) reject
     imports with the documented E1 error; no `node_modules` resolution.
     E4.
  3. `ctx.executeTool` returns upstream's unbound-runner result
     (`runner.ts:988-999`, "Nested tool calls are not available in this
     context"); the real nested path binds with the agent loop (E2) —
     rquickjs's runtime lock (a `RefCell` without the `parallel` feature)
     forbids re-entering the context from inside an op.
  4. `ctx.signal` is `undefined` (no host abort signal crosses the
     boundary yet); `pi.exec` accepts and ignores the `signal` option.
     E2.
  5. `setActiveTools` filters to registered non-hidden names; the full
     loadout logic (`_applyToolLoadout`, `--tools` patterns) belongs to
     agent-session and lands at host wiring. E1's `bind()` seeds the
     active set with declared (`direct`/`model-only`) tools whose
     `defaultActive !== false` — which is what makes `tool_search`'s
     candidate pool correct.
  6. Dispatch reads handler indexes from `__piInspect` snapshots but
     resolves them against the live JS registries; upstream slices the
     handler array once per dispatch. Divergence is only observable when
     a handler unsubscribes mid-dispatch; `/reload` semantics land in E4.
  7. `ToolInfo` carries name/description/parameters/exposure/namespace/
     defaultActive; `promptGuidelines`, `reachesTools`, `annotations`
     are host concerns (E2+).
  8. CJS interop not attempted (upstream extensions are ESM; §6.3).
- **rquickjs/oxc API notes banked (for E2+)**: the `loader` feature flag
  is required for `rt.set_loader`; `Function::call::<Args, R>` takes
  **only tuple args** (`IntoArgs` is implemented for tuples, not scalars);
  `Resolver::resolve(&mut self, ctx, base, name, attributes)`;
  `Loader::load` returns a declared `Module` directly;
  `Ctx::throw(value)` + `ctx.catch()` for JS-exception message
  extraction; `ctx.json_stringify` returns `Result<Option<String>>`;
  the runtime lock is a `RefCell` (non-`parallel`) — **never nest
  `ctx.with`**; workspace `unsafe_code = "forbid"` means signals go
  through `nix` (exec), not `libc`. oxc: `Transformer::new(&allocator,
  path, &options)`, `build_with_scoping(scoping, &mut program)`,
  diagnostics on `ParserReturn`/`TransformerReturn`, `Codegen::build` →
  `.code`; TS `Source` type = `SourceType::mjs().with_typescript(true)`.
- **Next: E2 — UI over protocol + host wiring**: dialogs/notify/
  setStatus/setWidget/setTitle via the no-op matrix headless and the RPC
  subprotocol (`docs/rpc-extension-ui.md`); `ui_prompt_start/end`;
  providers/virtual models incl. OAuth-config; `session_before_*` gates;
  `user_bash` interception; `context`/`context_with_system` transforms;
  feature-gate `pi-ext-js` behind `ext-js` on `pi-coding-agent` and
  replace the seam (`crates/pi-coding-agent/src/extensions/mod.rs`).

## E2: UI over protocol + host wiring (in progress, on `feat/e2-host-wiring`, 2026-11-15)

- **Slice 1 — engine UI surface** (`2355e01`): `ctx.ui`
  select/confirm/input/editor/custom queue a request through the
  `uiRequest` op and continue from a host-settled `__piUiResolve`
  promise (timeout answers resolve per the rpc-extension-ui contract);
  fire-and-forget methods queue only; `ui_prompt_start`/`ui_prompt_end`
  emit in upstream's order; the runner drains requests into a `UiSink`
  and threads responses (scripted `FakeSink` tests). The mode is read at
  call time, so binding a UI affects contexts created earlier.
- **Slice 2 — user_bash, context, providers** (`0cfa34b`):
  - `emit_user_bash` (runner.ts:1276-1307): handlers run in order; the
    first valid `{operations}`/`{result}` wins; `undefined` falls
    through to local execution; an invalid result is reported AND
    rethrown with the exact upstream message ("Invalid user_bash
    handler result: return undefined for local execution or exactly one
    valid { operations } or { result } object").
  - `call_bash_exec` bridge (core/tools/bash.ts:74-93): the stashed
    operations `exec` runs with a normalized options object; `onData`
    streams through the `bashData` op; `signal` stays `undefined` (no
    abort crossing — deviation). The result lands in a one-slot poll;
    the poll is bounded at 100k job drains and fails with a JSON
    engine-state dump instead of spinning.
  - `emit_context` (runner.ts:1312-1370): `context` handlers see the
    conversation minus system roles on a fresh per-handler event;
    in-place edits and `{messages}` returns both apply; a changed
    conversation reattaches the replayed system message
    (`restoreSystemMessages`). `context_with_system` handlers see the
    full transcript; a leading-system loss is reported but honored.
  - Provider/virtual-model registry (`model-runtime.ts:924-1030` +
    `loader.ts:468-524`) with the **two-level load-time deferral**:
    while an extension is loading, provider changes queue on the
    extension record (a failed load discards them, loader.ts:269-277);
    after commit they queue on the runtime; `bind()` drains the queues
    through the registry in order and records per-registration
    validation failures as `ProviderBindError {extension_path, event:
    "register_provider"|"register_virtual_model", error}` (read via
    `ExtensionRunner::provider_registration_errors()`); post-bind
    registrations apply immediately and throw into the caller. The
    API-level "Provider config is required when registering by name"
    check still throws at load (loader.ts:471).
  - Every rquickjs error surface maps through `js_error` (exception
    message extraction), so callers see the JS message instead of
    rquickjs's opaque engine error text.
- **E2 deviations so far**: `user_bash` `signal` stays `undefined`
  across the bridge; native-provider/virtual-model definitions cross as
  JSON (route functions dropped — routing protocol lands with the
  model-runtime wiring); the physical-model conflict check runs
  host-side when the registry is consumed; bind-flush validation
  failures are *recorded* (`provider_bind_errors`) rather than emitted
  through the runner error callback — that plumbing lands with slice
  3/4 host wiring.
- **The flaky-hang saga, resolved** (process lessons in Gotchas): the
  21-minute spin in `user_bash_operations_bridge…` was the bridge never
  writing its result slot (`__piBashText`), so the poll could never
  succeed. The same gate then surfaced two more deterministic bugs:
  slot errors must be JSON *strings* (`__piSessionLast` `JSON.parse`s
  them; the validation branch stored an object →
  `JSON.parse("[object Object]")` → the reported message came out as
  `unexpected token: 'object'`), and the provider-error test called
  `pi.appendEntry` at module top level — action methods throw until
  bind (loader.ts:179 `appendEntry: notInitialized`); it now appends
  from a `session_start` handler. What made these look flaky: the hunt
  rounds ran the test binary directly via `find` without a rebuild. The
  bounded poll stays as defense-in-depth. The provider deferral above
  was then ported faithfully — the first cut validated eagerly at load,
  which upstream does not.
- **Tests**: pi-ext-js 75 passed + 1 ignored (5 stability rounds at
  `--test-threads=4`); full-workspace gate + CI before merge.
- **Extension language (decided 2026-11-15)**: JS/TS-only for extension
  authors — upstream has no native-plugin semantics to mirror, and a
  second extension ABI would be pure divergence. Native-level extension
  points are: in-tree Rust modules (compiled into the host), MCP
  servers (language-agnostic), and RPC mode (external processes). The
  host-side registries keep a native-registration path open if a real
  use case appears.
- **Slice 3 — host wiring** (`1a17635`, branch `feat/e2-host-wiring`):
  the dead seam (`Extension` trait/registry, zero consumers) is replaced
  by the real host. `pi-ext-js` is an optional dep of `pi-coding-agent`
  behind the `ext-js` feature (default on; `--no-default-features`
  builds the engine-free shape). `main.rs build_runtime` now runs the
  upstream startup sequence (`main.ts:780-941`):
  - Discovery with `--no-extensions` semantics — CLI `-e` paths still
    load (`discover_cli_paths`, `resource-loader.ts:609-610`); missing
    local `-e` paths error once (`Failed to load extension "…":
    Extension path does not exist: …`) and are excluded from loading
    (`resource-loader.ts:620-627`; `isLocalPath` prefixes ported).
  - The pre-trust bootstrap pass runs only when trust actually needs
    resolving (no `--approve` override + trust-requiring resources,
    `main.ts:804-808`) and loads non-project origins only; the
    `project_trust` hook sits in the trust ladder before the store
    (`project-trust.ts:55-71`) and `remember` persists. The runner's
    settings view flips to the trusted dump after the decision
    (`setProjectTrusted` + `reload`).
  - The final pass loads trusted-allowed paths minus already-loaded
    modules and never retries bootstrap failures
    (`loadFinalExtensionSet`, `resource-loader.ts:791-833`), then
    `bind()`; bind-flush provider errors surface as warnings.
  - Flag claiming (`applyExtensionFlagValues`,
    `agent-session-services.ts:82-129`): registered booleans set `true`
    regardless of the captured value; strings require a value
    (`Extension flag "--x" requires a value`); unclaimed collapse into
    one `Unknown option(s): --a, --b` error.
  - Diagnostics: warnings print to stderr, error-level entries abort
    startup with the joined `Error: …` lines and the
    `"pi -ne"` hint (`main.ts:982-990`).
  - Tools: `select_tools(args, builtin, extension)` — `--no-tools`
    drops everything (`noTools: "all"`), `--no-builtin-tools` keeps
    extension tools (`noTools: "builtin"`), all-names `--tools`
    allowlists both, the modifier form keeps extension tools
    (`allowedToolNames` undefined, `sdk.ts:287-291`), `--exclude-tools`
    always removes. `PrintRuntime.executor` is `Arc<dyn ToolExecutor>`;
    the composite validates extension calls against their schema and
    dispatches through `ExtensionRunner::execute_tool` behind a mutex.
- **E2 slice-3 deviations/interpretations**:
  - rquickjs runs with the `parallel` feature (Send `Runtime`/`Context`)
    so the runner can live in `Arc<Mutex<_>>` next to the agent loop;
    the per-runtime lock is uncontended on the single-threaded host.
    Error listeners and host-action closures carry `Send`.
  - `--tools` unknown names still error (the P4 behavior; upstream
    silently ignores unknown allowlist names), and the message's tool
    list now includes extension tools.
  - `+exttool` does not activate a default-inactive extension tool and
    `-exttool` cannot remove one (upstream's modifiers apply to the
    builtin base only; `-` removal parity, `+` activation skipped) —
    the full active-loadout machinery is E3.
  - Flag diagnostics iterate name-sorted (`BTreeMap`), not CLI capture
    order (upstream `Map`) — same content, different order.
  - The runner's `active_tools` (`pi.getActiveTools`) reflects bind()'s
    default set, not the CLI-filtered declared set.
  - Post-bind tool registrations do not refresh the runner's dispatch
    snapshots (`/reload`/E3 concern).
  - `session_start` was not emitted by slice 3 — it lands with the
    slice-4 loop wiring (below); commands dispatch with the consuming
    hosts.
  - Interactive mode prints startup warnings to stderr (upstream shows
    them in the TUI).
- **Slice 4 — runtime events** (on `feat/e2-runtime-events`,
  2026-11-15): the runner is now live at every runtime seam.
  `extensions::load` takes the mode's UI surface and binds it **before**
  emitting `session_start` (mirroring upstream `bindExtensions` applying
  bindings then firing the event, `agent-session.ts:3429-3449`), so
  `session_start` handlers can already prompt; flags/tools collection
  happens after the emit, so event-time registrations count.
  Interactive and print/json stay UI-headless (no sink bound); only RPC
  mode bridges one.

- **user_bash** (`extensions::user_bash`; upstream dispatch
  `runner.ts:1279-1300`, executor `bash-executor.ts:48`): interactive
  `!` bash consults `user_bash` handlers first. `Operations` →
  `call_bash_exec` + `execute_bash_with_operations` (per-chunk
  stripAnsi→sanitizeBinaryOutput→`\r` removal, rolling 2×
  `DEFAULT_MAX_BYTES` window counted in chars ≈ JS `.length`,
  `truncate_tail`, temp-file spill above the threshold); `Result` → the
  extension's payload verbatim; handler failure → `show_error` with
  **no local fallback** (upstream returns early); no handlers → local
  execution. Fidelity fix: interactive transcripts now record the real
  output in `!` entries — they stored `output: ""` before, while
  upstream's `recordBashResult` keeps it and `bashExecutionToText`
  renders it (`agent_loop.rs` bash tool context).
- **Session gates** (`extensions::emit_gate`, cancel =
  `result.cancel == true`, last handler wins): `session_before_switch`
  `{reason:"new"/"resume", targetSessionFile?}` at `/new` and `/resume`
  with `session_shutdown` emitted after a non-cancelled gate;
  `session_before_compact` `{preparation, branchEntries, customInstructions,
  reason:"manual", willRetry:false}` at `/compact` (cancel-only);
  `session_shutdown` `{reason:"quit"}` at interactive-mode end.
  `branchEntries` are the raw JSONL records (new `Store::get_branch_records`)
  so extension-visible key order matches the file.
- **Context transform** (`extensions::context_transform`): an
  `Arc<dyn TransformContext>` is installed at all four `AgentLoopConfig`
  sites (print/json/rpc/interactive) when any `context` or
  `context_with_system` handler exists; `emit_context` errors warn to
  stderr and keep the input.
- **RPC dialog subprotocol** (`modes/rpc.rs`): a stdin reader thread
  routes `extension_ui_response` frames to per-request one-slot channels
  (registered at request time so a fast response cannot race; late
  responses after timeout are dropped) and everything else to the command
  channel the synchronous loop consumes — dialogs block inside command
  handling without deadlocking the transport. Every writer (command
  loop, agent-event sink, dialog sink) takes the shared stream lock per
  line — never across a call that can write again, or an extension
  dialog opened mid-command would deadlock against the command's own
  write guard. Dialog methods write
  `{"type":"extension_ui_request", id, method, …}` and wait — response
  channels register for the dialog methods only, and the sink is
  transport-level (the whole `extension_ui_response` frame crosses; the
  vendored glue's per-method `parseResponse` extracts the payload, so
  `select`/`input` read `.value`, `confirm` reads `.confirmed` with
  `false` never undefined);
  `timeout` → `recv_timeout` → `Value::Null` (the JS glue parses
  null/empty payload to the method default, matching upstream's `input`
  contract); `custom` resolves immediately; fire-and-forget methods
  (notify/setStatus/setWidget/setTitle/set_editor_text) register nothing
  and only write. EOF
  on stdin closes the command channel → shutdown (upstream
  `onInputEnd`).
- **Slice-4 deviations/interpretations** (user_bash etc. above are the
  landed behavior):
  - Virtual-model routing (`ModelRouteRequest` user/continuation/retry/
    direct, `pi.virtual-model-state` entries) and provider
    OAuth/streamSimple handling are **deferred to slice 5**; the slice-2
    provider seam stands in.
  - Fork/tree session gates are unreachable in the runtime (no fork/tree
    yet) and were not ported; extension-provided compaction results are
    not plumbed (`/compact` gates are cancel-only).
  - `session_before_compact` omits upstream's `signal` field (not part
    of our `CompactionPreparation`).
  - Headless sink fallback (no UI bound, e.g. interactive): dialog
    methods resolve with an empty payload → the method default; `custom`
    resolves immediately; fire-and-forget requests are silently dropped.
    Same semantics as the runner's pre-slice-4 behavior, now explicit.
- **Slice 5 — virtual-model routing** (on `feat/e2-slice5-virtual-models`):
  the full port of `core/virtual-models.ts` plus its routing consumers:
  `crates/pi-coding-agent/src/virtual_models.rs` (catalog construction
  `createVirtualModel`, `findLatestResponse`, `getVirtualModelState`,
  `getBranchSelection`, `withVirtualModels`' list semantics, `resolveModel`
  → `resolve_virtual_model` with `model-runtime.ts:1040-1071`'s verbatim
  errors), `clampThinkingLevel` (`models.ts:1315-1334`) in
  `pi-agent/src/types.rs` (+ `ThinkingLevel::EXTENDED_ORDER`), the
  `pi.registerVirtualModel`/`unregisterVirtualModel` API + route
  dispatch in `pi-ext-js`, and `TurnRouting` (prepare hook + routed
  stream fn + state drain) installed at all four loop sites
  (print/json/rpc/interactive). Selection plumbing: `resolve_model_with`
  carries virtual entries (`withVirtualModels`), `available_models`
  appends them outside the auth filter and hides same-id physicals,
  `/model` completions and `apply_model` (routed-dispatch stream fn)
  follow, interactive compaction routes its summary request with reason
  `direct`, and startup (`main.rs`) skips the credential ladder for a
  virtual selection (the routed provider's credentials decide).
  Bridge mechanics (upstream semantics, port mechanisms):
  - Route functions cannot cross `JSON.stringify`, so the definition's
    `route` stashes JS-side (`__piVirtualModels`, keyed
    `provider\0id`) with per-extension ownership records cleaned at
    `__piDiscardExtension` (identity-checked, so a live extension's
    re-registration survives its own discard); the host calls
    `runner.route_model(provider, id, request_json)`, which runs the
    session protocol with kind `"route"` and settles the router's reply.
  - The branch store is not shared with the loop thread, so
    `VirtualRouteHook` keeps an in-memory state overlay seeded from the
    branch; decided `pi.virtual-model-state` entries queue and the host
    appends them at turn end (`persist_state`).
  - The prepare hook cannot fail the request, so a routing failure is
    stored in a shared `RouteErrorSlot` and `RoutedStreamFn` surfaces it
    as the request's stream error → the run ends with an error response
    (upstream: routing failure rejects).
  - `RoutedStreamFn` dispatches per request through the routed model's
    own provider adapter, re-resolving credentials through the ladder
    (the `--api-key` flag stays scoped to the selected provider) and
    failing unrouted virtual requests with the TS message.
  Tests: 4 `pi-ext-js` route tests (stash + settle + error paths) and
  11 `virtual_models` tests (pure helpers + 2 `ext-js`-gated
  `resolve_virtual_model` integration tests). The getBranchSelection
  fallback semantics were corrected against upstream while testing:
  a `model_change` only holds when the model is still registered
  **and** virtual (`change && model && isVirtualModel(model) ? change :
  response`); otherwise the last physical response wins.
- **Slice-5 deviations/interpretations**:
  - Request reason `retry` is unreachable (no retry subsystem yet); the
    `failed` input is plumbed but nothing produces it.
  - Upstream's compaction-threshold re-prepare (routing a model that
    crosses the threshold re-runs `resolveModel`) is deferred — the
    threshold check keys on the routed model in a later slice.
  - `entry_appended` is not emitted for the `pi.virtual-model-state`
    appends (upstream emits it; the port's SessionManager append path
    has no event yet).
  - No abort signal crosses into `route()` calls (the runner's calls are
    synchronous; upstream passes `request.signal`).
  - Router state persists at end of turn, not mid-turn (mechanism-only;
    within a turn the overlay reads exactly as the branch would).
  - Routed-target and `previous`/`failed` lookups read the static
    catalog only — **CLOSED by slice 6b** (every lookup goes through
    `models_state::lookup`).
  - `create_virtual_model` defaults a missing `name` to `""` (TS would
    carry `undefined` through).
  - `TurnRouting` installs per turn (model is fixed per config) where
    upstream installs the wrapper unconditionally per session —
    observable behavior identical.
- **Slice 6a+6b — dynamic providers: models.json + composed catalog**
  (on `feat/e2-slice6-providers`, landed `66bcc7c`/`fe786fc`/`ae0f2ec`,
  CI 43902366391 success; design in docs/EXTENSIONS.md §8.1):
  - `models_config.rs` — `core/model-config.ts` plus the models.json
    halves of `core/provider-composer.ts`: JSONC loader (BOM strip,
    `//` comments, trailing commas — exactly upstream's two passes) with
    errors-as-values, model definitions with schema defaults
    (128000/16384), `modelOverrides` with the per-field and
    nested-routing-key (`openRouterRouting` et al.) merges, verbatim
    composition errors, the `"radius"` oauth rule.
  - Extension half: `apply_extension`/`extension_model_from_definition`
    (`provider-composer.ts:277-356`) with verbatim errors. The registry
    config JSON carries only the model subset — `streamSimple`/`oauth`
    functions are dropped by `JSON.stringify`; their JS-side stashes
    land with 6d/6c.
  - `models_state.rs` — the composed snapshot (base catalog ⊕
    models.json ⊕ extension providers ⊕ virtual models), rebuilt when
    `ProviderRegistry::generation` moves and read by every model lookup:
    print/json/rpc resolution, `--list-models`, `/model` completions,
    `available_models`, route resolvers, settings thinking rows. Before
    the first refresh the static catalog is the answer (engine-free
    shape preserved).
  - Deviations: port's own message text in the `- path: message` layout
    (TypeBox wording not replicated); `ModelCostSchema` required rates
    relaxed to serde defaults; native provider objects are data-only
    (models must come through config); composition errors surface via
    `models_state::errors()` (the registry error-state UI lands with 6c).
- **Slice 6e — auto-retry machine** (part 1 `9910fac`, part 2
  `16eca4f`+`6e263fa`, CI 43904515847 + 43907827713 success):
  - `pi-ai/retry.rs` — the classifier + policy (`retryAssistantCall`):
    `RetryPolicy`, verbatim non-retryable/retryable message patterns
    (quota/billing vs 429/5xx/socket codes; fancy-regex pinned to "0.16"
    — 0.14 mis-dispatches alternations mixing lookarounds with plain
    branches), exponential backoff with the `MAX_SAFE_INTEGER` clamp,
    `is_retryable_assistant_error` (non-retryable wins).
  - `modes/retry.rs` — the `agent-session.ts` state machine
    (`_retryAttempt`, `_willRetryAfterAgentEnd` :1266-1281,
    `_handlePostAgentRun` :2060-2094) as a controller the run tails
    drive: `will_retry`/`schedule` (attempt, maxAttempts, delayMs,
    errorMessage for `auto_retry_start`)/abort-aware `sleep` (50 ms
    slices)/`reset` after success; the failed response is never
    persisted.
  - Wired into the print/JSON/RPC run tails: the same prompt re-runs on
    a retryable error within budget (defaults enabled, 3 retries, base
    2000 ms, max 60 s — `RetrySettings` from settings);
    `auto_retry_start`/`auto_retry_end` emitted through each mode's sink
    (new `AgentEvent` variants) and mapped on the JSON wire (camelCase).
  - Deferred: the interactive pump's retry integration lands with its
    `AgentRun` rework (no-op arms in place); upstream's willRetry merge
    into `agent_end` likewise; the route reason `retry` re-resolve
    (slice-5 deviation above) stays open for the same reason.
- **Slice 6d — extension `streamSimple`** (on
  `feat/e2-slice6d-stream-simple`, landed `a51403f`..`10ad86e` after
  rebase, CI 43914127654):
  - JS-side: `registerProvider` now validates the api field with
    upstream's `validateExtensionProvider` message
    (`provider-composer.ts:544-548`) before storing, stashes the
    `streamSimple` fn in a `__piProviderStreams` map on `globalThis`
    (the virtual-models route-fn mechanism), and crosses the config
    with a `streamSimple: true` marker over a shallow copy —
    re-registration without the key keeps the previous handler, like
    the per-key config merge; `unregisterProvider`/`__piDiscardExtension`
    clean up with identity checks.
  - Runner: `provider_stream` marshals the request (model, context
    messages, options with apiKey/accessToken/clamped reasoning/
    sessionId), drives the handler's iterable through the job queue,
    and settles with `ProviderStreamOutcome{events, error}` — pre-error
    events are kept, matching `lazy.ts` setup-failure semantics.
  - Composer: `models_state::stream_with_extension` + `ComposedStreamFn`
    implement the `streamWith` ladder (`provider-composer.ts:588-613`):
    extension arm first (`model.api === extension.api`), the caller's
    base adapter as fallback; installed at the physical-selection sites
    (print/interactive) and behind the routed dispatch for virtual
    models. Engine-free builds unchanged (`#[cfg(feature = "ext-js")]`
    both ways).
  - Deviation (mechanism only): events replay after the handler
    finishes — the whole iterable is drained inside the engine (no
    incremental crossing of the QuickJS boundary); ordering and
    terminal-event semantics unchanged. No abort signal (existing E2
    deviation). QuickJS-NG async generators confirmed by the gate's
    runner tests.
  - Not yet marshalled: OAuth (6c) and the `options` callbacks
    (`onPayload`/`onResponse`/`onProviderStreamEvent`).
- **Slice 6c — OAuth auth composition, `/login`/`/logout`** (on
  `feat/e2-slice6c-oauth`, landed `09ec2a0` + fix commits as `31cc578`
  after rebase, CI 43919589348):
  - `oauth.rs` — the auth composition (`composeApiKeyAuth`/
    `composeOAuthAuth`/`adaptOAuth`, `provider-composer.ts:422-511`) +
    `AuthStatus`/auth-method menus: stored-credential type gating
    (`auth/resolve.ts:33-93` — the stored credential owns the provider,
    no env fallback on type mismatch), `envApiKeyAuth` display names per
    provider (`auth/helpers.ts:8-33`), `compareProviders` with
    locale-collation name order (`core/preferred-providers.ts:17-23` —
    upstream's `localeCompare` is case-insensitive on the primary key;
    the byte comparison leaked uppercase-first order), the login
    provider completions (`interactive-mode.ts:398-433` incl. the final
    `compareProviders` sort).
  - `auth_selector.rs` — the auth-type/provider selectors and the
    API-key prompt (`oauth-selector.ts`, `login-dialog.ts:229-257`);
    typed keys filter without consuming (`InputAction::None`, pi-tui
    `Input` semantics — printable input is never a consumed action).
  - `/login [provider]` (auth-type menu with Radius last, provider
    selector with search/status, API-key prompt → `AuthStorage::modify`,
    verbatim status lines) and `/logout` (stored-credential selector,
    verbatim empty/removed messages), plus `/login` argument
    completions.
  - Wire ladder: `resolve_wire_credential_composed` consults the
    composed config snapshots — `models_state` now mirrors
    `model-runtime.ts`'s per-auth config layers
    (`models_json_provider`/`extension_provider_config`).
  - Recon finding: upstream has NO auth RPC dialogs — nothing to port on
    the RPC side.
  - Deviations/deferred: per-provider OAuth flows
    (`packages/ai/src/auth/oauth/*.ts`) not ported — oauth login errors
    with the upstream error shape; `--api-key` stays unconditional (P4
    behavior); display names fall to provider id; login list is
    model-derived (apiKey-only models.json providers missing);
    bedrock/vertex/cloudflare custom logins → ambient notice; token
    refresh + `OAuthLoginCallbacks` over the JS host now unblocked by
    6d's function stashes.
- **Slice 6f — automatic threshold/overflow compaction** (on
  `feat/e2-slice6f-auto-compaction`, landed `38d83f1`+`fc639be`, CI
  43922308438):
  - `pi-ai/overflow.rs` — the verbatim classifiers
    (`packages/ai/src/utils/overflow.ts:39-188`): 28 provider overflow
    patterns, the 3 non-overflow exclusions (rate-limit/Bedrock
    throttling — the exclusion matches formatBedrockError's
    human-readable prefix; the raw `ThrottlingException:` string
    genuinely classifies as overflow upstream), the Cerebras bodyless
    pattern, the silent-overflow usage check (`input + cacheRead >
    contextWindow`), the Xiaomi MiMo length-stop case (≥ 0.99×window,
    zero output), and `isRecoverableLength`.
  - `modes/compaction.rs` — the `agent-session.ts` machine as a run-tail
    controller (the `retry.rs` precedent): `_compactBeforeNextAssistantResponse`
    (:839-848, threshold gate keyed on the model about to answer),
    `_checkCompaction` (:3119-3252 — overflow compact-and-retry ONCE with
    the `_overflowRecoveryAttempted` guard and verbatim one-shot failure
    messages, overflow without retry preserving the completed response,
    threshold case 3's estimate ladder incl. the stale-usage guards), and
    `_runAutoCompaction` (:3265-3412 — prepare → summarize → append →
    `compaction_start`/`compaction_end` events with the reason enum and
    the verbatim failure prefixes).
  - Wired into all three run tails; upstream `_handlePostAgentRun`
    (:2062-2098) order preserved — the retry decision first, compaction
    only when no retry was scheduled (overflow errors are not in the
    retryable set, so they fall through); the failed overflow attempt is
    never persisted. New `AgentEvent::CompactionStart/End` +
    `CompactionResultEvent` (camelCase wire mapping in json_event.rs).
  - Closes the slice-5 deviation "compaction-threshold re-prepare keys on
    the routed model" for the engine-free shape (the pre-response gate);
    the ROUTED re-check inside the prepare hook (:901-904, virtual
    selections) stays deferred with that rework. Extension compaction
    gates (`session_before_compact`/`session_compact`/
    `session_compact_failed`) ride the E3 bridge; the queued-messages
    continue is the run tail's loop structure; the interactive pump keeps
    deferred arms.
- **Slice 6g — `entry_appended` for router states + the route abort
  signal** (on `feat/e2-slice6g-entry-appended`, landed `f0407a9`+
  `c99c2f6`, CI 43923807214):
  - Recon correction: upstream emits `entry_appended` only for the
    appends the agent-session drives (`agent-session.ts:224`, cache
    warmer :531, virtual-model-state :897, boundary drafts :1090,
    recovery omissions :1336, extension `appendEntry` :3578) — NOT for
    regular messages or model/thinking/compaction records, which surface
    through their own events. The port's only gap was the
    virtual-model-state append (the slice-5 deviation), now closed:
    `TurnRouting::persist_state` returns the serialized records
    (byte-shaped via the record's ordered object) and the run tails emit
    `AgentEvent::EntryAppended` for each. Cache warmer/boundary drafts/
    extension appendEntry ride P7/E3.
  - Route abort signal: `PrepareRequestContext` carries the live run's
    signal (`prepareRequest(request, signal)`, `agent-loop.ts:222`) and
    `ModelRouteRequest` gains `request.signal` (`agent-session.ts:888`).
    The runner's calls are synchronous, so `resolve_virtual_model` polls
    at the dispatch boundary — an aborted request fails with the DOM
    abort message ("This operation was aborted") instead of reaching the
    router. Out-of-run resolutions (`direct`) pass no signal.
- **Slice 6h — retry/overflow omission fidelity** (on
  `feat/e2-slice6h-retry-omission`, landed `5d6f373`+fmt as `0bbba15`,
  CI 43924869153):
  - Corrects the 6e/6f mechanism deviation: upstream keeps a failed
    retry/overflow attempt in RAW history and omits it from the model
    projection via `context_edit` records (`_omitRecoveryAttempt`
    :1311-1336 — the failed assistant plus its orphaned tool results,
    `replacement: null`; `_prepareRetry` omits before the backoff
    :3959-3963; the overflow recovery omits before compaction
    :3209-3211). The tails never persisted the attempt — same
    projection, different session bytes.
  - `modes/retry.rs` gains `omit_attempt`: persists the failed assistant
    and the tool results after it whose ids match its tool calls, appends
    the omission edits, and returns the serialized records for the
    `entry_appended` emissions; all three tails call it in the retry arm
    (after `schedule`, before `auto_retry_start`) and the overflow-retry
    arm (before the recovery compaction). Budget exhaustion and the
    one-shot recovery failure leave their last attempt raw and projected,
    like upstream.
- **Slice 6i — the interactive pump's post-run machine** (part a
  `080194e`, part b `5821acf`+`3bb367f`, CI 43926399053 + 43927863903):
  - `agent_end` carries `willRetry` on the wire (`agent-session.ts`
    :1211 merges `_willRetryAfterAgentEnd` into the emit; :212-216).
    The core loop emits `false`; every run tail streams through a
    `RetryAwareSink` (`modes/retry.rs`) that computes the same verdict
    from the machine's budget — the run thread copies it by value
    (attempt count + settings), so the JSON wire and the pump see the
    merged field.
  - The pump retires its one-shot run thread and decides on the UI
    thread, which owns the session: retry first (`_prepareRetry`
    :3938-3975 — budget the attempt, omit the failed attempt from the
    projection via the branch-side `_omitRecoveryAttempt`, show the
    `RetryStatusIndicator` (warning spinner; the countdown renders as
    the initial seconds), sleep abortably on a wait thread, then re-enter
    `spawn_run` with the retired turn's prompts and context
    (`agent.continue()`)); interrupt cancels the backoff
    (`session.abortRetry`) and reports the verbatim cancelled-retry
    failure (:3922-3932). When no retry was scheduled, the 6f compaction
    machine decides: overflow recovery omits and compacts, then re-runs
    once (`willRetry` → the flush path); the manual compaction command
    now reads its settings through the mode's `SettingsManager`.
  - The pump's run thread wraps its channel sink with the retry-aware
    verdict too; the display arms land directly on the UI thread (the
    events an E3 bridge would need have their emission points here).
- **Slice 6j — JS-host OAuth** (on `feat/e2-slice6j-js-host-oauth`,
  landed `562060f`+`8b1b228`, CI 43934135916): the oauth half of the
  extension provider surface (`adaptOAuth`,
  `core/provider-composer.ts:358-377, 488-511`; the callback surface
  `packages/ai/src/compat/extension-oauth-types.ts:39-47`).
  - Engine bridge (pi-ext-js): the merged provider config's `oauth`
    object (`login`/`refreshToken`/`getApiKey`) is stashed JS-side with
    the same ownership/discard cleanup as the route and `streamSimple`
    stashes. `oauth_login_start` runs the stashed `login` in a dispatch
    session whose adapter callbacks cross: one-way notifications ride
    the `oauthNotify` op (the `auth_url`/`device_code`/`progress`
    envelopes `adaptOAuth` builds), the prompt callbacks
    (`text`/`manual_code`/`select`) suspend the flow on promises the
    host resolves token-verified through `oauthResolvePrompt` (the ask's
    token echoes back; a JSON `null` answer resolves JS `undefined`, so
    a cancelled select drops its key from the settled credential via
    `JSON.stringify`), `oauthAbortSignal` flips a plain object the
    handler polls, and the settled credential comes back
    `{...credential, type: "oauth"}`. `oauth_refresh`/`oauth_api_key`
    are the one-shot `refreshToken`/`getApiKey` arms. 9 runner tests.
  - Credential ladder: `resolve_wire_credential_composed`'s stored-oauth
    arm resolves through the stashed `getApiKey(credential)` for
    providers with extension oauth metadata (upstream's `toAuth`,
    :375) — `models_state::set_extension_runner` shares the engine and
    `extension_oauth_api_key` calls it. A handler that is gone or fails
    resolves to nothing (the provider reports unconfigured) instead of
    falling back to the raw access token — upstream's `toAuth` rejection
    fails request setup; the port's ladder has no error channel
    (deviation). Built-in providers keep the raw-token behavior. 3
    tests.
  - `/login` pump: `start_provider_login`'s oauth arm tries the
    extension bridge first (the built-in flows keep the unported error).
    The flow polls through the pump: notifications surface as status
    lines, `text`/`manual_code` asks mount the login prompt, `select`
    asks mount the option selector (the picked label maps back to its
    id), escape aborts + ends the session and returns to the `onBack`
    target, and the settled credential lands in auth.json
    (`AuthStorage::modify`) with the `Logged in to {name}. Credentials
    saved to {path}` status. End-to-end test. Deviation (presentation
    only, deferred to the E3 TUI slice): upstream mounts the rich
    `LoginDialogComponent` (`login-dialog.ts` — auth-url hyperlinks,
    device-code copy/open-browser hints); the port reuses the existing
    prompt/select components until then.
- **Slice 6k — stream options instrumentation** (on
  `feat/e2-slice6k-stream-options`, landed `1605bdd`+fmt as `921834c`,
  CI 43937212449): closes 6d's "not yet marshalled" note — the
  fn-valued `SimpleStreamOptions` members pi sets on every run
  (`sdk.ts:424-426`: the three hooks bridging the
  `before_provider_request`/`after_provider_response`/
  `provider_stream_event` extension events) now reach extension
  streamSimple handlers, whose contract requires calling them
  (`custom-provider.md:195-198`).
  - The stream glue decorates the marshalled options (functions cannot
    ride JSON): `onPayload` suspends on a promise the drain resolves
    with the `before_provider_request` transform chain's final value
    (`emitBeforeProviderRequest`, `runner.ts:1375-1398` — a fresh event
    per handler, a non-undefined return replaces, handler errors
    report), `onResponse`/`onProviderStreamEvent` queue through the
    `providerHookEvent` op and emit FIFO, and `options.signal` is a
    plain object the drain flips once the run aborts. `StreamRequest`
    carries the run's live signal (`agent-loop.ts:414`'s spread;
    `pi_agent::AbortSignal` is an `Arc<AtomicBool>` flag, so a clone
    observes the run's abort); models_state passes it as a reader
    closure — pi-ext-js stays `pi-agent`-free.
  - Deviations: the one-way hooks queue and emit between job rounds, so
    a handler awaiting them resumes before the fan-out finishes
    (upstream awaits the emit; emission ORDER is preserved — the
    per-chunk round-trips would multiply drains); the built-in pi-ai
    adapters still do not invoke the hooks (the P1 adapter subset —
    only extension handlers see them for now; adapter-side hooks are a
    P7 candidate); `options.env` (contract :198, provider-scoped
    environment) has no producer in the port and does not cross.
  - 6 tests: the runner set (full instrumentation with bridged-event
    assertions, no-listener pass-through, chain feed-forward across
    three listeners with an undefined return keeping the payload,
    signal flip vs live) and the composed-path test driving the
    marshalled signal through `stream_with_extension`.
- **Next**: P5 remainder (RPC envelope reconciliation, MCP, package
  manager) → P7 hardening (incl. pi-ai adapter payload/response hooks)
  → E3 TUI bridging after.

## Golden session fixtures (landed on `feat/golden-fixtures`, 2026-11-16)
- **P4 EXIT CRITERION MET**: five sessions generated by the real TS pi
  1.5.2 (npm-installed into a temp prefix; llama-swap `127.0.0.1:11111`
  as an `openai-completions` provider; `PI_CODING_AGENT_DIR` + session
  dirs under temp dirs; synthetic prompts only): plain exchange, `read`
  tool round-trip, model+thinking switch, named multi-turn, resumed
  mid-branch thinking switch.
- `tests/golden_sessions.rs`: per fixture — byte-equality of
  load→serialize on the raw `Record` path, typed view decodes
  kinds/order plus pinned fields, `SessionManager::open` on a copy does
  not rewrite the file, branch records re-serialize to the file body.
- **Finding**: the typed `SessionEntry` re-encode is NOT byte-exact (TS
  serializes in write-site order; the port's declaration order differs;
  `SystemMessage.sections` alphabetizes via `BTreeMap`). The canonical
  record path is exact, so on-disk compat holds; typed-order fidelity is
  open (P7 candidate).
- Upstream print mode cannot switch models mid-session (`setModel` is
  interactive-only), so the model-switch fixture used CLI flags. Fixture
  01's `responseModel` carries the local gguf path llama-swap echoes —
  not a credential; left verbatim for byte fidelity (private repo).

## Quick-wins slash commands (landed on `feat/quick-wins-commands`, 2026-11-16)
- `/copy` `/name` `/session` `/hotkeys` `/changelog` ported 1:1
  (`4e0b068`→`4e32bf9`, CI 43902058878 success). New modules:
  `changelog.rs` (`utils/changelog.ts` incl. the hand-written
  lookahead-free legacy-link rewrite), `clipboard.rs`
  (`utils/clipboard-command.ts` + the write half of `utils/clipboard.ts`:
  platform command ladder, OSC 52 with the 100k base64 cap, WSL interop
  route, upstream error strings verbatim), `session/cache_stats.rs`
  (`core/cache-stats.ts`: miss detection, waste totals, catalog-backed
  price source). Footer gained `get_usage_cost_breakdown`; theme gained
  the four display-only keybinding actions with upstream defaults.
- **`/bug` deliberately NOT ported** (needs bug-report bundle/zip/upload/
  crash-log infra). Policy for when it lands: **ZIP-EXPORT ONLY — never
  upload to upstream's Radius gateway**; pi-rs bug reports are ours.
- Deviations (commented in code): `/session` cache-warming always shows
  "unavailable" (no cache warmer yet); native-clipboard branch skipped
  (no Rust binding — platform tools are upstream's own fallback);
  `/changelog` ships no bundled `CHANGELOG.md` (empty state);
  extension-shortcuts hotkeys section lands with E3; number grouping is
  en-US fixed (upstream uses host locale); ctrl+x copy binding
  registered but not wired.
- Flagged decision: the port's `show_error` lacks upstream's `Error: `
  prefix (`interactive-mode.ts:4811-4814`) — new code matched upstream
  via an own `show_warning`; unify later.

## P5 package-manager subset (landed on `feat/p5-package-manager`, 2026-11-16)
- Ports the git + local source arms of `package-manager.ts` verbatim
  (`efc51a6`, CI 43950494495 success): `parseSource` over utils/git.ts
  (`parseGitUrl`/`splitRef`/`buildGitSource`), install/remove/
  installGit/ensureGitRef/getLocalGitUpdateTarget/removeGit, settings
  persistence (`addSourceToSettings`/`removeSourceFromSettings`;
  normalized local paths store relative to the scope base dir —
  project base `cwd/.pi`), `listConfiguredPackages`, the match keys,
  and the runCommand/runCommandCapture machinery with verbatim
  failure/timeout texts. CLI (`cli/package_command.rs`): install/
  remove/list/update with upstream's verbatim help texts, both trust
  gates (the PM storage text vs the CLI config text), and the
  SettingsManager flush contract — writes queue; the handler flushes,
  then reports `drain_errors`.
- **npm arm deferred with honest per-call errors** ("npm sources are
  not implemented in pi-rs yet"): registry resolution, tarball
  installs, the managed-install machinery (`:1357-1570`), `pi update`'s
  temporary-scope machinery, and `getInstalledPath`'s legacy global
  fallback all sit behind it. `build_no_matching_package_message` +
  `find_suggested_configured_source` (the suggestion form) serve
  `resolve()`'s missing-source callback — `#[allow(dead_code)]` until
  that arm lands; the `pi remove` CLI prints the plain no-match form
  directly, as upstream's CLI does.
- Deviations: chalk → plain output (upstream's chalk degrades to plain
  on non-TTY stdout and CLI subcommands run before any TUI exists);
  the clean-failure progress event in cleanAndInstallGitDependencies
  has no receiver (progress callback dropped — module doc).
- Tests: 12 unit tests including a live git e2e (clone → update-in-place
  fetch/reset via `@{upstream}` → removal with parent pruning) and a
  container smoke of the real binary (install → settings.json persist
  → list → remove → list-empty).

## P5 RPC envelope reconciliation (landed on `feat/p5-rpc-envelope`, 2026-11-16)
- Agent-written slice (two commits + gate fixes, `1cddbc0`, CI
  43951436483 success). The wire now matches `modes/rpc/` upstream
  (`rpc-types.ts`/`rpc-mode.ts` — note `rpc-entry.ts` is only the
  process bootstrap): the discriminant IS `type` (not the port's
  invented `{type:"command", command}` envelope), response frames
  `{type:"response", command, success, data?|error}` with absent keys
  dropped, upstream parse-error/unknown-command texts
  (`Failed to parse command: <msg>`, `Unknown command: <type>`,
  missing type → `Unknown command: undefined`), `steer` (upstream
  name; was `steerable_prompt`) with real queueing and
  `{disposition:"queued"}`, `follow_up` (was `followup_prompt`),
  `prompt`'s `{disposition:"started"}` response emitted at preflight
  BEFORE the event stream with post-preflight errors swallowed,
  `shutdown`/`command_schemas()` removed (they do not exist upstream —
  stdin EOF ends the mode), `set_model` with the full `Model` data +
  transcript record, `get_state` carrying the full `RpcSessionState`
  (optional keys dropped when unset), thinking-level CLAMP semantics
  (invalid levels clamp, never reject), and 15 added commands
  (`clear_queue`, `get_available_models`, `cycle_thinking_level`,
  `compact`, `get_entries`, `get_tree`, `set_session_name`,
  `get_fork_messages`, …), each mirroring its `handleCommand` case
  with verbatim error strings.
- pi-agent gained `agent_settled`/`queue_update`/
  `thinking_level_changed`/`session_info_changed` event variants and
  `duration_ms` on `tool_execution_end` (the invented `args` field is
  gone); all three run tails emit `agent_settled`.
- **Documented deviations** (in `rpc.rs`): serial dispatch (upstream
  processes stdin lines concurrently) — no in-run steer/abort latency;
  prompt has no extension-command execution, skill/template expansion,
  auth precheck, or preflight-failure path; `queue_update` is not
  re-emitted at delivery time; 9 commands parse as Unknown (machinery
  absent: new/fork/clone/bash/session stats/export/commands);
  wrong-typed payload fields read as absent rather than JS throws;
  null request lines answer `Unknown command: undefined` (upstream
  crashes).
- Beyond RPC: blank stdin lines now produce the upstream parse-error
  response (previously skipped); JSON mode emits the session header
  first line (print-mode.ts:129-134 gap).
- Tests: 12, including two e2e runs over a pipe pair (full state
  frame + dropped keys; steering delivered mid-run with the queued
  disposition, prompt response before events, `agent_settled` last).

## P5 MCP server-manager + tool bridge (landed on `feat/p5-mcp-server-manager`, 2026-11-16)
- Agent-written slice (+ gate fixes, `6d32c2f`). Server lifecycle
  mirrors the bundled MCP extension: connections open at
  `session_start` (the host now calls the previously-missing
  `runner.bind()` — a fidelity fix; production never bound the
  runtime), then `connect_mcp_servers()` reconciles the registry
  against connection states (enabled servers connect through the new
  `pi-ext-js::mcp_manager::McpManager` honoring `config.timeout ?? 60`;
  unregistered/re-registered servers drop with their tools
  re-registered `hidden`; disabled report `disabled`). Post-bind
  registrations queue and drain at the end of every dispatch session
  into one `mcp_servers_change` reconcile per change (re-entrancy
  guarded); `shutdown_mcp()` closes everything at session_shutdown.
- The tool bridge registers through the REAL extension surface: glue
  `__piMcpRegisterTools` stores definitions in a synthetic
  `builtin:mcp` record whose `execute` wrapper forwards to
  `mcpCallTool` — first-wins aggregation, exposure, active-set,
  validation, and `execute_tool` dispatch all behave extension-shaped.
  Naming is `createMcpToolName` (sanitize, 64-char cap, sha256-8
  suffix including the Codex rule that ALL in-list sanitize collisions
  get the suffix); exposure = `getMcpToolExposure` → `toToolExposure`
  (codemode → deferred); `convertMcpResult` is a line-by-line port
  (text/image/audio/resource_link/resource blocks, empty-error
  fallback text, 20 KiB middle truncation with a full-output temp
  file, `details {server, tool, fullOutputPath?}`, structuredContent
  = whole `CallToolResult` minus `_meta`).
- E1 `mcp_registry.rs` fixes corrected here: exposure/auth field
  validation with exact error strings, the `codemode-deferred` alias,
  the resolved (not raw) config copy stored, `unregisterMcpServer` as
  a silent no-op, and load-time registrations deferred through
  `pendingChanges` so a failed extension's server dies with it.
- Deviations (recorded): connects are synchronous at session_start
  (upstream connects in background with a first-prompt ≤10 s wait) —
  a hanging server delays startup by its timeout; `mcp_servers_change`
  fires only at the next dispatch end; no stderr tail on connect
  failures; `tools/list_changed` notifications unhandled; URL-pattern
  globs use an in-order literal scan; block-level `_meta`/`annotations`
  do not survive the decode. Open items with citations: Streamable
  HTTP transport + OAuth, resources, `/mcp` manager UI, `mcp.json`
  loading, `startupWaitMs`, the `mcp_servers` system-prompt section,
  `pi mcp list`.
- Tests: 24 new (config-value resolution incl. `!cmd`/`$$`/`$!`,
  result mapping + truncation, naming, validation, and five full
  runner pipeline tests through the JS bridge over a bash fixture
  server answering initialize/tools list/call/ping).

## User-test round: /settings freeze, footer thinking, /login catalog (landed 2026-11-16)

User tested the release binary side-by-side with upstream 1.5.2 and
reported four issues. Three fixed and landed (main `8697bd5`, all CI
green); the fourth pair was killed mid-flight (below).

- **`/settings` FREEZE** (`39408c1`, CI 43961022722): reported as a
  crash, actually a **self-deadlock** — `open_settings_selector`
  evaluated two `self.queues.lock()` calls inside the
  `SettingsSelectorConfig` struct literal, and the first MutexGuard is
  a temporary of the whole `let` statement (Rust keeps statement
  temporaries alive to the semicolon), so the second lock ran while the
  first guard was still held. Shipped with the P6-part-2b settings
  selector: its component tests construct the selector directly and no
  interactive e2e ever submitted `/settings`. Fix: one guard in a block
  reading both queue modes, dropped before the config is built.
  Regression test `settings_dialog_mounts_on_submit` drives the real
  submit path (hung 15+ minutes pre-fix, 0.09s post-fix).
- **Footer thinking levels** (`1062d64`+`37c3f33`, CI 43961421159;
  display-parity slice agent-authored, coordinator gated): two real
  gaps. (1) The footer right cell now mirrors `footer.ts:227-263`
  verbatim — `state.thinkingLevel || "off"` display, the
  `(provider)` prefix when `getAvailableProviderCount() > 1` and the
  width fits (count = providers whose wire credential resolves, the
  `updateAvailableProviderCount` source), and the routed virtual-model
  suffix ` → id • level`. (2) The actual "no thinking levels visible"
  bug: the port's session default was `"off"`, upstream's chain is
  `DEFAULT_THINKING_LEVEL = "medium"` (`core/defaults.ts:3`) with the
  `sdk.ts:253-278` startup resolution — CLI flag wins; a resumed
  session restores its recorded level (per-model override NOT
  consulted); a fresh session takes the per-model override, then
  `defaultThinkingLevel`, then medium; then `clampThinkingLevel`.
  Ported as `initial_thinking_level` in `main.rs`, read by all modes.
- **Easter egg "no fade"**: the exit animation was verified ALREADY
  ported 1:1 in `7374b40` (close() = reverse dissolve + gather +
  hint/star alpha fades over the 1s EXIT_DURATION). The slice added
  the `app.clear` close keybinding join (`easter-egg-3d.ts:935-937`)
  and exit tests (`the_exit_reassembles_the_screen`,
  `cancel_and_clear_input_close_the_egg`). If a fresh build still
  shows no fade, look at the terminal-reporting path, not the
  animation. Note upstream's second-close-skip finishes immediately —
  a second escape mid-exit looks like "no fade".
- **`/login` provider catalog** (`75cb249`+`8697bd5`, CI 43961989648;
  agent-authored, coordinator gated): the login selector enumerated
  only model-derived providers (zero-model and api-key-only providers
  vanished). Now `models_state` keeps a `PROVIDER_IDS` snapshot (the
  full registry incl. virtual providers), the 43 builtin display names
  are a verbatim table (`oauth-selector.ts:30-49` row labels,
  `compareProviders` sort with radius/openrouter first), the four
  custom-resolver providers get their ambient auth names,
  `--api-key` registers a runtime key overlay (`RUNTIME_API_KEYS`,
  `runtime-credentials.ts:36-41` mirror) that shows in status and
  logout rows, and logout lists stored credentials with
  " ✓ configured". Upstream has NO "Manage accounts"/"Configure
  manually" rows in 1.5.2 — none added.
- **Deviations from the display slice** (recorded in code): the
  routed suffix renders without the level until
  `AssistantMessage.thinking_level` exists (pi-ai's 5-level type
  can't express xhigh/max); `--api-key`-only providers don't count
  toward the prefix gate (flag key not passed); upstream refreshes the
  count on login/logout events, the port on `refresh_footer`.
- **Killed mid-flight** (user ended the day; agents stopped by user):
  `/fork /clone /tree` (worktree `~/Projects/pi-rs-wt-tree`, branch
  `feat/session-tree-commands`, no commits — dirty `config/settings.rs`,
  `interactive/theme.rs`, untracked
  `components/user_message_selector.rs`) and `/export /import`
  (worktree `~/Projects/pi-rs-wt-export`, branch
  `feat/export-import-commands`, no commits — dirty `session/mod.rs`,
  `session/records.rs`, untracked `session/export.rs` + `export-html/`;
  `/share` deliberately skipped per the Radius policy). Work preserved
  uncommitted on disk — resume or wipe and respawn.

## Oxc transpile disk cache (landed on `feat/oxc-transpile-cache`, 2026-11-17)

User benchmarked the release binary at **7.8 s** for `pi --model mock/
instant -p "hey"` (25 runs, ±0.6 s) and asked why the Rust port is
slower than the npm pi. Decomposition (hyperfine): no-extensions
floor `-ne` = 18.6 ms; one extension via `-e` = 192 ms; the ~260-file
extension set cost ~24 ms/file, linear — and a full run wrote zero
files into the agent cache dir. Upstream's loader leaves jiti 2.7.0's
`fsCache` at its default `true` (`loader.ts:585-599` sets only
`moduleCache: false`), so upstream warm starts reuse transformed
modules from disk; the oxc port re-transformed every source on every
start. That fidelity gap is fixed: `transpile()` now content-
addresses (pipeline version, path, source) into
`$XDG_CACHE_HOME/pi-rs` else `~/.cache/pi-rs` (`89a1a40`, CI
44132602447). Entries carry their key on the first line (garbage =
miss), writes are atomic tmp+rename, every cache I/O failure falls
back to transpiling, `.js` passthrough never caches, and
`transpile_with_cache_dir()` exposes explicit control for tests plus
an `#[ignore]`d cold/warm bench (242 fixture sources: 41.6 ms cold →
2.4 ms warm, 17.5×). Deviation from jiti: one flat content-addressed
file per source instead of jiti's per-path cache layout.

Honest scope note: the transpile cache is a fidelity fix, not the
main cause of that 7.8 s — per-extension timing pinned ~5 s of it on
the user's installed `pi-multi-account` extension, whose
`refreshModels` refetched every account's model catalog on every
launch (pi awaits each provider's refresh at startup and the composer
never persists the results). Fixed in the extension itself
(hakergeniusz/pi-multi-account `d41ceda`, disk-backed catalog cache,
10.5× on an A/B harness) outside this repo.

## Session tree commands: /fork /clone /tree (landed on `feat/session-tree-commands`, 2026-11-17)

Resumed the user-killed agent's WIP (settings/theme diffs + a 234-line
`user_message_selector.rs`) and completed the slice; landed as `e44afb8`
(CI 44129415351). All three commands are 1:1 ports; the `/resume`
fork-tree prefixes and `Store::get_branch_records` primitives from the
session-selector slice are reused, not duplicated.

- **WIP salvage**: `TreeFilterMode` in config/settings.rs
  (`tree-selector.ts:98`, cycle `:1065-1076`, status labels `:650-670`;
  `settings-manager.ts:1379-1390`), `app.session.tree/fork` +
  `app.tree.*` keybinding actions (`keybindings.ts:80-98`, `:143-170`),
  and the `/fork` dialog component (`user-message-selector.ts:1-171`).
- **`/fork` + `/clone`** (`interactive-mode.ts:3398-3411`, `:5692-5749`;
  `agent-session-runtime.ts:262-350`; `getUserMessagesForForking`
  `agent-session.ts:4342-4357`): the `session_before_fork` gate,
  `extractUserMessageText`, `createBranchedSession` for persisted
  sessions with the verbatim "not saved yet" error, the new-empty-session
  path for forks from the root, in-memory in-place mutation, and
  `teardownCurrent`'s abort → `session_shutdown` ordering. Dispatch and
  autocomplete entries verbatim (`slash-commands.ts:22,34-35`).
- **`BranchSummaryMessageComponent`** (`branch-summary-message.ts:1-96`)
  wired into both replay paths (`interactive-mode.ts:4063-4074`) so
  fork/tree leave visible transcript rows.
- **`TreeSelectorComponent`** (`tree-selector.ts:1-1439`):
  flatten/indentation (`:203-331`), five filter modes with
  nearest-visible-ancestor selection (`:333-431`), filtered-view
  recalculation (`:439-562`), horizontal viewport (`:65-95`),
  fold/unfold + segment jumps (`:1001-1016`, `:1108-1152`), search with
  reset-on-change, label edit, copy (`:907-934`), label timestamps
  (`:865-888`), help rows (`:1182-1273`), search-line cursor.
- **`ExtensionSelectorComponent`/`ExtensionEditorComponent`** (focused
  ports of `extension-selector.ts:1-107`, `extension-editor.ts:1-145`)
  for the "Summarize branch?" flow.
- **`session/branch_summarization.rs`** (`branch-summarization.ts:1-398`):
  `collectEntriesForBranchSummary`, `prepareBranchEntries`, the verbatim
  `BRANCH_SUMMARY_PROMPT`/preamble, `generateBranchSummary` reusing the
  compaction pipeline (helpers made `pub(crate)`).
- **`/tree` navigation** (`interactive-mode.ts:3408-3411`, `:5751-5898`;
  `AgentSession.navigateTree` `agent-session.ts:4140-4337`):
  current-leaf no-op, skip-prompt summary dialog loop, abort-first, busy
  recheck (the #9178 shape), the `session_before_tree` gate with the
  preparation payload, `branchWithSummary`/`resetLeaf`/`branch` leaf
  moves, `session_tree` emission, editor-text handback, transcript
  replay. `isCompacting` now covers branch summaries
  (`agent-session.ts:1719-1726`) for the submit queue and interrupt
  chain; the worker mirrors `CompactionRun`.
- **Tests** (41): 15 tree_selector, 12 interactive_mode (mirroring
  `interactive-mode-clone-command.test.ts` /
  `interactive-mode-tree-navigation.test.ts` incl. the busy-guard and
  abort-then-navigate cases), 3 branch_summarization, 3 branch_summary,
  2 extension_selector, 2 extension_editor, 2 settings.
- **Deviations**: no `session_start` emission on fork replacement
  (port-wide gap shared with `/resume`/`/new`, noted in the fork
  comment); `session_before_tree` is cancel-only — extension
  summary/label overrides land with E3, gate hoisted ahead of the
  summarizer worker (same ordering as upstream's in-`navigateTree`
  gate); branch summarization is a single LLM attempt (matches the
  port's compaction deviation, upstream wraps retries);
  `replaceInstructions` rides `false` to the worker; summarize-dialog
  countdown/timeout and tools-expand passthrough unwired (no caller
  passes opts — full extension-dialog machinery is E3); editor omits
  ctrl+G external-editor (P7); `fork`/`tree`/`resume` app actions stay
  unbound in `app_keys` (upstream default `[]` — the port reaches them
  via commands, consistent with `app.session.new/resume`); unparseable
  label timestamps render empty vs upstream's `NaN:NaN`.

## Export/import commands + HTML export (landed on `feat/export-import-commands`, 2026-11-17)

Resumed the user-killed agent's WIP (a correct 179-line
`session/export.rs`, two small record hooks, the vendored upstream
templates) and completed the slice; landed as `cff4973` (CI
44134392721). The branch was rebased onto main mid-flight because the
tree slice landed between branch creation and landing — see the rebase
gotchas below. **`/share` is deliberately NOT ported** (upstream Radius
gateway — banned by repo policy; a test pins its absence from
autocomplete).

- **`session/export.rs`** (`core/session-export.ts:1-45` whole file):
  `serializeSessionBranch` (v3 header in literal key order, `parentId`
  chaining rewritten per branch entry, first entry `null`, entries
  without id drop the key like `JSON.stringify` dropping `undefined`)
  and `exportSessionToJsonl` (timestamped default name with `[:.]→-`,
  mkdir-recursive, resolved path returned). A golden round-trip test
  exports a real pi-written fixture and reproduces every entry line
  byte-for-byte (only the header timestamp refreshes).
- **`session/cwd.rs`** (`core/session-cwd.ts:1-73` whole file):
  `SessionCwdIssue`, `getMissingSessionCwdIssue` with the exact 3-line
  error, `getMissingSessionCwdOptions`, `assertSessionCwdExists`.
- **`store.rs`**: `get_header_record()`/`get_entry_records()` —
  upstream's `getHeader`/`getEntries` return the file objects in their
  original key order, which the HTML export serializes verbatim.
- **`session/export_html/`**: `mod.rs` ports `core/export-html/index.ts:
  1-318` (`parseColor`/`getLuminance`/`adjustBrightness`/
  `deriveExportColors`/`generateThemeVars`/`generateHtml`/
  `preRenderCustomTools` with `TEMPLATE_RENDERED_TOOLS`, call/result
  merge keyed by tool-call id/`exportSessionToHtml`/`exportFromFile`)
  plus a `js_replace_first` helper reproducing `String.replace`'s
  first-occurrence + GetSubstitution `$`-pattern semantics (`$&`,
  `` $` ``, `$'`, `$$`; `$n` stays literal when n > captures — the
  vendored `highlight.min.js` contains a literal `$&` that upstream's
  own export rewrites to the needle text); `ansi_to_html.rs` ports
  `ansi-to-html.ts:1-297` (SGR state machine, 256-color cube/grayscale,
  truecolor, OSC 8 links with the `https?:|mailto:` allow-list, escape
  stripping); `tool_renderer.rs` carries the `ToolHtmlRenderer`
  interface (`tool-renderer.ts:26-37`). Templates live at
  `crates/pi-coding-agent/export-html/` (crate-root data dir, the
  `pi-tui/themes` + `pi-ai/data` convention) embedded byte-identically
  via `include_str!` (cmp-verified against upstream before/after).
- **Theme**: `theme.rs` gained the "HTML Export Helpers"
  (`theme.ts:834-897`) — `get_resolved_theme_colors`,
  `get_theme_export_colors` (vars refs, okhsl→hex, empty→`undefined`,
  indexed→hex, raw passthrough for hex/oklch).
- **CLI**: `--export <file> [output]` wired to `exportFromFile`
  (`main.ts:688-703`) — removes the P5/P6 stub for that flag.
- **Interactive wiring**: `handleExportCommand`
  (`interactive-mode.ts:6708-6724`), `getPathCommandArgument` verbatim
  (`:6726-6750`), `handleImportCommand` state machine (`:6752-6803`:
  usage line → Yes/No confirm → missing-cwd continue/fork/cancel →
  file-not-found/fatal lines), `importFromJsonl`
  (`agent-session-runtime.ts:359-407`: COPYFILE_EXCL copy into the
  session dir with `${name}-${suffix}${ext}` dedup, the
  `session_before_switch` resume gate, stored-cwd assertion dropping
  the copy on failure, session swap), `switchToForkedSession`
  (`:5982-5999`). The landed tree-slice `extension_selector.rs`
  backs the confirm dialog. Autocomplete entries verbatim
  (`slash-commands.ts:25-26`).
- **Tests** (49): export.rs 4, cwd.rs 4, export_html/mod.rs 16 (incl.
  the whitespace-rules mirror of `export-html-whitespace.test.ts` and
  `embedded_templates_are_upstream_bytes`), ansi_to_html.rs 11,
  extension_selector 4 (landed versions), interactive_mode.rs e2e
  through the real submit path (import copy/swap, confirm-cancel
  restore, missing-cwd continue/fork/cancel, path-parsing cases from
  `interactive-mode-import-command.test.ts:25-43`,
  autocomplete-advertises-export-and-import-but-not-share).
- **Deviations**: template embedding via `include_str!` vs upstream's
  runtime `getExportTemplateDir` (`config.ts:454-460`) — bytes are
  upstream's exactly; `createToolHtmlRenderer` (`tool-renderer.ts:
  57-175`) is not portable over the E1 headless extension boundary (no
  `resolveToolRenderers`, TUI components cannot cross QuickJS) — the
  trait + the full `preRenderCustomTools` orchestration are ported and
  tested against a fake renderer, shipped wiring passes
  `tool_renderer: None` so custom tools render via the template's
  structured path (E2 restores the factory); import keeps the startup
  runtime and swaps the session only (the documented `switch_session`
  seam; upstream rebuilds the runtime for the imported cwd) and
  `handleFatalRuntimeError` renders a non-fatal error line (no crash
  report/`exit(1)`); CSS-var order is pi's declaration order
  (`ThemeColor::ALL`/`ThemeBg::ALL`) vs upstream's theme-document key
  order (values identical, no upstream test pins order);
  extension-selector countdown/timeout and `onToggleToolsExpanded`
  omitted (need E2's TUI binding); `/share` and
  `createTrailingEntries` omitted (Radius policy; share was the only
  caller); SGR digit strings too big for `i64` parse to `i64::MAX` (no
  SGR branch matches, like JS's huge numbers);
  `exportToHtml`'s `[themeName, settings.getTheme()]` fallback is
  unreachable (the live theme always resolves).

## PTY re-verification of the user-test round (2026-11-16→17)

All three user-reported fixes re-verified live on the rebuilt release
binary with PTY drivers (`/tmp/pi-rs-smoke/verify_round.py` +
`esc_probe.py`, outside the repo): footer renders
`(openai-codex) gpt-6-luna • medium` (provider prefix + default
thinking level), `/settings` dialog mounts with the real rows and no
freeze, `/login` opens the method dialog and then the full provider
catalog (display names, radius/openrouter-first sort, `✓ stored` /
`✦ not configured` statuses), ESC closes each dialog and `/exit`
exits promptly. The egg close-fade stays code-verified 1:1 (visual
check is terminal-dependent).

## Gotchas / decisions
- **Two locks of the same mutex inside one statement self-deadlock**
  (hit 2026-11-16, `/settings`): `let x = Foo { a: m.lock().f(), b:
  m.lock().g() }` — the first MutexGuard is a temporary of the whole
  `let` (Rust keeps statement temporaries to the semicolon), so the
  second `.lock()` blocks forever on the same non-reentrant mutex.
  Same class as theme()'s 2026-11-14 deadlock (a read guard temp
  across a writing closure). Never lock the same mutex twice in one
  expression; bind one guard in a block first. Component-level tests
  missed it because they never run the mode's submit path — drive
  dialog mounts through `mode.submit(...)` in a test.
- **A hung probe test is a live deadlock repro — bisect with eprintln
  markers** (hit 2026-11-16): gdb attach is blocked by yama
  `ptrace_scope=1` (only parents may attach — use
  `gdb --batch -ex run -ex "thread apply all bt" --args <bin>` from
  the start if a stack is needed). Cheaper: temporary `eprintln!`
  markers at each step of the suspect chain, run under `timeout`,
  read where the markers stop. Remove the markers before landing.
- **Upstream renders display names, not provider ids, in auth rows AND
  statuses** (hit 2026-11-16 in the login slice): "Saved API key for
  Groq", "Removed stored API key for Groq", rows `Groq [API key] ✓
  configured` — tests asserting raw ids (`groq`) break once the
  builtin display-name table lands; unknown providers fall back to
  the id.
- **Record the FULL gate output, all steps (hit 2026-11-16)**: the 6j/6k
  landing notes recorded only FMT/CLIPPY/TEST/FMTCHECK=0 while
  `.gate.sh`'s no-default-features step had been failing since slice 6j
  (the login pump touched the ext-js-only `extension_runner` field
  without cfg gates; CI never runs NDF, so only the container gate can
  catch it). Record every step status — NDF=0 included — or a step can
  silently stay red for two slices (fixed on `1024181`).
- **A shebang-less fixture script dies with "Exec format error"
  (hit 2026-11-16, MCP slice)**: the MCP manager spawns the server
  command directly, so a fixture server script needs `#!/bin/sh`;
  dry-running it under an interpreter masks the missing shebang.
- **Python cut-and-relocate scripts corrupt files (hit 2026-11-16 on
  package_manager.rs)**: a regex-signature + doc-walk-back +
  brace-matching + dedent script applied 4× interleaved junk into the
  file head; the symptom was bogus rustc "prefix `X` is unknown" errors
  on plain-ASCII strings. The truncation repair then cut ~650 lines of
  real definitions — recovered by reconstructing from upstream ground
  truth with the surviving tests as the spec. Prefer Edit-tool
  relocations; verify head/tail integrity after any multi-cut script.
- **CI triggers only on pushes to `main` (2026-11-16)**: branch pushes
  create no runs — branch-level certification is the container gate, and
  `.gate.sh` now exits non-zero when ANY step fails (its per-step echoes
  alone masked failures once). Poll CI by RUN ID (`gh run view <id>`,
  fetched right after the push), not by branch head — a fresh push's run
  may not exist yet and `gh run list` then matches the previous commit
  (bit once on 2026-11-16).
- **`entry_appended` prerequisite (slice 6g)**: the port's
  `SessionManager::append_*` methods do not return the appended record;
  upstream emits the stored entry. Extend the append API before wiring
  the event.
- **Session-slot errors are JSON strings (E2 slice 5, 2026-11-15)**:
  every `error:` passed to `__piSessionSettle` must be the `__piErrorText`
  shape — `JSON.stringify({message, stack})` — because `__piSessionLast`
  `JSON.parse`s the slot value back into an object. A raw `{message}`
  object reaches the parser as the string `"[object Object]"` and dies
  with QuickJS's `unexpected token: 'object'`, masking the real error
  (hit by the route glue's unregistered-model branch; the user_bash
  validator already re-stringified for the same reason).
- **Round-hunting can exercise a stale binary (E2, 2026-11-15)**: a hunt
  script that `find`s the test binary and runs it directly skips the
  rebuild — three deterministic bugs looked flaky across 40+ "green"
  rounds and one 21-minute spin. Always `cargo test` (which rebuilds)
  when asserting source-level stability; treat `find`+run loops as
  binary-level evidence only.
- **`pkill -9 -f <pattern>` kills its own shell** when the pattern
  matches the shell's own cmdline (it does for `pi_ext_js` — the pattern
  is in the shell's argv) — a hunt script died exit 137 on its own
  cleanup line.
- **Tree-index churn when inserting a container (part 2a)**: the pending-messages
  slot shifted every later index — the `*_INDEX` consts are the single source of
  truth; never hardcode numbers in tests or handlers.
- **Same-millisecond message identity (part 2a)**: loop-delivered user messages
  are matched to the UI's queued list by timestamp — two messages created in the
  same ms collide, so the run's own prompt needs an explicit `active_prompt`
  marker (timestamp+text) checked BEFORE the queued match.
- **Global-init deadlock pattern (hit 2026-11-14)**: `static RwLock<Option<T>>` +
  `LOCK.read().clone().unwrap_or_else(|| init_that_writes())` deadlocks — the read
  guard temporary lives until the statement ends, so the write inside the closure
  blocks the same thread. Bind the guard in a block, drop, then init
  (`interactive/theme.rs::theme()`). A test that forces the cold path
  (`theme_lazy_init_from_cold_does_not_deadlock`) guards it.
- **Interactive tests drove 6 first-run fixes** (they were never executed before —
  the previous test run hung on the deadlock above): markdown cold-path slot never
  called `set_text`; `format_tool_duration` missed `% 60` on minutes; `timer_text`
  ordered `started_at` before the final-duration check (upstream: final duration
  wins first); `parse_diff_line` absorbed the separator space (regex backtracking
  port needed); BuiltInHeader/Footer needed `as_any_mut` opt-ins (downcasts
  silently no-oped). Lesson: run the full suite, not just the new crate's subset.
- Ported code once arrived with a systematic corruption: `map_or((default,` (extra `(` after `map_or(`) — fixed crate-wide by rewriting to `map_or(`. (Store.rs E0308 in old `test-log.txt` was another artifact of that batch, already fixed.)
- **`sh -c` + SIGTERM orphans the grandchild (CI hit 2026-11-15)**: dash (the
  CI runner's `/bin/sh`) does not exec-optimize `sh -c "sleep 30"` — SIGTERM
  kills the shell, the orphaned sleep keeps the pipe write ends open, and
  reader `join()` blocks until its natural exit (test ran the full 30 s; bash
  exec-optimizes, which masked it in the local container). Node upstream has
  the same characteristic (close waits on stdio), so `exec()` behavior is
  faithful — never spawn long-lived commands via `sh -c` in exec tests; use
  the binary directly (`/bin/sleep 30`).
- Workspace lints: clippy `all+pedantic`, CI `-D warnings`, `cargo fmt --check`, MSRV 1.87 (`rust-version = "1.87"`, bumped from 1.85 at the E0 spike for `rquickjs` 0.14). Newer-API fixes must respect MSRV (the `f64::midpoint` scoped `#[allow]` from the 1.85 era can be lifted now).
- `ToolDefinition.name` is `&'static str`; `Mode`/`PrintModeOptions` have no `Default`.
- pi-ai `StreamOptions.reasoning` has only 5 levels; `xhigh`/`max` clamp to `High` in `ProviderStreamFn` until P5.
- Catalog reality check for tests: `models.json` has `claude-sonnet-4-5/4-6/5/5-5` but NOT plain `claude-sonnet-4`; bare `claude-sonnet-4-6` is ambiguous (anthropic + cloudflare-ai-gateway + opencode).
- Deviations documented in code: NFKC subset for fuzzy edit matching (no `unicode-normalization` dep yet), no revision-cache in auth storage, no `powershell` tool, bash tool description hardcodes "bash".
- Local dev machine only (if ever resumed): plain `cargo` is broken there (rustup proxy misreads argv[0]); use `/tmp/ai-agent/cargo` wrapper (`exec -a cargo nice -n 19 ionice -c 2 -n 7 /usr/sbin/cargo`). On acer no wrapper needed — just prefix `nice -n 19`.

## Remaining built-in commands: /reload + /scoped-models (landed on `feat/reload-scoped-models`, 2026-11-17)

The last two unported built-ins. The slice started as a subagent run that
died to a quota limit after committing its module layer but before any
compile; the coordinator finished the wiring serially and the gate
burned four rounds to make the unverified code real. Landed as `a1d7ae2`
(CI 44151058808, 1780 tests green).

- **Agent-committed layer** (its `fffd765`, salvaged): `model_resolver.rs`
  (894 lines — `resolveModelReference` + `resolveModelScopeFromModels`
  with diagnostics, `core/model-resolver.ts:74-393`; globs via globset),
  `scoped_models_selector.rs` (1137 lines, full component port),
  `model_catalog_refresh.rs` (the coordinator collapses to a synchronous
  dispatch over the process-global compose — module doc carries the
  deviation), `models_state.rs` per-provider `ModelsRefreshResult`,
  settings `get/setEnabledModels` (`settings-manager.ts:1352-1367`), and
  the six `app.models.*` keybindings (`keybindings.ts:119-142`).
- **Coordinator-completed wiring** (0087417): `/scoped-models` mounts the
  selector over the synchronous refresh — upstream's async mount →
  "Refreshing model catalogs…" → swap sequence runs inline
  (`showModelsSelector`, `interactive-mode.ts:5570-5686`), with
  `updateSessionModels` (`:5591-5609`), `onPersist`'s clear-on-all-enabled
  (`:5630-5638`), and the exact refresh-status texts. The session scope
  seeds from the `enabledModels` setting at startup (`main.ts:871-882`),
  rides a shared slot into the `/model` completions, and drives
  ctrl+p cycling: `_cycleScopedModel` (`agent-session.ts:2709-2744`) with
  explicit pattern thinking levels beating `defaultThinkingLevel` beating
  the current level (`:2841-2853`).
- **`/reload`** (`handleReloadCommand`, `interactive-mode.ts:6618-6706`
  over `session.reload`, `agent-session.ts:3828-3874`): streaming/
  compaction guards, `session_shutdown(reason:"reload")`,
  `runner.invalidate`, `settingsManager.reload`, queue-mode re-sync,
  `extensions::reload` (re-discovery against the reloaded settings →
  incremental final-set load → re-bind → `session_start(reason:"reload")`
  → MCP reconnect → bind-error report), catalog recompose, theme
  re-resolve from settings, `hideThinkingBlock` re-read, the
  `models.json error` report (`model-runtime.getError()` shape,
  `model-runtime.ts:512-520`), and the verbatim status line.
- **Deviations (mechanism, documented in module docs)**: the port's
  reload is synchronous on the UI thread — upstream's bordered
  "Reloading…" box has no observable frame here; the default-tools delta
  rides a tool registry the port has not ported; the implicit
  project-trust save after reload is startup-only, so the status never
  carries "saved project trust"; keybindings are static constants
  (`theme::app_keys`), so `keybindings.reload()` is a no-op; the
  extension reload is INCREMENTAL (no per-path unload — already-loaded
  extensions keep their registered handlers where upstream clears the
  module cache and re-evaluates every file). `InteractiveOptions` now
  carries the `--extension` paths + `--no-extensions` flag for the
  re-discovery.

- **Rebase grafts lose definitions and registry lines silently**
  (hit 2026-11-17, export slice): resolving repeated rebase conflicts
  with `checkout --ours` against a tip graft preserves call sites and
  tests but silently drops items that live OUTSIDE the grafted hunks —
  the compiler catches referenced-but-missing items (enums, fns), but
  pure data lines (autocomplete registry entries) vanish invisibly
  until their specific test runs. After any programmatic graft,
  inventory-audit: diff every definition (enums/structs/fns) AND every
  registry/data line the pre-rebase tip had against the rebased branch
  — not just what the compiler complains about.
- **Green gate ≠ mergeable — ff-merge BEFORE deleting branches**
  (hit 2026-11-17): a branch whose gate is green can still be based on
  an older main (main moved twice during the export slice's gate
  rounds), making `--ff-only` fail AFTER the branch refs were already
  deleted. Recovery: the tip commit hash survives in the object store —
  `git branch <name> <tip-hash>`, re-push, rebase onto main, re-gate.
  Order is: verify `git merge-base main <branch>` == main tip →
  ff-merge → push → poll CI → only then delete branch/worktree.
- **rustfmt can be non-convergent on a first pass** (seen 2026-11-17):
  one gate's FMTCHECK failed with diffs in a file the slice never
  touched (pi-ext-js/transpile.rs) even though `cargo fmt --all` ran
  first with exit 0; a second write+check cycle converged. If FMTCHECK
  fails on untouched files, re-run `cargo fmt --all` and re-check
  before hunting for a cause. Also: `git add -A` after a gate round
  sweeps in the fmt rewrites left on the gate checkout — commit them
  deliberately as a `style:` commit (or restore them) rather than
  mixing them into a semantic fix.
- **PTY driver exit detection: EOF-read EIO swallows the exit**
  (hit 2026-11-16→17): on child exit the pty master becomes readable
  and `os.read` raises OSError — a driver's `except OSError: break`
  skips the waitpid reap, so a cleanly-exited process reads as "no
  exit". Discriminate with `/proc/<pid>/state` (Z = zombie, already
  exited; wchan/syscall are readable without ptrace). Related: `/login`
  opens the auth-METHOD dialog first — the provider catalog renders
  only after choosing a method, so PTY verifiers must drive through it;
  and CaptureTerminal-driven tests need an explicit `render_now(false)`
  after mounts/pumps or the frame never records the dialog.
- **A quota-killed agent's commits are UNVERIFIED state** (hit
  2026-11-17, reload slice): an agent that dies mid-run leaves
  "committed" modules that never compiled — 8 compile errors, 16 clippy
  lints, and 10 test failures surfaced across the first three gate
  rounds (an orphaned tail expression, a planned-but-uncalled enum
  conversion, unit tests asserting off-upstream values). Budget a
  salvage round: fix compile first, then lints, then run the module's
  own tests and treat each failure as either an implementation bug or a
  test-transcription bug — decide per case against the upstream source,
  not the test's authority.
- **Test helpers must mirror upstream's exact char arithmetic** (hit
  2026-11-17): upstream's `getMarkerStates` does `line.slice(2)` before
  checking the ✓ marker; a port using `trim_start()` breaks on the
  SELECTED row because its `"→ "` prefix is not whitespace. Same class:
  JS `slice(n)` = Rust `chars().skip(n)`, byte-index panics on the
  multi-byte arrows/boxes these renders use.
- **Invented tests must respect upstream no-op semantics and real key
  bytes** (hit 2026-11-17): `enableAll` from `null` returns `null`
  unchanged (`scoped-models-selector.ts:39`), so "enable all scopes to
  the filtered rows" only observable from an explicit list; and
  alt+down's terminal bytes are `\x1bn`, NOT the down-arrow `\x1b[B`
  (which is plain SELECT_DOWN). Also the fuzzy haystack includes the
  provider name (`getModelSearchText`), so a synthetic provider named
  "beta" matches the query "b" — give filtered-scoping fixtures
  providers that cannot match the query.

## Deferred items backlog (from P4 reports)
- `unicode-normalization` dep for byte-exact NFKC (edit tool fuzzy match).
- `serde_json` workspace feature `float_roundtrip` (exotic >17-digit literals, 1 ULP rewrites in session JSONL).
- `SessionListProgress` callback, `ReadonlySessionManager` (TS type-level surface).
- Node-runtime-specific parts of `config.ts` (Bun/npm install detection, self-update, QuickJS paths).
- JSON/RPC modes, extension binding, signals + stdout backpressure in print mode.
