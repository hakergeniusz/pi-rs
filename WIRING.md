# WIRING.md — P7 telemetry subset (branch `feat/p7-telemetry`)

Deliverable of the `feat/p7-telemetry` slice. The coordinator owns every
shared file; this document is the exact set of edits needed to land the new
module, plus the rationale for each upstream surface we deliberately skipped.

## 1. Files produced by this slice

| File | Contents |
|---|---|
| `crates/pi-coding-agent/src/telemetry.rs` | Metric instruments + `Telemetry` trait, `NoopTelemetry`, thread-safe `InMemoryTelemetry`, `render_trace`, the `core/telemetry.ts` env/settings gate and host constructor. `mod tests` is last. |
| `WIRING.md` | This file. |

No shared file was touched: `lib.rs`, `main.rs`, both `Cargo.toml`s,
`Cargo.lock`, `HANDOFF.md` and `AGENTS.md` are unchanged on this branch.

## 2. Required edit in `lib.rs` (one line)

Add the module declaration next to the other `pub mod`s (after
`pub mod skills;`, before `pub mod tools;`):

```rust
/// Telemetry subset: metrics, noop/in-memory recorders and trace renderer
/// (`packages/telemetry/src`, `core/telemetry.ts`).
pub mod telemetry;
```

The file is a single `src/telemetry.rs`; no `telemetry/` directory and no
`Cargo.toml` change are needed. All dependencies (`serde_json`, `uuid`) are
already workspace dependencies of `pi-coding-agent`.

## 3. Required edits in `main.rs`

### 3.1 Construct the telemetry once, after settings are final

`main.rs:386` is the authoritative `SettingsManager::create` (the one after
project trust is resolved; there is an earlier startup copy at `:304`). After
that `settings` binding is in scope, insert:

```rust
let telemetry: std::sync::Arc<dyn pi_coding_agent::telemetry::Telemetry> =
    pi_coding_agent::telemetry::telemetry_for_settings(
        settings.get_enable_install_telemetry(),
    );
```

Signature of the settings getter (already present,
`config/settings.rs:1867`):

```rust
pub fn get_enable_install_telemetry(&self) -> bool
```

Signatures exported by the new module:

```rust
pub const ENV_TELEMETRY: &str = "PI_TELEMETRY";

pub fn is_truthy_env_flag(value: Option<&str>) -> bool;
pub fn is_install_telemetry_enabled(settings_enabled: bool, telemetry_env: Option<&str>) -> bool;
pub fn is_install_telemetry_enabled_from_env(settings_enabled: bool) -> bool;
pub fn construct_telemetry(enabled: bool) -> std::sync::Arc<dyn Telemetry>;
pub fn telemetry_for_settings(settings_enabled: bool) -> std::sync::Arc<dyn Telemetry>;

pub fn telemetry_now() -> f64;                 // wall ms, microsecond resolution
pub fn render_trace(spans: &[StoredSpan]) -> String;
```

Semantics of the gate (verbatim from `core/telemetry.ts:3-13`):

* when `PI_TELEMETRY` is **set at all**, it overrides `enableInstallTelemetry`;
* truthy values are exactly `"1"`, `"true"`, `"yes"` (case-insensitive);
  empty, `"0"`, `"false"`, `"no"` and anything else are false;
* when the variable is absent, the persisted setting decides (default
  `true`, `settings.rs:86`);
* disabled → `NoopTelemetry` (retains nothing), enabled → a fresh
  `InMemoryTelemetry` with a random scope. Nothing is ever sent over the
  network by this slice.

`Arc<dyn Telemetry>` is `Send + Sync`, so it can be cloned into mode structs
or spawned threads without further wrapping.

### 3.2 Install / thread the handle

`main.rs` delegates execution to the mode runners (`run_print_mode`,
`run_json_mode`, the RPC entry and the interactive mode), so the handle must
be threaded to whichever of those the coordinator wants to observe it. Two
non-invasive options, in order of preference:

1. Add `telemetry: Arc<dyn pi_coding_agent::telemetry::Telemetry>` to
   `PrintRuntime` (and the equivalent interactive/runtime state) and pass the
   cloned `telemetry` into the chosen mode's constructor. This keeps the
   lifetime explicit and mirrors how `settings`/`extensions` are already
   carried.
