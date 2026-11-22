# AGENTS.md

Operating manual for AI coding agents working in this repository (useful to
humans too). Sections marked **workstation-specific** apply only when
building on the operator's machine; everything else is repo policy for any
contributor or agent.

## The project

pi-rs is a Rust rewrite of [earendil-works/pi](https://github.com/earendil-works/pi)
— the minimal, extensible agent harness — pinned to upstream **1.5.2**, aiming
for behavioral and on-disk compatibility (session JSONL v3, print/JSON/RPC
output schemas, project `.pi/` layout).

The standing directive is **1:1 fidelity**: port upstream semantics exactly.
When upstream and "what would be cleaner" disagree, upstream wins; every
deliberate deviation is documented in `HANDOFF.md`. There is exactly one
headline deviation: the user-level directory root is XDG (`~/.config/pi-rs`,
`~/.local/share/pi-rs`, …) instead of `~/.pi`, so the port can never touch a
real TypeScript pi install.

### Ground truth and the privacy rule

- **TypeScript source (canonical, cited by line in module docs):**
  `/tmp/pi` on the operator's machine — a read-only clone of
  `earendil-works/pi`; `packages/coding-agent` is at 1.5.2. `/tmp` is
  ephemeral; re-clone the public repo if it is gone:
  `gh repo clone earendil-works/pi /tmp/pi`.
- **Privacy rule (hard):** never open, `cat`, or Read anything under a
  live pi data root — the XDG `pi` directories (`~/.config/pi`,
  `~/.local/state/pi`, …) or a legacy `~/.pi` — because `auth.json`,
  real sessions, and real settings hold live credentials and chat
  history. The built `pi` binary reading its own config is fine; the
  agent reading those files is not. All test fixtures are synthetic.

### Where knowledge lives

| File | Contents |
|---|---|
| `README.md` | user-facing status, build, paths |
| `PLAN.md` | phased plan (P0…P7) and scope decisions |
| `HANDOFF.md` | dated status log: what landed, deviations, gotchas |
| `docs/PORTING.md` | the porting craft: workflow, crate map, TS→Rust pattern dictionary |
| `docs/EXTENSIONS.md` | extensions-host recon: API surface, event semantics, loading pipeline, runtime decision, E0–E4 phasing |
| module `//!` docs | per-file ground truth: the exact upstream file/lines ported |

**Write docs as you go** — even if they are only for other AI agents. Every
module cites its upstream ground truth; every landed slice updates
`HANDOFF.md` (and `README.md` when the user-visible status changes).

## Build and test

Anyone with Rust 1.97 (MSRV) can run the standard suite:

```sh
cargo build
cargo test --workspace -- --test-threads=4
cargo clippy --workspace --all-targets -- -D warnings   # pedantic lints are errors
cargo fmt --all -- --check
```

- The interactive-mode tests need `fd` and `rg` on `PATH` (the tools resolve
  them via `which()` like upstream's `ensureTool`; CI apt-installs both).
- Current clippy conventions: `assert!(x.is_empty())` must be
  `assert_eq!(x, "")` (`assert_is_empty`, clippy ≥ 1.99); long
  mirror-functions use the repo precedent
  `#[allow(clippy::too_many_lines)] // mirrors the TS …; split when extending`.
- Run the full suite before pushing — not just the crates you touched; the
  full run has caught cross-crate breakage more than once.

### Workstation-specific: memory-capped build machine

The operator's machine has OOM-killed cargo builds before; when building
there (and only there), these limits apply:

- Run heavy cargo commands inside the podman container `pi-rs-build`
  (workspace mounted at `/work`), never on the host.
- Always: `nice -n 19`, `CARGO_BUILD_JOBS=8`, `CARGO_INCREMENTAL=0`,
  `CARGO_PROFILE_DEV_DEBUGINFO=0` (and the release equivalent). Use the
  shared target dir `/build-target` so artifacts are reused.
- Never run memory-stress probes, `ulimit` experiments, or parallel heavy
  builds. If a command might OOM, reduce parallelism first and say so.
- The container runs with 8 CPUs / 7 GB RAM (user-ordered 2026-11-15; was
  4/4 earlier that day, 5/7 before that) — `-j 8` matches the 8 threads;
  if a container OOM ever reappears, drop `-j` first and report it.
- Do not block-wait on background tasks; poll. Do not pipe clippy through
  `tail`/`head` — the pipe masks its exit status. Log files written inside
  the container stay inside it (`podman exec pi-rs-build cat …`).

The full gate as run on that machine:

```sh
podman exec pi-rs-build sh -c 'export CARGO_TARGET_DIR=/build-target CARGO_BUILD_JOBS=8 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUGINFO=0; cd /work; nice -n 19 sh -c "cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace -- --test-threads=4 && cargo fmt --all -- --check"'
```

#### RAM-first build / low-TBW workflow (2026-11-21)

The old gate wrote a 16 GB cargo target dir directly to the NVMe SSD, one
small file at a time. Use the RAM-first variant instead: a second container
`pi-rs-build-ram` (built from `pi-rs-build-base`, the committed toolchain
layer) mounts a 1.8 GB tmpfs at `/ramdisk`, bind-mounts `/work` and the cargo
registry **read-only**, and sets `CARGO_NET_OFFLINE=true`. All cargo artifacts
and test caches live in `/ramdisk`; the SSD volume is touched just once per
gate by a single compressed snapshot (`scripts/ram-build.sh snapshot`).

```sh
podman exec pi-rs-build-ram sh /work/scripts/ram-build.sh check -p pi-mcp
podman exec pi-rs-build-ram sh /work/scripts/ram-build.sh test -p pi-mcp
podman exec pi-rs-build-ram sh /work/scripts/ram-build.sh gate
podman exec pi-rs-build-ram sh /work/scripts/ram-build.sh snapshot
```

`scripts/ram-build.sh` is the single source of truth for the env and the
gate steps (it also runs `cargo check --no-default-features`, which CI skips).
Targeted crate checks need only ~150 MB of tmpfs and ~15 s; a full workspace
build fits the 1.8 GB tmpfs only with `DEBUGINFO=0` + `CARGO_INCREMENTAL=0`
(and does not touch disk until `snapshot`). Keep scratch files, subagent
worktrees and test fixtures under `/tmp/...` (host tmpfs). The host has little
spare RAM (~2.4 GB free, heavy swap use) — never enlarge the tmpfs or run two
heavy gates in parallel; one RAM gate at a time.

## Git workflow

- **Topic branch per unit of work**: `feat/<slice>`, `fix/<slice>`,
  `docs/<slice>`; ff-merge to `main` after CI is green; delete the branch
  after the merge.
- **Conventional Commits**; AI-generated commits carry an `Assisted-by:`
  trailer naming the assistant (ZCode sessions use
  `Assisted-by: ZCode (GLM-5.3-Flash)`).
- Push after every commit, then check CI (`gh run list` / `gh run view`).
- Remote: `github.com/hakergeniusz/pi-rs` (**private**). Never commit
  secrets or files ≥ 25 MB.
- Version discipline: workspace `0.1.0-alpha.x`.

## Code conventions

- Every module's `//!` doc cites the upstream file (and line ranges) it
  ports; when you extend a module, extend the citation.
- Put `mod tests` **last** in a file (`items_after_test_module`).
- Never append to file tails with `>>` shell redirection — it glues onto a
  missing trailing newline. Use full-file writes or round-trip edits.
- pi-tui tree downcasts need an explicit `as_any_mut` override on the
  component; a missing override makes the downcast *silently* no-op.
- The full porting workflow, the crate↔package map, and the TS→Rust pattern
  dictionary live in [docs/PORTING.md](docs/PORTING.md).

## Current status

`HANDOFF.md` has the dated details; `README.md` the user summary;
[`docs/GAP-STATUS.md`](docs/GAP-STATUS.md) the upstream↔port coverage matrix.
**v0.1.0 is tagged and released** (Linux x86_64/aarch64 assets + checksums).
Builds are host-side (`scripts/host-build.sh`); podman was removed.

Landed through the 2026-11-22 gap wave (integration `16f0946`): the full
extensions arc E0→E3 plus the E4 items (CJS interop, per-path `/reload` unload,
`pi-ai`/`pi-tui`/`pi-agent-core`/`pi-codemode`/`source` host packages, fuller
WHATWG URL / `Stats` / HMAC shims), all P7 items, the P5 remainder, all MCP
opens, all 10 RPC commands, float-exact / tolerant-decode session JSON, RPC
prompt preflight, the real `pi-codemode` crate, the npm package-manager arm,
**seven more provider adapters** (Google GenAI/Vertex, Bedrock Converse,
OpenAI/Azure Responses, Mistral Conversations, pi-messages), **built-in OAuth
login/refresh** (Anthropic, Codex, Copilot), the new **`pi-env`** crate, the
**telemetry exporter/ping/conformance** surface, and the registered **image
pipeline** module.

A follow-up host-wiring wave (`16060d5`..`deb633e`) made stored built-in OAuth
credentials refresh at wire time, attached images from `read`/`@file` and the
`convertToLlm` filter in all four modes, fired the telemetry ping/exporter at
startup/shutdown, drove MCP startup/list-changed ticks, and upgraded `url` to
2.5.9 (RUSTSEC-2026-0592). Full gate green: 2565 passed, 0 failed, 3 ignored.

Still open (mechanism tails or deliberate scope decisions):

1. `pi-env` is still library-only: ticks/PTY/SSH onboarding have no host consumer.
2. Wiring tails: `blockImages` is installed in all four modes but headless
   modes snapshot it once at startup (upstream re-reads it per call); there is
   no image pixel backend, telemetry transports stay inert (no real HTTP), the
   MCP `StartupWait` is one-shot with poll-based notifications and no tick in
   the interactive/RPC pump modes, and OAuth refresh covers only the three
   built-in flows (Anthropic/Codex/Copilot).
3. RPC dispatch is still serial and session rebind/host events remain stubbed.
   Codemode Node/Workers/wasm transports are typed stubs; dynamic `require`,
   `node_modules` resolution and `URL.canParse` remain unported.
4. Out of scope: `chord`, `pi-durable`, `evals`, `apps/p2`. `/share` and `/bug`
   stay omitted (Radius upload policy); macOS release assets await a runner.
