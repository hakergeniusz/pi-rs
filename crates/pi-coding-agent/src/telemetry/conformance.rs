//! Runner-independent conformance shapes for telemetry adapters.
//!
//! Ground truth (all line numbers refer to earendil-works/pi v1.5.2):
//!
//! * `packages/telemetry/src/testing/types.ts:4-21` — `TelemetryAdapterFixture`
//!   (fresh adapter plus normalized snapshot readers),
//!   `TelemetryAdapterFixtureFactory` and `TelemetryAdapterConformanceCase`
//!   (`group`, `name`, `run()`).
//! * `packages/telemetry/src/testing/conformance.ts:10-41` — the case creator,
//!   span lookup and rejection helpers.
//! * `packages/telemetry/src/testing/conformance.ts:43-386` — the capability,
//!   metric, link, propagation, lifecycle, status, recording and parentage
//!   cases ported below.
//! * `packages/telemetry/src/testing/index.ts:1-6` — the public re-exports.
//!
//! # Deviation (mechanism only)
//!
//! Upstream cases are `async` (callback results are promises, fixtures are
//! `AsyncDisposable`). The port's `start_span` takes a synchronous `FnMut`
//! callback and its recorder needs no disposal, so `run()` is synchronous and
//! the fixture holds only the adapter. Two JS-only cases are not portable:
//! late calls on a handle that outlives the callback (Rust cannot retain the
//! borrowed handle) and rejection values (the port uses panics, covered by
//! `startInMemorySpan` unit tests). Everything else keeps upstream's group
//! and case names and assertion intent.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use super::{
    AttributeValue, HistogramOptions, InMemoryTelemetry, InstrumentOptions, MetricKind, NoopSpan,
    RecordedTelemetrySpan, SpanAttributes, SpanError, SpanLink, SpanOptions, SpanStatus, Telemetry,
    TelemetryCarrier,
};

/// `TelemetryAdapterFixture` (`testing/types.ts:4-11`). The port's recorder
/// has no async disposal or optional metric reader, so those fields collapse
/// into the one adapter handle.
pub struct TelemetryAdapterFixture {
    pub context: Arc<dyn Telemetry>,
}

/// `TelemetryAdapterFixtureFactory` (`testing/types.ts:14`): one isolated
/// adapter per conformance case.
pub type TelemetryAdapterFixtureFactory = Arc<dyn Fn() -> TelemetryAdapterFixture + Send + Sync>;

/// A runner-independent assertion over one fixture
/// (`testing/conformance.ts:10`).
pub type ConformanceTest = Arc<dyn Fn(&TelemetryAdapterFixture) + Send + Sync>;

/// `TelemetryAdapterConformanceCase` (`testing/types.ts:17-21`). The factory
/// is captured so each `run()` gets a fresh adapter, as upstream's
/// `createCase(factory, …)` does (`conformance.ts:12-26`).
pub struct TelemetryAdapterConformanceCase {
    pub group: &'static str,
    pub name: &'static str,
    factory: TelemetryAdapterFixtureFactory,
    test: ConformanceTest,
}

impl TelemetryAdapterConformanceCase {
    /// Creates a fresh fixture and runs this case's assertions.
    pub fn run(&self) {
        let fixture = (self.factory)();
        (self.test)(&fixture);
    }
}

/// The factory used by the port's own conformance run: a fresh
/// [`InMemoryTelemetry`] scope per case.
#[must_use]
pub fn in_memory_adapter_factory() -> TelemetryAdapterFixtureFactory {
    Arc::new(|| TelemetryAdapterFixture {
        context: Arc::new(InMemoryTelemetry::new()),
    })
}

fn case<F>(
    factory: &TelemetryAdapterFixtureFactory,
    group: &'static str,
    name: &'static str,
    test: F,
) -> TelemetryAdapterConformanceCase
where
    F: Fn(&TelemetryAdapterFixture) + Send + Sync + 'static,
{
    TelemetryAdapterConformanceCase {
        group,
        name,
        factory: Arc::clone(factory),
        test: Arc::new(test),
    }
}

fn attrs(pairs: &[(&str, AttributeValue)]) -> SpanAttributes {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), value.clone()))
        .collect()
}

fn find_span<'a>(spans: &'a [RecordedTelemetrySpan], name: &str) -> &'a RecordedTelemetrySpan {
    spans
        .iter()
        .find(|candidate| candidate.name == name)
        .unwrap_or_else(|| panic!("Expected recorded span {name}"))
}

// -- the ported cases --------------------------------------------------------

