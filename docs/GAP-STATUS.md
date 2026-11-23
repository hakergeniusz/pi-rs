# Upstream ↔ pi-rs coverage (gap-wave snapshot)

> Snapshot: 2026-11-23, at `45c3302` (the ten-agent gap-wave integration
> `1279ad7` plus follow-up fixes).
> Ground truth is pi 1.5.2 at `/home/hakergeniusz/tmp-11-18/pi-152` on the
> operator's machine (re-clone if gone: `gh repo clone earendil-works/pi
> /tmp/pi`). This is a mechanism-level map, **not** a claim of 1:1 parity:
> each module's `//!` docs cite the upstream lines it ports, and the `WIRING.md`
> notes next to the newer crates record the exact deviations and host call sites.

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
| `pi-ai` — built-in OAuth flows | `pi-ai::auth`, `pi-coding-agent::oauth` | Ported | Anthropic/Codex/Copilot login + wire-time refresh of expiring stored creds (5-min window, 15 s timeout, double-checked persistence, injectable seam); Copilot's bounded 429 back-off (`fetchWithRateLimitRetry`) and `toAuth` enterprise/proxy base URL now ported; remaining flows keep the honest error |
| `pi-agent-core` | `pi-agent` (native) + host package shim | Ported / data-shape | native loop complete; shim has no live Agent loop |
| `pi-tui` | `pi-tui` + host package shim | Ported / data-shape | full Rust renderer; shims are data-shape only |
| `pi-coding-agent` — CLI, modes, sessions, tools, extensions E0–E3 | `pi-coding-agent` | Ported | `pi-tui` `ui_loop` has autonomous timers (`set_timeout`/`set_interval` on a dedicated thread); other mechanism deviations documented in module docs |
| `pi-coding-agent` — RPC remaining fidelity | `modes/rpc.rs` | Partial | full preflight order (extension commands, input handlers, skill/template expansion, no-model/auth gates) with narrow host seams; bounded FIFO command dispatcher + `activeCommands` mirror, per-call `blockImages` re-read, session rebind, MCP tick per command; command **execution** is still serialized on the shared runtime behind the FIFO turnstile, and signal handling/graceful shutdown are open |
| `pi-coding-agent` — image attachment pipeline | `images/{mod,sniff,resize,transform}.rs` | Partial | real pixel backend (`image` 0.25.10: decode/Lanczos3 resize/PNG+JPEG encode, BMP→PNG) wired into `read.rs` and `@file`/`file_processor.rs`; `blockImages` in all four modes — interactive and RPC re-read it per `convertToLlm` call, print/JSON still snapshot once at startup; live `ImageSettings` cell + `afterToolCall` normalizer in interactive mode. Remaining: clipboard-image paste, image-setting live sweep outside interactive, tool/MCP image normalization at non-interactive hooks |
| `pi-coding-agent` — telemetry | `telemetry/{host,ping,exporters,conformance}.rs` | Partial | ping fires at upstream's interactive changelog point and the exporter flushes at shutdown, gated by `PI_OFFLINE`/`PI_TELEMETRY` + setting; 12 conformance cases; real reqwest ping/OTLP transports are installed at startup via `install_from_env_with_settings` (`bac3519`), gated by `PI_OFFLINE`/`PI_TELEMETRY` + setting so no socket opens by default; OTLP endpoint additive |
| `pi-mcp` — client, stdio/HTTP transports, OAuth, resources | `pi-mcp` | Ported | `pi mcp` CLI and `/mcp` selector wired |
| `pi-mcp` — startup wait, `list_changed`, result `_meta` | `pi-ext-js::mcp_manager`, `pi-mcp::types`, `pi-coding-agent::extensions` | Partial | background connects + per-server readiness queue survives the one-shot `StartupWait` bound; non-blocking `try_drain_notifications` + `list_changed` tool re-registration; host `mcp_tick` driven from the interactive pump and per RPC command; result `_meta`/`annotations` preserved. Remaining: stderr still captured via a temp-file sink, the blocking `poll_notifications` form is kept for barrier hosts |
| `pi-codemode` | `pi-codemode` | Partial | inline VM, deterministic runs, `models.*`, grammar hook; local real transports (`ThreadWorkerTransport`, `RemoteCodemodeTransport`, `serve_remote`, `InProcessRemote`) plus worker admission; true Node `worker_threads`/Cloudflare/wasm compilation remain typed stubs (API compat) |
| `pi-env` | `crates/pi-env` + `pi-coding-agent::remote_env` | Ported | all nine upstream modules, 46 fake-backend tests; host-wired as `pi remote` (`remote_env.rs`): daemon transport, `ExecutionEnv`, host-driven ticks, PTY input on the process Tokio runtime, explicit SSH host-key onboarding |
| `pi-coding-agent` — session deltas | `session/{bash_execution,readonly}.rs`, `session/store.rs` | Partial | `SessionListProgress` is consumed by the interactive session selector and `ReadonlySessionManager` by `session/cwd.rs`; live `bash_execution_update` emission is still library-only |
| extensions runtime | `pi-ext-js` | Partial | E0–E3 landed; E4 static CJS, `/reload` unload, `node:*` and host packages, `URL.canParse`, ancestor `node_modules` resolution (scoped packages, exports targets, main/index) and statically-foldable `require(expr)` landed; genuinely dynamic/computed specifiers still resolve to the missing-module error, plus a few unserved `pi-ai` subpaths and type-only exports |
| package manager (npm/git/local arms) | `pi-coding-agent::package_manager` | Ported | npm registry work delegated to `npm`/`pnpm`/`bun` |

