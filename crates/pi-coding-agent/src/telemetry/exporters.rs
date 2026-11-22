//! Exporter surface built on the ported telemetry store.
//!
//! Ground truth (all line numbers refer to earendil-works/pi v1.5.2):
//!
//! * `packages/telemetry/src/index.ts:3-7` — `AttributeValue` and
//!   `SpanAttributes`, the value kinds the OTLP payload encoder mirrors.
//! * `packages/telemetry/src/index.ts:32-52` — `StoredSpan`, the settled span
//!   snapshot exported as an OTLP span (events `:45`, links `:46-50`, status
//!   `:51`).
//! * `packages/telemetry/src/index.ts:82-92` — `StoredMetric`, exported as an
//!   OTLP sum/histogram data point.
//! * `packages/telemetry/src/memory.ts:298` — the 10-second measurement
//!   interval reused as the OTLP data-point time window.
//! * `packages/coding-agent/src/modes/interactive/interactive-mode.ts:1472-1489`
//!   — the only upstream egress is the fire-and-forget install ping (ported in
//!   [`super::ping`]); v1.5.2 has no span/metric exporter trait and no OTLP
//!   client. The traits, batching and OTLP JSON encoder below are additive
//!   port surface, shaped around the ported `StoredSpan`/`StoredMetric`
//!   records. `WIRING.md` records the deviation and the host wiring.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{
    AttributeValue, MetricKind, RecordedTelemetryMeasurement, SpanAttributes, SpanStatus,
    StoredMetric, StoredSpan, Telemetry, telemetry_now,
};

#[cfg(test)]
use super::{SpanError, StoredEvent, StoredLink};

/// Lock helper matching the recorder's poison-tolerant convention
/// (`memory.ts:130-134`).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// -- privacy: attribute redaction -------------------------------------------

/// Attribute keys whose values are never exported. Upstream's install ping
/// sends only a version, so this list keeps that privacy property for the
/// additive OTLP surface as well.
const SENSITIVE_KEYS: &[&str] = &[
    "authorization",
    "api_key",
    "apikey",
    "token",
    "access_token",
    "refresh_token",
    "secret",
    "client_secret",
    "password",
    "passwd",
    "credential",
    "credentials",
    "cookie",
    "set_cookie",
    "private_key",
    "access_key",
    "session_id",
];

/// String prefixes that identify a credential regardless of its attribute
/// name (API keys, GitHub tokens, Slack tokens, bearer headers, JWTs and PEM
/// blocks).
const SENSITIVE_PREFIXES: &[&str] = &[
    "sk-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "AKIA",
    "xoxb-",
    "xoxp-",
    "Bearer ",
    "-----BEGIN ",
];

/// The replacement written into the payload for a secret-looking string.
pub const REDACTED: &str = "[REDACTED]";

/// Whether an attribute key names a credential (case-insensitive, also
/// matching `*_token`/`*_key`/`secret_*` style compounds).
fn is_sensitive_key(key: &str) -> bool {
    // Header-style keys use hyphens (`x-api-key`), attribute keys use
    // underscores (`api_key`); normalize both to the same form.
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    SENSITIVE_KEYS.iter().any(|needle| {
        normalized == *needle
            || normalized.ends_with(&format!("_{needle}"))
            || normalized.starts_with(&format!("{needle}_"))
    })
}

/// Whether a string value has a credential's shape.
fn looks_like_secret(text: &str) -> bool {
    let trimmed = text.trim_start();
    SENSITIVE_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
        || (trimmed.starts_with("eyJ") && trimmed.split('.').count() >= 3)
}

fn redact_string(text: &str) -> String {
    if looks_like_secret(text) {
        REDACTED.to_string()
    } else {
        text.to_string()
    }
}

fn redact_value(value: &AttributeValue) -> AttributeValue {
    match value {
        AttributeValue::String(text) => AttributeValue::String(redact_string(text)),
        AttributeValue::StringArray(items) => {
            AttributeValue::StringArray(items.iter().map(|item| redact_string(item)).collect())
        }
        other => other.clone(),
    }
}

/// Drops credential-named attributes and scrubs credential-shaped values
/// before either payload encoder sees them.
#[must_use]
pub fn redact_attributes(attributes: &SpanAttributes) -> SpanAttributes {
    attributes
        .iter()
        .filter(|(key, _)| !is_sensitive_key(key))
        .map(|(key, value)| (key.clone(), redact_value(value)))
        .collect()
}

