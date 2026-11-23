//! Native Rust extension host loader (additive pi-rs capability).
//!
//! Upstream pi (`earendil-works/pi` 1.5.2) has no native extension host: its
//! extension system is JavaScript/TypeScript only (`extensions/index.ts`,
//! `extensions/runner.ts`). This module is new; it is not a port of an
//! upstream file. It gives native `cdylib` extensions the same three
//! extension points the JS host has: declared tools, event hooks, and a
//! generic JSON call surface.
//!
//! # Discovery
//!
//! * `PI_NATIVE_EXTENSIONS` - colon-separated paths to shared libraries.
//! * `<cwd>/.pi/native/` - every `.so` / `.dylib` / `.dll` file, sorted by
//!   path for deterministic load order. A missing directory is ignored.
//!
//! First declaration of a tool or hook wins, matching the JS host. A bad ABI
//! version, missing required symbol, failed init, or missing configured file
//! aborts startup with the extension name in the message. With neither source
//! configured the loader is inert: no library is opened and every dispatch
//! returns immediately.
//!
//! # ABI boundary
//!
//! All FFI lives in the `pi-native-ext` crate. This module only uses its safe
//! API, so the parent crate keeps `#![forbid(unsafe_code)]`. See
//! `pi-native-ext::abi` for the C ABI and the reason Rust's own ABI is not
//! used.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pi_agent::{AgentToolCall, AgentToolResult, BoxFuture, ToolExecutor, ToolSpec};
use pi_native_ext::abi::{self, HOOK_TRANSFORM_PROMPT};
use pi_native_ext::{CallResult, ExtensionSet, LoadSpec};
use serde_json::{Value, json};

use crate::tools::index::ToolDefinition;
use crate::tools::renderers::ToolRenderers;

/// Environment variable with colon-separated native library paths.
pub const NATIVE_EXTENSIONS_ENV: &str = "PI_NATIVE_EXTENSIONS";
/// Project-local directory additional libraries are loaded from.
pub const PROJECT_NATIVE_DIR: &str = ".pi/native";

/// Empty renderers, like the JS extension host: the interactive component
/// resolves real renderers through `renderers_for` and falls back to plain
/// text, so these placeholders are never reached in normal rendering.
const NATIVE_RENDERERS: ToolRenderers = ToolRenderers {
    render_call: |_args, _context| String::new(),
    render_result: |_args, _result, _context| String::new(),
};

/// All native extensions loaded for one runtime. `Clone` shares the same
/// loaded libraries via [`Arc`]; dropping the last clone runs their
/// `pi_ext_shutdown` before unloading.
#[derive(Clone)]
pub struct NativeExtensions {
    set: Arc<ExtensionSet>,
}

impl NativeExtensions {
    /// Discover and load from the environment and the project directory.
    pub fn load(cwd: &Path) -> Result<Self, String> {
        let env = std::env::var(NATIVE_EXTENSIONS_ENV).ok();
        let specs = discover(cwd, env.as_deref());
        Self::from_specs(&specs, cwd)
    }

