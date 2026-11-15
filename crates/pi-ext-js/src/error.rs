//! Error types for the JS extensions host.
//!
//! Mirrors the two upstream error channels: load errors are *values*
//! (`LoadExtensionsResult.errors`, `loader.ts:646-668, 712-715`), and
//! runtime handler errors fan out through `ExtensionError` listeners
//! (`runner.ts:744-753`).

use serde::{Deserialize, Serialize};

/// Errors raised by the embedded engine itself.
#[derive(Debug, thiserror::Error)]
pub enum JsHostError {
    /// The engine failed to compile, evaluate, or call into user code.
    #[error("javascript engine error: {0}")]
    Js(#[from] rquickjs::Error),
    /// A JavaScript exception with its message caught from the engine.
    #[error("{0}")]
    JsException(String),
    /// A JSON round-trip failure inside the runner.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// The loaded module has no default export to invoke as the factory.
    #[error("extension does not export a valid factory function: {0}")]
    MissingFactory(String),
    /// A pending engine job threw while the host drained it.
    #[error("javascript job error: {0}")]
    Job(String),
    /// A handler did not settle while the host drained the job queue.
    #[error("handler did not complete: {0}")]
    HandlerDidNotComplete(String),
}

/// A runtime error from one extension handler, as upstream reports it to
/// error listeners (`ExtensionError`, `runner.ts:1119-1127`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionError {
    /// Path of the extension whose handler failed.
    #[serde(rename = "extensionPath")]
    pub extension_path: String,
    /// Event type the handler was dispatched for.
    pub event: String,
    /// Handler error message (`error.message` or `String(error)`).
    pub error: String,
    /// Handler error stack when the throw was an `Error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

/// One load failure: the path and the raw failure message. The host
/// formats the diagnostic (`Failed to load extension "{path}": {error}`,
/// `main.ts:861-864`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    /// The path (or synthetic `builtin:<name>` path) that failed.
    pub path: String,
    /// The raw failure message.
    pub error: String,
}
