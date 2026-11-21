//! Telemetry subset: metric instruments, a noop context, an in-memory
//! recorder, and the trace renderer.
//!
//! Ground truth — read in full before porting (all lines refer to
//! earendil-works/pi v1.5.2):
//!
//! * `packages/coding-agent/src/core/telemetry.ts:1-13` — `isTruthyEnvFlag`
//!   (`:3-6`) and `isInstallTelemetryEnabled` (`:8-13`), the only telemetry
//!   surface the coding-agent host actually consumes.
//! * `packages/telemetry/src/metrics.ts:10-145` — `InstrumentOptions`
//!   (`:10-13`), `HistogramOptions` (`:15-18`), the three instrument traits
//!   (`:20-30`), `TelemetryMetrics` (`:32-41`).
//! * `packages/telemetry/src/index.ts:3-130` — `AttributeValue` (`:3`),
//!   `SpanAttributes` (`:5-7`), `TelemetryCarrier` (`:9-10`),
//!   `TelemetryPropagation` (`:12-17`), `SpanLink` (`:19-22`),
//!   `SpanOptions` (`:24-28`), `SpanStatus` (`:30`), `StoredSpan` (`:32-52`),
//!   `StoredTrace` (`:54-57`), `TraceQuery`/`MetricQuery` (`:59-80`),
//!   `StoredMetric` (`:82-92`), `TelemetryStore` (`:94-99`),
//!   `TelemetryContext`/`TelemetrySpan` (`:101-114`), re-exports (`:116-130`).
//! * `packages/telemetry/src/noop.ts:1-37` — the frozen noop instruments
//!   (`:8-17`), propagation (`:20-23`) and context/span (`:25-37`).
//! * `packages/telemetry/src/memory.ts:27-494` — recorded types (`:27-61`),
//!   helpers (`:93-159`), span creation/lifecycle (`:161-277`), measurement
//!   recording (`:279-307`), `InMemoryTelemetryContext` (`:309-494`).
//! * `packages/telemetry/src/render.ts:1-72` — control-character escaping
//!   (`:3-10`), microsecond rounding (`:12-15`), attribute formatting
//!   (`:17-21`), `renderTrace` (`:23-72`).
//!
//! # Scope of the subset
//!
//! Ported: every *runtime* behaviour in the files above — metric kinds and
//! recording, the noop implementation, the thread-safe in-memory recorder,
//! span/measurement storage and queries, carrier injection/extraction, and
//! the exact render output. Deliberately omitted (see `WIRING.md`):
//!
//! * TS type-level machinery (`defineTelemetrySchema`, `createTypedMetrics`,
//!   `createTypedSpanStarter`, the `Infer*`/`Exact*` conditional types —
//!   `index.ts:132-461`, `metrics.ts:43-145`): compile-time sugar with no
//!   runtime counterpart to port.
//! * The external backend surfaces (`reportInstallTelemetry`'s fetch to
//!   `pi.dev`, `PI_ATTRIBUTION` in `client-identity.ts`, HTTP/OTel/SQLite
//!   exporters): these require a network endpoint, which the task excludes.
//!
//! # Deviations (mechanism only)
//!
//! * Upstream's async `startSpan(options, callback)` becomes a synchronous
//!   `FnMut` callback: in-memory recording never awaits, so host closures
//!   capture results directly. The process-wide tokio runtime is not needed.
//! * The `WeakMap<TelemetryContext, TelemetryCarrier>` identity map
//!   (`memory.ts:129`) is replaced by an explicit `TelemetrySpan::carrier`
//!   method; `inject` calls it instead of a private map lookup.
//! * `performance.now()`/`Date.now()` (`memory.ts:108-126`) become
//!   `Instant` + `SystemTime`, keeping the same 10 ms re-anchor tolerance and
//!   microsecond rounding. `crypto.randomUUID()` (`:321`) becomes
//!   `uuid::Uuid::new_v4()` (uuid is already a workspace dependency).
//! * `Object.entries` insertion order becomes [`BTreeMap`] key order (stable
//!   and deterministic); tests choose keys explicitly, so render output is
//!   byte-exact.
//! * JS `JSON.stringify`/number-to-string are reproduced by
//!   [`json_quote`]/[`js_number_string`] for strings, booleans, arrays,
//!   integers, `-0` and non-finite numbers. Only magnitudes where JS prints
//!   exponent notation (≥ 1e21) use Rust's decimal display instead.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

// -- carrier keys (memory.ts:93-95, :179-183) ------------------------------

/// `pi-memory-scope` (`memory.ts:180`).
const SCOPE_KEY: &str = "pi-memory-scope";
/// `pi-memory-span` (`memory.ts:181`).
const SPAN_KEY: &str = "pi-memory-span";
/// `pi-memory-trace` (`memory.ts:182`).
const TRACE_KEY: &str = "pi-memory-trace";
/// How far the anchored clock may drift from the wall clock before it is
/// re-anchored (`TOLERANCE_MS`, `memory.ts:105`).
const TOLERANCE_MS: f64 = 10.0;
/// `PI_TELEMETRY` (`core/telemetry.ts:10`, `cli/args.ts:467`).
pub const ENV_TELEMETRY: &str = "PI_TELEMETRY";

/// `AttributeValue` (`index.ts:3`): the seven value kinds an attribute may
/// hold. Undefined attributes are dropped, matching `copyAttributes`
/// (`memory.ts:141-147`), so there is no `Undefined` variant.
#[derive(Clone, Debug, PartialEq)]
pub enum AttributeValue {
    String(String),
    Number(f64),
    Bool(bool),
    StringArray(Vec<String>),
    NumberArray(Vec<f64>),
    BoolArray(Vec<bool>),
}

/// `SpanAttributes` (`index.ts:5-7`). Upstream uses an insertion-ordered
/// object; the port uses a sorted map for deterministic iteration (module
/// doc).
pub type SpanAttributes = BTreeMap<String, AttributeValue>;

/// A scalar equality operand for `MetricQuery.attributes`/`TraceQuery.attributes`
/// (`index.ts:63`, `:79`). Upstream compares with `===`, so the type must
/// match as well as the value.
#[derive(Clone, Debug, PartialEq)]
pub enum QueryValue {
    String(String),
    Number(f64),
    Bool(bool),
}

/// `TelemetryCarrier` (`index.ts:9-10`): opaque adapter-owned propagation
/// data. The in-memory adapter uses the three `pi-memory-*` keys above.
pub type TelemetryCarrier = BTreeMap<String, String>;

/// `carrierId` (`memory.ts:93-95`).
fn carrier_id(carrier: &TelemetryCarrier) -> String {
    format!(
        "{}:{}",
        carrier.get(SCOPE_KEY).map_or("", String::as_str),
        carrier.get(SPAN_KEY).map_or("", String::as_str)
    )
}

/// `SpanStatus` (`index.ts:30`). An error without detail is represented by
/// `error: None`, matching upstream's `{ status: "error" }`.
#[derive(Clone, Debug, PartialEq)]
pub enum SpanStatus {
    Ok,
    Error { error: Option<SpanError> },
}

/// The `{ name, message }` payload of an error status (`index.ts:30`).
#[derive(Clone, Debug, PartialEq)]
pub struct SpanError {
    pub name: String,
    pub message: String,
}

/// `copyStatus` (`memory.ts:154-159`).
fn copy_status(status: &SpanStatus) -> SpanStatus {
    match status {
        SpanStatus::Ok => SpanStatus::Ok,
        SpanStatus::Error { error } => SpanStatus::Error {
            error: error.as_ref().map(|e| SpanError {
                name: e.name.clone(),
                message: e.message.clone(),
            }),
        },
    }
}

/// `SpanLink` (`index.ts:19-22`) with its context collapsed to the carrier
/// that upstream would recover from the private identity map (module doc).
#[derive(Clone, Debug)]
pub struct SpanLink {
    pub context: TelemetryCarrier,
    pub attributes: SpanAttributes,
}

/// `SpanOptions` (`index.ts:24-28`).
#[derive(Clone, Debug)]
pub struct SpanOptions {
    pub name: String,
    pub attributes: SpanAttributes,
    pub links: Vec<SpanLink>,
}

impl SpanOptions {
    /// A span with no attributes and no links.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            attributes: SpanAttributes::new(),
            links: Vec::new(),
        }
    }
}

/// One event of a settled span (`StoredSpan.events`, `index.ts:45`).
#[derive(Clone, Debug, PartialEq)]
pub struct StoredEvent {
    pub name: String,
    pub time_ms: f64,
    pub attributes: SpanAttributes,
}