// -- OTLP/HTTP JSON encoding -------------------------------------------------

const SERVICE_NAME: &str = "pi";
const SCOPE_NAME: &str = "pi-rs";
/// Histogram measurements are recorded over a 10-second interval
/// (`memory.ts:298`); the same window is used for every data point.
const INTERVAL_MS: f64 = 10_000.0;

/// FNV-1a 64-bit, used only to turn the recorder's local ids into stable
/// fixed-width OTLP ids (upstream has no W3C ids to export).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    })
}

/// A 16-hex-digit OTLP span id derived from the local span id.
#[must_use]
pub fn otlp_span_id(id: &str) -> String {
    let hash = fnv1a(id.as_bytes());
    format!("{hash:016x}")
}

/// A 32-hex-digit OTLP trace id derived from the local trace id.
#[must_use]
pub fn otlp_trace_id(id: &str) -> String {
    let first = fnv1a(id.as_bytes());
    let second = fnv1a(format!("{id}/trace").as_bytes());
    format!("{first:016x}{second:016x}")
}

fn nanos_string(milliseconds: f64) -> String {
    if !milliseconds.is_finite() || milliseconds <= 0.0 {
        return "0".to_string();
    }
    let nanos = (milliseconds * 1_000_000.0) as u128;
    format!("{nanos}")
}

fn otlp_value(value: &AttributeValue) -> Value {
    match value {
        AttributeValue::String(text) => json!({ "stringValue": text }),
        AttributeValue::Number(number) => json!({ "doubleValue": number }),
        AttributeValue::Bool(boolean) => json!({ "boolValue": boolean }),
        AttributeValue::StringArray(items) => json!({
            "arrayValue": {
                "values": items.iter().map(|item| json!({ "stringValue": item })).collect::<Vec<_>>(),
            },
        }),
        AttributeValue::NumberArray(items) => json!({
            "arrayValue": {
                "values": items.iter().map(|item| json!({ "doubleValue": item })).collect::<Vec<_>>(),
            },
        }),
        AttributeValue::BoolArray(items) => json!({
            "arrayValue": {
                "values": items.iter().map(|item| json!({ "boolValue": item })).collect::<Vec<_>>(),
            },
        }),
    }
}

fn otlp_attributes(attributes: &SpanAttributes) -> Vec<Value> {
    redact_attributes(attributes)
        .into_iter()
        .map(|(key, value)| json!({ "key": key, "value": otlp_value(&value) }))
        .collect()
}

fn resource() -> Value {
    json!({
        "attributes": [{
            "key": "service.name",
            "value": { "stringValue": SERVICE_NAME },
        }],
    })
}

fn status_json(status: &SpanStatus) -> Value {
    match status {
        SpanStatus::Ok => json!({ "code": 1 }),
        SpanStatus::Error { error: None } => json!({ "code": 2 }),
        SpanStatus::Error {
            error: Some(detail),
        } => json!({
            "code": 2,
            "message": format!("{}: {}", redact_string(&detail.name), redact_string(&detail.message)),
        }),
    }
}

fn otlp_span(span: &StoredSpan) -> Value {
    let duration = (span.ended_ms - span.started_ms).max(0.0);
    json!({
        "traceId": otlp_trace_id(&span.trace_id),
        "spanId": otlp_span_id(&span.id),
        "parentSpanId": span.parent_id.as_deref().map(otlp_span_id),
        "name": span.name,
        "startTimeUnixNano": nanos_string(span.started_ms),
        "endTimeUnixNano": nanos_string(span.ended_ms),
        "attributes": otlp_attributes(&span.attributes),
        "events": span.events.iter().map(|event| json!({
            "name": event.name,
            "timeUnixNano": nanos_string(event.time_ms),
            "attributes": otlp_attributes(&event.attributes),
        })).collect::<Vec<_>>(),
        "links": span.links.iter().map(|link| json!({
            "traceId": otlp_trace_id(&link.trace_id),
            "spanId": otlp_span_id(&link.span_id),
            "attributes": otlp_attributes(&link.attributes),
        })).collect::<Vec<_>>(),
        "status": status_json(&span.status),
        "durationNano": nanos_string(duration),
    })
}

