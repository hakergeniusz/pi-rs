//! The interactive mode's `ctx.ui` bridge (E3 slice 6a): the dialog half of
//! upstream's `createExtensionUIContext` (`interactive-mode.ts:2763-2830`)
//! over the [`pi_ext_js::UiSink`] seam.
//!
//! Upstream runs extensions in-process, so `ctx.ui.select(...)` mounts a
//! dialog directly and the promise resolves from its callback. The port's
//! engine is out-of-process (see `docs/EXTENSIONS.md`), so the same
//! requests cross as JSON: the runner forwards every `ctx.ui` call to
//! `UiSink::request` and dialog calls block in `UiSink::wait_response`
//! (the response shapes are the runner's: `{value}`, `{confirmed}`,
//! `{cancelled:true}`, `{}`). This bridge is the interactive answer to
//! that seam — the transport twin of RPC mode's `RpcUiSink`
//! (`modes/rpc.rs:570-643`): instead of `extension_ui_request` lines, the
//! requests queue on a channel the mode's pump polls and each mounted
//! dialog answers through `respond`.
//!
//! The mode owns the dialog lifetime (mounting in the editor slot, the
//! countdown, the answer); the bridge only carries the crossing.
//! Fire-and-forget methods (`notify`, `setStatus`, `setWidget`, …) arrive
//! as ordinary requests without a waiter. One instance serves the whole
//! session — `main.rs` builds it before extensions load (dialogs can open
//! during `session_start`) and hands the same `Arc` to both sides.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, SyncSender};

use serde_json::Value;

/// The interactive `ctx.ui` transport. The runner-side half implements
/// [`pi_ext_js::UiSink`]; the mode-side half polls `try_request` and
/// answers through `respond`.
pub struct ExtensionUiBridge {
    /// The request queue; `request` pushes, `try_request` polls.
    requests: Mutex<Sender<Value>>,
    drain: Mutex<Receiver<Value>>,
    /// The mode's answer slot per dialog request, registered at request
    /// time so an answer can never race the runner's wait.
    pending: Mutex<HashMap<String, SyncSender<Value>>>,
    /// The runner's wait half per dialog request (`wait_response` takes
    /// and receives, like `RpcUiSink`).
    receivers: Mutex<HashMap<String, Receiver<Value>>>,
}

impl ExtensionUiBridge {
    pub fn new() -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        Self {
            requests: Mutex::new(sender),
            drain: Mutex::new(receiver),
            pending: Mutex::new(HashMap::new()),
            receivers: Mutex::new(HashMap::new()),
        }
    }

    /// Polls one queued `ctx.ui` request, if any — the mode's pump calls
    /// this per frame; nothing blocks.
    pub fn try_request(&self) -> Option<Value> {
        self.drain
            .lock()
            .expect("extension ui drain")
            .try_recv()
            .ok()
    }

    /// The mode's answer for the dialog request with this id. No-op when
    /// the request never waited (fire-and-forget methods) or the runner
    /// already gave up its wait.
    pub fn respond(&self, id: &str, response: &Value) {
        let sender = self
            .pending
            .lock()
            .expect("extension ui pending")
            .remove(id);
        if let Some(sender) = sender {
            let _ = sender.send(response.clone());
        }
    }
}

impl Default for ExtensionUiBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl pi_ext_js::UiSink for ExtensionUiBridge {
    /// Forwards one request toward the mode; dialog methods additionally
    /// register their answer slot (mirrors `RpcUiSink::request`, minus the
    /// wire write — the channel IS the wire). The answer-class overlay
    /// crossings (E3 slice 6c) wait the same way — the mode answers from
    /// its overlay registry like a dialog close.
    fn request(&self, request: &Value) {
        let id = request
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // Only methods with a wait can register a slot (`createDialogPromise`,
        // rpc-mode.ts:94-145, plus the overlay answer crossings); fire-and-
        // forget requests (`notify`, `setStatus`, the overlay writes, …)
        // register nothing.
        let has_wait = matches!(
            request.get("method").and_then(Value::as_str),
            Some(
                "select" | "confirm" | "input" | "editor"
                    | "overlayIsHidden" | "overlayIsFocused" | "overlayGetBounds"
            )
        );
        if has_wait {
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            self.pending
                .lock()
                .expect("extension ui pending")
                .insert(id.clone(), sender);
            self.receivers
                .lock()
                .expect("extension ui receivers")
                .insert(id, receiver);
        }
        let _ = self
            .requests
            .lock()
            .expect("extension ui requests")
            .send(request.clone());
    }

    /// Blocks until the mode answers the dialog with this id. The mode
    /// owns dialog timeouts (the mounted countdown cancels and answers),
    /// so this wait has none — matching upstream's TUI flow, where the
    /// dialog promise settles only through the dialog itself. A request
    /// that never registered (fire-and-forget) answers the method default
    /// (`Value::Null` — the glue parses it into that default).
    fn wait_response(&self, id: &str, _method: &str) -> Value {
        let receiver = self
            .receivers
            .lock()
            .expect("extension ui receivers")
            .remove(id);
        match receiver {
            Some(receiver) => receiver.recv().unwrap_or(Value::Null),
            None => Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ext_js::UiSink as _;

    /// A dialog request registers its slot; the mode's answer reaches the
    /// runner's wait, and a second answer is a no-op.
    #[test]
    fn dialog_answer_crosses_once() {
        let bridge = std::sync::Arc::new(ExtensionUiBridge::new());
        bridge.request(&serde_json::json!({
            "id": "ui-1", "method": "select", "title": "Pick", "options": ["A", "B"],
        }));
        // The request reached the mode's drain.
        let request = bridge.try_request().expect("request queued");
        assert_eq!(request["method"], "select");
        // The runner waits; the mode answers.
        let waiter = std::thread::spawn({
            let bridge = std::sync::Arc::clone(&bridge);
            move || bridge.wait_response("ui-1", "select")
        });
        bridge.respond("ui-1", &serde_json::json!({ "value": "A" }));
        let response = waiter.join().expect("wait");
        assert_eq!(response["value"], "A");
        // The slot is spent: a second answer is a no-op.
        bridge.respond("ui-1", &serde_json::json!({ "value": "B" }));
    }

    /// Fire-and-forget requests arrive without registering a waiter; the
    /// runner's wait answers the method default.
    #[test]
    fn fire_and_forget_never_waits() {
        let bridge = ExtensionUiBridge::new();
        bridge.request(&serde_json::json!({
            "id": "ui-2", "method": "notify", "message": "hello",
        }));
        let request = bridge.try_request().expect("request queued");
        assert_eq!(request["method"], "notify");
        // No slot: the wait falls straight through to the default.
        assert_eq!(bridge.wait_response("ui-2", "notify"), Value::Null);
    }
}
