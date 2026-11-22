//! Sandbox, tool registration, and the host's tool executor seam.
//!
//! Ground truth: `packages/codemode/src/runtime/execution.ts:451-573`
//! (`CodemodeSandboxBase`: tool table, defaults, `execute`, `close`),
//! `runtime/execution.ts:151-167` (global-name checks), and
//! `runtime/inline.ts:115-126` (`InlineCodemodeSandbox`). Upstream's tool is
//! `{ name, description?, inputSchema?, outputSchema?, spread?, signature?,
//! execute(args, context) }` (`types.ts:14-41`); here it is the [`Tool`] trait
//! plus a closure-backed [`FnTool`]. Because the embedded engine is
//! synchronous, `execute` is synchronous too: an async host tool must be
//! adapted by blocking before returning (documented in `WIRING.md`).
//! Deterministic runs (`runtime/execution.ts:519-566`) are implemented in
//! [`crate::deterministic`], which shares this tool executor seam; progress
//! callbacks are described in [`crate::stream`].

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value as Json;

use crate::identifier::to_codemode_identifier;
use crate::protocol::{CallTargetKind, GlobalStartData, ToolStartData};
use crate::types::{CodemodeError, CodemodeResult, ExecuteOptions, ToolContext};
use crate::vm::{DEFAULT_INTERRUPT_BUDGET, VmOptions, run_vm};

/// Error a tool returns to the script (`types.ts:36-40`: a thrown error with
/// the same message, plus optional own properties).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolError {
    /// Message the script's `Error` carries.
    pub message: String,
    /// Extra own properties attached to that `Error`.
    pub properties: Option<BTreeMap<String, Json>>,
}

impl ToolError {
    /// Construct an error with a message and no extra properties.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            properties: None,
        }
    }

    /// Attach JSON properties (excluding `name`, `message`, `stack`, which the
    /// prelude ignores).
    #[must_use]
    pub fn with_properties(mut self, properties: BTreeMap<String, Json>) -> Self {
        self.properties = Some(properties);
        self
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// A tool or global the script can call (`types.ts:14-41`).
pub trait Tool: Send + Sync {
    /// Raw name. Scripts call `tools[toCodemodeIdentifier(name)]` and
    /// `tools[name]`.
    fn name(&self) -> &str;
    /// Doc comment shown in `ALL_TOOLS` / generated declarations.
    fn description(&self) -> Option<&str> {
        None
    }
    /// Schema of the single argument.
    fn input_schema(&self) -> Option<&Json> {
        None
    }
    /// Schema of the resolved value.
    fn output_schema(&self) -> Option<&Json> {
        None
    }
    /// Globals only: pass all arguments as an array instead of just the first.
    fn spread(&self) -> bool {
        false
    }
    /// Globals only: explicit TypeScript signature for declarations.
    fn signature(&self) -> Option<&str> {
        None
    }
    /// Run the tool. `args` is the JSON round-tripped argument (`Json::Null`
    /// stands in for `undefined`).
    fn execute(&self, args: Json, context: &ToolContext) -> Result<Json, ToolError>;
}

type ToolFn = Box<dyn Fn(Json, &ToolContext) -> Result<Json, ToolError> + Send + Sync>;

/// Closure-backed [`Tool`], convenient for tests and small hosts.
pub struct FnTool {
    name: String,
    description: Option<String>,
    input_schema: Option<Json>,
    output_schema: Option<Json>,
    spread: bool,
    signature: Option<String>,
    function: ToolFn,
}

impl FnTool {
    /// Create a tool with no metadata beyond its name.
    pub fn new(name: impl Into<String>, function: ToolFn) -> Self {
        Self {
            name: name.into(),
            description: None,
            input_schema: None,
            output_schema: None,
            spread: false,
            signature: None,
            function,
        }
    }

    /// Set the description.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the input schema.
    #[must_use]
    pub fn input_schema(mut self, schema: Json) -> Self {
        self.input_schema = Some(schema);
        self
    }

    /// Set the output schema.
    #[must_use]
    pub fn output_schema(mut self, schema: Json) -> Self {
        self.output_schema = Some(schema);
        self
    }

    /// Mark the tool as a spread global.
    #[must_use]
    pub fn spread(mut self, spread: bool) -> Self {
        self.spread = spread;
        self
    }

    /// Set an explicit TypeScript signature.
    #[must_use]
    pub fn signature(mut self, signature: impl Into<String>) -> Self {
        self.signature = Some(signature.into());
        self
    }
}

impl Tool for FnTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    fn input_schema(&self) -> Option<&Json> {
        self.input_schema.as_ref()
    }

    fn output_schema(&self) -> Option<&Json> {
        self.output_schema.as_ref()
    }

    fn spread(&self) -> bool {
        self.spread
    }

    fn signature(&self) -> Option<&str> {
        self.signature.as_deref()
    }

    fn execute(&self, args: Json, context: &ToolContext) -> Result<Json, ToolError> {
        (self.function)(args, context)
    }
}