fn otlp_metric(metric: &StoredMetric) -> Value {
    let attributes = otlp_attributes(&metric.attributes);
    let start = nanos_string(metric.interval_ms);
    let end = nanos_string(metric.interval_ms + INTERVAL_MS);
    match metric.kind {
        MetricKind::Counter | MetricKind::UpDownCounter => json!({
            "name": metric.name,
            "unit": metric.unit,
            "sum": {
                "dataPoints": [{
                    "asDouble": metric.sum,
                    "attributes": attributes,
                    "startTimeUnixNano": start,
                    "timeUnixNano": end,
                }],
                "aggregationTemporality": 2,
                "isMonotonic": metric.kind == MetricKind::Counter,
            },
        }),
        MetricKind::Histogram => json!({
            "name": metric.name,
            "unit": metric.unit,
            "histogram": {
                "dataPoints": [{
                    "count": metric.count.to_string(),
                    "sum": metric.sum,
                    "min": metric.min,
                    "max": metric.max,
                    "attributes": attributes,
                    "startTimeUnixNano": start,
                    "timeUnixNano": end,
                }],
                "aggregationTemporality": 1,
            },
        }),
    }
}

/// The OTLP/HTTP JSON trace payload for a set of settled spans.
#[must_use]
pub fn encode_spans(spans: &[StoredSpan]) -> Value {
    json!({
        "resourceSpans": [{
            "resource": resource(),
            "scopeSpans": [{
                "scope": { "name": SCOPE_NAME },
                "spans": spans.iter().map(otlp_span).collect::<Vec<_>>(),
            }],
        }],
    })
}

/// The OTLP/HTTP JSON metric payload for a set of stored metrics.
#[must_use]
pub fn encode_metrics(metrics: &[StoredMetric]) -> Value {
    json!({
        "resourceMetrics": [{
            "resource": resource(),
            "scopeMetrics": [{
                "scope": { "name": SCOPE_NAME },
                "metrics": metrics.iter().map(otlp_metric).collect::<Vec<_>>(),
            }],
        }],
    })
}

// -- exporter traits and transports ------------------------------------------

/// A failed export. `retryable` mirrors a transient transport/5xx failure;
/// the batching wrapper requeues on either kind, like a durable processor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportError {
    pub message: String,
    pub retryable: bool,
}

impl fmt::Display for ExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for ExportError {}

/// Transport seam for `OTLP/HTTP`. The default implementation performs no
/// I/O; a host supplies its own HTTP client without this crate taking a
/// networking dependency.
pub trait HttpTransport: Send + Sync {
    fn post_json(
        &self,
        endpoint: &str,
        headers: &BTreeMap<String, String>,
        body: &Value,
    ) -> Result<HttpResponse, ExportError>;
}

/// A minimal HTTP response: only the status code matters to the exporter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
}

/// The default transport: accepts everything locally and never opens a
/// socket, so enabling the exporter cannot cause network traffic by itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopHttpTransport;

impl HttpTransport for NoopHttpTransport {
    fn post_json(
        &self,
        _endpoint: &str,
        _headers: &BTreeMap<String, String>,
        _body: &Value,
    ) -> Result<HttpResponse, ExportError> {
        Ok(HttpResponse { status: 202 })
    }
}

impl<T: HttpTransport + ?Sized> HttpTransport for Arc<T> {
    fn post_json(
        &self,
        endpoint: &str,
        headers: &BTreeMap<String, String>,
        body: &Value,
    ) -> Result<HttpResponse, ExportError> {
        (**self).post_json(endpoint, headers, body)
    }
}

/// A recording fake used by tests and by hosts that want to inspect payloads.
#[derive(Debug, Default)]
pub struct FakeHttpTransport {
    requests: Mutex<Vec<RecordedRequest>>,
    failures: Mutex<usize>,
}

/// One captured `post_json` call.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedRequest {
    pub endpoint: String,
    pub headers: BTreeMap<String, String>,
    pub body: Value,
}

impl FakeHttpTransport {
    /// A fake that fails the next `failures` calls before succeeding.
    #[must_use]
    pub fn new(failures: usize) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            failures: Mutex::new(failures),
        }
    }

    /// All captured requests, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        lock(&self.requests).clone()
    }
}

