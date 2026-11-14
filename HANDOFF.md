# Handoff: Pi in Rust (`pi-rs`)

> Written 2026-11-11, updated same day after gate attempts. Repo: `/home/hakergeniusz/Projects/pi-rs` on `acer`.
> **GitHub: https://github.com/hakergeniusz/pi-rs (private)**, remote `origin` (ssh). **`main` = P0–P5-part-1 + codex adapter + interactive TUI (parts 1+2a); E2E PROVEN LIVE** — `pi -p` smoke test PASSED 2026-11-14 (PONG / read-tool round-trip ALBATROSS / JSON event mode) with the openai-codex stored OAuth credential and default model `openai-codex/gpt-6-luna`; required (1) a new `openai-codex-responses` adapter in pi-ai (`8efb00b`) and (2) the agent loop driven on a process-wide tokio runtime (`4c4d62c`, was futures::executor::block_on → "no reactor running"). — landing fast-forwarded `39b22ad..37782d1` on 2026-11-14; first landing CI failed only because ubuntu-latest ships **neither rg nor fd** (12 grep tests: `which("rg")` failed) → fixed in ci.yml (`8c6d178`, installs `ripgrep` + `fd-find`); CI run `43578686739` on `8c6d178` = **ok**. Interactive TUI landed `27769a9..500302b` + part 2a `8ae806d` on 2026-11-14 (see P6 sections). `wip/p4-pre-gate` == `main`; local checkout is ON `wip/p4-pre-gate`.
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
| P4 | `pi-coding-agent`: config, auth, sessions (JSONL v3 byte-compat), built-in tools, system prompt, prompt templates, skills, CLI args, print mode (`pi -p`) | ✅ committed — gate green in-container (1205 tests, 44 targets), landed on `main` 2026-11-14 |
| P5 | JSON/RPC modes, extensions seam, MCP client, trust, `@file`, OAuth | ⚠️ part 1 landed on `main` (`c8e1818` + reconciliation `a6b9934`); remainder open (rpc envelope, MCP server-manager/tool-bridge, package-manager, codemode, ext host, `defaultProjectTrust` getter) |
| P6 | Interactive mode | ⚠️ **part 1 landed on `main` (`500302b`)** — regular chat TUI 1:1 (theme, components, orchestrator, themed renderers, CaptureTerminal e2e, real-PTY smoke); remainder open (fullscreen mode, autocomplete/selector dialogs, extensions, compaction, queues, mouse toggles, OSC theme query) |

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
5. ~~**P6 — Interactive mode**~~ **part 1 DONE 2026-11-14** (see P6 section below); part 2 remains.
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

## Gotchas / decisions
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