/// The host side of the JS→tool bridge (`runtime/execution.ts:378-408`).
///
/// Implemented by [`Sandbox`]; a test double can implement it directly to
/// exercise [`crate::vm::run_vm`] without a full sandbox. `id` is the VM's own
/// call number (`prelude-source.ts:182-185`), which a deterministic run uses
/// to match the host's answer to the waiting call.
pub trait ToolExecutor {
    /// Execute one call from the VM.
    fn call(
        &self,
        id: u32,
        target: CallTargetKind,
        name: &str,
        args: Json,
        context: &ToolContext,
    ) -> Result<Json, ToolError>;
}

/// Construction options of a sandbox (`types.ts:112-132`,
/// `runtime/inline.ts:44-55`).
#[derive(Clone)]
pub struct SandboxOptions {
    /// Tools exposed as `tools.<jsName>(args)`.
    pub tools: Vec<Arc<dyn Tool>>,
    /// Globals exposed as `<name>(args)` or `<ns>.<member>(args)`.
    pub globals: Vec<Arc<dyn Tool>>,
    /// Overall deadline; `None` uses upstream's 300,000 ms default.
    pub timeout_ms: Option<u64>,
    /// QuickJS heap limit. `None` leaves the engine's default (unbounded up to
    /// the host process).
    pub memory_limit_bytes: Option<u64>,
    /// Interrupt polls before a synchronous script times out.
    pub interrupt_budget: u64,
}

impl Default for SandboxOptions {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            globals: Vec::new(),
            timeout_ms: None,
            memory_limit_bytes: None,
            interrupt_budget: DEFAULT_INTERRUPT_BUDGET,
        }
    }
}

const RESERVED_GLOBALS: [&str; 9] = [
    "tools",
    "ALL_TOOLS",
    "console",
    "text",
    "image",
    "exit",
    "globalThis",
    "store",
    "load",
];

fn is_simple_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    let valid_first = first.is_ascii_alphabetic() || first == '_' || first == '$';
    valid_first
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '$'
        })
}

/// Validate global names (`runtime/execution.ts:151-167`): identifiers or
/// `namespace.member`, not shadowing a built-in or each other. Shared with
/// deterministic runs, which declare globals as data rather than [`Tool`]s.
pub(crate) fn check_global_names<'a>(names: impl Iterator<Item = &'a str>) -> Result<(), String> {
    let mut seen = HashSet::new();
    let mut namespaces = HashSet::new();
    for name in names {
        let parts: Vec<&str> = name.split('.').collect();
        if parts.len() > 2
            || parts.iter().any(|part| !is_simple_identifier(part))
            || RESERVED_GLOBALS.contains(&parts[0])
        {
            return Err(format!("Invalid global name \"{name}\""));
        }
        if !seen.insert(name.to_string()) {
            return Err(format!("Global \"{name}\" is already registered"));
        }
        if parts.len() == 2 {
            namespaces.insert(parts[0].to_string());
        }
    }
    for namespace in namespaces {
        if seen.contains(&namespace) {
            return Err(format!(
                "Global \"{namespace}\" conflicts with the namespace \"{namespace}\""
            ));
        }
    }
    Ok(())
}