impl HttpTransport for FakeHttpTransport {
    fn post_json(
        &self,
        endpoint: &str,
        headers: &BTreeMap<String, String>,
        body: &Value,
    ) -> Result<HttpResponse, ExportError> {
        lock(&self.requests).push(RecordedRequest {
            endpoint: endpoint.to_string(),
            headers: headers.clone(),
            body: body.clone(),
        });
        let mut failures = lock(&self.failures);
        if *failures > 0 {
            *failures -= 1;
            return Err(ExportError {
                message: "fake transport failure".to_string(),
                retryable: true,
            });
        }
        Ok(HttpResponse { status: 202 })
    }
}

/// Exports settled spans.
pub trait SpanExporter: Send + Sync {
    fn export_spans(&self, spans: &[StoredSpan]) -> Result<(), ExportError>;
    fn shutdown(&self) {}
}

/// Exports stored metrics.
pub trait MetricExporter: Send + Sync {
    fn export_metrics(&self, metrics: &[StoredMetric]) -> Result<(), ExportError>;
    fn shutdown(&self) {}
}

/// A single exporter handling both signals.
pub trait TelemetryExporter: SpanExporter + MetricExporter {}

impl<T: SpanExporter + MetricExporter> TelemetryExporter for T {}

/// An exporter that discards everything (the Rust counterpart of the noop
/// adapter in `noop.ts:1-37`).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopExporter;

impl SpanExporter for NoopExporter {
    fn export_spans(&self, _spans: &[StoredSpan]) -> Result<(), ExportError> {
        Ok(())
    }
}

impl MetricExporter for NoopExporter {
    fn export_metrics(&self, _metrics: &[StoredMetric]) -> Result<(), ExportError> {
        Ok(())
    }
}

/// An `OTLP/HTTP` JSON exporter for traces and metrics.
#[derive(Debug)]
pub struct OtlpHttpExporter<T = NoopHttpTransport> {
    endpoint: String,
    transport: T,
    headers: BTreeMap<String, String>,
    export_spans_enabled: bool,
    export_metrics_enabled: bool,
}

impl Default for OtlpHttpExporter<NoopHttpTransport> {
    fn default() -> Self {
        Self::new("http://localhost:4318", NoopHttpTransport)
    }
}

impl<T: HttpTransport> OtlpHttpExporter<T> {
    /// An exporter posting both signals to `endpoint` through `transport`.
    pub fn new(endpoint: impl Into<String>, transport: T) -> Self {
        Self {
            endpoint: endpoint.into(),
            transport,
            headers: BTreeMap::new(),
            export_spans_enabled: true,
            export_metrics_enabled: true,
        }
    }

    /// Adds a request header (for example `Content-Type` or an API key).
    /// Callers must not put secrets here unless the transport requires them.
    #[must_use]
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(key.into(), value.into());
        self
    }

    /// Selects which signals are emitted.
    #[must_use]
    pub fn with_signals(mut self, spans: bool, metrics: bool) -> Self {
        self.export_spans_enabled = spans;
        self.export_metrics_enabled = metrics;
        self
    }

    /// The configured endpoint.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn post(&self, body: &Value) -> Result<(), ExportError> {
        let response = self
            .transport
            .post_json(&self.endpoint, &self.headers, body)?;
        if (200..300).contains(&response.status) {
            Ok(())
        } else {
            Err(ExportError {
                message: format!("OTLP endpoint returned {}", response.status),
                retryable: response.status == 429 || response.status >= 500,
            })
        }
    }
}

impl<T: HttpTransport> SpanExporter for OtlpHttpExporter<T> {
    fn export_spans(&self, spans: &[StoredSpan]) -> Result<(), ExportError> {
        if !self.export_spans_enabled || spans.is_empty() {
            return Ok(());
        }
        self.post(&encode_spans(spans))
    }
}

impl<T: HttpTransport> MetricExporter for OtlpHttpExporter<T> {
    fn export_metrics(&self, metrics: &[StoredMetric]) -> Result<(), ExportError> {
        if !self.export_metrics_enabled || metrics.is_empty() {
            return Ok(());
        }
        self.post(&encode_metrics(metrics))
    }
}

// -- batching ---------------------------------------------------------------