/// One link of a settled span (`StoredSpan.links`, `index.ts:46-50`).
#[derive(Clone, Debug, PartialEq)]
pub struct StoredLink {
    pub trace_id: String,
    pub span_id: String,
    pub attributes: SpanAttributes,
}

/// `StoredSpan` (`index.ts:32-52`): a settled span snapshot consumed by
/// [`render_trace`].
#[derive(Clone, Debug, PartialEq)]
pub struct StoredSpan {
    pub id: String,
    pub trace_id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub started_ms: f64,
    pub ended_ms: f64,
    pub attributes: SpanAttributes,
    pub events: Vec<StoredEvent>,
    pub links: Vec<StoredLink>,
    pub status: SpanStatus,
}

/// `StoredTrace` (`index.ts:54-57`).
#[derive(Clone, Debug, PartialEq)]
pub struct StoredTrace {
    pub trace_id: String,
    pub spans: Vec<StoredSpan>,
}

/// `TraceQuery` (`index.ts:59-72`).
#[derive(Clone, Debug, Default)]
pub struct TraceQuery {
    pub name: Option<String>,
    pub attributes: Option<BTreeMap<String, QueryValue>>,
    pub errors: bool,
    pub start_min: Option<f64>,
    pub start_max: Option<f64>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

/// One page returned by `findTraces` (`index.ts:96`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TracePage {
    pub traces: Vec<StoredTrace>,
    pub cursor: Option<String>,
}

/// `MetricQuery` (`index.ts:74-80`).
#[derive(Clone, Debug, Default)]
pub struct MetricQuery {
    pub name: String,
    pub start_min: Option<f64>,
    pub start_max: Option<f64>,
    pub attributes: BTreeMap<String, QueryValue>,
}

/// `RecordedTelemetryEvent` (`memory.ts:27-30`).
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedTelemetryEvent {
    pub name: String,
    pub attributes: SpanAttributes,
}

/// `RecordedTelemetryLink` (`memory.ts:32-35`).
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedTelemetryLink {
    pub context: TelemetryCarrier,
    pub attributes: SpanAttributes,
}

/// `RecordedTelemetrySpan` (`memory.ts:37-50`).
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedTelemetrySpan {
    pub id: u64,
    pub parent_id: Option<u64>,
    pub context: TelemetryCarrier,
    pub parent_context: Option<TelemetryCarrier>,
    pub links: Vec<RecordedTelemetryLink>,
    pub name: String,
    pub attributes: SpanAttributes,
    pub events: Vec<RecordedTelemetryEvent>,
    pub status: SpanStatus,
    pub settled: bool,
    pub end_sequence: Option<u64>,
}

/// `MetricKind` (`StoredMetric.kind`, `index.ts:85`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricKind {
    Counter,
    UpDownCounter,
    Histogram,
}

/// `InstrumentOptions` (`metrics.ts:10-13`).
#[derive(Clone, Debug, Default)]
pub struct InstrumentOptions {
    pub description: Option<String>,
    pub unit: Option<String>,
}

/// `HistogramOptions` (`metrics.ts:15-18`). Callers supply finite, strictly
/// increasing boundaries; malformed input behavior is unspecified upstream.
#[derive(Clone, Debug, Default)]
pub struct HistogramOptions {
    pub description: Option<String>,
    pub unit: Option<String>,
    pub boundaries: Option<Vec<f64>>,
}

/// `RecordedTelemetryMeasurement` (`memory.ts:52-61`).
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedTelemetryMeasurement {
    pub kind: MetricKind,
    pub name: String,
    pub description: Option<String>,
    pub unit: Option<String>,
    pub boundaries: Option<Vec<f64>>,
    pub value: f64,
    pub attributes: SpanAttributes,
    pub context: Option<TelemetryCarrier>,
}

/// `StoredMetric` (`index.ts:82-92`).
#[derive(Clone, Debug, PartialEq)]
pub struct StoredMetric {
    pub interval_ms: f64,
    pub name: String,
    pub kind: MetricKind,
    pub unit: Option<String>,
    pub attributes: SpanAttributes,
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
}

/// The `(interval, measurement)` pair stored in `InMemoryTelemetryState`
/// (`memory.ts:136`).
#[derive(Clone, Debug, PartialEq)]
struct MeasurementRecord {
    interval_ms: f64,
    measurement: RecordedTelemetryMeasurement,
}

/// `TelemetryCounter` (`metrics.ts:20-22`).
pub trait Counter: Send + Sync {
    fn add(&self, value: f64, attributes: SpanAttributes);
}

/// `TelemetryUpDownCounter` (`metrics.ts:24-26`).
pub trait UpDownCounter: Send + Sync {
    fn add(&self, value: f64, attributes: SpanAttributes);
}

/// `TelemetryHistogram` (`metrics.ts:28-30`). `context` is the recording
/// span's carrier (upstream's `TelemetryContext`, collapsed per module doc).
pub trait Histogram: Send + Sync {
    fn record(&self, value: f64, attributes: SpanAttributes, context: Option<TelemetryCarrier>);
}

/// `TelemetrySpan` (`index.ts:109-114`): the handle handed to a `startSpan`
/// callback. `carrier` bridges upstream's private `WeakMap` (module doc).
pub trait TelemetrySpan: Send + Sync {
    fn add_event(&self, name: &str, attributes: SpanAttributes);
    fn add_link(&self, link: SpanLink);
    fn set_attributes(&self, attributes: SpanAttributes);
    fn set_status(&self, status: SpanStatus);
    fn start_span(&self, options: SpanOptions, callback: &mut dyn FnMut(&dyn TelemetrySpan));
    fn carrier(&self) -> Option<TelemetryCarrier>;
}

/// A context returned by [`Telemetry::extract`]. Upstream returns `this`
/// (the recorder root) for an absent or malformed carrier (`memory.ts:343`);
/// the port represents that root with `carrier: None`.
#[derive(Clone, Debug, Default)]
pub struct SpanContext {
    carrier: Option<TelemetryCarrier>,
    /// Local span id when the carrier belongs to this recorder
    /// (`memory.ts:345`); `None` for a root or external parent.
    local_id: Option<u64>,
}

impl SpanContext {
    /// The root context (`extract` returning `this`, `memory.ts:343`).
    #[must_use]
    pub fn root() -> Self {
        Self {
            carrier: None,
            local_id: None,
        }
    }

    /// The extracted carrier, when the input was recognized.
    pub fn carrier(&self) -> Option<&TelemetryCarrier> {
        self.carrier.as_ref()
    }
}

/// Internal parent descriptor passed to `startInMemorySpan`
/// (`memory.ts:87-91`). `local_id` is `None` for an external parent; `span`
/// is present only for a live child call (`MemorySpan::start_span`) — an
/// `extract`ed parent has no `span` in upstream and therefore skips the
/// settled check (`memory.ts:219`).
struct ParentRef {
    local_id: Option<u64>,
    carrier: TelemetryCarrier,
    span: Option<Arc<Mutex<MutableSpan>>>,
}

/// The host-facing telemetry contract (`TelemetryContext` + `TelemetryStore`
/// + `TelemetryMetrics`, `index.ts:94-107` / `metrics.ts:37-41`).
pub trait Telemetry: Send + Sync {
    /// `counter(name, options)` (`metrics.ts:38`).
    fn counter(&self, name: &str, options: &InstrumentOptions) -> Box<dyn Counter>;
    /// `upDownCounter(name, options)` (`metrics.ts:39`).
    fn up_down_counter(&self, name: &str, options: &InstrumentOptions) -> Box<dyn UpDownCounter>;
    /// `histogram(name, options)` (`metrics.ts:40`).
    fn histogram(&self, name: &str, options: &HistogramOptions) -> Box<dyn Histogram>;

    /// `startSpan(options, callback)` at the recorder root (`index.ts:106`).
    fn start_span(&self, options: SpanOptions, callback: &mut dyn FnMut(&dyn TelemetrySpan));
    /// Start a span under an explicit (possibly extracted) parent.
    fn start_span_in(
        &self,
        parent: Option<&SpanContext>,
        options: SpanOptions,
        callback: &mut dyn FnMut(&dyn TelemetrySpan),
    );