    /// Load an explicit set of libraries (also used by tests).
    pub fn from_specs(specs: &[LoadSpec], cwd: &Path) -> Result<Self, String> {
        let mut set = ExtensionSet::new();
        for spec in specs {
            if !spec.path.is_file() {
                return Err(format!(
                    "native extension `{}`: file not found at {}",
                    spec.name,
                    spec.path.display()
                ));
            }
            let config = abi::InitConfig {
                name: spec.name.clone(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                cwd: cwd.to_string_lossy().into_owned(),
                shutdown_marker: None,
            };
            set.add_library(spec, &config)
                .map_err(|error| error.to_string())?;
        }
        Ok(Self::from_set(set))
    }

    /// Wrap an already-built set, for compiled-in extensions and tests.
    #[must_use]
    pub fn from_set(set: ExtensionSet) -> Self {
        Self { set: Arc::new(set) }
    }

    /// `true` when no native extension was loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// Append native tool declarations to `out` unless the name is taken.
    /// Native tools append after builtins and JS tools, like the JS host.
    pub fn register_tools(&self, out: &mut Vec<ToolDefinition>) {
        for declared in self.set.tools() {
            if out.iter().any(|tool| tool.name == declared.name) {
                continue;
            }
            out.push(tool_definition(declared));
        }
    }

    /// Deliver an event to every subscribing extension. Results are returned
    /// so callers can apply replacements; hosts that only notify may drop
    /// them. Unsubscribed events are a no-op.
    #[must_use]
    pub fn dispatch_hook(&self, hook: &str, payload: &Value) -> Vec<CallResult> {
        self.set.dispatch_hook(hook, payload)
    }

    /// Fire `on_session_start` once, when a runtime finishes assembling.
    pub fn on_session_start(&self, mode: &str, cwd: &str) -> Vec<CallResult> {
        self.dispatch_hook(
            abi::HOOK_SESSION_START,
            &json!({ "mode": mode, "cwd": cwd }),
        )
    }

    /// Chain every `transform_prompt` subscriber in load order. A subscriber
    /// that omits `prompt` leaves the current text unchanged.
    #[must_use]
    pub fn transform_prompt(&self, prompt: &str) -> String {
        let mut current = prompt.to_string();
        for result in self
            .set
            .dispatch_hook(HOOK_TRANSFORM_PROMPT, &json!({ "prompt": current }))
        {
            if let Some(next) = result.result.get("prompt").and_then(Value::as_str) {
                current = next.to_string();
            }
        }
        current
    }

    /// Call a declared tool directly. Useful to hosts and tests; the normal
    /// path is the executor returned by [`Self::wrap_executor`].
    pub fn call_tool(&self, name: &str, arguments: &Value) -> Result<CallResult, String> {
        self.set
            .call_tool(name, arguments)
            .map_err(|error| error.to_string())
    }

    /// Wrap `inner` so calls to native tools dispatch into their extension
    /// and every other name still reaches the builtin / JS executor.
    #[must_use]
    pub fn wrap_executor(
        &self,
        inner: Arc<dyn ToolExecutor>,
        selected: &[ToolDefinition],
    ) -> Arc<dyn ToolExecutor> {
        let native_names: HashSet<String> = self
            .set
            .tools()
            .iter()
            .filter(|declared| selected.iter().any(|tool| tool.name == declared.name))
            .map(|declared| declared.name.clone())
            .collect();
        Arc::new(NativeAwareExecutor {
            inner,
            native_names,
            set: Arc::clone(&self.set),
        })
    }

    /// Explicit shutdown for hosts that own a finite runtime. Dropping the
    /// last clone also shuts down, and both paths are idempotent.
    pub fn shutdown(&self) {
        self.set.shutdown();
    }
}

/// Pure discovery helper: environment paths first, then the sorted project
/// directory, deduplicated by path. A missing directory or a non-shared-file
/// entry is ignored.
#[must_use]
pub fn discover(cwd: &Path, env_paths: Option<&str>) -> Vec<LoadSpec> {
    let mut specs: Vec<LoadSpec> = env_paths
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|path| LoadSpec::from_path(&path))
        .collect();

    let directory = cwd.join(PROJECT_NATIVE_DIR);
    if let Ok(entries) = std::fs::read_dir(&directory) {
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && is_shared_library(path))
            .collect();
        paths.sort();
        specs.extend(paths.iter().map(|path| LoadSpec::from_path(path)));
    }

    let mut seen = HashSet::new();
    specs.retain(|spec| seen.insert(spec.path.clone()));
    specs
}

fn is_shared_library(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| matches!(extension, "so" | "dylib" | "dll"))
}