2. If threading through every mode is too invasive for the landing commit,
   keep a process-wide accessor in `main.rs`:

   ```rust
   static TELEMETRY: std::sync::OnceLock<
       std::sync::Arc<dyn pi_coding_agent::telemetry::Telemetry>,
   > = std::sync::OnceLock::new();

   fn telemetry() -> &'static std::sync::Arc<dyn pi_coding_agent::telemetry::Telemetry> {
       TELEMETRY.get_or_init(|| {
           pi_coding_agent::telemetry::telemetry_for_settings(true)
       })
   }
   ```

   and call `TELEMETRY.set(telemetry)` from the construction site in §3.1
   (guard with `let _ = …` because more than one `main.rs` path can construct
   the runtime). This is a port-only wiring convenience, not an upstream
   surface.

Leaving the handle installed but unused is acceptable and is the most
faithful choice — see §4.

### 3.3 Which events feed it (important fidelity note)

Upstream `packages/coding-agent` **does not feed any agent-loop event into a
`TelemetryContext`**. The only consumer of `isInstallTelemetryEnabled` is the
enable gate, used twice:

1. `core/client-identity.ts:10-15` → `attribution: PI_ATTRIBUTION | null` on
   outgoing provider requests;
2. `modes/interactive/interactive-mode.ts:1472-1489` → a one-shot
   `fetch("https://pi.dev/api/report-install?version=…")`.

Both are external-backend effects and are deliberately omitted here (§4).
Therefore **no event should be wired into this telemetry by default**:
constructing the handle at startup and dropping it is upstream-faithful.
The in-memory recorder exists for embedders/tests and is fully usable through
the public API:

```rust
// Span with nested child, event and error status:
telemetry.start_span(SpanOptions::new("turn"), &mut |span| {
    span.set_attributes(attrs);
    span.add_event("tool-call", event_attrs);
    span.start_span(SpanOptions::new("tool.read"), &mut |child| {
        child.set_status(SpanStatus::Error {
            error: Some(SpanError { name: "E".into(), message: "msg".into() }),
        });
    });
});

// Counters / histograms (measurements are recorded as count-one rows):
telemetry.counter("tool.calls", &InstrumentOptions::default())
    .add(1.0, attrs);
telemetry.histogram("latency", &HistogramOptions {
    boundaries: Some(vec![10.0, 100.0]), ..Default::default()
}).record(42.0, attrs, Some(carrier));

// Render / inspect:
let text = pi_coding_agent::telemetry::render_trace(&telemetry.stored_spans());
let rows = telemetry.find_metrics(&MetricQuery { name: "tool.calls".into(), ..Default::default() });
```

If the coordinator nonetheless wants a smoke of the recorder during startup,
the only honest place is a single root span around the gate decision itself:

```rust
telemetry.start_span(SpanOptions::new("install-telemetry-check"), &mut |span| {
    span.set_attributes([(
        "enabled".to_string(),
        AttributeValue::Bool(enabled),
    )].into_iter().collect());
});
```

Label that addition as port-specific in `HANDOFF.md`; it is **not** an
upstream event.

## 4. Deliberate omissions and why

