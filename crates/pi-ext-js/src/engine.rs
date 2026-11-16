//! The embedded engine: one `QuickJS` runtime per host, the Rust-side op
//! table, and the dispatch-session protocol the runner drives.
//!
//! Architecture per §6.4 of `docs/EXTENSIONS.md`: the JS side (glue.rs)
//! owns the per-extension registries and the live event objects, the Rust
//! side owns dispatch semantics and aggregation. The engine boundary
//! carries only registration calls (JS→Rust state), event dispatch
//! (Rust→JS handler calls with JSON payloads and results), and host-state
//! reads (settings, tools, flags, cwd) — the same three channels the E0
//! spike proved, extended to the full loader chain.
//!
//! Upstream ground truth: the op surface is `createExtensionAPI`
//! (`core/extensions/loader.ts:253-561`) bound to a runtime
//! (`createExtensionRuntime`, `loader.ts:160-244`); contexts and sessions
//! are `runner.ts:331-362, 577-622, 891-1041`. There is no built-in event
//! loop (§6.3): pending jobs are drained synchronously after every engine
//! entry, which settles async factories, async handlers, and async
//! `pi.exec` exactly where upstream awaits them. The provider-stream op
//! extends the same pattern to a stashed `streamSimple` handler's event
//! stream (`core/provider-composer.ts:588-613`, `packages/ai`
//! `StreamFunction` + `AssistantMessageEventStream`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rquickjs::{Context, Function, Runtime, Value};
use serde_json::Value as Json;

use crate::error::JsHostError;
use crate::exec;
use crate::mcp_registry::McpRegistry;
use crate::provider_registry::ProviderRegistry;
use crate::resolver::{ExtensionLoader, ExtensionResolver};

/// The kind of context a dispatch session uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextKind {
    /// The base event context.
    Event,
    /// The command context (`waitForIdle`, session mutation defaults).
    Command,
    /// The tool context (`tools`, nested `executeTool`).
    Tool,
    /// The reduced project-trust context (`cwd`, `mode`, `hasUI`, `ui`).
    ProjectTrust,
    /// The routing context: the base event context for a virtual model's
    /// `route` call (`runtime.createContext()`, loader.ts:518).
    Route,
    /// The base event context for a provider's `streamSimple` call
    /// (`runtime.createContext()`, loader.ts:518 — the session event is the
    /// marshalled `{model, context, options}` request).
    ProviderStream,
}

/// The host actions an embedder overrides; everything defaults to the
/// unbound runner behavior (`runner.ts:373-401`). The closures carry
/// `Send` so `HostCore` (and with rquickjs's `parallel` feature the whole
/// runner) can live behind a mutex shared with the agent loop.
pub struct HostActions {
    /// `ctx.isIdle()`.
    pub is_idle: Box<dyn Fn() -> bool + Send>,
    /// `ctx.isProjectTrusted()`.
    pub is_project_trusted: Box<dyn Fn() -> bool + Send>,
    /// `ctx.model`.
    pub get_model: Box<dyn Fn() -> Json + Send>,
    /// `ctx.getContextUsage()`.
    pub get_context_usage: Box<dyn Fn() -> Option<Json> + Send>,
    /// `ctx.getSystemPrompt()`.
    pub get_system_prompt: Box<dyn Fn() -> String + Send>,
    /// `ctx.getSystemPromptOptions()`.
    pub get_system_prompt_options: Box<dyn Fn() -> Json + Send>,
    /// `ctx.abort()`.
    pub abort: Box<dyn Fn() + Send>,
    /// `ctx.shutdown()`.
    pub shutdown: Box<dyn Fn() + Send>,
    /// `ctx.hasPendingMessages()`.
    pub has_pending_messages: Box<dyn Fn() -> bool + Send>,
}

impl Default for HostActions {
    fn default() -> Self {
        Self {
            is_idle: Box::new(|| true),
            is_project_trusted: Box::new(|| true),
            get_model: Box::new(|| Json::Null),
            get_context_usage: Box::new(|| None),
            get_system_prompt: Box::new(String::new),
            get_system_prompt_options: Box::new(|| Json::Object(serde_json::Map::new())),
            abort: Box::new(|| {}),
            shutdown: Box::new(|| {}),
            has_pending_messages: Box::new(|| false),
        }
    }
}

/// The Rust-side state every op reads and writes. The runner keeps this
/// authoritative and syncs it from snapshots as extensions load.
#[derive(Default)]
pub struct HostCore {
    /// `ctx.cwd`.
    pub cwd: String,
    /// `ctx.mode` (`"print"` until the host binds another mode).
    pub mode: String,
    /// `pi.getSettings()`.
    pub settings: Json,
    /// The active tool names (the runner's loadout state).
    pub active_tools: Vec<String>,
    /// Tool names the runner aggregated (for active-set validation).
    pub registered_tool_names: Vec<String>,
    /// Hidden tools, which `setActiveTools` drops (§4.1).
    pub hidden_tool_names: Vec<String>,
    /// The `pi.getAllTools()` payload — the aggregated `ToolInfo` list the
    /// runner syncs from its snapshots (name, description, parameters,
    /// exposure, namespace in E1).
    pub all_tools: Vec<Json>,
    /// Flag values: CLI overrides then registration defaults, first-wins.
    pub flag_values: BTreeMap<String, Json>,
    /// The MCP server registry.
    pub mcp: McpRegistry,
    /// The provider / virtual-model registry (`pi.registerProvider` and
    /// friends).
    pub providers: ProviderRegistry,
    /// Data `operations.exec`'s `onData` callback streamed, in order,
    /// for the current `call_bash_exec`.
    pub bash_output: Vec<String>,
    /// Stream events a provider's `streamSimple` handler emitted through
    /// the `providerStreamEvent` op, in emission order, tagged with the
    /// session id of the stream call that produced them
    /// (`__piSessionCallProviderStream`). The runner drains one id after
    /// the session settles.
    pub provider_stream_events: Vec<(u32, Json)>,
    /// Provider registrations that failed validation at the bind-time
    /// flush, in flush order (the bindCore `emitError` reports).
    pub provider_bind_errors: Vec<crate::events::ProviderBindError>,
    /// `pi.appendEntry` calls, in order.
    pub entries: Vec<(String, Json)>,
    /// `pi.sendMessage` calls, in order.
    pub outbox: Vec<(String, Json, Json)>,
    /// `pi.setSessionName` value.
    pub session_name: Option<String>,
    /// `pi.setLabel` calls, in order.
    pub labels: Vec<(String, String)>,
    /// `pi.setThinkingLevel` value.
    pub thinking_level: Option<String>,
    /// `ctx.ui.notify` calls, in order (the headless UI log).
    pub ui_log: Vec<String>,
    /// Whether a real UI sink is bound (`ctx.hasUI`; `false` until
    /// `set_ui_context`).
    pub has_ui: bool,
    /// UI requests queued by dialog/fire-and-forget `ctx.ui` calls, in
    /// order; each entry is `{id, method, ...params}`. The runner drains
    /// and hands them to its sink.
    pub ui_requests: Vec<Json>,
    /// Monotonic id source for UI requests.
    pub ui_counter: u64,
    /// Tool `onUpdate` callbacks, in order.
    pub tool_updates: Vec<(String, Json)>,
    /// The host actions.
    pub actions: HostActions,
}

/// The single-threaded embedded engine.
pub struct JsHost {
    // Dropped before `rt` so the context never outlives its runtime.
    ctx: Context,
    rt: Runtime,
    core: Arc<Mutex<HostCore>>,
}

