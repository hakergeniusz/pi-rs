# Porting guide — how upstream slices become Rust

`AGENTS.md` covers the operating constraints (container, gate, privacy,
git workflow); this covers the craft. It is written for AI agents doing the
porting, and consolidates conventions that were previously scattered through
`HANDOFF.md`.

## Ground truth

- **Canonical TS source:** `/tmp/ai-agent/pi` — read-only clone of
  `earendil-works/pi`; `packages/coding-agent` is at 1.5.2. `/tmp` is
  ephemeral; re-clone the public repo if it is gone.
- **Durable compiled upstream (allowed reads):**
  `~/.pi/agent/install/releases/1.5.2/node_modules/@earendil-works/*/dist/`
  — JS, `.d.ts`, and sourcemaps per package.
- Nothing else under `~/.pi` may be read (privacy — see `AGENTS.md`).

## Crate ↔ package map

| Rust crate | Upstream package | Notes |
|---|---|---|
| `pi-schema` | `pi-schema` | schema builders/compilers/validators without eval |
| `pi-ai` | `pi-ai` | types, model catalog, per-provider SSE adapters |
| `pi-agent` | `pi-agent-core` | agent loop, tool trait, steering/follow-up queues, events |
| `pi-tui` | `pi-tui` | differential renderer, alt screen, editor, fuzzy, mouse, theme, oklab |
| `pi-mcp` | `pi-mcp` | JSON-RPC types + stdio transport client |
| `pi-codemode` | `pi-codemode` | stub |
| `pi-coding-agent` (`pi` binary) | `pi-coding-agent` | the CLI — map below |

Deferred upstream workspaces (out of scope): `chord`, `durable`, `apps/p2`.

### `pi-coding-agent` module map

Upstream `packages/coding-agent/src` is `cli/`, `core/` (agent-session.ts,
sessions, tools, extensions runtime, model/provider machinery, package
manager), `extensions/` (loader entry), `modes/` (`interactive/`, `rpc/`,
`json-event.ts`, `print-mode.ts`), `utils/`, plus top-level `config.ts`,
`main.ts`, `migrations.ts`, `rpc-entry.ts`.

| Rust module | Ports |
|---|---|
| `cli/` (`args.rs`, `project_trust.rs`, `file_processor.rs`) | `cli/`, project trust |
| `config/` (`config.rs`, `settings.rs`) | `config.ts`, settings schema + load/save |
| `extensions/mod.rs` | `extensions/index.ts` — **seam only**; the JS runtime is an open decision (PLAN.md §2) |
| `modes/print_mode.rs` | `modes/print-mode.ts` |
| `modes/json_event.rs` | `modes/json-event.ts` |
| `modes/rpc.rs` | `modes/rpc-entry.ts` (+ `modes/rpc/`) — envelope reconciliation pending |
| `modes/interactive/` | `modes/interactive/interactive-mode.ts` (~253 KB) and `components/*` |
| `session/` (`records.rs`, `json.rs`, `store.rs`, `migrate.rs`, `compaction.rs`) | session JSONL v3 store, migration v1→v3, compaction |
| `tools/` (+ `renderers/`) | `core/tools/*` — read/write/edit/bash/grep/find/ls, truncation, themed renderers |
| `auth.rs` | `core/auth-storage.ts` + credential ladder |
| `skills.rs`, `prompt.rs`, `prompt_templates.rs` | skills, prompt templates |
| `runtime.rs` | the process-wide current-thread tokio runtime (`crate::runtime::block_on`) |

## The porting workflow

1. **Read the upstream slice in full.** Guards and return values only make
   sense with the whole file in view (the lazy-wrapper lesson:
   `easter-egg-3d.lazy.js` returns `true` — absorbed, no fallback — when an
   overlay exists, and `false` — fallback — only when the alt screen is off).
2. **Find the Rust home.** Existing module or a new file mirroring upstream
   naming (`session_selector.rs` ↔ `session-selector.ts`). Keep one Rust
   module ≈ one upstream file; when upstream is a 250 KB monster
   (`interactive-mode.ts`), split by upstream's own component boundaries.
3. **Port semantics, not syntax.** Keep upstream control flow, exact
   constants, tie-breaks, and error strings even where Rust could be
   "cleaner". Fidelity outranks elegance; note anything that looks like an
   upstream bug but replicate it, then flag it in `HANDOFF.md`.
4. **Document while porting.** The module `//!` doc cites the upstream file
   and line ranges; tricky branches get inline comments naming the upstream
   line. Deviations get a comment plus a `HANDOFF.md` entry.
5. **Tests.** Unit tests next to the code, names describing the ported
   behavior; TUI slices get CaptureTerminal e2e; real-terminal behavior gets
   a PTY smoke. Fixtures are synthetic.
6. **Land.** Gate → commit (Conventional Commits + trailer) on a topic
   branch → push → CI green → ff-merge to `main` → `HANDOFF.md`/`README.md`
   updates as a follow-up docs commit.

## TS→Rust pattern dictionary

Established translations. Use these; add new entries here when you invent a
pattern, so the next agent doesn't rediscover it.

- **Timers / `setInterval`** → host-driven time. The interactive loop's
  `pump` (~25 ms cadence) is the only clock; animation frames derive from
  elapsed time. State crossing into components travels as
  `Arc<AtomicU16>`/`Arc<AtomicBool>` (rows, exit flags) refreshed/polled by
  `pump`.
