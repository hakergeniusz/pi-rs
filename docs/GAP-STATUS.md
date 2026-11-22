# Upstream ↔ pi-rs coverage (gap-wave snapshot)

> Snapshot: 2026-11-22, after `16f0946` (the gap-wave integration on `main`).
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
| `pi-ai` — built-in OAuth flows | `pi-ai::auth`, `pi-coding-agent::oauth` | Partial | Anthropic/Codex/Copilot login wired to `/login`; refresh is library-only, Copilot 429 back-off omitted |
| `pi-agent-core` | `pi-agent` (native) + host package shim | Ported / data-shape | native loop complete; shim has no live Agent loop |
| `pi-tui` | `pi-tui` + host package shim | Ported / data-shape | full Rust renderer; shims are data-shape only |
| `pi-coding-agent` — CLI, modes, sessions, tools, extensions E0–E3 | `pi-coding-agent` | Ported | host-driven timers and other mechanism deviations documented in module docs |
| `pi-coding-agent` — RPC remaining fidelity | `modes/rpc.rs` | Partial | preflight, input handlers, expansion, no-model gate and `model_select` callbacks have inert defaults; serial dispatch, no runtime rebind |
| `pi-coding-agent` — image attachment pipeline | `images/{mod,sniff,resize}.rs` | Library | 29 tests; not wired into `read.rs`/`file_processor.rs`, no pixel backend dependency |
| `pi-coding-agent` — telemetry | `telemetry/{ping,exporters,conformance}.rs` | Library | inert-by-default ping + additive OTLP/HTTP exporters + 12 conformance cases; no host call sites |
| `pi-mcp` — client, stdio/HTTP transports, OAuth, resources | `pi-mcp` | Ported | `pi mcp` CLI and `/mcp` selector wired |
| `pi-mcp` — startup wait, `list_changed`, result `_meta` | `pi-ext-js::mcp_manager`, `pi-mcp::types` | Library | host-driven ticks not wired into the mode loop; stderr captured via temp-file sink |
| `pi-codemode` | `pi-codemode` | Partial | inline VM, deterministic runs, `models.*`, grammar hook; Node/Workers/wasm transports are typed stubs |
| `pi-env` | `crates/pi-env` | Library | all nine upstream modules, 46 fake-backend tests; no host crate consumes it yet |
| extensions runtime | `pi-ext-js` | Partial | E0–E3 landed; E4 static CJS, `/reload` unload, `node:*` and host packages landed; dynamic `require` / `node_modules` open |
| package manager (npm/git/local arms) | `pi-coding-agent::package_manager` | Ported | npm registry work delegated to `npm`/`pnpm`/`bun` |

## Still open

- **Host wiring for library slices:** `pi-env` ticks/PTY/SSH onboarding; image
  call sites in `read.rs`, `file_processor.rs` and `convertToLlm`; telemetry
  ping gating and exporter injection; MCP startup-wait/notification ticks.
- **Partial mechanisms:** OAuth refresh on the request path; serial RPC command
  dispatch, session rebind and `bash_execution_update` deltas; synchronous
  codemode tools with typed transport stubs.
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
