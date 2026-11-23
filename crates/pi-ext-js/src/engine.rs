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
//! `StreamFunction` + `AssistantMessageEventStream`). E4 adds the
//! `crate::resolver::node` op group: the `node:*` built-ins and host packages share
//! this same HostCore-backed boundary rather than reaching the host process.
//! The global `process` object is installed here at construction time from
//! `node/process_global.js`; real-world extensions use it without importing
//! `node:process`, so it must exist before the first extension module runs.

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
use crate::runner::{
    EntryRenderOutcome, ResolvedToolRenderers, ToolRenderSlot, ToolRenderersOutcome,
};

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
    /// The base event context for a provider's oauth `login` call — the
    /// session event is the empty request; notifications and prompts cross
    /// their own ops.
    OAuthLogin,
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
    /// `theme.fg(color, text)` for extension components (E3 slice 3):
    /// identity until the embedder installs the sink-connected mapping —
    /// headless renderers get plain text.
    pub theme_fg: ThemeStyler,
    /// `theme.bg(color, text)`.
    pub theme_bg: ThemeStyler,
    /// `theme.bold/italic/underline/inverse/strikethrough(text)` for the
    /// custom-component factory's theme adapter (E3 slice 6b): method
    /// name + text in, styled text out. Identity until the embedder
    /// installs the ANSI attribute mapping.
    pub theme_style: ThemeStyler,
    /// `keybindings.matches(data, action)` for the custom-component
    /// factory's keybindings adapter (E3 slice 6b): the embedder installs
    /// the port's matcher; the default answers false because the headless
    /// matrix never invokes factories.
    pub keybinding_match: KeybindingMatcher,
    /// The mode-side UI snapshot (E3 slice 7): footer data, the editor's
    /// text, and the tool-expansion flag, read synchronously by the
    /// `footerData`/`editorText`/`toolsExpanded` ops. The embedder
    /// installs closures over shared snapshots the mode refreshes each
    /// pump frame; the default reads empty/null/false because the
    /// headless matrix has no mode behind it.
    pub ui_snapshot: UiSnapshot,
    /// `ctx.ui.getAllThemes()` (E3 slice 10): the JSON
    /// `[{name, path}]` list — system first, then built-ins, then the
    /// custom themes directory. The embedder installs the mode's loader;
    /// the default answers an empty list (the headless matrix).
    pub theme_all: Box<dyn Fn() -> Json + Send>,
    /// `ctx.ui.getTheme(name)`: load without switching —
    /// `Some((handle, name, appearance))` crosses as an adapter object
    /// keyed by the handle; `None` is upstream's `undefined`. The
    /// handle keys the embedder's theme stash the `themeFgWith`/
    /// `themeBgWith`/`setThemeInstance` ops resolve.
    pub theme_by_name: ThemeByNameFn,
    /// `theme.fg(color, text)` against a stashed by-name theme (E3 slice
    /// 10): the handle resolves through the embedder's stash; unknown
    /// handles pass the text through like an unknown token.
    pub theme_fg_with: ThemeHandleStyler,
    /// `theme.bg(color, text)` against a stashed by-name theme.
    pub theme_bg_with: ThemeHandleStyler,
    /// The current theme's appearance (`theme.appearance`): `"dark"` or
    /// `"light"`.
    pub theme_appearance: Box<dyn Fn() -> String + Send>,
    /// `ctx.ui.setTheme(name)`'s synchronous half (E3 slice 10): load +
    /// install with the system fallback (`theme.ts:703-724`); the
    /// `themeChanged` crossing carries the mode-side visuals and the
    /// settings persistence. `Err` maps to `{success: false, error}`.
    pub theme_set_name: ThemeSetNameFn,
    /// `ctx.ui.setTheme(instance)`'s synchronous half: install the
    /// stashed theme; `false` = unknown handle.
    pub theme_set_instance: Box<dyn Fn(u32) -> bool + Send>,
}

/// The synchronous mode-state reads the `footerData`/`editorText`/
/// `toolsExpanded` ops serve (E3 slice 7): a JSON footer snapshot, the
/// editor's text, and the tool-expansion flag. E3 slice 9 adds the
/// editor chrome the custom editor component's wiring reads
/// (`setCustomEditorComponent`, `interactive-mode.ts:2999-3016`).
pub struct UiSnapshot {
    /// `getGitBranch()`/`getExtensionStatuses()`/
    /// `getAvailableProviderCount()` (`footer-data-provider.ts:385-390`):
    /// one JSON object `{gitBranch, extensionStatuses,
    /// availableProviderCount}`.
    pub footer_data: Box<dyn Fn() -> Json + Send>,
    /// `editor.getExpandedText()` (`interactive-mode.ts:2797`).
    pub editor_text: Box<dyn Fn() -> String + Send>,
    /// `toolOutputExpanded` (`interactive-mode.ts:2822`).
    pub tools_expanded: Box<dyn Fn() -> bool + Send>,
    /// The editor chrome (E3 slice 9): padding, the autocomplete max
    /// visible, the app action ids, and the current border color token —
    /// one JSON object `{paddingX, maxVisible, appActions, borderColor}`
    /// the editor-component wire reads (`getPaddingX()`/
    /// `getAutocompleteMaxVisible()`, `interactive-mode.ts:3005-3011`).
    pub editor_chrome: Box<dyn Fn() -> Json + Send>,
    /// The editor border color token (`updateEditorBorderColor`,
    /// `interactive-mode.ts:4700-4707`): the camelCase `ThemeColor` name
    /// the wired `borderColor` function maps through the theme ops.
    pub editor_border_color: Box<dyn Fn() -> String + Send>,
}

impl Default for UiSnapshot {
    fn default() -> Self {
        Self {
            footer_data: Box::new(|| Json::Null),
            editor_text: Box::new(String::new),
            tools_expanded: Box::new(|| false),
            editor_chrome: Box::new(|| Json::Null),
            editor_border_color: Box::new(String::new),
        }
    }
}

/// One synchronous `theme.fg`/`theme.bg` call
/// (`theme.ts:297/:303`): token name + text in, styled text out.
pub type ThemeStyler = Box<dyn Fn(&str, &str) -> String + Send>;