- **Callbacks that would borrow the mode** (re-entrancy during dispatch) →
  queue into `Arc<Mutex<Vec<T>>>` set as a boxed callback; `pump` drains with
  `std::mem::take` and dispatches (header logo clicks → easter egg).
- **`instanceof TuiAltScreen`** → `tui.alt_screen_active()`;
  **`hasOverlay()`** → `tui.has_overlay()`. Mind the semantics: upstream's
  overlay guard *absorbs* the call (returns `true` without fallback).
- **JS shared object references** (aliased list items, shared tree nodes) →
  explicit identity: filtered views as `Vec<usize>` indices into the owned
  `Vec`, component ids as heap addresses taken *after* boxing, host slots
  recorded by id so the viewport can move components.
- **`Math.imul`** → `u32::wrapping_mul`.
- **Lazy `import()` chunks** → plain `use` in the same crate; port the lazy
  wrapper's *guard* logic, not the laziness.
- **`===` on floats exact by construction** → keep `==` with
  `#[allow(clippy::float_cmp)] // exact in upstream too`.
- **Event emitters** → direct trait methods or explicit
  `set_on_*` callbacks stored on the component; no global registries unless
  the emitter itself is the API being ported.
- **`Map`/`Set` iteration order** → Rust `HashMap` is unordered: use
  `Vec`/`BTreeMap`/insertion-ordered structures to match upstream order.
- **`??` / `?.` / optional chains** → `unwrap_or`/`unwrap_or_else`/
  `map_or_else` and edition-2024 let-chains
  (`if let Some(x) = a && let Ok(y) = b`).
- **Try-operator on optional lookups in hot paths** → `?` inside fns
  returning `Option` (`PI_LOGO_PIXELS.get(row)?`), matching JS `undefined`
  propagation.

## Testing conventions

- **Unit tests** live in the same file (`mod tests` LAST), named for the
  ported behavior; assertions on rendered output go through
  `pi_tui::ansi::strip_terminal_sequences`.
- **CaptureTerminal e2e** (no tty needed) drives the real agent loop with a
  scripted `StreamFn` (text → `read` tool call → final text is the classic
  round trip); CaptureTerminal is `Clone` (Arc response store) so tests can
  keep a terminal handle.
- **PTY smokes** live outside the repo at `/tmp/pi-rs-pty-smoke.py`:
  `pty.openpty()`, ioctl `TIOCSWINSZ` on the master *before* fork, then
  setsid + TIOCSCTTY + dup2 + execv. Setting the size after forking races
  the child's first size query and hangs at 0-width. In interactive smokes,
  ctrl+u before `/exit`, or Enter submits typed text to the real API.
- **Session JSONL byte-compat** rests on hand-written synthetic fixtures;
  golden fixtures generated by the real TS pi are the P4 exit criterion and
  still pending.
- **Provider integration tests** are `#[ignore]`-gated and only ever point
  at free or local endpoints (llama-swap at `127.0.0.1:11111`).
- Interactive-mode tests need `fd` and `rg` on `PATH`.

## Durable gotchas (condensed; `HANDOFF.md` has the full dated log)

- pi-tui `as_any_mut` must be overridden per component for tree downcasts —
  the default returns `None`, so a missing override looks like "the feature
  did nothing".
- `truncate_to_width(_, _, _, pad)` — upstream `pad` defaults to `false`;
  passing `true` silently pads to full width and wrecks header math.
- `Input` has both `Focusable::set_focused` and `Component::set_focused` —
  disambiguate with `Focusable::set_focused(&mut input, …)`.
- Temporary-lifetime deadlock:
  `LOCK.read().clone().unwrap_or_else(|| init_that_writes())` holds the read
  guard until the statement ends, so the write inside the closure deadlocks
  the same thread. Bind the guard in a block, drop it, then init.
- Sorted session lists make selector tests order-dependent — select rows by
  query text, not blind arrow-key counts.
- Inserting into the component tree shifts every later index — `*_INDEX`
  consts are the single source of truth.
- toolTimerText: a final result's recorded `durationMs` wins before any
  started-at check; "Elapsed" only while partial.
- The agent loop must run on the process-wide current-thread tokio runtime
  (`crate::runtime::block_on`), not `futures::executor::block_on` — adapters
  `tokio::spawn` and need the reactor.
- The read tool caps at 50 KB, so compaction e2e tests need a 100 KB prompt
  to exceed the 20000-token keep-recent budget.

## Deviations ledger

Deliberate deviations from upstream are documented in `HANDOFF.md` at the
slice that introduced them. The headline ones:

- **XDG directory roots** (`~/.config/pi-rs`, …) instead of `~/.pi` — the
  one product-level deviation; `PI_CODING_AGENT_DIR` still collapses roles
  into one directory exactly like upstream.
- **No timers**: the host-driven pump replaces `setInterval` everywhere.
- **rg download fallback** (upstream downloads a binary; we apt-install in
  CI and document the deviation).
- **`pi auth login` is not ported**; the ported error hint mentions it.
- **copySelection** is internal OSC-52; `open_url`/`on_right_click_paste`
  are unset.
- The chat viewport is **rebuilt per `switch_tui_mode`** (scroll offset
  resets) because the port's component tree is single-owner, not shared.