/// Bounds for [`BatchExporter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchConfig {
    /// Emit as soon as this many items of one signal are queued.
    pub max_items: usize,
    /// Drop the oldest queued items beyond this bound.
    pub max_queue_items: usize,
    /// Emit a non-empty queue that has been idle this long on the next call.
    pub flush_after: Duration,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            max_items: 64,
            max_queue_items: 2048,
            flush_after: Duration::from_millis(5000),
        }
    }
}

struct Pending<T> {
    items: Vec<T>,
    enqueued_at: Instant,
}

impl<T> Default for Pending<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            enqueued_at: Instant::now(),
        }
    }
}

#[derive(Default)]
struct BatchState {
    spans: Pending<StoredSpan>,
    metrics: Pending<StoredMetric>,
    exports: usize,
    failures: usize,
}

fn enqueue<T: Clone>(
    pending: &mut Pending<T>,
    incoming: &[T],
    config: &BatchConfig,
) -> Option<Vec<T>> {
    let now = Instant::now();
    if !pending.items.is_empty() && now.duration_since(pending.enqueued_at) >= config.flush_after {
        return Some(std::mem::take(&mut pending.items));
    }
    pending.items.extend_from_slice(incoming);
    if pending.items.len() > config.max_queue_items {
        let drop_count = pending.items.len() - config.max_queue_items;
        pending.items.drain(..drop_count);
    }
    pending.enqueued_at = now;
    if pending.items.len() >= config.max_items {
        Some(std::mem::take(&mut pending.items))
    } else {
        None
    }
}

/// A batching wrapper around any [`TelemetryExporter`]. Failed batches are
/// requeued at the front so a retry keeps their original order.
pub struct BatchExporter {
    inner: Arc<dyn TelemetryExporter>,
    config: BatchConfig,
    state: Mutex<BatchState>,
}

impl BatchExporter {
    /// A batcher with upstream-neutral defaults (see [`BatchConfig`]).
    #[must_use]
    pub fn new(inner: Arc<dyn TelemetryExporter>) -> Self {
        Self::with_config(inner, BatchConfig::default())
    }

    /// A batcher with explicit bounds.
    #[must_use]
    pub fn with_config(inner: Arc<dyn TelemetryExporter>, config: BatchConfig) -> Self {
        Self {
            inner,
            config,
            state: Mutex::new(BatchState::default()),
        }
    }

    /// Queued `(spans, metrics)` counts.
    #[must_use]
    pub fn queued(&self) -> (usize, usize) {
        let state = lock(&self.state);
        (state.spans.items.len(), state.metrics.items.len())
    }

    /// Number of failed export attempts.
    #[must_use]
    pub fn failures(&self) -> usize {
        lock(&self.state).failures
    }

    /// Number of export attempts (successful or not).
    #[must_use]
    pub fn exports(&self) -> usize {
        lock(&self.state).exports
    }

    /// Flushes both queues. On failure the batch is put back at the front.
    pub fn flush(&self) -> Result<(), ExportError> {
        let (spans, metrics) = {
            let mut state = lock(&self.state);
            (
                std::mem::take(&mut state.spans.items),
                std::mem::take(&mut state.metrics.items),
            )
        };
        let span_result = self.send_spans(spans);
        let metric_result = self.send_metrics(metrics);
        span_result.and(metric_result)
    }

    /// Best-effort final flush, like upstream's fire-and-forget fetch.
    pub fn shutdown(&self) {
        let _ = self.flush();
    }

    fn send_spans(&self, batch: Vec<StoredSpan>) -> Result<(), ExportError> {
        if batch.is_empty() {
            return Ok(());
        }
        let result = self.inner.export_spans(&batch);
        let mut state = lock(&self.state);
        state.exports += 1;
        if result.is_err() {
            state.failures += 1;
            let mut requeued = batch;
            requeued.extend(std::mem::take(&mut state.spans.items));
            state.spans.items = requeued;
            state.spans.enqueued_at = Instant::now();
        }
        result
    }

