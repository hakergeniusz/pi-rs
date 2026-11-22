//! Streaming progress updates for the in-process VM (`onUpdate` seam).
//!
//! Ground truth:
//! `packages/coding-agent/src/extensions/codemode/execute.ts:496-520` and
//! `:742-850`: upstream's tool passes an `onUpdate` callback into
//! `executeCodemode`, and `publish()` calls
//! `onUpdate?.({ content: [], details: snapshot() })` whenever a nested model
//! call starts or finishes. The worker launcher
//! (`packages/codemode/src/runtime/protocol.ts:43-60`) also relays every
//! `output` and `call` message to the host before the terminal `done`.
//!
//! The Rust VM is synchronous, so it cannot yield to an async UI thread. This
//! module defines an owned event and an `Arc<dyn Fn>` sink that the driver
//! calls at the same points: each output item, each call's start and finish,
//! and the terminal event. The host maps the events onto its own update
//! snapshots (see `WIRING.md`); no event depends on the VM's JS objects.

use std::sync::Arc;

use serde_json::Value as Json;

use crate::types::{CallStatus, OutputItem};

/// The script-visible target of a call (`protocol.ts:44`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamTarget {
    /// `tools.<name>`.
    Tool,
    /// A top-level global or namespace member.
    Global,
}

/// One progress event from a running script.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateEvent {
    /// The script produced an output item, in production order.
    Output(OutputItem),
    /// The script issued a call; the host may show a nested call row.
    CallStarted {
        /// The VM's call id.
        id: u32,
        /// Tool or global.
        target: StreamTarget,
        /// Raw target name.
        name: String,
        /// JSON arguments (array for spread globals).
        args: Json,
    },
    /// A call was answered. Globals are included for observers even though
    /// they are not recorded in `result.calls` (`types.ts:114-119`).
    CallFinished {
        /// The VM's call id.
        id: u32,
        /// Raw target name.
        name: String,
        /// Final status.
        status: CallStatus,
    },
    /// The VM settled (successfully or not).
    Finished {
        /// Whether the execution succeeded.
        ok: bool,
    },
}

/// Callback invoked synchronously from the VM driver. Keep it cheap: it runs
/// on the thread executing the script, before the next tool call is issued.
pub type UpdateSink = Arc<dyn Fn(UpdateEvent) + Send + Sync>;

/// A sink that ignores every event, for hosts that do not render progress.
#[must_use]
pub fn noop_update_sink() -> UpdateSink {
    Arc::new(|_event| {})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_sink_accepts_every_event() {
        let sink = noop_update_sink();
        sink(UpdateEvent::Output(OutputItem::text("hi")));
        sink(UpdateEvent::CallStarted {
            id: 1,
            target: StreamTarget::Tool,
            name: "double".to_string(),
            args: Json::Null,
        });
        sink(UpdateEvent::CallFinished {
            id: 1,
            name: "double".to_string(),
            status: CallStatus::Ok,
        });
        sink(UpdateEvent::Finished { ok: true });
    }

    #[test]
    fn events_are_owned_and_cloneable() {
        let event = UpdateEvent::CallStarted {
            id: 7,
            target: StreamTarget::Global,
            name: "models.classify".to_string(),
            args: serde_json::json!([1]),
        };
        assert_eq!(event.clone(), event);
    }
}
