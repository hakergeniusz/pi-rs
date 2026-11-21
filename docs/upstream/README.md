# Upstream coding-agent documentation (1.5.2)

This directory is a faithful port of the 42 markdown files in upstream
`packages/coding-agent/docs` (plus its `docs.json` navigation manifest and the
four images the docs reference). Upstream prose is **verbatim**; every file
starts with a short `> pi-rs note:` block that records where the Rust port's
behavior differs. Do not edit the ported prose — fix the note instead.

The grouping below mirrors upstream's `docs.json` navigation (copied here
unchanged, including its redirects). This README is an index for readers of
the repository; `docs.json` remains the canonical site navigation.

## Get Started

- [Overview](index.md)
- [Quickstart](quickstart.md)
- [How Pi Works](how-pi-works.md)

## Guides

### Run Pi

- [Use Pi in the Terminal](usage.md)
- [Choose a Model](models.md)
- [Manage Sessions and Context](sessions.md)
- [Run Pi Safely](security.md)
- [Isolate Pi](containerization.md)
- [Run Local Models](llama-cpp.md)
- [Run Local Models on NVIDIA RTX](nvidia-rtx-llama-cpp.md)
- [Configure Your Terminal](terminal-setup.md)
- [Configure Shell Commands](shell-aliases.md)
- [Run Pi in tmux](tmux.md)
- [Run Pi on Windows](windows.md)
- [Run Pi on Android](termux.md)

### Customize Pi

- [Configure Pi](configuration.md)
- [Create Prompt Templates](prompt-templates.md)
- [Add Skills](skills.md)
- [Create Themes](themes.md)
- [Use Pi Packages](packages.md)
- [Connect MCP Servers](mcp.md)

### Build on Pi

- [Build Extensions](extensions.md)
- [Add Custom Providers](custom-provider.md)
- [Route with Virtual Models](virtual-models.md)
- [Build Terminal UI Components](tui.md)
- [Integrate with the CLI](cli-integration.md)
- [Use the SDK](sdk.md)

## Reference

- [CLI](cli.md)
- [Codemode](codemode.md)
- [Slash Commands](slash-commands.md)
- [Settings](settings.md)
- [Models Configuration](models-configuration.md)
- [Environment Variables](environment-variables.md)
- [Keybindings](keybindings.md)
- [Providers](providers.md)
- [Session File Format](session-format.md)
- [Compaction and Branch Summaries](compaction.md)
- [JSON Event Stream](json.md)
- [RPC Protocol](rpc.md)
- [RPC Commands](rpc-commands.md)
- [RPC Extension UI](rpc-extension-ui.md)
- [Message Types](message-types.md)

## Flagged deviations and omitted features

These are called out at the top of the affected documents; see the repository
`README.md`, `PLAN.md`, `HANDOFF.md`, and `docs/PORTING.md` for the port's
authoritative state.

- **XDG paths:** every `~/.pi/agent/…` path below maps to `~/.config/pi-rs/…`
  (config, settings, auth, models, themes, prompts, skills, extensions, trust,
  `mcp.json`) or `~/.local/share/pi-rs/…` (sessions). Project-local `.pi/` is
  unchanged. `PI_CODING_AGENT_DIR` still collapses all roles into one dir.
- **Rust binary, not npm:** the executable is still `pi`, but it is built from
  the Rust workspace and installed from Linux release assets /
  `scripts/install.sh`. Node-specific install detection, self-update, and the
  TypeScript SDK are absent; macOS release assets are not built.
- **Deliberately omitted:** `/share` and `/bug` and their Radius/Gist uploads
  (so `sessionShareDestination`, `PI_SHARE_VIEWER_URL` and the bug-report upload
  text are inert).
- **Still open (1:1 gaps):** the cache warmer, `!` bash via PTY, ten RPC
  commands, E4 extension completeness (`node:*` builtins and host packages),
  serde_json `float_roundtrip`, and codemode host wiring.

## Other files

- [docs.json](docs.json) — upstream's navigation manifest, ported verbatim.
- `images/` — the four images referenced by the docs (`doom-extension.png`,
  `exy.png`, `interactive-mode.png`, `tree-view.png`), ported verbatim.