| Upstream surface | Why omitted |
|---|---|
| `reportInstallTelemetry` fetch (`interactive-mode.ts:1472-1489`) and `PI_ATTRIBUTION` in `client-identity.ts:10-15` | Require an external `pi.dev` endpoint / outbound HTTP. Explicitly excluded by the "skip what requires an external telemetry backend" instruction. The env/settings *gate* that drives them is ported verbatim. |
| HTTP/OTel exporters, Chord SQLite store, `TelemetryStore` implementations other than the in-memory one | No external backend in scope; `TelemetryStore` is represented by the in-memory `get_trace`/`find_traces`/`find_metrics` methods. |
| TS type-level machinery: `defineTelemetrySchema`, `createTypedMetrics`, `createTypedSpanStarter`, `Infer*`/`Exact*`/`AttributeDefinitionValue` conditional types (`index.ts:132-461`, `metrics.ts:43-145`) | Compile-time type sugar with no runtime behavior; Rust has no equivalent need. No data-model semantics were lost. |
| Async `startSpan(options, callback): Promise<T>` | In-memory recording is synchronous; callbacks are `&mut dyn FnMut(&dyn TelemetrySpan)` and capture results directly. Avoids forcing the process-wide tokio runtime into the API. |
| `WeakMap<TelemetryContext, TelemetryCarrier>` (`memory.ts:129`) | Private identity bookkeeping; replaced by the explicit `TelemetrySpan::carrier()` method, which `inject` calls. Observable semantics (inject returns a detached carrier for known spans, `None` otherwise) are preserved. |
| Settled-parent "inert span" path (`memory.ts:219`) | Implemented for live child calls (`MemorySpan::start_span` carries the parent span). It is effectively unreachable through the Rust API because the `&dyn TelemetrySpan` handle cannot outlive its callback; upstream's JS handle can be stashed and reused. Extracted parents deliberately skip this check, exactly like upstream's parent object, which has no `span` field. A panicking callback produces `SpanStatus::Error { error: None }` (bare error) and the panic is resumed after settlement, because Rust panic payloads are not JS `Error` objects. |
| `findTraces` cursor JSON parse / `limit == 0` throw (`memory.ts:400-435`) | Malformed cursor → treated as no cursor, `limit == 0` → empty page, instead of throwing. Documented in the module doc and code comments. |
| `Object.entries` insertion order for attributes | `BTreeMap` gives deterministic key order. Tests choose keys explicitly, so `render_trace` output is byte-exact. |
| JS exponent notation for magnitudes ≥ 1e21 in `JSON.stringify` | Rust's decimal display is used for those extremes; every value exercised by upstream's tests and by this port's tests is identical. |

## 5. Mechanism deviations (no behavior change to ported surfaces)

* `performance.now()`/`Date.now()` → `Instant` + `SystemTime`, keeping the
  same microsecond rounding and 10 ms re-anchor tolerance (`memory.ts:105,
  116-126`). `crypto.randomUUID()` → `uuid::Uuid::new_v4()` (`:321`).
* `NOOP_TELEMETRY_CONTEXT`/`NOOP_TELEMETRY_METRICS`/`NOOP_TELEMETRY_PROPAGATION`
  are represented by one `NoopTelemetry` implementing the whole host
  contract (`noop.ts:8-37`), which is all the port needs.
* `SpanLink.context` is the carrier directly rather than an opaque context
  resolved through a private map (`index.ts:19-22`, `memory.ts:149-152`).
* `findMetrics` returns `count = 1` rows exactly as upstream (`memory.ts:449-459`);
  no aggregation engine was added.

## 6. Tests in the module (synthetic data only)

1. `env_flag_matches_upstream_truthy_rules` — `core/telemetry.ts:3-13`.
2. `noop_drops_everything_but_runs_callbacks` — `noop.ts:8-37`.
3. `instruments_record_measurements_with_metadata` — `metrics.ts:20-41`,
   `memory.ts:279-307`.
4. `spans_accumulate_events_attributes_and_status` — `memory.ts:213-277`.
5. `extract_inject_round_trip` — `memory.ts:332-354`.
6. `extracted_parent_creates_a_real_child_after_settlement` — `memory.ts:339-354`, `:219` (extracted parents have no `span`, so they skip the inert check).
7. `find_metrics_filters_and_aggregates` — `memory.ts:439-460`.
8. `find_traces_paginates_with_a_cursor` — `memory.ts:399-437`.
9. `escaping_rounding_and_json_are_verbatim` — `render.ts:3-21`.
10. `render_trace_matches_upstream_output` — `render.ts:28-72`, exact string.
11. `render_trace_renders_bare_error_status` — `render.ts:49-53`.
12. `render_trace_deduplicates_by_id` — `render.ts:29`.
13. `concurrent_recording_is_thread_safe` — four threads × 50 measurements
    and 10 spans; asserts 200 measurements and 40 spans.

**13 tests.** The coordinator should run the full serial RAM-container gate
(`scripts/ram-build.sh gate`) — this slice never runs cargo itself.