    /// `TelemetryPropagation::inject` (`index.ts:14`).
    fn inject(&self, context: &dyn TelemetrySpan) -> Option<TelemetryCarrier>;
    /// `TelemetryPropagation::extract` (`index.ts:16`).
    fn extract(&self, carrier: Option<&TelemetryCarrier>) -> SpanContext;

    /// `getSpans` (`memory.ts:472-493`); detached, in span-start order.
    fn get_spans(&self) -> Vec<RecordedTelemetrySpan>;
    /// `getMetrics` (`memory.ts:462-470`); detached, in recording order.
    fn get_metrics(&self) -> Vec<RecordedTelemetryMeasurement>;
    /// Settled spans, sorted by start then id (`memory.ts:368-392`).
    fn stored_spans(&self) -> Vec<StoredSpan>;
    /// `findMetrics` (`memory.ts:439-460`).
    fn find_metrics(&self, query: &MetricQuery) -> Vec<StoredMetric>;
    /// `getTrace` (`memory.ts:394-397`).
    fn get_trace(&self, trace_id: &str) -> Option<StoredTrace>;
    /// `findTraces` (`memory.ts:399-437`).
    fn find_traces(&self, query: &TraceQuery) -> TracePage;
}

// -- noop implementation (noop.ts:1-37) ------------------------------------

/// Shared instrument that never inspects or retains anything
/// (`noop.ts:8-17`).
struct NoopInstrument;

impl Counter for NoopInstrument {
    fn add(&self, _value: f64, _attributes: SpanAttributes) {}
}

impl UpDownCounter for NoopInstrument {
    fn add(&self, _value: f64, _attributes: SpanAttributes) {}
}

impl Histogram for NoopInstrument {
    fn record(&self, _value: f64, _attributes: SpanAttributes, _context: Option<TelemetryCarrier>) {
    }
}

/// Shared noop span (`noopTelemetrySpan`, `noop.ts:25-34`).
struct NoopSpan;

impl TelemetrySpan for NoopSpan {
    fn add_event(&self, _name: &str, _attributes: SpanAttributes) {}
    fn add_link(&self, _link: SpanLink) {}
    fn set_attributes(&self, _attributes: SpanAttributes) {}
    fn set_status(&self, _status: SpanStatus) {}

    fn start_span(&self, _options: SpanOptions, callback: &mut dyn FnMut(&dyn TelemetrySpan)) {
        let span = NoopSpan;
        callback(&span);
    }

    fn carrier(&self) -> Option<TelemetryCarrier> {
        None
    }
}

/// `NOOP_TELEMETRY_CONTEXT` (`noop.ts:36-37`), implemented as the whole
/// host contract so it can stand in for either backend.
pub struct NoopTelemetry;

impl Telemetry for NoopTelemetry {
    fn counter(&self, _name: &str, _options: &InstrumentOptions) -> Box<dyn Counter> {
        Box::new(NoopInstrument)
    }

    fn up_down_counter(&self, _name: &str, _options: &InstrumentOptions) -> Box<dyn UpDownCounter> {
        Box::new(NoopInstrument)
    }

    fn histogram(&self, _name: &str, _options: &HistogramOptions) -> Box<dyn Histogram> {
        Box::new(NoopInstrument)
    }

    fn start_span(&self, options: SpanOptions, callback: &mut dyn FnMut(&dyn TelemetrySpan)) {
        self.start_span_in(None, options, callback);
    }

    fn start_span_in(
        &self,
        _parent: Option<&SpanContext>,
        _options: SpanOptions,
        callback: &mut dyn FnMut(&dyn TelemetrySpan),
    ) {
        let span = NoopSpan;
        callback(&span);
    }

    fn inject(&self, _context: &dyn TelemetrySpan) -> Option<TelemetryCarrier> {
        None
    }

    fn extract(&self, _carrier: Option<&TelemetryCarrier>) -> SpanContext {
        SpanContext::root()
    }

    fn get_spans(&self) -> Vec<RecordedTelemetrySpan> {
        Vec::new()
    }

    fn get_metrics(&self) -> Vec<RecordedTelemetryMeasurement> {
        Vec::new()
    }

    fn stored_spans(&self) -> Vec<StoredSpan> {
        Vec::new()
    }

    fn find_metrics(&self, _query: &MetricQuery) -> Vec<StoredMetric> {
        Vec::new()
    }

    fn get_trace(&self, _trace_id: &str) -> Option<StoredTrace> {
        None
    }

    fn find_traces(&self, _query: &TraceQuery) -> TracePage {
        TracePage::default()
    }
}

// -- the anchored clock and JS-compatible formatting -----------------------

/// `wall_clock_ms`: upstream's `Date.now()` (`memory.ts:118`).
fn wall_clock_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64() * 1000.0)
}

static MONOTONIC_START: OnceLock<Instant> = OnceLock::new();
static CLOCK_ANCHOR: OnceLock<Mutex<f64>> = OnceLock::new();

/// `performance.now()` (`memory.ts:117`): monotonic milliseconds from the
/// first call, so the re-anchor math in [`telemetry_now`] is the same.
fn monotonic_ms() -> f64 {
    let start = MONOTONIC_START.get_or_init(Instant::now);
    start.elapsed().as_secs_f64() * 1000.0
}

/// `telemetryNow` (`memory.ts:116-121`): wall-clock milliseconds rounded
/// to the microsecond, re-anchored when the two clocks drift by more than
/// [`TOLERANCE_MS`].
pub fn telemetry_now() -> f64 {
    let wall = wall_clock_ms();
    let clock = monotonic_ms();
    let anchor = CLOCK_ANCHOR.get_or_init(|| Mutex::new(wall - clock));
    let mut anchored = anchor.lock().unwrap_or_else(PoisonError::into_inner);
    if (*anchored + clock - wall).abs() > TOLERANCE_MS {
        *anchored = wall - clock; // :119
    }
    js_micros(*anchored + clock) // :120
}

/// `Math.round` (`render.ts:13-15`): JS rounds half toward +∞, unlike
/// `f64::round` which rounds half away from zero.
#[allow(clippy::float_cmp)] // mirrors exact number checks in render.ts
fn js_round(value: f64) -> f64 {
    (value + 0.5).floor()
}

/// `microseconds` (`render.ts:13-15`, `memory.ts:123-126`).
fn js_micros(ms: f64) -> f64 {
    js_round(ms * 1000.0) / 1000.0
}

/// `escaped` (`render.ts:5-10`): C0/C1 controls and DEL become `\uXXXX`.
fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        let code = u32::from(character);
        if code <= 0x1f || (0x7f..=0x9f).contains(&code) {
            out.push_str(&format!("\\u{code:04x}"));
        } else {
            out.push(character);
        }
    }
    out
}

/// `JSON.stringify` for a string (`render.ts:19`): the same short escapes
/// and `\u00xx` for the remaining controls.
fn json_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if u32::from(character) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", u32::from(character)));
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

/// JS `Number::toString` for the finite values relevant here: integral
/// doubles have no `.0`, `-0` prints `0`, and `NaN`/±∞ stringify as `null`.
#[allow(clippy::float_cmp)] // mirrors JS's exact number checks
fn js_number_string(number: f64) -> String {
    if number == 0.0 {
        return "0".to_string(); // covers -0 (JSON.stringify(-0) === "0")
    }
    if !number.is_finite() {
        return "null".to_string();
    }
    if number.fract() == 0.0 && number.abs() < 1e21 {
        return format!("{number:.0}");
    }
    format!("{number}")
}

/// `JSON.stringify` for an `AttributeValue` (`render.ts:19`).
fn json_stringify(value: &AttributeValue) -> String {
    match value {
        AttributeValue::String(text) => json_quote(text),
        AttributeValue::Number(number) => js_number_string(*number),
        AttributeValue::Bool(boolean) => boolean.to_string(),
        AttributeValue::StringArray(items) => {
            let entries = items
                .iter()
                .map(String::as_str)
                .map(json_quote)
                .collect::<Vec<_>>();
            format!("[{}]", entries.join(","))
        }
        AttributeValue::NumberArray(items) => {
            let entries = items
                .iter()
                .map(|n| js_number_string(*n))
                .collect::<Vec<_>>();
            format!("[{}]", entries.join(","))
        }
        AttributeValue::BoolArray(items) => {
            let entries = items.iter().map(bool::to_string).collect::<Vec<_>>();
            format!("[{}]", entries.join(","))
        }
    }
}

/// `formatAttributes` (`render.ts:17-21`).
fn format_attributes(attributes: &SpanAttributes) -> String {
    attributes
        .iter()
        .map(|(key, value)| format!(" {}={}", escaped(key), json_stringify(value)))
        .collect()
}