impl JsHost {
    /// Creates the runtime (256 MiB engine memory limit, the extension
    /// resolver/loader chain) and evaluates the bootstrap glue.
    pub fn new(core: HostCore) -> Result<Self, JsHostError> {
        let rt = Runtime::new()?;
        rt.set_memory_limit(256 * 1024 * 1024);
        rt.set_loader(ExtensionResolver, ExtensionLoader::default());
        let ctx = Context::full(&rt)?;
        // Single-threaded host: the shared core is `Rc`-shaped, but the ops
        // closures must live as `'js` engine values, so `Arc` it is.
        #[allow(clippy::arc_with_non_send_sync)]
        let core = Arc::new(Mutex::new(core));
        ctx.with(|ctx| Self::install_ops(&ctx, &core))?;
        ctx.with(|ctx| ctx.eval::<(), _>(crate::glue::GLUE))?;
        Ok(Self { ctx, rt, core })
    }

    /// The shared host state, for the runner to sync and assert against.
    pub fn core(&self) -> &Arc<Mutex<HostCore>> {
        &self.core
    }

    /// Drains pending engine jobs so promises settle before control
    /// returns to Rust (the E0 pattern; the job-error type is not publicly
    /// nameable in `rquickjs`, so the error is flattened to its message).
    pub fn drive_jobs(&self) -> Result<(), JsHostError> {
        while self
            .rt
            .execute_pending_job()
            .map_err(|err| JsHostError::Job(err.to_string()))?
        {}
        Ok(())
    }

    /// Maps an engine error to a host error, extracting the message of a
    /// pending JavaScript exception (`ctx.catch()`); everything else keeps
    /// its engine error.
    fn js_error(ctx: &rquickjs::Ctx<'_>, err: rquickjs::Error) -> JsHostError {
        if matches!(err, rquickjs::Error::Exception) {
            let caught = ctx.catch();
            let message = caught
                .into_object()
                .and_then(|object| object.get::<_, Option<String>>("message").ok())
                .flatten();
            match message {
                Some(message) => JsHostError::JsException(message),
                None => JsHostError::Js(err),
            }
        } else {
            JsHostError::Js(err)
        }
    }