## Still open

The ten-agent gap wave (`1279ad7`) closed most of the previous remainder; what
follows is the *genuine* remainder. Each item is a mechanism tail or a
deliberate scope decision, not a claim of full 1:1 parity.

- **RPC execution serialization:** commands are received concurrently and
  dispatched through a bounded FIFO turnstile, but execution itself is
  serialized on the shared runtime (upstream fires each `handleInputLine` as
  an unawaited promise); signal handling/graceful shutdown are still open. The
  `activeCommands` mirror, session rebind, per-call `blockImages` re-read and
  per-command MCP tick landed.
- **Extensions E4 tail:** `URL.canParse`, ancestor `node_modules` resolution and
  `require(expr)` folding are ported, but `require` folding covers only
  *statically resolvable specifiers* — a genuinely dynamic/computed specifier
  still takes the unresolved-module error. A few unserved `pi-ai` subpaths and
  type-only exports remain.
- **Codemode transports:** the local thread and injected-client transports are
  real; a true Node `worker_threads` runtime, a Cloudflare Worker isolate and
  WebAssembly compilation remain typed stubs (kept for API compatibility with
  upstream's exact error strings). The wasm `Stats`/`HMAC`/WHATWG URL shims are
  unported, and codemode tools/`ModelRuntime` stay synchronous.
- **Image tails:** `blockImages` is re-read per call in interactive and RPC but
  still snapshotted once at startup in print/JSON; clipboard-image paste needs a
  host clipboard-read hook; the live image-setting sweep and `afterToolCall`
  tool/MCP image normalization are wired in interactive mode only.
- **MCP mechanism tails:** `StartupWait` is still one-shot once its bound
  elapses (late connections now survive via a per-server readiness queue), and
  the stdio stderr tail still uses a temp-file sink; the blocking
  `poll_notifications` form is retained for hosts that need a barrier.
- **Session deltas are partly wired:** `SessionListProgress` is consumed by the
  interactive session selector and `ReadonlySessionManager` by `session/cwd.rs`;
  live `bash_execution_update` emission is still library-only.
- **`pi-env` is host-driven by design:** `pi remote` consumes the crate, but
  keepalive/watch ticks, PTY input on the process Tokio runtime and SSH
  host-key onboarding require the host to drive them (no internal timers).
- **OAuth refresh scope:** wire-time refresh now covers the three built-in flows
  (Anthropic/Codex/Copilot) including Copilot's 429 back-off and enterprise base
  URL; other providers keep the honest error. `sha2`+`base64` replace Web
  Crypto, and a cancelled wait cannot roll back a refresh another process made.
- **Policy / environment:** `/share` and `/bug` omitted (Radius upload policy).
- **Out of scope:** `chord`, `pi-durable`, `evals`, `apps/p2`.

For the dated narrative, see [HANDOFF.md](../HANDOFF.md); for the porting craft
and the crate map, see [PORTING.md](PORTING.md). The newest `WIRING.md` notes
live beside their crates: `crates/pi-env/WIRING.md`,
`crates/pi-codemode/WIRING.md`, and
`crates/pi-coding-agent/src/images/WIRING.md`.
