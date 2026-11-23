//! The native extension C ABI, version 1.
//!
//! Upstream pi (`earendil-works/pi` 1.5.2) has **no** native extension host:
//! its extension system is JavaScript/TypeScript only (`extensions/index.ts`,
//! `extensions/runner.ts`). Everything in this module is an additive pi-rs
//! capability, not a port of an upstream file.
//!
//! # Why a C ABI and `JSON` strings
//!
//! Rust has no stable application binary interface. Passing `String`,
//! `serde_json::Value`, or a Rust trait object across a dynamic-library
//! boundary would be undefined behaviour the moment the extension and the
//! host are built with different `rustc` versions or crate versions. The
//! stable boundary here is deliberately tiny:
//!
//! * symbol names are plain C symbols (`#[no_mangle] extern "C"`);
//! * the only scalar is a `u32` version;
//! * every structured value is a NUL-terminated UTF-8 `JSON` `C` string;
//! * the extension owns returned strings and frees them through its own
//!   exported `pi_ext_free_string`, so allocator choice never crosses the
//!   boundary.
//!
//! Adding a field is a compatible change (all structs use `#[serde(default)]`
//! where it matters). Changing a symbol's signature or removing a required
//! symbol requires bumping [`ABI_VERSION`].
//!
//! # Symbols
//!
//! | Symbol | Signature | Required |
//! |---|---|---|
//! | `pi_ext_abi_version` | `fn() -> u32` | yes |
//! | `pi_ext_init` | `fn(*const c_char) -> *mut c_char` | yes |
//! | `pi_ext_call` | `fn(*const c_char) -> *mut c_char` | yes |
//! | `pi_ext_free_string` | `fn(*mut c_char)` | yes |
//! | `pi_ext_shutdown` | `fn()` | no |

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The ABI version both host and extension must agree on.
pub const ABI_VERSION: u32 = 1;

/// Symbol: `fn() -> u32`, returns [`ABI_VERSION`].
pub const SYMBOL_ABI_VERSION: &str = "pi_ext_abi_version";
/// Symbol: `fn(*const c_char) -> *mut c_char`, takes an [`InitConfig`].
pub const SYMBOL_INIT: &str = "pi_ext_init";
/// Symbol: `fn(*const c_char) -> *mut c_char`, takes a [`CallRequest`].
pub const SYMBOL_CALL: &str = "pi_ext_call";
/// Symbol: `fn()`, invoked once before the library is unloaded.
pub const SYMBOL_SHUTDOWN: &str = "pi_ext_shutdown";
/// Symbol: `fn(*mut c_char)`, frees a string the extension returned.
pub const SYMBOL_FREE_STRING: &str = "pi_ext_free_string";

/// Hook name: emitted once when a runtime finishes assembling.
pub const HOOK_SESSION_START: &str = "on_session_start";
/// Hook name: every user message; may return a replacement payload.
pub const HOOK_MESSAGE: &str = "on_message";
/// Hook name: every completed tool call; may return a replacement result.
pub const HOOK_TOOL_RESULT: &str = "on_tool_result";
/// Hook name: transforms the system prompt before the first model call.
pub const HOOK_TRANSFORM_PROMPT: &str = "transform_prompt";

/// The `JSON` object passed to `pi_ext_init`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InitConfig {
    /// Extension name (the file stem), used in diagnostics.
    pub name: String,
    /// Extension version, for logging and diagnostics.
    pub version: String,
    /// Current working directory of the host process.
    pub cwd: String,
    /// Optional diagnostic path the extension may touch on shutdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_marker: Option<String>,
}

/// A tool an extension declares during [`InitConfig`] handling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeclaredTool {
    /// The tool name the model calls and the host dispatches on.
    pub name: String,
    /// Description shown to the model.
    pub description: String,
    /// `JSON` Schema for the arguments.
    pub parameters: Value,
}

/// The `JSON` object returned by `pi_ext_init`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InitResult {
    /// `false` aborts the load; [`Self::error`] must then be populated.
    pub ok: bool,
    /// Tools this extension wants registered, in declaration order.
    #[serde(default)]
    pub tools: Vec<DeclaredTool>,
    /// Hook names this extension subscribes to.
    #[serde(default)]
    pub hooks: Vec<String>,
    /// Human-readable failure reason when [`Self::ok`] is `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The `JSON` object passed to `pi_ext_call`.
///
/// Serializes to a tagged object such as
/// `{"method":"tool","name":"native_echo","arguments":{"text":"hi"}}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum CallRequest {
    /// Invoke a tool declared during init.
    Tool {
        /// Declared tool name.
        name: String,
        /// Validated argument object.
        arguments: Value,
    },
    /// Deliver a hook event.
    Hook {
        /// One of the `HOOK_*` constants (or an extension-private name).
        hook: String,
        /// Event payload; a `transform_prompt` payload carries `prompt`.
        payload: Value,
    },
    /// Liveness probe; extensions should answer `{"pong": true}`.
    Ping,
}

/// The `JSON` object returned by `pi_ext_call`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CallResult {
    /// `false` turns the call into a tool error / dropped hook result.
    pub ok: bool,
    /// The call's payload. Tool results follow pi's `ToolResult` shape
    /// (`content`, optional `details` / `structuredContent` / `isError`);
    /// hooks may echo a replacement `prompt`, or any other shape the caller
    /// understands.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub result: Value,
    /// Human-readable failure reason when [`Self::ok`] is `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CallResult {
    /// A successful result carrying `result`.
    #[must_use]
    pub fn ok(result: Value) -> Self {
        Self {
            ok: true,
            result,
            error: None,
        }
    }

    /// A failed result carrying `error`.
    #[must_use]
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            result: Value::Null,
            error: Some(message.into()),
        }
    }
}
