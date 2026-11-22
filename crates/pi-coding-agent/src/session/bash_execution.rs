//! Incremental bash execution output deltas (`bash_execution_update`).
//!
//! Ground truth:
//! * `packages/coding-agent/src/core/agent-session.ts:251` — the
//!   `AgentSessionEvent` variant
//!   `{ type: "bash_execution_update"; id?: string; delta: string }`.
//! * `packages/coding-agent/src/core/agent-session.ts:4003-4035` —
//!   `executeBash` feeds every `onChunk(delta)` into
//!   `_emit({ type: "bash_execution_update", id: options?.id, delta })`.
//! * `packages/coding-agent/src/core/tools/bash.ts:289-364` — the tool's
//!   `onUpdate` throttling: an initial empty update, then snapshots of the
//!   accumulated output as the command streams.
//!
//! Upstream exposes command output *while* a bash tool call is running, not
//! just in the final `BashExecutionMessage`. The types here let a tool host
//! feed those deltas into the session layer with a JSON shape byte-identical
//! to the event pi emits, and accumulate them in arrival order into the output
//! the final message carries. The interactive `!` host is that tool host:
//! `modes::interactive::interactive_mode::emit_bash_execution_delta` pushes
//! each PTY chunk and emits the returned event on the extension/session event
//! stream before the chat row repaints.

use serde::{Deserialize, Serialize};

/// The `AgentSessionEvent` union restricted to the streaming-output variant
/// (`agent-session.ts:251`).
///
/// The internally-tagged representation serializes `type` first, then `id`
/// (omitted when absent), then `delta` — the exact object-literal order pi
/// emits. `serde`'s `snake_case` rename maps the variant name to
/// `"bash_execution_update"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BashExecutionEvent {
    /// `{ type: "bash_execution_update"; id?: string; delta: string }`.
    BashExecutionUpdate {
        /// Optional identifier correlating updates with one execution
        /// (`options.id` in `executeBash`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// The incremental output chunk (one `onChunk` / `onData` delivery).
        delta: String,
    },
}

impl BashExecutionEvent {
    /// Build a `bash_execution_update` event with the given correlation id and
    /// delta, mirroring `executeBash`'s `_emit` call.
    pub fn execution_update(id: Option<String>, delta: impl Into<String>) -> Self {
        Self::BashExecutionUpdate {
            id,
            delta: delta.into(),
        }
    }

    /// Borrow the event's `id`, if any.
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::BashExecutionUpdate { id, .. } => id.as_deref(),
        }
    }

    /// Borrow the event's delta.
    pub fn delta(&self) -> &str {
        match self {
            Self::BashExecutionUpdate { delta, .. } => delta,
        }
    }
}

/// Accumulates the ordered `onChunk` deltas of one bash execution.
///
/// Upstream's `executeBash` (`agent-session.ts:4022-4025`) emits one
/// `bash_execution_update` per delta and concatenates the deltas into the
/// output stored on the final `BashExecutionMessage`. A tool host feeds raw
/// deltas through [`BashExecutionDeltaCollector::push`]; each call returns the
/// event to publish and advances the accumulated output in arrival order.
///
/// This is a pure, transport-independent accumulator: it performs no I/O and
/// owns no provider state, so it can be tested with synthetic delta sequences.
#[derive(Debug, Clone)]
pub struct BashExecutionDeltaCollector {
    id: Option<String>,
    output: String,
    deltas: Vec<String>,
}

impl BashExecutionDeltaCollector {
    /// Start a collector for one execution, optionally tagged with `id`.
    pub fn new(id: Option<String>) -> Self {
        Self {
            id,
            output: String::new(),
            deltas: Vec::new(),
        }
    }

    /// Feed one delta chunk, returning the exact `bash_execution_update` event
    /// pi would emit for it. Deltas are recorded in call order.
    pub fn push(&mut self, delta: impl Into<String>) -> BashExecutionEvent {
        let delta = delta.into();
        self.deltas.push(delta.clone());
        self.output.push_str(&delta);
        BashExecutionEvent::execution_update(self.id.clone(), delta)
    }

    /// The correlation id all emitted events carry.
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// The concatenation of every delta fed so far, in order — the output the
    /// final `BashExecutionMessage` carries.
    pub fn output(&self) -> &str {
        &self.output
    }

    /// The fed deltas, in arrival order.
    pub fn deltas(&self) -> &[String] {
        &self.deltas
    }

    /// Number of deltas fed so far.
    pub fn len(&self) -> usize {
        self.deltas.len()
    }

    /// Whether no delta has been fed yet.
    pub fn is_empty(&self) -> bool {
        self.deltas.is_empty()
    }
}

impl Default for BashExecutionDeltaCollector {
    fn default() -> Self {
        Self::new(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_has_upstream_shape_and_field_order() {
        let with_id = BashExecutionEvent::execution_update(Some("call-7".into()), "hello\n");
        assert_eq!(
            serde_json::to_string(&with_id).unwrap(),
            r#"{"type":"bash_execution_update","id":"call-7","delta":"hello\n"}"#
        );

        let without_id = BashExecutionEvent::execution_update(None, "x");
        assert_eq!(
            serde_json::to_string(&without_id).unwrap(),
            r#"{"type":"bash_execution_update","delta":"x"}"#
        );
    }

    #[test]
    fn event_round_trips_through_json() {
        let event = BashExecutionEvent::execution_update(Some("id".into()), "a\tb\nc");
        let json = serde_json::to_string(&event).unwrap();
        let decoded: BashExecutionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, event);
        assert_eq!(decoded.id(), Some("id"));
        assert_eq!(decoded.delta(), "a\tb\nc");
    }

    #[test]
    fn synthetic_delta_sequence_preserves_order_and_accumulates() {
        let mut collector = BashExecutionDeltaCollector::new(Some("call-1".into()));
        assert!(collector.is_empty());
        assert_eq!(collector.output(), "");

        let chunks = ["first\n", "sec", "ond\n", "third"];
        let mut events = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let event = collector.push(*chunk);
            assert_eq!(event.id(), Some("call-1"));
            assert_eq!(event.delta(), *chunk);
            assert_eq!(collector.len(), index + 1);
            events.push(event);
        }

        assert_eq!(collector.deltas(), &chunks);
        assert_eq!(collector.output(), "first\nsecond\nthird");
        assert_eq!(events.len(), 4);
        assert_eq!(events[1].delta(), "sec");
        assert_eq!(events[2].delta(), "ond\n");
    }

    #[test]
    fn collector_without_id_emits_idless_events() {
        let mut collector = BashExecutionDeltaCollector::default();
        let event = collector.push("chunk");
        assert_eq!(event.id(), None);
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"type":"bash_execution_update","delta":"chunk"}"#
        );
        assert_eq!(collector.output(), "chunk");
    }
}