fn check_globals(globals: &[Arc<dyn Tool>]) -> Result<(), String> {
    check_global_names(globals.iter().map(|global| global.name()))
}

/// Defaults a deterministic run inherits from its sandbox
/// (`runtime/execution.ts:542-555`).
#[derive(Clone)]
pub(crate) struct SandboxDefaults {
    /// Overall deadline.
    pub(crate) timeout_ms: Option<u64>,
    /// Heap limit.
    pub(crate) memory_limit_bytes: Option<u64>,
    /// Interrupt polls before a synchronous script times out.
    pub(crate) interrupt_budget: u64,
    /// Progress callback installed with [`Sandbox::with_update_sink`].
    pub(crate) update_sink: Option<crate::stream::UpdateSink>,
}

/// A sandbox holding a tool table and execution defaults. Each [`execute`]
/// call gets a fresh VM (`runtime/execution.ts:451-459`).
pub struct Sandbox {
    tools: Mutex<BTreeMap<String, Arc<dyn Tool>>>,
    globals: BTreeMap<String, Arc<dyn Tool>>,
    timeout_ms: Option<u64>,
    memory_limit_bytes: Option<u64>,
    interrupt_budget: u64,
    closed: AtomicBool,
    update_sink: Option<crate::stream::UpdateSink>,
}

impl Sandbox {
    /// Create a sandbox, registering its tools and validating its globals.
    ///
    /// Returns an error for duplicate tool names, invalid global names, and
    /// namespace conflicts — the same cases that throw in upstream's
    /// constructor.
    pub fn new(options: SandboxOptions) -> Result<Self, String> {
        let mut tools = BTreeMap::new();
        for tool in options.tools {
            if tools.contains_key(tool.name()) {
                return Err(format!("Tool \"{}\" is already registered", tool.name()));
            }
            tools.insert(tool.name().to_string(), tool);
        }
        check_globals(&options.globals)?;
        let globals = options
            .globals
            .into_iter()
            .map(|tool| (tool.name().to_string(), tool))
            .collect();
        Ok(Self {
            tools: Mutex::new(tools),
            globals,
            timeout_ms: options.timeout_ms,
            memory_limit_bytes: options.memory_limit_bytes,
            interrupt_budget: options.interrupt_budget,
            closed: AtomicBool::new(false),
            update_sink: None,
        })
    }

    /// Install a progress callback for `execute()` and `start()` runs
    /// (`execute.ts:496-520`). It runs on the VM thread; keep it cheap.
    #[must_use]
    pub fn with_update_sink(mut self, sink: crate::stream::UpdateSink) -> Self {
        self.update_sink = Some(sink);
        self
    }

    /// Defaults inherited by deterministic runs.
    pub(crate) fn defaults(&self) -> SandboxDefaults {
        SandboxDefaults {
            timeout_ms: self.timeout_ms,
            memory_limit_bytes: self.memory_limit_bytes,
            interrupt_budget: self.interrupt_budget,
            update_sink: self.update_sink.clone(),
        }
    }

    /// Whether new executions and deterministic runs are rejected.
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Register a tool. Errors when the name is already taken
    /// (`runtime/execution.ts:478-482`).
    pub fn register_tool(&self, tool: Arc<dyn Tool>) -> Result<(), String> {
        let mut tools = self
            .tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tools.contains_key(tool.name()) {
            return Err(format!("Tool \"{}\" is already registered", tool.name()));
        }
        tools.insert(tool.name().to_string(), tool);
        Ok(())
    }