    #[allow(clippy::too_many_lines)] // one op per upstream runtime method; split when extending
    fn install_ops(
        ctx: &rquickjs::Ctx<'_>,
        core: &Arc<Mutex<HostCore>>,
    ) -> Result<(), JsHostError> {
        ctx.globals()
            .set("__piOps", rquickjs::Object::new(ctx.clone())?)?;
        let ops: rquickjs::Object = ctx.globals().get("__piOps")?;
        macro_rules! op {
            ($name:literal, $capture:ident, || -> $ret:ty $body:block) => {{
                let $capture = core.clone();
                let function = Function::new(ctx.clone(), move || -> $ret { $body })?;
                ops.set($name, function)?;
            }};
            ($name:literal, $capture:ident, |$($arg:ident : $ty:ty),*| -> $ret:ty $body:block) => {{
                let $capture = core.clone();
                let function = Function::new(ctx.clone(), move |$($arg : $ty),*| -> $ret { $body })?;
                ops.set($name, function)?;
            }};
        }

        op!("print", _core, |level: String, text: String| -> String {
            eprintln!("[pi-ext-js {level}] {text}");
            String::new()
        });
        op!("refreshTools", _core, || -> String { String::new() });
        op!("flagDefault", core, |name: String,
                                  json: String|
         -> String {
            let mut core = core.lock().expect("host core mutex poisoned");
            if let Ok(value) = serde_json::from_str(&json) {
                core.flag_values.entry(name).or_insert(value);
            }
            String::new()
        });
        op!("getFlagValue", core, |name: String| -> String {
            let core = core.lock().expect("host core mutex poisoned");
            core.flag_values
                .get(&name)
                .map(|value| serde_json::to_string(value).unwrap_or_default())
                .unwrap_or_default()
        });
        op!("getAllTools", core, || -> String {
            let core = core.lock().expect("host core mutex poisoned");
            serde_json::to_string(&core.all_tools).unwrap_or_else(|_| "[]".to_string())
        });
        op!("getActiveTools", core, || -> String {
            let core = core.lock().expect("host core mutex poisoned");
            serde_json::to_string(&core.active_tools).unwrap_or_else(|_| "[]".to_string())
        });
        op!("setActiveTools", core, |json: String| -> String {
            let mut core = core.lock().expect("host core mutex poisoned");
            let requested: Vec<String> = serde_json::from_str(&json).unwrap_or_default();
            core.active_tools = requested
                .into_iter()
                .filter(|name| core.registered_tool_names.contains(name))
                .filter(|name| !core.hidden_tool_names.contains(name))
                .collect();
            String::new()
        });
        op!("getSettings", core, || -> String {
            let core = core.lock().expect("host core mutex poisoned");
            serde_json::to_string(&core.settings).unwrap_or_else(|_| "{}".to_string())
        });
        op!("appendEntry", core, |custom_type: String,
                                  json: String|
         -> String {
            let mut core = core.lock().expect("host core mutex poisoned");
            core.entries.push((
                custom_type,
                serde_json::from_str(&json).unwrap_or(Json::Null),
            ));
            String::new()
        });
        op!("sendMessage", core, |message: String,
                                  options: String|
         -> String {
            let mut core = core.lock().expect("host core mutex poisoned");
            core.outbox.push((
                "sendMessage".to_string(),
                serde_json::from_str(&message).unwrap_or(Json::Null),
                serde_json::from_str(&options).unwrap_or(Json::Null),
            ));
            String::new()
        });
        op!("sendUserMessage", core, |message: String,
                                      options: String|
         -> String {
            let mut core = core.lock().expect("host core mutex poisoned");
            core.outbox.push((
                "sendUserMessage".to_string(),
                serde_json::from_str(&message).unwrap_or(Json::Null),
                serde_json::from_str(&options).unwrap_or(Json::Null),
            ));
            String::new()
        });
        op!("exec", core, |command: String,
                           args: String,
                           options: String|
         -> String {
            let core = core.lock().expect("host core mutex poisoned");
            let cwd = core.cwd.clone();
            drop(core);
            let args: Vec<String> = serde_json::from_str(&args).unwrap_or_default();
            let options: Json = serde_json::from_str(&options).unwrap_or(Json::Null);
            serde_json::to_string(&exec::exec(&command, &args, &cwd, &options)).unwrap_or_else(
                |_| r#"{"execError":"exec result serialization failed"}"#.to_string(),
            )
        });
        op!("validateMcpServer", _core, |name: String,
                                         json: String|
         -> String {
            let config: Json = serde_json::from_str(&json).unwrap_or(Json::Null);
            crate::mcp_registry::validate_mcp_server_config(&name, &config)
                .err()
                .unwrap_or_default()
        });
        op!("mcpRegister", core, |name: String,
                                  ext_path: String,
                                  json: String|
         -> String {
            let mut core = core.lock().expect("host core mutex poisoned");
            let config: Json = serde_json::from_str(&json).unwrap_or(Json::Null);
            core.mcp
                .register(&name, &ext_path, config)
                .err()
                .unwrap_or_default()
        });
        op!("mcpUnregister", core, |name: String,
                                    ext_path: String|
         -> String {
            let mut core = core.lock().expect("host core mutex poisoned");
            core.mcp
                .unregister(&name, &ext_path)
                .err()
                .unwrap_or_default()
        });
        op!("getMcpServers", core, || -> String {
            let core = core.lock().expect("host core mutex poisoned");
            let servers: Vec<Json> = core
                .mcp
                .list()
                .iter()
                .map(|server| {
                    serde_json::json!({
                        "name": server.name,
                        "extensionPath": server.extension_path,
                        "config": server.config,
                    })
                })
                .collect();
            serde_json::to_string(&servers).unwrap_or_else(|_| "[]".to_string())
        });
        op!("cwd", core, || -> String {
            core.lock().expect("host core mutex poisoned").cwd.clone()
        });
        op!("mode", core, || -> String {
            core.lock().expect("host core mutex poisoned").mode.clone()
        });
        op!("hasUi", core, || -> String {
            if core.lock().expect("host core mutex poisoned").has_ui {
                "true".to_string()
            } else {
                "false".to_string()
            }
        });
        // A `ctx.ui` call crossing the boundary: the request JSON is
        // `{method, ...params}`; dialogs get the id back so their promise
        // can be resolved later via `__piUiResolve`.
        op!("uiRequest", core, |request: String| -> String {
            let mut core = core.lock().expect("host core mutex poisoned");
            core.ui_counter += 1;
            let id = format!("ui-{}", core.ui_counter);
            let mut value: Json = serde_json::from_str(&request).unwrap_or(Json::Null);
            if let Some(object) = value.as_object_mut() {
                object.shift_insert(0, "id".to_string(), Json::String(id.clone()));
            }
            core.ui_requests.push(value);
            id
        });
        op!("providerRegister", core, |name: String,
                                       extension_path: String,
                                       config: String|
         -> String {
            let mut core = core.lock().expect("host core mutex poisoned");
            let config: Json = serde_json::from_str(&config).unwrap_or(Json::Null);
            match core.providers.register(&name, &extension_path, &config) {
                Ok(()) => String::new(),
                Err(message) => message,
            }
        });
        op!("providerRegisterNative", core, |provider: String,
                                             extension_path: String|
         -> String {
            let mut core = core.lock().expect("host core mutex poisoned");
            let provider: Json = serde_json::from_str(&provider).unwrap_or(Json::Null);
            match core.providers.register_native(provider, &extension_path) {
                Ok(()) => String::new(),
                Err(message) => message,
            }
        });
        op!("providerUnregister", core, |name: String| -> String {
            core.lock()
                .expect("host core mutex poisoned")
                .providers
                .unregister(&name);
            String::new()
        });
        op!("virtualModelRegister", core, |definition: String,
                                           extension_path: String|
         -> String {
            let mut core = core.lock().expect("host core mutex poisoned");
            let definition: Json = serde_json::from_str(&definition).unwrap_or(Json::Null);
            match core
                .providers
                .register_virtual_model(definition, &extension_path)
            {
                Ok(()) => String::new(),
                Err(message) => message,
            }
        });
        op!("virtualModelUnregister", core, |provider: String,
                                             id: String|
         -> String {
            core.lock()
                .expect("host core mutex poisoned")
                .providers
                .unregister_virtual_model(&provider, &id);
            String::new()
        });
        // `operations.exec`'s `onData` callback: streaming data lands in
        // order for the current `call_bash_exec`.
        op!("bashData", core, |data: String| -> String {
            core.lock()
                .expect("host core mutex poisoned")
                .bash_output
                .push(data);
            String::new()
        });
        // A provider streamSimple handler's event emission
        // (`__piSessionCallProviderStream`'s `for await`): each protocol
        // event lands in order, tagged with the stream session id.
        op!("providerStreamEvent", core, |id: u32, event: String| -> String {
            core.lock()
                .expect("host core mutex poisoned")
                .provider_stream_events
                .push((id, serde_json::from_str(&event).unwrap_or(Json::Null)));
            String::new()
        });
        op!("getModel", core, || -> String {
            let core = core.lock().expect("host core mutex poisoned");
            let model = (core.actions.get_model)();
            serde_json::to_string(&model).unwrap_or_default()
        });
        op!("getThinkingLevel", core, || -> String {
            let core = core.lock().expect("host core mutex poisoned");
            core.thinking_level.clone().unwrap_or_default()
        });
        op!("setThinkingLevel", core, |level: String| -> String {
            let mut core = core.lock().expect("host core mutex poisoned");
            core.thinking_level = if level.is_empty() { None } else { Some(level) };
            String::new()
        });
        op!("isIdle", core, || -> bool {
            let core = core.lock().expect("host core mutex poisoned");
            (core.actions.is_idle)()
        });
        op!("isProjectTrusted", core, || -> bool {
            let core = core.lock().expect("host core mutex poisoned");
            (core.actions.is_project_trusted)()
        });
        op!("abort", core, || -> String {
            let core = core.lock().expect("host core mutex poisoned");
            (core.actions.abort)();
            String::new()
        });
        op!("hasPendingMessages", core, || -> bool {
            let core = core.lock().expect("host core mutex poisoned");
            (core.actions.has_pending_messages)()
        });
        op!("shutdown", core, || -> String {
            let core = core.lock().expect("host core mutex poisoned");
            (core.actions.shutdown)();
            String::new()
        });
        op!("getContextUsage", core, || -> String {
            let core = core.lock().expect("host core mutex poisoned");
            (core.actions.get_context_usage)()
                .map(|usage| serde_json::to_string(&usage).unwrap_or_default())
                .unwrap_or_default()
        });
        op!("compact", _core, |_options: String| -> String {
            String::new()
        });
        op!("getSystemPrompt", core, || -> String {
            let core = core.lock().expect("host core mutex poisoned");
            (core.actions.get_system_prompt)()
        });
        op!("getSystemPromptOptions", core, || -> String {
            let core = core.lock().expect("host core mutex poisoned");
            serde_json::to_string(&(core.actions.get_system_prompt_options)())
                .unwrap_or_else(|_| "{}".to_string())
        });
        op!("notify", core, |text: String| -> String {
            core.lock()
                .expect("host core mutex poisoned")
                .ui_log
                .push(text);
            String::new()
        });
        op!("toolUpdate", core, |tool_call_id: String,
                                 json: String|
         -> String {
            let mut core = core.lock().expect("host core mutex poisoned");
            core.tool_updates.push((
                tool_call_id,
                serde_json::from_str(&json).unwrap_or(Json::Null),
            ));
            String::new()
        });
        // Nested tool execution is the one surface the unbound runner
        // reports as unavailable; the exact result shape is
        // runner.ts:988-999. E2 binds a real executor with the agent loop.
        op!("nestedExecuteTool", _core, |tool_call_id: String,
                                         name: String,
                                         _args: String,
                                         _options: String|
         -> String {
            serde_json::json!({
                "toolCall": {
                    "type": "toolCall",
                    "id": format!("{tool_call_id}/0"),
                    "name": name,
                    "arguments": {},
                },
                "result": {
                    "content": [{ "type": "text", "text": "Nested tool calls are not available in this context" }],
                    "details": {},
                },
                "isError": true,
            })
            .to_string()
        });
        op!("getCallableTools", core, || -> String {
            let core = core.lock().expect("host core mutex poisoned");
            serde_json::to_string(&core.active_tools).unwrap_or_else(|_| "[]".to_string())
        });
        op!("getCommands", _core, || -> String {
            // The runner resolves commands from snapshots; the pi API
            // surface reads through the op for symmetry with upstream.
            "[]".to_string()
        });
        op!("setSessionName", core, |name: String| -> String {
            core.lock().expect("host core mutex poisoned").session_name = Some(name);
            String::new()
        });
        op!("getSessionName", core, || -> String {
            core.lock()
                .expect("host core mutex poisoned")
                .session_name
                .clone()
                .unwrap_or_default()
        });
        op!("setLabel", core, |entry_id: String,
                               label: String|
         -> String {
            core.lock()
                .expect("host core mutex poisoned")
                .labels
                .push((entry_id, label));
            String::new()
        });
        Ok(())
    }

    /// Extension lifecycle: allocates the JS-side record and returns its
    /// index (`__piBeginExtension`).
    pub fn begin_extension(&self, path: &str) -> Result<usize, JsHostError> {
        self.ctx.with(|ctx| -> Result<usize, JsHostError> {
            let begin: Function = ctx.globals().get("__piBeginExtension")?;
            Ok(begin.call((path,))?)
        })
    }

    /// Marks an extension active (`__piCommitExtension`).
    pub fn commit_extension(&self, index: usize) -> Result<(), JsHostError> {
        self.call_unit("__piCommitExtension", index)
    }

    /// Marks an extension failed and unsubscribes it (`__piDiscardExtension`).
    pub fn discard_extension(&self, index: usize) -> Result<(), JsHostError> {
        self.call_unit("__piDiscardExtension", index)
    }

