# WIRING — telemetry exporter surface

Slice `feat/telemetry-exporters`. Files changed:

- `crates/pi-coding-agent/src/telemetry.rs` — module docs and `pub mod`
  declarations for the three submodules below; no runtime behaviour changed.
- `crates/pi-coding-agent/src/telemetry/exporters.rs` (new) — exporter traits,
  batching, OTLP/HTTP JSON encoding, redaction, fake transports.
- `crates/pi-coding-agent/src/telemetry/ping.rs` (new) — install/update ping,
  env gates, client identity/attribution, `User-Agent`.
- `crates/pi-coding-agent/src/telemetry/conformance.rs` (new) — adapter
  fixture/case shapes and the ported conformance case list.

No other crate, `Cargo.toml`, `Cargo.lock`, `main.rs`, `lib.rs`, `HANDOFF.md`
or `AGENTS.md` was touched.

## Ground-truth finding

Upstream v1.5.2 ships **no span/metric exporter trait and no OTLP client**. A
search over `packages/` for `TelemetryExporter`, `SpanExporter`, `OTLP`,
`exportSpans`, `BatchSpanProcessor` and related names matches only
`packages/coding-agent/src/modes/interactive/interactive-mode.ts` (the install
ping). The "exporter surface" asked for in this slice is therefore additive,
built around the ported `StoredSpan` (`index.ts:32-52`) and `StoredMetric`
(`index.ts:82-92`) records; upstream's own telemetry egress is just the ping
(`interactive-mode.ts:1472-1489`) and the conformance shapes
(`testing/{types,conformance}.ts`).

## Dependencies

**None added.** No Cargo manifest or lockfile change, no Cargo feature. The
new modules use only `std` plus `serde_json` (already a workspace dependency)
and the existing `pi-coding-agent` telemetry types. The HTTP side is a trait
(`ping::PingTransport`, `exporters::HttpTransport`) so the crate still has no
HTTP client dependency.

## Host wiring (to be applied by the coordinator in `main.rs` / interactive mode)

1. **Gate once, upstream-exact.** `PI_TELEMETRY` overrides the persisted
   setting; only `1`/`true`/`yes` (case-insensitive) are truthy, and an env
   value of `""` is *false*:

   ```rust
   let settings_enabled = settings.get_enable_install_telemetry();
   let enabled = telemetry::is_install_telemetry_enabled_from_env(settings_enabled);
   let recorder = telemetry::telemetry_for_settings(settings_enabled);
   ```

2. **Install/update ping** at the two upstream changelog trigger points
   (`interactive-mode.ts:1383-1386` fresh install, `:1396-1399` detected
   update — both in `getChangelogForDisplay`, `:1373-1403`):

   ```rust
   use pi_coding_agent::telemetry::ping::{
       report_install_ping_from_env, NoopPingTransport, PingTransport,
   };
   // Default: inert, no socket. A networked host injects its own transport.
   let outcome = report_install_ping_from_env(
       VERSION, settings_enabled, &NoopPingTransport,
   );
   ```

   The gates are checked in upstream's order: `PI_OFFLINE` (JS truthiness,
   any non-empty string) then the telemetry gate. The request contains only
   the version in the query string and the runtime-neutral `User-Agent`;
   no body, session id, model, prompt or path is sent. `PingOutcome` is for
   diagnostics only — upstream swallows all errors, and so does this port.

3. **Provider attribution** (`client-identity.ts:10-16`) when building client
   identities:

   ```rust
   let identity = telemetry::ping::get_client_identity(
       VERSION, settings_enabled, telemetry_env.as_deref(),
   );
   // identity.attribution == Some(PI_ATTRIBUTION) only when the gate passes.
   ```

4. **OTLP export** (additive; upstream has no equivalent). Build the exporter
   with the host's HTTP client behind the `HttpTransport` trait, optionally
   wrap it in a `BatchExporter`, and flush at shutdown or on demand:

   ```rust
   use pi_coding_agent::telemetry::exporters::{
       BatchExporter, ExportError, HttpTransport, OtlpHttpExporter,
       export_telemetry,
   };
   let inner = OtlpHttpExporter::new("http://localhost:4318", host_transport);
   let exporter = BatchExporter::new(std::sync::Arc::new(inner));
   let report = export_telemetry(recorder.as_ref(), &exporter);
   // report.{spans, metrics, errors} — then exporter.shutdown().
   ```

   `OtlpHttpExporter::default()` uses `NoopHttpTransport` and never opens a
   socket. Nothing starts a timer or background task.