    /// Remove a tool, returning whether it existed (`runtime/execution.ts:484-486`).
    pub fn unregister_tool(&self, name: &str) -> bool {
        self.tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(name)
            .is_some()
    }

    /// Snapshot of the registered tools (`runtime/execution.ts:488-490`).
    #[must_use]
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// Snapshot of the registered globals (`runtime/execution.ts:492-494`).
    #[must_use]
    pub fn globals(&self) -> Vec<Arc<dyn Tool>> {
        self.globals.values().cloned().collect()
    }

    /// Run `code` as an async function body. Script failures are reported in
    /// the returned [`CodemodeResult`], never as Rust errors
    /// (`runtime/execution.ts:496-517`).
    #[must_use]
    pub fn execute(&self, code: &str, options: ExecuteOptions) -> CodemodeResult {
        if self.closed.load(Ordering::SeqCst) {
            return CodemodeResult::Err {
                error: CodemodeError::sandbox("Sandbox is closed"),
                output: Vec::new(),
                calls: Vec::new(),
                during: None,
            };
        }
        let tools: Vec<ToolStartData> = self
            .tools()
            .iter()
            .map(|tool| ToolStartData {
                name: tool.name().to_string(),
                js_name: to_codemode_identifier(tool.name()),
                description: tool.description().unwrap_or_default().to_string(),
            })
            .collect();
        let globals: Vec<GlobalStartData> = self
            .globals
            .values()
            .map(|global| GlobalStartData {
                name: global.name().to_string(),
                spread: global.spread(),
            })
            .collect();
        let store = options.store.unwrap_or_default();
        let vm_options = VmOptions {
            code,
            tools: &tools,
            globals: &globals,
            memory_limit_bytes: self.memory_limit_bytes,
            store: &store,
            timeout_ms: options.timeout_ms.or(self.timeout_ms),
            interrupt_budget: self.interrupt_budget,
            signal: options.signal,
            deterministic: None,
            clock: None,
            update_sink: self.update_sink.clone(),
        };
        run_vm(&vm_options, self)
    }