/// `conformance.ts:48-74`: capabilities stay reachable from contexts, span
/// handles, children and extracted parents.
fn capabilities(fixture: &TelemetryAdapterFixture) {
    let telemetry = fixture.context.as_ref();
    let mut carrier = None;
    telemetry.start_span(SpanOptions::new("capabilities"), &mut |span| {
        span.add_event("event", SpanAttributes::new());
        carrier = telemetry.inject(span);
        let mut child_ran = false;
        span.start_span(SpanOptions::new("capabilities.child"), &mut |child| {
            child_ran = true;
            assert!(child.carrier().is_some());
        });
        assert!(child_ran);
    });
    let carrier = carrier.expect("carrier of the capabilities span");
    let parent = telemetry.extract(Some(&carrier));
    assert!(parent.carrier().is_some());
    telemetry.start_span_in(
        Some(&parent),
        SpanOptions::new("capabilities.extracted"),
        &mut |span| {
            assert!(span.carrier().is_some());
        },
    );
}

/// `conformance.ts:76-107`: ordered measurements with metadata and an
/// exemplar context taken from the active span.
#[allow(clippy::float_cmp)] // exact measurement values, as in upstream's deepStrictEqual
fn ordered_measurements(fixture: &TelemetryAdapterFixture) {
    let telemetry = fixture.context.as_ref();
    let mut span_carrier = None;
    telemetry.start_span(SpanOptions::new("measured"), &mut |span| {
        span_carrier = telemetry.inject(span);
    });
    telemetry
        .counter(
            "bytes",
            &InstrumentOptions {
                description: Some("Encoded bytes".to_string()),
                unit: Some("By".to_string()),
            },
        )
        .add(
            12.0,
            attrs(&[("direction", AttributeValue::String("out".to_string()))]),
        );
    let active = telemetry.up_down_counter("active", &InstrumentOptions::default());
    active.add(1.0, SpanAttributes::new());
    active.add(-1.0, SpanAttributes::new());
    telemetry
        .histogram(
            "duration",
            &HistogramOptions {
                unit: Some("ms".to_string()),
                boundaries: Some(vec![1.0, 10.0]),
                ..HistogramOptions::default()
            },
        )
        .record(
            5.0,
            attrs(&[("method", AttributeValue::String("read".to_string()))]),
            span_carrier.clone(),
        );

    let measurements = telemetry.get_metrics();
    assert_eq!(measurements.len(), 4);
    let expected = [
        (MetricKind::Counter, "bytes", 12.0),
        (MetricKind::UpDownCounter, "active", 1.0),
        (MetricKind::UpDownCounter, "active", -1.0),
        (MetricKind::Histogram, "duration", 5.0),
    ];
    for (measurement, (kind, name, value)) in measurements.iter().zip(expected) {
        assert_eq!(measurement.kind, kind);
        assert_eq!(measurement.name, name);
        assert_eq!(measurement.value, value);
    }
    assert_eq!(
        measurements[0].attributes,
        attrs(&[("direction", AttributeValue::String("out".to_string()))])
    );
    assert_eq!(
        measurements[0].description.as_deref(),
        Some("Encoded bytes")
    );
    assert_eq!(measurements[0].unit.as_deref(), Some("By"));
    assert_eq!(measurements[3].boundaries, Some(vec![1.0, 10.0]));
    assert_eq!(measurements[3].context, span_carrier);
}

/// `conformance.ts:109-126`: instrument metadata and attributes are copied at
/// creation/recording time, not shared with the caller.
fn metadata_is_copied(fixture: &TelemetryAdapterFixture) {
    let telemetry = fixture.context.as_ref();
    let mut boundaries = vec![1.0, 10.0];
    let mut options = HistogramOptions {
        description: Some("Original".to_string()),
        boundaries: Some(boundaries.clone()),
        ..HistogramOptions::default()
    };
    let histogram = telemetry.histogram("snapshot", &options);
    boundaries.push(100.0);
    options.description = Some("Changed".to_string());
    let mut tags = vec!["original".to_string()];
    histogram.record(
        -1.0,
        attrs(&[("tags", AttributeValue::StringArray(tags.clone()))]),
        None,
    );
    tags.push("changed".to_string());

    let measurements = telemetry.get_metrics();
    assert_eq!(measurements.len(), 1);
    assert_eq!(
        measurements[0].attributes,
        attrs(&[(
            "tags",
            AttributeValue::StringArray(vec!["original".to_string()])
        )])
    );
    assert_eq!(measurements[0].boundaries, Some(vec![1.0, 10.0]));
    assert_eq!(measurements[0].description.as_deref(), Some("Original"));
    assert_eq!(measurements[0].context, None);
}