/// ABI declaration to the host's `ToolDefinition`. Names leak to `'static`
/// on purpose, exactly like the JS extension host: they live for the process
/// and the count is bounded by the loaded extensions.
fn tool_definition(declared: &abi::DeclaredTool) -> ToolDefinition {
    let name: &'static str = Box::leak(declared.name.clone().into_boxed_str());
    ToolDefinition {
        name,
        label: name,
        description: declared.description.clone(),
        parameters: if declared.parameters.is_object() {
            declared.parameters.clone()
        } else {
            json!({ "type": "object", "properties": {} })
        },
        output_schema: None,
        prompt_snippet: None,
        prompt_guidelines: Vec::new(),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        render_shell: None,
        renderers: NATIVE_RENDERERS,
    }
}

/// Turns a native `CallResult` into the tool result shape the agent loop
/// expects. A failed call becomes an error result rather than an exception.
fn tool_result(call: CallResult) -> AgentToolResult {
    if !call.ok {
        let mut result = AgentToolResult::error(
            call.error
                .unwrap_or_else(|| "native extension call failed".to_string()),
        );
        result.is_error = true;
        return result;
    }
    let value = call.result;
    let text = match &value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Object(map) => match map.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(other) => {
                serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string())
            }
            None => serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
        },
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };
    let mut result = crate::tools::text_result(text);
    if let Some(details) = value.get("details") {
        result.details = details.clone();
    }
    if let Some(structured) = value
        .get("structuredContent")
        .filter(|value| !value.is_null())
    {
        result.structured_content = Some(structured.clone());
    }
    result.is_error = value
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    result
}

/// Executor that routes native tool names to their extension, delegating all
/// other tools to the host's existing executor.
struct NativeAwareExecutor {
    inner: Arc<dyn ToolExecutor>,
    native_names: HashSet<String>,
    set: Arc<ExtensionSet>,
}

impl ToolExecutor for NativeAwareExecutor {
    fn tools(&self) -> Vec<ToolSpec> {
        self.inner.tools()
    }

    fn execution_mode(&self, name: &str) -> Option<pi_agent::ToolExecutionMode> {
        self.inner.execution_mode(name)
    }