    fn send_metrics(&self, batch: Vec<StoredMetric>) -> Result<(), ExportError> {
        if batch.is_empty() {
            return Ok(());
        }
        let result = self.inner.export_metrics(&batch);
        let mut state = lock(&self.state);
        state.exports += 1;
        if result.is_err() {
            state.failures += 1;
            let mut requeued = batch;
            requeued.extend(std::mem::take(&mut state.metrics.items));
            state.metrics.items = requeued;
            state.metrics.enqueued_at = Instant::now();
        }
        result
    }
}

impl SpanExporter for BatchExporter {
    fn export_spans(&self, spans: &[StoredSpan]) -> Result<(), ExportError> {
        let ready = {
            let mut state = lock(&self.state);
            enqueue(&mut state.spans, spans, &self.config)
        };
        ready.map_or(Ok(()), |batch| self.send_spans(batch))
    }

    fn shutdown(&self) {
        BatchExporter::shutdown(self);
    }
}

impl MetricExporter for BatchExporter {
    fn export_metrics(&self, metrics: &[StoredMetric]) -> Result<(), ExportError> {
        let ready = {
            let mut state = lock(&self.state);
            enqueue(&mut state.metrics, metrics, &self.config)
        };
        ready.map_or(Ok(()), |batch| self.send_metrics(batch))
    }

    fn shutdown(&self) {
        BatchExporter::shutdown(self);
    }
}

// -- one-shot host helper ----------------------------------------------------

/// Outcome of [`export_telemetry`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportReport {
    pub spans: usize,
    pub metrics: usize,
    pub errors: Vec<ExportError>,
}

fn stored_metric_from_measurement(measurement: &RecordedTelemetryMeasurement) -> StoredMetric {
    StoredMetric {
        interval_ms: (telemetry_now() / INTERVAL_MS).floor() * INTERVAL_MS,
        name: measurement.name.clone(),
        kind: measurement.kind,
        unit: measurement.unit.clone(),
        attributes: measurement.attributes.clone(),
        count: 1,
        sum: measurement.value,
        min: measurement.value,
        max: measurement.value,
    }
}