/// `conformance.ts:127-148`: initial and late links are recorded without
/// changing the parent, and caller mutations do not alter snapshots.
fn links_do_not_change_parents(fixture: &TelemetryAdapterFixture) {
    let telemetry = fixture.context.as_ref();
    let mut target_carrier = None;
    telemetry.start_span(SpanOptions::new("target"), &mut |span| {
        target_carrier = telemetry.inject(span);
    });
    let target_carrier = target_carrier.expect("target carrier");
    let mut reason = vec!["initial".to_string()];
    let initial = SpanLink {
        context: target_carrier.clone(),
        attributes: attrs(&[("reason", AttributeValue::StringArray(reason.clone()))]),
    };
    telemetry.start_span(
        SpanOptions {
            name: "linked".to_string(),
            attributes: SpanAttributes::new(),
            links: vec![initial],
        },
        &mut |span| {
            span.add_link(SpanLink {
                context: target_carrier.clone(),
                attributes: attrs(&[("reason", AttributeValue::String("late".to_string()))]),
            });
        },
    );
    reason.push("mutated".to_string());

    let spans = telemetry.get_spans();
    let linked = find_span(&spans, "linked");
    assert_eq!(linked.parent_id, None);
    assert_eq!(linked.parent_context, None);
    assert_eq!(linked.links.len(), 2);
    assert_eq!(
        linked.links[0].attributes,
        attrs(&[(
            "reason",
            AttributeValue::StringArray(vec!["initial".to_string()])
        )])
    );
    assert_eq!(
        linked.links[1].attributes,
        attrs(&[("reason", AttributeValue::String("late".to_string()))])
    );
    assert_eq!(linked.links[0].context, find_span(&spans, "target").context);
}

/// `conformance.ts:127-148`: a noop (empty) carrier contributes no link.
fn noop_links_are_ignored(fixture: &TelemetryAdapterFixture) {
    let telemetry = fixture.context.as_ref();
    telemetry.start_span(
        SpanOptions {
            name: "no-links".to_string(),
            attributes: SpanAttributes::new(),
            links: vec![SpanLink {
                context: TelemetryCarrier::new(),
                attributes: SpanAttributes::new(),
            }],
        },
        &mut |span| {
            span.add_link(SpanLink {
                context: TelemetryCarrier::new(),
                attributes: SpanAttributes::new(),
            });
        },
    );
    assert_eq!(find_span(&telemetry.get_spans(), "no-links").links.len(), 0);
}

/// `conformance.ts:159-178`: the carrier survives a wire-style clone and a
/// later mutation of the caller's copy; injection stays stable.
fn carrier_roundtrip(fixture: &TelemetryAdapterFixture) {
    let telemetry = fixture.context.as_ref();
    let mut carrier = None;
    telemetry.start_span(SpanOptions::new("sender"), &mut |span| {
        carrier = telemetry.inject(span);
    });
    let carrier = carrier.expect("sender carrier");
    let wire = carrier.clone(); // the port has no JSON.parse/stringify step
    let parent = telemetry.extract(Some(&wire));
    let mut mutated = wire.clone();
    for key in mutated.keys().cloned().collect::<Vec<_>>() {
        mutated.insert(key, "mutated".to_string());
    }
    let mutated_parent = telemetry.extract(Some(&mutated));
    telemetry.start_span_in(Some(&parent), SpanOptions::new("receiver"), &mut |_| {});
    telemetry.start_span_in(
        Some(&mutated_parent),
        SpanOptions::new("receiver-mutated"),
        &mut |_| {},
    );
    let spans = telemetry.get_spans();
    assert_eq!(
        find_span(&spans, "receiver").parent_context,
        Some(carrier.clone())
    );
    assert_eq!(
        find_span(&spans, "receiver-mutated").parent_context,
        Some(mutated)
    );
    // The extracted snapshot kept the original carrier even though the
    // caller's copy was mutated afterwards.
    assert_eq!(parent.carrier(), Some(&wire));
}

