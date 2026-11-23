//! Integration coverage for the native extension ABI and loader.
//!
//! The cdylib test really compiles the sample extension with `cargo build`
//! into a private target directory, dlopens it, and checks that a tool call,
//! a hook dispatch, and shutdown all reach native code. It proves the C ABI +
//! `JSON` boundary end to end; it does not prove rendering or mode wiring,
//! which live in the host crate.

use std::path::{Path, PathBuf};
use std::process::Command;

use pi_native_ext::abi::{
    ABI_VERSION, HOOK_MESSAGE, HOOK_SESSION_START, HOOK_TOOL_RESULT, HOOK_TRANSFORM_PROMPT,
};
use pi_native_ext::{
    CallRequest, CallResult, DeclaredTool, ExtensionSet, InitConfig, InitResult, LoadSpec,
    NativeExtension,
};
use serde_json::{Value, json};

fn config(marker: Option<&Path>) -> InitConfig {
    InitConfig {
        name: "sample".to_string(),
        version: "0.1.0".to_string(),
        cwd: std::env::current_dir()
            .expect("cwd")
            .to_string_lossy()
            .into_owned(),
        shutdown_marker: marker.map(|path| path.to_string_lossy().into_owned()),
    }
}

#[test]
fn abi_payloads_round_trip() {
    let request = CallRequest::Tool {
        name: "native_echo".to_string(),
        arguments: json!({ "text": "hi" }),
    };
    let encoded = serde_json::to_string(&request).expect("serialize request");
    assert_eq!(
        encoded,
        r#"{"method":"tool","name":"native_echo","arguments":{"text":"hi"}}"#
    );
    let decoded: CallRequest = serde_json::from_str(&encoded).expect("decode request");
    assert_eq!(decoded, request);

    let result = CallResult::ok(json!({ "text": "native:hi" }));
    let encoded = serde_json::to_string(&result).expect("serialize result");
    let decoded: CallResult = serde_json::from_str(&encoded).expect("decode result");
    assert_eq!(decoded, result);
    assert_eq!(ABI_VERSION, 1);
}

struct TestExtension;

impl NativeExtension for TestExtension {
    fn name(&self) -> &'static str {
        "in-tree"
    }

    fn init(&self, _config: &InitConfig) -> InitResult {
        InitResult {
            ok: true,
            tools: vec![DeclaredTool {
                name: "in_tree_echo".to_string(),
                description: "In-tree echo".to_string(),
                parameters: json!({ "type": "object" }),
            }],
            hooks: vec![HOOK_MESSAGE.to_string()],
            error: None,
        }
    }

    fn call(&self, request: &CallRequest) -> Result<CallResult, String> {
        match request {
            CallRequest::Tool { name, arguments } if name == "in_tree_echo" => {
                let text = arguments.get("text").and_then(Value::as_str).unwrap_or("");
                Ok(CallResult::ok(json!({ "text": format!("in-tree:{text}") })))
            }
            CallRequest::Hook { hook, .. } if hook == HOOK_MESSAGE => {
                Ok(CallResult::ok(json!({ "handled": true })))
            }
            other => Err(format!("unexpected call: {other:?}")),
        }
    }
}

#[test]
fn in_tree_extension_uses_the_same_dispatch_surface() {
    let mut set = ExtensionSet::new();
    set.add_in_tree(Box::new(TestExtension), &config(None))
        .expect("register in-tree extension");
    assert_eq!(set.tools().len(), 1);
    assert!(set.declares_hook(HOOK_MESSAGE));
    assert!(!set.declares_hook(HOOK_TOOL_RESULT));

    let result = set
        .call_tool("in_tree_echo", &json!({ "text": "hello" }))
        .expect("call in-tree tool");
    assert_eq!(result.result["text"].as_str(), Some("in-tree:hello"));

    let hook = set.dispatch_hook(HOOK_MESSAGE, &json!({ "text": "hi" }));
    assert_eq!(hook.len(), 1);
    assert_eq!(hook[0].result["handled"].as_bool(), Some(true));
    assert_eq!(set.dispatch_hook(HOOK_TOOL_RESULT, &json!({})), Vec::new());
    set.shutdown();
}

/// Build the sample `cdylib` once into a private target dir. Using a separate
/// target dir avoids contending with the outer `cargo test` build lock.
fn sample_library() -> PathBuf {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives two levels under the workspace root")
        .to_path_buf();
    // `/var/tmp` is preferred where available: it survives reboots and is
    // not the quota-blocked `/tmp` on constrained build hosts.
    let temp_root = if Path::new("/var/tmp").is_dir() {
        PathBuf::from("/var/tmp")
    } else {
        std::env::temp_dir()
    };
    let target = temp_root.join("pi-native-ext-sample-target");
    let name = format!(
        "libpi_native_ext_sample_extension.{}",
        std::env::consts::DLL_EXTENSION
    );
    let library = target.join("debug").join(name);
    if !library.exists() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let status = Command::new(cargo)
            .args(["build", "--offline", "--jobs", "1", "--manifest-path"])
            .arg(workspace.join("Cargo.toml"))
            .args(["-p", "pi-native-ext-sample-extension", "--target-dir"])
            .arg(&target)
            .env("CARGO_NET_OFFLINE", "true")
            .status()
            .expect("spawn nested cargo build for the sample extension");
        assert!(status.success(), "sample extension cdylib failed to build");
    }
    assert!(
        library.exists(),
        "sample cdylib missing at {}",
        library.display()
    );
    library
}

#[test]
fn sample_cdylib_receives_tool_calls_hooks_and_shutdown() {
    let library = sample_library();
    let temp = tempfile::tempdir().expect("temp dir");
    let marker = temp.path().join("shutdown.marker");
    let spec = LoadSpec::from_path(&library);
    assert_eq!(spec.name, "pi_native_ext_sample_extension");

    let mut set = ExtensionSet::new();
    set.add_library(&spec, &config(Some(&marker)))
        .expect("load sample cdylib");
    assert!(!set.is_empty());
    let names: Vec<&str> = set.tools().iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, ["native_echo", "native_add"]);
    assert!(set.declares_hook(HOOK_SESSION_START));
    assert!(set.declares_hook(HOOK_TRANSFORM_PROMPT));

    // A tool call crosses the C boundary into native Rust code.
    let echo = set
        .call_tool("native_echo", &json!({ "text": "ping" }))
        .expect("native_echo");
    assert_eq!(echo.result["content"].as_str(), Some("native:ping"));
    let add = set
        .call_tool("native_add", &json!({ "a": 20, "b": 22 }))
        .expect("native_add");
    assert_eq!(add.result["content"].as_str(), Some("42"));
    assert_eq!(add.result["structuredContent"]["sum"].as_i64(), Some(42));

    // Hook payloads cross the same boundary and can carry a replacement.
    let transformed = set.dispatch_hook(HOOK_TRANSFORM_PROMPT, &json!({ "prompt": "base prompt" }));
    assert_eq!(transformed.len(), 1);
    let prompt = transformed[0].result["prompt"].as_str().expect("prompt");
    assert!(prompt.starts_with("base prompt"));
    assert!(prompt.contains("[native hook]"));

    set.shutdown();
    assert!(marker.exists(), "native shutdown did not touch the marker");
}