    /// Loads one extension module and invokes its default-export factory
    /// with its `pi` API object. `source` overrides the file contents
    /// (tests); otherwise the file is read and TypeScript-stripped here
    /// (the entry module is declared directly; its imports resolve through
    /// the loader chain, which rquickjs drives inside `eval`).
    pub fn load_extension_module(
        &self,
        index: usize,
        path: &str,
        source: Option<&str>,
    ) -> Result<(), JsHostError> {
        let factory_result =
            self.ctx
                .with(|ctx| -> Result<Result<(), JsHostError>, JsHostError> {
                    let source = if let Some(inline) = source {
                        inline.to_string()
                    } else {
                        let bytes = std::fs::read(path).map_err(|err| {
                            JsHostError::Job(format!("failed to read {path}: {err}"))
                        })?;
                        crate::transpile::transpile(
                            Path::new(path),
                            &String::from_utf8_lossy(&bytes),
                        )
                        .map_err(|err| JsHostError::Job(err.to_string()))?
                    };
                    let (module, _promise) =
                        match rquickjs::Module::declare(ctx.clone(), path, source) {
                            Ok(declared) => match declared.eval() {
                                Ok(evaluated) => evaluated,
                                Err(err) => return Ok(Err(Self::js_error(&ctx, err))),
                            },
                            Err(err) => return Ok(Err(Self::js_error(&ctx, err))),
                        };
                    let namespace = module.namespace()?;
                    let factory: Option<Function> = namespace.get("default")?;
                    let Some(factory) = factory else {
                        return Ok(Err(JsHostError::MissingFactory(path.to_string())));
                    };
                    let create_api: Function = ctx.globals().get("__piCreateApi")?;
                    let pi: Value = create_api.call((index,))?;
                    if let Err(err) = factory.call::<(Value,), Value>((pi,)) {
                        return Ok(Err(Self::js_error(&ctx, err)));
                    }
                    Ok(Ok(()))
                })?;
        factory_result?;
        self.drive_jobs()?;
        Ok(())
    }

    /// The Rust runner's view of all loaded extensions (`__piInspect`).
    pub fn inspect(&self) -> Result<String, JsHostError> {
        self.ctx.with(|ctx| -> Result<String, JsHostError> {
            let inspect: Function = ctx.globals().get("__piInspect")?;
            inspect
                .call::<(), String>(())
                .map_err(|err| Self::js_error(&ctx, err))
        })
    }

