//! Data types of the codemode bridge.
//!
//! Ground truth: `packages/codemode/src/types.ts:1-254`. Every structure
//! upstream exchanges with a script has a Rust counterpart here; values that
//! cross the JS bridge are represented with `serde_json::Value` (the Rust
//! analogue of upstream's JSON round trip). The transport-specific option
//! fields (`wasm`, `workerUrl`, `acquireWorker`, `onWorkerStarted`) are omitted
//! because this port runs the VM in-process; see `WIRING.md`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// A JSON-serializable value crossing the sandbox boundary.
pub type CodemodeJsonSchema = Json;

/// Cooperative abort flag handed to a tool call.
///
/// Upstream passes an `AbortSignal` (`types.ts:3-9`); the in-process VM has a
/// single synchronous call stack, so the signal is a shared atomic flag the
/// host can set from another thread or from a nested tool call.
#[derive(Debug, Clone, Default)]
pub struct AbortFlag(Arc<AtomicBool>);

impl AbortFlag {
    /// Create a flag that has not been aborted.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the execution this flag belongs to has been aborted.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Signal an abort. Idempotent.
    pub fn abort(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Context a script's tool call receives (`types.ts:3-9`).
#[derive(Debug, Clone, Default)]
pub struct ToolContext {
    /// Aborted when the script finishes, times out, is aborted, or the sandbox closes.
    pub signal: AbortFlag,
}

impl ToolContext {
    /// Convenience check mirroring `context.signal.aborted`.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.signal.is_aborted()
    }
}

/// One item of script output, in production order (`types.ts:47-49`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputItem {
    /// `text()` / `console.*` output.
    Text {
        /// The rendered text.
        text: String,
        /// Set by `console.*` rather than `text()` / the return value.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        console: bool,
    },
    /// `image()` output (`data` is base64).
    Image {
        /// Base64-encoded image bytes.
        data: String,
        /// Detected MIME type.
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

impl OutputItem {
    /// Construct a non-console text item.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            console: false,
        }
    }

    /// Construct a console text item.
    #[must_use]
    pub fn console(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            console: true,
        }
    }

    /// Construct an image item.
    #[must_use]
    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Image {
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }
}

/// Status of one recorded tool call (`types.ts:51-57`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallStatus {
    /// The call resolved.
    Ok,
    /// The tool threw.
    Error,
    /// The script ended before the call was answered.
    Cancelled,
}

/// A tool call the script made (`types.ts:53-57`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Call {
    /// Raw tool name.
    pub name: String,
    /// Final status of the call.
    pub status: CallStatus,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: f64,
}

/// Why an execution failed (`types.ts:59-73`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeErrorKind {
    /// The script threw or failed to parse.
    Script,
    /// The deadline expired or the VM's interrupt budget was exhausted.
    Timeout,
    /// The caller's signal fired or the sandbox was closed.
    Aborted,
    /// The engine failed outside the script's control.
    Sandbox,
}

/// A codemode failure (`types.ts:75-80`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodemodeError {
    /// Category of the failure.
    pub kind: CodemodeErrorKind,
    /// Error name reported by the script, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Human-readable message.
    pub message: String,
    /// Stack reported by the script, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

impl CodemodeError {
    /// Construct an error of a given kind with no script name or stack.
    #[must_use]
    pub fn new(kind: CodemodeErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            name: None,
            message: message.into(),
            stack: None,
        }
    }

    /// Construct a host-side sandbox error.
    #[must_use]
    pub fn sandbox(message: impl Into<String>) -> Self {
        Self::new(CodemodeErrorKind::Sandbox, message)
    }
}

impl std::fmt::Display for CodemodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.name {
            Some(name) => write!(f, "{}: {}", name, self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for CodemodeError {}

/// Keys a script changed with `store()` (`types.ts:82-87`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CodemodeStoreWrites {
    /// Keys written to a JSON value.
    pub set: std::collections::BTreeMap<String, Json>,
    /// Keys stored as `undefined` (deleted).
    pub delete: Vec<String>,
}

/// Which result the VM was processing when a deterministic run ended (`types.ts:200-204`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodemodeDuring {
    /// Result number (`0` for the script's first run).
    pub seq: u32,
    /// Calls issued by then.
    pub issued: u32,
}

/// The outcome of one script execution (`types.ts:89-109`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeResult {
    /// The script settled successfully.
    Ok {
        /// The script's `return` value, or `null` when it returned nothing.
        value: Json,
        /// Everything the script printed.
        output: Vec<OutputItem>,
        /// Calls the script made, in order.
        calls: Vec<Call>,
        /// Store keys the script wrote.
        store_writes: CodemodeStoreWrites,
        /// Deterministic-mode progress, when applicable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        during: Option<CodemodeDuring>,
    },
    /// The script, deadline, or sandbox failed.
    Err {
        /// Why it failed.
        error: CodemodeError,
        /// Output produced before the failure.
        output: Vec<OutputItem>,
        /// Calls made before the failure.
        calls: Vec<Call>,
        /// Deterministic-mode progress, when applicable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        during: Option<CodemodeDuring>,
    },
}

impl CodemodeResult {
    /// Whether the script succeeded.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }

    /// The output produced before the script ended (available on both outcomes).
    #[must_use]
    pub fn output(&self) -> &[OutputItem] {
        match self {
            Self::Ok { output, .. } | Self::Err { output, .. } => output,
        }
    }