    /// Reject new executions (`runtime/execution.ts:568-572`). In-process
    /// executions are synchronous, so there are no in-flight calls to abort.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

/// Upstream's in-thread sandbox name; this port is always in-thread.
pub type InlineCodemodeSandbox = Sandbox;

impl ToolExecutor for Sandbox {
    fn call(
        &self,
        _id: u32,
        target: CallTargetKind,
        name: &str,
        args: Json,
        context: &ToolContext,
    ) -> Result<Json, ToolError> {
        if target == CallTargetKind::Global {
            return self.globals.get(name).map_or_else(
                || Err(unknown(target, name)),
                |tool| tool.execute(args, context),
            );
        }
        let table = self
            .tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Clone out of the lock so a tool may re-enter the sandbox.
        let tool = table.get(name).cloned();
        drop(table);
        tool.map_or_else(
            || Err(unknown(target, name)),
            |tool| tool.execute(args, context),
        )
    }
}

fn unknown(target: CallTargetKind, name: &str) -> ToolError {
    let kind = if target == CallTargetKind::Tool {
        "tool"
    } else {
        "global"
    };
    ToolError::new(format!("Unknown {kind} \"{name}\""))
}

/// Helper for building a shared tool from a closure.
#[must_use]
pub fn tool<T>(name: impl Into<String>, function: T) -> Arc<dyn Tool>
where
    T: Fn(Json, &ToolContext) -> Result<Json, ToolError> + Send + Sync + 'static,
{
    Arc::new(FnTool::new(name, Box::new(function)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CallStatus;
    use crate::types::{AbortFlag, CodemodeErrorKind, OutputItem};
    use serde_json::json;

    fn sandbox(tools: Vec<Arc<dyn Tool>>, globals: Vec<Arc<dyn Tool>>) -> Sandbox {
        Sandbox::new(SandboxOptions {
            tools,
            globals,
            ..SandboxOptions::default()
        })
        .expect("sandbox")
    }

    fn echo_tool() -> Arc<dyn Tool> {
        tool("double", |args, _| {
            let n = args.get("n").and_then(Json::as_i64).unwrap_or(0);
            Ok(json!({"value": n * 2}))
        })
    }

    #[test]
    fn bridge_round_trip_calls_a_tool_and_returns_its_value() {
        let sandbox = sandbox(vec![echo_tool()], vec![]);
        let result = sandbox.execute(
            "const r = await tools.double({ n: 21 }); text(JSON.stringify(r)); return r.value;",
            ExecuteOptions::default(),
        );
        assert!(result.is_ok(), "{result:?}");
        match result {
            CodemodeResult::Ok {
                value,
                output,
                calls,
                ..
            } => {
                assert_eq!(value, json!(42));
                assert_eq!(output, vec![OutputItem::text(r#"{"value":42}"#)]);
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "double");
                assert_eq!(calls[0].status, crate::types::CallStatus::Ok);
            }
            CodemodeResult::Err { .. } => unreachable!(),
        }
    }

    #[test]
    fn raw_name_and_normalized_identifier_both_resolve() {
        let sandbox = sandbox(vec![tool("my-tool", |_args, _| Ok(json!("ok")))], vec![]);
        let result = sandbox.execute(
            "return (await tools.my_tool({})) + (await tools[\"my-tool\"]({}));",
            ExecuteOptions::default(),
        );
        assert!(result.is_ok(), "{result:?}");
        match result {
            CodemodeResult::Ok { value, calls, .. } => {
                assert_eq!(value, json!("okok"));
                assert_eq!(calls.len(), 2);
                assert!(
                    calls.iter().all(|call| call.name == "my-tool"
                        && call.status == crate::types::CallStatus::Ok)
                );
            }
            CodemodeResult::Err { .. } => unreachable!(),
        }
    }

    #[test]
    fn thrown_tool_error_rejects_the_script_promise() {
        let failing = tool("fail", |_args, _| Err(ToolError::new("boom")));
        let sandbox = sandbox(vec![failing], vec![]);
        let result = sandbox.execute(
            "try { await tools.fail({}); text('no'); } catch (e) { text(e.message); } return 'ok';",
            ExecuteOptions::default(),
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(result.output(), &[OutputItem::text("boom")]);
        assert_eq!(result.calls()[0].status, crate::types::CallStatus::Error);
    }

    #[test]
    fn unhandled_tool_error_fails_the_script() {
        let failing = tool("fail", |_args, _| Err(ToolError::new("boom")));
        let sandbox = sandbox(vec![failing], vec![]);
        let result = sandbox.execute("await tools.fail({}); return 1;", ExecuteOptions::default());
        let error = result.error().expect("must fail");
        assert_eq!(error.kind, CodemodeErrorKind::Script);
        assert_eq!(error.message, "boom");
        assert_eq!(result.calls()[0].status, crate::types::CallStatus::Error);
    }

    #[test]
    fn error_properties_are_attached_to_the_script_error() {
        let failing = tool("fail", |_args, _| {
            let mut properties = BTreeMap::new();
            properties.insert("structuredOutput".to_string(), json!({"code": 7}));
            Err(ToolError::new("boom").with_properties(properties))
        });
        let sandbox = sandbox(vec![failing], vec![]);
        let result = sandbox.execute(
            "try { await tools.fail({}); } catch (e) { text(JSON.stringify(e.structuredOutput)); } return 1;",
            ExecuteOptions::default(),
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(result.output(), &[OutputItem::text(r#"{"code":7}"#)]);
    }

    #[test]
    fn output_helpers_and_exit_are_marshalled() {
        let sandbox = sandbox(vec![], vec![]);
        let result = sandbox.execute(
            "text('a'); console.log('b'); text(3); return 'r';",
            ExecuteOptions::default(),
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            result.output(),
            &[
                OutputItem::text("a"),
                OutputItem::console("b"),
                OutputItem::text("3"),
            ]
        );
        let exited = sandbox.execute(
            "text('before'); exit(); text('after');",
            ExecuteOptions::default(),
        );
        assert!(exited.is_ok(), "{exited:?}");
        assert_eq!(exited.output(), &[OutputItem::text("before")]);
    }

    #[test]
    fn store_and_load_round_trip_through_the_executor() {
        let sandbox = sandbox(vec![], vec![]);
        let mut store = BTreeMap::new();
        store.insert("a".to_string(), json!(1));
        let result = sandbox.execute(
            "store('b', load('a') + 1); store('gone', undefined); return load('b');",
            ExecuteOptions {
                store: Some(store),
                ..ExecuteOptions::default()
            },
        );
        assert!(result.is_ok(), "{result:?}");
        match result {
            CodemodeResult::Ok {
                value,
                store_writes,
                ..
            } => {
                assert_eq!(value, json!(2));
                assert_eq!(store_writes.set.get("b"), Some(&json!(2)));
                assert_eq!(store_writes.delete, vec!["gone"]);
            }
            CodemodeResult::Err { .. } => unreachable!(),
        }
    }

    #[test]
    fn globals_can_spread_arguments_and_live_in_namespaces() {
        let global: std::sync::Arc<dyn Tool> =
            std::sync::Arc::new(FnTool::new("ns.echo", Box::new(|args, _| Ok(args))).spread(true));
        let sandbox = sandbox(vec![], vec![global]);
        let result = sandbox.execute("return await ns.echo(1, 'x');", ExecuteOptions::default());
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(result.output().len(), 0);
        assert_eq!(result.calls().len(), 0, "globals are not recorded as calls");
        match result {
            CodemodeResult::Ok { value, .. } => assert_eq!(value, json!([1, "x"])),
            CodemodeResult::Err { .. } => unreachable!(),
        }
    }

    #[test]
    fn all_tools_lists_registered_tools() {
        let sandbox = sandbox(vec![echo_tool()], vec![]);
        let result = sandbox.execute(
            "return ALL_TOOLS.map((tool) => tool.name);",
            ExecuteOptions::default(),
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(result.output().len(), 0);
        match result {
            CodemodeResult::Ok { value, .. } => assert_eq!(value, json!(["double"])),
            CodemodeResult::Err { .. } => unreachable!(),
        }
    }

    #[test]
    fn missing_tool_gives_a_helpful_error() {
        let sandbox = sandbox(vec![echo_tool()], vec![]);
        let result = sandbox.execute(
            "try { tools.doubl({}); } catch (e) { text(e.message); } return 1;",
            ExecuteOptions::default(),
        );
        assert!(result.is_ok(), "{result:?}");
        assert!(result.output().iter().any(|item| matches!(item, OutputItem::Text { text, .. } if text.contains("Did you mean tools.double"))));
    }

    #[test]
    fn syntax_errors_are_script_errors() {
        let sandbox = sandbox(vec![], vec![]);
        let result = sandbox.execute("return (((;", ExecuteOptions::default());
        let error = result.error().expect("must fail");
        assert_eq!(error.kind, CodemodeErrorKind::Script);
    }

    #[test]
    fn interrupt_budget_bounds_synchronous_loops() {
        let options = SandboxOptions {
            interrupt_budget: 50,
            ..SandboxOptions::default()
        };
        let sandbox = Sandbox::new(options).expect("sandbox");
        let result = sandbox.execute("while (true) {}", ExecuteOptions::default());
        let error = result.error().expect("must time out");
        assert_eq!(error.kind, CodemodeErrorKind::Timeout);
    }

    #[test]
    fn aborted_execution_is_reported() {
        let sandbox = sandbox(vec![], vec![]);
        let flag = AbortFlag::new();
        flag.abort();
        let result = sandbox.execute(
            "return 1;",
            ExecuteOptions {
                signal: Some(flag),
                ..ExecuteOptions::default()
            },
        );
        let error = result.error().expect("must abort");
        assert_eq!(error.kind, CodemodeErrorKind::Aborted);
    }

    #[test]
    fn closed_sandbox_rejects_new_executions() {
        let sandbox = sandbox(vec![], vec![]);
        sandbox.close();
        let result = sandbox.execute("return 1;", ExecuteOptions::default());
        assert_eq!(
            result.error().map_or("", |error| error.message.as_str()),
            "Sandbox is closed"
        );
    }

    #[test]
    fn registration_validates_names() {
        let sandbox = sandbox(vec![echo_tool()], vec![]);
        assert!(sandbox.register_tool(echo_tool()).is_err());
        assert!(sandbox.unregister_tool("double"));
        assert!(!sandbox.unregister_tool("double"));
        assert!(sandbox.register_tool(echo_tool()).is_ok());

        assert!(
            Sandbox::new(SandboxOptions {
                globals: vec![tool("console.log", |_args, _| Ok(Json::Null))],
                ..SandboxOptions::default()
            })
            .is_err()
        );
        assert!(
            Sandbox::new(SandboxOptions {
                globals: vec![
                    tool("ns.a", |_args, _| Ok(Json::Null)),
                    tool("ns", |_args, _| Ok(Json::Null))
                ],
                ..SandboxOptions::default()
            })
            .is_err()
        );
    }

    #[test]
    fn update_sink_reports_output_calls_and_completion() {
        use crate::stream::UpdateEvent;
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink_recorded = Arc::clone(&recorded);
        let sandbox = Sandbox::new(SandboxOptions {
            tools: vec![echo_tool()],
            ..SandboxOptions::default()
        })
        .expect("sandbox")
        .with_update_sink(Arc::new(move |event| {
            sink_recorded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }));
        let result = sandbox.execute(
            "text('a'); const r = await tools.double({ n: 3 }); text(String(r.value)); return r.value;",
            ExecuteOptions::default(),
        );
        assert!(result.is_ok(), "{result:?}");
        let recorded = recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            recorded
                .iter()
                .any(|event| matches!(event, UpdateEvent::Output(OutputItem::Text { text, .. }) if text == "a"))
        );
        assert!(recorded.iter().any(
            |event| matches!(event, UpdateEvent::CallStarted { name, .. } if name == "double")
        ));
        assert!(recorded.iter().any(|event| matches!(
            event,
            UpdateEvent::CallFinished {
                status: CallStatus::Ok,
                ..
            }
        )));
        assert!(matches!(
            recorded.last(),
            Some(UpdateEvent::Finished { ok: true })
        ));
    }

    #[test]
    fn model_globals_are_reachable_as_a_namespace() {
        let globals = crate::models::model_globals(Arc::new(crate::models::EmptyModelRuntime));
        let sandbox = sandbox(vec![], globals);
        let result = sandbox.execute(
            "return (await models.getModelsOfType('chat')).length;",
            ExecuteOptions::default(),
        );
        assert!(result.is_ok(), "{result:?}");
        match result {
            CodemodeResult::Ok { value, .. } => assert_eq!(value, json!(0)),
            CodemodeResult::Err { .. } => unreachable!(),
        }
    }

    #[test]
    fn vm_exposes_no_dynamic_require() {
        // Upstream deliberately omits `require` and modules from the sandbox
        // (`runtime/execution.ts:451-458`); QuickJS has no CommonJS loader.
        let sandbox = sandbox(vec![], vec![]);
        let result = sandbox.execute("return typeof require;", ExecuteOptions::default());
        assert!(result.is_ok(), "{result:?}");
        match result {
            CodemodeResult::Ok { value, .. } => assert_eq!(value, json!("undefined")),
            CodemodeResult::Err { .. } => unreachable!(),
        }
    }
}
