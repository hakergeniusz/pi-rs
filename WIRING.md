# Wiring: upstream coding-agent docs → `docs/upstream/`

## Scope

Ported all 47 files from upstream
`/home/hakergeniusz/tmp-11-18/pi-152/packages/coding-agent/docs` (pinned
1.5.2) into `docs/upstream/`:

- 42 markdown documents, one per upstream filename;
- `docs.json` (upstream navigation manifest, including redirects);
- `images/doom-extension.png`, `images/exy.png`,
  `images/interactive-mode.png`, `images/tree-view.png`.

Nothing else was touched: no `.rs` files, root manifests, `HANDOFF.md`,
`AGENTS.md`, `README.md`, `docs/PORTING.md`, `docs/EXTENSIONS.md` or
`docs/RELEASE.md`.

## Method

Each markdown file was copied byte-for-byte, then a single short
`> **pi-rs note:**` blockquote was inserted immediately after its H1. The
prose below the note is upstream's verbatim text; no upstream sentence was
rewritten, removed, or reordered. `docs.json` and the images carry no note
(there is no prose to adapt) and are exact copies.

A round-trip check (`/tmp/opencode/verify_docs.py`) strips the injected note
and compares the result with upstream: **42/42 documents match upstream
exactly** after note removal.

## Adaptation themes

1. **XDG roots (all 42 docs).** Upstream's `~/.pi/agent/…` becomes
   `~/.config/pi-rs/…` for config/settings/auth/models/themes/prompts/skills/
   extensions/trust/`mcp.json`, and `~/.local/share/pi-rs/…` for sessions.
   Project `.pi/` stays the same; `PI_CODING_AGENT_DIR` still collapses roles.
   Source of truth: `README.md` ("Paths"), `crates/pi-coding-agent/src/config.rs`.
2. **Rust binary, not npm.** The executable is still `pi`, but ships as Linux
   x86_64/aarch64 release assets installed by `scripts/install.sh`; the
   upstream `curl` installer, npm/Nix installs, Node install detection and
   `pi update` self-update are not ported. macOS release assets are not built.
3. **Deliberately omitted.** `/share` and `/bug` and their Radius/Gist uploads
   (Radius upload policy), so `sessionShareDestination`,
   `PI_SHARE_VIEWER_URL` and the bug-report upload text are inert.
4. **Node/TypeScript-only surfaces.** The in-process SDK, `RpcClient`,
   `@earendil-works/pi-tui`, `node:*` builtins and npm `node_modules`
   resolution are upstream runtime features; pi-rs offers Rust crates and the
   print/JSON/RPC subprocess modes instead. E4 extension completeness is open.
5. **Partial parity, flagged where relevant.** RPC command set (10 commands
   open), codemode host wiring, `float_roundtrip` for exotic session floats,
   and the cached pi.dev model-catalog refresh.

## Per-document notes

| Document | Note focus |
|---|---|
| `index.md` | XDG + overview; points to the repo README for parity |
| `quickstart.md` | install: release assets / install.sh, no npm/Nix/self-update |
| `how-pi-works.md` | XDG paths only |
| `usage.md` | `/share` omitted; `sessionShareDestination` inert |
| `models.md` | bundled catalog + `models.json`; pi.dev refresh is upstream |
| `sessions.md` | XDG data root; `/share` and `/bug` omitted |
| `security.md` | XDG trust path |
| `containerization.md` | copy the Linux binary; mount XDG roots, not `~/.pi/agent` |
| `llama-cpp.md` | XDG paths only |
| `nvidia-rtx-llama-cpp.md` | PowerShell/winget examples are Windows/upstream |
| `terminal-setup.md` | macOS sections kept; Linux-only release assets |
| `shell-aliases.md` | XDG settings path |
| `tmux.md` | XDG paths only |
| `windows.md` | no native Windows asset; source build or WSL |
| `termux.md` | npm steps upstream-only; aarch64 release asset |
| `configuration.md` | `<agent-dir>` = `~/.config/pi-rs`; sessions in data root |
| `prompt-templates.md` | XDG prompts directory |
| `skills.md` | XDG skills directory |
| `themes.md` | XDG themes directory |
| `packages.md` | npm/git/local arms; npm delegated to npm/pnpm/bun |
| `mcp.md` | stdio/HTTP/OAuth/resources/`/mcp` ported; XDG `mcp-auth.json` |
| `extensions.md` | QuickJS host + E0–E3; E4 `node:*` gap; XDG extensions dir |
| `custom-provider.md` | models.json + extension providers; XDG `auth.json` |
| `virtual-models.md` | XDG paths only |
| `tui.md` | `pi-tui` crate vs Node package; QuickJS component host |
| `cli-integration.md` | no TypeScript SDK; print/JSON/RPC are the surface |
| `sdk.md` | no in-process TS SDK in pi-rs |
| `cli.md` | no install detection/self-update; `/share`, `/bug` omitted |
| `codemode.md` | real `pi-codemode` crate; host wiring open; JS, not Node |
| `models-configuration.md` | XDG `models.json` location; generated-file header kept |
| `compaction.md` | XDG settings path |
| `json.md` | XDG paths only |
| `rpc.md` | partial command set; SDK/`RpcClient` upstream-only |
| `rpc-commands.md` | only the ported subset is available |
| `rpc-extension-ui.md` | UI subprotocol ported; RPC coverage partial |
| `message-types.md` | XDG paths only |
| `session-format.md` | JSONL v3 byte-compat; `float_roundtrip` gap |
| `settings.md` | XDG `settings.json`; inert share setting; `npmCommand` |
| `environment-variables.md` | `PI_CODING_AGENT_DIR`; inert share vars |
| `keybindings.md` | XDG keybindings path |
| `slash-commands.md` | `/share` and `/bug` not in the ported menu |
| `providers.md` | Radius login ported; uploads omitted |

## Verification

- `rg --files docs/upstream | wc -l` → 47 files (42 md + docs.json + 4 images).
- Round-trip note-strip comparison against upstream → 42/42 exact matches.
- Links in `docs/upstream/README.md` use upstream filenames, so they resolve
  relative to that directory and match `docs.json`'s `path` entries.