5. **Conformance** for adapter authors:
   `telemetry::conformance::{in_memory_adapter_factory,
   create_telemetry_adapter_conformance}` returns cases with upstream's
   `group`/`name` metadata; call `case.run()` on a fresh fixture per case.

## Deviations (mechanism only)

- **Exporters are additive.** The traits, batching and OTLP JSON payload are
  new surface with no upstream counterpart. They are shaped around existing
  ported records and documented line-by-line in `exporters.rs`.
- **No default network.** Upstream's `fetch` and OTLP-style exporters (in
  later pi versions) call the network directly. Here both egress points are
  trait-driven and the default implementations are inert. Hosts opt in by
  injecting a transport; tests use recording fakes only.
- **`PI_OFFLINE` truthiness.** The ping checks `if (process.env.PI_OFFLINE)`
  (`interactive-mode.ts:1473`), so `"0"` blocks it. `PI_TELEMETRY` uses the
  separate `isTruthyEnvFlag` rule (`core/telemetry.ts:3-6`), so `"0"`
  disables. Both are reproduced exactly rather than unified.
- **`User-Agent` runtime token.** Upstream formats `node/vX` or `bun/X` from
  `process.versions`; Rust has no equivalent, so the token is the literal
  `rust` while the `pi/<version> (os; runtime; arch)` shape is preserved.
  The ping payload itself is byte-for-byte the same version-only URL.
- **No `ClientIdentity` in the `pi-ai` port.** `ClientAttribution`,
  `ClientIdentity`, `PI_ATTRIBUTION` and `format_user_agent` are defined
  locally from `identity.ts:24`, `:75-82`; if `pi-ai` later ports them, these
  should be re-exported instead.
- **Conformance is synchronous.** Upstream fixtures are `AsyncDisposable`
  and callbacks return promises. Here `run()` is sync, the fixture owns only
  an `Arc<dyn Telemetry>`, and the factory yields a fresh `InMemoryTelemetry`.
  Two upstream cases are not portable: late calls on a JS span object that
  outlives its callback (Rust cannot retain the borrowed handle) and
  arbitrary async rejection values. The explicit-status-over-panic case uses
  `catch_unwind`; `undefined` attribute drops become explicit final values
  because the port's `AttributeValue` has no undefined variant. All other
  group and case names and assertion intents are preserved.
- **OTLP encoding details.** Local span ids are not W3C ids, so they are
  hashed with FNV-1a into fixed-width 16/32 hex ids. `StoredMetric` has no
  description or bucket boundaries, so histogram bucket counts are omitted
  and each recorded measurement maps to one data point over the 10-second
  interval from `memory.ts:298`.
- **Privacy.** `exporters::redact_attributes` drops keys matching
  credential names (`authorization`, `api_key`, `token`, `password`,
  `secret`, `session_id`, …) and replaces credential-shaped strings (API
  keys, GitHub/Slack tokens, bearer values, JWTs, PEM headers) with
  `[REDACTED]` in every exported attribute, event, link and error message.
  `OtlpHttpExporter::with_header` exists for `Content-Type` and similar;
  callers must not place secrets in headers. The ping sends no data beyond
  the version and `User-Agent`.

## Tests

Added 11 tests (no network, no env mutation, fakes only):

- `exporters.rs` (7): redaction, OTLP payload shape, inert default, batching
  retry/order, idle flush + queue cap, non-2xx status, host helper.
- `ping.rs` (3): env gating, version-only payload/agent, swallowed failures
  and attribution.
- `conformance.rs` (1): case count/groups and all 12 cases running against
  the in-memory adapter.

The existing 13 `telemetry.rs` tests are unchanged. The coordinator runs the
full gate; this slice ran single-file `rustfmt` only, per the no-cargo rule.