/// Strict-equality match for query attributes (upstream `===`,
/// `memory.ts:422` / `:447`).
#[allow(clippy::float_cmp)] // exact equality, as in upstream
fn attribute_matches(actual: &AttributeValue, expected: &QueryValue) -> bool {
    match (actual, expected) {
        (AttributeValue::String(left), QueryValue::String(right)) => left == right,
        (AttributeValue::Number(left), QueryValue::Number(right)) => left == right,
        (AttributeValue::Bool(left), QueryValue::Bool(right)) => left == right,
        _ => false,
    }
}

/// `JSON.stringify([startedMs, traceId])` (`memory.ts:435`).
fn encode_cursor(started_ms: f64, trace_id: &str) -> String {
    format!(
        "[{},{}]",
        js_number_string(started_ms),
        json_quote(trace_id)
    )
}

/// Parse the opaque cursor produced by [`encode_cursor`]. Upstream's
/// `JSON.parse` throws on malformed input; the port treats it as no cursor.
fn decode_cursor(cursor: &str) -> Option<(f64, String)> {
    let value: serde_json::Value = serde_json::from_str(cursor).ok()?;
    let pair = value.as_array()?;
    let started_ms = pair.first()?.as_f64()?;
    let trace_id = pair.get(1)?.as_str()?.to_string();
    Some((started_ms, trace_id))
}

/// `renderTrace` (`render.ts:28-72`): spans as a chronological forest, with
/// absent parents treated as roots. Ids, names, keys and error messages are
/// escaped; attribute values are JSON.
pub fn render_trace(spans: &[StoredSpan]) -> String {
    // `new Map(spans.map(...))` (`:29`): a duplicate id replaces the value
    // but keeps its first insertion position.
    let mut by_id: Vec<StoredSpan> = Vec::new();
    let mut positions: HashMap<&str, usize> = HashMap::new();
    for span in spans {
        if let Some(&index) = positions.get(span.id.as_str()) {
            by_id[index] = span.clone();
        } else {
            positions.insert(span.id.as_str(), by_id.len());
            by_id.push(span.clone());
        }
    }
    let ids: HashSet<&str> = positions.keys().copied().collect();
    by_id.sort_by(|left, right| left.started_ms.total_cmp(&right.started_ms)); // :32

    let mut roots: Vec<StoredSpan> = Vec::new();
    let mut children: HashMap<String, Vec<StoredSpan>> = HashMap::new();
    for span in by_id {
        match &span.parent_id {
            None => roots.push(span),
            Some(parent) if !ids.contains(parent.as_str()) => roots.push(span),
            Some(parent) => children.entry(parent.clone()).or_default().push(span),
        }
    }

    let mut pending: Vec<(StoredSpan, usize)> =
        roots.into_iter().rev().map(|span| (span, 0)).collect(); // :43
    let mut lines: Vec<String> = Vec::new();
    while let Some((span, depth)) = pending.pop() {
        let indent = "  ".repeat(depth); // :46
        let mut error = String::new();
        match &span.status {
            SpanStatus::Ok => {}
            SpanStatus::Error { error: None } => error.push_str(" error"), // :49
            SpanStatus::Error {
                error: Some(detail),
            } => {
                error.push_str(" error ");
                error.push_str(&escaped(&detail.name));
                error.push_str(": ");
                error.push_str(&escaped(&detail.message));
            }
        }
        lines.push(format!(
            "{}{}{} ({}ms){}",
            indent,
            escaped(&span.name),
            format_attributes(&span.attributes),
            js_micros(span.ended_ms - span.started_ms),
            error
        )); // :54-56
        for event in &span.events {
            lines.push(format!(
                "{}  · {}{} (+{}ms)",
                indent,
                escaped(&event.name),
                format_attributes(&event.attributes),
                js_micros(event.time_ms)
            )); // :57-61
        }
        for link in &span.links {
            lines.push(format!(
                "{}  → link trace {} span {}{}",
                indent,
                escaped(&link.trace_id),
                escaped(&link.span_id),
                format_attributes(&link.attributes)
            )); // :62-65
        }
        if let Some(descendants) = children.get(&span.id) {
            for child in descendants.iter().rev() {
                pending.push((child.clone(), depth + 1)); // :66-69
            }
        }
    }
    lines.join("\n") // :71
}

// -- in-memory implementation (memory.ts:63-494) --------------------------

#[derive(Clone, Debug, PartialEq)]
struct MutableEvent {
    time_ms: f64,
    name: String,
    attributes: SpanAttributes,
}

#[derive(Clone, Debug, PartialEq)]
struct RecordedLink {
    carrier: TelemetryCarrier,
    attributes: SpanAttributes,
}

struct MutableSpan {
    started_ms: f64,
    clock: Instant,
    ended_ms: Option<f64>,
    id: u64,
    parent_id: Option<u64>,
    carrier: TelemetryCarrier,
    parent_carrier: Option<TelemetryCarrier>,
    links: Vec<RecordedLink>,
    name: String,
    attributes: SpanAttributes,
    events: Vec<MutableEvent>,
    status: SpanStatus,
    explicit_status: bool,
    settled: bool,
    end_sequence: Option<u64>,
}

struct InMemoryState {
    scope: String,
    spans: Vec<Arc<Mutex<MutableSpan>>>,
    measurements: Vec<MeasurementRecord>,
    next_span_id: u64,
    next_end_sequence: u64,
}

/// `copyLink` (`memory.ts:149-152`): an empty carrier stands in for an
/// unknown context (upstream's missing `WeakMap` entry).
fn copy_link(link: &SpanLink) -> Option<RecordedLink> {
    if link.context.is_empty() {
        return None;
    }
    Some(RecordedLink {
        carrier: link.context.clone(),
        attributes: link.attributes.clone(),
    })
}

/// `createSpan` (`memory.ts:161-193`). Caller holds no other lock.
fn create_span(
    state: &Arc<Mutex<InMemoryState>>,
    parent: Option<&ParentRef>,
    options: SpanOptions,
) -> Arc<Mutex<MutableSpan>> {
    let mut recorder = state.lock().unwrap_or_else(PoisonError::into_inner);
    let id = recorder.next_span_id;
    recorder.next_span_id += 1;
    let trace_id = parent
        .and_then(|p| p.carrier.get(TRACE_KEY).cloned())
        .unwrap_or_else(|| format!("{}:{id}", recorder.scope));
    let carrier = TelemetryCarrier::from([
        (SCOPE_KEY.to_string(), recorder.scope.clone()),
        (SPAN_KEY.to_string(), id.to_string()),
        (TRACE_KEY.to_string(), trace_id),
    ]);
    let span = MutableSpan {
        started_ms: telemetry_now(), // :175
        clock: Instant::now(),
        ended_ms: None,
        id,
        parent_id: parent.and_then(|p| p.local_id), // :178
        carrier: carrier.clone(),
        parent_carrier: parent.map(|p| p.carrier.clone()), // :184
        links: options.links.iter().filter_map(copy_link).collect(),
        name: options.name,
        attributes: options.attributes,
        events: Vec::new(),
        status: SpanStatus::Ok,
        explicit_status: false,
        settled: false,
        end_sequence: None,
    };
    let stored = Arc::new(Mutex::new(span));
    recorder.spans.push(Arc::clone(&stored));
    stored
}

/// `startInertSpan` (`memory.ts:195-211`): a settled parent starts a noop
/// span so late children never extend the recorded trace.
fn start_inert_span(callback: &mut dyn FnMut(&dyn TelemetrySpan)) {
    let span = NoopSpan;
    callback(&span);
}