/// `conformance.ts:179-188`: absent, empty and unknown carriers all produce
/// recorded roots, and injecting a noop span yields nothing.
fn carrier_fallbacks(fixture: &TelemetryAdapterFixture) {
    let telemetry = fixture.context.as_ref();
    assert!(telemetry.inject(&NoopSpan).is_none());
    let mut unknown = TelemetryCarrier::new();
    unknown.insert("unknown".to_string(), "format".to_string());
    let carriers: [Option<TelemetryCarrier>; 3] =
        [None, Some(TelemetryCarrier::new()), Some(unknown)];
    for (index, carrier) in carriers.iter().enumerate() {
        let parent = telemetry.extract(carrier.as_ref());
        let mut ran = false;
        telemetry.start_span_in(
            Some(&parent),
            SpanOptions::new(format!("fallback-{index}")),
            &mut |span| {
                ran = true;
                assert!(span.carrier().is_some());
            },
        );
        assert!(ran);
    }
    for span in telemetry.get_spans() {
        assert_eq!(span.parent_context, None);
    }
}

/// `conformance.ts:190-212`: an extracted carrier crosses fresh adapters and
/// multiple hops.
fn multi_hop_propagation(
    factory: &TelemetryAdapterFixtureFactory,
    fixture: &TelemetryAdapterFixture,
) {
    let receiver = factory();
    let worker = factory();
    let mut sender_carrier = None;
    fixture
        .context
        .start_span(SpanOptions::new("sender"), &mut |span| {
            sender_carrier = fixture.context.inject(span);
        });
    let sender_carrier = sender_carrier.expect("sender carrier");

    let mut receiver_carrier = None;
    let parent = receiver.context.extract(Some(&sender_carrier));
    receiver
        .context
        .start_span_in(Some(&parent), SpanOptions::new("receiver"), &mut |span| {
            receiver_carrier = receiver.context.inject(span);
        });
    let receiver_carrier = receiver_carrier.expect("receiver carrier");

    let parent = worker.context.extract(Some(&receiver_carrier));
    worker
        .context
        .start_span_in(Some(&parent), SpanOptions::new("worker"), &mut |_| {});

    assert_eq!(
        find_span(&receiver.context.get_spans(), "receiver").parent_context,
        Some(sender_carrier)
    );
    assert_eq!(
        find_span(&worker.context.get_spans(), "worker").parent_context,
        Some(receiver_carrier)
    );
}

/// `conformance.ts:213-233`: admission is synchronous, once, and the span
/// settles as ok.
fn callback_lifecycle(fixture: &TelemetryAdapterFixture) {
    let mut calls = 0;
    let mut admitted = false;
    fixture
        .context
        .start_span(SpanOptions::new("success"), &mut |span| {
            admitted = true;
            calls += 1;
            span.add_event("event", SpanAttributes::new());
        });
    assert!(admitted);
    assert_eq!(calls, 1);
    let spans = fixture.context.get_spans();
    let span = find_span(&spans, "success");
    assert_eq!(span.status, SpanStatus::Ok);
    assert!(span.settled);
}

/// `conformance.ts:268-308`: the last explicit status wins, including over a
/// panicking callback (upstream's sync/async rejection).
fn explicit_status_wins(fixture: &TelemetryAdapterFixture) {
    let telemetry = fixture.context.as_ref();
    telemetry.start_span(SpanOptions::new("last-status"), &mut |span| {
        span.set_status(SpanStatus::Error {
            error: Some(SpanError {
                name: "Expected".to_string(),
                message: "first".to_string(),
            }),
        });
        span.set_status(SpanStatus::Ok);
    });
    assert_eq!(
        find_span(&telemetry.get_spans(), "last-status").status,
        SpanStatus::Ok
    );

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        telemetry.start_span(SpanOptions::new("explicit-before-throw"), &mut |span| {
            span.set_status(SpanStatus::Ok);
            panic!("after explicit status");
        });
    }));
    assert!(outcome.is_err());
    assert_eq!(
        find_span(&telemetry.get_spans(), "explicit-before-throw").status,
        SpanStatus::Ok
    );
}

/// `conformance.ts:310-330`: attributes merge and events keep their order.
/// Upstream's `undefined` drop is represented by an explicit final value,
/// since [`AttributeValue`] has no undefined variant.
fn recording_merges_and_orders(fixture: &TelemetryAdapterFixture) {
    fixture.context.start_span(
        SpanOptions {
            name: "recording".to_string(),
            attributes: attrs(&[
                ("start", AttributeValue::String("value".to_string())),
                ("overwrite", AttributeValue::String("start".to_string())),
            ]),
            links: Vec::new(),
        },
        &mut |span| {
            span.set_attributes(attrs(&[
                ("count", AttributeValue::Number(1.0)),
                ("overwrite", AttributeValue::String("middle".to_string())),
            ]));
            span.set_attributes(attrs(&[
                ("count", AttributeValue::Number(2.0)),
                ("overwrite", AttributeValue::String("end".to_string())),
            ]));
            span.add_event("first", attrs(&[("index", AttributeValue::Number(1.0))]));
            span.add_event("second", attrs(&[("index", AttributeValue::Number(2.0))]));
        },
    );

    let spans = fixture.context.get_spans();
    let span = find_span(&spans, "recording");
    assert_eq!(
        span.attributes,
        attrs(&[
            ("start", AttributeValue::String("value".to_string())),
            ("overwrite", AttributeValue::String("end".to_string())),
            ("count", AttributeValue::Number(2.0)),
        ])
    );
    assert_eq!(span.events.len(), 2);
    assert_eq!(span.events[0].name, "first");
    assert_eq!(span.events[1].name, "second");
}

