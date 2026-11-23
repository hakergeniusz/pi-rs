//! Sample native extension (`cdylib`) for the pi-rs ABI v1.
//!
//! This is a reference implementation and a test fixture: it exports every
//! required symbol, declares two tools, subscribes to hooks, and proves that
//! a tool call and a hook dispatch cross the `C` boundary into native code.
//! It is additive pi-rs functionality; upstream pi has no native host.
//!
//! The crate shares [`pi_native_ext::abi`] types with the host at compile
//! time, but nothing Rust-shaped crosses the boundary at runtime: only C
//! strings carrying `JSON`, which keeps the ABI stable across compiler
//! versions.

use std::ffi::{CStr, CString, c_char};
use std::fs;
use std::sync::Mutex;

use pi_native_ext::abi::{
    ABI_VERSION, CallRequest, CallResult, DeclaredTool, HOOK_MESSAGE, HOOK_SESSION_START,
    HOOK_TOOL_RESULT, HOOK_TRANSFORM_PROMPT, InitConfig, InitResult,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

static SHUTDOWN_MARKER: Mutex<Option<String>> = Mutex::new(None);

/// Required ABI symbol: the version this extension speaks.
#[unsafe(no_mangle)]
pub extern "C" fn pi_ext_abi_version() -> u32 {
    ABI_VERSION
}

/// Required ABI symbol: declare two tools and four hooks.
#[unsafe(no_mangle)]
pub extern "C" fn pi_ext_init(config_json: *const c_char) -> *mut c_char {
    let config = match parse::<InitConfig>(config_json) {
        Ok(config) => config,
        Err(error) => {
            return to_c_json(&InitResult {
                ok: false,
                tools: Vec::new(),
                hooks: Vec::new(),
                error: Some(error),
            });
        }
    };
    if let Ok(mut marker) = SHUTDOWN_MARKER.lock() {
        marker.clone_from(&config.shutdown_marker);
    }
    let echo = DeclaredTool {
        name: "native_echo".to_string(),
        description: "Echo text through the native sample extension".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "required": ["text"],
            "properties": { "text": { "type": "string", "description": "Text to echo" } }
        }),
    };
    let add = DeclaredTool {
        name: "native_add".to_string(),
        description: "Add two integers in native code".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "required": ["a", "b"],
            "properties": {
                "a": { "type": "integer" },
                "b": { "type": "integer" }
            }
        }),
    };
    to_c_json(&InitResult {
        ok: true,
        tools: vec![echo, add],
        hooks: vec![
            HOOK_SESSION_START.to_string(),
            HOOK_MESSAGE.to_string(),
            HOOK_TOOL_RESULT.to_string(),
            HOOK_TRANSFORM_PROMPT.to_string(),
        ],
        error: None,
    })
}

/// Required ABI symbol: JSON-in / JSON-out dispatch.
#[unsafe(no_mangle)]
pub extern "C" fn pi_ext_call(method_json: *const c_char) -> *mut c_char {
    let result = match parse::<CallRequest>(method_json) {
        Ok(request) => dispatch(&request),
        Err(error) => CallResult::err(error),
    };
    to_c_json(&result)
}

/// Required ABI symbol: free a string this library allocated.
///
/// # Safety
///
/// `ptr` must be non-null and must have been returned by `to_c_json` in this
/// library and not freed before. The host upholds this by calling this exact
/// function once for each pointer it receives.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pi_ext_free_string(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: `ptr` was produced by `CString::into_raw` in `to_c_json` in this
    // same library. The host calls this matching deallocator exactly once.
    unsafe { drop(CString::from_raw(ptr)) };
}

/// Optional ABI symbol: touch the configured marker so tests can prove the
/// shutdown notification reached native code.
#[unsafe(no_mangle)]
pub extern "C" fn pi_ext_shutdown() {
    let marker = SHUTDOWN_MARKER
        .lock()
        .ok()
        .and_then(|mut guard| guard.take());
    if let Some(path) = marker {
        let _ = fs::write(path, b"native shutdown reached\n");
    }
}

fn dispatch(request: &CallRequest) -> CallResult {
    match request {
        CallRequest::Ping => CallResult::ok(serde_json::json!({ "pong": true })),
        CallRequest::Tool { name, arguments } => match name.as_str() {
            "native_echo" => {
                let text = arguments
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                CallResult::ok(serde_json::json!({ "content": format!("native:{text}") }))
            }
            "native_add" => {
                let a = arguments
                    .get("a")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                let b = arguments
                    .get("b")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                let sum = a + b;
                CallResult::ok(serde_json::json!({
                    "content": sum.to_string(),
                    "structuredContent": { "sum": sum }
                }))
            }
            other => CallResult::err(format!("unknown native tool: {other}")),
        },
        CallRequest::Hook { hook, payload } => match hook.as_str() {
            HOOK_SESSION_START => CallResult::ok(serde_json::json!({ "started": true })),
            HOOK_MESSAGE | HOOK_TOOL_RESULT => {
                CallResult::ok(serde_json::json!({ "handled": hook }))
            }
            HOOK_TRANSFORM_PROMPT => {
                let prompt = payload
                    .get("prompt")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                CallResult::ok(serde_json::json!({
                    "prompt": format!("{prompt}\n\n[native hook] sample extension was here")
                }))
            }
            other => CallResult::err(format!("unknown hook: {other}")),
        },
    }
}

fn parse<T: DeserializeOwned>(raw: *const c_char) -> Result<T, String> {
    if raw.is_null() {
        return Err("host passed a null JSON pointer".to_string());
    }
    // SAFETY: the host passes a live NUL-terminated UTF-8 `C` string for the
    // duration of the call, as required by the ABI, and reads nothing after
    // returning.
    let text = unsafe { CStr::from_ptr(raw) }
        .to_str()
        .map_err(|error| format!("invalid UTF-8: {error}"))?;
    serde_json::from_str(text).map_err(|error| format!("invalid JSON: {error}"))
}

fn to_c_json<T: Serialize>(value: &T) -> *mut c_char {
    serde_json::to_string(value)
        .ok()
        .and_then(|json| CString::new(json).ok())
        .map_or(std::ptr::null_mut(), CString::into_raw)
}