/// `startInMemorySpan` (`memory.ts:213-277`).
fn start_in_memory_span(
    state: &Arc<Mutex<InMemoryState>>,
    parent: Option<&ParentRef>,
    options: SpanOptions,
    callback: &mut dyn FnMut(&dyn TelemetrySpan),
) {
    // `parent?.span?.settled` (`memory.ts:219`): only a live in-process span
    // handle carries `span`; an extracted carrier does not, so it never
    // triggers the inert path.
    let parent_settled = parent.is_some_and(|p| {
        p.span
            .as_ref()
            .is_some_and(|arc| arc.lock().unwrap_or_else(PoisonError::into_inner).settled)
    });
    if parent_settled {
        start_inert_span(callback); // :219
        return;
    }

    let recorded = create_span(state, parent, options);
    let carrier = recorded
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .carrier
        .clone();
    let handle = MemorySpan {
        state: Arc::clone(state),
        inner: Arc::clone(&recorded),
        carrier,
    };

    // `try`/`catch`/`finally` (`memory.ts:263-276`): a panicking callback
    // leaves an errored status (upstream's thrown Error), and every path
    // settles the span before the panic is resumed.
    let outcome = catch_unwind(AssertUnwindSafe(|| callback(&handle)));
    {
        // Lock order is always state → span (also used by `stored_spans`).
        let mut recorder = state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut span = recorded.lock().unwrap_or_else(PoisonError::into_inner);
        if outcome.is_err() && !span.explicit_status {
            span.status = SpanStatus::Error { error: None }; // :266-270
        }
        let elapsed = span.clock.elapsed().as_secs_f64() * 1000.0;
        span.ended_ms = Some(js_micros(span.started_ms + elapsed)); // :274
        span.settled = true;
        span.end_sequence = Some(recorder.next_end_sequence);
        recorder.next_end_sequence += 1; // :275
    }
    if let Err(payload) = outcome {
        resume_unwind(payload);
    }
}

/// The span handle created by `startInMemorySpan` (`memory.ts:224-259`).
struct MemorySpan {
    state: Arc<Mutex<InMemoryState>>,
    inner: Arc<Mutex<MutableSpan>>,
    carrier: TelemetryCarrier,
}

impl TelemetrySpan for MemorySpan {
    fn add_event(&self, name: &str, attributes: SpanAttributes) {
        let mut span = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if span.settled {
            return;
        }
        let elapsed = span.clock.elapsed().as_secs_f64() * 1000.0;
        span.events.push(MutableEvent {
            name: name.to_string(),
            time_ms: js_micros(elapsed), // :246
            attributes,
        });
    }

    fn add_link(&self, link: SpanLink) {
        let mut span = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if span.settled {
            return;
        }
        if let Some(copy) = copy_link(&link) {
            span.links.push(copy); // :240
        }
    }

    fn set_attributes(&self, attributes: SpanAttributes) {
        let mut span = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if span.settled {
            return;
        }
        span.attributes.extend(attributes); // :252
    }

    fn set_status(&self, status: SpanStatus) {
        let mut span = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if span.settled {
            return;
        }
        span.status = copy_status(&status);
        span.explicit_status = true; // :257
    }

    fn start_span(&self, options: SpanOptions, callback: &mut dyn FnMut(&dyn TelemetrySpan)) {
        let id = self.inner.lock().unwrap_or_else(PoisonError::into_inner).id;
        let parent = ParentRef {
            local_id: Some(id),
            carrier: self.carrier.clone(),
            span: Some(Arc::clone(&self.inner)),
        };
        start_in_memory_span(&self.state, Some(&parent), options, callback); // :227-236
    }

    fn carrier(&self) -> Option<TelemetryCarrier> {
        Some(self.carrier.clone())
    }
}

/// `createMeasurementRecorder` (`memory.ts:279-307`).
fn record_measurement(
    state: &Arc<Mutex<InMemoryState>>,
    kind: MetricKind,
    name: String,
    description: Option<String>,
    unit: Option<String>,
    boundaries: Option<Vec<f64>>,
    value: f64,
    attributes: SpanAttributes,
    context: Option<TelemetryCarrier>,
) {
    let record = MeasurementRecord {
        interval_ms: (wall_clock_ms() / 10_000.0).floor() * 10_000.0, // :298
        measurement: RecordedTelemetryMeasurement {
            kind,
            name,
            description,
            unit,
            boundaries,
            value,
            attributes,
            context,
        },
    };
    let mut recorder = state.lock().unwrap_or_else(PoisonError::into_inner);
    recorder.measurements.push(record);
}

/// One instrument: `counter`, `upDownCounter` and `histogram` share the same
/// recorder (`memory.ts:356-366`). Rust lets one type implement all three
/// instrument traits.
struct MemoryInstrument {
    state: Arc<Mutex<InMemoryState>>,
    kind: MetricKind,
    name: String,
    description: Option<String>,
    unit: Option<String>,
    boundaries: Option<Vec<f64>>,
}

impl MemoryInstrument {
    fn record(&self, value: f64, attributes: SpanAttributes, context: Option<TelemetryCarrier>) {
        record_measurement(
            &self.state,
            self.kind,
            self.name.clone(),
            self.description.clone(),
            self.unit.clone(),
            self.boundaries.clone(),
            value,
            attributes,
            context,
        );
    }
}

impl Counter for MemoryInstrument {
    fn add(&self, value: f64, attributes: SpanAttributes) {
        self.record(value, attributes, None);
    }
}

impl UpDownCounter for MemoryInstrument {
    fn add(&self, value: f64, attributes: SpanAttributes) {
        self.record(value, attributes, None);
    }
}

impl Histogram for MemoryInstrument {
    fn record(&self, value: f64, attributes: SpanAttributes, context: Option<TelemetryCarrier>) {
        self.record(value, attributes, context);
    }
}

/// `InMemoryTelemetryContext` (`memory.ts:313-494`), renamed to fit the
/// single-host-contract port. A fresh instance is an isolated recording
/// scope (`:309-312`).
pub struct InMemoryTelemetry {
    state: Arc<Mutex<InMemoryState>>,
}

impl Default for InMemoryTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryTelemetry {
    /// A fresh scope with `crypto.randomUUID()`'s identity (`memory.ts:321`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(InMemoryState {
                scope: uuid::Uuid::new_v4().to_string(),
                spans: Vec::new(),
                measurements: Vec::new(),
                next_span_id: 1,
                next_end_sequence: 1,
            })),
        }
    }
}

impl Telemetry for InMemoryTelemetry {
    fn counter(&self, name: &str, options: &InstrumentOptions) -> Box<dyn Counter> {
        Box::new(MemoryInstrument {
            state: Arc::clone(&self.state),
            kind: MetricKind::Counter,
            name: name.to_string(),
            description: options.description.clone(),
            unit: options.unit.clone(),
            boundaries: None,
        })
    }

    fn up_down_counter(&self, name: &str, options: &InstrumentOptions) -> Box<dyn UpDownCounter> {
        Box::new(MemoryInstrument {
            state: Arc::clone(&self.state),
            kind: MetricKind::UpDownCounter,
            name: name.to_string(),
            description: options.description.clone(),
            unit: options.unit.clone(),
            boundaries: None,
        })
    }

    fn histogram(&self, name: &str, options: &HistogramOptions) -> Box<dyn Histogram> {
        Box::new(MemoryInstrument {
            state: Arc::clone(&self.state),
            kind: MetricKind::Histogram,
            name: name.to_string(),
            description: options.description.clone(),
            unit: options.unit.clone(),
            // Boundaries only apply to histograms (`memory.ts:287`).
            boundaries: options.boundaries.clone(),
        })
    }

    fn start_span(&self, options: SpanOptions, callback: &mut dyn FnMut(&dyn TelemetrySpan)) {
        self.start_span_in(None, options, callback);
    }

    fn start_span_in(
        &self,
        parent: Option<&SpanContext>,
        options: SpanOptions,
        callback: &mut dyn FnMut(&dyn TelemetrySpan),
    ) {
        let carrier = parent.and_then(SpanContext::carrier).cloned();
        let parent_ref = carrier.map(|carrier| ParentRef {
            local_id: parent.and_then(|context| context.local_id),
            carrier,
            span: None, // extracted parents have no `span` (`memory.ts:346-351`)
        });
        start_in_memory_span(&self.state, parent_ref.as_ref(), options, callback);
    }

    fn inject(&self, context: &dyn TelemetrySpan) -> Option<TelemetryCarrier> {
        context.carrier() // `memory.ts:332-336`
    }

    fn extract(&self, carrier: Option<&TelemetryCarrier>) -> SpanContext {
        let Some(carrier) = carrier else {
            return SpanContext::root();
        };
        // All three keys must be present strings (`memory.ts:343`); the
        // BTreeMap already guarantees string values. `traceId` travels in
        // the carrier itself; `localId` is meaningful only in this scope
        // (`:345`).
        if ![SCOPE_KEY, SPAN_KEY, TRACE_KEY]
            .iter()
            .all(|key| carrier.contains_key(*key))
        {
            return SpanContext::root();
        }
        let scope = &carrier[SCOPE_KEY];
        let span = &carrier[SPAN_KEY];
        SpanContext {
            carrier: Some(carrier.clone()),
            local_id: self
                .scope_matches(scope)
                .then(|| span.parse::<u64>().ok())
                .flatten(),
        }
    }