/// One synchronous `keybindings.matches(data, action)` call for the
/// custom-component factory's keybindings adapter (E3 slice 6b).
pub type KeybindingMatcher = Box<dyn Fn(&str, &str) -> bool + Send>;
/// `theme_by_name(name)` — the by-name theme loader's crossing tuple:
/// `(handle, name, appearance)` or `None` (upstream's `undefined`).
pub type ThemeByNameFn = Box<dyn Fn(&str) -> Option<(u32, String, String)> + Send>;
/// `theme.fg/bg` against a stashed by-name theme (handle-keyed).
pub type ThemeHandleStyler = Box<dyn Fn(u32, &str, &str) -> String + Send>;
/// `setTheme(name)`'s synchronous half — load + install + the system
/// fallback.
pub type ThemeSetNameFn = Box<dyn Fn(&str) -> Result<(), String> + Send>;

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
            theme_fg: Box::new(|_token, text| text.to_string()),
            theme_bg: Box::new(|_token, text| text.to_string()),
            theme_style: Box::new(|_method, text| text.to_string()),
            keybinding_match: Box::new(|_data, _action| false),
            ui_snapshot: UiSnapshot::default(),
            theme_all: Box::new(|| Json::Array(Vec::new())),
            theme_by_name: Box::new(|_name| None),
            theme_fg_with: Box::new(|_handle, _token, text| text.to_string()),
            theme_bg_with: Box::new(|_handle, _token, text| text.to_string()),
            theme_appearance: Box::new(|| "dark".to_string()),
            theme_set_name: Box::new(|_name| Err("UI not available".to_string())),
            theme_set_instance: Box::new(|_handle| false),
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
    /// The resolved keybindings table (id → chords) the embedder crosses at
    /// bind time; the extension components' `keyText`/`keyHint` read it
    /// (`getKeybindings()` upstream).
    pub app_keybindings: Json,
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
    /// Registered MCP server names whose registry change has not been
    /// emitted yet; the runner drains them into `mcp_servers_change`
    /// emissions (the `McpServerRegistry` change listener,
    /// `runner.ts:465-466`).
    pub mcp_changes: Vec<String>,
    /// The native MCP server manager (connections to the registered
    /// servers; `mcp_manager`).
    pub mcp_manager: crate::mcp_manager::McpManager,
    /// Agent config dir whose `mcp-auth.json` holds MCP OAuth states
    /// (`mcp-auth.ts`); the host sets it before the manager first connects.
    pub mcp_auth_dir: Option<std::path::PathBuf>,
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
    /// OAuth login notifications an extension's `login` emitted through
    /// the `oauthNotify` op, in emission order, tagged with the session id
    /// (`__piSessionCallOAuthLogin`).
    pub oauth_notify_events: Vec<(u32, Json)>,
    /// OAuth login prompts awaiting host answers: (session id, token,
    /// payload) in ask order. One prompt per session at a time — the login
    /// function awaits its answer.
    pub oauth_prompts: Vec<(u32, u64, Json)>,
    /// Sessions whose oauth `signal` the host aborted
    /// (`oauthAbortSignal`); the login function polls `signal.aborted`.
    pub oauth_aborted: Vec<u32>,
    /// Provider-stream `onPayload` asks awaiting the host's transform
    /// chain: (session id, token, payload) in ask order
    /// (`__piSessionCallProviderStream` decorates the options; the
    /// `before_provider_request` fan-out replaces the payload, its final
    /// value resolves the handler's promise).
    pub provider_hook_requests: Vec<(u32, u64, Json)>,
    /// One-way provider-stream hook events: (session id, event kind —
    /// `"after_provider_response"` or `"provider_stream_event"` — payload)
    /// in emission order. The drain emits them FIFO.
    pub provider_hook_events: Vec<(u32, String, Json)>,
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
        // Install the global `process` before any extension module is
        // declared: third-party extensions reference it at top level
        // without importing `node:process`. `node:process` re-exports this
        // same object (see `node/process_global.js`).
        ctx.with(|ctx| ctx.eval::<(), _>(crate::resolver::node::process_global_source()))?;
        // Install the WHATWG `URL`/`URLSearchParams` globals at bootstrap:
        // extensions reference `new URL(import.meta.url)` at module top level
        // without importing `node:url` (pi-subagents-ported/index.ts:94).
        // Evaluating the shim here runs its `globalThis.URL ??=` guards; a
        // later `import "node:url"` reuses those already-installed globals.
        ctx.with(|ctx| -> Result<(), JsHostError> {
            let source = crate::resolver::virtual_module_source("node:url")
                .expect("the node:url shim is always available");
            let declared = rquickjs::Module::declare(ctx.clone(), "virtual:node:url", source)
                .map_err(|err| Self::js_error(&ctx, err))?;
            let (module, _promise) = declared.eval().map_err(|err| Self::js_error(&ctx, err))?;
            let _namespace = module.namespace()?;
            Ok(())
        })?;
        // Same bootstrap for `node:util`: its shim installs the global
        // `TextEncoder`/`TextDecoder` that extensions use at module top level
        // without importing the module (pi-result-cap/result-cap.ts).
        ctx.with(|ctx| -> Result<(), JsHostError> {
            let source = crate::resolver::virtual_module_source("node:util")
                .expect("the node:util shim is always available");
            let declared = rquickjs::Module::declare(ctx.clone(), "virtual:node:util", source)
                .map_err(|err| Self::js_error(&ctx, err))?;
            let (module, _promise) = declared.eval().map_err(|err| Self::js_error(&ctx, err))?;
            let _namespace = module.namespace()?;
            Ok(())
        })?;
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
            let raw: Json = serde_json::from_str(&json).unwrap_or(Json::Null);
            // The validated (alias-resolved) config copy is what the
            // registry stores (`loader.ts:497`).
            let config = match crate::mcp_registry::validate_mcp_server_config(&name, &raw) {
                Ok(config) => config,
                Err(error) => return error,
            };
            let error = core
                .mcp
                .register(&name, &ext_path, config)
                .err()
                .unwrap_or_default();
            if error.is_empty() {
                // Every applied registration reaches the change queue
                // (registry.register fires the listener unconditionally,
                // `mcp-servers.ts:243-246`); the runner emits only once
                // bound, mirroring the listener being set at bindCore
                // (`runner.ts:465`).
                core.mcp_changes.push(name);
            }
            error
        });
        op!("mcpOwnership", core, |name: String,
                                   ext_path: String|
         -> String {
            let core = core.lock().expect("host core mutex poisoned");
            core.mcp
                .check_register(&name, &ext_path)
                .err()
                .unwrap_or_default()
        });
        op!("mcpUnregister", core, |name: String,
                                    ext_path: String|
         -> String {
            let mut core = core.lock().expect("host core mutex poisoned");
            // Silent no-op for unknown or foreign servers
            // (`mcp-servers.ts:249-253`); a removed entry reaches the
            // change queue (the listener fires only on actual removal).
            let owned = core
                .mcp
                .list()
                .iter()
                .any(|server| server.name == name && server.extension_path == ext_path);
            if owned {
                core.mcp.unregister(&name, &ext_path);
                core.mcp_changes.push(name);
            }
            String::new()
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
        // One MCP tool call from a registered tool's execute wrapper: the
        // manager enforces the `getClient` guards (`index.ts:424-438`) and
        // maps the result (`convertMcpResult`, `tools.ts:215-235`). The
        // reply is `{"result": ...}` or `{"error": "..."}` — the wrapper
        // throws the error, so a failed call is a tool error, and a
        // protocol-level `isError` result stays a result.
        op!("mcpCallTool", core, |server: String,
                                  tool: String,
                                  args: String|
         -> String {
            let manager = {
                let core = core.lock().expect("host core mutex poisoned");
                core.mcp_manager.clone()
            };
            let arguments: Json = serde_json::from_str(&args).unwrap_or(Json::Null);
            serde_json::to_string(&manager.call_tool(&server, &tool, arguments)).unwrap_or_else(
                |_| r#"{"error":"mcp tool result serialization failed"}"#.to_string(),
            )
        });
        // The three MCP resource tools share one op: the manager builds the
        // payload, applies limitMcpContent and returns the tool result shape
        // (`resources.ts:179-193`, `:236-260`).
        op!("mcpResource", core, |tool: String,
                                  args: String|
         -> String {
            let manager = {
                let core = core.lock().expect("host core mutex poisoned");
                core.mcp_manager.clone()
            };
            let params: Json = serde_json::from_str(&args).unwrap_or(Json::Null);
            serde_json::to_string(&manager.resource_tool_result(&tool, &params)).unwrap_or_else(
                |_| r#"{"error":"mcp resource result serialization failed"}"#.to_string(),
            )
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
        // can be resolved later via `__piUiResolve`. A caller-supplied id
        // survives — the customDone crossing carries the dialog's id for
        // the mode's close lookup (E3 slice 6b).
        op!("uiRequest", core, |request: String| -> String {
            let mut core = core.lock().expect("host core mutex poisoned");
            core.ui_counter += 1;
            let id = format!("ui-{}", core.ui_counter);
            let mut value: Json = serde_json::from_str(&request).unwrap_or(Json::Null);
            if let Some(object) = value.as_object_mut()
                && !object.contains_key("id")
            {
                object.shift_insert(0, "id".to_string(), Json::String(id.clone()));
            }
            core.ui_requests.push(value);
            id
        });
        // The extension components' theme surface (E3 slice 3): synchronous
        // styling during render — the closures default to identity and the
        // embedder installs the sink-connected mapping at bind time.
        op!("themeFg", core, |token: String, text: String| -> String {
            let guard = core.lock().expect("host core mutex poisoned");
            (guard.actions.theme_fg)(&token, &text)
        });
        op!("themeBg", core, |token: String, text: String| -> String {
            let guard = core.lock().expect("host core mutex poisoned");
            (guard.actions.theme_bg)(&token, &text)
        });
        // The custom-component factory's theme adapter attribute methods
        // (E3 slice 6b): `theme.bold/italic/underline/inverse/
        // strikethrough(text)` cross with the method name; the embedder
        // installs the ANSI attribute mapping at bind time.
        op!("themeStyle", core, |method: String,
                                 text: String|
         -> String {
            let guard = core.lock().expect("host core mutex poisoned");
            (guard.actions.theme_style)(&method, &text)
        });
        // The ctx.ui themes API (E3 slice 10, `interactive-mode.ts:2806
        // -2822`): the reads cross synchronously through the embedder's
        // theme loaders; `setTheme`'s synchronous half installs the
        // global with the system fallback and the `themeChanged`
        // crossing carries the mode-side visuals + settings persistence.
        op!("getAllThemes", core, || -> String {
            let guard = core.lock().expect("host core mutex poisoned");
            serde_json::to_string(&(guard.actions.theme_all)()).unwrap_or_else(|_| "[]".to_string())
        });
        op!("getTheme", core, |name: String| -> String {
            let guard = core.lock().expect("host core mutex poisoned");
            match (guard.actions.theme_by_name)(&name) {
                Some((handle, name, appearance)) => serde_json::to_string(&serde_json::json!({
                    "handle": handle,
                    "name": name,
                    "appearance": appearance,
                }))
                .unwrap_or_else(|_| "null".to_string()),
                None => "null".to_string(),
            }
        });
        op!("themeFgWith", core, |handle: u32,
                                  token: String,
                                  text: String|
         -> String {
            let guard = core.lock().expect("host core mutex poisoned");
            (guard.actions.theme_fg_with)(handle, &token, &text)
        });
        op!("themeBgWith", core, |handle: u32,
                                  token: String,
                                  text: String|
         -> String {
            let guard = core.lock().expect("host core mutex poisoned");
            (guard.actions.theme_bg_with)(handle, &token, &text)
        });
        op!("themeAppearance", core, || -> String {
            let guard = core.lock().expect("host core mutex poisoned");
            (guard.actions.theme_appearance)()
        });
        op!("setThemeName", core, |name: String| -> String {
            let guard = core.lock().expect("host core mutex poisoned");
            serde_json::to_string(&match (guard.actions.theme_set_name)(&name) {
                Ok(()) => serde_json::json!({ "success": true }),
                Err(error) => serde_json::json!({ "success": false, "error": error }),
            })
            .unwrap_or_else(|_| "{\"success\":false}".to_string())
        });
        op!("setThemeInstance", core, |handle: u32| -> bool {
            let guard = core.lock().expect("host core mutex poisoned");
            (guard.actions.theme_set_instance)(handle)
        });
        // The custom-component factory's `keybindings.matches` (E3 slice
        // 6b): the embedder installs the port's matcher; the default
        // answers false because the headless matrix never invokes
        // factories (headless custom resolves undefined without one).
        op!("keybindingMatch", core, |data: String,
                                      action: String|
         -> String {
            let guard = core.lock().expect("host core mutex poisoned");
            if (guard.actions.keybinding_match)(&data, &action) {
                "true".to_string()
            } else {
                "false".to_string()
            }
        });
        // The resolved keybindings table (id → chords) — `getKeys`
        // upstream (`keybinding-hints.ts:34-36`).
        op!("getAppKeybindings", core, || -> String {
            let core = core.lock().expect("host core mutex poisoned");
            if core.app_keybindings.is_null() {
                "{}".to_string()
            } else {
                serde_json::to_string(&core.app_keybindings).unwrap_or_else(|_| "{}".to_string())
            }
        });
        // The mode-state snapshot the synchronous ui reads serve (E3
        // slice 7): the custom footer factory's data adapter, the
        // `getEditorText` query, and the `getToolsExpanded` query. The
        // embedder installs closures over shared snapshots the mode
        // refreshes; the defaults read null/""/false (headless matrix).
        op!("footerData", core, || -> String {
            let guard = core.lock().expect("host core mutex poisoned");
            serde_json::to_string(&(guard.actions.ui_snapshot.footer_data)())
                .unwrap_or_else(|_| "null".to_string())
        });
        op!("editorText", core, || -> String {
            let guard = core.lock().expect("host core mutex poisoned");
            (guard.actions.ui_snapshot.editor_text)()
        });
        op!("toolsExpanded", core, || -> String {
            let guard = core.lock().expect("host core mutex poisoned");
            if (guard.actions.ui_snapshot.tools_expanded)() {
                "true".to_string()
            } else {
                "false".to_string()
            }
        });
        // The editor chrome (E3 slice 9): padding, autocomplete max
        // visible, app action ids, and the border color token the editor
        // component's wiring reads (`setCustomEditorComponent`,
        // `interactive-mode.ts:2999-3016`); the defaults read null.
        op!("editorChrome", core, || -> String {
            let guard = core.lock().expect("host core mutex poisoned");
            serde_json::to_string(&(guard.actions.ui_snapshot.editor_chrome)())
                .unwrap_or_else(|_| "null".to_string())
        });
        op!("editorBorderColor", core, || -> String {
            let guard = core.lock().expect("host core mutex poisoned");
            (guard.actions.ui_snapshot.editor_border_color)()
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
        op!("providerStreamEvent", core, |id: u32,
                                          event: String|
         -> String {
            core.lock()
                .expect("host core mutex poisoned")
                .provider_stream_events
                .push((id, serde_json::from_str(&event).unwrap_or(Json::Null)));
            String::new()
        });
        // An oauth login's one-way notifications (`adaptOAuth`'s notify
        // mapping: auth_url / device_code / progress), tagged with the
        // login session id.
        op!("oauthNotify", core, |id: u32, event: String| -> String {
            core.lock()
                .expect("host core mutex poisoned")
                .oauth_notify_events
                .push((id, serde_json::from_str(&event).unwrap_or(Json::Null)));
            String::new()
        });
        // An oauth login's prompt ask: the login function awaits the
        // returned promise until the host resolves it with the answer
        // (`oauthResolvePrompt`).
        op!("oauthPrompt", core, |id: u32,
                                  token: u64,
                                  payload: String|
         -> String {
            core.lock()
                .expect("host core mutex poisoned")
                .oauth_prompts
                .push((
                    id,
                    token,
                    serde_json::from_str(&payload).unwrap_or(Json::Null),
                ));
            String::new()
        });
        // A provider-stream `onPayload` ask: the handler awaits the
        // returned promise until the host runs the
        // `before_provider_request` transform chain and resolves it with
        // the (possibly replaced) payload (`__piResolveProviderHook`).
        op!("providerHookPayload", core, |id: u32,
                                          token: u64,
                                          payload: String|
         -> String {
            core.lock()
                .expect("host core mutex poisoned")
                .provider_hook_requests
                .push((
                    id,
                    token,
                    serde_json::from_str(&payload).unwrap_or(Json::Null),
                ));
            String::new()
        });
        // A provider-stream one-way hook (`onResponse` /
        // `onProviderStreamEvent`): the event queues FIFO; the drain
        // emits it to the extension listeners.
        op!("providerHookEvent", core, |id: u32,
                                        kind: String,
                                        payload: String|
         -> String {
            core.lock()
                .expect("host core mutex poisoned")
                .provider_hook_events
                .push((
                    id,
                    kind,
                    serde_json::from_str(&payload).unwrap_or(Json::Null),
                ));
            String::new()
        });
        // Flags the login session's `signal.aborted` — the host's escape
        // during a login flow (`session.abortRetry`'s oauth analog).
        op!("oauthAbortSignal", core, |id: u32| -> String {
            let mut core = core.lock().expect("host core mutex poisoned");
            if !core.oauth_aborted.contains(&id) {
                core.oauth_aborted.push(id);
            }
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
        // E4: the `node:*` and host-package shims install their own ops on
        // the same private object; they read the same HostCore state the pi
        // API does (see `resolver::node`).
        crate::resolver::node::install_ops(ctx, core)?;
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
                    let declaration = match rquickjs::Module::declare(ctx.clone(), path, source) {
                        Ok(declared) => declared,
                        Err(err) => return Ok(Err(Self::js_error(&ctx, err))),
                    };
                    // Node always populates `import.meta.url`; QuickJS leaves
                    // it undefined. Extensions use it at module top level
                    // (`pi-subagents/index.ts:109`, `pi-gemini-router/index.ts:339`),
                    // so supply a file URL for the module's own path.
                    if let Err(err) = declaration
                        .meta()
                        .and_then(|meta| meta.set("url", format!("file://{path}")))
                    {
                        return Ok(Err(Self::js_error(&ctx, err)));
                    }
                    let (module, promise) = match declaration.eval() {
                        Ok(evaluated) => evaluated,
                        Err(err) => return Ok(Err(Self::js_error(&ctx, err))),
                    };
                    // QuickJS reports a failed module *evaluation* through
                    // the returned promise, not as a thrown exception from
                    // `eval()`. Ignoring it let a top-level throw (for
                    // example `URL is not defined`) slip through and the
                    // factory below ran against a half-evaluated module,
                    // surfacing a misleading TDZ error instead. Surface the
                    // real rejection to the caller.
                    if matches!(promise.state(), rquickjs::promise::PromiseState::Rejected)
                        && let Some(Err(err)) = promise.result::<Value>()
                    {
                        return Ok(Err(Self::js_error(&ctx, err)));
                    }
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

    /// Ensures a record exists for a synthetic extension path (the port's
    /// native `builtin:mcp` host; upstream loads the bundled extension as a
    /// real module with the synthetic `builtin:<name>` path). Returns its
    /// index.
    pub fn ensure_extension_record(&self, path: &str) -> Result<usize, JsHostError> {
        let existing = self.ctx.with(|ctx| -> Result<Option<usize>, JsHostError> {
            let find: Function = ctx.globals().get("__piFindExtension")?;
            let index: i64 = find
                .call((path,))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok((index >= 0).then_some(index as usize))
        })?;
        if let Some(index) = existing {
            return Ok(index);
        }
        let index = self.begin_extension(path)?;
        self.commit_extension(index)?;
        Ok(index)
    }

    /// Registers MCP tool definitions into a synthetic extension's tool
    /// registry with execute wrappers that call the `mcpCallTool` op (the
    /// `pi.registerTool` surface the bundled MCP extension uses,
    /// `index.ts:442`). `tools` is the JSON array of definitions.
    pub fn register_mcp_tools(&self, path: &str, tools: &Json) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let register: Function = ctx.globals().get("__piMcpRegisterTools")?;
            let tools_json =
                serde_json::to_string(tools).map_err(|err| JsHostError::Job(err.to_string()))?;
            register
                .call::<(String, String), ()>((path.to_string(), tools_json))
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
            ContextKind::OAuthLogin => "oauthLogin",
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

    /// Calls one extension shortcut's handler inside a session
    /// (`__piSessionCallShortcut`); the handler receives the base ctx.
    pub fn session_call_shortcut(
        &self,
        id: u32,
        index: usize,
        key: &str,
    ) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx.globals().get("__piSessionCallShortcut")?;
            let _: Value = call
                .call((id, index, key))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Runs one extension's markdown transformer synchronously
    /// (`__piMarkdownTransform`; `markdown-transform.ts:28-38`). The
    /// context crosses as JSON. Errors and non-string results both map to
    /// `None` — upstream catches around every transformer and keeps the
    /// current markdown when the return isn't a string.
    pub fn markdown_transform(
        &self,
        index: usize,
        markdown: &str,
        context_json: &str,
    ) -> Option<String> {
        self.ctx.with(|ctx| -> Option<String> {
            let call: Function = match ctx.globals().get("__piMarkdownTransform") {
                Ok(call) => call,
                Err(_) => return None,
            };
            // A string return converts; `undefined` maps to `None`, and a
            // non-string return fails the conversion — both keep the
            // current markdown. A transformer throw errors the call.
            call.call::<(usize, &str, &str), Option<String>>((index, markdown, context_json))
                .unwrap_or_default()
        })
    }

    /// Folds one terminal input chunk through the extension-registered
    /// listeners (`__piTerminalInputFold`; the `inputListeners` chain,
    /// `tui.ts:1095-1108`). `None` means the chunk is dropped — a listener
    /// consumed it, or a transform emptied it.
    pub fn terminal_input_fold(&self, data: &str) -> Option<String> {
        self.ctx.with(|ctx| {
            let call: Function = match ctx.globals().get("__piTerminalInputFold") {
                Ok(call) => call,
                Err(_) => return Some(data.to_string()),
            };
            // A listener throw is a port-side deviation from upstream (a
            // throw there propagates out of `handleTerminalInput`): the
            // fold keeps the chunk so one broken extension cannot eat the
            // input path.
            call.call::<(&str,), Option<String>>((data,))
                .unwrap_or_else(|_| Some(data.to_string()))
        })
    }

    /// Clears the extension-registered terminal input listeners
    /// (`resetExtensionUI`'s `clearExtensionTerminalInputListeners`,
    /// `interactive-mode.ts:2750-2753`).
    pub fn clear_terminal_input_handlers(&self) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx
                .globals()
                .get("__piTerminalInputClear")
                .map_err(|err| Self::js_error(&ctx, err))?;
            let _: Value = call.call(()).map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Fires the custom footer's `onBranchChange` callbacks (the
    /// footer-data provider's watcher, crossed through the
    /// `__piFooterBranchHooks` stash). Fire-and-forget: a callback error
    /// does not fail the notify.
    pub fn notify_footer_branch_change(&self, branch: &str) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx
                .globals()
                .get("__piFooterBranchNotify")
                .map_err(|err| Self::js_error(&ctx, err))?;
            let _: Value = call
                .call((branch,))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Calls one adopted component's duck-typed `setExpanded` (the custom
    /// header/footer swap, `interactive-mode.ts:2709-2711`/`:4749-4752`):
    /// a no-op when the component lacks the method — upstream checks
    /// `isExpandable` first.
    pub fn component_set_expanded(&self, handle: u32, expanded: bool) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx
                .globals()
                .get("__piComponentSetExpanded")
                .map_err(|err| Self::js_error(&ctx, err))?;
            let _: Value = call
                .call((handle, expanded))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    // ---- E3 slice 9: the extension editor component crossings ----
    // `EditorComponent` (`editor-component.ts:11-67`): text reads/writes,
    // history, and the autocomplete re-set cross mailbox-style through the
    // adopted-component registry. All synchronous — the glue answers from
    // the component object in one turn.

    /// Reads one editor component's text (`getExpandedText?.() ??
    /// getText()`, `interactive-mode.ts:2984`); empty when the handle is
    /// gone or `getText` is missing (required by the interface — an absent
    /// method means the component was released).
    pub fn component_editor_text(&self, handle: u32) -> Result<String, JsHostError> {
        let raw = self
            .ctx
            .with(|ctx| -> Result<Option<String>, JsHostError> {
                let call: Function = ctx.globals().get("__piComponentEditorText")?;
                let result: Option<String> = call
                    .call((handle,))
                    .map_err(|err| Self::js_error(&ctx, err))?;
                Ok(result)
            })?;
        let Some(raw) = raw else {
            return Ok(String::new());
        };
        let value: Json =
            serde_json::from_str(&raw).map_err(|err| JsHostError::Job(err.to_string()))?;
        Ok(value
            .get("text")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string())
    }

    /// Writes one editor component's text (`setText`, `interactive-mode
    /// .ts:3000`): a no-op when the handle is gone or the method is
    /// missing.
    pub fn component_set_text(&self, handle: u32, text: &str) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx
                .globals()
                .get("__piComponentSetText")
                .map_err(|err| Self::js_error(&ctx, err))?;
            let _: Value = call
                .call((handle, text))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Feeds one editor component's history (`addToHistory?.()`, duck
    /// typed — `editor-component.ts:34-37`): a no-op when absent.
    pub fn component_add_to_history(&self, handle: u32, text: &str) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx
                .globals()
                .get("__piComponentAddToHistory")
                .map_err(|err| Self::js_error(&ctx, err))?;
            let _: Value = call
                .call((handle, text))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Re-sets one editor component's autocomplete provider
    /// (`setupAutocompleteProvider` re-calls it on the non-default editor,
    /// `interactive-mode.ts:855-871`): a fresh composed adapter each call,
    /// duck-typed no-op when absent.
    pub fn component_set_autocomplete_provider(&self, handle: u32) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx
                .globals()
                .get("__piComponentSetAutocomplete")
                .map_err(|err| Self::js_error(&ctx, err))?;
            let _: Value = call
                .call((handle,))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Clears the stashed editor-component factory (upstream's reset sets
    /// `editorComponentFactory = undefined` through
    /// `setCustomEditorComponent(undefined)`, `interactive-mode.ts:2611`).
    pub fn editor_component_clear_stash(&self) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx
                .globals()
                .get("__piEditorComponentFactoryClear")
                .map_err(|err| Self::js_error(&ctx, err))?;
            let _: Value = call.call(()).map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Runs one autocomplete wrapper's `getSuggestions` (E3 slice 8; the
    /// provider is awaitable upstream, `autocomplete.ts:274-280`) — the
    /// async call settles through the job queue (`drive_jobs`), which is
    /// safe because nothing here waits on the mode: the base chain's
    /// result crosses as `base_json` and the wrapper resolves against it.
    /// `None` = no suggestions (no wrapper, a throw, or an unsettled
    /// promise).
    pub fn wrapped_suggest(
        &self,
        index: u32,
        base_json: &str,
        lines_json: &str,
        cursor_line: usize,
        cursor_col: usize,
        force: bool,
    ) -> Option<String> {
        let called = self.ctx.with(|ctx| -> bool {
            // The async call answers a Promise; the Value drops inside the
            // closure — a borrowed rquickjs type cannot escape `with`
            // (lifetime invariance).
            let call: Function = match ctx.globals().get("__piWrappedSuggest") {
                Ok(call) => call,
                Err(_) => return false,
            };
            let _: Value =
                match call.call((index, base_json, lines_json, cursor_line, cursor_col, force)) {
                    Ok(value) => value,
                    Err(_) => return false,
                };
            true
        });
        if !called {
            return None;
        }
        if self.drive_jobs().is_err() {
            return None;
        }
        self.take_autocomplete_result()
    }

    /// Runs one autocomplete wrapper's `applyCompletion`
    /// (`autocomplete.ts:283-292` — synchronous upstream). `None` falls
    /// back to the base's application.
    #[allow(clippy::too_many_arguments)] // the crossing protocol + upstream's apply shape
    pub fn wrapped_apply(
        &self,
        index: u32,
        base_json: &str,
        lines_json: &str,
        cursor_line: usize,
        cursor_col: usize,
        item_json: &str,
        prefix: &str,
    ) -> Option<String> {
        let called = self.ctx.with(|ctx| -> bool {
            // The sync call answers `undefined`; coerce to a Value and drop
            // it inside the closure — the slot carries the result.
            let call: Function = match ctx.globals().get("__piWrappedApply") {
                Ok(call) => call,
                Err(_) => return false,
            };
            let _: Value = match call.call((
                index,
                base_json,
                lines_json,
                cursor_line,
                cursor_col,
                item_json,
                prefix,
            )) {
                Ok(value) => value,
                Err(_) => return false,
            };
            true
        });
        if !called {
            return None;
        }
        if self.drive_jobs().is_err() {
            return None;
        }
        self.take_autocomplete_result()
    }

    /// Reads and clears the slot `__piWrappedSuggest`/`__piWrappedApply`
    /// write. An empty string means "no result".
    fn take_autocomplete_result(&self) -> Option<String> {
        self.ctx.with(|ctx| {
            let raw: String = ctx
                .globals()
                .get("__piAutocompleteResult")
                .unwrap_or_default();
            let _ = ctx.globals().set("__piAutocompleteResult", "");
            if raw.is_empty() { None } else { Some(raw) }
        })
    }

    /// Clears the autocomplete wrapper stash
    /// (`resetExtensionUI`'s `autocompleteProviderWrappers = []`,
    /// `interactive-mode.ts:2609`).
    pub fn clear_autocomplete_wrappers(&self) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx
                .globals()
                .get("__piAutocompleteWrappersClear")
                .map_err(|err| Self::js_error(&ctx, err))?;
            let _: Value = call.call(()).map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Calls one extension's entry renderer for a custom session entry
    /// (`__piRenderEntry`): the renderer runs inside the engine, its
    /// returned Component is adopted into the handle registry, and the
    /// outcome crosses as JSON. `undefined` (no renderer, or the renderer
    /// returned nothing) maps to [`EntryRenderOutcome::None`].
    pub fn render_entry(
        &self,
        ext_index: usize,
        custom_type: &str,
        entry_json: &str,
        options_json: &str,
    ) -> Result<EntryRenderOutcome, JsHostError> {
        let raw = self
            .ctx
            .with(|ctx| -> Result<Option<String>, JsHostError> {
                let call: Function = ctx.globals().get("__piRenderEntry")?;
                let result: Option<String> = call
                    .call((ext_index, custom_type, entry_json, options_json))
                    .map_err(|err| Self::js_error(&ctx, err))?;
                Ok(result)
            })?;
        let Some(raw) = raw else {
            return Ok(EntryRenderOutcome::None);
        };
        let value: Json =
            serde_json::from_str(&raw).map_err(|err| JsHostError::Job(err.to_string()))?;
        match value.get("kind").and_then(Json::as_str) {
            Some("component") => Ok(EntryRenderOutcome::Component {
                handle: value
                    .get("handle")
                    .and_then(Json::as_u64)
                    .unwrap_or_default() as u32,
                has_input: value
                    .get("hasInput")
                    .and_then(Json::as_bool)
                    .unwrap_or(false),
            }),
            Some("error") => Ok(EntryRenderOutcome::Failed(
                value
                    .get("message")
                    .and_then(Json::as_str)
                    .unwrap_or("renderer failed")
                    .to_string(),
            )),
            _ => Ok(EntryRenderOutcome::None),
        }
    }

    /// Calls one extension's message renderer for a custom message
    /// (`__piRenderMessage`): the same adopt-and-report crossing as
    /// [`Self::render_entry`], with `MessageRenderOptions` carrying
    /// `outputPad` alongside `expanded` (`types.ts:1556-1560`). A renderer
    /// throw maps to [`EntryRenderOutcome::Failed`], which the host's
    /// component turns into the default rendering (`custom-message.ts:93-95`
    /// — upstream swallows the throw silently).
    pub fn render_message(
        &self,
        ext_index: usize,
        custom_type: &str,
        message_json: &str,
        options_json: &str,
    ) -> Result<EntryRenderOutcome, JsHostError> {
        let raw = self
            .ctx
            .with(|ctx| -> Result<Option<String>, JsHostError> {
                let call: Function = ctx.globals().get("__piRenderMessage")?;
                let result: Option<String> = call
                    .call((ext_index, custom_type, message_json, options_json))
                    .map_err(|err| Self::js_error(&ctx, err))?;
                Ok(result)
            })?;
        let Some(raw) = raw else {
            return Ok(EntryRenderOutcome::None);
        };
        let value: Json =
            serde_json::from_str(&raw).map_err(|err| JsHostError::Job(err.to_string()))?;
        match value.get("kind").and_then(Json::as_str) {
            Some("component") => Ok(EntryRenderOutcome::Component {
                handle: value
                    .get("handle")
                    .and_then(Json::as_u64)
                    .unwrap_or_default() as u32,
                has_input: value
                    .get("hasInput")
                    .and_then(Json::as_bool)
                    .unwrap_or(false),
            }),
            Some("error") => Ok(EntryRenderOutcome::Failed(
                value
                    .get("message")
                    .and_then(Json::as_str)
                    .unwrap_or("renderer failed")
                    .to_string(),
            )),
            _ => Ok(EntryRenderOutcome::None),
        }
    }

    /// Runs one tool row's resolver chain (`__piResolveToolRenderers`):
    /// the chain and its stash live in the glue; the outcome crosses as
    /// JSON with the per-slot classification (builtin/custom/absent).
    pub fn resolve_tool_renderers(
        &self,
        tool_name: &str,
        has_builtin: bool,
    ) -> Result<ToolRenderersOutcome, JsHostError> {
        let raw = self
            .ctx
            .with(|ctx| -> Result<Option<String>, JsHostError> {
                let call: Function = ctx.globals().get("__piResolveToolRenderers")?;
                let result: Option<String> = call
                    .call((tool_name, has_builtin))
                    .map_err(|err| Self::js_error(&ctx, err))?;
                Ok(result)
            })?;
        let Some(raw) = raw else {
            return Ok(ToolRenderersOutcome::None);
        };
        let value: Json =
            serde_json::from_str(&raw).map_err(|err| JsHostError::Job(err.to_string()))?;
        match value.get("kind").and_then(Json::as_str) {
            Some("renderers") => {
                let slot = |key: &str| match value.get(key).and_then(Json::as_str) {
                    Some("builtin") => ToolRenderSlot::Builtin,
                    Some("custom") => ToolRenderSlot::Custom,
                    _ => ToolRenderSlot::Absent,
                };
                Ok(ToolRenderersOutcome::Renderers(ResolvedToolRenderers {
                    stash_id: value.get("id").and_then(Json::as_u64).unwrap_or_default() as u32,
                    render_shell_self: value.get("renderShell").and_then(Json::as_str)
                        == Some("self"),
                    render_call: slot("renderCall"),
                    render_result: slot("renderResult"),
                }))
            }
            Some("error") => Ok(ToolRenderersOutcome::Failed(
                value
                    .get("message")
                    .and_then(Json::as_str)
                    .unwrap_or("resolver failed")
                    .to_string(),
            )),
            _ => Ok(ToolRenderersOutcome::None),
        }
    }

    /// Calls one row's stashed `renderCall` (`__piToolRenderCall`): the
    /// same adopt-and-report crossing as [`Self::render_entry`].
    pub fn tool_render_call(
        &self,
        stash_id: u32,
        args_json: &str,
        context_json: &str,
    ) -> Result<EntryRenderOutcome, JsHostError> {
        let raw = self
            .ctx
            .with(|ctx| -> Result<Option<String>, JsHostError> {
                let call: Function = ctx.globals().get("__piToolRenderCall")?;
                let result: Option<String> = call
                    .call((stash_id, args_json, context_json))
                    .map_err(|err| Self::js_error(&ctx, err))?;
                Ok(result)
            })?;
        Self::tool_slot_outcome(raw)
    }

    /// Calls one row's stashed `renderResult` (`__piToolRenderResult`).
    pub fn tool_render_result(
        &self,
        stash_id: u32,
        args_json: &str,
        result_json: &str,
        options_json: &str,
        context_json: &str,
    ) -> Result<EntryRenderOutcome, JsHostError> {
        let raw = self
            .ctx
            .with(|ctx| -> Result<Option<String>, JsHostError> {
                let call: Function = ctx.globals().get("__piToolRenderResult")?;
                let result: Option<String> = call
                    .call((stash_id, args_json, result_json, options_json, context_json))
                    .map_err(|err| Self::js_error(&ctx, err))?;
                Ok(result)
            })?;
        Self::tool_slot_outcome(raw)
    }

    /// The `{kind:"component"|"error"}` mapping shared by the two tool
    /// slots (undefined — builtin or absent slot, nothing crossed — maps
    /// to [`EntryRenderOutcome::None`]).
    fn tool_slot_outcome(raw: Option<String>) -> Result<EntryRenderOutcome, JsHostError> {
        let Some(raw) = raw else {
            return Ok(EntryRenderOutcome::None);
        };
        let value: Json =
            serde_json::from_str(&raw).map_err(|err| JsHostError::Job(err.to_string()))?;
        match value.get("kind").and_then(Json::as_str) {
            Some("component") => Ok(EntryRenderOutcome::Component {
                handle: value
                    .get("handle")
                    .and_then(Json::as_u64)
                    .unwrap_or_default() as u32,
                has_input: value
                    .get("hasInput")
                    .and_then(Json::as_bool)
                    .unwrap_or(false),
            }),
            Some("error") => Ok(EntryRenderOutcome::Failed(
                value
                    .get("message")
                    .and_then(Json::as_str)
                    .unwrap_or("renderer failed")
                    .to_string(),
            )),
            _ => Ok(EntryRenderOutcome::None),
        }
    }

    /// Drops one row's stashed renderers (`__piToolRendererRelease`).
    pub fn tool_renderer_release(&self, stash_id: u32) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx.globals().get("__piToolRendererRelease")?;
            let _: Value = call
                .call((stash_id,))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Renders one adopted component (`__piComponentRender`): the JS
    /// `render(width)` lines cross as JSON. A component throw propagates —
    /// upstream doesn't catch render failures here either.
    pub fn component_render(&self, handle: u32, width: usize) -> Result<Vec<String>, JsHostError> {
        let raw = self
            .ctx
            .with(|ctx| -> Result<Option<String>, JsHostError> {
                let call: Function = ctx.globals().get("__piComponentRender")?;
                let result: Option<String> = call
                    .call((handle, width))
                    .map_err(|err| Self::js_error(&ctx, err))?;
                Ok(result)
            })?;
        let Some(raw) = raw else {
            return Ok(Vec::new());
        };
        let value: Json =
            serde_json::from_str(&raw).map_err(|err| JsHostError::Job(err.to_string()))?;
        Ok(value
            .get("lines")
            .and_then(Json::as_array)
            .map(|lines| {
                lines
                    .iter()
                    .filter_map(Json::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Feeds one key chunk to an adopted component
    /// (`__piComponentHandleInput`): the `{consume}` decision crosses as
    /// JSON. `undefined` (unknown handle, missing handler) is not-mine.
    pub fn component_handle_input(&self, handle: u32, data: &str) -> Result<bool, JsHostError> {
        let raw = self
            .ctx
            .with(|ctx| -> Result<Option<String>, JsHostError> {
                let call: Function = ctx.globals().get("__piComponentHandleInput")?;
                let result: Option<String> = call
                    .call((handle, data))
                    .map_err(|err| Self::js_error(&ctx, err))?;
                Ok(result)
            })?;
        let Some(raw) = raw else {
            return Ok(false);
        };
        let value: Json =
            serde_json::from_str(&raw).map_err(|err| JsHostError::Job(err.to_string()))?;
        Ok(value
            .get("consume")
            .and_then(Json::as_bool)
            .unwrap_or(false))
    }

    /// Invalidates one adopted component (`__piComponentInvalidate`) — the
    /// theme-change hook.
    pub fn component_invalidate(&self, handle: u32) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx.globals().get("__piComponentInvalidate")?;
            let _: Value = call
                .call((handle,))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Drops one adopted component (`__piComponentRelease`) so the JS-side
    /// registry does not leak handles.
    pub fn component_release(&self, handle: u32) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx.globals().get("__piComponentRelease")?;
            let _: Value = call
                .call((handle,))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Disposes one adopted component (`__piComponentDispose`) — the
    /// custom dialog's close path: `dispose?.()` with errors swallowed,
    /// then the registry entry drops.
    pub fn component_dispose(&self, handle: u32) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx.globals().get("__piComponentDispose")?;
            let _: Value = call
                .call((handle,))
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
    pub fn session_call_provider_stream(&self, id: u32, provider: &str) -> Result<(), JsHostError> {
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

    /// Calls the stashed oauth `login` (`__piSessionCallOAuthLogin`): the
    /// adapter callbacks cross as described in the glue; notifications and
    /// prompts poll through their slots, and the login's credential
    /// settles the session slot.
    pub fn session_call_oauth_login(&self, id: u32, provider: &str) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let call: Function = ctx.globals().get("__piSessionCallOAuthLogin")?;
            let _: Value = call
                .call((id, provider))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Takes the oauth notifications queued for one session id, in
    /// emission order; other sessions' events are kept.
    pub fn take_oauth_notify_events(&self, id: u32) -> Vec<Json> {
        let mut core = self.core.lock().expect("host core mutex poisoned");
        let mut events = Vec::new();
        let mut rest = Vec::new();
        for (queued, event) in std::mem::take(&mut core.oauth_notify_events) {
            if queued == id {
                events.push(event);
            } else {
                rest.push((queued, event));
            }
        }
        core.oauth_notify_events = rest;
        events
    }

    /// Takes the session's pending prompt ask, if any: `(token, payload)`.
    pub fn take_oauth_prompt(&self, id: u32) -> Option<(u64, Json)> {
        let mut core = self.core.lock().expect("host core mutex poisoned");
        let position = core
            .oauth_prompts
            .iter()
            .position(|(queued, _, _)| *queued == id)?;
        let (_, token, payload) = core.oauth_prompts.remove(position);
        Some((token, payload))
    }

    /// Resolves the session's pending prompt promise with the answer,
    /// verifying the token the ask handed out (`__piResolveOAuthPrompt`
    /// answers `"ok"`, `"no-prompt"`, or `"token-mismatch"`); the caller
    /// drains jobs so the login function continues.
    pub fn oauth_resolve_prompt(
        &self,
        id: u32,
        token: u64,
        answer: &Json,
    ) -> Result<String, JsHostError> {
        let answer_json =
            serde_json::to_string(answer).map_err(|err| JsHostError::Job(err.to_string()))?;
        self.ctx.with(|ctx| -> Result<String, JsHostError> {
            let resolve: Function = ctx.globals().get("__piResolveOAuthPrompt")?;
            let resolution: String = resolve
                .call((id, token, answer_json))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(resolution)
        })
    }

    /// Whether the provider registered an oauth `login` handler
    /// (`__piOAuthHasLogin`).
    pub fn oauth_has_login(&self, provider: &str) -> Result<bool, JsHostError> {
        self.ctx.with(|ctx| -> Result<bool, JsHostError> {
            let check: Function = ctx.globals().get("__piOAuthHasLogin")?;
            check
                .call((provider,))
                .map_err(|err| Self::js_error(&ctx, err))
        })
    }

    /// Flags the session's oauth `signal.aborted`
    /// (`__piAbortOAuthSignal`).
    pub fn oauth_abort_signal(&self, id: u32) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let abort: Function = ctx.globals().get("__piAbortOAuthSignal")?;
            let _: Value = abort.call((id,)).map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Takes the stream session's pending `onPayload` asks, in ask
    /// order; other sessions' asks are kept.
    pub fn take_provider_hook_requests(&self, id: u32) -> Vec<(u64, Json)> {
        let mut core = self.core.lock().expect("host core mutex poisoned");
        let mut taken = Vec::new();
        let mut rest = Vec::new();
        for (queued, token, payload) in std::mem::take(&mut core.provider_hook_requests) {
            if queued == id {
                taken.push((token, payload));
            } else {
                rest.push((queued, token, payload));
            }
        }
        core.provider_hook_requests = rest;
        taken
    }

    /// Takes the stream session's queued one-way hook events, in
    /// emission order; other sessions' events are kept.
    pub fn take_provider_hook_events(&self, id: u32) -> Vec<(String, Json)> {
        let mut core = self.core.lock().expect("host core mutex poisoned");
        let mut taken = Vec::new();
        let mut rest = Vec::new();
        for (queued, kind, payload) in std::mem::take(&mut core.provider_hook_events) {
            if queued == id {
                taken.push((kind, payload));
            } else {
                rest.push((queued, kind, payload));
            }
        }
        core.provider_hook_events = rest;
        taken
    }

    /// Resolves the stream session's pending `onPayload` promise with the
    /// transform chain's final value, verifying the ask's token
    /// (`__piResolveProviderHook` answers `"ok"`, `"no-prompt"`, or
    /// `"token-mismatch"`); the caller drains jobs so the handler resumes.
    pub fn provider_resolve_hook(
        &self,
        id: u32,
        token: u64,
        payload: &Json,
    ) -> Result<String, JsHostError> {
        let payload_json =
            serde_json::to_string(payload).map_err(|err| JsHostError::Job(err.to_string()))?;
        self.ctx.with(|ctx| -> Result<String, JsHostError> {
            let resolve: Function = ctx.globals().get("__piResolveProviderHook")?;
            let resolution: String = resolve
                .call((id, token, payload_json))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(resolution)
        })
    }

    /// Flags the stream session's `signal.aborted` — the run's abort
    /// reaching a handler that polls the crossed signal.
    pub fn provider_flip_signal(&self, id: u32) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let flip: Function = ctx.globals().get("__piFlipProviderSignal")?;
            let _: Value = flip.call((id,)).map_err(|err| Self::js_error(&ctx, err))?;
            Ok(())
        })
    }

    /// Calls the stashed oauth `refreshToken` (`__piCallOAuthRefresh`) and
    /// waits for its promise through the job queue. Returns the refreshed
    /// credential or the error text.
    pub fn call_oauth_refresh(
        &self,
        provider: &str,
        credential: &Json,
    ) -> Result<Json, JsHostError> {
        let credential_json =
            serde_json::to_string(credential).map_err(|err| JsHostError::Job(err.to_string()))?;
        {
            self.ctx.with(|ctx| -> Result<(), JsHostError> {
                let call: Function = ctx.globals().get("__piCallOAuthRefresh")?;
                let _: Value = call
                    .call((0, provider, credential_json))
                    .map_err(|err| Self::js_error(&ctx, err))?;
                Ok(())
            })?;
        }
        let mut probes: u32 = 0;
        loop {
            self.drive_jobs()?;
            let ready: bool = self.ctx.with(|ctx| -> Result<bool, JsHostError> {
                ctx.eval("__piOAuthCallReady()")
                    .map_err(|err| Self::js_error(&ctx, err))
            })?;
            if ready {
                break;
            }
            probes += 1;
            if probes > 100_000 {
                return Err(JsHostError::HandlerDidNotComplete(
                    "oauth refreshToken did not settle after 100k job drains".to_string(),
                ));
            }
        }
        let text: String = self.ctx.with(|ctx| -> Result<String, JsHostError> {
            ctx.eval("globalThis.__piOAuthCallText")
                .map_err(|err| Self::js_error(&ctx, err))
        })?;
        Ok(serde_json::from_str(&text).unwrap_or(Json::Null))
    }

    /// Calls the stashed oauth `getApiKey` (`__piCallOAuthApiKey`); the
    /// conversion is synchronous and answers inline.
    pub fn call_oauth_api_key(
        &self,
        provider: &str,
        credential: &Json,
    ) -> Result<Json, JsHostError> {
        let credential_json =
            serde_json::to_string(credential).map_err(|err| JsHostError::Job(err.to_string()))?;
        self.ctx.with(|ctx| -> Result<Json, JsHostError> {
            let call: Function = ctx.globals().get("__piCallOAuthApiKey")?;
            let text: String = call
                .call((0, provider, credential_json))
                .map_err(|err| Self::js_error(&ctx, err))?;
            Ok(serde_json::from_str(&text).unwrap_or(Json::Null))
        })
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
    fn url_global_is_available_without_importing_node_url() {
        // Regression: `pi-subagents-ported/index.ts:94` evaluates
        // `path.dirname(new URL(import.meta.url).pathname)` at module top
        // level without importing `node:url`. Before the bootstrap fix the
        // top-level throw was swallowed by the eval promise and the factory
        // ran against a half-evaluated module, producing "Semaphore is not
        // initialized" instead of loading.
        let dir = std::env::temp_dir().join(format!("pi-ext-js-url-global-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let entry = dir.join("main.ts");
        std::fs::write(
            &entry,
            r#"import * as path from "node:path";
const EXT_DIR = path.dirname(new URL(import.meta.url).pathname);
export default function () {
  (globalThis as Record<string, unknown>).__extDir = EXT_DIR;
  (globalThis as Record<string, unknown>).__urlType = typeof URL;
}
"#,
        )
        .expect("write");
        let host = host();
        let index = host
            .begin_extension(&entry.to_string_lossy())
            .expect("begin");
        host.load_extension_module(index, &entry.to_string_lossy(), None)
            .expect("URL global must exist at module top level");
        assert_eq!(
            host.eval_for_tests("globalThis.__extDir").expect("eval"),
            dir.to_string_lossy()
        );
        assert_eq!(
            host.eval_for_tests("globalThis.__urlType").expect("eval"),
            "function"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_world_shim_exports_resolve() {
        // Every export below was a link-time blocker for one of the four
        // real-world extensions; keep them together so a future shim edit
        // cannot silently break a load again.
        let dir = std::env::temp_dir().join(format!("pi-ext-js-exports-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let entry = dir.join("main.ts");
        std::fs::write(
            &entry,
            r#"import { mkdtempSync, chmodSync, constants as fsConstants } from "node:fs";
import { open, FileHandle } from "node:fs/promises";
import http from "node:http";
import { generateKeyPairSync, privateDecrypt, createPublicKey, constants as cryptoConstants } from "node:crypto";
import { complete } from "@earendil-works/pi-ai/compat";
import { estimateTokens, findCutPoint, sessionEntryToContextMessages } from "@earendil-works/pi-coding-agent";
export default function () {
  (globalThis as Record<string, unknown>).__shims = {
    mkdtemp: typeof mkdtempSync,
    chmod: typeof chmodSync,
    open: typeof open,
    handle: typeof FileHandle,
    listen: typeof http.createServer,
    keygen: typeof generateKeyPairSync,
    decrypt: typeof privateDecrypt,
    pubkey: typeof createPublicKey,
    oaep: cryptoConstants.RSA_PKCS1_OAEP_PADDING,
    createFlag: fsConstants.O_CREAT,
    complete: typeof complete,
    estimate: typeof estimateTokens,
    cut: typeof findCutPoint,
    project: typeof sessionEntryToContextMessages,
  };
}
"#,
        )
        .expect("write");
        let host = host();
        let index = host
            .begin_extension(&entry.to_string_lossy())
            .expect("begin");
        host.load_extension_module(index, &entry.to_string_lossy(), None)
            .expect("all real-world shim exports resolve");
        let out = host
            .eval_for_tests("JSON.stringify(globalThis.__shims)")
            .expect("eval");
        let value: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(value["mkdtemp"], json!("function"));
        assert_eq!(value["chmod"], json!("function"));
        assert_eq!(value["open"], json!("function"));
        assert_eq!(value["handle"], json!("function"));
        assert_eq!(value["listen"], json!("function"));
        assert_eq!(value["keygen"], json!("function"));
        assert_eq!(value["decrypt"], json!("function"));
        assert_eq!(value["pubkey"], json!("function"));
        assert_eq!(value["oaep"], json!(4));
        assert_eq!(value["createFlag"], json!(64));
        assert_eq!(value["complete"], json!("function"));
        assert_eq!(value["estimate"], json!("function"));
        assert_eq!(value["cut"], json!("function"));
        assert_eq!(value["project"], json!("function"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn intl_number_format_subset_is_installed() {
        let host = host();
        assert_eq!(
            host.eval_for_tests(
                "new Intl.NumberFormat('en-US', { maximumFractionDigits: 0 }).format(1234567.89)"
            )
            .expect("eval"),
            "1,234,568"
        );
        assert_eq!(
            host.eval_for_tests(
                "new Intl.NumberFormat('en-US', { style: 'percent', maximumFractionDigits: 0 }).format(0.125)"
            )
            .expect("eval"),
            "13%"
        );
    }

    #[test]
    fn module_evaluation_rejection_is_surfaced() {
        // A top-level throw must reach the caller as the load error; before
        // the eval-promise check this was swallowed and the loader reported a
        // later TDZ error from the factory instead.
        let host = host();
        let index = host
            .begin_extension("<test-eval-rejection>")
            .expect("begin");
        let err = host
            .load_extension_module(
                index,
                "<test-eval-rejection>",
                Some("throw new Error(\"top level boom\");\nexport default function () {}\n"),
            )
            .expect_err("module evaluation must reject");
        assert!(err.to_string().contains("top level boom"), "{err}");
        assert!(!err.to_string().contains("not initialized"), "{err}");
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
    fn url_can_parse_matches_the_constructor() {
        let dir = std::env::temp_dir().join(format!("pi-ext-js-url-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let entry = dir.join("url.ts");
        std::fs::write(
            &entry,
            r#"import { URL } from "node:url";
export default function (): void {
  const cases: Array<[string, string | undefined]> = [
    ["https://example.com/a", undefined],
    ["not a url", undefined],
    ["../a", "https://example.com/base/path"],
    ["../a", "not a base"],
    ["custom:thing", undefined],
    ["  https://example.com  ", undefined],
    ["https://", undefined],
  ];
  (globalThis as Record<string, unknown>).__canParse = JSON.stringify(
    cases.map(([value, base]) => URL.canParse(value, base)),
  );
  (globalThis as Record<string, unknown>).__ctorAgrees = JSON.stringify(
    cases.map(([value, base]) => {
      let constructed = true;
      try { new URL(value, base); } catch { constructed = false; }
      return URL.canParse(value, base) === constructed;
    }),
  );
}
"#,
        )
        .expect("write");
        let host = host();
        let index = host
            .begin_extension(&entry.to_string_lossy())
            .expect("begin");
        host.load_extension_module(index, &entry.to_string_lossy(), None)
            .expect("node:url now resolves");
        assert_eq!(
            host.eval_for_tests("globalThis.__canParse").expect("eval"),
            "[true,false,true,false,true,true,false]"
        );
        assert_eq!(
            host.eval_for_tests("globalThis.__ctorAgrees")
                .expect("eval"),
            "[true,true,true,true,true,true,true]"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn supported_node_imports_resolve_and_unsupported_ones_reject() {
        let dir = std::env::temp_dir().join(format!("pi-ext-js-node-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("main.ts"),
            "import { join } from \"node:path\";\nexport default function (): void {\n  globalThis.__used = join(\"a\", \"b\");\n}\n",
        )
        .expect("write");
        let host = host();
        let index = host
            .begin_extension(&dir.join("main.ts").to_string_lossy())
            .expect("begin");
        host.load_extension_module(index, &dir.join("main.ts").to_string_lossy(), None)
            .expect("node:path now resolves");
        assert_eq!(
            host.eval_for_tests("globalThis.__used").expect("eval"),
            "a/b"
        );

        std::fs::write(
            dir.join("unsupported.ts"),
            "import { x } from \"node:sqlite\";\nexport default function (): void { void x; }\n",
        )
        .expect("write");
        let index = host
            .begin_extension(&dir.join("unsupported.ts").to_string_lossy())
            .expect("begin");
        let err = host
            .load_extension_module(index, &dir.join("unsupported.ts").to_string_lossy(), None)
            .expect_err("unsupported node built-in must fail");
        assert!(err.to_string().contains("not polyfilled"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn process_global_is_defined_in_an_evaluated_extension() {
        let host = host();
        let index = host
            .begin_extension("<test-process-global>")
            .expect("begin");
        host.load_extension_module(
            index,
            "<test-process-global>",
            Some(
                r#"
import processModule from "node:process";
export default function () {
  globalThis.__proc = {
    defined: typeof process !== "undefined",
    sameModule: processModule === globalThis.process,
    envObject: typeof process.env === "object" && process.env !== null,
    cwd: process.cwd(),
    platform: process.platform,
    platformIsString: typeof process.platform === "string",
    argv: process.argv,
    argvIsArray: Array.isArray(process.argv),
    hasStdout: typeof process.stdout.write === "function",
    hasStderr: typeof process.stderr.write === "function",
    initialCode: process.exitCode,
  };
  // No-exit semantics: the code is recorded, the host keeps running.
  process.exit(7);
  globalThis.__proc.afterExit = process.exitCode;
  process.on("SIGTERM", () => {});
  globalThis.__proc.signalListeners = process.listenerCount("SIGTERM");
  process.stdout.write("from extension\n");
}
"#,
            ),
        )
        .expect("load extension using the global process");
        let out = host
            .eval_for_tests("JSON.stringify(globalThis.__proc)")
            .expect("eval");
        let value: Json = serde_json::from_str(&out).expect("json");
        assert_eq!(value["defined"], true);
        assert_eq!(value["sameModule"], true);
        assert_eq!(value["envObject"], true);
        assert_eq!(value["cwd"], "/tmp");
        assert_eq!(value["platform"], "linux");
        assert_eq!(value["platformIsString"], true);
        assert_eq!(value["argvIsArray"], true);
        assert_eq!(
            value["argv"],
            serde_json::json!(["node"]),
            "process.argv must include the runtime name: {value}"
        );
        assert_eq!(value["hasStdout"], true);
        assert_eq!(value["hasStderr"], true);
        assert_eq!(value["initialCode"], 0);
        assert_eq!(value["afterExit"], 7);
        assert_eq!(value["signalListeners"], 1);
    }

    #[test]
    fn structured_clone_global_copies_plain_data_and_cycles() {
        let host = host();
        assert_eq!(
            host.eval_for_tests(
                r#"(() => {
  const source = { nested: [1, 2], when: new Date(0) };
  source.self = source;
  const copy = structuredClone(source);
  return JSON.stringify({
    defined: typeof structuredClone === "function",
    equal: copy.nested[1] === 2 && copy.when.getTime() === 0,
    cycle: copy.self === copy,
    detached: copy !== source,
  });
})()"#,
            )
            .expect("eval"),
            r#"{"defined":true,"equal":true,"cycle":true,"detached":true}"#
        );
    }

    #[test]
    fn import_meta_url_is_available_to_extension_modules() {
        let dir = std::env::temp_dir().join(format!("pi-ext-js-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let entry = dir.join("main.ts");
        let source = r#"
import { fileURLToPath } from "node:url";
export default function () {
  globalThis.__meta = { url: import.meta.url, path: fileURLToPath(import.meta.url) };
}
"#;
        std::fs::write(&entry, source).expect("write");
        let host = host();
        let index = host
            .begin_extension(&entry.to_string_lossy())
            .expect("begin");
        host.load_extension_module(index, &entry.to_string_lossy(), None)
            .expect("import.meta.url available");
        let out = host
            .eval_for_tests("JSON.stringify(globalThis.__meta)")
            .expect("eval");
        let value: Json = serde_json::from_str(&out).expect("json");
        assert_eq!(value["url"], format!("file://{}", entry.to_string_lossy()));
        assert_eq!(value["path"], entry.to_string_lossy().as_ref());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn node_module_create_require_resolves_virtual_and_relative_modules() {
        let dir = std::env::temp_dir().join(format!("pi-ext-js-require-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("helper.ts"),
            "export const tag = \"from-helper\";\n",
        )
        .expect("write helper");
        let entry = dir.join("main.ts");
        std::fs::write(
            &entry,
            r#"
import { createRequire } from "node:module";
const req = createRequire(import.meta.url);
export default function () {
  globalThis.__require = {
    join: req("node:path").join("a", "b"),
    resolved: req.resolve("node:path"),
    helper: req("./helper.ts").tag,
  };
}
"#,
        )
        .expect("write entry");
        let host = host();
        let index = host
            .begin_extension(&entry.to_string_lossy())
            .expect("begin");
        host.load_extension_module(index, &entry.to_string_lossy(), None)
            .expect("node:module resolves");
        let out = host
            .eval_for_tests("JSON.stringify(globalThis.__require)")
            .expect("eval");
        let value: Json = serde_json::from_str(&out).expect("json");
        assert_eq!(value["join"], "a/b", "{value}");
        assert_eq!(value["resolved"], "virtual:node:path", "{value}");
        assert_eq!(value["helper"], "from-helper", "{value}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_open_sync_handle_reads_writes_and_closes() {
        let dir = std::env::temp_dir().join(format!("pi-ext-js-fd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("data.txt");
        std::fs::write(&path, b"hello").expect("seed");
        let source = r#"
import { openSync, closeSync, readSync, writeSync, readFileSync } from "node:fs";
export default function () {
  const path = __PATH__;
  const fd = openSync(path, "r+");
  const buffer = Buffer.alloc(8);
  const read = readSync(fd, buffer, 0, 5, 0);
  writeSync(fd, Buffer.from("!"), 0, 1, 5);
  closeSync(fd);
  globalThis.__fd = {
    fd,
    read,
    text: JSON.stringify(Array.prototype.slice.call(buffer, 0, read)),
    after: readFileSync(path, "utf8"),
  };
}
"#
        .replace(
            "__PATH__",
            &serde_json::to_string(&path.to_string_lossy()).expect("json"),
        );
        let host = host();
        let index = host.begin_extension("<test-fd>").expect("begin");
        host.load_extension_module(index, "<test-fd>", Some(&source))
            .expect("load fs handle extension");
        let out = host
            .eval_for_tests("JSON.stringify(globalThis.__fd)")
            .expect("eval");
        let value: Json = serde_json::from_str(&out).expect("json");
        assert!(value["fd"].as_i64().is_some_and(|fd| fd >= 100));
        assert_eq!(value["read"], 5);
        assert_eq!(value["text"], "[104,101,108,108,111]");
        assert_eq!(value["after"], "hello!");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn child_process_exec_file_settles_callback_and_promise() {
        let host = host();
        let index = host.begin_extension("<test-exec-file>").expect("begin");
        host.load_extension_module(
            index,
            "<test-exec-file>",
            Some(
                r#"
import { execFile } from "node:child_process";
export default function () {
  globalThis.__execFile = {};
  execFile("/bin/echo", ["callback"], (error, stdout, stderr) => {
    globalThis.__execFile.callback = {
      error: error ? String(error) : null,
      stdout: String(stdout).trim(),
      stderr: String(stderr).trim(),
    };
  });
  (async () => {
    try {
      const result = await execFile("/bin/echo", ["promise"]);
      globalThis.__execFile.promise = String(result.stdout).trim();
    } catch (error) {
      globalThis.__execFile.promise = "ERR:" + String(error);
    }
  })();
}
"#,
            ),
        )
        .expect("load execFile extension");
        let out = host
            .eval_for_tests("JSON.stringify(globalThis.__execFile)")
            .expect("eval");
        let value: Json = serde_json::from_str(&out).expect("json");
        assert_eq!(value["callback"]["error"], Json::Null);
        assert_eq!(value["callback"]["stdout"], "callback");
        assert_eq!(value["callback"]["stderr"], "");
        assert_eq!(value["promise"], "promise");
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