    fn execute(
        &self,
        call: AgentToolCall,
        updates: tokio::sync::mpsc::UnboundedSender<AgentToolResult>,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        if !self.native_names.contains(&call.name) {
            return self.inner.execute(call, updates);
        }
        let name = call.name.clone();
        let arguments = Value::Object(call.arguments.clone());
        let set = Arc::clone(&self.set);
        Box::pin(async move {
            let result = set
                .call_tool(&name, &arguments)
                .map_err(|error| error.to_string())?;
            Ok(tool_result(result))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_agent::ToolExecutionMode;
    use pi_ai::types::{ContentBlock, JsonObject, ToolCall};
    use pi_native_ext::{
        DeclaredTool, InitResult, NativeExtension,
        abi::{CallRequest, HOOK_MESSAGE, InitConfig},
    };

    struct EchoExtension;

    impl NativeExtension for EchoExtension {
        fn name(&self) -> &'static str {
            "echo"
        }

        fn init(&self, _config: &InitConfig) -> InitResult {
            InitResult {
                ok: true,
                tools: vec![DeclaredTool {
                    name: "native_echo".to_string(),
                    description: "Echo".to_string(),
                    parameters: json!({ "type": "object" }),
                }],
                hooks: vec![HOOK_MESSAGE.to_string(), HOOK_TRANSFORM_PROMPT.to_string()],
                error: None,
            }
        }

        fn call(&self, request: &CallRequest) -> Result<CallResult, String> {
            match request {
                CallRequest::Tool { arguments, .. } => {
                    let text = arguments.get("text").and_then(Value::as_str).unwrap_or("");
                    Ok(CallResult::ok(
                        json!({ "content": format!("native:{text}") }),
                    ))
                }
                CallRequest::Hook { hook, payload } if hook == HOOK_TRANSFORM_PROMPT => {
                    let prompt = payload.get("prompt").and_then(Value::as_str).unwrap_or("");
                    Ok(CallResult::ok(
                        json!({ "prompt": format!("{prompt}+native" ) }),
                    ))
                }
                CallRequest::Hook { .. } => Ok(CallResult::ok(json!({ "handled": true }))),
                CallRequest::Ping => Ok(CallResult::ok(json!({ "pong": true }))),
            }
        }
    }

    struct NoopExecutor;

    impl ToolExecutor for NoopExecutor {
        fn tools(&self) -> Vec<ToolSpec> {
            Vec::new()
        }

        fn execution_mode(&self, _name: &str) -> Option<ToolExecutionMode> {
            None
        }

        fn execute(
            &self,
            _call: AgentToolCall,
            _updates: tokio::sync::mpsc::UnboundedSender<AgentToolResult>,
        ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
            Box::pin(async { Err("not a native tool".to_string()) })
        }
    }

    #[test]
    fn discover_env_then_sorted_project_dir_deduped() {
        let temp = tempfile::tempdir().expect("temp dir");
        let native = temp.path().join(".pi/native");
        std::fs::create_dir_all(&native).expect("create native dir");
        std::fs::write(native.join("b.so"), b"").expect("b.so");
        std::fs::write(native.join("a.so"), b"").expect("a.so");
        std::fs::write(native.join("notes.txt"), b"").expect("notes.txt");
        let env = format!(
            "{}:{}",
            native.join("a.so").display(),
            temp.path().join("missing.so").display()
        );
        let specs = discover(temp.path(), Some(&env));
        let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
        // Explicit environment entries are kept even when missing so
        // `from_specs` can fail loudly with their name.
        assert_eq!(names, ["a", "missing", "b"]);
    }

    #[test]
    fn missing_project_dir_is_inert() {
        let temp = tempfile::tempdir().expect("temp dir");
        assert_eq!(discover(temp.path(), None), Vec::new());
        let set = ExtensionSet::new();
        let native = NativeExtensions::from_set(set);
        assert!(native.is_empty());
        assert_eq!(native.dispatch_hook(HOOK_MESSAGE, &json!({})), Vec::new());
        assert_eq!(native.transform_prompt("plain"), "plain");
    }

    #[test]
    fn native_tools_register_and_execute_through_the_wrapper() {
        let mut set = ExtensionSet::new();
        set.add_in_tree(
            Box::new(EchoExtension),
            &abi::InitConfig {
                name: "echo".to_string(),
                version: "0.1.0".to_string(),
                cwd: "/test".to_string(),
                shutdown_marker: None,
            },
        )
        .expect("register echo");
        let native = NativeExtensions::from_set(set);

        let mut tools = Vec::new();
        native.register_tools(&mut tools);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "native_echo");
        assert_eq!(tools[0].label, "native_echo");

        let executor = native.wrap_executor(Arc::new(NoopExecutor), &tools);
        let call = ToolCall {
            id: "call-1".to_string(),
            name: "native_echo".to_string(),
            arguments: JsonObject::from_iter([("text".to_string(), json!("hi"))]),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        };
        let (updates, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let result =
            crate::runtime::block_on(executor.execute(call, updates)).expect("native tool result");
        match &result.content[0] {
            ContentBlock::Text(text) => assert_eq!(text.text, "native:hi"),
            other => panic!("expected text content, got {other:?}"),
        }
        assert!(!result.is_error);

        // A non-native name still reaches the wrapped executor.
        let builtin = ToolCall {
            id: "call-2".to_string(),
            name: "read".to_string(),
            arguments: JsonObject::new(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        };
        let (updates, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let error = crate::runtime::block_on(executor.execute(builtin, updates))
            .expect_err("delegated call");
        assert_eq!(error, "not a native tool");
    }

    #[test]
    fn hooks_are_dispatched_and_prompt_transform_chains() {
        let mut set = ExtensionSet::new();
        set.add_in_tree(
            Box::new(EchoExtension),
            &abi::InitConfig {
                name: "echo".to_string(),
                version: "0.1.0".to_string(),
                cwd: "/test".to_string(),
                shutdown_marker: None,
            },
        )
        .expect("register echo");
        let native = NativeExtensions::from_set(set);
        assert!(native.dispatch_hook(HOOK_MESSAGE, &json!({}))[0].ok);
        assert_eq!(native.transform_prompt("base"), "base+native");
    }
}