    fn get_spans(&self) -> Vec<RecordedTelemetrySpan> {
        let recorder = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        recorder
            .spans
            .iter()
            .map(|arc| {
                let span = arc.lock().unwrap_or_else(PoisonError::into_inner);
                RecordedTelemetrySpan {
                    id: span.id,
                    parent_id: span.parent_id,
                    context: span.carrier.clone(),
                    parent_context: span.parent_carrier.clone(),
                    links: span
                        .links
                        .iter()
                        .map(|link| RecordedTelemetryLink {
                            context: link.carrier.clone(),
                            attributes: link.attributes.clone(),
                        })
                        .collect(),
                    name: span.name.clone(),
                    attributes: span.attributes.clone(),
                    events: span
                        .events
                        .iter()
                        .map(|event| RecordedTelemetryEvent {
                            name: event.name.clone(),
                            attributes: event.attributes.clone(),
                        })
                        .collect(),
                    status: copy_status(&span.status),
                    settled: span.settled,
                    end_sequence: span.end_sequence,
                }
            })
            .collect()
    }

    fn get_metrics(&self) -> Vec<RecordedTelemetryMeasurement> {
        let recorder = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        recorder
            .measurements
            .iter()
            .map(|record| record.measurement.clone())
            .collect()
    }

    fn stored_spans(&self) -> Vec<StoredSpan> {
        let recorder = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut spans: Vec<StoredSpan> = recorder
            .spans
            .iter()
            .filter_map(|arc| {
                let span = arc.lock().unwrap_or_else(PoisonError::into_inner);
                if !span.settled {
                    return None;
                }
                let ended_ms = span.ended_ms.unwrap_or(span.started_ms);
                Some(StoredSpan {
                    id: carrier_id(&span.carrier),
                    trace_id: span.carrier.get(TRACE_KEY).cloned().unwrap_or_default(),
                    parent_id: span.parent_carrier.as_ref().map(carrier_id),
                    name: span.name.clone(),
                    started_ms: span.started_ms,
                    ended_ms,
                    attributes: span.attributes.clone(),
                    events: span
                        .events
                        .iter()
                        .map(|event| StoredEvent {
                            name: event.name.clone(),
                            time_ms: event.time_ms,
                            attributes: event.attributes.clone(),
                        })
                        .collect(),
                    links: span
                        .links
                        .iter()
                        .map(|link| StoredLink {
                            trace_id: link.carrier.get(TRACE_KEY).cloned().unwrap_or_default(),
                            span_id: carrier_id(&link.carrier),
                            attributes: link.attributes.clone(),
                        })
                        .collect(),
                    status: copy_status(&span.status),
                })
            })
            .collect();
        spans.sort_by(|left, right| {
            left.started_ms
                .total_cmp(&right.started_ms)
                .then_with(|| left.id.cmp(&right.id))
        }); // :391
        spans
    }

    fn find_metrics(&self, query: &MetricQuery) -> Vec<StoredMetric> {
        let recorder = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        recorder
            .measurements
            .iter()
            .filter(|record| {
                let measurement = &record.measurement;
                measurement.name == query.name
                    && query.start_min.is_none_or(|min| record.interval_ms >= min)
                    && query.start_max.is_none_or(|max| record.interval_ms <= max)
                    && query.attributes.iter().all(|(key, value)| {
                        measurement
                            .attributes
                            .get(key)
                            .is_some_and(|actual| attribute_matches(actual, value))
                    })
            })
            .map(|record| StoredMetric {
                interval_ms: record.interval_ms,
                name: record.measurement.name.clone(),
                kind: record.measurement.kind,
                unit: record.measurement.unit.clone(),
                attributes: record.measurement.attributes.clone(),
                count: 1,
                sum: record.measurement.value,
                min: record.measurement.value,
                max: record.measurement.value,
            })
            .collect() // :449-459
    }

    fn get_trace(&self, trace_id: &str) -> Option<StoredTrace> {
        let spans: Vec<StoredSpan> = self
            .stored_spans()
            .into_iter()
            .filter(|span| span.trace_id == trace_id)
            .collect();
        (!spans.is_empty()).then_some(StoredTrace {
            trace_id: trace_id.to_string(),
            spans,
        }) // :394-397
    }

    fn find_traces(&self, query: &TraceQuery) -> TracePage {
        let limit = query.limit.unwrap_or(20); // :400
        if limit == 0 {
            // Upstream slices to zero and then indexes `at(-1)` (`:432`);
            // the port returns an empty page instead of panicking.
            return TracePage::default();
        }
        let after = query.cursor.as_deref().and_then(decode_cursor); // :401

        // Group settled spans by trace id, preserving first-seen order
        // (`memory.ts:402-410`).
        let mut grouped: Vec<StoredTrace> = Vec::new();
        let mut positions: HashMap<String, usize> = HashMap::new();
        for span in self.stored_spans() {
            if let Some(&index) = positions.get(&span.trace_id) {
                grouped[index].spans.push(span);
            } else {
                positions.insert(span.trace_id.clone(), grouped.len());
                grouped.push(StoredTrace {
                    trace_id: span.trace_id.clone(),
                    spans: vec![span],
                });
            }
        }

        let mut matches: Vec<StoredTrace> = grouped
            .into_iter()
            .filter(|trace| {
                let Some(first) = trace.spans.first() else {
                    return false;
                };
                if query.name.as_ref().is_some_and(|name| first.name != *name) {
                    return false;
                }
                if query.start_min.is_some_and(|min| first.started_ms < min) {
                    return false;
                }
                if query.start_max.is_some_and(|max| first.started_ms > max) {
                    return false;
                }
                if let Some((after_start, after_id)) = &after
                    && !(first.started_ms < *after_start
                        || (first.started_ms == *after_start && trace.trace_id > *after_id))
                {
                    return false;
                }
                if query.errors
                    && !trace
                        .spans
                        .iter()
                        .any(|span| matches!(span.status, SpanStatus::Error { .. }))
                {
                    return false;
                }
                if let Some(attributes) = &query.attributes
                    && !trace.spans.iter().any(|span| {
                        attributes.iter().all(|(key, value)| {
                            span.attributes
                                .get(key)
                                .is_some_and(|actual| attribute_matches(actual, value))
                        })
                    })
                {
                    return false;
                }
                true
            })
            .collect();
        matches.sort_by(|left, right| {
            let left_start = left.spans.first().map_or(0.0, |span| span.started_ms);
            let right_start = right.spans.first().map_or(0.0, |span| span.started_ms);
            right_start
                .total_cmp(&left_start)
                .then_with(|| left.trace_id.cmp(&right.trace_id))
        }); // :426-430
        let traces: Vec<StoredTrace> = matches.iter().take(limit).cloned().collect();
        let cursor = if matches.len() > limit {
            let last = traces.last().expect("limit is nonzero");
            let first = last.spans.first().expect("a trace has a first span");
            Some(encode_cursor(first.started_ms, &last.trace_id)) // :435
        } else {
            None
        };
        TracePage { traces, cursor }
    }
}

impl InMemoryTelemetry {
    /// `scope_matches` helper used by `extract` (`memory.ts:345`).
    fn scope_matches(&self, scope: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .scope
            == scope
    }
}

// -- host-facing constructor (core/telemetry.ts:1-13) ----------------------

/// `isTruthyEnvFlag` (`core/telemetry.ts:3-6`): only `1`, `true` and `yes`
/// (case-insensitive) are truthy; empty and absent are false.
pub fn is_truthy_env_flag(value: Option<&str>) -> bool {
    value.is_some_and(|text| {
        text == "1" || text.eq_ignore_ascii_case("true") || text.eq_ignore_ascii_case("yes")
    })
}

/// `isInstallTelemetryEnabled` (`core/telemetry.ts:8-13`): the env var, when
/// set at all, overrides the persisted setting.
pub fn is_install_telemetry_enabled(settings_enabled: bool, telemetry_env: Option<&str>) -> bool {
    telemetry_env.map_or(settings_enabled, |value| is_truthy_env_flag(Some(value)))
}

/// [`is_install_telemetry_enabled`] with upstream's default
/// `process.env.PI_TELEMETRY` parameter (`core/telemetry.ts:10`).
pub fn is_install_telemetry_enabled_from_env(settings_enabled: bool) -> bool {
    let env = std::env::var(ENV_TELEMETRY).ok();
    is_install_telemetry_enabled(settings_enabled, env.as_deref())
}