    /// Marks the runtime bound: action methods work from now on (the
    /// runner calls this once after loading, mirroring `bindCore`).
    /// Before binding, the provider registrations queued during
    /// extension loading flush through the registry in order; a
    /// per-registration validation failure is recorded as a
    /// [`ProviderBindError`] instead of failing the load
    /// (`runner.ts:470-521`).
    pub fn bind_runtime(&self) -> Result<(), JsHostError> {
        let drained: String = self.ctx.with(|ctx| -> Result<String, JsHostError> {
            let drain: Function = ctx.globals().get("__piDrainPendingProviderQueues")?;
            drain
                .call::<(), String>(())
                .map_err(|err| Self::js_error(&ctx, err))
        })?;
        let queues: Json =
            serde_json::from_str(&drained).map_err(|err| JsHostError::Job(err.to_string()))?;
        {
            let mut core = self.core.lock().expect("host core mutex poisoned");
            if let Some(entries) = queues.get("providers").and_then(Json::as_array) {
                for entry in entries {
                    let name = entry.get("name").and_then(Json::as_str).unwrap_or_default();
                    let extension_path = entry
                        .get("extensionPath")
                        .and_then(Json::as_str)
                        .unwrap_or_default();
                    let config = serde_json::from_str(
                        entry
                            .get("configJson")
                            .and_then(Json::as_str)
                            .unwrap_or("null"),
                    )
                    .unwrap_or(Json::Null);
                    if let Err(error) = core.providers.register(name, extension_path, &config) {
                        core.provider_bind_errors
                            .push(crate::events::ProviderBindError {
                                extension_path: extension_path.to_string(),
                                event: "register_provider".to_string(),
                                error,
                            });
                    }
                }
            }
            if let Some(entries) = queues.get("nativeProviders").and_then(Json::as_array) {
                for entry in entries {
                    let extension_path = entry
                        .get("extensionPath")
                        .and_then(Json::as_str)
                        .unwrap_or_default();
                    let provider = serde_json::from_str(
                        entry
                            .get("providerJson")
                            .and_then(Json::as_str)
                            .unwrap_or("null"),
                    )
                    .unwrap_or(Json::Null);
                    if let Err(error) = core.providers.register_native(provider, extension_path) {
                        core.provider_bind_errors
                            .push(crate::events::ProviderBindError {
                                extension_path: extension_path.to_string(),
                                event: "register_provider".to_string(),
                                error,
                            });
                    }
                }
            }
            if let Some(entries) = queues.get("virtualModels").and_then(Json::as_array) {
                for entry in entries {
                    let extension_path = entry
                        .get("extensionPath")
                        .and_then(Json::as_str)
                        .unwrap_or_default();
                    let definition = serde_json::from_str(
                        entry
                            .get("definitionJson")
                            .and_then(Json::as_str)
                            .unwrap_or("null"),
                    )
                    .unwrap_or(Json::Null);
                    if let Err(error) = core
                        .providers
                        .register_virtual_model(definition, extension_path)
                    {
                        core.provider_bind_errors
                            .push(crate::events::ProviderBindError {
                                extension_path: extension_path.to_string(),
                                event: "register_virtual_model".to_string(),
                                error,
                            });
                    }
                }
            }
        }
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let bind: Function = ctx.globals().get("__piBindRuntime")?;
            bind.call::<(), ()>(())
                .map_err(|err| Self::js_error(&ctx, err))
        })
    }

    /// Applies the staleness protocol (`__piInvalidate`).
    pub fn invalidate(&self, message: Option<&str>) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let invalidate: Function = ctx.globals().get("__piInvalidate")?;
            let _: Value = invalidate
                .call((message,))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Begins a dispatch session: materializes the event as a live JS
    /// object with the kind-appropriate context, returns the session id.
    pub fn session_begin(
        &self,
        event_json: &str,
        kind: ContextKind,
        tool_call_id: Option<&str>,
    ) -> Result<u32, JsHostError> {
        let kind = match kind {
            ContextKind::Event => "event",
            ContextKind::Command => "command",
            ContextKind::Tool => "tool",
            ContextKind::ProjectTrust => "trust",
            ContextKind::Route => "route",
            ContextKind::ProviderStream => "providerStream",
        };
        self.ctx.with(|ctx| -> Result<u32, JsHostError> {
            let begin: Function = ctx.globals().get("__piSessionBegin")?;
            begin
                .call::<(String, String, Option<String>), u32>((
                    event_json.to_string(),
                    kind.to_string(),
                    tool_call_id.map(str::to_string),
                ))
                .map_err(|err| Self::js_error(&ctx, err))
        })
    }

    /// Calls one event handler inside a session (`__piSessionCall`); the
    /// result lands in the session slot after the job queue drains.
    pub fn session_call(
        &self,
        id: u32,
        index: usize,
        event: &str,
        handler: usize,
    ) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx.globals().get("__piSessionCall")?;
            let _: Value = call
                .call((id, index, event, handler))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Calls one command handler inside a session.
    pub fn session_call_command(
        &self,
        id: u32,
        index: usize,
        name: &str,
        args: &str,
    ) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx.globals().get("__piSessionCallCommand")?;
            let _: Value = call
                .call((id, index, name, args))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Calls one tool's `execute` inside a session
    /// (`__piSessionCallTool`); the session event holds the tool args.
    pub fn session_call_tool(
        &self,
        id: u32,
        index: usize,
        name: &str,
        tool_call_id: &str,
    ) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx.globals().get("__piSessionCallTool")?;
            let _: Value = call
                .call((id, index, name, tool_call_id))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Calls a virtual model's `route` inside a session
    /// (`__piSessionCallRoute`); the session event is the routing request
    /// the host built, the reply lands in the session slot.
    pub fn session_call_route(
        &self,
        id: u32,
        provider: &str,
        model_id: &str,
    ) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx.globals().get("__piSessionCallRoute")?;
            let _: Value = call
                .call((id, provider, model_id))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Calls a provider's stashed `streamSimple` inside a session
    /// (`__piSessionCallProviderStream`); the session event is the
    /// marshalled `{model, context, options}` request, events stream
    /// through the `providerStreamEvent` op, and the slot settles when
    /// iteration finishes.
    pub fn session_call_provider_stream(
        &self,
        id: u32,
        provider: &str,
    ) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx.globals().get("__piSessionCallProviderStream")?;
            let _: Value = call
                .call((id, provider))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Takes the stream events the `providerStreamEvent` op queued for one
    /// session id, in emission order; other sessions' events are kept.
    pub fn take_provider_stream_events(&self, id: u32) -> Vec<Json> {
        let mut core = self.core.lock().expect("host core mutex poisoned");
        let mut events = Vec::new();
        let mut rest = Vec::new();
        for (queued, event) in std::mem::take(&mut core.provider_stream_events) {
            if queued == id {
                events.push(event);
            } else {
                rest.push((queued, event));
            }
        }
        core.provider_stream_events = rest;
        events
    }

    /// Reads the session result slot (`__piSessionLast`): a JSON object
    /// `{done, error?, value?}`.
    pub fn session_last(&self, id: u32) -> Result<String, JsHostError> {
        self.read_string_function("__piSessionLast", id)
    }

    /// Reads the session's (possibly handler-mutated) event as JSON.
    pub fn session_event(&self, id: u32) -> Result<String, JsHostError> {
        self.read_string_function("__piSessionEvent", id)
    }

    /// Replaces one key of the session event with a JSON value (the
    /// boundary preview rebuild, `runner.ts:1074-1086`).
    pub fn session_patch(&self, id: u32, key: &str, value_json: &str) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let patch: Function = ctx.globals().get("__piSessionPatch")?;
            let _: Value = patch
                .call((id, key, value_json))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Ends a dispatch session, releasing its live objects.
    pub fn session_end(&self, id: u32) -> Result<(), JsHostError> {
        self.call_unit("__piSessionEnd", id as usize)
    }

    /// Binds (or unbinds) the UI surface: `ctx.mode` and `ctx.hasUI` read
    /// these lazily through the ops, so no JS push is needed.
    pub fn set_ui_mode(&self, mode: &str, has_ui: bool) {
        let mut core = self.core.lock().expect("host core mutex poisoned");
        core.mode = mode.to_string();
        core.has_ui = has_ui;
    }

    /// Takes the UI requests queued since the last drain
    /// (`{id, method, ...params}`, in call order).
    pub fn drain_ui_requests(&self) -> Vec<Json> {
        let mut core = self.core.lock().expect("host core mutex poisoned");
        std::mem::take(&mut core.ui_requests)
    }

    /// Settles one pending dialog promise (`__piUiResolve`): the payload
    /// is a JSON-encoded `RpcExtensionUIResponse` shape
    /// (`{value}`, `{confirmed}`, `{cancelled:true}`, or `{}` for the
    /// sink-side default). The caller drives jobs afterwards.
    pub fn resolve_ui(&self, id: &str, payload_json: &str) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let resolve: Function = ctx.globals().get("__piUiResolve")?;
            let _: Value = resolve
                .call((id, payload_json))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Calls the stashed `user_bash` `operations.exec` (`__piCallBashOperations`)
    /// and waits for its promise through the job queue. Returns the JSON
    /// result (`{exitCode}` or `{error}`); streamed `onData` chunks sit
    /// in `HostCore::bash_output` afterwards.
    pub fn call_bash_exec(
        &self,
        command: &str,
        cwd: &str,
        options_json: &str,
    ) -> Result<Json, JsHostError> {
        {
            let mut core = self.core.lock().expect("host core mutex poisoned");
            core.bash_output.clear();
        }
        {
            self.ctx.with(|ctx| -> Result<(), JsHostError> {
                let call: Function = ctx.globals().get("__piCallBashOperations")?;
                let _: Value = call
                    .call((command, cwd, options_json))
                    .map_err(|err| Self::js_error(&ctx, err))?;
                Ok(())
            })?;
        }
        // Bounded poll: the bridge promise settles in one or two job
        // drains, so a lingering null slot means a handler's `exec` chain
        // stalled; fail fast with the engine state instead of spinning
        // (a flaky hang was observed twice under parallel test threads).
        let mut probes: u32 = 0;
        loop {
            self.drive_jobs()?;
            let ready: bool = self.ctx.with(|ctx| -> Result<bool, JsHostError> {
                ctx.eval("__piBashReady()")
                    .map_err(|err| Self::js_error(&ctx, err))
            })?;
            if ready {
                break;
            }
            probes += 1;
            if probes > 100_000 {
                let state = self.ctx.with(|ctx| -> Result<String, JsHostError> {
                    ctx.eval(
                        "JSON.stringify({textNull: globalThis.__piBashText === null, \
                         operations: typeof globalThis.__piBashOperations, \
                         exec: globalThis.__piBashOperations \
                           ? typeof globalThis.__piBashOperations.exec : \"none\"})",
                    )
                    .map_err(|err| Self::js_error(&ctx, err))
                })?;
                return Err(JsHostError::HandlerDidNotComplete(format!(
                    "bash operations.exec did not settle after 100k job drains; engine state: {state}"
                )));
            }
        }
        let text: String = self.ctx.with(|ctx| -> Result<String, JsHostError> {
            ctx.eval("globalThis.__piBashText")
                .map_err(|err| Self::js_error(&ctx, err))
        })?;
        Ok(serde_json::from_str(&text).unwrap_or(Json::Null))
    }

    /// Evaluates one JS source string (tests and capability probes):
    /// string results come back raw, everything else JSON-encoded.
    pub fn eval_for_tests(&self, source: &str) -> Result<String, JsHostError> {
        self.ctx.with(|ctx| -> Result<String, JsHostError> {
            let value: Value = ctx.eval(source)?;
            if value.is_string() {
                let text: String = value.get()?;
                return Ok(text);
            }
            let json = ctx.json_stringify(value)?;
            Ok(json
                .map(|json| {
                    json.to_string()
                        .map_err(JsHostError::from)
                        .unwrap_or_default()
                })
                .unwrap_or_default())
        })
    }

    fn call_unit(&self, name: &str, arg: usize) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let function: Function = ctx.globals().get(name)?;
            function
                .call::<(i32,), ()>((arg as i32,))
                .map_err(|err| Self::js_error(&ctx, err))
        })
    }

    fn read_string_function(&self, name: &str, arg: u32) -> Result<String, JsHostError> {
        self.ctx.with(|ctx| -> Result<String, JsHostError> {
            let function: Function = ctx.globals().get(name)?;
            function
                .call((arg,))
                .map_err(|err| Self::js_error(&ctx, err))
        })
    }
}