/// `conformance.ts:358-385`: nested children keep their parent and settle in
/// child-before-parent order.
fn parentage(fixture: &TelemetryAdapterFixture) {
    fixture
        .context
        .start_span(SpanOptions::new("parent"), &mut |parent| {
            parent.start_span(SpanOptions::new("first-child"), &mut |_| {});
            parent.start_span(SpanOptions::new("second-child"), &mut |_| {});
        });
    let spans = fixture.context.get_spans();
    let parent = find_span(&spans, "parent");
    let first = find_span(&spans, "first-child");
    let second = find_span(&spans, "second-child");
    assert_eq!(parent.parent_id, None);
    assert_eq!(first.parent_id, Some(parent.id));
    assert_eq!(second.parent_id, Some(parent.id));
    let first_end = first.end_sequence.expect("first end sequence");
    let second_end = second.end_sequence.expect("second end sequence");
    let parent_end = parent.end_sequence.expect("parent end sequence");
    assert!(second_end < first_end);
    assert!(first_end < parent_end);
}

/// `createTelemetryAdapterConformance` (`testing/conformance.ts:43-386`):
/// runner-independent cases, each bound to a fresh-adapter factory.
#[must_use]
#[allow(clippy::too_many_lines)] // mirrors the TS case list; split when extending
pub fn create_telemetry_adapter_conformance(
    factory: &TelemetryAdapterFixtureFactory,
) -> Vec<TelemetryAdapterConformanceCase> {
    vec![
        case(
            factory,
            "capabilities",
            "carries adapter capabilities through contexts and spans",
            capabilities,
        ),
        case(
            factory,
            "metrics",
            "records ordered measurements and optional histogram exemplars",
            ordered_measurements,
        ),
        case(
            factory,
            "metrics",
            "copies instrument metadata and measurement attributes",
            metadata_is_copied,
        ),
        case(
            factory,
            "links",
            "records initial and late links without changing parents",
            links_do_not_change_parents,
        ),
        case(
            factory,
            "links",
            "ignores links with a noop context",
            noop_links_are_ignored,
        ),
        case(
            factory,
            "propagation",
            "roundtrips parent identity without retaining the carrier",
            carrier_roundtrip,
        ),
        case(
            factory,
            "propagation",
            "absent and unknown carriers produce recorded roots",
            carrier_fallbacks,
        ),
        case(
            factory,
            "propagation",
            "propagates across fresh adapters and multiple hops",
            {
                let factory = Arc::clone(factory);
                move |fixture: &TelemetryAdapterFixture| multi_hop_propagation(&factory, fixture)
            },
        ),
        case(
            factory,
            "callback lifecycle",
            "admits once synchronously and settles as ok",
            callback_lifecycle,
        ),
        case(
            factory,
            "status",
            "uses last explicit status without automatic overwrite",
            explicit_status_wins,
        ),
        case(
            factory,
            "recording",
            "merges attributes and records ordered events",
            recording_merges_and_orders,
        ),
        case(
            factory,
            "parentage",
            "records nested child relationships",
            parentage,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// The shapes carry upstream's group/name metadata and every case passes
    /// against the ported in-memory adapter.
    #[test]
    fn conformance_cases_have_expected_shape_and_pass() {
        let factory = in_memory_adapter_factory();
        let cases = create_telemetry_adapter_conformance(&factory);
        assert_eq!(cases.len(), 12);
        let groups: BTreeSet<&str> = cases.iter().map(|item| item.group).collect();
        assert_eq!(
            groups,
            BTreeSet::from([
                "capabilities",
                "metrics",
                "links",
                "propagation",
                "callback lifecycle",
                "status",
                "recording",
                "parentage",
            ])
        );
        for item in &cases {
            assert!(!item.name.is_empty());
            item.run();
        }
    }
}