    /// The recorded calls (available on both outcomes).
    #[must_use]
    pub fn calls(&self) -> &[Call] {
        match self {
            Self::Ok { calls, .. } | Self::Err { calls, .. } => calls,
        }
    }

    /// The failure, if any.
    #[must_use]
    pub fn error(&self) -> Option<&CodemodeError> {
        match self {
            Self::Ok { .. } => None,
            Self::Err { error, .. } => Some(error),
        }
    }
}

/// Per-execution options (`types.ts:245-254`).
#[derive(Debug, Clone, Default)]
pub struct ExecuteOptions {
    /// Values the script reads with `load(key)`.
    pub store: Option<std::collections::BTreeMap<String, Json>>,
    /// Overrides the sandbox's default deadline.
    pub timeout_ms: Option<u64>,
    /// Aborts the execution when set (or when `abort()` is called).
    pub signal: Option<AbortFlag>,
}

/// A tool as a deterministic run's script sees it (`types.ts:175-184`).
#[derive(Debug, Clone, PartialEq)]
pub struct CodemodeDeclaredTool {
    /// Raw tool name.
    pub name: String,
    /// Identifier the script uses; defaults to `to_codemode_identifier(name)`.
    pub js_name: Option<String>,
    /// Text listed in `ALL_TOOLS`.
    pub description: Option<String>,
}

/// A global as a deterministic run's script sees it (`types.ts:186-190`).
#[derive(Debug, Clone, PartialEq)]
pub struct CodemodeDeclaredGlobal {
    /// Global name, optionally `namespace.member`.
    pub name: String,
    /// Whether all call arguments cross as an array.
    pub spread: bool,
}

/// A call a deterministic run's script made (`types.ts:165-173`).
#[derive(Debug, Clone, PartialEq)]
pub struct CodemodeHostCall {
    /// The VM's own call id.
    pub id: u32,
    /// Whether the call targets a tool or a global.
    pub target: CallTarget,
    /// Raw name.
    pub name: String,
    /// JSON arguments (an array for spread globals).
    pub args: Json,
}

/// Which prelude function a call targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallTarget {
    /// `tools.<name>`.
    Tool,
    /// A top-level global or namespace member.
    Global,
}

/// The host's answer to a deterministic call (`types.ts:192-198`).
#[derive(Debug, Clone, PartialEq)]
pub enum CodemodeOutcome {
    /// The call resolved with a JSON value.
    Ok {
        /// Value handed back to the script.
        value: Json,
    },
    /// The call rejected with an `Error`.
    Error {
        /// Error message.
        message: String,
        /// Extra own properties attached to the script's `Error`.
        properties: Option<std::collections::BTreeMap<String, Json>>,
    },
}

/// Options of a deterministic run (`types.ts:206-226`). Kept so downstream
/// code compiles; the in-process VM does not expose `start()` (see `WIRING.md`).
#[derive(Debug, Clone, Default)]
pub struct CodemodeRunOptions {
    /// VM clock start, in ms since the epoch.
    pub started_at: f64,
    /// Seed for `Math.random()` and the VM's other randomness.
    pub seed: u32,
    /// Tools declared for this run.
    pub tools: Vec<CodemodeDeclaredTool>,
    /// Globals declared for this run.
    pub globals: Vec<CodemodeDeclaredGlobal>,
    /// `load()` snapshot.
    pub store: Option<std::collections::BTreeMap<String, Json>>,
    /// Abort signal.
    pub signal: Option<AbortFlag>,
    /// Deadline of this run.
    pub timeout_ms: Option<f64>,
}

/// A running deterministic execution (`types.ts:228-243`). Kept for API
/// parity; constructed only by hosts implementing a replay transport.
#[derive(Clone)]
pub struct CodemodeRun {
    /// The run's result.
    pub result: Arc<dyn std::future::Future<Output = CodemodeResult> + Send + Sync>,
    /// Whether the outcome has been decided.
    pub finished: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abort_flag_is_idempotent() {
        let flag = AbortFlag::new();
        assert!(!flag.is_aborted());
        flag.abort();
        flag.abort();
        assert!(flag.is_aborted());
        assert!(ToolContext { signal: flag }.is_aborted());
    }

    #[test]
    fn output_item_json_uses_camel_case_mime_type() {
        let item = OutputItem::image("AAAA", "image/png");
        let json = serde_json::to_string(&item).expect("serialize");
        assert_eq!(json, r#"{"image":{"data":"AAAA","mimeType":"image/png"}}"#);
    }

    #[test]
    fn result_accessors_cover_both_variants() {
        let ok = CodemodeResult::Ok {
            value: Json::Null,
            output: vec![OutputItem::text("hi")],
            calls: vec![],
            store_writes: CodemodeStoreWrites::default(),
            during: None,
        };
        assert!(ok.is_ok());
        assert_eq!(ok.output().len(), 1);
        assert!(ok.error().is_none());

        let err = CodemodeResult::Err {
            error: CodemodeError::sandbox("boom"),
            output: vec![],
            calls: vec![],
            during: None,
        };
        assert!(!err.is_ok());
        assert_eq!(err.error().map_or("", |e| e.message.as_str()), "boom");
    }
}