/// Host-facing constructor: a noop recorder when telemetry is disabled and
/// the in-memory recorder when it is enabled. The omitted install ping and
/// attribution are documented in `WIRING.md`.
pub fn construct_telemetry(enabled: bool) -> Arc<dyn Telemetry> {
    if enabled {
        Arc::new(InMemoryTelemetry::new())
    } else {
        Arc::new(NoopTelemetry)
    }
}

/// Compose the setting + `PI_TELEMETRY` gate with [`construct_telemetry`].
pub fn telemetry_for_settings(settings_enabled: bool) -> Arc<dyn Telemetry> {
    construct_telemetry(is_install_telemetry_enabled_from_env(settings_enabled))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(pairs: &[(&str, AttributeValue)]) -> SpanAttributes {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect()
    }

    fn stored(
        id: &str,
        name: &str,
        started_ms: f64,
        ended_ms: f64,
        parent_id: Option<&str>,
    ) -> StoredSpan {
        StoredSpan {
            id: id.to_string(),
            trace_id: "trace".to_string(),
            parent_id: parent_id.map(str::to_string),
            name: name.to_string(),
            started_ms,
            ended_ms,
            attributes: SpanAttributes::new(),
            events: Vec::new(),
            links: Vec::new(),
            status: SpanStatus::Ok,
        }
    }

    /// `isTruthyEnvFlag` and `isInstallTelemetryEnabled`
    /// (`core/telemetry.ts:3-13`).
    #[test]
    fn env_flag_matches_upstream_truthy_rules() {
        assert!(!is_truthy_env_flag(None));
        assert!(!is_truthy_env_flag(Some("")));
        assert!(is_truthy_env_flag(Some("1")));
        assert!(is_truthy_env_flag(Some("true")));
        assert!(is_truthy_env_flag(Some("TRUE")));
        assert!(is_truthy_env_flag(Some("YeS")));
        assert!(!is_truthy_env_flag(Some("0")));
        assert!(!is_truthy_env_flag(Some("false")));
        assert!(!is_truthy_env_flag(Some("no")));
        assert!(!is_truthy_env_flag(Some("2")));

        // An env override wins in both directions; absence falls back.
        assert!(is_install_telemetry_enabled(false, Some("true")));
        assert!(!is_install_telemetry_enabled(true, Some("0")));
        assert!(is_install_telemetry_enabled(true, None));
        assert!(!is_install_telemetry_enabled(false, None));
    }

    /// `noop.ts:8-37`: instruments, spans and propagation retain nothing.
    #[test]
    fn noop_drops_everything_but_runs_callbacks() {
        let telemetry = NoopTelemetry;
        telemetry
            .counter("tokens", &InstrumentOptions::default())
            .add(5.0, attrs(&[("model", AttributeValue::String("x".into()))]));
        telemetry
            .up_down_counter("queue", &InstrumentOptions::default())
            .add(-1.0, SpanAttributes::new());
        telemetry
            .histogram("latency", &HistogramOptions::default())
            .record(2.5, SpanAttributes::new(), None);

        let mut ran = false;
        telemetry.start_span(SpanOptions::new("outer"), &mut |span| {
            ran = true;
            span.add_event("event", SpanAttributes::new());
            span.set_attributes(attrs(&[("k", AttributeValue::Bool(true))]));
            span.set_status(SpanStatus::Error { error: None });
            let mut child_ran = false;
            span.start_span(SpanOptions::new("inner"), &mut |child| {
                child_ran = true;
                child.add_event("nested", SpanAttributes::new());
            });
            assert!(child_ran);
        });
        assert!(ran);

        assert_eq!(telemetry.get_metrics().len(), 0);
        assert_eq!(telemetry.get_spans().len(), 0);
        assert_eq!(telemetry.stored_spans().len(), 0);
        assert_eq!(telemetry.find_metrics(&MetricQuery::default()).len(), 0);
        assert_eq!(
            telemetry.find_traces(&TraceQuery::default()),
            TracePage::default()
        );
        assert!(telemetry.get_trace("any").is_none());

        let noop_span = NoopSpan;
        assert!(telemetry.inject(&noop_span).is_none());
        assert!(
            telemetry
                .extract(Some(&TelemetryCarrier::new()))
                .carrier()
                .is_none()
        );
    }

    /// `metrics.ts:20-41` and `memory.ts:279-307`: all three instruments
    /// record detached measurements with their metadata.
    #[test]
    fn instruments_record_measurements_with_metadata() {
        let telemetry = InMemoryTelemetry::new();
        let counter = telemetry.counter(
            "tokens",
            &InstrumentOptions {
                description: Some("input".to_string()),
                unit: Some("tok".to_string()),
            },
        );
        counter.add(3.0, attrs(&[("model", AttributeValue::String("x".into()))]));
        counter.add(4.0, SpanAttributes::new());
        telemetry
            .up_down_counter("queue", &InstrumentOptions::default())
            .add(-1.0, SpanAttributes::new());
        telemetry
            .histogram(
                "latency",
                &HistogramOptions {
                    unit: Some("ms".to_string()),
                    boundaries: Some(vec![1.0, 10.0]),
                    ..HistogramOptions::default()
                },
            )
            .record(2.5, SpanAttributes::new(), None);

        let recorded = telemetry.get_metrics();
        assert_eq!(recorded.len(), 4);
        assert_eq!(recorded[0].kind, MetricKind::Counter);
        assert_eq!(recorded[0].name, "tokens");
        assert_eq!(recorded[0].value, 3.0);
        assert_eq!(recorded[0].description.as_deref(), Some("input"));
        assert_eq!(recorded[0].unit.as_deref(), Some("tok"));
        assert_eq!(recorded[1].value, 4.0);
        assert_eq!(recorded[2].kind, MetricKind::UpDownCounter);
        assert_eq!(recorded[2].value, -1.0);
        assert_eq!(recorded[3].kind, MetricKind::Histogram);
        assert_eq!(recorded[3].boundaries, Some(vec![1.0, 10.0]));
    }

    /// `memory.ts:213-277`: events, attributes, status, children and
    /// settlement all accumulate in the owning recorder.
    #[test]
    fn spans_accumulate_events_attributes_and_status() {
        let telemetry = InMemoryTelemetry::new();
        telemetry.start_span(SpanOptions::new("outer"), &mut |span| {
            span.set_attributes(attrs(&[("route", AttributeValue::String("/".into()))]));
            span.add_event("start", SpanAttributes::new());
            span.start_span(SpanOptions::new("inner"), &mut |child| {
                child.set_status(SpanStatus::Error {
                    error: Some(SpanError {
                        name: " boom ".to_string(),
                        message: "failed".to_string(),
                    }),
                });
                child.add_link(SpanLink {
                    context: TelemetryCarrier::from([
                        (SCOPE_KEY.to_string(), "external".to_string()),
                        (SPAN_KEY.to_string(), "9".to_string()),
                        (TRACE_KEY.to_string(), "external:9".to_string()),
                    ]),
                    attributes: SpanAttributes::new(),
                });
            });
        });

        let recorded = telemetry.get_spans();
        assert_eq!(recorded.len(), 2);
        assert!(recorded[0].settled);
        assert_eq!(recorded[0].name, "outer");
        assert_eq!(recorded[0].events.len(), 1);
        assert_eq!(
            recorded[0].attributes.get("route"),
            Some(&AttributeValue::String("/".into()))
        );
        assert_eq!(recorded[1].name, "inner");
        assert_eq!(recorded[1].parent_id, Some(recorded[0].id));
        assert_eq!(recorded[1].links.len(), 1);
        assert!(matches!(
            recorded[1].status,
            SpanStatus::Error { error: Some(_) }
        ));
        assert_eq!(recorded[0].end_sequence, Some(2));
        assert_eq!(recorded[1].end_sequence, Some(1));

        let stored = telemetry.stored_spans();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[1].parent_id, Some(stored[0].id.clone()));
        assert_eq!(stored[1].links[0].span_id, "external:9");
        let trace = telemetry.get_trace(&stored[0].trace_id).expect("trace");
        assert_eq!(trace.spans.len(), 2);
    }

    /// `memory.ts:332-354`: inject/extract preserve the carrier and external
    /// parents keep their trace id.
    #[test]
    fn extract_inject_round_trip() {
        let telemetry = InMemoryTelemetry::new();
        let mut carrier = None;
        telemetry.start_span(SpanOptions::new("root"), &mut |span| {
            carrier = telemetry.inject(span);
        });
        let carrier = carrier.expect("carrier of a recorded span");
        assert_eq!(carrier.get(SPAN_KEY).map(String::as_str), Some("1"));

        let context = telemetry.extract(Some(&carrier));
        assert_eq!(context.carrier(), Some(&carrier));
        telemetry.start_span_in(Some(&context), SpanOptions::new("child"), &mut |_| {});
        let stored = telemetry.stored_spans();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[1].trace_id, stored[0].trace_id);
        assert_eq!(stored[1].parent_id, Some(stored[0].id.clone()));

        // A malformed carrier and an absent one both mean the root.
        let mut bad = TelemetryCarrier::new();
        bad.insert(TRACE_KEY.to_string(), "orphan".to_string());
        assert!(telemetry.extract(Some(&bad)).carrier().is_none());
        assert!(telemetry.extract(None).carrier().is_none());
    }

    /// `memory.ts:339-354`, `:219`: an extracted parent has no `span` field,
    /// so a child started from it is recorded even though the original span
    /// has settled. Only a live child call takes the inert path.
    #[test]
    fn extracted_parent_creates_a_real_child_after_settlement() {
        let telemetry = InMemoryTelemetry::new();
        let mut late_carrier = None;
        telemetry.start_span(SpanOptions::new("first"), &mut |span| {
            late_carrier = telemetry.inject(span);
        });
        let context = telemetry.extract(Some(&late_carrier.expect("carrier")));
        let mut ran = false;
        telemetry.start_span_in(Some(&context), SpanOptions::new("late"), &mut |span| {
            ran = true;
            span.add_event("recorded", SpanAttributes::new());
            assert!(span.carrier().is_some());
        });
        assert!(ran);
        let recorded = telemetry.get_spans();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[1].name, "late");
        assert_eq!(recorded[1].parent_id, Some(recorded[0].id));
        assert_eq!(recorded[1].events.len(), 1);
    }

    /// `memory.ts:439-460`: findMetrics filters on name, interval bounds and
    /// attributes and returns count-one rows.
    #[test]
    fn find_metrics_filters_and_aggregates() {
        let telemetry = InMemoryTelemetry::new();
        let counter = telemetry.counter("tokens", &InstrumentOptions::default());
        counter.add(2.0, attrs(&[("model", AttributeValue::String("a".into()))]));
        counter.add(5.0, attrs(&[("model", AttributeValue::String("b".into()))]));
        telemetry
            .counter("other", &InstrumentOptions::default())
            .add(9.0, SpanAttributes::new());

        let rows = telemetry.find_metrics(&MetricQuery {
            name: "tokens".to_string(),
            attributes: attrs(&[("model", AttributeValue::String("a".into()))]),
            ..MetricQuery::default()
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].count, 1);
        assert_eq!(rows[0].sum, 2.0);
        assert_eq!(rows[0].min, 2.0);
        assert_eq!(rows[0].max, 2.0);

        let missing = telemetry.find_metrics(&MetricQuery {
            name: "tokens".to_string(),
            attributes: attrs(&[("model", AttributeValue::Number(1.0))]),
            ..MetricQuery::default()
        });
        assert_eq!(missing.len(), 0);
    }

    /// `memory.ts:399-437`: findTraces paginates newest-first with an opaque
    /// continuation cursor.
    #[test]
    fn find_traces_paginates_with_a_cursor() {
        let telemetry = InMemoryTelemetry::new();
        telemetry.start_span(SpanOptions::new("first"), &mut |_| {});
        std::thread::sleep(std::time::Duration::from_millis(2));
        telemetry.start_span(SpanOptions::new("second"), &mut |_| {});

        let page = telemetry.find_traces(&TraceQuery {
            limit: Some(1),
            ..TraceQuery::default()
        });
        assert_eq!(page.traces.len(), 1);
        assert_eq!(page.traces[0].spans[0].name, "second");
        let cursor = page.cursor.expect("continuation cursor");

        let next = telemetry.find_traces(&TraceQuery {
            limit: Some(1),
            cursor: Some(cursor),
            ..TraceQuery::default()
        });
        assert_eq!(next.traces.len(), 1);
        assert_eq!(next.traces[0].spans[0].name, "first");
        assert!(next.cursor.is_none());
    }

    /// `render.ts:5-15`, `:19`, `:17-21`: escaping, rounding and JSON output.
    #[test]
    fn escaping_rounding_and_json_are_verbatim() {
        assert_eq!(escaped("a\u{0}\u{9f}z"), "a\\u0000\\u009fz");
        assert_eq!(escaped("plain"), "plain");
        assert_eq!(js_micros(1.2345).to_string(), "1.235");
        assert_eq!(js_micros(1.2344).to_string(), "1.234");
        assert_eq!(js_number_string(-0.0), "0");
        assert_eq!(js_number_string(5.0), "5");
        assert_eq!(js_number_string(f64::NAN), "null");
        assert_eq!(json_quote("a\"\n\t"), "\"a\\\"\\n\\t\"");
        assert_eq!(
            format_attributes(&attrs(&[("k", AttributeValue::Number(-0.0))])),
            " k=0"
        );
    }

    /// `render.ts:28-72`: the full forest — nesting, events, links, error
    /// detail and control characters — rendered byte-for-byte.
    #[test]
    fn render_trace_matches_upstream_output() {
        let root = stored("scope:1", "root", 0.0, 2.5, None);
        let child = stored("scope:2", "child", 0.5, 1.5, Some("scope:1"));
        let mut detailed = stored("scope:3", "with events", 1.0, 2.0, Some("scope:1"));
        detailed.attributes = attrs(&[("k", AttributeValue::String("v\n".into()))]);
        detailed.events.push(StoredEvent {
            name: "e\u{7f}".to_string(),
            time_ms: 0.25,
            attributes: SpanAttributes::new(),
        });
        detailed.links.push(StoredLink {
            trace_id: "t2".to_string(),
            span_id: "s\u{1}".to_string(),
            attributes: SpanAttributes::new(),
        });
        detailed.status = SpanStatus::Error {
            error: Some(SpanError {
                name: "Err".to_string(),
                message: "bad\u{1f}".to_string(),
            }),
        };
        let orphan = stored("scope:4", "orphan", 3.0, 3.5, Some("missing"));

        assert_eq!(
            render_trace(&[root, child, detailed, orphan]),
            "root (2.5ms)\n  child (1ms)\n  with events k=\"v\\n\" (1ms) error Err: bad\\u001f\n    · e\\u007f (+0.25ms)\n    → link trace t2 span s\\u0001\norphan (0.5ms)"
        );
    }

    /// `render.ts:49-53`: an error with no payload renders as bare " error".
    #[test]
    fn render_trace_renders_bare_error_status() {
        let mut span = stored("scope:1", "failed", 0.0, 1.0, None);
        span.status = SpanStatus::Error { error: None };
        assert_eq!(render_trace(&[span]), "failed (1ms) error");
    }

    /// `render.ts:29`: a duplicate id replaces the value but keeps the first
    /// insertion position (both are sorted by start time afterwards).
    #[test]
    fn render_trace_deduplicates_by_id() {
        let first = stored("scope:1", "old", 0.0, 1.0, None);
        let replacement = stored("scope:1", "new", 0.0, 2.0, None);
        assert_eq!(render_trace(&[first, replacement]), "new (2ms)");
    }

    /// `memory.ts:131-139`: all recorder state is behind a mutex, so the
    /// instruments and spans can be driven from several threads.
    #[test]
    fn concurrent_recording_is_thread_safe() {
        let telemetry = Arc::new(InMemoryTelemetry::new());
        let mut handles = Vec::new();
        for _ in 0..4 {
            let recorder = Arc::clone(&telemetry);
            handles.push(std::thread::spawn(move || {
                let counter = recorder.counter("n", &InstrumentOptions::default());
                for _ in 0..50 {
                    counter.add(1.0, SpanAttributes::new());
                }
                for index in 0..10 {
                    recorder.start_span(SpanOptions::new(format!("s{index}")), &mut |_| {});
                }
            }));
        }
        for handle in handles {
            handle.join().expect("recording thread");
        }
        assert_eq!(telemetry.get_metrics().len(), 200);
        assert_eq!(telemetry.get_spans().len(), 40);
        assert_eq!(telemetry.stored_spans().len(), 40);
    }
}
