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

Anyone with Rust 1.87 (MSRV) can run the standard suite:

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

`HANDOFF.md` has the details; `README.md` the summary. Landed through the 3D
easter egg + `/arminsayshi` (P6 parts 1+2a+2b+2c). Open work, in the recorded
priority order:

1. **Extensions host** — engine decided and spike-proven: embedded JS engine
   (`rquickjs` 0.14 / QuickJS-NG; `deno_core` demoted after Cloudflare's Deno
   deprecation, Bun rejected as non-embeddable) behind a feature-gated
   `pi-ext-js` crate, phased E0→E4 in
   [docs/EXTENSIONS.md](docs/EXTENSIONS.md). E0 spike landed (round trip +
   measurements in `HANDOFF.md`); **E1 headless core landed** (Rust
   `ExtensionRunner` with the §2 dispatch semantics, loader, de-facto core
   API subset; exit met — the vendored bundled `tool-search` extension runs
   identically to upstream, deviations itemized in `HANDOFF.md`). Next step
   is **E2 UI over protocol + host wiring** (feature-gate `pi-ext-js` into
   `pi-coding-agent`, replace the seam
   `crates/pi-coding-agent/src/extensions/mod.rs`, RPC dialog subprotocol,
   `session_before_*` gates, `user_bash`, context transforms).
2. Golden session fixtures generated by the real TS pi (P4 exit criterion).
3. P5 remainder: `modes/rpc.rs` vs `rpc-entry.ts` envelope reconciliation,
   MCP server-manager + tool bridge, package-manager subset.
4. P7 hardening: stdin piping, `!` bash via PTY, release.
