# Upstream ↔ pi-rs coverage (gap-wave snapshot)

> Snapshot: 2026-11-22, after `deb633e` (the host-wiring wave on `main`).
> Ground truth is pi 1.5.2 (`/tmp/pi`; re-clone with
> `gh repo clone earendil-works/pi /tmp/pi` if the tmpfs copy is gone). This is
> a mechanism-level map, **not** a claim of 1:1 parity: each module's `//!` docs
> cite the upstream lines it ports, and the `WIRING.md` notes next to the newer
> crates record the exact deviations and host call sites.

Legend:

- **Ported** — implemented, registered and reachable from the `pi` binary.
- **Library** — module complete and registered, but no host call site yet.
- **Partial** — behavior ported with deliberate, documented mechanism deviations.
- **Out of scope** — deliberately not ported.

| Upstream package / API | Rust home | Status | Notes |
|---|---|---|---|
| `pi-schema` | `pi-schema` | Ported | schema builders/compilers/validators |
| `pi-ai` — wire types, catalog, cost, retry | `pi-ai` | Ported | 1.5.2 catalog; retry policy in `pi-ai::retry` |
| `pi-ai` — baseline adapters | `pi-ai::providers` (anthropic, openai-completions, openai-codex) | Ported | existing adapters unchanged by this wave |
| `pi-ai` — Google GenAI + Vertex | `providers/google.rs`, `google_shared.rs` | Partial | thought signatures, tools, usage ported; provider knobs via `metadata`, no local ADC, subset strict-schema rewrite |
| `pi-ai` — Bedrock Converse Stream | `providers/bedrock.rs`, `bedrock_sigv4.rs` | Ported | SigV4 signing + binary event-stream decode, no AWS SDK dependency |
| `pi-ai` — OpenAI / Azure Responses | `providers/openai_responses{,_shared}.rs`, `azure_openai_config.rs` | Partial | full event protocol; provider options ride in `metadata` |
| `pi-ai` — Mistral Conversations + pi-messages | `providers/mistral.rs`, `pi_messages.rs` | Partial | no `signal`/strict-schema helper; pi-messages rejects unknown wire events instead of forwarding |
| `pi-ai` — built-in OAuth flows | `pi-ai::auth`, `pi-coding-agent::oauth` | Ported | Anthropic/Codex/Copilot login + wire-time refresh of expiring stored creds (5-min window, 15 s timeout, double-checked persistence, injectable seam); remaining flows keep the honest error, Copilot 429 back-off omitted |
| `pi-agent-core` | `pi-agent` (native) + host package shim | Ported / data-shape | native loop complete; shim has no live Agent loop |
| `pi-tui` | `pi-tui` + host package shim | Ported / data-shape | full Rust renderer; shims are data-shape only |
| `pi-coding-agent` — CLI, modes, sessions, tools, extensions E0–E3 | `pi-coding-agent` | Ported | host-driven timers and other mechanism deviations documented in module docs |
| `pi-coding-agent` — RPC remaining fidelity | `modes/rpc.rs` | Partial | preflight, input handlers, expansion, no-model gate and `model_select` callbacks have inert defaults; serial dispatch, no runtime rebind |
| `pi-coding-agent` — image attachment pipeline | `images/{mod,sniff,resize}.rs` | Partial | wired into `read.rs`, `@file`/`file_processor.rs` and `blockImages` `convertToLlm` in all four modes; headless modes snapshot the setting at startup, no pixel backend, 34 tests |
| `pi-coding-agent` — telemetry | `telemetry/{host,ping,exporters,conformance}.rs` | Partial | ping fires at upstream's interactive changelog point and the exporter flushes at shutdown, gated by `PI_OFFLINE`/`PI_TELEMETRY` + setting; 12 conformance cases; default transports stay inert (no real HTTP transport), OTLP endpoint additive |
| `pi-mcp` — client, stdio/HTTP transports, OAuth, resources | `pi-mcp` | Ported | `pi mcp` CLI and `/mcp` selector wired |
| `pi-mcp` — startup wait, `list_changed`, result `_meta` | `pi-ext-js::mcp_manager`, `pi-mcp::types`, `pi-coding-agent::extensions` | Partial | `mcp_tick` + `settle_mcp_startup` before first prompt, tool-exposure glob delegation and `list_changed` re-registration wired; interactive/RPC pump ticks unwired, `StartupWait` one-shot, notifications poll-based, stderr via temp-file sink |
| `pi-codemode` | `pi-codemode` | Partial | inline VM, deterministic runs, `models.*`, grammar hook; Node/Workers/wasm transports are typed stubs |
| `pi-env` | `crates/pi-env` | Library | all nine upstream modules, 46 fake-backend tests; no host crate consumes it yet |
| extensions runtime | `pi-ext-js` | Partial | E0–E3 landed; E4 static CJS, `/reload` unload, `node:*` and host packages landed; dynamic `require` / `node_modules` open |
| package manager (npm/git/local arms) | `pi-coding-agent::package_manager` | Ported | npm registry work delegated to `npm`/`pnpm`/`bun` |

## Still open

- **`pi-env` host wiring (still library-only):** daemon keepalive/watch ticks,
  PTY/process input on a Tokio runtime, explicit SSH host-key onboarding; no
  host crate consumes it yet.
- **Wiring tails:** the image `blockImages` filter is installed in all four
  modes, but headless modes snapshot it once at startup (upstream re-reads it
  per call); no pixel backend, so oversized/BMP images stay on upstream's
  "unavailable" omission path;
  telemetry ping/HTTP transports are inert by default (no real HTTP transport);
  MCP `StartupWait` is one-shot once its bound elapses, notifications are
  poll-based, and the interactive/RPC pump modes do not drive `mcp_tick`.
- **Partial mechanisms:** OAuth wire-time refresh covers only the three
  built-in flows (Anthropic/Codex/Copilot) and omits Copilot's 429 back-off;
  serial RPC command dispatch, session rebind and `bash_execution_update`
  deltas; synchronous codemode tools with typed transport stubs.
- **Extensions E4 tail:** dynamic `require`, `node_modules` resolution,
  `URL.canParse`, a few unserved `pi-ai` subpaths and type-only exports.
- **Policy / environment:** `/share` and `/bug` omitted (Radius upload policy);
  macOS release assets await a hosted runner.
- **Out of scope:** `chord`, `pi-durable`, `evals`, `apps/p2`.

For the dated narrative, see [HANDOFF.md](../HANDOFF.md); for the porting craft
and the crate map, see [PORTING.md](PORTING.md). The newest `WIRING.md` notes
live beside their crates: `crates/pi-env/WIRING.md`,
`crates/pi-codemode/WIRING.md`, and
`crates/pi-coding-agent/src/images/WIRING.md`.