/// Paths are absolute after loader resolution; the cache key is the path.
pub type TranspileCache = BTreeMap<PathBuf, String>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn host() -> JsHost {
        let core = HostCore {
            cwd: "/tmp".to_string(),
            mode: "print".to_string(),
            settings: serde_json::json!({"model": "glm-5.3-flash"}),
            ..Default::default()
        };
        JsHost::new(core).expect("host")
    }

    #[test]
    fn intl_segmenter_shim_constructs_and_segments() {
        let host = host();
        let out = host
            .eval_for_tests(
                r#"Array.from(new Intl.Segmenter("en", {granularity: "word"}).segment("hello world"), s => s.segment).join("|")"#,
            )
            .expect("eval");
        assert_eq!(out, "hello|world");
    }

    #[test]
    fn console_shim_exists() {
        let host = host();
        assert_eq!(
            host.eval_for_tests("typeof console.error").expect("eval"),
            "function"
        );
    }

    #[test]
    fn loads_module_with_relative_import_and_virtual_module() {
        let dir = std::env::temp_dir().join(format!("pi-ext-js-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("helper.ts"), "export const tag: string = \"h\";\n")
            .expect("write");
        std::fs::write(
            dir.join("main.ts"),
            "import { tag } from \"./helper.ts\";\nimport { Type } from \"typebox\";\nexport default function (pi: unknown): void {\n  (globalThis as Record<string, unknown>).__loaded = tag + Type.String({ description: \"d\" }).type;\n}\n",
        )
        .expect("write");
        let host = host();
        let index = host
            .begin_extension(&dir.join("main.ts").to_string_lossy())
            .expect("begin");
        host.load_extension_module(index, &dir.join("main.ts").to_string_lossy(), None)
            .expect("load");
        host.commit_extension(index).expect("commit");
        let out = host.eval_for_tests("globalThis.__loaded").expect("eval");
        assert_eq!(out, "hstring");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn node_imports_error_with_the_e4_message() {
        let dir = std::env::temp_dir().join(format!("pi-ext-js-node-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("main.ts"),
            "import { readFileSync } from \"node:fs\";\nexport default function (): void {\n  globalThis.__used = typeof readFileSync;\n}\n",
        )
        .expect("write");
        let host = host();
        let index = host
            .begin_extension(&dir.join("main.ts").to_string_lossy())
            .expect("begin");
        let err = host
            .load_extension_module(index, &dir.join("main.ts").to_string_lossy(), None)
            .expect_err("node import must fail");
        assert!(err.to_string().contains("land in E4"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn async_factory_and_async_handlers_settle_through_the_job_queue() {
        let host = host();
        let index = host.begin_extension("<test>").expect("begin");
        host.load_extension_module(
            index,
            "<test>",
            Some(
                r#"
export default async function (pi) {
  pi.on("e", async (event) => {
    await Promise.resolve();
    return "done:" + event.value;
  });
}
"#,
            ),
        )
        .expect("load");
        host.commit_extension(index).expect("commit");
        let id = host
            .session_begin(r#"{"value": 1}"#, ContextKind::Event, None)
            .expect("begin");
        host.session_call(id, index, "e", 0).expect("call");
        host.drive_jobs().expect("drive");
        assert_eq!(
            host.session_last(id).expect("last"),
            r#"{"done":true,"value":"done:1"}"#
        );
        host.session_end(id).expect("end");
    }

    #[test]
    fn in_place_event_mutation_chains_across_handlers() {
        let host = host();
        let index = host.begin_extension("<test>").expect("begin");
        host.load_extension_module(
            index,
            "<test>",
            Some(
                r#"
export default function (pi) {
  pi.on("tool_call", (event) => {
    event.input.seen = [event.input.first];
    event.input.first = "patched";
  });
  pi.on("tool_call", (event) => event.input);
}
"#,
            ),
        )
        .expect("load");
        host.commit_extension(index).expect("commit");
        let id = host
            .session_begin(
                r#"{"type":"tool_call","input":{"first":"original"}}"#,
                ContextKind::Event,
                None,
            )
            .expect("begin");
        host.session_call(id, index, "tool_call", 0)
            .expect("call 1");
        host.drive_jobs().expect("drive");
        host.session_call(id, index, "tool_call", 1)
            .expect("call 2");
        host.drive_jobs().expect("drive");
        // The second handler saw the first handler's in-place writes.
        assert_eq!(
            host.session_last(id).expect("last"),
            r#"{"done":true,"value":{"first":"patched","seen":["original"]}}"#
        );
        host.session_end(id).expect("end");
    }

    #[test]
    fn staleness_invalidation_throws_from_ctx_and_pi() {
        let host = host();
        let index = host.begin_extension("<test>").expect("begin");
        host.load_extension_module(
            index,
            "<test>",
            Some(
                r#"
export default function (pi) {
  pi.on("e", async (event, ctx) => {
    try { ctx.cwd; return "ctx-ok"; } catch (error) { return "ctx-stale:" + error.message.slice(0, 24); }
  });
  pi.on("pi", async () => {
    try { pi.getSettings(); return "pi-ok"; } catch (error) { return "pi-stale"; }
  });
}
"#,
            ),
        )
        .expect("load");
        host.commit_extension(index).expect("commit");
        host.invalidate(None).expect("invalidate");
        for (event, expected) in [("e", "ctx-stale:This extension ctx is"), ("pi", "pi-stale")] {
            let id = host
                .session_begin("{}", ContextKind::Event, None)
                .expect("begin");
            host.session_call(id, index, event, 0).expect("call");
            host.drive_jobs().expect("drive");
            let last = host.session_last(id).expect("last");
            assert!(last.contains(expected), "{event}: {last}");
            host.session_end(id).expect("end");
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // exercises the full op surface in one host
    fn ops_serve_settings_flags_tools_and_mcp() {
        let host = host();
        let index = host.begin_extension("<test>").expect("begin");
        host.load_extension_module(
            index,
            "<test>",
            Some(
                r#"
import { Type } from "typebox";
export default function (pi) {
  pi.registerTool({
    name: "synthetic_tool",
    label: "synthetic_tool",
    description: "A synthetic tool.",
    parameters: Type.Object({ query: Type.String({ description: "q" }), limit: Type.Optional(Type.Number()) }),
    exposure: "deferred",
    execute: async () => ({ content: [{ type: "text", text: "ran" }], details: { ok: true } }),
  });
  pi.registerFlag("my-flag", { type: "boolean", default: true });
  pi.registerMcpServer("synth", { command: "synthd" });
  pi.on("probe", async () => ({
    tools: pi.getAllTools(),
    active: pi.getActiveTools(),
    settings: pi.getSettings(),
    mcp: pi.getMcpServers(),
    flag: pi.getFlag("my-flag"),
  }));
  pi.on("duringLoad", async () => pi.getAllTools());
  try {
    pi.getAllTools();
    globalThis.__preBind = "ok";
  } catch (error) {
    globalThis.__preBind = error.message;
  }
}
"#,
            ),
        )
        .expect("load");
        host.commit_extension(index).expect("commit");
        // Sync the aggregated state the ops serve from (the runner does
        // this from __piInspect) and bind, exactly like bindCore.
        {
            let mut core = host.core().lock().expect("core");
            core.registered_tool_names = vec!["synthetic_tool".to_string()];
            core.active_tools = vec!["synthetic_tool".to_string()];
            core.all_tools = vec![serde_json::json!({
                "name": "synthetic_tool",
                "description": "A synthetic tool.",
                "exposure": "deferred",
            })];
        }
        // Action methods throw before the bind: the factory's own try/catch
        // recorded the not-initialized error.
        assert_eq!(
            host.eval_for_tests("globalThis.__preBind").expect("eval"),
            "Extension runtime not initialized. Action methods cannot be called during extension loading."
        );
        host.bind_runtime().expect("bind");

        let id = host
            .session_begin("{}", ContextKind::Event, None)
            .expect("begin");
        host.session_call(id, index, "probe", 0).expect("call");
        host.drive_jobs().expect("drive");
        let probe: serde_json::Value =
            serde_json::from_str(&host.session_last(id).expect("last")).expect("probe json");
        host.session_end(id).expect("end");
        let probe = probe["value"].clone();
        // The flag default and the settings map the ops serve from.
        assert_eq!(probe["flag"], serde_json::json!(true));
        assert_eq!(
            probe["settings"],
            serde_json::json!({"model": "glm-5.3-flash"})
        );
        assert_eq!(probe["active"], serde_json::json!(["synthetic_tool"]));
        assert!(
            probe["mcp"]
                .as_array()
                .expect("servers")
                .iter()
                .any(|server| server["name"] == "synth")
        );
        let tool = &probe["tools"].as_array().expect("tools")[0];
        assert_eq!(tool["name"], "synthetic_tool");
        assert_eq!(tool["exposure"], "deferred");
        // The registered schema (from the snapshot's live registry) must be
        // byte-identical to real TypeBox output: type, required
        // (non-optional only), properties — options last, kind markers
        // non-enumerable.
        let snapshots: serde_json::Value =
            serde_json::from_str(&host.inspect().expect("inspect")).expect("snapshots");
        let schema = &snapshots.as_array().expect("list")[index]["tools"]
            .as_array()
            .expect("tools")[0]["parameters"];
        assert_eq!(
            schema,
            &serde_json::json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": {"type": "string", "description": "q"},
                    "limit": {"type": "number"},
                },
            })
        );
        // Tool execution through the session machinery.
        let id = host
            .session_begin(r#"{"query":"x"}"#, ContextKind::Tool, Some("t1"))
            .expect("begin");
        host.session_call_tool(id, index, "synthetic_tool", "t1")
            .expect("call");
        host.drive_jobs().expect("drive");
        assert_eq!(
            host.session_last(id).expect("last"),
            r#"{"done":true,"value":{"content":[{"type":"text","text":"ran"}],"details":{"ok":true}}}"#
        );
        host.session_end(id).expect("end");
    }

    #[test]
    fn tool_schema_rejection_matches_upstream_message() {
        let host = host();
        let index = host.begin_extension("<test>").expect("begin");
        let err = host
            .load_extension_module(
                index,
                "<test>",
                Some(
                    r#"
export default function (pi) {
  pi.registerTool({ name: "bad", description: "", parameters: null });
}
"#,
                ),
            )
            .expect_err("bad schema must fail");
        assert!(
            err.to_string()
                .contains("Tool \"bad\" registered by extension \"<test>\" must define an object parameter schema."),
            "{err}"
        );
        host.discard_extension(index).expect("discard");
    }

    // ------------------------------------------------------------------
    // E1 exit gate, part 1: the vendored upstream tool-search module
    // (tests/fixtures/tool-search) through the full pipeline — transpile,
    // relative imports, virtual modules (`typebox`,
    // `@earendil-works/pi-codemode/declarations`), the `Intl.Segmenter`
    // shim. The cases port `packages/coding-agent/test/tool-search.test.ts`;
    // ICU-dependent segmentation cases are E4 deviations (see the ignored
    // test below).
    // ------------------------------------------------------------------

    fn fixture_path(relative: &str) -> String {
        format!("{}/tests/fixtures/{relative}", env!("CARGO_MANIFEST_DIR"))
    }

    /// Imports the vendored tool.ts through the loader chain and installs
    /// the test helpers the ported cases use.
    fn load_vendored_tool_module(host: &JsHost) {
        let path = fixture_path("tool-search/tool.ts");
        host.ctx
            .with(|ctx| {
                ctx.eval::<(), _>(format!(
                    "(async () => {{ globalThis.__ts = await import({}); }})()",
                    serde_json::to_string(&path).expect("path json")
                ))
            })
            .expect("import eval");
        host.drive_jobs().expect("drive");
        host.ctx
            .with(|ctx| {
                ctx.eval::<(), _>(
                    r"
globalThis.__mkdoc = (name, description, parameters, namespace) =>
  __ts.createToolSearchDocument({ name, description, parameters }, namespace);
globalThis.__rank = (query, documents, limit) =>
  new __ts.Bm25Ranker().rank(query, documents, limit).map((match) => match.name);
globalThis.__doctext = (name, description, parameters) =>
  __ts.createToolSearchDocument({ name, description, parameters }).text;
",
                )
            })
            .expect("helpers eval");
    }

    fn eval_json(host: &JsHost, source: &str) -> serde_json::Value {
        serde_json::from_str(&host.eval_for_tests(source).expect("eval")).expect("json")
    }

    #[test]
    fn vendored_tokenizer_matches_upstream_ascii_and_shim_capable_unicode() {
        let host = host();
        load_vendored_tool_module(&host);
        // "splits camelCase and snake_case, drops stop words, and folds plurals"
        assert_eq!(
            eval_json(
                &host,
                r#"JSON.stringify(__ts.tokenize("listIssues for the GitHub_repo"))"#
            ),
            json!(["list", "issue", "git", "hub", "repo"])
        );
        assert_eq!(
            eval_json(
                &host,
                r#"JSON.stringify(__ts.tokenize("searches queries HTTPServer"))"#
            ),
            json!(["search", "query", "http", "server"])
        );
        assert_eq!(
            eval_json(
                &host,
                r#"JSON.stringify(__ts.tokenize("repo.issues/search:queries"))"#
            ),
            json!(["repo", "issue", "search", "query"])
        );
        // Unicode words the property-run shim segments identically to ICU.
        assert_eq!(
            eval_json(&host, r#"JSON.stringify(__ts.tokenize("Éléments cafés"))"#),
            json!(["éléments", "cafés"])
        );
        assert_eq!(
            eval_json(&host, r#"JSON.stringify(__ts.tokenize("문서 검색"))"#),
            json!(["문서", "검색"])
        );
        assert_eq!(
            eval_json(
                &host,
                r#"JSON.stringify(__ts.tokenize("Поиск документов"))"#
            ),
            json!(["поиск", "документов"])
        );
        assert_eq!(
            eval_json(&host, r#"JSON.stringify(__ts.tokenize("CAFÉ cafe\u0301"))"#),
            json!(["café", "café"])
        );
        assert_eq!(
            eval_json(&host, r#"JSON.stringify(__ts.tokenize(" \t，。!? 😀 "))"#),
            json!([])
        );
    }

    #[test]
    #[ignore = "ICU word segmentation: QuickJS-NG ships no Intl, the shim segments unspaced scripts as one run (E4 replaces the shim with a real Intl subset)"]
    fn vendored_tokenizer_unspaced_scripts_need_icu() {
        let host = host();
        load_vendored_tool_module(&host);
        assert_eq!(
            eval_json(&host, r#"JSON.stringify(__ts.tokenize("搜索仓库问题"))"#),
            json!(["搜索", "仓库", "问题"])
        );
    }

    #[test]
    fn vendored_ranker_ranks_by_term_relevance_and_respects_the_limit() {
        let host = host();
        load_vendored_tool_module(&host);
        // The upstream test corpus (`tool-search.test.ts:59-67`).
        host.eval_for_tests(
            r#"
globalThis.__docs = [
  __mkdoc("mcp__github__list_issues", "List issues in a repository.", { type: "object", properties: { state: { type: "string", description: "open or closed" } } }),
  __mkdoc("mcp__github__create_pull_request", "Open a pull request.", { type: "object", properties: {} }),
  __mkdoc("mcp__linear__search_issues", "Search Linear issues by text.", { type: "object", properties: {} }),
  __mkdoc("mcp__docs__search", "Search the documentation.", { type: "object", properties: {} }),
];
"#,
        )
        .expect("corpus");
        assert_eq!(
            eval_json(&host, r#"JSON.stringify(__rank("issue", __docs, 8))"#),
            json!(["mcp__linear__search_issues", "mcp__github__list_issues"])
        );
        assert_eq!(
            eval_json(
                &host,
                r#"JSON.stringify(__rank("pull requests", __docs, 8)[0])"#
            ),
            json!("mcp__github__create_pull_request")
        );
        assert_eq!(
            eval_json(&host, r#"__rank("search", __docs, 1).length"#),
            json!(1)
        );
        // Property names and descriptions are searchable.
        assert_eq!(
            eval_json(&host, r#"JSON.stringify(__rank("closed", __docs, 8))"#),
            json!(["mcp__github__list_issues"])
        );
        // Unknown, stop-word, and known-v1-limit queries find nothing.
        assert_eq!(
            eval_json(&host, r#"JSON.stringify(__rank("kubernetes", __docs, 8))"#),
            json!([])
        );
        assert_eq!(
            eval_json(&host, r#"JSON.stringify(__rank("the", __docs, 8))"#),
            json!([])
        );
        assert_eq!(
            eval_json(&host, r#"JSON.stringify(__rank("tickets", __docs, 8))"#),
            json!([])
        );
    }

    #[test]
    fn vendored_ranker_name_queries_rank_first() {
        let host = host();
        load_vendored_tool_module(&host);
        host.eval_for_tests(
            r#"
globalThis.__named = [
  __mkdoc("mcp__app__queue_read", "Read a queued message of this thread."),
  __mkdoc("mcp__dev-app__thread_read", "Read durable state and a paginated timeline from any thread. The default view returns user messages, assistant messages and plans. Continue with afterPosition until hasMore is false."),
];
"#,
        )
        .expect("corpus");
        for query in [
            "thread_read",
            "dev_app__thread_read",
            "mcp__dev-app__thread_read",
            "Thread_Read",
            "thread read",
        ] {
            let source = format!("JSON.stringify(__rank({query:?}, __named, 1))");
            assert_eq!(
                eval_json(&host, &source),
                json!(["mcp__dev-app__thread_read"]),
                "query: {query}"
            );
        }
        // Partial names still rank by BM25.
        assert_eq!(
            eval_json(&host, r#"JSON.stringify(__rank("read", __named, 1))"#),
            json!(["mcp__app__queue_read"])
        );
    }

    #[test]
    fn vendored_ranker_name_match_beats_description_mentions() {
        let host = host();
        load_vendored_tool_module(&host);
        host.eval_for_tests(
            r#"
globalThis.__ci = [
  __mkdoc("mcp__ci__status", "Deploy status: last deploy, deploy log, deploy errors."),
  __mkdoc("mcp__ci__deploy_release", "Start a release of the current branch to an environment, run the migrations, switch traffic over, and wait until the new version is live and healthy."),
];
"#,
        )
        .expect("corpus");
        assert_eq!(
            eval_json(&host, r#"JSON.stringify(__rank("deploy", __ci, 1))"#),
            json!(["mcp__ci__deploy_release"])
        );
    }

    #[test]
    fn vendored_ranker_whole_name_bonus_and_coverage() {
        let host = host();
        load_vendored_tool_module(&host);
        host.eval_for_tests(
            r#"
const peekaboo = { name: "mcp__peekaboo", instructions: "Peekaboo controls macOS apps by app name." };
globalThis.__peek = [
  __mkdoc("mcp__peekaboo__see", "Capture the app window of the peekaboo app and describe the app.", {}, peekaboo),
  __mkdoc("mcp__peekaboo__menu", "Click a menu item of an app.", {}, peekaboo),
  __mkdoc("mcp__peekaboo__app", "Launch, quit, relaunch, focus, hide, or switch applications. Targets an application by name, bundle identifier, or process ID, and can wait until it is ready.", {}, peekaboo),
  __mkdoc("mcp__app__queue_read", "Read a queued message of this thread."),
  __mkdoc("mcp__dev-app__thread_read", "Read durable state and a paginated timeline from any thread. The default view returns user messages, assistant messages and plans."),
];
"#,
        )
        .expect("corpus");
        assert_eq!(
            eval_json(
                &host,
                r#"JSON.stringify(__rank("peekaboo app", __peek, 1))"#
            ),
            json!(["mcp__peekaboo__app"])
        );
        assert_eq!(
            eval_json(
                &host,
                r#"JSON.stringify(__rank("open the peekaboo app tool", __peek, 1))"#
            ),
            json!(["mcp__peekaboo__app"])
        );
        assert_eq!(
            eval_json(
                &host,
                r#"JSON.stringify(__rank("read thread_read state", __peek, 1))"#
            ),
            json!(["mcp__dev-app__thread_read"])
        );
        // Without the whole name, mentions in descriptions decide.
        assert_eq!(
            eval_json(
                &host,
                r#"JSON.stringify(__rank("describe window", __peek, 1))"#
            ),
            json!(["mcp__peekaboo__see"])
        );
    }

    #[test]
    fn vendored_ranker_more_covered_terms_win() {
        let host = host();
        load_vendored_tool_module(&host);
        host.eval_for_tests(
            r#"
globalThis.__four = [
  __mkdoc("mcp__peekaboo__app", "Launch, quit, focus, or switch applications by name or bundle identifier."),
  __mkdoc("mcp__peekaboo__see", "Capture the app window and describe the UI elements of the app."),
  __mkdoc("mcp__peekaboo__window", "Move, resize, or close a window of an app."),
  __mkdoc("mcp__peekaboo__image", "Take a screenshot of the screen, an app, or a window."),
];
"#,
        )
        .expect("corpus");
        // `window` and `app` are whole names, but each covers one term.
        assert_eq!(
            eval_json(
                &host,
                r#"JSON.stringify(__rank("screenshot window", __four, 1))"#
            ),
            json!(["mcp__peekaboo__image"])
        );
        assert_eq!(
            eval_json(
                &host,
                r#"JSON.stringify(__rank("describe app", __four, 1))"#
            ),
            json!(["mcp__peekaboo__see"])
        );
        // An exact name still ranks first.
        assert_eq!(
            eval_json(&host, r#"JSON.stringify(__rank("window", __four, 1))"#),
            json!(["mcp__peekaboo__window"])
        );
    }

    #[test]
    fn vendored_ranker_includes_the_namespace_in_the_search_text() {
        let host = host();
        load_vendored_tool_module(&host);
        host.eval_for_tests(
            r#"
globalThis.__nsdoc = [__mkdoc("mcp__x__run", "Run it.", {}, { name: "mcp__x", description: "Kubernetes cluster tools" })];
"#,
        )
        .expect("corpus");
        assert_eq!(
            eval_json(&host, r#"JSON.stringify(__rank("kubernetes", __nsdoc, 8))"#),
            json!(["mcp__x__run"])
        );
    }

    #[test]
    fn vendored_schema_text_reads_local_references() {
        let host = host();
        load_vendored_tool_module(&host);
        // The #11803 cases, verbatim expectations.
        assert_eq!(
            eval_json(
                &host,
                r##"JSON.stringify(__doctext("mcp__ledger__inspect", "Inspect records", { type: "object", properties: { options: { $ref: "#/$defs/options" } }, $defs: { options: { type: "object", properties: { mode: { type: "string", description: "reconciliation" } } } } }))"##
            ),
            json!("Inspect records options mode reconciliation")
        );
        assert_eq!(
            eval_json(
                &host,
                r##"JSON.stringify(__doctext("mcp__ledger__inspect", "Inspect records", { type: "object", properties: { range: { $ref: "#/$defs/date~1range" } }, $defs: { "date/range": { description: "fiscal period" } } }))"##
            ),
            json!("Inspect records range fiscal period")
        );
        assert_eq!(
            eval_json(
                &host,
                r##"JSON.stringify(__doctext("mcp__ledger__inspect", "Inspect records", { type: "object", properties: { root: { $ref: "#/$defs/node" }, other: { $ref: "#/$defs/node" }, self: { $ref: "#" } }, $defs: { node: { description: "tree node", properties: { children: { type: "array", items: { $ref: "#/$defs/node" } } } } } }))"##
            ),
            json!("Inspect records root tree node children other self")
        );
        assert_eq!(
            eval_json(
                &host,
                r##"JSON.stringify(__doctext("mcp__ledger__inspect", "Inspect records", { type: "object", properties: { remote: { $ref: "https://example.com/schema.json#/$defs/x" }, missing: { $ref: "#/$defs/missing" }, malformed: { $ref: "#/$defs/%E0%A4%A" } }, $defs: { unused: { description: "reconciliation" } } }))"##
            ),
            json!("Inspect records remote missing malformed")
        );
    }
}