/// Exports everything currently recorded by a telemetry adapter. Hosts call
/// this at flush/shutdown; no timer or background task is started.
pub fn export_telemetry(
    telemetry: &dyn Telemetry,
    exporter: &dyn TelemetryExporter,
) -> ExportReport {
    let mut report = ExportReport::default();
    let spans = telemetry.stored_spans();
    report.spans = spans.len();
    if let Err(error) = exporter.export_spans(&spans) {
        report.errors.push(error);
    }
    let measurements = telemetry.get_metrics();
    let metrics = measurements
        .iter()
        .map(stored_metric_from_measurement)
        .collect::<Vec<_>>();
    report.metrics = metrics.len();
    if let Err(error) = exporter.export_metrics(&metrics) {
        report.errors.push(error);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{InMemoryTelemetry, SpanAttributes as Attrs};

    fn attrs(pairs: &[(&str, AttributeValue)]) -> SpanAttributes {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect()
    }

    fn span(name: &str) -> StoredSpan {
        StoredSpan {
            id: "scope:1".to_string(),
            trace_id: "scope:1".to_string(),
            parent_id: None,
            name: name.to_string(),
            started_ms: 1_700_000_000_000.0,
            ended_ms: 1_700_000_000_002.5,
            attributes: SpanAttributes::new(),
            events: Vec::new(),
            links: Vec::new(),
            status: SpanStatus::Ok,
        }
    }

    fn metric(name: &str, kind: MetricKind) -> StoredMetric {
        StoredMetric {
            interval_ms: 1_700_000_000_000.0,
            name: name.to_string(),
            kind,
            unit: Some("By".to_string()),
            attributes: SpanAttributes::new(),
            count: 1,
            sum: 12.0,
            min: 12.0,
            max: 12.0,
        }
    }

    /// Privacy rule: credential-named keys are dropped and credential-shaped
    /// values are replaced before any payload leaves the process.
    #[test]
    fn redaction_drops_keys_and_scrubs_values() {
        let input = attrs(&[
            ("authorization", AttributeValue::String("Bearer abc".into())),
            ("x-api-key", AttributeValue::String("sk-live-123".into())),
            ("user_token", AttributeValue::String("t".into())),
            ("password", AttributeValue::String("hunter2".into())),
            ("session_id", AttributeValue::String("s".into())),
            (
                "prompt",
                AttributeValue::StringArray(vec!["eyJhbGciOiJIUzI1NiJ9.x.y".into()]),
            ),
            ("safe", AttributeValue::String("visible".into())),
        ]);
        let redacted = redact_attributes(&input);
        assert_eq!(redacted.len(), 1);
        assert_eq!(
            redacted.get("safe"),
            Some(&AttributeValue::String("visible".into()))
        );

        let input = attrs(&[
            (
                "note",
                AttributeValue::String("-----BEGIN PRIVATE KEY-----".into()),
            ),
            ("note2", AttributeValue::String("ghp_abcdefghij".into())),
            ("count", AttributeValue::Number(4.0)),
        ]);
        let redacted = redact_attributes(&input);
        assert_eq!(
            redacted.get("note"),
            Some(&AttributeValue::String(REDACTED.into()))
        );
        assert_eq!(
            redacted.get("note2"),
            Some(&AttributeValue::String(REDACTED.into()))
        );
        assert_eq!(redacted.get("count"), Some(&AttributeValue::Number(4.0)));
    }

    /// The encoder emits an OTLP/HTTP JSON trace and metric payload with
    /// fixed-width ids, nanosecond timestamps and redacted attributes.
    #[test]
    fn otlp_payloads_have_the_expected_shape() {
        let mut item = span("root");
        item.attributes = attrs(&[("api_key", AttributeValue::String("sk-secret".into()))]);
        item.events.push(StoredEvent {
            name: "event".to_string(),
            time_ms: 1.0,
            attributes: SpanAttributes::new(),
        });
        item.links.push(StoredLink {
            trace_id: "other".to_string(),
            span_id: "other:9".to_string(),
            attributes: SpanAttributes::new(),
        });
        item.status = SpanStatus::Error {
            error: Some(SpanError {
                name: "Err".to_string(),
                message: "Bearer leaked".to_string(),
            }),
        };

        let payload = encode_spans(&[item]);
        let resources = payload["resourceSpans"].as_array().expect("resource spans");
        assert_eq!(resources.len(), 1);
        assert_eq!(
            resources[0]["resource"]["attributes"][0]["key"],
            "service.name"
        );
        let scopes = resources[0]["scopeSpans"].as_array().expect("scope spans");
        let encoded = &scopes[0]["spans"][0];
        assert_eq!(encoded["name"], "root");
        assert_eq!(encoded["traceId"].as_str().map(str::len), Some(32));
        assert_eq!(encoded["spanId"].as_str().map(str::len), Some(16));
        assert!(encoded["parentSpanId"].is_null());
        assert_eq!(encoded["status"]["code"], 2);
        assert_eq!(encoded["status"]["message"], "Err: [REDACTED]");
        assert_eq!(encoded["attributes"].as_array().map(Vec::len), Some(0));
        assert_eq!(encoded["events"][0]["name"], "event");
        assert_eq!(
            encoded["links"][0]["spanId"].as_str().map(str::len),
            Some(16)
        );
        assert!(
            encoded["startTimeUnixNano"]
                .as_str()
                .map(str::len)
                .is_some()
        );

        let metrics = encode_metrics(&[
            metric("sent", MetricKind::Counter),
            metric("latency", MetricKind::Histogram),
        ]);
        let list = metrics["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .expect("metrics");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["sum"]["isMonotonic"], true);
        assert_eq!(list[0]["sum"]["aggregationTemporality"], 2);
        assert_eq!(list[1]["histogram"]["dataPoints"][0]["count"], "1");
        assert_eq!(list[1]["histogram"]["dataPoints"][0]["sum"], 12.0);
    }

    /// With the default noop transport the exporter is inert: no socket, no
    /// traffic, even when telemetry is enabled.
    #[test]
    fn default_exporter_is_inert() {
        let exporter = OtlpHttpExporter::default();
        assert!(SpanExporter::export_spans(&exporter, &[span("s")]).is_ok());
        assert!(
            MetricExporter::export_metrics(&exporter, &[metric("m", MetricKind::Counter)]).is_ok()
        );
        assert!(SpanExporter::export_spans(&exporter, &[]).is_ok());
    }

    /// Batching flushes at capacity and requeues failed batches at the front.
    #[test]
    fn batching_retries_failed_batches_in_order() {
        let transport = Arc::new(FakeHttpTransport::new(1));
        let inner = Arc::new(OtlpHttpExporter::new(
            "http://collector",
            Arc::clone(&transport),
        ));
        let batcher = BatchExporter::with_config(
            inner,
            BatchConfig {
                max_items: 2,
                ..BatchConfig::default()
            },
        );
        assert!(SpanExporter::export_spans(&batcher, &[span("first")]).is_ok());
        assert_eq!(batcher.queued(), (1, 0));
        assert!(SpanExporter::export_spans(&batcher, &[span("second")]).is_err());
        assert_eq!(batcher.failures(), 1);
        assert_eq!(batcher.queued(), (2, 0));
        assert!(batcher.flush().is_ok());
        assert_eq!(batcher.queued(), (0, 0));
        assert_eq!(batcher.exports(), 2);

        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].endpoint, "http://collector");
        let first = requests[0].body["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["name"]
            .as_str()
            .map(str::to_string);
        assert_eq!(first.as_deref(), Some("first"));
    }

    /// An idle queue is emitted on the next call, and a single oversized
    /// call keeps only the newest `max_queue_items` entries.
    #[test]
    fn batching_honours_idle_flush_and_queue_cap() {
        let transport = Arc::new(FakeHttpTransport::new(0));
        let inner = Arc::new(OtlpHttpExporter::new(
            "http://collector",
            Arc::clone(&transport),
        ));
        let batcher = BatchExporter::with_config(
            inner,
            BatchConfig {
                max_items: 100,
                max_queue_items: 2,
                flush_after: Duration::ZERO,
            },
        );
        assert!(SpanExporter::export_spans(&batcher, &[span("one")]).is_ok());
        assert!(SpanExporter::export_spans(&batcher, &[span("two")]).is_ok());
        assert!(SpanExporter::export_spans(&batcher, &[span("three")]).is_ok());
        assert_eq!(transport.requests().len(), 2);
        assert_eq!(batcher.queued(), (1, 0));

        let capped = BatchExporter::with_config(
            Arc::new(OtlpHttpExporter::new(
                "http://other",
                Arc::new(FakeHttpTransport::default()),
            )),
            BatchConfig {
                max_items: 100,
                max_queue_items: 2,
                ..BatchConfig::default()
            },
        );
        let batch = vec![span("a"), span("b"), span("c")];
        assert!(SpanExporter::export_spans(&capped, &batch).is_ok());
        assert_eq!(capped.queued(), (2, 0));
        assert!(capped.flush().is_ok());
    }

    /// A non-success HTTP status is an export error and is retryable for
    /// throttling/server failures.
    #[test]
    fn non_success_status_is_reported() {
        #[derive(Debug)]
        struct StatusTransport(u16);
        impl HttpTransport for StatusTransport {
            fn post_json(
                &self,
                _endpoint: &str,
                _headers: &BTreeMap<String, String>,
                _body: &Value,
            ) -> Result<HttpResponse, ExportError> {
                Ok(HttpResponse { status: self.0 })
            }
        }
        let exporter = OtlpHttpExporter::new("http://collector", StatusTransport(429));
        let error = SpanExporter::export_spans(&exporter, &[span("s")]).expect_err("429");
        assert!(error.retryable);
        let exporter = OtlpHttpExporter::new("http://collector", StatusTransport(400));
        let error = SpanExporter::export_spans(&exporter, &[span("s")]).expect_err("400");
        assert!(!error.retryable);
    }

    /// The host helper exports settled spans and recorded measurements from a
    /// telemetry adapter without touching the network.
    #[test]
    fn export_telemetry_forwards_both_signals() {
        let telemetry = InMemoryTelemetry::new();
        telemetry.start_span(crate::telemetry::SpanOptions::new("work"), &mut |_| {});
        telemetry
            .counter("count", &crate::telemetry::InstrumentOptions::default())
            .add(1.0, Attrs::new());
        let transport = Arc::new(FakeHttpTransport::new(0));
        let exporter = OtlpHttpExporter::new("http://collector", Arc::clone(&transport));
        let report = export_telemetry(&telemetry, &exporter);
        assert_eq!(report.spans, 1);
        assert_eq!(report.metrics, 1);
        assert_eq!(report.errors.len(), 0);
        assert_eq!(transport.requests().len(), 2);
    }
}
