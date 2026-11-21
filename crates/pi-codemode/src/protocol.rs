//! Messages exchanged between the host and a VM.
//!
//! Ground truth: `packages/codemode/src/runtime/protocol.ts:1-93`. Tool
//! arguments, results, and values cross as JSON strings; Rust's enum mirrors
//! the TypeScript discriminated unions. The worker-only fields (`wasm`,
//! `SharedArrayBuffer`) have no Rust counterpart because this port runs the
//! engine in-process, but the validation helpers are ported so a transport
//! can still use them.

use std::collections::BTreeMap;

use serde_json::Value as Json;

use crate::types::{CodemodeDuring, OutputItem};

/// JSON text of an error thrown by the script (`protocol.ts:40-41`).
pub type ScriptErrorJson = String;

/// One tool a VM needs to run a script (`protocol.ts:14-28`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolStartData {
    /// Raw tool name.
    pub name: String,
    /// Identifier the script uses.
    pub js_name: String,
    /// Text listed in `ALL_TOOLS`.
    pub description: String,
}

/// One global a VM needs to run a script (`protocol.ts:18`).
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalStartData {
    /// Global name, optionally `namespace.member`.
    pub name: String,
    /// Whether arguments cross as an array.
    pub spread: bool,
}

/// Deterministic-mode configuration (`protocol.ts:23-27`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeterministicData {
    /// VM clock start, in ms since the epoch.
    pub started_at: f64,
    /// Randomness seed.
    pub seed: u32,
}

/// Everything a VM needs to run one script (`protocol.ts:14-28`).
#[derive(Debug, Clone, PartialEq)]
pub struct VmStartData {
    /// Async-function body to evaluate.
    pub code: String,
    /// Tools the script can call.
    pub tools: Vec<ToolStartData>,
    /// Globals the script can call.
    pub globals: Vec<GlobalStartData>,
    /// QuickJS heap limit.
    pub memory_limit_bytes: Option<u64>,
    /// `load()` snapshot: key to JSON text.
    pub store: BTreeMap<String, String>,
    /// Deterministic-mode configuration.
    pub deterministic: Option<DeterministicData>,
}

impl VmStartData {
    /// Build start data from a script and its declared tools/globals.
    #[must_use]
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            tools: Vec::new(),
            globals: Vec::new(),
            memory_limit_bytes: None,
            store: BTreeMap::new(),
            deterministic: None,
        }
    }
}

/// A message from the VM to the host (`protocol.ts:43-60`).
#[derive(Debug, Clone, PartialEq)]
pub enum WorkerToHostMessage {
    /// The script called a tool or a global.
    Call {
        /// VM's call id.
        id: u32,
        /// Target kind.
        target: CallTargetKind,
        /// Raw name.
        name: String,
        /// JSON arguments, or `None` for no arguments.
        args: Option<String>,
    },
    /// The script produced an output item.
    Output {
        /// The item.
        item: OutputItem,
    },
    /// The script settled successfully.
    DoneOk {
        /// JSON value text, or `None` for `undefined`.
        value: Option<String>,
        /// JSON array of store writes.
        writes: String,
        /// Deterministic-mode progress.
        during: Option<CodemodeDuring>,
    },
    /// The script threw.
    DoneError {
        /// JSON `{ name?, message, stack? }`.
        error: ScriptErrorJson,
        /// Deterministic-mode progress.
        during: Option<CodemodeDuring>,
    },
    /// The VM failed outside the script's control.
    Crash {
        /// Engine error text.
        message: String,
    },
    /// Deterministic mode: fingerprint before the script ran.
    Started {
        /// Fingerprint.
        fingerprint: String,
    },
    /// Deterministic mode: the VM is waiting again.
    Idle {
        /// Result number just processed.
        seq: u32,
        /// Total calls issued so far.
        issued: u32,
    },
}

/// Kind of a call's target, as it crosses the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallTargetKind {
    /// `tools.<name>`.
    Tool,
    /// A top-level global or namespace member.
    Global,
}

impl CallTargetKind {
    /// The wire value used by the protocol.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Global => "global",
        }
    }
}

/// The host's answer to one call (`protocol.ts:62-76`).
#[derive(Debug, Clone, PartialEq)]
pub struct HostToWorkerMessage {
    /// Call id.
    pub id: u32,
    /// Whether the call resolved.
    pub ok: bool,
    /// JSON result text or error message.
    pub payload: Option<String>,
    /// Deterministic-mode result number.
    pub seq: Option<u32>,
    /// Deterministic-mode clock reading.
    pub now: Option<f64>,
    /// JSON object of extra error properties.
    pub properties: Option<String>,
}

impl HostToWorkerMessage {
    /// Build a reply without deterministic-mode fields.
    #[must_use]
    pub fn new(id: u32, ok: bool, payload: Option<String>) -> Self {
        Self {
            id,
            ok,
            payload,
            seq: None,
            now: None,
            properties: None,
        }
    }
}

/// Whether a JSON value looks like a VM→host message (`protocol.ts:78-89`).
#[must_use]
pub fn is_worker_to_host_message(value: &Json) -> bool {
    value
        .as_object()
        .and_then(|object| object.get("type"))
        .and_then(Json::as_str)
        .is_some_and(|kind| {
            matches!(
                kind,
                "call" | "output" | "done" | "crash" | "started" | "idle"
            )
        })
}

/// Whether a JSON value looks like a host→VM message (`protocol.ts:91-93`).
#[must_use]
pub fn is_host_to_worker_message(value: &Json) -> bool {
    value
        .as_object()
        .and_then(|object| object.get("type"))
        .and_then(Json::as_str)
        == Some("result")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_worker_messages() {
        for kind in ["call", "output", "done", "crash", "started", "idle"] {
            assert!(is_worker_to_host_message(&json!({"type": kind})));
        }
        assert!(!is_worker_to_host_message(&json!({"type": "result"})));
        assert!(!is_worker_to_host_message(&json!(null)));
        assert!(!is_worker_to_host_message(&Json::String(
            "done".to_string()
        )));
    }

    #[test]
    fn validates_host_messages() {
        assert!(is_host_to_worker_message(
            &json!({"type": "result", "id": 1})
        ));
        assert!(!is_host_to_worker_message(&json!({"type": "call"})));
        assert!(!is_host_to_worker_message(&json!([])));
    }

    #[test]
    fn call_target_wire_names_match_upstream() {
        assert_eq!(CallTargetKind::Tool.as_str(), "tool");
        assert_eq!(CallTargetKind::Global.as_str(), "global");
    }

    #[test]
    fn start_data_defaults_match_upstream() {
        let data = VmStartData::new("return 1;");
        assert_eq!(data.code, "return 1;");
        assert_eq!(data.tools, vec![]);
        assert!(data.memory_limit_bytes.is_none());
        assert!(data.deterministic.is_none());
    }
}
