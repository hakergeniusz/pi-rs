//! The `ExtensionRunner` port: dispatch semantics, aggregation, tool and
//! command execution.
//!
//! Ported method-by-method from `core/extensions/runner.ts` (1.5.2); §2 of
//! `docs/EXTENSIONS.md` tabulates the per-event semantics. The runner owns
//! all dispatch decisions in Rust; the JS side (engine + glue) only holds
//! the registries and live event objects. Aggregation semantics are the
//! upstream ones: tools and flags are first-wins per name across
//! extensions in load order (`getAllRegisteredTools`, `getFlags`,
//! `runner.ts:637-679`), duplicate command names get `name:occurrence`
//! invocation names with the collision-avoidance loop
//! (`resolveRegisteredCommands`, `runner.ts:821-855`), and handler errors
//! fan out through error listeners (`emitError`). The provider-stream
//! surface is the host half of `streamWith`'s extension arm
//! (`core/provider-composer.ts:588-613`): the stashed `streamSimple`
//! handler (`core/extensions/types.ts:1989-2007`) is invoked with the
//! marshalled request and its event stream drains through the
//! `providerStreamEvent` op. The crossed options' instrumentation is
//! serviced by the drain: `onPayload` runs `before_provider_request`'s
//! transform chain (`emitBeforeProviderRequest`, `runner.ts:1375-1398`;
//! pi sets the three hooks on every run, `sdk.ts:424-426`), the one-way
//! `onResponse`/`onProviderStreamEvent` events emit FIFO, and the run's
//! abort flips the crossed `signal` object. The oauth surface is the host half of
//! `adaptOAuth` (`core/provider-composer.ts:358-377, 488-511`):
//! [`ExtensionRunner::oauth_login_start`] drives the stashed `login` in a
//! dispatch session (notifications poll through the oauth slots, prompts
//! suspend it until [`ExtensionRunner::oauth_login_answer`], the adapter
//! `signal` flips through [`ExtensionRunner::oauth_login_abort`]), and
//! [`ExtensionRunner::oauth_refresh`]/[`ExtensionRunner::oauth_api_key`]
//! are the one-shot `refreshToken`/`getApiKey` calls. The runner is also
//! the host of the native MCP server manager (the port's `builtin:mcp`):
//! [`ExtensionRunner::connect_mcp_servers`] connects the servers
//! `pi.registerMcpServer` registered (upstream: the bundled extension's
//! `session_start` handler, `index.ts:1112-1149`), the connected tools
//! register into a synthetic record through the bridge
//! (`__piMcpRegisterTools` — the `pi.registerTool` surface), and registry
//! changes reconcile into `mcp_servers_change` emissions at the end of
//! every dispatch (the change listener, `runner.ts:465-466`). The UI
//! surface (E3 slices 6a/6b) is the host half of `wrapUIPromptContext`
//! (`runner.ts:577-613`): dialog methods emit the depth-gated
//! `ui_prompt_*` pair around the mode's answer, interactive `ctx.ui.custom`
//! mounts the factory's adopted component and resolves its deferred from
//! the `customDone` crossing before the pair ends (upstream's
//! `.finally(finish)` order, `runner.ts:585-611`), and the pump's
//! deliveries split into [`ExtensionRunner::begin_ui_dialog`] +
//! [`ExtensionRunner::complete_ui_dialog`] so a dialog's wait never holds
//! the runner mutex.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::{Value as Json, json};

use crate::engine::{ContextKind, HostCore, JsHost};
use crate::error::{ExtensionError, JsHostError};
use crate::events::{
    self, BeforeAgentStartCombined, BeforeAgentStartResult, BeforeAgentStartTrigger,
    BoundaryDispatchResult, InputResult, ProjectTrustResult, SessionBeforeResult, SessionOutcome,
    ToolCallResult, ToolResultPatch, parse_session_outcome,
};
use crate::mcp_registry::{
    create_mcp_tool_name, get_mcp_tool_exposure, mcp_namespace, to_tool_exposure,
};
use crate::transcript;

/// One registered tool as the runner sees it (the E1 `ToolInfo`:
/// name, description, parameters, exposure, namespace — §4.2 of
/// `docs/EXTENSIONS.md`; `promptGuidelines`/`reachesTools`/`annotations`
/// are E2 host concerns).
#[derive(Debug, Clone, Deserialize)]
pub struct ToolInfo {
    /// The tool name.
    pub name: String,
    /// The display label (the MCP bridge sets it to `server/tool`,
    /// `tools.ts:263`; everything else falls back to the name).
    #[serde(default)]
    pub label: String,
    /// The model-facing description.
    #[serde(default)]
    pub description: String,
    /// The JSON-schema parameters.
    #[serde(default)]
    pub parameters: Option<Json>,
    /// The exposure (`direct` | `model-only` | `codemode` | `deferred` |
    /// `hidden`).
    #[serde(default = "default_exposure")]
    pub exposure: String,
    /// The tool namespace (MCP servers set this).
    #[serde(default)]
    pub namespace: Option<Json>,
    /// Whether the tool starts active (`defaultActive !== false`).
    #[serde(rename = "defaultActive", default = "default_true")]
    pub default_active: bool,
    /// Whether an `execute` was registered.
    #[serde(rename = "hasExecute", default)]
    pub has_execute: bool,
}

fn default_exposure() -> String {
    "direct".to_string()
}

fn default_true() -> bool {
    true
}

/// A command with its disambiguated invocation name
/// (`ResolvedCommand`).
#[derive(Debug, Clone)]
pub struct ResolvedCommand {
    /// The registered name.
    pub name: String,
    /// The registered description.
    pub description: String,
    /// The `name` or `name:occurrence` form users invoke.
    pub invocation_name: String,
    /// The extension that registered it.
    pub extension_path: String,
}

/// The outcome of one provider-stream call: the events the handler
/// emitted, in emission order, plus the session error when the handler
/// threw (before or mid-iteration). Events emitted before a throw still
/// arrive, mirroring upstream's lazyStream — a setup/iteration failure
/// appends the terminal error event after the events already forwarded
/// (`packages/ai/src/api/lazy.ts:65-105`).
#[derive(Debug, Default)]
pub struct ProviderStreamOutcome {
    /// The protocol events, in emission order.
    pub events: Vec<Json>,
    /// The handler's error message, when it threw.
    pub error: Option<String>,
}

/// One step of an oauth login flow (`oauth_login_poll`): the
/// notifications emitted since the last poll, the prompt now awaiting a
/// host answer, and the settled credential or error when the login
/// function finished.
#[derive(Debug, Default)]
pub struct OAuthLoginPoll {
    pub notifications: Vec<Json>,
    pub prompt: Option<(u64, Json)>,
    /// `Some(Ok(credential))` on success, `Some(Err(message))` on throw.
    pub settled: Option<Result<Json, String>>,
}

/// One flag registration (`ExtensionFlag`).
#[derive(Debug, Clone, Deserialize)]
pub struct FlagSpec {
    /// The flag name.
    pub name: String,
    /// The help description.
    #[serde(default)]
    pub description: String,
    /// `"boolean"` or `"string"`.
    #[serde(default)]
    pub kind: String,
}

/// The Rust-side record of one loaded extension (the inspect snapshot of
/// upstream's `Extension`, `types.ts:2317-2333`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExtensionSnapshot {
    /// The path (or synthetic `builtin:<name>` path).
    pub path: String,
    /// Event name → handler indexes in registration order.
    #[serde(default)]
    pub events: HashMap<String, Vec<usize>>,
    /// Registered tools.
    #[serde(default)]
    pub tools: Vec<ToolInfo>,
    /// Registered commands.
    #[serde(default)]
    pub commands: Vec<RegisteredCommandSpec>,
    /// Registered flags.
    #[serde(default)]
    pub flags: Vec<FlagSpec>,
    /// Registered shortcuts.
    #[serde(default)]
    pub shortcuts: Vec<ShortcutSpec>,
    /// The one registered markdown transformer, with its surfaces (the
    /// transformer fn stays in the JS-side record).
    #[serde(default, rename = "markdownTransformer")]
    pub markdown_transformer: Option<MarkdownTransformerSpec>,
    /// The custom entry types this extension registered a renderer for.
    #[serde(default, rename = "entryRenderers")]
    pub entry_renderers: Vec<String>,
    /// The custom message types this extension registered a renderer for.
    #[serde(default, rename = "messageRenderers")]
    pub message_renderers: Vec<String>,
    /// How many tool-renderer resolvers this extension registered
    /// (`toolRenderers`, `loader.ts:379-383` — resolvers accumulate).
    #[serde(default, rename = "toolRenderers")]
    pub tool_renderers: u32,
}

/// `{name, description}` of one registered command.
#[derive(Debug, Clone, Deserialize)]
pub struct RegisteredCommandSpec {
    /// The command name.
    pub name: String,
    /// The description.
    #[serde(default)]
    pub description: String,
}

/// `{shortcut, description}` of one registered shortcut (upstream stores
/// `{shortcut, extensionPath, description?, handler}` — `loader.ts:337`;
/// the handler stays in the JS-side record, the inspect snapshot carries
/// the metadata).
#[derive(Debug, Clone, Deserialize)]
pub struct ShortcutSpec {
    /// The chord exactly as registered (normalization happens in
    /// `get_shortcuts`).
    pub shortcut: String,
    /// The optional description.
    #[serde(default)]
    pub description: String,
}

/// Extension shortcuts compete with canonical keybinding ids; only the
/// editor-global actions are reserved — picker-specific bindings are not
/// (`RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS`, `runner.ts:96-114`).
const RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS: &[&str] = &[
    "app.interrupt",
    "app.clear",
    "app.exit",
    "app.suspend",
    "app.thinking.cycle",
    "app.model.cycleForward",
    "app.model.cycleBackward",
    "app.model.select",
    "app.tools.expand",
    "app.thinking.toggle",
    "app.editor.external",
    "app.message.copy",
    "app.message.followUp",
    "tui.input.submit",
    "tui.select.confirm",
    "tui.select.cancel",
    "tui.input.copy",
    "tui.editor.deleteToLineEnd",
];

/// One shortcut-conflict warning (a `type: "warning"` `ResourceDiagnostic`
/// upstream; the runner only produces warnings for shortcuts).
#[derive(Debug, Clone)]
pub struct ShortcutDiagnostic {
    /// The verbatim upstream warning text.
    pub message: String,
    /// The owning extension's path.
    pub path: String,
}

/// The surfaces of one registered markdown transformer
/// (`MarkdownTransformerRegistration`, `types.ts:1590-1593`; the
/// transformer fn stays in the JS-side record keyed by the extension
/// index).
#[derive(Debug, Clone, Deserialize)]
pub struct MarkdownTransformerSpec {
    /// The `MarkdownMessageType` names the transformer runs on.
    #[serde(default, rename = "messageTypes")]
    pub message_types: Vec<String>,
}

/// One extension markdown transformer, resolved in load order
/// (`getMarkdownTransformers`, `runner.ts:793-803`: each one leaves
/// markdown of unregistered surfaces unchanged — the surface check runs at
/// application time here).
#[derive(Debug, Clone)]
pub struct ResolvedMarkdownTransformer {
    /// The owning extension's index in the snapshot list (the JS-side
    /// `__piMarkdownTransform` key).
    pub ext_index: usize,
    /// The owning extension's path.
    pub extension_path: String,
    /// The `MarkdownMessageType` names the transformer runs on.
    pub message_types: Vec<String>,
}

/// The embedder-installed `theme.fg`/`theme.bg` mapping
/// ([`Self::set_theme_surface`]): token name + text in, styled text out.
pub type ThemeSurfaceFn = std::sync::Arc<dyn Fn(&str, &str) -> String + Send + Sync>;

/// The embedder-installed mode-state reads the synchronous ui queries
/// serve ([`Self::set_ui_snapshot_surface`], E3 slice 7): one JSON footer
/// snapshot `{gitBranch, extensionStatuses, availableProviderCount}`, the
/// editor's expanded text, and the tool-expansion flag.
pub struct UiSnapshotSurface {
    /// `footerData` op — the custom footer factory's data adapter reads.
    pub footer_data: std::sync::Arc<dyn Fn() -> Json + Send + Sync>,
    /// `editorText` op — `ctx.ui.getEditorText()`.
    pub editor_text: std::sync::Arc<dyn Fn() -> String + Send + Sync>,
    /// `toolsExpanded` op — `ctx.ui.getToolsExpanded()`.
    pub tools_expanded: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
}

/// One `ToolRenderers` slot after resolution (`types.ts:693`): the
/// built-in rendering stays Rust-side (`builtin`), a resolver-supplied fn
/// is bridged through the stash (`custom`), and `absent` renders the row's
/// generic fallback (`tool-execution.ts:300-301`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRenderSlot {
    /// The built-in (or the tool definition's own Rust-side) rendering.
    Builtin,
    /// A resolver-supplied function; cross through the stash.
    Custom,
    /// No fn at all — the generic fallback.
    Absent,
}

/// The winning `ToolRenderers` for one tool row
/// (`getRegisteredToolDefinition`, `interactive-mode.ts:2356-2360`): the
/// object is stashed JS-side behind `stash_id` and lives until the row is
/// dropped ([`Self::tool_renderer_release`]).
#[derive(Debug, Clone)]
pub struct ResolvedToolRenderers {
    /// The stash key for [`Self::tool_render_call`] /
    /// [`Self::tool_render_result`].
    pub stash_id: u32,
    /// `renderShell` (`types.ts:616`): `self` rows draw their own framing.
    pub render_shell_self: bool,
    /// The call-line slot.
    pub render_call: ToolRenderSlot,
    /// The result-body slot.
    pub render_result: ToolRenderSlot,
}

/// What one `resolveToolRenderers` chain produced: the stashed renderers,
/// nothing (a resolver returned undefined without calling `next` — the row
/// takes the generic path), or the resolver's failure text (upstream
/// propagates a throw into row creation; the port falls back).
#[derive(Debug, Clone)]
pub enum ToolRenderersOutcome {
    Renderers(ResolvedToolRenderers),
    None,
    Failed(String),
}

/// What one entry-renderer call produced (`CustomEntryComponent.rebuild`
/// upstream): the adopted component's handle + input flag, the renderer's
/// failure text, or nothing.
#[derive(Debug, Clone)]
pub enum EntryRenderOutcome {
    /// The renderer returned a component; it lives under the handle in the
    /// JS registry until released.
    Component {
        /// The component handle (the `__piComponent*` calls' key).
        handle: u32,
        /// Whether the JS component implements `handleInput`.
        has_input: bool,
    },
    /// The renderer threw; the message goes into the host's error box.
    Failed(String),
    /// No renderer for the type, or the renderer returned undefined.
    None,
}

/// One extension shortcut the resolution accepted (`ExtensionShortcut`
/// upstream — the port keeps the owning extension's index instead of the
/// handler closure).
#[derive(Debug, Clone)]
pub struct ResolvedShortcut {
    /// The normalized (lowercased) chord the dispatcher matches against.
    pub key: String,
    /// The chord exactly as registered (the JS registry is keyed by it).
    pub raw_key: String,
    /// The owning extension's index in the snapshot list.
    pub ext_index: usize,
    /// The owning extension's path.
    pub extension_path: String,
    /// The optional description.
    pub description: String,
}

/// `buildBuiltinKeybindings` (`runner.ts:121-141`): map every chord the
/// resolved keybindings bind to its action, with `restrictOverride` for
/// the reserved ids. When two actions bind the same chord, the reserved
/// action wins so extensions stay blocked regardless of iteration order.
fn build_builtin_keybindings<'a>(
    resolved_keybindings: &[(&'a str, &[&'a str])],
) -> HashMap<String, (&'a str, bool)> {
    let mut builtins: HashMap<String, (&'a str, bool)> = HashMap::new();
    for (keybinding, keys) in resolved_keybindings {
        let restrict_override = RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS.contains(keybinding);
        for key in *keys {
            let normalized = key.to_lowercase();
            let blocked = builtins
                .get(&normalized)
                .is_some_and(|&(_, existing_restrict)| existing_restrict && !restrict_override);
            if blocked {
                continue;
            }
            builtins.insert(normalized, (*keybinding, restrict_override));
        }
    }
    builtins
}

/// The synthetic path of the native MCP host (`builtin:<name>` synthetic
/// paths, `package-manager.ts:1041-1056` — upstream loads the bundled
/// extension from a real module with this path; the port registers its
/// tools into a synthetic record the runner creates on first connect).
pub const MCP_EXTENSION_PATH: &str = "builtin:mcp";

/// The runner's view of one MCP server (`McpServer`, `index.ts:103-116`).
///
/// Headless subset: the registered config (for re-registration detection,
/// `index.ts:1221-1228`), whether the connection is up, and the tool names
/// currently offered.
#[derive(Debug, Clone)]
struct McpServerState {
    name: String,
    registered_config: Json,
    connected: bool,
    tools: Vec<String>,
    error: Option<String>,
}

/// One server's status for the host report (`describeState`,
/// `index.ts:232-250`, reduced to the connect outcomes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerReport {
    /// The server name.
    pub name: String,
    /// `"connected"` | `"failed"` | `"disabled"`.
    pub state: String,
    /// Tools the server offered at connect.
    pub tool_count: usize,
    /// The connect failure, when the state is `failed`.
    pub error: Option<String>,
}

/// An error from a tool's `execute` (the host wraps it into an
/// `isError: true` result, as agent-core does upstream).
#[derive(Debug, thiserror::Error)]
#[error("tool `{name}` failed: {message}")]
pub struct ToolExecutionError {
    /// The tool name.
    pub name: String,
    /// The error message.
    pub message: String,
}

/// The runner's error listeners (`onError`). `Send` so `ExtensionRunner`
/// stays `Send` (rquickjs `Runtime`/`Context` already are without the
/// `parallel` feature) and the host can share it as
/// `Arc<Mutex<ExtensionRunner>>`.
type ErrorListeners = Vec<Box<dyn Fn(&ExtensionError) + Send>>;

/// The runner: dispatch semantics over the loaded extensions.
/// The host side of the UI-over-protocol surface (E2): every `ctx.ui`
/// request that crosses the engine boundary reaches [`UiSink::request`],
/// and dialog requests additionally block on
/// [`UiSink::wait_response`] — the sink owns the transport (RPC lines in
/// E2), the timeout default resolution, and the response shapes
/// (`{value}`, `{confirmed}`, `{cancelled:true}`, `{}`).
pub trait UiSink: Send + Sync {
    /// Delivers one request (`{id, method, ...params}`). Fire-and-forget
    /// methods never get a response; dialog methods must eventually
    /// produce one through [`UiSink::wait_response`].
    fn request(&self, request: &Json);

    /// Waits for the response to the dialog request with this id. The
    /// sink resolves its own timeout default (the request payload carries
    /// the `timeout` field upstream includes in the wire request).
    fn wait_response(&self, id: &str, method: &str) -> Json;
}

/// One delivered dialog awaiting its mode response
/// ([`ExtensionRunner::begin_ui_dialog`]'s return). The wait runs on the
/// delivery thread WITHOUT the runner mutex held — upstream's pending
/// dialogs hold nothing but the JS dispatch they started in, and the
/// port's component render/input crossings need the mutex while a dialog
/// shows (E3 slice 6b).
pub struct UiDialogWait {
    sink: std::sync::Arc<dyn UiSink>,
    id: String,
    method: String,
    /// Whether the delivery opened a `ui_prompt` pair. The dialog methods
    /// wrap their wait in the pair; the answer-class overlay crossings (E3
    /// slice 6c — `isHidden`/`isFocused`/`getBounds`) read a value without
    /// blocking like a dialog, so they open no pair.
    prompt: bool,
}

impl UiDialogWait {
    /// The dialog request's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Blocks for the mode's response. Call this OUTSIDE the runner guard;
    /// complete through [`ExtensionRunner::complete_ui_dialog`].
    pub fn wait(&self) -> Json {
        self.sink.wait_response(&self.id, &self.method)
    }
}

/// What a `user_bash` handler decided (`UserBashEventResult`): custom
/// `operations` the host invokes through [`ExtensionRunner::call_bash_exec`],
/// or a full `result` replacing local execution.
#[derive(Debug, Clone)]
pub enum UserBashDecision {
    /// The handler supplied `operations`; the host runs commands through
    /// the stashed `exec`.
    Operations,
    /// The handler executed the command itself; this is the full
    /// `BashResult`.
    Result(Json),
}

pub struct ExtensionRunner {
    host: JsHost,
    core: std::sync::Arc<Mutex<HostCore>>,
    extensions: Vec<ExtensionSnapshot>,
    error_listeners: ErrorListeners,
    /// The bound UI surface; `None` keeps the headless no-op matrix.
    ui_sink: Option<std::sync::Arc<dyn UiSink>>,
    /// `withUIPrompt` depth (runner.ts:589-613): only the outermost
    /// dialog emits `ui_prompt_start`/`ui_prompt_end`.
    ui_prompt_depth: u32,
    /// The outermost prompt's kind/title, replayed on its end event.
    active_ui_prompt: Option<(String, Option<String>)>,
    /// The MCP server connection states the native manager drives
    /// (`McpServer`, `index.ts:103-116` — the extension-registered half;
    /// `mcp.json` entries are P5).
    mcp_servers: Vec<McpServerState>,
    /// Last definition registered under each bridge tool name, to
    /// re-register withdrawn tools as hidden (`index.ts:377`).
    mcp_definitions: HashMap<String, Json>,
    /// pi tool name to the `<server>\0<tool>` it was assigned to, so names
    /// stay unique and stable (`index.ts:373`).
    mcp_tool_owners: HashMap<String, String>,
    /// Whether a `mcp_servers_change` drain is running (a change made by a
    /// handler of the event itself must not recurse).
    mcp_draining: bool,
    /// The shortcuts the last `get_shortcuts` resolved, in load order with
    /// per-chord last-wins (`ExtensionShortcut` map upstream — the port
    /// keeps the ext index instead of the handler closure).
    resolved_shortcuts: Vec<ResolvedShortcut>,
    /// The warnings the last `get_shortcuts` produced
    /// (`ResourceDiagnostic` entries upstream; the runner only emits
    /// `type: "warning"` diagnostics for shortcuts).
    shortcut_diagnostics: Vec<ShortcutDiagnostic>,
}

impl ExtensionRunner {
    /// Creates a runner over a fresh host with the given core state.
    pub fn new(core: HostCore) -> Result<Self, JsHostError> {
        let core = {
            let host = JsHost::new(core)?;
            let core = host.core().clone();
            (host, core)
        };
        Ok(Self {
            host: core.0,
            core: core.1,
            extensions: Vec::new(),
            error_listeners: Vec::new(),
            ui_sink: None,
            ui_prompt_depth: 0,
            active_ui_prompt: None,
            mcp_servers: Vec::new(),
            mcp_definitions: HashMap::new(),
            mcp_tool_owners: HashMap::new(),
            mcp_draining: false,
            resolved_shortcuts: Vec::new(),
            shortcut_diagnostics: Vec::new(),
        })
    }

    /// The shared host core, for host-level state assertions in tests.
    pub fn core(&self) -> &std::sync::Arc<Mutex<HostCore>> {
        &self.core
    }

    /// Registers an error listener (`onError`); every handler error and
    /// load-time failure reaches it. Listeners must be `Send` so the runner
    /// can live behind a mutex shared with the agent loop.
    pub fn on_error(&mut self, listener: Box<dyn Fn(&ExtensionError) + Send>) {
        self.error_listeners.push(listener);
    }

    /// Paths of the loaded extensions, in load order.
    pub fn extension_paths(&self) -> Vec<String> {
        self.extensions.iter().map(|ext| ext.path.clone()).collect()
    }

    /// Whether any extension registered a handler for `event`.
    pub fn has_handlers(&self, event: &str) -> bool {
        self.extensions.iter().any(|ext| {
            ext.events
                .get(event)
                .is_some_and(|handlers| !handlers.is_empty())
        })
    }

    /// Loads one extension: begin → module eval → factory → commit, with
    /// `discard` on failure (the `initializeExtension` lifecycle,
    /// `loader.ts:625-644`). Errors propagate to the caller, which turns
    /// them into `LoadError` values.
    pub fn load_extension(&mut self, path: &str, source: Option<&str>) -> Result<(), JsHostError> {
        let index = self.host.begin_extension(path)?;
        let result = self.host.load_extension_module(index, path, source);
        match result {
            Ok(()) => self.host.commit_extension(index)?,
            Err(err) => {
                self.host.discard_extension(index)?;
                return Err(err);
            }
        }
        self.refresh_snapshots()?;
        Ok(())
    }

    /// Binds the runtime and syncs the aggregated tool state the ops
    /// serve from (`bindCore`, `runner.ts:417-551`, plus the first-wins
    /// tool aggregation): tools and flags are first-wins per name across
    /// extensions in load order, and the initial active set is every
    /// declared tool with `defaultActive !== false` — `direct` and
    /// `model-only` exposures are declared from the start; `codemode` and
    /// `deferred` become declared only when activated (§4.4 of
    /// `docs/EXTENSIONS.md`), which is exactly the candidate pool
    /// `tool_search` manages.
    pub fn bind(&mut self) -> Result<(), JsHostError> {
        self.refresh_snapshots()?;
        // Pre-bind registry changes produce no event (the listener is only
        // set at bindCore, `runner.ts:465`); the queue accumulated during
        // loading and commit drops here.
        {
            let mut core = self.core.lock().expect("host core mutex poisoned");
            core.mcp_changes.clear();
        }
        let mut tool_infos: Vec<Json> = Vec::new();
        let mut registered: Vec<String> = Vec::new();
        let mut hidden: Vec<String> = Vec::new();
        let mut active: Vec<String> = Vec::new();
        for ext in &self.extensions {
            for tool in &ext.tools {
                if registered.iter().any(|name| name == &tool.name) {
                    continue;
                }
                registered.push(tool.name.clone());
                if tool.exposure == "hidden" {
                    hidden.push(tool.name.clone());
                }
                let declared = matches!(tool.exposure.as_str(), "direct" | "model-only");
                if tool.default_active && declared {
                    active.push(tool.name.clone());
                }
                tool_infos.push(serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters.clone().unwrap_or(Json::Null),
                    "exposure": tool.exposure,
                    "namespace": tool.namespace.clone().unwrap_or(Json::Null),
                }));
            }
        }
        {
            let mut core = self.core.lock().expect("host core mutex poisoned");
            core.registered_tool_names = registered;
            core.hidden_tool_names = hidden;
            core.active_tools = active;
            core.all_tools = tool_infos;
        }
        self.host.bind_runtime()?;
        Ok(())
    }

    /// The post-bind tool-state recompute (`refreshTools`): the aggregated
    /// registry crosses again and `names` join the active set — a newly
    /// registered declarable, default-active tool activates on
    /// registration (`_refreshToolRegistry`, `agent-session.ts:3742-3753`,
    /// `_isActivatedOnRegistration`, `:3766-3769`).
    fn sync_tool_state(&mut self, names: &[String]) -> Result<(), JsHostError> {
        self.refresh_snapshots()?;
        let mut tool_infos: Vec<Json> = Vec::new();
        let mut registered: Vec<String> = Vec::new();
        let mut hidden: Vec<String> = Vec::new();
        for ext in &self.extensions {
            for tool in &ext.tools {
                if registered.iter().any(|name| name == &tool.name) {
                    continue;
                }
                registered.push(tool.name.clone());
                if tool.exposure == "hidden" {
                    hidden.push(tool.name.clone());
                }
                tool_infos.push(serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters.clone().unwrap_or(Json::Null),
                    "exposure": tool.exposure,
                    "namespace": tool.namespace.clone().unwrap_or(Json::Null),
                }));
            }
        }
        let mut core = self.core.lock().expect("host core mutex poisoned");
        let mut active = core.active_tools.clone();
        for name in names {
            if !active.contains(name) {
                active.push(name.clone());
            }
        }
        active.retain(|name| registered.contains(name) && !hidden.contains(name));
        core.registered_tool_names = registered;
        core.hidden_tool_names = hidden;
        core.active_tools = active;
        core.all_tools = tool_infos;
        Ok(())
    }

    /// The `session_start` connection pass for extension-registered
    /// servers (`index.ts:1112-1149`, minus the `mcp.json` half, which is
    /// P5).
    ///
    /// Every enabled server connects; failures are per-server values — the
    /// failed state shows in the report and the server's tools stay
    /// absent, never failing the host. Registry-state or engine failures
    /// are the error value.
    pub fn connect_mcp_servers(&mut self) -> Result<Vec<McpServerReport>, JsHostError> {
        self.reconcile_mcp_servers()
    }

    /// The registry/connection reconciliation (`index.ts:1215-1248`
    /// without the event emission).
    ///
    /// Servers unregistered or re-registered with a new config are dropped
    /// (tools hidden, connection closed); added ones connect.
    #[allow(clippy::too_many_lines)] // mirrors the mcp_servers_change handler; split when extending
    fn reconcile_mcp_servers(&mut self) -> Result<Vec<McpServerReport>, JsHostError> {
        let (registered, cwd, manager) = {
            let core = self.core.lock().expect("host core mutex poisoned");
            let registered: Vec<(String, Json)> = core
                .mcp
                .list()
                .iter()
                .map(|server| (server.name.clone(), server.config.clone()))
                .collect();
            (registered, core.cwd.clone(), core.mcp_manager.clone())
        };
        // Unregistered servers and re-registered ones with a new config
        // are dropped (`index.ts:1221-1228`).
        let removed: Vec<String> = self
            .mcp_servers
            .iter()
            .filter(|state| {
                !registered
                    .iter()
                    .any(|(name, config)| name == &state.name && config == &state.registered_config)
            })
            .map(|state| state.name.clone())
            .collect();
        for name in &removed {
            let Some(state) = self.mcp_servers.iter().find(|state| &state.name == name) else {
                continue;
            };
            // Tools cannot be unregistered, so withdrawn tools are
            // re-registered as hidden (`index.ts:447-450, 455-462`).
            let offered = state.tools.clone();
            if !offered.is_empty() {
                self.hide_server_tools(&offered)?;
            }
            manager.drop_server(name);
        }
        self.mcp_servers
            .retain(|state| !removed.contains(&state.name));

        let mut reports = Vec::new();
        for (name, config) in &registered {
            if self.mcp_servers.iter().any(|state| &state.name == name) {
                continue;
            }
            let enabled = config
                .get("enabled")
                .and_then(Json::as_bool)
                .unwrap_or(true);
            if !enabled {
                self.mcp_servers.push(McpServerState {
                    name: name.clone(),
                    registered_config: config.clone(),
                    connected: false,
                    tools: Vec::new(),
                    error: None,
                });
                reports.push(McpServerReport {
                    name: name.clone(),
                    state: "disabled".to_string(),
                    tool_count: 0,
                    error: None,
                });
                continue;
            }
            match manager.connect(name, config, &cwd) {
                Ok(outcome) => {
                    self.ensure_mcp_extension_record()?;
                    let tools = self.register_server_tools(
                        name,
                        config,
                        &outcome.tools,
                        outcome.instructions.as_deref(),
                        &[],
                    )?;
                    self.mcp_servers.push(McpServerState {
                        name: name.clone(),
                        registered_config: config.clone(),
                        connected: true,
                        tools: tools.clone(),
                        error: None,
                    });
                    reports.push(McpServerReport {
                        name: name.clone(),
                        state: "connected".to_string(),
                        tool_count: tools.len(),
                        error: None,
                    });
                }
                Err(error) => {
                    self.mcp_servers.push(McpServerState {
                        name: name.clone(),
                        registered_config: config.clone(),
                        connected: false,
                        tools: Vec::new(),
                        error: Some(error.clone()),
                    });
                    reports.push(McpServerReport {
                        name: name.clone(),
                        state: "failed".to_string(),
                        tool_count: 0,
                        error: Some(error),
                    });
                }
            }
        }
        Ok(reports)
    }

    /// Creates the synthetic record the bridge tools register into, once.
    fn ensure_mcp_extension_record(&mut self) -> Result<(), JsHostError> {
        self.host.ensure_extension_record(MCP_EXTENSION_PATH)?;
        self.refresh_snapshots()
    }

    /// Registers one connected server's tools into the bridge record and
    /// re-registers withdrawn ones as hidden (`registerTools`,
    /// `index.ts:385-452`). Returns the pi tool names now offered.
    #[allow(clippy::too_many_lines)] // mirrors registerTools; split when extending
    fn register_server_tools(
        &mut self,
        server: &str,
        config: &Json,
        tools: &[Json],
        instructions: Option<&str>,
        previous: &[String],
    ) -> Result<Vec<String>, JsHostError> {
        let description = config
            .get("description")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(ToOwned::to_owned);
        let namespace = {
            let mut namespace = json!({
                "name": mcp_namespace(server),
                // `describeNamespace("docs")` finds `mcp__docs`.
                "aliases": [server],
            });
            if let Some(description) = &description {
                namespace["description"] = json!(description.as_str());
            }
            if let Some(instructions) = instructions.filter(|text| !text.is_empty()) {
                namespace["instructions"] = json!(instructions);
            }
            namespace
        };
        // Like Codex, all tools whose names sanitize to the same name get
        // the hash suffix, so which one keeps the plain name does not
        // depend on the list order (`index.ts:399-403`).
        let plain: Vec<String> = {
            let mut seen: Vec<&str> = Vec::new();
            tools
                .iter()
                .filter_map(|tool| tool.get("name").and_then(Json::as_str))
                .filter(|name| {
                    if seen.contains(name) {
                        false
                    } else {
                        seen.push(name);
                        true
                    }
                })
                .map(|name| create_mcp_tool_name(server, name, |_| false))
                .collect()
        };
        let owners = self.mcp_tool_owners.clone();
        let mut current: Vec<String> = Vec::new();
        let mut definitions: Vec<Json> = Vec::new();
        for tool in tools {
            let Some(tool_name) = tool.get("name").and_then(Json::as_str) else {
                continue;
            };
            let owner = format!("{server}\0{tool_name}");
            let name = create_mcp_tool_name(server, tool_name, |candidate| {
                owners
                    .get(candidate)
                    .is_some_and(|existing| *existing != owner)
                    || current.iter().any(|existing| existing == candidate)
                    || plain.iter().filter(|plain| *plain == candidate).count() > 1
            });
            current.push(name.clone());
            self.mcp_tool_owners.insert(name.clone(), owner);
            // The description fallback chain (`tools.ts:267`): the tool's
            // trimmed description, its title, then the generic sentence.
            let tool_description = tool
                .get("description")
                .and_then(Json::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(ToOwned::to_owned)
                .or_else(|| {
                    tool.get("title")
                        .and_then(Json::as_str)
                        .map(ToOwned::to_owned)
                })
                .unwrap_or_else(|| format!("MCP tool {tool_name} from server {server}"));
            // Exposure: the server's `toolExposure`/`exposure` resolution,
            // then the tool-exposure mapping (`tools.ts:46-48`,
            // `mcp-servers.ts:170-178`).
            let exposure = to_tool_exposure(&get_mcp_tool_exposure(config, tool_name));
            definitions.push(json!({
                "name": name,
                "label": format!("{server}/{tool_name}"),
                "description": tool_description,
                "parameters": pi_mcp::normalize_mcp_input_schema(
                    &tool.get("inputSchema").cloned().unwrap_or(Json::Null),
                ),
                "exposure": exposure,
                "namespace": namespace.clone(),
                "server": server,
                "serverTool": tool_name,
            }));
        }
        self.host
            .register_mcp_tools(MCP_EXTENSION_PATH, &json!(definitions))?;
        // Keep the last definition per name for withdraw hiding
        // (`index.ts:377`).
        for definition in &definitions {
            let name = definition["name"].as_str().expect("name").to_string();
            self.mcp_definitions.insert(name, definition.clone());
        }
        // Tools the server dropped are re-registered as hidden (`index.ts:447-450`).
        let withdrawn: Vec<Json> = previous
            .iter()
            .filter(|name| !current.contains(name))
            .filter_map(|name| self.mcp_definitions.get(name))
            .map(|definition| {
                let mut hidden = definition.clone();
                hidden["exposure"] = json!("hidden");
                hidden
            })
            .collect();
        if !withdrawn.is_empty() {
            self.host
                .register_mcp_tools(MCP_EXTENSION_PATH, &json!(withdrawn))?;
        }
        // Direct tools are declared and activated on registration
        // (`index.ts:770-772`, `agent-session.ts:3766-3769`).
        let activated: Vec<String> = definitions
            .iter()
            .filter(|definition| definition["exposure"] == "direct")
            .filter_map(|definition| definition["name"].as_str().map(ToOwned::to_owned))
            .collect();
        self.sync_tool_state(&activated)?;
        Ok(current)
    }

    /// Re-registers a server's tools as hidden (`hideTools`,
    /// `index.ts:455-462`).
    fn hide_server_tools(&mut self, offered: &[String]) -> Result<(), JsHostError> {
        let hidden: Vec<Json> = offered
            .iter()
            .filter_map(|name| self.mcp_definitions.get(name))
            .map(|definition| {
                let mut hidden = definition.clone();
                hidden["exposure"] = json!("hidden");
                hidden
            })
            .collect();
        if hidden.is_empty() {
            return Ok(());
        }
        self.host
            .register_mcp_tools(MCP_EXTENSION_PATH, &json!(hidden))?;
        self.sync_tool_state(&[])
    }

    /// Emits `mcp_servers_change` for registry changes made after bind and
    /// reconciles the connections (the change listener set at bindCore,
    /// `runner.ts:465-466`, plus the handler's connect half,
    /// `index.ts:1215-1248`). Drained at the end of every dispatch
    /// session, standing in for upstream's async emit; the guard keeps a
    /// change made by a handler of the event itself from recursing.
    fn drain_mcp_changes(&mut self) -> Result<(), JsHostError> {
        if self.mcp_draining {
            return Ok(());
        }
        let changes: Vec<String> = {
            let mut core = self.core.lock().expect("host core mutex poisoned");
            std::mem::take(&mut core.mcp_changes)
        };
        if changes.is_empty() {
            return Ok(());
        }
        self.mcp_draining = true;
        let result = self.emit_mcp_changes_then_reconcile(&changes);
        self.mcp_draining = false;
        result.map(|_| ())
    }

    fn emit_mcp_changes_then_reconcile(
        &mut self,
        changes: &[String],
    ) -> Result<Vec<McpServerReport>, JsHostError> {
        for _change in changes {
            // One emit per change, each carrying the full registry list
            // (`runner.ts:465-466`). Handler errors report through the
            // listeners; an emission failure must not fail the dispatch
            // that triggered the change (upstream's emit is void).
            let servers: Vec<Json> = {
                let core = self.core.lock().expect("host core mutex poisoned");
                core.mcp
                    .list()
                    .iter()
                    .map(|server| {
                        json!({
                            "name": server.name,
                            "extensionPath": server.extension_path,
                            "config": server.config,
                        })
                    })
                    .collect()
            };
            let _ = self.emit(&json!({ "type": "mcp_servers_change", "servers": servers }));
        }
        self.reconcile_mcp_servers()
    }

    /// Ends a dispatch session and drains queued MCP registry changes.
    fn end_session(&mut self, id: u32) -> Result<(), JsHostError> {
        self.host.session_end(id)?;
        self.drain_mcp_changes()
    }

    /// Closes every MCP connection and clears the runner state
    /// (`session_shutdown`, `index.ts:1250-1256` — the registered-server
    /// half; a later reconcile reconnects).
    pub fn shutdown_mcp(&mut self) {
        let manager = {
            let core = self.core.lock().expect("host core mutex poisoned");
            core.mcp_manager.clone()
        };
        manager.shutdown_all();
        self.mcp_servers.clear();
    }

    /// The current per-server status (`mcp_server_reports`).
    pub fn mcp_server_reports(&self) -> Vec<McpServerReport> {
        self.mcp_servers
            .iter()
            .map(|state| McpServerReport {
                name: state.name.clone(),
                state: if state.connected {
                    "connected".to_string()
                } else if state.error.is_some() {
                    "failed".to_string()
                } else {
                    "disabled".to_string()
                },
                tool_count: state.tools.len(),
                error: state.error.clone(),
            })
            .collect()
    }

    /// Re-reads the JS-side registries into the runner's snapshots.
    pub fn refresh_snapshots(&mut self) -> Result<(), JsHostError> {
        let raw = self.host.inspect()?;
        self.extensions =
            serde_json::from_str(&raw).map_err(|err| JsHostError::Job(err.to_string()))?;
        Ok(())
    }

    /// The first-wins tool list across extensions in load order.
    pub fn all_registered_tools(&self) -> Vec<&ToolInfo> {
        let mut seen: Vec<&str> = Vec::new();
        let mut tools = Vec::new();
        for ext in &self.extensions {
            for tool in &ext.tools {
                if seen.contains(&tool.name.as_str()) {
                    continue;
                }
                seen.push(&tool.name);
                tools.push(tool);
            }
        }
        tools
    }

    /// One tool's definition by name (first registration wins).
    pub fn tool(&self, name: &str) -> Option<&ToolInfo> {
        self.all_registered_tools()
            .into_iter()
            .find(|tool| tool.name == name)
    }

    /// The first-wins flag map (`getFlags`).
    pub fn flags(&self) -> Vec<FlagSpec> {
        let mut seen: Vec<&str> = Vec::new();
        let mut flags = Vec::new();
        for ext in &self.extensions {
            for flag in &ext.flags {
                if seen.contains(&flag.name.as_str()) {
                    continue;
                }
                seen.push(&flag.name);
                flags.push(flag.clone());
            }
        }
        flags
    }

    /// The current active tool names (`getActiveTools` state).
    pub fn active_tools(&self) -> Vec<String> {
        self.core
            .lock()
            .expect("host core mutex poisoned")
            .active_tools
            .clone()
    }

    /// Replaces the active tool set with the same validation the
    /// `pi.setActiveTools` op applies (unknown and `hidden` names drop).
    pub fn set_active_tools(&self, names: Vec<String>) {
        let mut core = self.core.lock().expect("host core mutex poisoned");
        core.active_tools = names
            .into_iter()
            .filter(|name| core.registered_tool_names.contains(name))
            .filter(|name| !core.hidden_tool_names.contains(name))
            .collect();
    }

    /// Sets a CLI flag value, overriding registration defaults.
    pub fn set_flag_value(&self, name: &str, value: Json) {
        self.core
            .lock()
            .expect("host core mutex poisoned")
            .flag_values
            .insert(name.to_string(), value);
    }

    /// Resolves the command table with `name:occurrence` disambiguation
    /// (the `resolveRegisteredCommands` port, collision loop included).
    pub fn resolved_commands(&self) -> Vec<ResolvedCommand> {
        let mut commands: Vec<(&str, &str, &str)> = Vec::new();
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for ext in &self.extensions {
            for command in &ext.commands {
                commands.push((&command.name, &command.description, &ext.path));
                *counts.entry(&command.name).or_insert(0) += 1;
            }
        }
        let mut seen: HashMap<&str, usize> = HashMap::new();
        let mut taken: Vec<String> = Vec::new();
        let mut resolved = Vec::with_capacity(commands.len());
        for (name, description, extension_path) in commands {
            let occurrence = seen.entry(name).or_insert(0);
            *occurrence += 1;
            let mut invocation = if counts[name] > 1 {
                format!("{name}:{occurrence}")
            } else {
                name.to_string()
            };
            if taken.contains(&invocation) {
                let mut suffix = *occurrence;
                loop {
                    suffix += 1;
                    invocation = format!("{name}:{suffix}");
                    if !taken.contains(&invocation) {
                        break;
                    }
                }
            }
            taken.push(invocation.clone());
            resolved.push(ResolvedCommand {
                name: name.to_string(),
                description: description.to_string(),
                invocation_name: invocation,
                extension_path: extension_path.to_string(),
            });
        }
        resolved
    }

    /// One resolved command by invocation name.
    pub fn command(&self, invocation_name: &str) -> Option<ResolvedCommand> {
        self.resolved_commands()
            .into_iter()
            .find(|command| command.invocation_name == invocation_name)
    }

    /// Executes a command handler (`args` is the raw argument text; the
    /// handler receives `(args, ctx)`). Returns `Ok(false)` when no
    /// command has that invocation name.
    pub fn run_command(&mut self, invocation_name: &str, args: &str) -> Result<bool, JsHostError> {
        let Some(command) = self.command(invocation_name) else {
            return Ok(false);
        };
        let index = self
            .extensions
            .iter()
            .position(|ext| ext.path == command.extension_path)
            .ok_or_else(|| {
                JsHostError::Job(format!("extension `{}` vanished", command.extension_path))
            })?;
        let id = self.host.session_begin("{}", ContextKind::Command, None)?;
        self.host
            .session_call_command(id, index, &command.name, args)?;
        self.settle_session(id)?;
        let outcome = parse_session_outcome(&self.host.session_last(id)?)?;
        self.end_session(id)?;
        Self::outcome_result(outcome)?;
        Ok(true)
    }

    /// Resolves the extension shortcuts against the resolved keybindings
    /// (`getShortcuts`, `runner.ts:680-712`): every chord lowercases, a
    /// reserved built-in blocks the shortcut (skip + warning), a
    /// non-reserved built-in warns and the extension wins, a later
    /// extension overrides an earlier one per chord with a warning. The
    /// warnings stay readable through [`Self::shortcut_diagnostics`] until
    /// the next call.
    pub fn get_shortcuts(&mut self, resolved_keybindings: &[(&str, &[&str])]) {
        self.shortcut_diagnostics.clear();
        let builtins = build_builtin_keybindings(resolved_keybindings);
        let mut resolved: Vec<ResolvedShortcut> = Vec::new();
        for index in 0..self.extensions.len() {
            let ext = &self.extensions[index];
            for spec in &ext.shortcuts {
                let key = spec.shortcut.to_lowercase();
                match builtins.get(&key) {
                    Some((_, true)) => {
                        self.shortcut_diagnostics.push(ShortcutDiagnostic {
                            message: format!(
                                "Extension shortcut '{}' from {} conflicts with built-in shortcut. Skipping.",
                                spec.shortcut, ext.path
                            ),
                            path: ext.path.clone(),
                        });
                        continue;
                    }
                    Some((action, false)) => {
                        self.shortcut_diagnostics.push(ShortcutDiagnostic {
                            message: format!(
                                "Extension shortcut conflict: '{}' is built-in shortcut for {} and {}. Using {}.",
                                spec.shortcut, action, ext.path, ext.path
                            ),
                            path: ext.path.clone(),
                        });
                    }
                    None => {}
                }
                let shortcut = ResolvedShortcut {
                    key: key.clone(),
                    raw_key: spec.shortcut.clone(),
                    ext_index: index,
                    extension_path: ext.path.clone(),
                    description: spec.description.clone(),
                };
                if let Some(existing) = resolved.iter().position(|s| s.key == key) {
                    self.shortcut_diagnostics.push(ShortcutDiagnostic {
                        message: format!(
                            "Extension shortcut conflict: '{}' registered by both {} and {}. Using {}.",
                            spec.shortcut, resolved[existing].extension_path, ext.path, ext.path
                        ),
                        path: ext.path.clone(),
                    });
                    resolved[existing] = shortcut;
                } else {
                    resolved.push(shortcut);
                }
            }
        }
        self.resolved_shortcuts = resolved;
    }

    /// The shortcuts the last [`Self::get_shortcuts`] resolved, in load
    /// order with per-chord last-wins.
    pub fn shortcuts(&self) -> &[ResolvedShortcut] {
        &self.resolved_shortcuts
    }

    /// The warnings the last [`Self::get_shortcuts`] produced
    /// (`getShortcutDiagnostics`, `runner.ts:396/:680`).
    pub fn shortcut_diagnostics(&self) -> &[ShortcutDiagnostic] {
        &self.shortcut_diagnostics
    }

    /// The extension markdown transformers in load order
    /// (`getMarkdownTransformers`, `runner.ts:793-803`). The surface
    /// filter of upstream's wrap runs at application time
    /// ([`ResolvedMarkdownTransformer::message_types`]).
    pub fn get_markdown_transformers(&self) -> Vec<ResolvedMarkdownTransformer> {
        self.extensions
            .iter()
            .enumerate()
            .filter_map(|(index, extension)| {
                extension
                    .markdown_transformer
                    .as_ref()
                    .map(|spec| ResolvedMarkdownTransformer {
                        ext_index: index,
                        extension_path: extension.path.clone(),
                        message_types: spec.message_types.clone(),
                    })
            })
            .collect()
    }

    /// Runs one extension's markdown transformer synchronously
    /// (`markdown-transform.ts:28-38`; the engine maps throws and
    /// non-string returns to `None` — keep the current markdown).
    /// Extension `ext_index` comes from [`Self::get_markdown_transformers`].
    pub fn markdown_transform(
        &self,
        ext_index: usize,
        markdown: &str,
        context_json: &str,
    ) -> Option<String> {
        self.host
            .markdown_transform(ext_index, markdown, context_json)
    }

    /// The FIRST extension whose renderer handles this custom entry type
    /// (`getEntryRenderer`, `runner.ts:815-823` — load order wins).
    pub fn get_entry_renderer(&self, custom_type: &str) -> Option<usize> {
        self.extensions.iter().position(|extension| {
            extension
                .entry_renderers
                .iter()
                .any(|registered| registered == custom_type)
        })
    }

    /// Calls the entry renderer of extension `ext_index` for one custom
    /// entry (`addCustomEntryToChat`, `interactive-mode.ts:3986-4007`):
    /// the engine adopts the returned component and reports the outcome.
    /// `entry_json` is the raw record; `options_json` carries
    /// `{expanded}` (the keybinding hints cross through the JS shim).
    pub fn render_entry(
        &self,
        ext_index: usize,
        custom_type: &str,
        entry_json: &str,
        options_json: &str,
    ) -> Result<EntryRenderOutcome, JsHostError> {
        self.host
            .render_entry(ext_index, custom_type, entry_json, options_json)
    }

    /// The FIRST extension whose renderer handles this custom message type
    /// (`getMessageRenderer`, `runner.ts:782-790` — load order wins; sole
    /// caller `addMessageToChat` case "custom", `interactive-mode.ts:4034`).
    pub fn get_message_renderer(&self, custom_type: &str) -> Option<usize> {
        self.extensions.iter().position(|extension| {
            extension
                .message_renderers
                .iter()
                .any(|registered| registered == custom_type)
        })
    }

    /// Calls the message renderer of extension `ext_index` for one custom
    /// message (`addMessageToChat` case "custom",
    /// `interactive-mode.ts:4032-4048`): the engine adopts the returned
    /// component and reports the outcome. `message_json` is the record with
    /// its `role:"custom"`; `options_json` carries `{expanded, outputPad}`
    /// (the keybinding hints cross through the JS shim). A renderer throw
    /// surfaces as [`EntryRenderOutcome::Failed`] and the host falls through
    /// to the default rendering (`custom-message.ts:93-95`).
    pub fn render_message(
        &self,
        ext_index: usize,
        custom_type: &str,
        message_json: &str,
        options_json: &str,
    ) -> Result<EntryRenderOutcome, JsHostError> {
        self.host
            .render_message(ext_index, custom_type, message_json, options_json)
    }

    /// Runs the tool-renderer resolver chain for one tool name
    /// (`resolveToolRenderers`, `runner.ts:798-806` — resolvers in load
    /// order, each receiving `next()`; the base mirrors
    /// `withBuiltInRenderers` — `has_builtin` decides whether it is the
    /// built-in marker object or undefined for unknown tools). The winning
    /// object is stashed per row; release it with
    /// [`Self::tool_renderer_release`] when the row goes away.
    pub fn resolve_tool_renderers(
        &self,
        tool_name: &str,
        has_builtin: bool,
    ) -> Result<ToolRenderersOutcome, JsHostError> {
        self.host.resolve_tool_renderers(tool_name, has_builtin)
    }

    /// Calls the stashed renderers' `renderCall`
    /// (`tool-execution.ts:305`): the engine adopts the returned component
    /// and reports the outcome. `args_json` is the row's current arguments;
    /// `context_json` carries the `ToolRenderContext` flags. A renderer
    /// throw surfaces as [`EntryRenderOutcome::Failed`] and the row falls
    /// back to the generic call line (`tool-execution.ts:309-313`).
    pub fn tool_render_call(
        &self,
        stash_id: u32,
        args_json: &str,
        context_json: &str,
    ) -> Result<EntryRenderOutcome, JsHostError> {
        self.host
            .tool_render_call(stash_id, args_json, context_json)
    }

    /// Calls the stashed renderers' `renderResult`
    /// (`tool-execution.ts:326-331`): the result record and the
    /// `{expanded, isPartial}` options cross beside the context flags. A
    /// throw falls back to the sliced-output text
    /// (`tool-execution.ts:335-341`).
    pub fn tool_render_result(
        &self,
        stash_id: u32,
        args_json: &str,
        result_json: &str,
        options_json: &str,
        context_json: &str,
    ) -> Result<EntryRenderOutcome, JsHostError> {
        self.host
            .tool_render_result(stash_id, args_json, result_json, options_json, context_json)
    }

    /// Drops one row's stashed renderers (the row's Rust side calls this
    /// from `Drop` so the JS registry does not leak).
    pub fn tool_renderer_release(&self, stash_id: u32) -> Result<(), JsHostError> {
        self.host.tool_renderer_release(stash_id)
    }

    /// Disposes one adopted component (`disposeComponent` — `dispose?.()`
    /// swallowed, `interactive-mode.ts:3104-3109`) and drops the registry
    /// entry. The custom dialog's close path calls this once.
    pub fn component_dispose(&self, handle: u32) -> Result<(), JsHostError> {
        self.host.component_dispose(handle)
    }

    /// Renders one adopted component (`custom-entry.ts` embeds it as a
    /// child; the handle comes from [`Self::render_entry`]).
    pub fn component_render(&self, handle: u32, width: usize) -> Result<Vec<String>, JsHostError> {
        self.host.component_render(handle, width)
    }

    /// Feeds one key chunk to an adopted component; `true` = consumed.
    pub fn component_handle_input(&self, handle: u32, data: &str) -> Result<bool, JsHostError> {
        self.host.component_handle_input(handle, data)
    }

    /// Invalidates one adopted component (the theme-change hook).
    pub fn component_invalidate(&self, handle: u32) -> Result<(), JsHostError> {
        self.host.component_invalidate(handle)
    }

    /// Drops one adopted component from the JS registry.
    pub fn component_release(&self, handle: u32) -> Result<(), JsHostError> {
        self.host.component_release(handle)
    }

    /// Calls one adopted component's duck-typed `setExpanded` (the custom
    /// header/footer swap and the tools toggle, E3 slice 7): a no-op when
    /// the component lacks the method.
    pub fn component_set_expanded(&self, handle: u32, expanded: bool) -> Result<(), JsHostError> {
        self.host.component_set_expanded(handle, expanded)
    }

    /// Executes a registered shortcut handler (`interactive-mode.ts:2425-2427`
    /// invokes `handler(ctx)`; the port dispatches the stashed handler
    /// through the base-ctx session machinery — the same shape the command
    /// dispatch uses, minus the command-ctx additions). Returns `Ok(false)`
    /// when no shortcut resolved to `key`.
    pub fn run_shortcut(&mut self, key: &str) -> Result<bool, JsHostError> {
        let Some(resolved) = self
            .resolved_shortcuts
            .iter()
            .find(|shortcut| shortcut.key == key)
        else {
            return Ok(false);
        };
        let (index, raw_key) = (resolved.ext_index, resolved.raw_key.clone());
        let id = self.host.session_begin("{}", ContextKind::Event, None)?;
        self.host.session_call_shortcut(id, index, &raw_key)?;
        self.settle_session(id)?;
        let outcome = parse_session_outcome(&self.host.session_last(id)?)?;
        self.end_session(id)?;
        Self::outcome_result(outcome)?;
        Ok(true)
    }

    /// Executes a registered tool: first-wins lookup, the session
    /// machinery calls `execute(toolCallId, params, signal, onUpdate,
    /// ctx)`. The returned JSON is the `AgentToolResult` the host records.
    pub fn execute_tool(
        &mut self,
        name: &str,
        tool_call_id: &str,
        args: &Json,
    ) -> Result<Json, ToolExecutionError> {
        let Some(tool) = self.tool(name) else {
            return Err(ToolExecutionError {
                name: name.to_string(),
                message: "no such tool".to_string(),
            });
        };
        if !tool.has_execute {
            return Err(ToolExecutionError {
                name: name.to_string(),
                message: "tool has no execute".to_string(),
            });
        }
        let index = self
            .extensions
            .iter()
            .position(|ext| ext.tools.iter().any(|candidate| candidate.name == name))
            .expect("first-wins tool has an extension");
        let id = self
            .host
            .session_begin(
                &serde_json::to_string(args).unwrap_or_default(),
                ContextKind::Tool,
                Some(tool_call_id),
            )
            .map_err(|err| ToolExecutionError {
                name: name.to_string(),
                message: err.to_string(),
            })?;
        let call = self.host.session_call_tool(id, index, name, tool_call_id);
        let driven = call.and_then(|()| self.settle_session(id));
        let outcome = match driven.and_then(|()| self.host.session_last(id)) {
            Ok(raw) => parse_session_outcome(&raw),
            Err(err) => {
                let _ = self.end_session(id);
                return Err(ToolExecutionError {
                    name: name.to_string(),
                    message: err.to_string(),
                });
            }
        };
        let _ = self.end_session(id);
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(err) => {
                return Err(ToolExecutionError {
                    name: name.to_string(),
                    message: err.to_string(),
                });
            }
        };
        let value = Self::outcome_result(outcome).map_err(|err| ToolExecutionError {
            name: name.to_string(),
            message: err.to_string(),
        })?;
        let Some(value) = value else {
            return Err(ToolExecutionError {
                name: name.to_string(),
                message: "tool returned no result".to_string(),
            });
        };
        Ok(value)
    }

    fn outcome_result(outcome: SessionOutcome) -> Result<Option<Json>, JsHostError> {
        if !outcome.done {
            return Err(JsHostError::HandlerDidNotComplete(
                "the handler never settled while draining the job queue".to_string(),
            ));
        }
        match outcome.error {
            Some(error) => Err(JsHostError::JsException(error.message)),
            None => Ok(outcome.value),
        }
    }

    /// The `(extension index, handler index)` dispatch order for one
    /// event: extension load order, then registration order (the
    /// `snapshotEventHandlers` shape).
    fn handlers_for(&self, event: &str) -> Vec<(usize, usize)> {
        let mut handlers = Vec::new();
        for (index, ext) in self.extensions.iter().enumerate() {
            if let Some(list) = ext.events.get(event) {
                for handler in list {
                    handlers.push((index, *handler));
                }
            }
        }
        handlers
    }

    fn report_error(
        &self,
        extension_index: usize,
        event: &str,
        error: crate::events::HandlerErrorJson,
    ) {
        let extension_path = self
            .extensions
            .get(extension_index)
            .map(|ext| ext.path.clone())
            .unwrap_or_default();
        let extension_error = ExtensionError {
            extension_path,
            event: event.to_string(),
            error: error.message,
            stack: error.stack,
        };
        for listener in &self.error_listeners {
            listener(&extension_error);
        }
    }

    /// Crosses the resolved keybindings table (id → chords) the extension
    /// components' `keyText`/`keyHint` read (`getKeybindings()` upstream —
    /// the port has no `keybindings.json` overrides, so this is the default
    /// table, serialized by the embedder).
    pub fn set_app_keybindings(&mut self, keybindings_json: &str) {
        let value: Json = serde_json::from_str(keybindings_json).unwrap_or(Json::Null);
        self.core
            .lock()
            .expect("host core mutex poisoned")
            .app_keybindings = value;
    }

    /// Installs the extension components' theme mapping — the synchronous
    /// `theme.fg`/`theme.bg` inside renderer calls (`theme.ts:297/:303`).
    /// The closures read the port's live theme per call, so theme changes
    /// need no rebind. Without this the identity default renders plain
    /// text (the headless matrix). The interactive mode calls this even
    /// though its dialog surface (`set_ui_context`) lands with the
    /// ui-custom slice — entry renderers style but never prompt.
    pub fn set_theme_surface(&mut self, fg: ThemeSurfaceFn, bg: ThemeSurfaceFn) {
        let mut core = self.core.lock().expect("host core mutex poisoned");
        core.actions.theme_fg = Box::new(move |token, text| fg(token, text));
        core.actions.theme_bg = Box::new(move |token, text| bg(token, text));
    }

    /// Installs the custom-component factory's keybinding matcher (E3
    /// slice 6b) — the port's `pi_tui::keybindings::matches` over the
    /// live app defaults, so a factory's `keybindings.matches(data,
    /// action)` behaves like the mode's.
    pub fn set_keybinding_matcher(&mut self, matcher: fn(&str, &str) -> bool) {
        let mut core = self.core.lock().expect("host core mutex poisoned");
        core.actions.keybinding_match = Box::new(matcher);
    }

    /// Installs the custom-component factory's theme attribute styler (E3
    /// slice 6b) — `theme.bold/italic/underline/inverse/strikethrough`
    /// wrap with the live theme's ANSI attributes, next to the fg/bg
    /// surface [`Self::set_theme_surface`] installs.
    pub fn set_style_surface(&mut self, style: ThemeSurfaceFn) {
        let mut core = self.core.lock().expect("host core mutex poisoned");
        core.actions.theme_style = Box::new(move |method, text| style(method, text));
    }

    /// Installs the mode-state snapshot the synchronous ui reads serve (E3
    /// slice 7): the `footerData`/`editorText`/`toolsExpanded` ops call
    /// these closures, so `ctx.ui.getEditorText()`/`getToolsExpanded()` and
    /// the custom footer's data adapter read the mode's live state without
    /// a blocking round-trip. The mode refreshes the shared snapshots each
    /// pump frame; without this the defaults read null/""/false (the
    /// headless matrix).
    pub fn set_ui_snapshot_surface(&mut self, snapshot: UiSnapshotSurface) {
        let mut core = self.core.lock().expect("host core mutex poisoned");
        core.actions.ui_snapshot.footer_data = Box::new(move || (snapshot.footer_data)());
        core.actions.ui_snapshot.editor_text = Box::new(move || (snapshot.editor_text)());
        core.actions.ui_snapshot.tools_expanded = Box::new(move || (snapshot.tools_expanded)());
    }

    /// Folds one terminal input chunk through the extension-registered
    /// listeners (`__piTerminalInputFold`; the `inputListeners` chain,
    /// `tui.ts:1095-1108`). `None` means the chunk is dropped. The mode
    /// calls this from its input path — with a `try_lock` on the runner
    /// mutex, deferring instead of blocking: an engine busy settling a
    /// dialog holds the mutex, and blocking the input thread here would
    /// freeze the pump that must service that dialog (the 6b deadlock
    /// class).
    pub fn deliver_terminal_input(&self, data: &str) -> Result<Option<String>, JsHostError> {
        Ok(self.host.terminal_input_fold(data))
    }

    /// Clears the extension-registered terminal input listeners
    /// (`resetExtensionUI`'s `clearExtensionTerminalInputListeners`,
    /// `interactive-mode.ts:2750-2753`).
    pub fn clear_terminal_input_handlers(&self) -> Result<(), JsHostError> {
        self.host.clear_terminal_input_handlers()
    }

    /// Fires the custom footer's `onBranchChange` callbacks — the mode
    /// calls it when the git branch it shows flips. Fire-and-forget.
    pub fn notify_footer_branch_change(&self, branch: &str) -> Result<(), JsHostError> {
        self.host.notify_footer_branch_change(branch)
    }

    /// Binds the UI surface (`setUIContext`, `runner.ts:570-575`):
    /// `ctx.mode`/`ctx.hasUI` flip, and every `ctx.ui` request crosses to
    /// the sink. Without this call the runner stays headless.
    pub fn set_ui_context(&mut self, sink: std::sync::Arc<dyn UiSink>, mode: &str) {
        self.host.set_ui_mode(mode, true);
        self.ui_sink = Some(sink);
    }

    /// Unbinds the UI surface (`setUIContext(undefined)`): back to the
    /// headless matrix and `mode: "print"`.
    pub fn clear_ui_context(&mut self) {
        self.host.set_ui_mode("print", false);
        self.ui_sink = None;
    }

    /// Drives jobs, then services UI requests until the session settles
    /// or nothing is pending. Replaces the bare job drain inside every
    /// dispatch loop: a handler awaiting a dialog promise only finishes
    /// once the sink's response resolves it. Draining precedes the
    /// settled check so fire-and-forget requests queued during the last
    /// job batch still cross now (upstream's void `ctx.ui` calls are
    /// synchronous).
    fn settle_session(&mut self, id: u32) -> Result<(), JsHostError> {
        self.host.drive_jobs()?;
        loop {
            let requests = self.host.drain_ui_requests();
            if !requests.is_empty() {
                for request in &requests {
                    self.deliver_ui_request(request)?;
                    self.host.drive_jobs()?;
                }
                continue;
            }
            if self.session_settled(id)? {
                return Ok(());
            }
            // A promise only jobs can settle; leave the slot pending
            // (the caller reads `{done:false}`).
            return Ok(());
        }
    }

    /// Whether the session's result slot has settled.
    fn session_settled(&self, id: u32) -> Result<bool, JsHostError> {
        let raw = self.host.session_last(id)?;
        let value: Json =
            serde_json::from_str(&raw).map_err(|err| JsHostError::Job(err.to_string()))?;
        Ok(value.get("done").and_then(Json::as_bool).unwrap_or(true))
    }

    /// Drains the engine's `ui_requests` OUTSIDE a dispatch (E3 slice 6b) —
    /// the interactive mode's pump calls this every frame. Fire-and-forget
    /// requests (`customDone`, `requestRender`, the overlay writes) forward
    /// to the sink immediately; a `customDone` also resolves its custom
    /// deferred (the result `done()` queued on the crossing) and ends the
    /// active custom prompt — resolution precedes the pair's end, so
    /// `ui_prompt_end` precedes the resolved continuation like upstream's
    /// `.finally(finish)` (`runner.ts:585-611`); dialog methods AND the
    /// answer-class overlay crossings (`overlayIsHidden`/`overlayIsFocused`/
    /// `overlayGetBounds`, E3 slice 6c — the mode answers from its overlay
    /// registry like a dialog close) come back to the caller, which delivers
    /// them on threads through
    /// [`Self::begin_ui_dialog`]/[`Self::complete_ui_dialog`] — the wait
    /// must never hold the runner mutex here, or the pump's component
    /// crossings would stall behind it. Anything returned or forwarded
    /// drives jobs afterwards: the custom `done()` continuation is a job.
    pub fn service_ui_requests(&mut self) -> Result<Vec<Json>, JsHostError> {
        let requests = self.host.drain_ui_requests();
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let mut deferred: Vec<Json> = Vec::new();
        let mut ends = 0usize;
        for request in &requests {
            match request.get("method").and_then(Json::as_str) {
                Some("customDone") => {
                    if let Some(sink) = self.ui_sink.clone() {
                        sink.request(request);
                    }
                    // Mailbox symmetry with the dialog methods: the glue
                    // registers the custom deferred under the crossing's id,
                    // and the wrapper glue-side (mirroring the ui methods'
                    // `parseValue`) adds the reaction link that keeps the
                    // handler's continuation behind the prompt-end handlers.
                    let id = request
                        .get("id")
                        .and_then(Json::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let payload = match request.get("result") {
                        Some(result) => {
                            serde_json::to_string(&json!({ "value": result })).unwrap_or_default()
                        }
                        None => String::new(),
                    };
                    self.host.resolve_ui(&id, &payload)?;
                    ends += 1;
                }
                Some("select" | "confirm" | "input" | "editor") => deferred.push(request.clone()),
                // The answer-class overlay crossings ride the same deferred
                // shape: the pump's delivery threads block on the bridge
                // like a dialog close, and the mode answers on its next
                // frame (E3 slice 6c).
                Some("overlayIsHidden" | "overlayIsFocused" | "overlayGetBounds") => {
                    deferred.push(request.clone());
                }
                _ => {
                    if let Some(sink) = self.ui_sink.clone() {
                        sink.request(request);
                    }
                }
            }
        }
        // Each custom's done() pairs with exactly one prompt end — two
        // stacked customs closing in one drain end twice (the depth-gated
        // begins counted them too).
        for _ in 0..ends {
            self.end_ui_prompt()?;
        }
        self.host.drive_jobs()?;
        Ok(deferred)
    }

    /// Delivers one UI request. Dialog methods (`select`, `confirm`,
    /// `input`, `editor`) emit the `ui_prompt_*` pair around the blocking
    /// wait (`wrapUIPromptContext`, `runner.ts:577-613`); `custom` splits
    /// by shape: the interactive arm (a `handle` field — an adopted
    /// component, E3 slice 6b) emits only the pair's start and forwards —
    /// the promise resolves glue-side when the extension calls `done()`,
    /// and the fire-and-forget `customDone` crossing ends the prompt
    /// ([`Self::service_ui_requests`]) — while the RPC arm keeps the
    /// blocking pair (rpc-mode answers immediately). The answer-class
    /// overlay crossings settle with the method default (they cannot reach
    /// the mode from a dispatch — see the arm). Everything else is
    /// fire-and-forget.
    ///
    /// The blocking wait runs under the CALLER's mutex tenure: callers
    /// are [`Self::settle_session`]'s drains (a tool handler awaiting a
    /// dialog must be settled before its `block` decision — the wait is
    /// load-bearing) and the mode thread's own emits. Delivery threads
    /// that must not hold the runner mutex while the dialog is up (the
    /// pump's, E3 slice 6b — component render/input crossings need the
    /// mutex then) use [`Self::begin_ui_dialog`] +
    /// [`Self::complete_ui_dialog`] instead.
    pub fn deliver_ui_request(&mut self, request: &Json) -> Result<(), JsHostError> {
        let method = request
            .get("method")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string();
        let id = request
            .get("id")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string();
        match method.as_str() {
            "custom" if request.get("handle").is_some() => {
                // Interactive `ctx.ui.custom` (E3 slice 6b; the crossing
                // shapes of `showExtensionCustom`, `interactive-mode.ts
                // :3074-3178` — the overlay arm rides the same request with
                // `overlay`/`overlayOptions`/`blocked`, E3 slice 6c): the
                // mode mounts the adopted component (inline in the editor
                // slot, or on top via `tui.ts:792-816`'s showOverlay);
                // done() queues the result on the customDone crossing and
                // [`Self::service_ui_requests`] resolves the deferred. NO
                // blocking wait — the runner mutex must stay free for the
                // component crossings, and nothing ever answers this
                // request through the sink.
                self.begin_ui_prompt("custom", None)?;
                if let Some(sink) = self.ui_sink.clone() {
                    sink.request(request);
                }
            }
            "select" | "confirm" | "input" | "editor" => {
                let title = request
                    .get("title")
                    .and_then(Json::as_str)
                    .map(ToOwned::to_owned);
                self.begin_ui_prompt(&method, title.as_deref())?;
                if let Some(sink) = self.ui_sink.clone() {
                    sink.request(request);
                    let response = sink.wait_response(&id, &method);
                    let payload = serde_json::to_string(&response).unwrap_or_default();
                    self.host.resolve_ui(&id, &payload)?;
                }
                self.end_ui_prompt()?;
            }
            "custom" => {
                self.begin_ui_prompt("custom", None)?;
                self.host.resolve_ui(&id, "")?;
                self.end_ui_prompt()?;
            }
            // The answer-class overlay crossings made INSIDE a dispatch
            // cannot reach the mode: the dispatch thread may be the mode's
            // own pump (a handler awaiting a dialog must be settled before
            // its `block` decision), so waiting here could deadlock the UI.
            // Deviation: such a crossing settles with the method default
            // instead — upstream answers synchronously from the in-process
            // TUI (`tui.ts:845/:883/:884-887`); the port's reads only
            // reflect the registry when they arrive through the pump path.
            "overlayIsHidden" | "overlayIsFocused" | "overlayGetBounds" => {
                let payload = match method.as_str() {
                    "overlayIsFocused" => "{\"focused\":false}",
                    "overlayGetBounds" => "{}",
                    _ => "{\"hidden\":false}",
                };
                self.host.resolve_ui(&id, payload)?;
            }
            _ => {
                if let Some(sink) = &self.ui_sink {
                    sink.request(request);
                }
            }
        }
        Ok(())
    }

    /// Delivers one UI request for a caller that must NOT hold the runner
    /// mutex across the dialog wait (E3 slice 6b — the pump's delivery
    /// threads): dialog methods emit the `ui_prompt_*` pair's start, cross
    /// the request, and return a [`UiDialogWait`]; the caller blocks on it
    /// outside the guard and completes through [`Self::complete_ui_dialog`].
    /// While a dialog is up the mutex stays free for component render/input
    /// crossings, further deliveries, and deadline closes. The answer-class
    /// overlay crossings (E3 slice 6c) return a wait with no `ui_prompt`
    /// pair — they read a value, they do not open a dialog. Non-dialog
    /// requests ride [`Self::deliver_ui_request`] (no wait involved).
    pub fn begin_ui_dialog(&mut self, request: &Json) -> Result<Option<UiDialogWait>, JsHostError> {
        let method = request
            .get("method")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string();
        match method.as_str() {
            "select" | "confirm" | "input" | "editor" => {
                let title = request
                    .get("title")
                    .and_then(Json::as_str)
                    .map(ToOwned::to_owned);
                let id = request
                    .get("id")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.begin_ui_prompt(&method, title.as_deref())?;
                if let Some(sink) = self.ui_sink.clone() {
                    sink.request(request);
                    return Ok(Some(UiDialogWait {
                        sink,
                        id,
                        method,
                        prompt: true,
                    }));
                }
                // No sink: nothing can ever answer — mirror
                // [`Self::deliver_ui_request`]'s no-sink arm and close
                // the prompt pair instead of leaking it.
                self.end_ui_prompt()?;
                Ok(None)
            }
            "overlayIsHidden" | "overlayIsFocused" | "overlayGetBounds" => {
                let id = request
                    .get("id")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_string();
                if let Some(sink) = self.ui_sink.clone() {
                    sink.request(request);
                    return Ok(Some(UiDialogWait {
                        sink,
                        id,
                        method,
                        prompt: false,
                    }));
                }
                // No sink: no registry could ever answer (a handle only
                // exists after a showOverlay crossing, which needs one).
                Ok(None)
            }
            _ => {
                self.deliver_ui_request(request)?;
                Ok(None)
            }
        }
    }

    /// Completes a dialog delivered through [`Self::begin_ui_dialog`]:
    /// resolves the glue deferred, ends the `ui_prompt` pair, drives jobs
    /// (the resolved handler's continuation). Answer-class overlay crossings
    /// opened no pair, so they skip the end.
    pub fn complete_ui_dialog(
        &mut self,
        wait: &UiDialogWait,
        response: &Json,
    ) -> Result<(), JsHostError> {
        let payload = serde_json::to_string(response).unwrap_or_default();
        self.host.resolve_ui(&wait.id, &payload)?;
        if wait.prompt {
            self.end_ui_prompt()?;
        }
        self.host.drive_jobs()
    }

    /// Depth-gated `ui_prompt_start`: only the outermost prompt emits,
    /// with its kind and title.
    fn begin_ui_prompt(&mut self, kind: &str, title: Option<&str>) -> Result<(), JsHostError> {
        self.ui_prompt_depth += 1;
        if self.ui_prompt_depth == 1 {
            self.active_ui_prompt = Some((kind.to_string(), title.map(ToOwned::to_owned)));
            self.emit(&events::ui_prompt_start(kind, title))?;
        }
        Ok(())
    }

    /// Depth-gated `ui_prompt_end`, replaying the outermost prompt's
    /// kind/title; emitted before the resolved handler's continuation
    /// runs (upstream queues the end emit ahead of it).
    fn end_ui_prompt(&mut self) -> Result<(), JsHostError> {
        if self.ui_prompt_depth > 0 {
            self.ui_prompt_depth -= 1;
        }
        if self.ui_prompt_depth == 0 {
            let (kind, title) = self
                .active_ui_prompt
                .take()
                .unwrap_or_else(|| ("custom".to_string(), None));
            self.emit(&events::ui_prompt_end(&kind, title.as_deref()))?;
        }
        Ok(())
    }

    /// Begins a session, runs `dispatch` per handler, ends the session.
    /// Returns the per-handler outcomes in dispatch order.
    fn run_session(
        &mut self,
        event: &Json,
        kind: ContextKind,
        tool_call_id: Option<&str>,
        handlers: &[(usize, usize)],
        event_name: &str,
    ) -> Result<Vec<(usize, SessionOutcome)>, JsHostError> {
        let payload =
            serde_json::to_string(event).map_err(|err| JsHostError::Job(err.to_string()))?;
        let id = self.host.session_begin(&payload, kind, tool_call_id)?;
        let mut outcomes = Vec::with_capacity(handlers.len());
        let mut fatal: Option<JsHostError> = None;
        for (index, handler) in handlers {
            if fatal.is_some() {
                break;
            }
            let call = self.host.session_call(id, *index, event_name, *handler);
            if let Err(err) = call.and_then(|()| self.settle_session(id)) {
                fatal = Some(err);
                break;
            }
            match parse_session_outcome(&self.host.session_last(id)?) {
                Ok(outcome) => outcomes.push((*index, outcome)),
                Err(err) => fatal = Some(JsHostError::Job(err.to_string())),
            }
        }
        let _ = self.end_session(id);
        match fatal {
            Some(err) => Err(err),
            None => Ok(outcomes),
        }
    }

    /// The generic `emit`: notification-only for most events; the
    /// `session_before_*` family keeps the last handler result and
    /// short-circuits on `cancel: true` (`runner.ts:1103-1132`).
    pub fn emit(&mut self, event: &Json) -> Result<Option<Json>, JsHostError> {
        let event_type = event
            .get("type")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string();
        let is_session_before = event_type.starts_with("session_before_");
        let handlers = self.handlers_for(&event_type);
        let outcomes = self.run_session(event, ContextKind::Event, None, &handlers, &event_type)?;
        let mut last: Option<Json> = None;
        for (index, outcome) in outcomes {
            if let Some(error) = outcome.error {
                self.report_error(index, &event_type, error);
                continue;
            }
            if is_session_before && let Some(value) = outcome.value {
                last = Some(value.clone());
                if value.get("cancel").and_then(Json::as_bool) == Some(true) {
                    break;
                }
            }
        }
        Ok(if is_session_before { last } else { None })
    }

    /// The `session_before_*` emit with the parsed cancel result.
    pub fn emit_session_before(
        &mut self,
        event: &Json,
    ) -> Result<SessionBeforeResult, JsHostError> {
        Ok(self
            .emit(event)?
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default())
    }

    /// `emitToolCall`: the event is live across handlers so `input`
    /// mutations chain; the last non-undefined result wins and `block`
    /// short-circuits. Handler errors propagate (upstream has no catch in
    /// this loop, `runner.ts:1256-1274`); the caller receives the mutated
    /// event so it executes the (possibly patched) input.
    pub fn emit_tool_call(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        input: &Json,
    ) -> Result<(Option<ToolCallResult>, Json), JsHostError> {
        let event = events::tool_call(tool_call_id, tool_name, input);
        let handlers = self.handlers_for("tool_call");
        let payload =
            serde_json::to_string(&event).map_err(|err| JsHostError::Job(err.to_string()))?;
        let id = self
            .host
            .session_begin(&payload, ContextKind::Event, None)?;
        let mut result: Option<ToolCallResult> = None;
        let mut fatal: Option<JsHostError> = None;
        for (index, handler) in handlers {
            if let Err(err) = self
                .host
                .session_call(id, index, "tool_call", handler)
                .and_then(|()| self.settle_session(id))
            {
                let _ = self.end_session(id);
                fatal = Some(err);
                break;
            }
            let outcome = match parse_session_outcome(&self.host.session_last(id)?) {
                Ok(outcome) => outcome,
                Err(err) => {
                    let _ = self.end_session(id);
                    fatal = Some(JsHostError::Job(err.to_string()));
                    break;
                }
            };
            if let Some(error) = outcome.error {
                // No catch upstream: the error propagates to the caller.
                let _ = self.end_session(id);
                fatal = Some(JsHostError::JsException(error.message));
                break;
            }
            if let Some(value) = outcome.value {
                result = Some(serde_json::from_value(value).unwrap_or_default());
                if result.as_ref().is_some_and(|result| result.block) {
                    break;
                }
            }
        }
        if let Some(fatal) = fatal {
            return Err(fatal);
        }
        let event: Json = serde_json::from_str(&self.host.session_event(id)?)
            .map_err(|err| JsHostError::Job(err.to_string()))?;
        self.end_session(id)?;
        Ok((result, event))
    }

    /// `emitToolResult`: the middleware chain over
    /// content/details/structuredContent/isError/usage; replacing content
    /// without structuredContent drops it (`runner.ts:1197-1254`).
    /// Returns the final event when any handler modified it.
    pub fn emit_tool_result(&mut self, event: Json) -> Result<Option<Json>, JsHostError> {
        let handlers = self.handlers_for("tool_result");
        let mut current = event;
        let mut modified = false;
        for (index, handler) in handlers {
            let payload =
                serde_json::to_string(&current).map_err(|err| JsHostError::Job(err.to_string()))?;
            let id = self
                .host
                .session_begin(&payload, ContextKind::Event, None)?;
            self.host.session_call(id, index, "tool_result", handler)?;
            self.settle_session(id)?;
            let outcome = parse_session_outcome(&self.host.session_last(id)?);
            self.end_session(id)?;
            let outcome = outcome?;
            if let Some(error) = outcome.error {
                self.report_error(index, "tool_result", error);
                continue;
            }
            let Some(value) = outcome.value else {
                continue;
            };
            let Ok(patch) = serde_json::from_value::<ToolResultPatch>(value) else {
                continue;
            };
            let Some(object) = current.as_object_mut() else {
                continue;
            };
            if let Some(content) = patch.content {
                object.insert("content".to_string(), content);
                // Structured content that is not replaced along with the
                // content may no longer match it.
                if patch.structured_content.is_none() {
                    object.remove("structuredContent");
                }
                modified = true;
            }
            if let Some(details) = patch.details {
                object.insert("details".to_string(), details);
                modified = true;
            }
            if let Some(structured_content) = patch.structured_content {
                object.insert("structuredContent".to_string(), structured_content);
                modified = true;
            }
            if let Some(is_error) = patch.is_error {
                object.insert("isError".to_string(), Json::Bool(is_error));
                modified = true;
            }
            if let Some(usage) = patch.usage {
                object.insert("usage".to_string(), usage);
                modified = true;
            }
        }
        Ok(modified.then_some(current))
    }

    /// `emitInput`: the transform chain over text/images; `handled`
    /// short-circuits (`runner.ts:1532-1572`).
    pub fn emit_input(&mut self, event: Json) -> Result<InputResult, JsHostError> {
        let handlers = self.handlers_for("input");
        let mut current = event;
        let original_text = current
            .get("text")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string();
        let mut images_supplied = false;
        for (index, handler) in handlers {
            let payload =
                serde_json::to_string(&current).map_err(|err| JsHostError::Job(err.to_string()))?;
            let id = self
                .host
                .session_begin(&payload, ContextKind::Event, None)?;
            self.host.session_call(id, index, "input", handler)?;
            self.settle_session(id)?;
            let outcome = parse_session_outcome(&self.host.session_last(id)?);
            self.end_session(id)?;
            let outcome = outcome?;
            if let Some(error) = outcome.error {
                self.report_error(index, "input", error);
                continue;
            }
            let Some(value) = outcome.value else {
                continue;
            };
            let action = value
                .get("action")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_string();
            if action == "handled" {
                return Ok(InputResult::Handled { result: value });
            }
            if action == "transform" {
                if let Some(text) = value.get("text").and_then(Json::as_str) {
                    current["text"] = Json::String(text.to_string());
                }
                if let Some(images) = value.get("images")
                    && !images.is_null()
                {
                    current["images"] = images.clone();
                    images_supplied = true;
                }
            }
        }
        let text = current
            .get("text")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string();
        if text != original_text || images_supplied {
            let images = current
                .get("images")
                .filter(|images| !images.is_null())
                .cloned();
            Ok(InputResult::Transform { text, images })
        } else {
            Ok(InputResult::Continue)
        }
    }

    /// `emitBeforeAgentStart`: messages are collected, the `systemPrompt`
    /// override chains through `forceSystemPrompt`, and each handler
    /// observes the current system prompt (`runner.ts:1434-1484`).
    /// `render` builds the prompt from the current options, as
    /// `buildSystemPrompt` does upstream.
    pub fn emit_before_agent_start(
        &mut self,
        trigger: &BeforeAgentStartTrigger,
        system_prompt_options: Json,
        render: &dyn Fn(&Json) -> String,
    ) -> Result<BeforeAgentStartCombined, JsHostError> {
        let handlers = self.handlers_for("before_agent_start");
        let mut options = system_prompt_options;
        if !options.is_object() {
            options = Json::Object(serde_json::Map::new());
        }
        let mut combined = BeforeAgentStartCombined {
            messages: Vec::new(),
            system_prompt_options: options.clone(),
        };
        for (index, handler) in handlers {
            let mut event = trigger.to_json();
            let object = event.as_object_mut().expect("trigger object");
            object.insert(
                "type".to_string(),
                Json::String("before_agent_start".to_string()),
            );
            object.insert("systemPrompt".to_string(), Json::String(render(&options)));
            object.insert("systemPromptOptions".to_string(), options.clone());
            let payload =
                serde_json::to_string(&event).map_err(|err| JsHostError::Job(err.to_string()))?;
            let id = self
                .host
                .session_begin(&payload, ContextKind::Event, None)?;
            self.host
                .session_call(id, index, "before_agent_start", handler)?;
            self.settle_session(id)?;
            let outcome = parse_session_outcome(&self.host.session_last(id)?);
            self.end_session(id)?;
            let outcome = outcome?;
            if let Some(error) = outcome.error {
                self.report_error(index, "before_agent_start", error);
                continue;
            }
            let Some(value) = outcome.value else {
                continue;
            };
            let Ok(result) = serde_json::from_value::<BeforeAgentStartResult>(value) else {
                continue;
            };
            if let Some(message) = result.message {
                combined.messages.push(message);
            }
            if let Some(system_prompt) = result.system_prompt {
                options["forceSystemPrompt"] = Json::String(system_prompt);
                combined.system_prompt_options = options.clone();
            }
        }
        combined.system_prompt_options = options;
        Ok(combined)
    }

    /// `emitMessageEnd`: the message rewrite chain; a replacement with a
    /// different role is reported and skipped (`runner.ts:1158-1195`).
    pub fn emit_message_end(&mut self, message: Json) -> Result<Option<Json>, JsHostError> {
        let handlers = self.handlers_for("message_end");
        let mut current = message;
        let mut modified = false;
        for (index, handler) in handlers {
            let event = serde_json::json!({ "type": "message_end", "message": current });
            let payload =
                serde_json::to_string(&event).map_err(|err| JsHostError::Job(err.to_string()))?;
            let id = self
                .host
                .session_begin(&payload, ContextKind::Event, None)?;
            self.host.session_call(id, index, "message_end", handler)?;
            self.settle_session(id)?;
            let outcome = parse_session_outcome(&self.host.session_last(id)?);
            self.end_session(id)?;
            let outcome = outcome?;
            if let Some(error) = outcome.error {
                self.report_error(index, "message_end", error);
                continue;
            }
            let Some(value) = outcome.value else {
                continue;
            };
            let Some(replacement) = value.get("message") else {
                continue;
            };
            if replacement.get("role") != current.get("role") {
                self.report_error(
                    index,
                    "message_end",
                    crate::events::HandlerErrorJson {
                        message: "message_end handlers must return a message with the same role"
                            .to_string(),
                        stack: None,
                    },
                );
                continue;
            }
            current = replacement.clone();
            modified = true;
        }
        Ok(modified.then_some(current))
    }

    /// `emitBoundary` for `turn_end`/`agent_before_settle`: each handler
    /// sees the accumulated `entries` draft and `continue` flag, and the
    /// context preview is rebuilt after every handler; a failed rebuild
    /// discards everything (`runner.ts:1043-1092`).
    pub fn emit_boundary(
        &mut self,
        base_event: &Json,
        build_context: &mut dyn FnMut(&Json) -> Result<Json, String>,
    ) -> Result<BoundaryDispatchResult, JsHostError> {
        let event_type = base_event
            .get("type")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string();
        let handlers = self.handlers_for(&event_type);
        let mut entries = Json::Array(Vec::new());
        let mut continue_run = false;
        let mut context = build_context(&entries).map_err(JsHostError::Job)?;
        let mut valid = true;
        for (index, handler) in handlers {
            let mut event = base_event.clone();
            let object = event.as_object_mut().expect("boundary event object");
            object.insert("entries".to_string(), entries.clone());
            object.insert("continue".to_string(), Json::Bool(continue_run));
            object.insert("context".to_string(), context.clone());
            let payload =
                serde_json::to_string(&event).map_err(|err| JsHostError::Job(err.to_string()))?;
            let id = self
                .host
                .session_begin(&payload, ContextKind::Event, None)?;
            self.host.session_call(id, index, &event_type, handler)?;
            self.settle_session(id)?;
            let outcome = parse_session_outcome(&self.host.session_last(id)?);
            self.end_session(id)?;
            if let Ok(outcome) = &outcome {
                if let Some(error) = &outcome.error {
                    self.report_error(index, &event_type, error.clone());
                } else if let Some(value) = &outcome.value {
                    if let Some(handler_entries) = value.get("entries")
                        && !handler_entries.is_null()
                    {
                        entries = handler_entries.clone();
                    }
                    if let Some(handler_continue) = value.get("continue").and_then(Json::as_bool) {
                        continue_run = handler_continue;
                    }
                }
            }
            match build_context(&entries) {
                Ok(preview) => {
                    context = preview;
                    valid = true;
                }
                Err(error) => {
                    valid = false;
                    self.report_error(
                        index,
                        &event_type,
                        crate::events::HandlerErrorJson {
                            message: format!("Invalid boundary entries: {error}"),
                            stack: None,
                        },
                    );
                }
            }
        }
        Ok(if valid {
            BoundaryDispatchResult {
                entries,
                continue_run,
                context,
                valid: true,
            }
        } else {
            BoundaryDispatchResult {
                entries: Json::Array(Vec::new()),
                continue_run: false,
                context,
                valid: false,
            }
        })
    }

    /// `emitProjectTrustEvent`: the first handler that decides wins;
    /// `"undecided"` falls through and errors are collected
    /// (`runner.ts:302-329`).
    pub fn emit_project_trust(
        &mut self,
        cwd: &str,
    ) -> Result<(Option<ProjectTrustResult>, Vec<ExtensionError>), JsHostError> {
        let handlers = self.handlers_for("project_trust");
        let event = events::project_trust(cwd);
        let mut errors = Vec::new();
        for (index, handler) in handlers {
            let payload =
                serde_json::to_string(&event).map_err(|err| JsHostError::Job(err.to_string()))?;
            let id = self
                .host
                .session_begin(&payload, ContextKind::ProjectTrust, None)?;
            self.host
                .session_call(id, index, "project_trust", handler)?;
            self.settle_session(id)?;
            let outcome = parse_session_outcome(&self.host.session_last(id)?);
            self.end_session(id)?;
            let outcome = outcome?;
            if let Some(error) = outcome.error {
                let extension_path = self
                    .extensions
                    .get(index)
                    .map(|ext| ext.path.clone())
                    .unwrap_or_default();
                errors.push(ExtensionError {
                    extension_path,
                    event: "project_trust".to_string(),
                    error: error.message,
                    stack: error.stack,
                });
                continue;
            }
            let Some(value) = outcome.value else {
                continue;
            };
            let Ok(result) = serde_json::from_value::<ProjectTrustResult>(value) else {
                continue;
            };
            if result.trusted == "undecided" {
                continue;
            }
            return Ok((Some(result), errors));
        }
        Ok((None, errors))
    }

    /// Applies the staleness protocol: every captured `pi`/ctx throws the
    /// descriptive error afterwards (`invalidate`, `runner.ts:729-742`).
    pub fn invalidate(&mut self, message: Option<&str>) -> Result<(), JsHostError> {
        self.host.invalidate(message)
    }

    /// `emitUserBash` (`runner.ts:1276-1307`): handlers run in order;
    /// `undefined` falls through to local execution, the first valid
    /// `{operations}`/`{result}` wins. An invalid result or a handler
    /// error is reported and PROPAGATES (upstream rethrows).
    pub fn emit_user_bash(
        &mut self,
        command: &str,
        exclude_from_context: bool,
        cwd: &str,
    ) -> Result<Option<UserBashDecision>, JsHostError> {
        let event = events::user_bash(command, exclude_from_context, cwd);
        let handlers = self.handlers_for("user_bash");
        let payload =
            serde_json::to_string(&event).map_err(|err| JsHostError::Job(err.to_string()))?;
        let id = self
            .host
            .session_begin(&payload, ContextKind::Event, None)?;
        let mut decision: Option<UserBashDecision> = None;
        for (index, handler) in handlers {
            let outcome = match self
                .host
                .session_call(id, index, "user_bash", handler)
                .and_then(|()| self.settle_session(id))
                .and_then(|()| {
                    parse_session_outcome(&self.host.session_last(id)?)
                        .map_err(|err| JsHostError::Job(err.to_string()))
                }) {
                Ok(outcome) => outcome,
                Err(err) => {
                    self.report_error(
                        index,
                        "user_bash",
                        crate::events::HandlerErrorJson {
                            message: err.to_string(),
                            stack: None,
                        },
                    );
                    let _ = self.end_session(id);
                    return Err(err);
                }
            };
            if let Some(error) = outcome.error {
                self.report_error(
                    index,
                    "user_bash",
                    crate::events::HandlerErrorJson {
                        message: error.message.clone(),
                        stack: error.stack.clone(),
                    },
                );
                let _ = self.end_session(id);
                return Err(JsHostError::JsException(error.message));
            }
            match outcome.value {
                Some(value) if value.get("operations").and_then(Json::as_bool) == Some(true) => {
                    decision = Some(UserBashDecision::Operations);
                    break;
                }
                Some(value) if value.get("result").is_some() => {
                    decision = Some(UserBashDecision::Result(
                        value.get("result").cloned().unwrap_or(Json::Null),
                    ));
                    break;
                }
                _ => {}
            }
        }
        self.end_session(id)?;
        Ok(decision)
    }

    /// Runs the stashed `user_bash` `operations.exec` for one command.
    /// Returns `{exitCode}` (or `{error}`) plus the streamed output
    /// chunks; the host's bash executor consumes both.
    pub fn call_bash_exec(
        &mut self,
        command: &str,
        cwd: &str,
        options: &Json,
    ) -> Result<(Json, Vec<String>), JsHostError> {
        let options_json = serde_json::to_string(options).unwrap_or_else(|_| "{}".to_string());
        let result = self.host.call_bash_exec(command, cwd, &options_json)?;
        let output = std::mem::take(
            &mut self
                .core
                .lock()
                .expect("host core mutex poisoned")
                .bash_output,
        );
        Ok((result, output))
    }

    /// `emitContext` (`runner.ts:1312-1370`): `context` handlers see the
    /// conversation without system messages (in-place edits and
    /// `{messages}` returns both apply; Pi reattaches the replayed system
    /// message when the conversation changed), then
    /// `context_with_system` handlers see the full transcript and their
    /// output is used as returned. Handler errors are reported and
    /// skipped.
    // Ported verbatim as one linear upstream method; splitting the two
    // phases apart would obscure the shared `current` threading.
    #[allow(clippy::too_many_lines)]
    pub fn emit_context(&mut self, messages: &[Json]) -> Result<Vec<Json>, JsHostError> {
        let mut current: Vec<Json> = messages.to_vec();
        let payload = serde_json::to_string(&json!({
            "type": "context",
            "messages": [],
        }))
        .map_err(|err| JsHostError::Job(err.to_string()))?;
        let id = self
            .host
            .session_begin(&payload, ContextKind::Event, None)?;
        for (index, handler) in self.handlers_for("context") {
            let visible: Vec<Json> = current
                .iter()
                .filter(|message| message.get("role").and_then(Json::as_str) != Some("system"))
                .cloned()
                .collect();
            let snapshot = visible.clone();
            self.host.session_patch(
                id,
                "messages",
                &serde_json::to_string(&visible).unwrap_or_default(),
            )?;
            let outcome = self
                .host
                .session_call(id, index, "context", handler)
                .and_then(|()| self.settle_session(id))
                .and_then(|()| {
                    parse_session_outcome(&self.host.session_last(id)?)
                        .map_err(|err| JsHostError::Job(err.to_string()))
                });
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(err) => {
                    self.report_error(
                        index,
                        "context",
                        crate::events::HandlerErrorJson {
                            message: err.to_string(),
                            stack: None,
                        },
                    );
                    continue;
                }
            };
            if let Some(error) = outcome.error {
                self.report_error(
                    index,
                    "context",
                    crate::events::HandlerErrorJson {
                        message: error.message,
                        stack: error.stack,
                    },
                );
                continue;
            }
            let returned = if let Some(messages) = outcome
                .value
                .and_then(|value| value.get("messages").cloned())
            {
                Some(messages)
            } else {
                let event: Json =
                    serde_json::from_str(&self.host.session_event(id)?).unwrap_or(Json::Null);
                let mutated = event.get("messages").cloned().unwrap_or(Json::Null);
                if mutated == serde_json::to_value(&snapshot).unwrap_or(Json::Null) {
                    None
                } else {
                    Some(mutated)
                }
            };
            let Some(returned) = returned else {
                continue;
            };
            let returned: Vec<Json> = serde_json::from_value(returned).unwrap_or_default();
            if returned == snapshot {
                continue;
            }
            // restoreSystemMessages (runner.ts:292-301): a changed
            // conversation gets the replayed system message as one head.
            let head = transcript::get_current_system_message(&current);
            current = match head {
                Some(head) => {
                    let mut restored = vec![head];
                    restored.extend(returned);
                    restored
                }
                None => returned,
            };
        }
        for (index, handler) in self.handlers_for("context_with_system") {
            let had_leading_system = current
                .first()
                .and_then(|message| message.get("role").and_then(Json::as_str))
                == Some("system");
            self.host.session_patch(
                id,
                "messages",
                &serde_json::to_string(&current).unwrap_or_default(),
            )?;
            let outcome = self
                .host
                .session_call(id, index, "context_with_system", handler)
                .and_then(|()| self.settle_session(id))
                .and_then(|()| {
                    parse_session_outcome(&self.host.session_last(id)?)
                        .map_err(|err| JsHostError::Job(err.to_string()))
                });
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(err) => {
                    self.report_error(
                        index,
                        "context_with_system",
                        crate::events::HandlerErrorJson {
                            message: err.to_string(),
                            stack: None,
                        },
                    );
                    continue;
                }
            };
            if let Some(error) = outcome.error {
                self.report_error(
                    index,
                    "context_with_system",
                    crate::events::HandlerErrorJson {
                        message: error.message,
                        stack: error.stack,
                    },
                );
                continue;
            }
            if let Some(messages) = outcome
                .value
                .and_then(|value| value.get("messages").cloned())
            {
                current = serde_json::from_value(messages).unwrap_or(current);
            } else {
                // In-place edits to the live event.messages array are the
                // currentMessages array upstream.
                let event: Json =
                    serde_json::from_str(&self.host.session_event(id)?).unwrap_or(Json::Null);
                if let Some(messages) = event.get("messages").cloned() {
                    current = serde_json::from_value(messages).unwrap_or(current);
                }
            }
            if had_leading_system
                && current
                    .first()
                    .and_then(|message| message.get("role").and_then(Json::as_str))
                    != Some("system")
            {
                self.report_error(
                    index,
                    "context_with_system",
                    crate::events::HandlerErrorJson {
                        message: "Handler removed the leading system message; the request has no prompt or initial tool declarations. Keep it at index 0 or replace a dropped prefix with getCurrentSystemMessage().".to_string(),
                        stack: None,
                    },
                );
            }
        }
        self.end_session(id)?;
        Ok(current)
    }

    /// Monotonic mutation counter of the provider registry, for host-side
    /// composed-catalog rebuilds.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.core
            .lock()
            .expect("host core mutex poisoned")
            .providers
            .generation()
    }

    /// The provider registrations the host applies to its model catalog.
    #[must_use]
    pub fn providers(&self) -> Vec<(String, String, Json)> {
        let core = self.core.lock().expect("host core mutex poisoned");
        core.providers
            .providers()
            .iter()
            .map(|provider| {
                (
                    provider.name.clone(),
                    provider.extension_path.clone(),
                    provider.config.clone(),
                )
            })
            .collect()
    }

    /// The native provider registrations.
    #[must_use]
    pub fn native_providers(&self) -> Vec<(Json, String)> {
        let core = self.core.lock().expect("host core mutex poisoned");
        core.providers
            .native_providers()
            .iter()
            .map(|entry| (entry.provider.clone(), entry.extension_path.clone()))
            .collect()
    }

    /// The virtual-model definitions.
    #[must_use]
    pub fn virtual_models(&self) -> Vec<(Json, String)> {
        let core = self.core.lock().expect("host core mutex poisoned");
        core.providers
            .virtual_models()
            .iter()
            .map(|entry| (entry.definition.clone(), entry.extension_path.clone()))
            .collect()
    }

    /// Calls a virtual model's `route` with the request the host built
    /// (`ModelRuntime.resolveModel`'s `virtual.route(request)` leg,
    /// `model-runtime.ts:1059-1065`); the reply is the router's
    /// `{model, thinkingLevel, state?}`. The request crosses as the session
    /// event; dialog-like settling applies (promises drain through the job
    /// queue before the slot is read).
    pub fn route_model(
        &mut self,
        provider: &str,
        model_id: &str,
        request: &Json,
    ) -> Result<Option<Json>, JsHostError> {
        let payload =
            serde_json::to_string(request).map_err(|err| JsHostError::Job(err.to_string()))?;
        let id = self
            .host
            .session_begin(&payload, ContextKind::Route, None)?;
        let call = self.host.session_call_route(id, provider, model_id);
        let outcome = match call.and_then(|()| self.settle_session(id)) {
            Ok(()) => parse_session_outcome(&self.host.session_last(id)?),
            Err(err) => {
                let _ = self.end_session(id);
                return Err(err);
            }
        };
        let _ = self.end_session(id);
        Self::outcome_result(outcome.map_err(|err| JsHostError::Job(err.to_string()))?)
    }

    /// The api a provider's registered `streamSimple` handler covers —
    /// `streamWith`'s `model.api === extension.api` check
    /// (`provider-composer.ts:588-613`): `Some(api)` when the provider's
    /// merged config carries the JS-side stash marker (`streamSimple: true`
    /// — the function itself cannot cross the engine boundary) and declares
    /// an `api`. `None` otherwise, so the caller falls through the
    /// remaining `streamWith` arms.
    #[must_use]
    /// Whether the provider registered an oauth `login` handler (the
    /// stash is JS-side; the config JSON only carries the metadata).
    pub fn oauth_has_login(&self, provider: &str) -> bool {
        self.host.oauth_has_login(provider).unwrap_or(false)
    }

    /// Starts an oauth login flow: a session whose handler is the
    /// provider's stashed `login` (`__piSessionCallOAuthLogin`). Returns
    /// the session id; poll it with [`Self::oauth_login_poll`], answer
    /// prompts with [`Self::oauth_login_answer`], and end it with
    /// [`Self::oauth_login_end`].
    pub fn oauth_login_start(&mut self, provider: &str) -> Result<u32, JsHostError> {
        let payload = "{}";
        let id = self
            .host
            .session_begin(payload, ContextKind::OAuthLogin, None)?;
        self.host.session_call_oauth_login(id, provider)?;
        Ok(id)
    }

    /// One settle step of the login flow: drain jobs so the login function
    /// progresses, then collect the notifications emitted, the prompt now
    /// awaiting an answer, and the settled credential when the function
    /// finished. A prompt pauses the flow until
    /// [`Self::oauth_login_answer`].
    pub fn oauth_login_poll(&mut self, id: u32) -> Result<OAuthLoginPoll, JsHostError> {
        self.settle_session(id)?;
        let notifications = self.host.take_oauth_notify_events(id);
        let prompt = self.host.take_oauth_prompt(id);
        let settled = if self.session_settled(id)? {
            let raw = self.host.session_last(id)?;
            let outcome: SessionOutcome =
                parse_session_outcome(&raw).map_err(|err| JsHostError::Job(err.to_string()))?;
            if let Some(error) = outcome.error {
                Some(Err(error.message))
            } else {
                // The glue settles with `JSON.stringify(credential)` — a
                // JSON string crossing as the outcome's string value.
                let credential = outcome
                    .value
                    .as_ref()
                    .and_then(Json::as_str)
                    .and_then(|text| serde_json::from_str::<Json>(text).ok())
                    .unwrap_or(Json::Null);
                Some(Ok(credential))
            }
        } else {
            None
        };
        Ok(OAuthLoginPoll {
            notifications,
            prompt,
            settled,
        })
    }

    /// Resolves the login flow's pending prompt with the host's answer
    /// (the credential field the prompt asked for, or `null` for an
    /// undefined select) and drains jobs so the login function continues.
    pub fn oauth_login_answer(
        &mut self,
        id: u32,
        token: u64,
        answer: &Json,
    ) -> Result<(), JsHostError> {
        let resolution = self.host.oauth_resolve_prompt(id, token, answer)?;
        if resolution != "ok" {
            return Err(JsHostError::Job(format!(
                "oauth prompt answer rejected: {resolution}"
            )));
        }
        self.host.drive_jobs()?;
        Ok(())
    }

    /// Flags the login flow's `signal.aborted` — the UI's interrupt during
    /// a login flow; the login function's own logic decides how to stop.
    pub fn oauth_login_abort(&mut self, id: u32) -> Result<(), JsHostError> {
        self.host.oauth_abort_signal(id)?;
        self.host.drive_jobs()?;
        Ok(())
    }

    /// Ends a login flow session, releasing its live objects.
    pub fn oauth_login_end(&mut self, id: u32) -> Result<(), JsHostError> {
        self.end_session(id)
    }

    /// `adaptOAuth`'s `refresh` arm (`provider-composer.ts:508-510`): the
    /// stashed `refreshToken` with the refreshed credential, `type:
    /// "oauth"` included by the glue.
    pub fn oauth_refresh(&mut self, provider: &str, credential: &Json) -> Result<Json, String> {
        let outcome = self
            .host
            .call_oauth_refresh(provider, credential)
            .map_err(|err| err.to_string())?;
        if let Some(error) = outcome.get("error").and_then(Json::as_str) {
            Err(error.to_string())
        } else {
            Ok(outcome.get("value").cloned().unwrap_or(Json::Null))
        }
    }

    /// `adaptOAuth`'s `toAuth` arm (`provider-composer.ts:511`): the
    /// stashed `getApiKey`'s key for the stored credential.
    pub fn oauth_api_key(&mut self, provider: &str, credential: &Json) -> Result<String, String> {
        self.host
            .call_oauth_api_key(provider, credential)
            .map_err(|err| err.to_string())
            .and_then(|outcome| {
                if let Some(error) = outcome.get("error").and_then(Json::as_str) {
                    Err(error.to_string())
                } else {
                    Ok(outcome
                        .get("value")
                        .and_then(Json::as_str)
                        .unwrap_or_default()
                        .to_string())
                }
            })
    }

    pub fn provider_stream_api(&self, provider: &str) -> Option<String> {
        let core = self.core.lock().expect("host core mutex poisoned");
        let config = core
            .providers
            .providers()
            .iter()
            .find(|entry| entry.name == provider)?
            .config
            .as_object()?;
        if !config
            .get("streamSimple")
            .is_some_and(|v| v.as_bool().unwrap_or(false))
        {
            return None;
        }
        config.get("api").and_then(Json::as_str).map(str::to_string)
    }

    /// Calls a provider's `streamSimple` with the request the host built
    /// (`streamWith`'s extension arm, `provider-composer.ts:588-613` →
    /// `extension.streamSimple(model, context, options)`): the marshalled
    /// `{model, context, options}` crosses as the session event, the
    /// handler's event stream drains inside the engine (each event through
    /// the `providerStreamEvent` op), and the events return in emission
    /// order. Dialog-like settling applies (promises drain through the job
    /// queue before the slot is read). A handler throw — before or
    /// mid-iteration — lands in [`ProviderStreamOutcome::error`] with the
    /// events already emitted, mirroring upstream's lazyStream: a
    /// setup/iteration failure appends the terminal error event after the
    /// events already forwarded (`packages/ai/src/api/lazy.ts:65-105`).
    ///
    /// The `SimpleStreamOptions` instrumentation the glue decorated the
    /// options with is serviced here: `onPayload` asks run the
    /// `before_provider_request` transform chain and resolve with its
    /// final value (`sdk.ts:374-382`'s `transformProviderPayload`), the
    /// one-way `onResponse`/`onProviderStreamEvent` events emit FIFO, and
    /// `signal` — a reader over the run's abort flag — flips the crossed
    /// object once the run aborts. `signal` is a closure because this
    /// crate stays `pi-agent`-free.
    pub fn provider_stream(
        &mut self,
        provider: &str,
        request: &Json,
        signal: Option<&dyn Fn() -> bool>,
    ) -> Result<ProviderStreamOutcome, JsHostError> {
        let payload =
            serde_json::to_string(request).map_err(|err| JsHostError::Job(err.to_string()))?;
        // The `provider_stream_event` model fallback: a handler may omit
        // the model argument; upstream's adapters always pass the request
        // model (the shape sdk.ts:388-397 emits).
        let fallback = request.get("model").cloned().unwrap_or(Json::Null);
        let model_field = |model: &Json, field: &str, backstop: &str| -> String {
            model
                .get(field)
                .and_then(Json::as_str)
                .unwrap_or(backstop)
                .to_string()
        };
        let id = self
            .host
            .session_begin(&payload, ContextKind::ProviderStream, None)?;
        let call = self.host.session_call_provider_stream(id, provider);
        let outcome = match call
            .and_then(|()| self.drain_stream_session(id, signal, &fallback, &model_field))
        {
            Ok(()) => parse_session_outcome(&self.host.session_last(id)?),
            Err(err) => {
                let events = self.host.take_provider_stream_events(id);
                let _ = self.end_session(id);
                return Ok(ProviderStreamOutcome {
                    events,
                    error: Some(err.to_string()),
                });
            }
        };
        let events = self.host.take_provider_stream_events(id);
        let _ = self.end_session(id);
        let outcome = outcome.map_err(|err| JsHostError::Job(err.to_string()))?;
        // A session that never settled (the handler awaited something only
        // jobs could resolve and none came) is a failure like the route
        // dispatch's `HandlerDidNotComplete`, not a clean end — the events
        // already emitted still cross.
        let error = match outcome.error {
            Some(error) => Some(error.message),
            None if outcome.done => None,
            None => Some("the handler never settled while draining the job queue".to_string()),
        };
        Ok(ProviderStreamOutcome { events, error })
    }

    /// The stream session's drain: job rounds interleaved with the
    /// hook servicing (`onPayload` transforms, one-way emissions, the
    /// abort flip). Stops when the session settles or a round makes no
    /// progress — the caller reads the pending slot like
    /// [`Self::settle_session`]'s contract.
    fn drain_stream_session(
        &mut self,
        id: u32,
        signal: Option<&dyn Fn() -> bool>,
        fallback_model: &Json,
        model_field: &dyn Fn(&Json, &str, &str) -> String,
    ) -> Result<(), JsHostError> {
        let mut flipped = false;
        loop {
            self.host.drive_jobs()?;
            let requests = self.host.drain_ui_requests();
            if !requests.is_empty() {
                for request in &requests {
                    self.deliver_ui_request(request)?;
                    self.host.drive_jobs()?;
                }
                continue;
            }
            if !flipped && signal.is_some_and(|signal| signal()) {
                self.host.provider_flip_signal(id)?;
                flipped = true;
            }
            let mut progressed = false;
            let hooks = self.host.take_provider_hook_requests(id);
            if !hooks.is_empty() {
                progressed = true;
            }
            for (token, payload) in hooks {
                let replaced = self.emit_before_provider_request(&payload)?;
                self.host.provider_resolve_hook(id, token, &replaced)?;
                self.host.drive_jobs()?;
            }
            let events = self.host.take_provider_hook_events(id);
            if !events.is_empty() {
                progressed = true;
            }
            for (kind, payload) in events {
                match kind.as_str() {
                    "after_provider_response" => {
                        let status = payload.get("status").and_then(Json::as_i64).unwrap_or(0);
                        let headers = payload.get("headers").cloned().unwrap_or(Json::Null);
                        self.emit_after_provider_response(status, &headers)?;
                    }
                    "provider_stream_event" => {
                        let data = payload.get("data").cloned().unwrap_or(Json::Null);
                        let model = payload.get("model").cloned().unwrap_or(Json::Null);
                        let provider = model_field(
                            &model,
                            "provider",
                            &model_field(fallback_model, "provider", "unknown"),
                        );
                        let api = model_field(
                            &model,
                            "api",
                            &model_field(fallback_model, "api", "unknown"),
                        );
                        let model_id = model_field(
                            &model,
                            "id",
                            &model_field(fallback_model, "id", "unknown"),
                        );
                        self.emit_provider_stream_event(&data, &provider, &api, &model_id)?;
                    }
                    _ => {}
                }
            }
            if self.session_settled(id)? {
                return Ok(());
            }
            if !progressed {
                // A promise only host actions can settle; leave the slot
                // pending (the caller reads `{done:false}`).
                return Ok(());
            }
        }
    }

    /// `before_provider_request`'s transform chain
    /// (`emitBeforeProviderRequest`, `runner.ts:1375-1398`): each handler
    /// receives a fresh event carrying the current payload; a
    /// non-undefined return replaces it (a `null` replacement crosses as
    /// `null`). Handler errors report through the error listeners; the
    /// chain always answers the final payload.
    pub fn emit_before_provider_request(&mut self, payload: &Json) -> Result<Json, JsHostError> {
        let mut current = payload.clone();
        for (index, handler) in self.handlers_for("before_provider_request") {
            let event = json!({ "type": "before_provider_request", "payload": current });
            let event_payload =
                serde_json::to_string(&event).map_err(|err| JsHostError::Job(err.to_string()))?;
            let id = self
                .host
                .session_begin(&event_payload, ContextKind::Event, None)?;
            self.host
                .session_call(id, index, "before_provider_request", handler)?;
            self.settle_session(id)?;
            let raw = self.host.session_last(id)?;
            let outcome =
                parse_session_outcome(&raw).map_err(|err| JsHostError::Job(err.to_string()))?;
            let _ = self.end_session(id);
            if let Some(error) = outcome.error {
                self.report_error(index, "before_provider_request", error);
            } else if let Some(value) = outcome.value {
                current = value;
            }
        }
        Ok(current)
    }

    /// `after_provider_response`'s notification fan-out
    /// (`sdk.ts:379-387`'s `handleProviderResponse`): `{type, status,
    /// headers}`; handler errors report, nothing returns.
    pub fn emit_after_provider_response(
        &mut self,
        status: i64,
        headers: &Json,
    ) -> Result<(), JsHostError> {
        self.emit(&json!({
            "type": "after_provider_response",
            "status": status,
            "headers": headers,
        }))?;
        Ok(())
    }

    /// `provider_stream_event`'s notification fan-out (`sdk.ts:388-397`):
    /// `{type, data, provider, api, model}`; handler errors report,
    /// nothing returns.
    pub fn emit_provider_stream_event(
        &mut self,
        data: &Json,
        provider: &str,
        api: &str,
        model_id: &str,
    ) -> Result<(), JsHostError> {
        self.emit(&json!({
            "type": "provider_stream_event",
            "data": data,
            "provider": provider,
            "api": api,
            "model": model_id,
        }))?;
        Ok(())
    }

    /// Provider registrations that failed validation at the bind-time
    /// flush, in flush order (upstream's bindCore `emitError` reports,
    /// `runner.ts:470-521`).
    #[must_use]
    pub fn provider_registration_errors(&self) -> Vec<crate::events::ProviderBindError> {
        self.core
            .lock()
            .expect("host core mutex poisoned")
            .provider_bind_errors
            .clone()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::assert_is_empty)] // fixture assertions read cleaner with assert!
    use super::*;
    use crate::engine::HostCore;
    use serde_json::json;

    /// Two extensions whose handlers observe and react.
    const EXT_ONE: &str = r#"
export default function (pi) {
  pi.on("session_start", (event) => pi.appendEntry("saw", event.reason));
  pi.on("session_before_switch", () => ({ cancel: false }));
  pi.on("tool_call", (event) => {
    event.input.touched = "one";
  });
  pi.on("input", (event) => ({ action: "transform", text: event.text.trim() }));
  pi.on("before_agent_start", () => ({ systemPrompt: "override-one" }));
  pi.on("message_end", (event) => ({ message: { ...event.message, role: event.message.role, patched: true } }));
  pi.on("turn_end", () => ({ entries: [{ customType: "draft-one" }], continue: true }));
  pi.on("project_trust", () => ({ trusted: "undecided" }));
  pi.registerCommand("deploy", { description: "one", handler: async () => pi.appendEntry("ran", "deploy-one") });
  pi.registerCommand("deploy:2", { description: "collides", handler: async () => {} });
  pi.registerFlag("mode", { type: "string", default: "first" });
}
"#;

    const EXT_TWO: &str = r#"
export default function (pi) {
  pi.on("session_start", () => {
    throw new Error("boom-two");
  });
  pi.on("session_before_switch", () => ({ cancel: true }));
  pi.on("session_before_switch", () => ({ cancel: false }));
  pi.on("tool_call", (event) => {
    if (event.input.touched !== "one") throw new Error("mutation lost");
    return { block: true, reason: "guarded" };
  });
  pi.on("input", () => ({ action: "handled" }));
  pi.on("before_agent_start", (event, ctx) => {
    ctx.ui.notify(event.systemPrompt);
    return { message: { customType: "note", content: "hello" } };
  });
  pi.on("message_end", () => ({ message: { role: "user", different: true } }));
  pi.on("turn_end", () => ({ continue: false }));
  pi.on("project_trust", () => ({ trusted: "yes", remember: true }));
  pi.registerCommand("deploy", { description: "two", handler: async () => pi.appendEntry("ran", "deploy-two") });
  pi.registerFlag("mode", { type: "string", default: "second" });
}
"#;

    const EXT_THREE: &str = r#"
export default function (pi) {
  pi.on("tool_result", (event) => ({ content: [{ type: "text", text: "redacted" }] }));
  pi.on("tool_result", (event) => ({ isError: !event.isError }));
  pi.registerCommand("deploy", { description: "three", handler: async () => {} });
}
"#;

    fn runner_with(extensions: &[&str]) -> ExtensionRunner {
        let mut runner = ExtensionRunner::new(HostCore {
            cwd: "/tmp".to_string(),
            mode: "print".to_string(),
            settings: json!({}),
            ..Default::default()
        })
        .expect("runner");
        for (index, source) in extensions.iter().enumerate() {
            runner
                .load_extension(&format!("<test-{index}>"), Some(source))
                .expect("load");
        }
        runner.bind().expect("bind");
        runner
    }

    fn errors_of(
        runner: &mut ExtensionRunner,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<ExtensionError>>> {
        // Listeners are `Send`, so the sink is an `Arc<Mutex<_>>`, not `Rc`.
        let errors = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = errors.clone();
        runner.on_error(Box::new(move |error| {
            sink.lock().expect("sink").push(error.clone());
        }));
        errors
    }

    // ---- E2 UI surface ----

    /// Scripted UI sink: records every request, answers dialogs by
    /// method (or `{}` — the sink-side timeout default resolution).
    struct FakeSink {
        requests: Mutex<Vec<Json>>,
        responses: Mutex<std::collections::HashMap<String, Json>>,
    }

    impl FakeSink {
        fn scripted(responses: &[(&str, Json)]) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(
                    responses
                        .iter()
                        .map(|(method, response)| (method.to_string(), response.clone()))
                        .collect(),
                ),
            })
        }

        fn requests(&self) -> Vec<Json> {
            self.requests.lock().expect("requests").clone()
        }
    }

    impl UiSink for FakeSink {
        fn request(&self, request: &Json) {
            self.requests
                .lock()
                .expect("requests")
                .push(request.clone());
        }

        fn wait_response(&self, _id: &str, method: &str) -> Json {
            self.responses
                .lock()
                .expect("responses")
                .get(method)
                .cloned()
                .unwrap_or_else(|| json!({}))
        }
    }

    fn entries_of(runner: &ExtensionRunner) -> Vec<Json> {
        runner
            .core()
            .lock()
            .expect("core")
            .entries
            .iter()
            .map(|(custom_type, data)| json!({ "type": custom_type, "data": data }))
            .collect()
    }

    const EXT_UI_DIALOG: &str = r#"
export default function (pi) {
  pi.on("session_start", async (event, ctx) => {
    const choice = await ctx.ui.select("Pick", ["a", "b"]);
    pi.appendEntry("choice", choice);
  });
  pi.on("ui_prompt_start", (event) => pi.appendEntry("prompt-start", event.kind));
  pi.on("ui_prompt_end", (event) => pi.appendEntry("prompt-end", event.kind));
}
"#;

    #[test]
    fn ui_dialog_round_trip_and_prompt_events() {
        let mut runner = runner_with(&[EXT_UI_DIALOG]);
        let sink = FakeSink::scripted(&[("select", json!({ "value": "a" }))]);
        runner.set_ui_context(sink.clone(), "rpc");
        runner
            .emit(&json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        let requests = sink.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["method"], "select");
        assert_eq!(requests[0]["title"], "Pick");
        assert_eq!(requests[0]["options"], json!(["a", "b"]));
        assert!(requests[0]["id"].is_string());
        assert_eq!(
            entries_of(&runner),
            vec![
                json!({ "type": "prompt-start", "data": "select" }),
                json!({ "type": "prompt-end", "data": "select" }),
                json!({ "type": "choice", "data": "a" }),
            ]
        );
    }

    // ---- E3 slice 6b: interactive ctx.ui.custom ----

    /// The factory's adapters: theme.fg stays identity (no surface
    /// installed), theme.bold crosses through the installed styler, the
    /// keybindings adapter answers matches/getKeys from the installed
    /// matcher + table, and `tui.requestRender()` queues a crossing.
    const EXT_UI_CUSTOM: &str = r#"
export default function (pi) {
  pi.on("session_start", async (event, ctx) => {
    try {
      const value = await ctx.ui.custom((tui, theme, keybindings, done) => ({
        render: (width) => [
          theme.fg("accent",
            "PICK " + width +
            " m:" + keybindings.matches("\x1b", "select.cancel") +
            " k:" + keybindings.getKeys("app.cancel").join("/") +
            " b:" + theme.bold("B")),
        ],
        invalidate: () => {},
        handleInput: (data) => {
          if (data === "x") {
            done("picked-value");
            tui.requestRender();
            return true;
          }
          return false;
        },
      }));
      pi.appendEntry("picked", String(value));
    } catch (error) {
      pi.appendEntry("caught", String(error && error.message ? error.message : error));
    }
  });
  pi.on("ui_prompt_start", (event) => pi.appendEntry("prompt-start", event.kind));
  pi.on("ui_prompt_end", (event) => pi.appendEntry("prompt-end", event.kind));
}
"#;

    fn esc_select_cancel(data: &str, action: &str) -> bool {
        data == "\x1b" && action == "select.cancel"
    }

    #[test]
    fn ui_custom_factory_mounts_and_done_resolves() {
        let mut runner = runner_with(&[EXT_UI_CUSTOM]);
        let sink = FakeSink::scripted(&[]);
        runner.set_ui_context(sink.clone(), "tui");
        runner.set_keybinding_matcher(esc_select_cancel);
        runner.set_app_keybindings(r#"{"app.cancel":["ctrl+c"]}"#);
        runner.set_style_surface(std::sync::Arc::new(|method, text| match method {
            "bold" => format!("\x1b[1m{text}\x1b[22m"),
            _ => text.to_string(),
        }));
        runner
            .emit(&json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");

        // The factory's component adopted; exactly the `custom` crossing
        // (the prompt pair's start emit carries no request).
        let requests = sink.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["method"], "custom");
        let handle = requests[0]["handle"].as_u64().expect("adopted handle") as u32;

        // Render crosses the adapters: identity fg, the matcher, the
        // crossed keybinding table, the installed styler.
        assert_eq!(
            runner.component_render(handle, 40).expect("render"),
            vec!["PICK 40 m:true k:ctrl+c b:\x1b[1mB\x1b[22m".to_string()]
        );
        assert!(runner.component_handle_input(handle, "x").expect("input"));
        assert!(!runner.component_handle_input(handle, "y").expect("input"));

        // done() queued the result on the customDone crossing: the next
        // service forwards it, resolves the deferred (registered glue-side
        // under the crossing's id), ends the prompt pair, and runs the
        // resolved continuation — the end emit lands before the continuation
        // (upstream's `.finally(finish)` order).
        let deferred = runner.service_ui_requests().expect("service");
        assert!(deferred.is_empty());
        let requests = sink.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1]["method"], "customDone");
        assert_eq!(requests[1]["id"], requests[0]["id"]);
        assert_eq!(requests[2]["method"], "requestRender");
        assert_eq!(
            entries_of(&runner),
            vec![
                json!({ "type": "prompt-start", "data": "custom" }),
                json!({ "type": "prompt-end", "data": "custom" }),
                json!({ "type": "picked", "data": "picked-value" }),
            ]
        );
    }

    #[test]
    fn ui_custom_factory_throw_rejects_without_crossing() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", async (event, ctx) => {
    try {
      await ctx.ui.custom(() => {
        throw new Error("boom-factory");
      });
    } catch (error) {
      pi.appendEntry("caught", String(error && error.message ? error.message : error));
    }
  });
}
"#;
        let mut runner = runner_with(&[EXT]);
        let sink = FakeSink::scripted(&[]);
        runner.set_ui_context(sink.clone(), "tui");
        runner
            .emit(&json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        // The rejection carries the thrown error; nothing was adopted or
        // crossed (no request, no ui_prompt pair).
        assert!(sink.requests().is_empty());
        assert_eq!(
            entries_of(&runner),
            vec![json!({ "type": "caught", "data": "boom-factory" })]
        );
    }

    #[test]
    fn ui_custom_done_before_crossing_resolves_without_a_prompt() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", async (event, ctx) => {
    const value = await ctx.ui.custom((_tui, _theme, _kb, done) => {
      done("early");
      return { render: () => ["SHOULD NOT RENDER"], invalidate: () => {} };
    });
    pi.appendEntry("picked", String(value));
  });
  pi.on("ui_prompt_start", (event) => pi.appendEntry("prompt-start", event.kind));
  pi.on("ui_prompt_end", (event) => pi.appendEntry("prompt-end", event.kind));
}
"#;
        let mut runner = runner_with(&[EXT]);
        let sink = FakeSink::scripted(&[]);
        runner.set_ui_context(sink.clone(), "tui");
        runner
            .emit(&json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        // done() closed before the component crossed: the promise resolved
        // without the `custom` request, the mount, or the prompt pair.
        assert!(sink.requests().is_empty());
        assert_eq!(
            entries_of(&runner),
            vec![json!({ "type": "picked", "data": "early" })]
        );
    }

    /// The overlay arm of `ctx.ui.custom` (E3 slice 6c, `interactive-mode.ts
    /// :3089/:3139-3157`): `overlay: true` crosses the resolved static
    /// options and the blocked status with the `custom` request, `onHandle`
    /// fires with an adapter bound to the crossing id whose writes queue
    /// fire-and-forget crossings, and `done()` rides the same customDone
    /// path the inline custom uses.
    #[test]
    fn ui_custom_overlay_crosses_options_blocked_and_on_handle() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", async (event, ctx) => {
    let handleActions = 0;
    const value = await ctx.ui.custom(
      (_tui, _theme, _kb, done) => ({
        render: (width) => ["OVERLAY " + width],
        invalidate: () => {},
        handleInput: (data) => {
          if (data === "x") {
            done("overlay-value");
            return true;
          }
          return false;
        },
      }),
      {
        overlay: true,
        overlayOptions: { width: 30, anchor: "bottom-right" },
        blocked: { kind: "question", message: "Blocking" },
        onHandle: (handle) => {
          handleActions += 1;
          handle.setHidden(true);
          handle.setHidden(false);
        },
      },
    );
    pi.appendEntry("picked", String(value));
    pi.appendEntry("handles", String(handleActions));
  });
  pi.on("ui_prompt_start", (event) => pi.appendEntry("prompt-start", event.kind));
  pi.on("ui_prompt_end", (event) => pi.appendEntry("prompt-end", event.kind));
}
"#;
        let mut runner = runner_with(&[EXT]);
        let sink = FakeSink::scripted(&[]);
        runner.set_ui_context(sink.clone(), "tui");
        runner
            .emit(&json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");

        // The custom crossing carries the overlay fields; onHandle's writes
        // crossed right behind it as fire-and-forget requests.
        let requests = sink.requests();
        assert_eq!(requests.len(), 3, "{requests:?}");
        assert_eq!(requests[0]["method"], "custom");
        assert_eq!(requests[0]["overlay"], true);
        assert_eq!(requests[0]["overlayOptions"]["width"], 30);
        assert_eq!(requests[0]["overlayOptions"]["anchor"], "bottom-right");
        assert_eq!(
            requests[0]["blocked"],
            json!({ "kind": "question", "message": "Blocking" })
        );
        assert!(requests[0]["id"].is_string());
        assert_eq!(requests[1]["method"], "overlaySetHidden");
        assert_eq!(requests[1]["id"], requests[0]["id"]);
        assert_eq!(requests[1]["hidden"], true);
        assert_eq!(requests[2]["method"], "overlaySetHidden");
        assert_eq!(requests[2]["hidden"], false);
        // The prompt pair opened around the overlay custom like the inline
        // one (`deliver_ui_request`'s custom arm).
        assert_eq!(
            entries_of(&runner),
            vec![json!({ "type": "prompt-start", "data": "custom" })]
        );

        // done() closes through the customDone crossing, ending the pair.
        let handle = requests[0]["handle"].as_u64().expect("adopted handle") as u32;
        assert!(runner.component_handle_input(handle, "x").expect("input"));
        let deferred = runner.service_ui_requests().expect("service");
        assert!(deferred.is_empty());
        let requests = sink.requests();
        assert_eq!(requests[3]["method"], "customDone");
        assert_eq!(requests[3]["id"], requests[0]["id"]);
        assert_eq!(
            entries_of(&runner),
            vec![
                json!({ "type": "prompt-start", "data": "custom" }),
                json!({ "type": "prompt-end", "data": "custom" }),
                json!({ "type": "picked", "data": "overlay-value" }),
                json!({ "type": "handles", "data": "1" }),
            ]
        );
    }

    // ------------------------------------------------- small ctx.ui surfaces (E3 slice 7)

    /// The one-way status/widget/editor-text crossings queue in call order
    /// with the shapes the mode's pump arms read (`createExtensionUIContext`,
    /// `interactive-mode.ts:2773-2824`); `undefined` resets ride as absent
    /// fields.
    #[test]
    fn ui_small_surfaces_cross_in_call_order() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", async (event, ctx) => {
    ctx.ui.setWorkingMessage("Compacting");
    ctx.ui.setWorkingMessage(undefined);
    ctx.ui.setWorkingVisible(false);
    ctx.ui.setWorkingIndicator({ frames: ["|", "/"], intervalMs: 120 });
    ctx.ui.setWorkingIndicator(undefined);
    ctx.ui.setHiddenThinkingLabel("hidden");
    ctx.ui.setHiddenThinkingLabel(undefined);
    ctx.ui.setWidget("alpha", ["one", "two"], { placement: "belowEditor" });
    ctx.ui.setWidget("alpha", undefined);
    ctx.ui.setToolsExpanded(true);
    ctx.ui.pasteToEditor("pasted");
    ctx.ui.setEditorText("draft");
  });
}
"#;
        let mut runner = runner_with(&[EXT]);
        let sink = FakeSink::scripted(&[]);
        runner.set_ui_context(sink.clone(), "tui");
        runner
            .emit(&json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        let requests = sink.requests();
        assert_eq!(requests.len(), 12, "{requests:?}");
        assert_eq!(requests[0]["method"], "setWorkingMessage");
        assert_eq!(requests[0]["message"], "Compacting");
        assert_eq!(requests[1]["method"], "setWorkingMessage");
        assert!(requests[1].get("message").is_none(), "undefined resets");
        assert_eq!(requests[2]["method"], "setWorkingVisible");
        assert_eq!(requests[2]["visible"], false);
        assert_eq!(requests[3]["method"], "setWorkingIndicator");
        assert_eq!(
            requests[3]["indicator"],
            json!({ "frames": ["|", "/"], "intervalMs": 120 })
        );
        assert_eq!(requests[4]["method"], "setWorkingIndicator");
        assert!(requests[4].get("indicator").is_none());
        assert_eq!(requests[5]["method"], "setHiddenThinkingLabel");
        assert_eq!(requests[5]["label"], "hidden");
        assert_eq!(requests[6]["method"], "setHiddenThinkingLabel");
        assert!(requests[6].get("label").is_none());
        assert_eq!(requests[7]["method"], "setWidget");
        assert_eq!(requests[7]["widgetKey"], "alpha");
        assert_eq!(requests[7]["widgetLines"], json!(["one", "two"]));
        assert_eq!(requests[7]["widgetPlacement"], "belowEditor");
        assert_eq!(requests[8]["method"], "setWidget");
        assert!(requests[8].get("widgetLines").is_none(), "undefined removes");
        assert_eq!(requests[9]["method"], "setToolsExpanded");
        assert_eq!(requests[9]["expanded"], true);
        assert_eq!(requests[10]["method"], "pasteToEditor");
        assert_eq!(requests[10]["text"], "pasted");
        assert_eq!(requests[11]["method"], "set_editor_text");
        assert_eq!(requests[11]["text"], "draft");
    }

    /// `setWidget`'s factory arm (`interactive-mode.ts:2570-2573`): the
    /// factory runs at set time with the factory adapters, its component
    /// is adopted, and the handle crosses for the mode's widget registry.
    #[test]
    fn ui_set_widget_factory_adopts_a_component() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", async (event, ctx) => {
    ctx.ui.setWidget("clock", (_tui, _theme) => ({
      render: (width) => ["WIDGET " + String(width)],
      invalidate: () => {},
    }));
  });
}
"#;
        let mut runner = runner_with(&[EXT]);
        let sink = FakeSink::scripted(&[]);
        runner.set_ui_context(sink.clone(), "tui");
        runner
            .emit(&json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        let requests = sink.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0]["method"], "setWidget");
        assert_eq!(requests[0]["widgetKey"], "clock");
        assert!(requests[0].get("widgetLines").is_none());
        let handle = requests[0]["widgetHandle"].as_u64().expect("adopted") as u32;
        assert_eq!(
            runner.component_render(handle, 20).expect("render"),
            ["WIDGET 20"]
        );
    }

    /// `setFooter`/`setHeader` (`interactive-mode.ts:2664-2730`): the
    /// factories run at set time, the footer's data adapter reads the
    /// snapshot surface synchronously, the synchronous ui queries answer
    /// from it too, and `undefined` restores the built-ins (no handle
    /// crosses).
    #[test]
    fn ui_footer_and_header_factories_adopt_and_read_the_snapshot() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", async (event, ctx) => {
    ctx.ui.setFooter((_tui, _theme, footerData) => ({
      render: (_width) => [
        "FOOTER " + String(footerData.getGitBranch()) + "/" + String(footerData.getAvailableProviderCount()),
      ],
      invalidate: () => {},
    }));
    ctx.ui.setHeader((_tui, _theme) => ({
      render: (_width) => ["HEADER"],
      invalidate: () => {},
    }));
    ctx.ui.setFooter(undefined);
    ctx.ui.setHeader(undefined);
    pi.appendEntry("editor", ctx.ui.getEditorText());
    pi.appendEntry("expanded", String(ctx.ui.getToolsExpanded()));
  });
}
"#;
        let mut runner = runner_with(&[EXT]);
        let sink = FakeSink::scripted(&[]);
        runner.set_ui_context(sink.clone(), "tui");
        runner.set_ui_snapshot_surface(UiSnapshotSurface {
            footer_data: std::sync::Arc::new(|| {
                json!({
                    "gitBranch": "main",
                    "extensionStatuses": {},
                    "availableProviderCount": 3
                })
            }),
            editor_text: std::sync::Arc::new(|| "draft text".to_string()),
            tools_expanded: std::sync::Arc::new(|| true),
        });
        runner
            .emit(&json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        let requests = sink.requests();
        assert_eq!(requests.len(), 4, "{requests:?}");
        assert_eq!(requests[0]["method"], "setFooter");
        let handle = requests[0]["handle"].as_u64().expect("adopted") as u32;
        assert_eq!(
            runner.component_render(handle, 20).expect("render"),
            ["FOOTER main/3"]
        );
        assert_eq!(requests[1]["method"], "setHeader");
        assert!(requests[1]["handle"].is_u64());
        assert_eq!(requests[2]["method"], "setFooter");
        assert!(requests[2].get("handle").is_none(), "restore is bare");
        assert_eq!(requests[3]["method"], "setHeader");
        assert!(requests[3].get("handle").is_none());
        assert_eq!(
            entries_of(&runner),
            vec![
                json!({ "type": "editor", "data": "draft text" }),
                json!({ "type": "expanded", "data": "true" }),
            ]
        );
    }

    /// `onTerminalInput` (`interactive-mode.ts:2732-2741` + the fold,
    /// `tui.ts:1095-1108`): handlers register in order, the count syncs to
    /// the mode, the fold chains transforms and consume, the unsubscribe
    /// drops one handler, and the reset clears the rest.
    #[test]
    fn ui_terminal_input_folds_through_registered_handlers() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", async (event, ctx) => {
    globalThis.__unsub = [
      ctx.ui.onTerminalInput((data) => (data === "a" ? { data: "b" } : undefined)),
      ctx.ui.onTerminalInput((data) => (data === "b" ? { consume: true } : undefined)),
    ];
  });
  pi.on("session_shutdown", async () => {
    const unsubscribe = globalThis.__unsub.pop();
    if (unsubscribe) unsubscribe();
  });
}
"#;
        let mut runner = runner_with(&[EXT]);
        let sink = FakeSink::scripted(&[]);
        runner.set_ui_context(sink.clone(), "tui");
        runner
            .emit(&json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        let requests = sink.requests();
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert_eq!(requests[0]["method"], "terminalInputListeners");
        assert_eq!(requests[0]["count"], 1);
        assert_eq!(requests[1]["count"], 2);

        // The fold chains the transform into the consumer.
        assert_eq!(
            runner.deliver_terminal_input("a").expect("fold"),
            None,
            "the transform fed the consumer, which consumed"
        );
        assert_eq!(
            runner.deliver_terminal_input("x").expect("fold"),
            Some("x".to_string()),
            "both listeners pass it through"
        );

        // The unsubscribe drops the consumer; the transform survives.
        runner
            .emit(&json!({ "type": "session_shutdown" }))
            .expect("emit");
        assert_eq!(
            runner.deliver_terminal_input("a").expect("fold"),
            Some("b".to_string())
        );

        // The reset clears what is left.
        runner.clear_terminal_input_handlers().expect("clear");
        assert_eq!(
            runner.deliver_terminal_input("a").expect("fold"),
            Some("a".to_string())
        );
    }

    /// The answer-class handle reads (`isHidden`/`isFocused`/`getBounds`)
    /// ride the deferred path: the request comes back from
    /// [`ExtensionRunner::service_ui_requests`], the delivery thread
    /// forwards it and waits WITHOUT the runner mutex, and the completion
    /// resolves the glue deferred without ending a `ui_prompt` pair
    /// (E3 slice 6c).
    #[test]
    fn ui_overlay_answer_class_rides_the_deferred_path_without_a_prompt() {
        let mut runner = runner_with(&[EXT_UI_PROMPTS]);
        // A scripted sink whose per-method answer carries the mode's reply.
        let sink = FakeSink::scripted(&[("overlayIsHidden", json!({ "hidden": true }))]);
        runner.set_ui_context(sink.clone(), "tui");

        // The glue queues the read under its crossing id; the deferred
        // registration happens glue-side (`__piUiPending`).
        runner
            .core()
            .lock()
            .expect("core")
            .ui_requests
            .push(json!({ "method": "overlayIsHidden", "id": "ui-7" }));
        let deferred = runner.service_ui_requests().expect("service");
        assert_eq!(deferred.len(), 1, "the read waits for the mode");
        // Forwarded only on the delivery thread's `begin_ui_dialog`.
        assert!(sink.requests().is_empty());

        let runner = std::sync::Arc::new(std::sync::Mutex::new(runner));
        let worker_runner = std::sync::Arc::clone(&runner);
        let worker = std::thread::spawn(move || {
            let wait = worker_runner
                .lock()
                .expect("runner")
                .begin_ui_dialog(&json!({ "method": "overlayIsHidden", "id": "ui-7" }))
                .expect("begin")
                .expect("a wait");
            let response = wait.wait();
            worker_runner
                .lock()
                .expect("runner")
                .complete_ui_dialog(&wait, &response)
                .expect("complete");
        });
        worker.join().expect("worker");
        // The request crossed and the completion resolved the deferred —
        // with no `ui_prompt` pair around it.
        assert_eq!(sink.requests().len(), 1);
        assert_eq!(sink.requests()[0]["method"], "overlayIsHidden");
        assert_eq!(sink.requests()[0]["id"], "ui-7");
        let entries = {
            let runner = runner.lock().expect("runner");
            entries_of(&runner)
        };
        assert!(
            entries.is_empty(),
            "no ui_prompt pair for an answer crossing: {entries:?}"
        );
    }

    /// An answer-class read made INSIDE a dispatch settles with the method
    /// default instead of blocking the dispatch thread on the mode — the
    /// settle path may BE the mode's pump, and blocking there deadlocks the
    /// UI (Deviation: upstream answers synchronously from the in-process
    /// TUI, `tui.ts:845/:883/:884-887`).
    #[test]
    fn ui_overlay_answer_inside_a_dispatch_settles_with_the_default() {
        let mut runner = runner_with(&[EXT_UI_PROMPTS]);
        let sink = FakeSink::scripted(&[]);
        runner.set_ui_context(sink.clone(), "tui");
        // The settle path (`deliver_ui_request`) must answer at once and
        // must not cross the request toward the mode.
        runner
            .deliver_ui_request(&json!({ "method": "overlayGetBounds", "id": "ui-9" }))
            .expect("deliver");
        runner
            .deliver_ui_request(&json!({ "method": "overlayIsFocused", "id": "ui-10" }))
            .expect("deliver");
        runner
            .deliver_ui_request(&json!({ "method": "overlayIsHidden", "id": "ui-11" }))
            .expect("deliver");
        assert!(
            sink.requests().is_empty(),
            "the reads never cross on the settle path"
        );
        let entries = entries_of(&runner);
        assert!(entries.is_empty(), "no prompt pair: {entries:?}");
    }

    /// A sink whose dialog answers block on a gate — for asserting that a
    /// split-path wait ([`UiDialogWait`]) holds no runner mutex.
    struct GateSink {
        requests: Mutex<Vec<Json>>,
        gate: std::sync::Arc<(Mutex<bool>, std::sync::Condvar)>,
    }

    impl UiSink for GateSink {
        fn request(&self, request: &Json) {
            self.requests
                .lock()
                .expect("requests")
                .push(request.clone());
        }

        fn wait_response(&self, _id: &str, _method: &str) -> Json {
            let (flag, cv) = &*self.gate;
            let mut open = flag.lock().expect("gate");
            while !*open {
                open = cv.wait(open).expect("gate");
            }
            json!({ "value": "a" })
        }
    }

    const EXT_UI_PROMPTS: &str = r#"
export default function (pi) {
  pi.on("ui_prompt_start", (event) => pi.appendEntry("prompt-start", event.kind));
  pi.on("ui_prompt_end", (event) => pi.appendEntry("prompt-end", event.kind));
}
"#;

    #[test]
    fn ui_dialog_wait_releases_the_runner_mutex() {
        let mut runner = runner_with(&[EXT_UI_PROMPTS]);
        let gate: std::sync::Arc<(Mutex<bool>, std::sync::Condvar)> =
            std::sync::Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let sink = std::sync::Arc::new(GateSink {
            requests: Mutex::new(Vec::new()),
            gate: gate.clone(),
        });
        runner.set_ui_context(sink.clone(), "tui");

        // A dialog request queued OUTSIDE a dispatch rides the split path:
        // service_ui_requests defers it, the delivery thread begins under
        // a guard and waits WITHOUT one.
        let request = json!({
            "id": "ui-1",
            "method": "select",
            "title": "Pick",
            "options": ["a", "b"],
        });
        runner
            .core()
            .lock()
            .expect("core")
            .ui_requests
            .push(request);
        let deferred = runner.service_ui_requests().expect("service");
        assert_eq!(deferred.len(), 1);

        let runner = std::sync::Arc::new(std::sync::Mutex::new(runner));
        let worker_runner = std::sync::Arc::clone(&runner);
        let worker = std::thread::spawn(move || {
            let wait = {
                let mut guard = worker_runner
                    .lock()
                    .expect("extension runner mutex poisoned");
                guard
                    .begin_ui_dialog(&deferred[0])
                    .expect("begin")
                    .expect("wait handle")
            };
            let response = wait.wait();
            let mut guard = worker_runner
                .lock()
                .expect("extension runner mutex poisoned");
            guard
                .complete_ui_dialog(&wait, &response)
                .expect("complete");
        });

        // The worker is blocked in wait_response; the runner mutex must be
        // free — this lock would hang if the wait held it.
        {
            let _guard = runner.lock().expect("mutex free during the wait");
        }
        let (flag, cv) = &*gate;
        *flag.lock().expect("gate") = true;
        cv.notify_all();
        worker.join().expect("worker");

        // The pair opened at begin and closed at complete; the completion
        // resolved the (unknown-id) deferred harmlessly.
        assert_eq!(sink.requests.lock().expect("requests").len(), 1);
        assert_eq!(
            entries_of(&runner.lock().expect("runner")),
            vec![
                json!({ "type": "prompt-start", "data": "select" }),
                json!({ "type": "prompt-end", "data": "select" }),
            ]
        );

        // A second split round re-emits the pair — the depth did not leak.
        let second = json!({ "id": "ui-2", "method": "confirm", "title": "Sure?" });
        let wait = runner
            .lock()
            .expect("runner")
            .begin_ui_dialog(&second)
            .expect("begin")
            .expect("wait handle");
        let response = wait.wait();
        runner
            .lock()
            .expect("runner")
            .complete_ui_dialog(&wait, &response)
            .expect("complete");
        assert_eq!(
            entries_of(&runner.lock().expect("runner")),
            vec![
                json!({ "type": "prompt-start", "data": "select" }),
                json!({ "type": "prompt-end", "data": "select" }),
                json!({ "type": "prompt-start", "data": "confirm" }),
                json!({ "type": "prompt-end", "data": "confirm" }),
            ]
        );
    }

    const EXT_UI_FIRE: &str = r#"
export default function (pi) {
  pi.on("session_start", (event, ctx) => {
    ctx.ui.notify("hello", "warning");
    ctx.ui.setStatus("k", "v");
    ctx.ui.setWidget("w", ["l1", "l2"], { placement: "belowEditor" });
    ctx.ui.setWidget("w2", ["x"]);
    ctx.ui.setWidget("w3", () => "component");
    ctx.ui.setTitle("t");
    ctx.ui.setEditorText("e");
  });
}
"#;

    #[test]
    fn ui_fire_and_forget_requests() {
        let mut runner = runner_with(&[EXT_UI_FIRE]);
        let sink = FakeSink::scripted(&[]);
        runner.set_ui_context(sink.clone(), "rpc");
        runner
            .emit(&json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        let requests = sink.requests();
        assert_eq!(requests.len(), 6); // the component-factory widget is ignored
        assert_eq!(
            requests[0],
            json!({ "id": requests[0]["id"], "method": "notify", "message": "hello", "notifyType": "warning" })
        );
        assert_eq!(
            requests[1],
            json!({ "id": requests[1]["id"], "method": "setStatus", "statusKey": "k", "statusText": "v" })
        );
        assert_eq!(
            requests[2],
            json!({ "id": requests[2]["id"], "method": "setWidget", "widgetKey": "w", "widgetLines": ["l1", "l2"], "widgetPlacement": "belowEditor" })
        );
        assert_eq!(
            requests[3],
            json!({ "id": requests[3]["id"], "method": "setWidget", "widgetKey": "w2", "widgetLines": ["x"] })
        );
        assert_eq!(
            requests[4],
            json!({ "id": requests[4]["id"], "method": "setTitle", "title": "t" })
        );
        assert_eq!(
            requests[5],
            json!({ "id": requests[5]["id"], "method": "set_editor_text", "text": "e" })
        );
    }

    const EXT_UI_ANSWERS: &str = r#"
export default function (pi) {
  pi.on("session_start", async (event, ctx) => {
    pi.appendEntry("s", String(await ctx.ui.select("T", ["x"], { timeout: 1000 })));
    pi.appendEntry("c", String(await ctx.ui.confirm("T2", "m")));
    pi.appendEntry("i", String(await ctx.ui.input("T3", "ph")));
  });
}
"#;

    #[test]
    fn ui_dialog_timeout_default_is_the_method_default() {
        let mut runner = runner_with(&[EXT_UI_ANSWERS]);
        // `{}` is what the sink resolves on a timeout: select/input fall
        // to undefined, confirm to false.
        let sink = FakeSink::scripted(&[]);
        runner.set_ui_context(sink.clone(), "rpc");
        runner
            .emit(&json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        assert_eq!(
            entries_of(&runner),
            vec![
                json!({ "type": "s", "data": "undefined" }),
                json!({ "type": "c", "data": "false" }),
                json!({ "type": "i", "data": "undefined" }),
            ]
        );
        let requests = sink.requests();
        assert_eq!(requests[0]["timeout"], 1000);
        assert_eq!(requests[2]["placeholder"], "ph");
    }

    #[test]
    fn ui_dialog_cancel_and_value_responses() {
        let mut runner = runner_with(&[EXT_UI_ANSWERS]);
        let sink = FakeSink::scripted(&[
            ("select", json!({ "cancelled": true })),
            ("confirm", json!({ "confirmed": true })),
            ("input", json!({ "value": "typed" })),
        ]);
        runner.set_ui_context(sink.clone(), "rpc");
        runner
            .emit(&json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        assert_eq!(
            entries_of(&runner),
            vec![
                json!({ "type": "s", "data": "undefined" }),
                json!({ "type": "c", "data": "true" }),
                json!({ "type": "i", "data": "typed" }),
            ]
        );
    }

    const EXT_UI_NESTED: &str = r#"
export default function (pi) {
  pi.on("session_start", async (event, ctx) => {
    const choice = await ctx.ui.select("Outer", ["a"]);
    pi.appendEntry("choice", choice);
  });
  pi.on("ui_prompt_start", async (event, ctx) => {
    pi.appendEntry("prompt-start", event.kind);
    if (event.kind === "select") {
      const ok = await ctx.ui.confirm("Inner", "sure?");
      pi.appendEntry("inner", ok === true);
    }
  });
  pi.on("ui_prompt_end", (event) => pi.appendEntry("prompt-end", event.kind));
}
"#;

    #[test]
    fn ui_prompt_pair_nests_to_the_outermost_dialog() {
        let mut runner = runner_with(&[EXT_UI_NESTED]);
        let sink = FakeSink::scripted(&[
            ("select", json!({ "value": "a" })),
            ("confirm", json!({ "confirmed": true })),
        ]);
        runner.set_ui_context(sink.clone(), "rpc");
        runner
            .emit(&json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        // The nested confirm (inside the start listener) emits no pair:
        // only the outermost select does.
        assert_eq!(
            entries_of(&runner),
            vec![
                json!({ "type": "prompt-start", "data": "select" }),
                json!({ "type": "inner", "data": true }),
                json!({ "type": "prompt-end", "data": "select" }),
                json!({ "type": "choice", "data": "a" }),
            ]
        );
    }

    const EXT_UI_LIMITS: &str = r#"
export default function (pi) {
  pi.on("session_start", async (event, ctx) => {
    pi.appendEntry("editorText", JSON.stringify(ctx.ui.getEditorText()));
    pi.appendEntry("custom", String(await ctx.ui.custom(() => {})));
    pi.appendEntry("theme", JSON.stringify(ctx.ui.setTheme("dark")));
    pi.appendEntry("themes", JSON.stringify(ctx.ui.getAllThemes()));
    pi.appendEntry("expanded", JSON.stringify(ctx.ui.getToolsExpanded()));
    pi.appendEntry("hasUI", String(ctx.hasUI));
    pi.appendEntry("mode", ctx.mode);
  });
}
"#;

    #[test]
    fn ui_live_limitations_match_the_rpc_doc() {
        let mut runner = runner_with(&[EXT_UI_LIMITS]);
        let sink = FakeSink::scripted(&[]);
        runner.set_ui_context(sink.clone(), "rpc");
        runner
            .emit(&json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        assert_eq!(
            entries_of(&runner),
            vec![
                json!({ "type": "editorText", "data": "\"\"" }),
                json!({ "type": "custom", "data": "undefined" }),
                json!({ "type": "theme", "data": "{\"success\":false,\"error\":\"Theme switching not supported in RPC mode\"}" }),
                json!({ "type": "themes", "data": "[]" }),
                json!({ "type": "expanded", "data": "false" }),
                json!({ "type": "hasUI", "data": "true" }),
                json!({ "type": "mode", "data": "rpc" }),
            ]
        );
    }

    #[test]
    fn ui_headless_matrix_unchanged_without_a_sink() {
        let mut runner = runner_with(&[EXT_UI_LIMITS]);
        runner
            .emit(&json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        assert_eq!(
            entries_of(&runner),
            vec![
                json!({ "type": "editorText", "data": "\"\"" }),
                json!({ "type": "custom", "data": "undefined" }),
                json!({ "type": "theme", "data": "{\"success\":false,\"error\":\"UI not available\"}" }),
                json!({ "type": "themes", "data": "[]" }),
                json!({ "type": "expanded", "data": "false" }),
                json!({ "type": "hasUI", "data": "false" }),
                json!({ "type": "mode", "data": "print" }),
            ]
        );
        // Notify still reaches the headless log.
        assert_eq!(runner.core().lock().expect("core").ui_log.len(), 0);
    }

    const EXT_TRUST_UI: &str = r#"
export default function (pi) {
  pi.on("project_trust", async (event, ctx) => {
    const ok = await ctx.ui.confirm("Trust", event.cwd);
    return { trusted: ok ? "yes" : "no" };
  });
}
"#;

    #[test]
    fn trust_context_ui_prompts_in_live_mode() {
        let mut runner = runner_with(&[EXT_TRUST_UI]);
        let sink = FakeSink::scripted(&[("confirm", json!({ "confirmed": true }))]);
        runner.set_ui_context(sink.clone(), "rpc");
        let (result, errors) = runner.emit_project_trust("/tmp").expect("trust");
        assert!(errors.is_empty());
        assert_eq!(result.expect("result").trusted, "yes");
        assert_eq!(sink.requests()[0]["method"], "confirm");
        assert_eq!(sink.requests()[0]["title"], "Trust");
        assert_eq!(sink.requests()[0]["message"], "/tmp");
    }

    // ---- E2 slice 2: user_bash, context, providers ----

    const EXT_BASH_OPS: &str = r#"
export default function (pi) {
  pi.on("user_bash", () => ({
    operations: {
      exec: async (command, cwd, options) => {
        options.onData(command + "|" + cwd + "|" + String(options.timeout) + "|" + String(options.signal === undefined));
        if (command === "boom") throw new Error("exec-failed");
        return { exitCode: 7 };
      },
    },
  }));
}
"#;

    const EXT_BASH_PASS: &str = r#"
export default function (pi) {
  pi.on("user_bash", () => undefined);
}
"#;

    const EXT_BASH_RESULT_ONE: &str = r#"
export default function (pi) {
  pi.on("user_bash", () => ({ result: { output: "blocked", exitCode: 3, cancelled: false, truncated: false } }));
}
"#;

    const EXT_BASH_RESULT_TWO: &str = r#"
export default function (pi) {
  pi.on("user_bash", () => {
    pi.appendEntry("second-ran", true);
    return { operations: { exec: async () => ({ exitCode: 0 }) } };
  });
}
"#;

    const EXT_BASH_INVALID: &str = r#"
export default function (pi) {
  pi.on("user_bash", (event) => {
    if (event.command === "empty") return {};
    if (event.command === "both") return { operations: { exec: async () => ({}) }, result: { output: "", exitCode: 0, cancelled: false, truncated: false } };
    if (event.command === "nonobject") return 42;
    return { result: { output: "ok", exitCode: 0, cancelled: false, truncated: false } };
  });
}
"#;

    const EXT_CONTEXT_APPEND: &str = r#"
export default function (pi) {
  pi.on("context", (event) => ({ messages: [...event.messages, { role: "user", content: "injected" }] }));
  pi.on("context_with_system", (event) => ({ messages: [...event.messages, { role: "user", content: "sys-note" }] }));
}
"#;

    const EXT_CONTEXT_INPLACE: &str = r#"
export default function (pi) {
  pi.on("context", (event) => {
    event.messages[0].content += "[c]";
  });
}
"#;

    const EXT_CONTEXT_THROW: &str = r#"
export default function (pi) {
  pi.on("context", () => { throw new Error("ctx-boom"); });
}
"#;

    const EXT_CONTEXT_DROP: &str = r#"
export default function (pi) {
  pi.on("context", (event) => ({ messages: [...event.messages, { role: "user", content: "injected" }] }));
  pi.on("context_with_system", (event) => ({ messages: event.messages.filter((message) => message.role !== "system") }));
}
"#;

    const EXT_PROVIDERS: &str = r#"
export default function (pi) {
  pi.registerProvider("acme", { baseUrl: "https://acme.test", api: "openai-completions" });
  pi.registerProvider({ id: "native-co", api: "anthropic-messages" });
  pi.registerVirtualModel({ provider: "acme", id: "router", name: "Router", route: {} });
}
"#;

    const EXT_PROVIDERS_MERGE: &str = r#"
export default function (pi) {
  pi.registerProvider("acme", { baseUrl: "https://merged.test" });
  pi.registerVirtualModel({ provider: "acme", id: "router", name: "Router Two", route: {} });
}
"#;

    const EXT_PROVIDERS_UNREG: &str = r#"
export default function (pi) {
  pi.unregisterProvider("acme");
  pi.unregisterVirtualModel("acme", "router");
}
"#;

    const EXT_PROVIDER_ERR: &str = r#"
export default function (pi) {
  const caught = [];
  const attempt = (run) => {
    try { run(); } catch (error) { caught.push(error.message); }
  };
  attempt(() => pi.registerProvider("acme"));
  attempt(() => pi.registerProvider({ id: "  " }));
  attempt(() => pi.registerVirtualModel({ provider: "acme", id: "" }));
  // appendEntry is an action method: it throws until the runner binds
  // (loader.ts:179), so the collected messages are appended post-bind.
  pi.on("session_start", () => pi.appendEntry("provider-errors", caught));
}
"#;

    #[test]
    fn user_bash_operations_bridge_streams_and_forwards_options() {
        let mut runner = runner_with(&[EXT_BASH_OPS]);
        let decision = runner
            .emit_user_bash("git status", false, "/tmp")
            .expect("emit");
        assert!(matches!(decision, Some(UserBashDecision::Operations)));
        let (result, output) = runner
            .call_bash_exec("git status", "/tmp", &json!({ "timeout": 1000 }))
            .expect("exec");
        assert_eq!(result, json!({ "exitCode": 7 }));
        assert_eq!(output, vec!["git status|/tmp|1000|true".to_string()]);
        // An exec that throws degrades to `{error}`; chunks streamed
        // before the throw still land.
        let (result, output) = runner
            .call_bash_exec("boom", "/tmp", &json!({}))
            .expect("exec");
        assert_eq!(result, json!({ "error": "exec-failed" }));
        assert_eq!(output, vec!["boom|/tmp|undefined|true".to_string()]);
    }

    #[test]
    fn user_bash_falls_through_to_local_execution() {
        let mut runner = runner_with(&[EXT_BASH_PASS]);
        assert!(
            runner
                .emit_user_bash("ls", false, "/tmp")
                .expect("emit")
                .is_none()
        );
        // No handlers at all: the same fall-through.
        let mut runner = runner_with(&[]);
        assert!(
            runner
                .emit_user_bash("ls", false, "/tmp")
                .expect("emit")
                .is_none()
        );
    }

    #[test]
    fn user_bash_result_wins_before_later_handlers() {
        let mut runner = runner_with(&[EXT_BASH_RESULT_ONE, EXT_BASH_RESULT_TWO]);
        let decision = runner.emit_user_bash("make", false, "/tmp").expect("emit");
        match decision {
            Some(UserBashDecision::Result(result)) => assert_eq!(
                result,
                json!({ "output": "blocked", "exitCode": 3, "cancelled": false, "truncated": false })
            ),
            other => panic!("expected a result decision, got {other:?}"),
        }
        // The losing handler never ran.
        assert!(entries_of(&runner).is_empty());
    }

    #[test]
    fn user_bash_invalid_result_propagates_exact_message() {
        const INVALID: &str = "Invalid user_bash handler result: return undefined for local execution or exactly one valid { operations } or { result } object";
        let mut runner = runner_with(&[EXT_BASH_INVALID]);
        let errors = errors_of(&mut runner);
        for command in ["empty", "both", "nonobject"] {
            let error = runner
                .emit_user_bash(command, false, "/tmp")
                .expect_err("invalid results propagate");
            assert_eq!(error.to_string(), INVALID);
        }
        let errors = errors.lock().expect("sink");
        assert_eq!(errors.len(), 3);
        for error in errors.iter() {
            assert_eq!(error.extension_path, "<test-0>");
            assert_eq!(error.event, "user_bash");
            assert_eq!(error.error, INVALID);
        }
        // The valid branch still resolves after the failures.
        let decision = runner.emit_user_bash("make", false, "/tmp").expect("emit");
        assert!(matches!(decision, Some(UserBashDecision::Result(_))));
    }

    #[test]
    fn context_returns_apply_and_restore_system() {
        let mut runner = runner_with(&[EXT_CONTEXT_APPEND]);
        let messages = vec![
            json!({ "role": "system", "content": "s1", "timestamp": 5 }),
            json!({ "role": "user", "content": "u1" }),
        ];
        let result = runner.emit_context(&messages).expect("emit");
        // Phase 1 rewrote the conversation, so the replayed system head
        // comes back on top; phase 2 appends to the full transcript.
        assert_eq!(
            result,
            vec![
                json!({ "role": "system", "content": "s1", "timestamp": 5 }),
                json!({ "role": "user", "content": "u1" }),
                json!({ "role": "user", "content": "injected" }),
                json!({ "role": "user", "content": "sys-note" }),
            ]
        );
    }

    #[test]
    fn context_inplace_mutation_applies_and_restores_system() {
        let mut runner = runner_with(&[EXT_CONTEXT_INPLACE]);
        let messages = vec![
            json!({ "role": "system", "content": "s1", "timestamp": 5 }),
            json!({ "role": "user", "content": "u1" }),
        ];
        let result = runner.emit_context(&messages).expect("emit");
        // In-place edits to the live event.messages apply without a
        // `{messages}` return, and still get the replayed system head.
        assert_eq!(
            result,
            vec![
                json!({ "role": "system", "content": "s1", "timestamp": 5 }),
                json!({ "role": "user", "content": "u1[c]" }),
            ]
        );
    }

    #[test]
    fn context_handler_errors_are_reported_and_skipped() {
        let mut runner = runner_with(&[EXT_CONTEXT_THROW, EXT_CONTEXT_DROP]);
        let errors = errors_of(&mut runner);
        let messages = vec![
            json!({ "role": "system", "content": "s1", "timestamp": 5 }),
            json!({ "role": "user", "content": "u1" }),
        ];
        let result = runner.emit_context(&messages).expect("emit");
        // Phase 1: the throwing handler is skipped and the appending one
        // still applies. Phase 2: dropping the leading system message is
        // reported but the handler's output is honored.
        assert_eq!(
            result,
            vec![
                json!({ "role": "user", "content": "u1" }),
                json!({ "role": "user", "content": "injected" }),
            ]
        );
        let errors = errors.lock().expect("sink");
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].extension_path, "<test-0>");
        assert_eq!(errors[0].event, "context");
        assert_eq!(errors[0].error, "ctx-boom");
        assert_eq!(errors[1].extension_path, "<test-1>");
        assert_eq!(errors[1].event, "context_with_system");
        assert_eq!(
            errors[1].error,
            "Handler removed the leading system message; the request has no prompt or initial tool declarations. Keep it at index 0 or replace a dropped prefix with getCurrentSystemMessage()."
        );
    }

    /// A virtual model whose router answers from the request: it picks the
    /// target by the state, mirrors the thinking level, throws on demand,
    /// and can be async.
    const EXT_VIRTUAL_ROUTER: &str = r#"
export default function (pi) {
  pi.registerVirtualModel({
    provider: "acme",
    id: "router",
    name: "Router",
    route: (request) => {
      if (request.reason === "boom") throw new Error("router exploded");
      if (request.reason === "async") return Promise.resolve({
        model: { provider: "anthropic", id: "claude-async", api: "anthropic-messages" },
        thinkingLevel: "low",
      });
      return {
        model: {
          provider: "anthropic",
          id: request.state === "pro" ? "claude-pro" : "claude-x",
          api: "anthropic-messages",
        },
        thinkingLevel: request.thinkingLevel,
        state: request.reason === "user" ? { picked: request.messages.length } : request.state,
      };
    },
  });
}
"#;

    /// A factory that registers and then dies: the registration must die
    /// with the record (the route function is stashed JS-side, so the
    /// discard has to clean it).
    const EXT_VIRTUAL_FAIL: &str = r#"
export default function (pi) {
  pi.registerVirtualModel({ provider: "acme", id: "ghost", name: "Ghost", route: () => ({}) });
  throw new Error("factory died");
}
"#;

    fn exception_of(error: JsHostError) -> String {
        match error {
            JsHostError::JsException(message) => message,
            other => panic!("expected a JS exception, got {other:?}"),
        }
    }

    #[test]
    fn route_model_invokes_the_stashed_route_function() {
        let mut runner = runner_with(&[EXT_VIRTUAL_ROUTER]);
        // The whole request the host built crosses as the session event.
        let reply = runner
            .route_model(
                "acme",
                "router",
                &json!({
                    "reason": "user",
                    "thinkingLevel": "high",
                    "messages": [{ "role": "user", "content": "hi" }, { "role": "assistant", "content": "hey" }],
                }),
            )
            .expect("route")
            .expect("route reply");
        assert_eq!(
            reply.get("model"),
            Some(
                &json!({ "provider": "anthropic", "id": "claude-x", "api": "anthropic-messages" })
            )
        );
        assert_eq!(reply.get("thinkingLevel"), Some(&json!("high")));
        // The router's new state rides the reply.
        assert_eq!(reply.get("state"), Some(&json!({ "picked": 2 })));
    }

    #[test]
    fn route_model_round_trips_state_and_async_routers_settle() {
        let mut runner = runner_with(&[EXT_VIRTUAL_ROUTER]);
        // Continuation with state: the router echoes it back unchanged.
        let reply = runner
            .route_model(
                "acme",
                "router",
                &json!({ "reason": "continuation", "thinkingLevel": "off", "state": "pro" }),
            )
            .expect("route")
            .expect("route reply");
        assert_eq!(
            reply.get("model"),
            Some(
                &json!({ "provider": "anthropic", "id": "claude-pro", "api": "anthropic-messages" })
            )
        );
        assert_eq!(reply.get("state"), Some(&json!("pro")));
        // An async router settles through the job queue before the slot is
        // read.
        let reply = runner
            .route_model("acme", "router", &json!({ "reason": "async" }))
            .expect("route")
            .expect("route reply");
        assert_eq!(
            reply.get("model"),
            Some(
                &json!({ "provider": "anthropic", "id": "claude-async", "api": "anthropic-messages" })
            )
        );
    }

    #[test]
    fn route_model_reports_router_errors_and_unregistered_models() {
        let mut runner = runner_with(&[EXT_VIRTUAL_ROUTER]);
        let message = exception_of(
            runner
                .route_model("acme", "router", &json!({ "reason": "boom" }))
                .expect_err("router throws"),
        );
        assert_eq!(message, "router exploded");
        // No stash entry: the glue answers with the TS error message.
        let message = exception_of(
            runner
                .route_model("acme", "missing", &json!({}))
                .expect_err("not registered"),
        );
        assert_eq!(message, "Virtual model acme/missing is not registered.");
    }

    #[test]
    fn route_model_of_a_failed_extension_is_not_registered() {
        let mut runner = ExtensionRunner::new(HostCore {
            cwd: "/tmp".to_string(),
            mode: "print".to_string(),
            settings: json!({}),
            ..Default::default()
        })
        .expect("runner");
        assert!(
            runner
                .load_extension("<test-0>", Some(EXT_VIRTUAL_FAIL))
                .is_err()
        );
        runner.bind().expect("bind");
        let message = exception_of(
            runner
                .route_model("acme", "ghost", &json!({}))
                .expect_err("discard cleaned the stash"),
        );
        assert_eq!(message, "Virtual model acme/ghost is not registered.");
    }

    /// A provider whose `streamSimple` handler echoes the marshalled
    /// request into its events: sync yields, then an async leg, then the
    /// terminal `done` (the protocol shape an async-generator handler
    /// produces).
    const EXT_PROVIDER_STREAM: &str = r#"
export default function (pi) {
  pi.registerProvider("acme", {
    api: "acme-api",
    baseUrl: "https://acme.test/v1",
    apiKey: "$ACME_KEY",
    models: [{ id: "acme-1", name: "Acme One", reasoning: true, input: ["text"], contextWindow: 90000, maxTokens: 4096 }],
    streamSimple: async function* (model, context, options) {
      yield { type: "text_start", contentIndex: 0, partial: null };
      yield { type: "text_delta", contentIndex: 0, delta: `${model.id}|${context.messages.length}|${options.reasoning}|${options.apiKey}` };
      await Promise.resolve();
      yield { type: "done", reason: "stop", message: { role: "assistant", api: model.api, provider: model.provider, model: model.id, content: [] } };
    },
  });
}
"#;

    /// A handler that streams one event and then throws: the events before
    /// the throw still cross (upstream's lazyStream forwards until the
    /// failure, then appends the terminal error event).
    const EXT_PROVIDER_STREAM_THROW: &str = r#"
export default function (pi) {
  pi.registerProvider("acme", {
    api: "acme-api",
    baseUrl: "https://acme.test/v1",
    streamSimple: async function* () {
      yield { type: "text_delta", contentIndex: 0, delta: "first" };
      throw new Error("stream-boom");
    },
  });
}
"#;

    /// A factory that registers a streamSimple provider and then dies: the
    /// stashed handler must die with the record (the discard has to clean
    /// it, like the route-function stash).
    const EXT_PROVIDER_STREAM_FAIL: &str = r#"
export default function (pi) {
  pi.registerProvider("acme", { api: "acme-api", baseUrl: "https://x", streamSimple: () => {} });
  throw new Error("factory died");
}
"#;

    /// streamSimple without an api: `validateExtensionProvider` throws
    /// into the extension before anything is stored
    /// (provider-composer.ts:544-548). appendEntry is an action method, so
    /// the caught message is appended post-bind.
    const EXT_PROVIDER_STREAM_NO_API: &str = r#"
export default function (pi) {
  const caught = [];
  try {
    pi.registerProvider("acme", { streamSimple: () => {} });
    caught.push("no-throw");
  } catch (error) {
    caught.push(error.message);
  }
  pi.on("session_start", () => pi.appendEntry("stream-errors", caught));
}
"#;

    #[test]
    fn provider_stream_drains_the_stashed_handler_events() {
        let mut runner = runner_with(&[EXT_PROVIDER_STREAM]);
        // The whole marshalled request crosses as the session event; the
        // handler reads model/context/options off it.
        let outcome = runner
            .provider_stream(
                "acme",
                &json!({
                    "model": { "id": "acme-1", "name": "Acme One", "api": "acme-api", "provider": "acme",
                               "baseUrl": "https://acme.test/v1", "reasoning": true, "input": ["text"],
                               "contextWindow": 90000, "maxTokens": 4096 },
                    "context": { "messages": [{ "role": "user", "content": "hi" }] },
                    "options": { "reasoning": "high", "apiKey": "sk-acme" },
                }),
                None,
            )
            .expect("stream");
        assert!(outcome.error.is_none());
        assert_eq!(
            outcome.events,
            vec![
                json!({ "type": "text_start", "contentIndex": 0, "partial": null }),
                json!({ "type": "text_delta", "contentIndex": 0, "delta": "acme-1|1|high|sk-acme" }),
                json!({ "type": "done", "reason": "stop", "message": { "role": "assistant", "api": "acme-api", "provider": "acme", "model": "acme-1", "content": [] } }),
            ]
        );
    }

    #[test]
    fn provider_stream_handler_throw_keeps_the_events_already_emitted() {
        let mut runner = runner_with(&[EXT_PROVIDER_STREAM_THROW]);
        let outcome = runner
            .provider_stream(
                "acme",
                &json!({ "model": {}, "context": {}, "options": {} }),
                None,
            )
            .expect("stream");
        assert_eq!(
            outcome.events,
            vec![json!({ "type": "text_delta", "contentIndex": 0, "delta": "first" })]
        );
        assert_eq!(outcome.error.as_deref(), Some("stream-boom"));
    }

    #[test]
    fn provider_stream_of_an_unregistered_provider_answers_the_glue_error() {
        let mut runner = runner_with(&[]);
        // No stash entry: the glue settles the session with the
        // not-registered error (the route-glue's unregistered branch
        // shape), which the runner reports as the outcome error.
        let outcome = runner
            .provider_stream("acme", &json!({}), None)
            .expect("settled");
        assert_eq!(outcome.events, Vec::<Json>::new());
        assert_eq!(
            outcome.error.as_deref(),
            Some("Provider acme is not registered with a streamSimple handler.")
        );
    }

    #[test]
    fn provider_stream_stash_dies_with_a_failed_extension() {
        let mut runner = ExtensionRunner::new(HostCore {
            cwd: "/tmp".to_string(),
            mode: "print".to_string(),
            settings: json!({}),
            ..Default::default()
        })
        .expect("runner");
        assert!(
            runner
                .load_extension("<test-0>", Some(EXT_PROVIDER_STREAM_FAIL))
                .is_err()
        );
        runner.bind().expect("bind");
        // The discard removed the stash entry, so the glue answers with the
        // not-registered error even though a live registration could have
        // survived.
        let outcome = runner
            .provider_stream("acme", &json!({}), None)
            .expect("settled");
        assert_eq!(
            outcome.error.as_deref(),
            Some("Provider acme is not registered with a streamSimple handler.")
        );
    }

    #[test]
    fn provider_stream_api_tracks_the_stash_marker_through_merges() {
        let runner = runner_with(&[EXT_PROVIDER_STREAM]);
        // The registered api is what `streamWith` matches model.api
        // against (`model.api === extension.api`).
        assert_eq!(
            runner.provider_stream_api("acme").as_deref(),
            Some("acme-api")
        );
        assert_eq!(runner.provider_stream_api("missing"), None);
        // A re-registration without the streamSimple key keeps the
        // previous handler, like the per-key config merge.
        let runner = runner_with(&[EXT_PROVIDER_STREAM, EXT_PROVIDERS_MERGE]);
        assert_eq!(
            runner.provider_stream_api("acme").as_deref(),
            Some("acme-api")
        );
        // Unregistering removes the whole config, marker included.
        let runner = runner_with(&[EXT_PROVIDER_STREAM, EXT_PROVIDERS_UNREG]);
        assert_eq!(runner.provider_stream_api("acme"), None);
    }

    #[test]
    fn provider_stream_without_an_api_throws_the_upstream_validation() {
        let mut runner = runner_with(&[EXT_PROVIDER_STREAM_NO_API]);
        runner
            .emit(&events::session_start("startup"))
            .expect("emit");
        assert_eq!(
            entries_of(&runner),
            vec![json!({
                "type": "stream-errors",
                "data": ["Provider acme: \"api\" is required when registering streamSimple."],
            })]
        );
    }

    #[test]
    fn provider_registrations_merge_replace_and_unregister() {
        let runner = runner_with(&[EXT_PROVIDERS, EXT_PROVIDERS_MERGE]);
        assert_eq!(
            runner.providers(),
            vec![(
                "acme".to_string(),
                "<test-0>".to_string(),
                json!({ "baseUrl": "https://merged.test", "api": "openai-completions" }),
            )]
        );
        assert_eq!(
            runner.native_providers(),
            vec![(
                json!({ "id": "native-co", "api": "anthropic-messages" }),
                "<test-0>".to_string(),
            )]
        );
        // The same (provider, id) replaces the earlier definition.
        assert_eq!(
            runner.virtual_models(),
            vec![(
                json!({ "provider": "acme", "id": "router", "name": "Router Two", "route": {} }),
                "<test-1>".to_string(),
            )]
        );

        let runner = runner_with(&[EXT_PROVIDERS, EXT_PROVIDERS_UNREG]);
        // Unregistering by name removes the config provider; the native
        // registration survives (its id differs).
        assert!(runner.providers().is_empty());
        assert!(runner.virtual_models().is_empty());
        assert_eq!(
            runner.native_providers(),
            vec![(
                json!({ "id": "native-co", "api": "anthropic-messages" }),
                "<test-0>".to_string(),
            )]
        );
    }

    #[test]
    fn provider_registration_errors_surface_exact_messages() {
        let mut runner = runner_with(&[EXT_PROVIDER_ERR]);
        runner
            .emit(&events::session_start("startup"))
            .expect("emit");
        // The config check throws at load (loader.ts:471); the id
        // validations are bind-time and surface as error reports, not
        // load failures or handler throws (runner.ts:470-521).
        assert_eq!(
            entries_of(&runner),
            vec![json!({
                "type": "provider-errors",
                "data": ["Provider config is required when registering by name"],
            })]
        );
        let errors = runner.provider_registration_errors();
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].extension_path, "<test-0>");
        assert_eq!(errors[0].event, "register_provider");
        assert_eq!(errors[0].error, "Provider id must not be empty.");
        assert_eq!(errors[1].extension_path, "<test-0>");
        assert_eq!(errors[1].event, "register_virtual_model");
        assert_eq!(
            errors[1].error,
            "Virtual model provider and id must not be empty."
        );
    }

    #[test]
    fn notifications_fan_out_in_order_and_isolate_handler_errors() {
        let mut runner = runner_with(&[EXT_ONE, EXT_TWO]);
        let errors = errors_of(&mut runner);
        runner
            .emit(&events::session_start("startup"))
            .expect("emit");
        // Extension one ran (appendEntry works post-bind), extension two
        // threw, and the dispatch continued.
        let entries = runner.core().lock().expect("core").entries.clone();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "saw");
        assert_eq!(entries[0].1, json!("startup"));
        let errors = errors.lock().expect("sink");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].extension_path, "<test-1>");
        assert_eq!(errors[0].event, "session_start");
        assert_eq!(errors[0].error, "boom-two");
    }

    #[test]
    fn session_before_last_result_wins_and_cancel_short_circuits() {
        let mut runner = runner_with(&[EXT_ONE, EXT_TWO]);
        // Extension two's first handler cancels; the second handler (and
        // nothing after it) must not run.
        let result = runner
            .emit_session_before(&events::session_before_switch("new", None))
            .expect("emit");
        assert!(result.cancel);
        // The two extension-one entries from the other tests are absent:
        // no appendEntry happened on this event.
        let entries = runner.core().lock().expect("core").entries.clone();
        assert!(entries.is_empty());
    }

    #[test]
    fn tool_call_mutates_in_place_then_blocks() {
        let mut runner = runner_with(&[EXT_ONE, EXT_TWO]);
        let (result, event) = runner
            .emit_tool_call("t1", "read", &json!({"path": "x"}))
            .expect("emit");
        let result = result.expect("block result");
        assert!(result.block);
        assert_eq!(result.reason.as_deref(), Some("guarded"));
        // The mutation from extension one is in the final event.
        assert_eq!(event["input"]["touched"], json!("one"));
    }

    #[test]
    fn tool_result_chain_applies_patches_in_order() {
        let mut runner = runner_with(&[EXT_ONE, EXT_TWO, EXT_THREE]);
        let event = events::tool_result(
            "t1",
            "read",
            &json!({"path": "x"}),
            &json!([{ "type": "text", "text": "secret" }]),
            false,
            Some(json!({"structured": true})),
        );
        let final_event = runner
            .emit_tool_result(event)
            .expect("emit")
            .expect("modified");
        // Extension three replaced content (dropping structuredContent) and
        // flipped isError.
        assert_eq!(
            final_event["content"],
            json!([{ "type": "text", "text": "redacted" }])
        );
        assert_eq!(final_event["isError"], json!(true));
        assert!(final_event.get("structuredContent").is_none());
    }

    #[test]
    fn input_transform_chain_and_handled_short_circuit() {
        let mut runner = runner_with(&[EXT_ONE]);
        match runner
            .emit_input(events::input("  hello  ", "interactive", None))
            .expect("emit")
        {
            InputResult::Transform { text, images } => {
                assert_eq!(text, "hello");
                assert!(images.is_none());
            }
            other => panic!("expected transform, got {other:?}"),
        }
        // Extension two handles: the chain stops there.
        let mut runner = runner_with(&[EXT_ONE, EXT_TWO]);
        match runner
            .emit_input(events::input("  hello  ", "interactive", None))
            .expect("emit")
        {
            InputResult::Handled { result } => assert_eq!(result["action"], json!("handled")),
            other => panic!("expected handled, got {other:?}"),
        }
    }

    #[test]
    fn before_agent_start_collects_messages_and_chains_overrides() {
        let mut runner = runner_with(&[EXT_ONE, EXT_TWO]);
        let combined = runner
            .emit_before_agent_start(
                &BeforeAgentStartTrigger::User {
                    prompt: "hi".to_string(),
                    images: None,
                },
                json!({ "cwd": "/tmp" }),
                &|options| {
                    options
                        .get("forceSystemPrompt")
                        .and_then(Json::as_str)
                        .unwrap_or("base-prompt")
                        .to_string()
                },
            )
            .expect("emit");
        // Extension one overrode the prompt; extension two observed the
        // override through event.systemPrompt.
        let ui_log = runner.core().lock().expect("core").ui_log.clone();
        assert_eq!(ui_log, vec!["override-one".to_string()]);
        // The message from extension two was collected.
        assert_eq!(combined.messages.len(), 1);
        assert_eq!(combined.messages[0]["customType"], json!("note"));
        // The options carry the chained override.
        assert_eq!(
            combined.system_prompt_options["forceSystemPrompt"],
            json!("override-one")
        );
    }

    #[test]
    fn message_end_rejects_role_changes_and_chains_same_role() {
        let mut runner = runner_with(&[EXT_ONE, EXT_TWO]);
        let errors = errors_of(&mut runner);
        let final_message = runner
            .emit_message_end(json!({ "role": "assistant", "content": "original" }))
            .expect("emit");
        // Extension one patched (same role), extension two's role change
        // was reported and skipped.
        let final_message = final_message.expect("modified");
        assert_eq!(final_message["patched"], json!(true));
        assert_eq!(final_message["role"], json!("assistant"));
        let errors = errors.lock().expect("sink");
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].error,
            "message_end handlers must return a message with the same role"
        );
    }

    #[test]
    fn boundary_threads_entries_and_continue_and_reports_invalid_previews() {
        let mut runner = runner_with(&[EXT_ONE, EXT_TWO]);
        let mut rebuilds = 0;
        let result = runner
            .emit_boundary(
                &json!({ "type": "turn_end", "turnIndex": 0, "message": {}, "toolResults": [], "messageEntryId": "m", "toolResultEntryIds": [] }),
                &mut |_entries| {
                    rebuilds += 1;
                    Ok(json!({ "contextEntries": [], "canContinue": true }))
                },
            )
            .expect("emit");
        // Extension one added its draft and set continue; extension two
        // cleared it. The preview rebuilt after every handler.
        assert!(rebuilds >= 2);
        assert_eq!(result.entries, json!([{ "customType": "draft-one" }]));
        assert!(!result.continue_run);
        assert!(result.valid);
    }

    #[test]
    fn project_trust_first_decided_handler_wins() {
        let mut runner = runner_with(&[EXT_ONE, EXT_TWO]);
        let (result, errors) = runner.emit_project_trust("/work").expect("emit");
        let result = result.expect("decided");
        assert_eq!(result.trusted, "yes");
        assert!(result.remember);
        assert!(errors.is_empty());
    }

    #[test]
    fn commands_get_name_occurrence_with_collision_loop() {
        let runner = runner_with(&[EXT_ONE, EXT_TWO, EXT_THREE]);
        let invocations: Vec<String> = runner
            .resolved_commands()
            .into_iter()
            .map(|command| command.invocation_name)
            .collect();
        // Three `deploy` registrations plus a literal `deploy:2`: the
        // duplicates become deploy:1..3 and the literal `deploy:2` (second
        // in registration order) keeps its name; the third `deploy` dodges
        // to deploy:4 (the upstream resolveRegisteredCommands loop).
        assert_eq!(
            invocations,
            vec!["deploy:1", "deploy:2", "deploy:3", "deploy:4"]
        );
        // Invoking by disambiguated name reaches the right handler:
        // extension two's `deploy` resolved to deploy:3 here.
        let mut runner = runner_with(&[EXT_ONE, EXT_TWO]);
        runner.run_command("deploy:3", "").expect("run");
        let entries = runner.core().lock().expect("core").entries.clone();
        assert_eq!(entries, vec![("ran".to_string(), json!("deploy-two"))]);
    }

    #[test]
    fn flags_are_first_wins_and_cli_values_override() {
        let runner = runner_with(&[EXT_ONE, EXT_TWO]);
        let flags = runner.flags();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].name, "mode");
        // The CLI value overrides the seeded default and reads through
        // pi.getFlag inside a handler (extension one's command appends it).
        runner.set_flag_value("mode", json!("cli"));
        let mut runner = runner_with(&[EXT_ONE]);
        runner.set_flag_value("mode", json!("cli"));
        runner.run_command("deploy:1", "").expect("run");
        let core = runner.core().lock().expect("core");
        let flag_json = serde_json::to_string(&core.flag_values["mode"]).expect("flag");
        assert_eq!(flag_json, r#""cli""#);
    }

    #[test]
    fn tool_execution_uses_first_wins_registration() {
        const EXT_A: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "shared", label: "shared", description: "from-a",
    parameters: { type: "object", properties: {} },
    execute: async () => ({ content: [{ type: "text", text: "a-ran" }], details: {} }),
  });
}
"#;
        const EXT_B: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "shared", label: "shared", description: "from-b",
    parameters: { type: "object", properties: {} },
    execute: async () => ({ content: [{ type: "text", text: "b-ran" }], details: {} }),
  });
}
"#;
        let mut runner = runner_with(&[EXT_A, EXT_B]);
        assert_eq!(runner.tool("shared").expect("tool").description, "from-a");
        let result = runner
            .execute_tool("shared", "t1", &json!({}))
            .expect("execute");
        assert_eq!(result["content"][0]["text"], json!("a-ran"));
        // The active set starts with the declared tools; hidden ones never
        // join, and setActiveTools drops them.
        assert!(runner.active_tools().contains(&"shared".to_string()));
        runner.set_active_tools(vec!["shared".to_string(), "ghost".to_string()]);
        assert_eq!(runner.active_tools(), vec!["shared".to_string()]);
    }

    #[test]
    fn staleness_invalidation_reaches_handlers_through_the_runner() {
        const STALE: &str = r#"
export default function (pi) {
  pi.on("session_start", (event, ctx) => {
    try { ctx.cwd; return "ok"; } catch { return "stale"; }
  });
}
"#;
        let mut runner = runner_with(&[STALE]);
        runner.invalidate(None).expect("invalidate");
        let outcome = runner
            .emit(&events::session_start("startup"))
            .expect("emit");
        // The notification emit returns nothing; the handler's outcome is
        // observable through the entries it would have appended (none).
        assert!(outcome.is_none());
        assert!(runner.core().lock().expect("core").entries.is_empty());
    }

    // ---- Extension shortcuts (E3 slice 1: `pi.registerShortcut`,
    // `getShortcuts` conflict resolution, `run_shortcut` dispatch) ----

    #[test]
    fn shortcuts_resolve_against_builtin_keybindings() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerShortcut("ctrl+d", { description: "shadow exit", handler: () => {} });
  pi.registerShortcut("CTRL+Shift+P", { description: "shadow cycle", handler: () => {} });
  pi.registerShortcut("ctrl+shift+7", { description: "clean", handler: () => {} });
}
"#;
        let mut runner = runner_with(&[EXT]);
        // `app.exit` is in the reserved set — the shortcut is skipped with
        // a warning; `app.message.dequeue` is not — the extension wins with
        // a warning; the clean chord resolves and lowercases. (The runner
        // paths are zero-indexed: `<test-0>`.)
        let resolved: &[(&str, &[&str])] = &[
            ("app.exit", &["ctrl+d"]),
            ("app.message.dequeue", &["ctrl+shift+p"]),
        ];
        runner.get_shortcuts(resolved);
        let shortcuts = runner.shortcuts();
        assert_eq!(shortcuts.len(), 2);
        assert_eq!(shortcuts[0].key, "ctrl+shift+p");
        assert_eq!(shortcuts[0].raw_key, "CTRL+Shift+P");
        assert_eq!(shortcuts[0].description, "shadow cycle");
        assert_eq!(shortcuts[1].key, "ctrl+shift+7");
        assert_eq!(shortcuts[1].extension_path, "<test-0>");
        let diagnostics = runner.shortcut_diagnostics();
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(
            diagnostics[0].message,
            "Extension shortcut 'ctrl+d' from <test-0> conflicts with built-in shortcut. Skipping."
        );
        assert_eq!(
            diagnostics[1].message,
            "Extension shortcut conflict: 'CTRL+Shift+P' is built-in shortcut for app.message.dequeue and <test-0>. Using <test-0>."
        );
    }

    #[test]
    fn duplicate_shortcut_keys_take_the_last_extension() {
        const EXT_A: &str = r#"
export default function (pi) {
  pi.registerShortcut("ctrl+q", { description: "from-a", handler: () => {} });
}
"#;
        const EXT_B: &str = r#"
export default function (pi) {
  pi.registerShortcut("ctrl+q", { description: "from-b", handler: () => {} });
}
"#;
        let mut runner = runner_with(&[EXT_A, EXT_B]);
        runner.get_shortcuts(&[]);
        let shortcuts = runner.shortcuts();
        assert_eq!(shortcuts.len(), 1);
        assert_eq!(shortcuts[0].extension_path, "<test-1>");
        assert_eq!(shortcuts[0].description, "from-b");
        let diagnostics = runner.shortcut_diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].message,
            "Extension shortcut conflict: 'ctrl+q' registered by both <test-0> and <test-1>. Using <test-1>."
        );
    }

    #[test]
    fn run_shortcut_dispatches_the_handler_with_the_base_ctx() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerShortcut("ctrl+shift+7", {
    description: "fire",
    handler: (ctx) => pi.appendEntry("shortcut-fired", {
      hasUi: ctx.hasUI,
      hasWaitForIdle: typeof ctx.waitForIdle === "function",
    }),
  });
  pi.registerShortcut("ctrl+shift+8", { handler: () => { throw new Error("boom"); } });
}
"#;
        let mut runner = runner_with(&[EXT]);
        runner.get_shortcuts(&[]);
        assert!(runner.run_shortcut("ctrl+shift+7").expect("run"));
        let entries = runner.core().lock().expect("core").entries.clone();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "shortcut-fired");
        // The base ctx: `hasUI` reflects the headless runner and the
        // command-only `waitForIdle` addition stays off the shortcut ctx.
        assert_eq!(
            entries[0].1,
            json!({ "hasUi": false, "hasWaitForIdle": false })
        );
        // A handler rejection surfaces through the dispatch outcome — the
        // same channel the mode maps to `Shortcut handler error: …`.
        let error = runner.run_shortcut("ctrl+shift+8");
        assert!(error.is_err(), "{error:?}");
        assert!(error.unwrap_err().to_string().contains("boom"));
        // Unknown chords report not-run.
        assert!(!runner.run_shortcut("ctrl+shift+9").expect("run"));
    }

    // ---- Markdown transformers (E3 slice 2: `pi.registerMarkdownTransformer`,
    // `getMarkdownTransformers` load order, the sync `__piMarkdownTransform`
    // call) ----

    #[test]
    fn markdown_transformers_resolve_in_load_order_with_replacement() {
        const EXT_A: &str = r#"
export default function (pi) {
  pi.registerMarkdownTransformer((markdown) => markdown);
  // A second registration REPLACES the first, no warning
  // (`loader.ts:365-372`), narrowing the surfaces to `summary`.
  pi.registerMarkdownTransformer((markdown) => markdown, { messageTypes: ["summary"] });
}
"#;
        const EXT_B: &str = r"
export default function (pi) {
  pi.registerMarkdownTransformer((markdown) => markdown);
}
";
        const EXT_NONE: &str = r"
export default function (pi) {}
";
        let runner = runner_with(&[EXT_A, EXT_B, EXT_NONE]);
        let resolved = runner.get_markdown_transformers();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].ext_index, 0);
        assert_eq!(resolved[0].extension_path, "<test-0>");
        // The replacement took: only `summary` remains from the first
        // extension; the second keeps the default transcript surfaces.
        assert_eq!(resolved[0].message_types, vec!["summary".to_string()]);
        assert_eq!(
            resolved[1].message_types,
            vec![
                "user".to_string(),
                "assistant".to_string(),
                "assistant-thinking".to_string()
            ]
        );
    }

    #[test]
    fn markdown_transform_runs_and_maps_failures_to_none() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerMarkdownTransformer((markdown, context) => {
    if (markdown === "throw") throw new Error("nope");
    if (markdown === "nonstring") return 42;
    return markdown + "|" + context.messageType + "|" + context.availableWidth +
      (context.isStreaming ? "|streaming" : "") +
      (context.messageId === undefined ? "" : "|id:" + context.messageId) +
      (context.timestamp === undefined ? "" : "|ts:" + context.timestamp);
  });
}
"#;
        let runner = runner_with(&[EXT]);
        // The context crosses as JSON; absent optional fields stay absent.
        let transformed = runner
            .markdown_transform(
                0,
                "hello",
                r#"{"messageType":"assistant","isStreaming":true,"availableWidth":80}"#,
            )
            .expect("transform");
        assert_eq!(transformed, "hello|assistant|80|streaming");
        let with_id = runner
            .markdown_transform(
                0,
                "hello",
                r#"{"messageType":"user","isStreaming":false,"availableWidth":40,"messageId":"m1","timestamp":"2026-01-01"}"#,
            )
            .expect("transform");
        assert_eq!(with_id, "hello|user|40|id:m1|ts:2026-01-01");
        // A transformer throw keeps the current markdown (`markdown-transform.ts:34-36`).
        assert!(
            runner
                .markdown_transform(
                    0,
                    "throw",
                    r#"{"messageType":"user","isStreaming":false,"availableWidth":10}"#
                )
                .is_none()
        );
        // A non-string return is ignored the same way.
        assert!(
            runner
                .markdown_transform(
                    0,
                    "nonstring",
                    r#"{"messageType":"user","isStreaming":false,"availableWidth":10}"#
                )
                .is_none()
        );
    }

    /// The host shares the runner with the agent loop through
    /// `Arc<Mutex<_>>` (E2 slice 3): that only works while the runner stays
    /// `Send` — rquickjs's `parallel` feature makes `Runtime`/`Context`
    /// `Send`, and the error listeners and host actions carry the bound.
    #[test]
    fn runner_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<ExtensionRunner>();
    }

    // ---- Provider-stream options instrumentation (E2 slice 6k:
    // `SimpleStreamOptions` crosses as functions — pi sets the three hooks
    // on every run, `sdk.ts:424-426`; the handler contract is
    // `custom-provider.md:195-198`) ----

    /// A provider whose handler exercises the whole crossed surface: the
    /// `onPayload` round-trip (its return feeds the handler), the one-way
    /// `onResponse`/`onProviderStreamEvent` events, and the `signal`
    /// object. A sibling extension listens to all three bridged events —
    /// the `before_provider_request` handler replaces the payload, the
    /// notifications append entries the test reads back.
    const EXT_PROVIDER_HOOKS: &str = r#"
export default function (pi) {
  pi.on("before_provider_request", (event) => ({ replaced: true, saw: event.payload }));
  pi.on("after_provider_response", (event) => pi.appendEntry("hook-response", { status: event.status, header: event.headers["x-a"] }));
  pi.on("provider_stream_event", (event) => pi.appendEntry("hook-event", { data: event.data, provider: event.provider, api: event.api, model: event.model }));
  pi.registerProvider("acme", {
    api: "acme-api",
    baseUrl: "https://acme.test/v1",
    streamSimple: async function* (model, context, options) {
      const payload = await options.onPayload({ original: true });
      yield { type: "text_delta", contentIndex: 0, delta: `payload:${JSON.stringify(payload)}` };
      options.onResponse({ status: 201, headers: { "x-a": "b" } });
      await options.onProviderStreamEvent?.({ raw: "chunk" }, model);
      yield { type: "done", reason: "stop", message: { role: "assistant", api: model.api, provider: model.provider, model: model.id, content: [] } };
    },
  });
}
"#;

    #[test]
    fn provider_stream_options_carry_the_instrumentation() {
        let mut runner = runner_with(&[EXT_PROVIDER_HOOKS]);
        let outcome = runner
            .provider_stream(
                "acme",
                &json!({
                    "model": { "id": "acme-1", "name": "Acme One", "api": "acme-api", "provider": "acme",
                               "baseUrl": "https://acme.test/v1", "reasoning": true, "input": ["text"],
                               "contextWindow": 90000, "maxTokens": 4096 },
                    "context": { "messages": [] },
                    "options": { "reasoning": "high" },
                }),
                None,
            )
            .expect("stream");
        assert!(outcome.error.is_none());
        // The handler's `onPayload` resolved with the transform chain's
        // replacement — and the replacement carried the payload the
        // handler sent (the fresh-event-per-handler chain).
        assert!(
            outcome.events.iter().any(|event| {
                event.get("delta").and_then(Json::as_str)
                    == Some(r#"payload:{"replaced":true,"saw":{"original":true}}"#)
            }),
            "events: {:?}",
            outcome.events
        );
        // The one-way hooks reached the extension listeners in FIFO order.
        let core = runner.core().lock().expect("core");
        let entry_data = |key: &str| -> Json {
            core.entries
                .iter()
                .find(|(custom_type, _)| custom_type == key)
                .map(|(_, data)| data.clone())
                .expect(key)
        };
        assert_eq!(
            entry_data("hook-response"),
            json!({ "status": 201, "header": "b" })
        );
        assert_eq!(
            entry_data("hook-event"),
            json!({
                "data": { "raw": "chunk" },
                "provider": "acme",
                "api": "acme-api",
                "model": "acme-1",
            })
        );
    }

    /// Without listeners the chain answers the payload unchanged (upstream's
    /// `transformProviderPayload` returns it; the handler still observes a
    /// callable hook).
    const EXT_PROVIDER_HOOKS_NO_LISTENERS: &str = r#"
export default function (pi) {
  pi.registerProvider("acme", {
    api: "acme-api",
    streamSimple: async function* (model, context, options) {
      const payload = await options.onPayload({ keep: "me" });
      yield { type: "text_delta", contentIndex: 0, delta: `payload:${JSON.stringify(payload)}` };
      yield { type: "done", reason: "stop", message: { role: "assistant", api: model.api, provider: model.provider, model: model.id, content: [] } };
    },
  });
}
"#;

    #[test]
    fn provider_stream_payload_chain_without_listeners_answers_the_input() {
        let mut runner = runner_with(&[EXT_PROVIDER_HOOKS_NO_LISTENERS]);
        let outcome = runner
            .provider_stream(
                "acme",
                &json!({
                    "model": { "id": "acme-1", "api": "acme-api", "provider": "acme" },
                    "context": { "messages": [] },
                    "options": {},
                }),
                None,
            )
            .expect("stream");
        assert!(outcome.error.is_none());
        assert!(
            outcome.events.iter().any(|event| {
                event.get("delta").and_then(Json::as_str) == Some(r#"payload:{"keep":"me"}"#)
            }),
            "events: {:?}",
            outcome.events
        );
    }

    /// The transform chain feeds forward in listener order: the second
    /// listener's replacement carries the first listener's, and an
    /// undefined return keeps the current payload (upstream's
    /// `handlerResult !== undefined` check).
    const EXT_PROVIDER_HOOK_CHAIN: &str = r#"
export default function (pi) {
  pi.on("before_provider_request", (event) => ({ order: 1, saw: event.payload.order }));
  pi.on("before_provider_request", () => undefined);
  pi.on("before_provider_request", (event) => ({ order: event.payload.order + 1 }));
  pi.registerProvider("acme", {
    api: "acme-api",
    streamSimple: async function* (model, context, options) {
      const payload = await options.onPayload({ order: 0 });
      yield { type: "text_delta", contentIndex: 0, delta: `order:${payload.order}` };
      yield { type: "done", reason: "stop", message: { role: "assistant", api: model.api, provider: model.provider, model: model.id, content: [] } };
    },
  });
}
"#;

    #[test]
    fn provider_stream_payload_chain_feeds_forward_in_listener_order() {
        let mut runner = runner_with(&[EXT_PROVIDER_HOOK_CHAIN]);
        let outcome = runner
            .provider_stream(
                "acme",
                &json!({
                    "model": { "id": "acme-1", "api": "acme-api", "provider": "acme" },
                    "context": { "messages": [] },
                    "options": {},
                }),
                None,
            )
            .expect("stream");
        assert!(outcome.error.is_none());
        // listener 1 → {order:1}, listener 2 keeps it, listener 3 → {order:2}.
        assert!(
            outcome
                .events
                .iter()
                .any(|event| event.get("delta").and_then(Json::as_str) == Some("order:2")),
            "events: {:?}",
            outcome.events
        );
    }

    /// The crossed `signal` flips when the run aborts: a handler polling
    /// it between yields observes the host's flag (`signal` rides the
    /// options, `agent-loop.ts:414`).
    const EXT_PROVIDER_SIGNAL: &str = r#"
export default function (pi) {
  pi.registerProvider("acme", {
    api: "acme-api",
    streamSimple: async function* (model, context, options) {
      yield { type: "text_delta", contentIndex: 0, delta: "before" };
      // A handler passes the crossed signal to its own network waits; the
      // host's abort during such an await is observed on resume.
      await options.onPayload(null);
      yield { type: "text_delta", contentIndex: 0, delta: options.signal.aborted ? "aborted" : "live" };
      yield { type: "done", reason: "stop", message: { role: "assistant", api: model.api, provider: model.provider, model: model.id, content: [] } };
    },
  });
}
"#;

    fn signal_request() -> Json {
        json!({
            "model": { "id": "acme-1", "api": "acme-api", "provider": "acme" },
            "context": { "messages": [] },
            "options": {},
        })
    }

    #[test]
    fn provider_stream_signal_flip_reaches_the_handler() {
        let mut runner = runner_with(&[EXT_PROVIDER_SIGNAL]);
        let outcome = runner
            .provider_stream("acme", &signal_request(), Some(&|| true))
            .expect("stream");
        assert!(outcome.error.is_none());
        assert!(
            outcome
                .events
                .iter()
                .any(|event| event.get("delta").and_then(Json::as_str) == Some("aborted")),
            "events: {:?}",
            outcome.events
        );
        // No abort: the handler sees the live signal.
        let mut runner = runner_with(&[EXT_PROVIDER_SIGNAL]);
        let outcome = runner
            .provider_stream("acme", &signal_request(), None)
            .expect("stream");
        assert!(
            outcome
                .events
                .iter()
                .any(|event| event.get("delta").and_then(Json::as_str) == Some("live")),
            "events: {:?}",
            outcome.events
        );
    }

    // ---- OAuth bridge (E2 slice 6j: `adaptOAuth`, provider-composer.ts:488-511;
    // callback surface `extension-oauth-types.ts:39-47`) ----

    /// A provider whose `login` only notifies and settles: one-way
    /// notifications cross the `oauthNotify` op in emission order and the
    /// credential comes back with `type: "oauth"` appended by the glue.
    const EXT_OAUTH_NOTIFY_ONLY: &str = r#"
export default function (pi) {
  pi.registerProvider("acme", {
    api: "acme-api",
    baseUrl: "https://acme.test/v1",
    oauth: {
      login: async (cb) => {
        cb.onProgress("starting");
        cb.onAuth({ url: "https://auth.test/authorize", instructions: "Open and approve." });
        cb.onDeviceCode({ userCode: "ABCD-1234", verificationUri: "https://verify.test" });
        return { access: "tok", refresh: "r" };
      },
    },
  });
}
"#;

    /// A `login` that exercises every ask: a text prompt, a select the
    /// host answers with `null` (resolve `undefined` — the key drops out
    /// of the settled credential), and a manual-code prompt.
    const EXT_OAUTH_PROMPTS: &str = r#"
export default function (pi) {
  pi.registerProvider("acme", {
    api: "acme-api",
    oauth: {
      login: async (cb) => {
        const code = await cb.onPrompt({ message: "paste the code", placeholder: "code", allowEmpty: false });
        const picked = await cb.onSelect({ message: "workspace", options: [{ id: "w1", label: "One" }, { id: "w2", label: "Two" }] });
        const manual = await cb.onManualCodeInput();
        return { access: code, workspace: picked, manual };
      },
    },
  });
}
"#;

    /// A `login` that checks the adapter `signal` after its prompt: the
    /// host's abort flip must be observable to the handler.
    const EXT_OAUTH_ABORT: &str = r#"
export default function (pi) {
  pi.registerProvider("acme", {
    api: "acme-api",
    oauth: {
      login: async (cb) => {
        await cb.onPrompt({ message: "waiting for approval" });
        if (cb.signal.aborted) throw new Error("login aborted");
        return { access: "late" };
      },
    },
  });
}
"#;

    /// A factory that registers an oauth provider and then dies: the
    /// stashed config must die with the record, like the route and
    /// streamSimple stashes.
    const EXT_OAUTH_FAIL: &str = r#"
export default function (pi) {
  pi.registerProvider("acme", { api: "acme-api", oauth: { login: async () => ({}) } });
  throw new Error("factory died");
}
"#;

    /// `refreshToken` (async, slot-settled) and `getApiKey` (sync,
    /// inline) on one provider (`adaptOAuth`'s refresh and toAuth arms).
    const EXT_OAUTH_CALLS: &str = r#"
export default function (pi) {
  pi.registerProvider("acme", {
    api: "acme-api",
    oauth: {
      login: async () => ({}),
      refreshToken: async (credential) => ({ access: `fresh-${credential.access}`, refresh: "r2" }),
      getApiKey: (credential) => `key-${credential.access}`,
    },
  });
}
"#;

    /// Register then unregister: the discard must clear the oauth stash.
    const EXT_OAUTH_UNREGISTER: &str = r#"
export default function (pi) {
  pi.registerProvider("acme", { api: "acme-api", oauth: { login: async () => ({}) } });
  pi.unregisterProvider("acme");
}
"#;

    #[test]
    fn oauth_login_notifies_and_settles_the_credential() {
        let mut runner = runner_with(&[EXT_OAUTH_NOTIFY_ONLY]);
        assert!(runner.oauth_has_login("acme"));
        assert!(!runner.oauth_has_login("missing"));
        let id = runner.oauth_login_start("acme").expect("start");
        let poll = runner.oauth_login_poll(id).expect("poll");
        assert!(poll.prompt.is_none());
        assert_eq!(
            poll.notifications,
            vec![
                json!({ "type": "progress", "message": "starting" }),
                json!({ "type": "auth_url", "url": "https://auth.test/authorize", "instructions": "Open and approve." }),
                json!({ "type": "device_code", "userCode": "ABCD-1234", "verificationUri": "https://verify.test" }),
            ]
        );
        assert_eq!(
            poll.settled.expect("settled"),
            Ok(json!({ "access": "tok", "refresh": "r", "type": "oauth" }))
        );
        runner.oauth_login_end(id).expect("end");
    }

    #[test]
    fn oauth_login_prompts_round_trip_through_the_host() {
        let mut runner = runner_with(&[EXT_OAUTH_PROMPTS]);
        let id = runner.oauth_login_start("acme").expect("start");
        // Text prompt: the payload rides the op; the answer crosses back
        // through the token-verified resolve.
        let (token, payload) = runner
            .oauth_login_poll(id)
            .expect("poll")
            .prompt
            .expect("text prompt");
        assert_eq!(
            payload,
            json!({ "type": "text", "message": "paste the code", "placeholder": "code", "allowEmpty": false })
        );
        runner
            .oauth_login_answer(id, token, &json!("the-code"))
            .expect("answer");
        // Select prompt answered `null`: the promise resolves `undefined`,
        // so the key drops out of the settled credential.
        let (token, payload) = runner
            .oauth_login_poll(id)
            .expect("poll")
            .prompt
            .expect("select prompt");
        assert_eq!(
            payload,
            json!({ "type": "select", "message": "workspace", "options": [
                { "id": "w1", "label": "One" }, { "id": "w2", "label": "Two" }] })
        );
        runner
            .oauth_login_answer(id, token, &Json::Null)
            .expect("answer");
        // Manual-code prompt.
        let (token, payload) = runner
            .oauth_login_poll(id)
            .expect("poll")
            .prompt
            .expect("manual prompt");
        assert_eq!(
            payload,
            json!({ "type": "manual_code", "message": "Paste the authorization code" })
        );
        runner
            .oauth_login_answer(id, token, &json!("manual-1"))
            .expect("answer");
        let poll = runner.oauth_login_poll(id).expect("poll");
        assert!(poll.prompt.is_none());
        assert_eq!(
            poll.settled.expect("settled"),
            Ok(json!({ "access": "the-code", "manual": "manual-1", "type": "oauth" }))
        );
        runner.oauth_login_end(id).expect("end");
    }

    #[test]
    fn oauth_login_prompt_answers_are_token_verified() {
        let mut runner = runner_with(&[EXT_OAUTH_PROMPTS]);
        let id = runner.oauth_login_start("acme").expect("start");
        let (token, _) = runner
            .oauth_login_poll(id)
            .expect("poll")
            .prompt
            .expect("text prompt");
        // A stale/foreign token is rejected and leaves the ask pending.
        let error = runner
            .oauth_login_answer(id, token + 1, &json!("nope"))
            .expect_err("rejected");
        // `JsHostError::Job`'s Display wraps the glue's message.
        assert_eq!(
            error.to_string(),
            "javascript job error: oauth prompt answer rejected: token-mismatch"
        );
        // The correct token still resolves the pending ask.
        runner
            .oauth_login_answer(id, token, &json!("the-code"))
            .expect("answer");
        runner.oauth_login_end(id).expect("end");
    }

    #[test]
    fn oauth_login_abort_flips_the_signal_the_handler_polls() {
        let mut runner = runner_with(&[EXT_OAUTH_ABORT]);
        let id = runner.oauth_login_start("acme").expect("start");
        let (token, _) = runner
            .oauth_login_poll(id)
            .expect("poll")
            .prompt
            .expect("prompt");
        runner.oauth_login_abort(id).expect("abort");
        // The answer still resolves the pending ask (the UI's dialog
        // closes); the handler then observes `signal.aborted` and throws.
        runner
            .oauth_login_answer(id, token, &Json::Null)
            .expect("answer");
        let poll = runner.oauth_login_poll(id).expect("poll");
        assert_eq!(
            poll.settled.expect("settled"),
            Err("login aborted".to_string())
        );
        runner.oauth_login_end(id).expect("end");
    }

    #[test]
    fn oauth_login_of_an_unregistered_provider_settles_the_glue_error() {
        let mut runner = runner_with(&[]);
        let id = runner.oauth_login_start("acme").expect("start");
        let poll = runner.oauth_login_poll(id).expect("poll");
        assert_eq!(
            poll.settled.expect("settled"),
            Err("Provider acme is not registered with an oauth login handler.".to_string())
        );
        runner.oauth_login_end(id).expect("end");
    }

    #[test]
    fn oauth_stash_dies_with_a_failed_extension() {
        let mut runner = ExtensionRunner::new(HostCore {
            cwd: "/tmp".to_string(),
            mode: "print".to_string(),
            settings: json!({}),
            ..Default::default()
        })
        .expect("runner");
        assert!(
            runner
                .load_extension("<test-0>", Some(EXT_OAUTH_FAIL))
                .is_err()
        );
        runner.bind().expect("bind");
        assert!(!runner.oauth_has_login("acme"));
        let id = runner.oauth_login_start("acme").expect("start");
        let poll = runner.oauth_login_poll(id).expect("poll");
        assert_eq!(
            poll.settled.expect("settled"),
            Err("Provider acme is not registered with an oauth login handler.".to_string())
        );
        runner.oauth_login_end(id).expect("end");
    }

    #[test]
    fn oauth_unregister_clears_the_stash() {
        let runner = runner_with(&[EXT_OAUTH_UNREGISTER]);
        assert!(!runner.oauth_has_login("acme"));
    }

    #[test]
    fn oauth_refresh_and_api_key_call_the_stashed_handlers() {
        let mut runner = runner_with(&[EXT_OAUTH_CALLS]);
        let credential = json!({ "access": "tok" });
        assert_eq!(
            runner.oauth_refresh("acme", &credential).expect("refresh"),
            json!({ "access": "fresh-tok", "refresh": "r2", "type": "oauth" })
        );
        assert_eq!(
            runner.oauth_api_key("acme", &credential).expect("api key"),
            "key-tok"
        );
    }

    #[test]
    fn oauth_calls_of_unregistered_providers_answer_the_glue_error() {
        let mut runner = runner_with(&[]);
        assert_eq!(
            runner.oauth_refresh("acme", &json!({})).expect_err("error"),
            "Provider acme is not registered with an oauth refresh handler."
        );
        assert_eq!(
            runner.oauth_api_key("acme", &json!({})).expect_err("error"),
            "Provider acme is not registered with an oauth getApiKey handler."
        );
    }

    // ---- the native MCP server manager (`builtin:mcp`) ----

    /// The extension source registering the fixture server; `extra` adds
    /// handlers.
    fn mcp_extension_source(server: &std::path::Path, extra: &str) -> String {
        let command = server
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        format!(
            r#"
export default function (pi) {{
  pi.registerMcpServer("synth", {{ command: "{command}", args: [], exposure: "direct", timeout: 5 }});
  pi.on("probe", async () => {{ pi.appendEntry("mcp-servers", pi.getMcpServers()); }});
  {extra}
}}
"#
        )
    }

    #[test]
    fn mcp_servers_connect_and_bridge_round_trip() {
        let directory = tempfile::tempdir().expect("tempdir");
        let server = crate::mcp_manager::write_fixture_server(directory.path()).expect("fixture");
        let source = mcp_extension_source(&server, "");
        let mut runner = runner_with(&[source.as_str()]);
        // The session_start connection pass: one connected server with its
        // three tools.
        let reports = runner.connect_mcp_servers().expect("connect");
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].name, "synth");
        assert_eq!(reports[0].state, "connected");
        assert_eq!(reports[0].tool_count, 3);
        // The bridge tools registered with upstream's naming, including
        // the sanitized `odd-name`.
        let names: Vec<&str> = runner
            .all_registered_tools()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert!(names.contains(&"mcp__synth__echo"), "{names:?}");
        assert!(names.contains(&"mcp__synth__boom"), "{names:?}");
        assert!(names.contains(&"mcp__synth__odd_name"), "{names:?}");
        // Direct exposure activated them.
        for name in [
            "mcp__synth__echo",
            "mcp__synth__boom",
            "mcp__synth__odd_name",
        ] {
            assert!(
                runner.active_tools().iter().any(|active| active == name),
                "{name} not active"
            );
        }
        // The echo tool round-trips through tools/call and the result
        // mapping (tools.ts convertMcpResult).
        let result = runner
            .execute_tool("mcp__synth__echo", "call-1", &json!({ "text": "hi" }))
            .expect("execute");
        assert_eq!(result["content"][0]["text"], "echo-ok");
        assert_eq!(result["details"]["server"], "synth");
        assert_eq!(result["details"]["tool"], "echo");
        // The script result is the whole CallToolResult minus _meta.
        assert_eq!(result["structuredContent"]["structuredContent"]["ok"], true);
        // A protocol-level isError result stays a result.
        let result = runner
            .execute_tool("mcp__synth__boom", "call-2", &json!({}))
            .expect("execute");
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "it failed");
        // getMcpServers reports the registry shape.
        runner.emit(&json!({ "type": "probe" })).expect("emit");
        let entries = {
            let core = runner.core().lock().expect("core");
            core.entries.clone()
        };
        let servers = &entries.last().expect("probe entry").1;
        assert_eq!(servers[0]["name"], "synth");
        assert_eq!(servers[0]["extensionPath"], "<test-0>");
        assert_eq!(servers[0]["config"]["exposure"], "direct");
        runner.shutdown_mcp();
        assert_eq!(runner.mcp_server_reports(), Vec::<McpServerReport>::new());
    }

    #[test]
    fn mcp_server_connect_failure_is_a_reported_value() {
        let source = r#"
export default function (pi) {
  pi.registerMcpServer("ghost", { command: "/nonexistent-pi-rs-mcp-synth", timeout: 1 });
}
"#;
        let mut runner = runner_with(&[source]);
        let reports = runner.connect_mcp_servers().expect("connect");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, "failed");
        let error = reports[0].error.as_deref().expect("error message");
        assert!(error.contains("failed to spawn"), "{error}");
        // The failed server registered no tools, and the host keeps going.
        assert!(
            runner
                .all_registered_tools()
                .iter()
                .all(|tool| !tool.name.starts_with("mcp__"))
        );
    }

    #[test]
    fn default_exposure_registers_deferred_and_inactive() {
        let directory = tempfile::tempdir().expect("tempdir");
        let server = crate::mcp_manager::write_fixture_server(directory.path()).expect("fixture");
        let source = format!(
            r#"
export default function (pi) {{
  pi.registerMcpServer("synth", {{ command: "{}", args: [], timeout: 5 }});
}}
"#,
            server.to_string_lossy()
        );
        let mut runner = runner_with(&[source.as_str()]);
        runner.connect_mcp_servers().expect("connect");
        // `exposure` defaults to codemode, which registers as the deferred
        // tool exposure (tools.ts:46-48) — not declared, not active.
        let tool = runner.tool("mcp__synth__echo").expect("tool");
        assert_eq!(tool.exposure, "deferred");
        assert!(
            !runner
                .active_tools()
                .iter()
                .any(|active| active == "mcp__synth__echo"),
            "deferred tools stay inactive"
        );
    }

    #[test]
    fn session_start_registrations_connect_right_away() {
        let directory = tempfile::tempdir().expect("tempdir");
        let server = crate::mcp_manager::write_fixture_server(directory.path()).expect("fixture");
        let extra = format!(
            r#"pi.on("session_start", async () => {{
    pi.registerMcpServer("late", {{ command: "{}", args: [], timeout: 5 }});
  }});
  pi.on("late-probe", async () => {{ pi.appendEntry("late", pi.getAllTools()); }});"#,
            server.to_string_lossy()
        );
        let source = mcp_extension_source(&server, &extra);
        let mut runner = runner_with(&[source.as_str()]);
        // The session_start emission runs the handler, which registers the
        // second server; the post-dispatch drain connects it right away
        // (runner.ts:465-466 + index.ts:1215-1248) — and the reconcile also
        // picks up `synth`, which the test never connected directly.
        runner
            .emit(&json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        let tool = runner
            .tool("mcp__late__echo")
            .expect("late tool registered");
        assert_eq!(tool.exposure, "deferred", "no exposure configured");
        assert_eq!(runner.mcp_server_reports().len(), 2);
        assert!(
            runner
                .mcp_server_reports()
                .iter()
                .all(|report| report.state == "connected")
        );
    }

    #[test]
    fn unregistering_a_server_hides_its_tools() {
        let directory = tempfile::tempdir().expect("tempdir");
        let server = crate::mcp_manager::write_fixture_server(directory.path()).expect("fixture");
        let extra = r#"pi.on("drop", async () => { pi.unregisterMcpServer("synth"); });"#;
        let source = mcp_extension_source(&server, extra);
        let mut runner = runner_with(&[source.as_str()]);
        runner.connect_mcp_servers().expect("connect");
        runner.emit(&json!({ "type": "drop" })).expect("emit");
        // The connection is gone; the tools stay registered but
        // unreachable (index.ts:455-462).
        assert_eq!(runner.mcp_server_reports(), Vec::<McpServerReport>::new());
        let tool = runner.tool("mcp__synth__echo").expect("tool");
        assert_eq!(tool.exposure, "hidden");
        assert!(
            !runner
                .active_tools()
                .iter()
                .any(|active| active == "mcp__synth__echo"),
            "hidden tools leave the active set"
        );
    }

    // ---- E3 slice 3: entry renderers + component handles ----

    /// The on-disk custom-entry record `__piRenderEntry` JSON.parses and
    /// hands the renderer (the `data` field feeds the round-trip test).
    const ENTRY_NOTE: &str = r#"{"type":"custom","id":"e1","timestamp":"2026-01-01T00:00:00Z","customType":"note","data":{"k":1}}"#;

    #[test]
    fn entry_renderer_first_extension_wins() {
        const EXT_A: &str = r#"
export default function (pi) {
  pi.registerEntryRenderer("note", () => ({
    render: (width) => ["a-0 w" + width],
    invalidate: () => {},
  }));
}
"#;
        const EXT_B: &str = r#"
export default function (pi) {
  pi.registerEntryRenderer("note", () => ({
    render: (width) => ["b-0 w" + width],
    invalidate: () => {},
  }));
}
"#;
        let runner = runner_with(&[EXT_A, EXT_B]);
        // Load order decides (`getEntryRenderer`, runner.ts:815-823).
        assert_eq!(runner.get_entry_renderer("note"), Some(0));
        assert_eq!(runner.get_entry_renderer("other"), None);
        let outcome = runner
            .render_entry(0, "note", ENTRY_NOTE, r#"{"expanded":true}"#)
            .expect("render");
        let EntryRenderOutcome::Component { handle, has_input } = outcome else {
            panic!("expected a component, got {outcome:?}");
        };
        assert!(!has_input);
        assert_eq!(
            runner.component_render(handle, 10).expect("render"),
            vec!["a-0 w10".to_string()]
        );
    }

    #[test]
    fn render_entry_none_without_renderer_and_undefined_result() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerEntryRenderer("note", () => undefined);
}
"#;
        let runner = runner_with(&[EXT]);
        // A renderer returning undefined adopts nothing (custom-entry.ts
        // rebuild maps that to the "none" outcome upstream).
        let outcome = runner
            .render_entry(0, "note", ENTRY_NOTE, "{}")
            .expect("render");
        assert!(matches!(outcome, EntryRenderOutcome::None), "{outcome:?}");
        // No renderer for the type is the same outcome.
        let outcome = runner
            .render_entry(0, "missing", ENTRY_NOTE, "{}")
            .expect("render");
        assert!(matches!(outcome, EntryRenderOutcome::None), "{outcome:?}");
    }

    #[test]
    fn render_entry_failed_on_renderer_throw_and_adoption_validation() {
        const EXT_THROW: &str = r#"
export default function (pi) {
  pi.registerEntryRenderer("note", () => {
    throw new Error("boom-renderer");
  });
}
"#;
        const EXT_NOFN: &str = r#"
export default function (pi) {
  // Neither render nor invalidate — adoption rejects it (`tui.ts:124-155`).
  pi.registerEntryRenderer("note", () => ({}));
}
"#;
        let runner = runner_with(&[EXT_THROW, EXT_NOFN]);
        // The glue catch maps a renderer throw to {kind:"error",message}
        // (custom-entry.ts:44-50 renders the failure box upstream).
        let thrown = runner
            .render_entry(0, "note", ENTRY_NOTE, "{}")
            .expect("render");
        let EntryRenderOutcome::Failed(message) = thrown else {
            panic!("expected a failure, got {thrown:?}");
        };
        assert_eq!(message, "boom-renderer");
        // The adopt validation throws into the same catch.
        let rejected = runner
            .render_entry(1, "note", ENTRY_NOTE, "{}")
            .expect("render");
        let EntryRenderOutcome::Failed(message) = rejected else {
            panic!("expected a failure, got {rejected:?}");
        };
        assert_eq!(
            message,
            "extension components must implement render(width) and invalidate()"
        );
    }

    #[test]
    fn component_round_trip_render_input_invalidate_release() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerEntryRenderer("note", (entry) => ({
    render: (width) => ["w:" + width, String(entry.data.k)],
    invalidate: () => {},
    handleInput: (data) => data === "yes",
  }));
}
"#;
        let runner = runner_with(&[EXT]);
        let outcome = runner
            .render_entry(0, "note", ENTRY_NOTE, r#"{"expanded":true}"#)
            .expect("render");
        let EntryRenderOutcome::Component { handle, has_input } = outcome else {
            panic!("expected a component, got {outcome:?}");
        };
        assert!(has_input);
        // The width and the parsed entry JSON both crossed the boundary.
        assert_eq!(
            runner.component_render(handle, 42).expect("render"),
            vec!["w:42".to_string(), "1".to_string()]
        );
        // `handleInput(data) === true` decides the consume flag
        // (`__piComponentHandleInput`).
        assert!(runner.component_handle_input(handle, "yes").expect("input"));
        assert!(!runner.component_handle_input(handle, "no").expect("input"));
        runner.component_invalidate(handle).expect("invalidate");
        runner.component_release(handle).expect("release");
        // The registry slot is gone: renders come back empty.
        assert_eq!(
            runner.component_render(handle, 42).expect("render"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn keytext_and_keyhint_cross_the_app_keybindings() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerEntryRenderer("note", (entry, options) => ({
    render: () => [
      options.keyText("pi.editor.submit"),
      options.keyText("pi.app.unknown"),
      options.keyHint("pi.app.exit", "quit"),
    ],
    invalidate: () => {},
  }));
}
"#;
        let mut runner = runner_with(&[EXT]);
        // The keybinding table crosses before the render (`getKeybindings()`
        // upstream — keybinding-hints.ts:34-44, without the darwin alt swap).
        runner.set_app_keybindings(
            r#"{"pi.editor.submit":["enter","alt+enter"],"pi.app.exit":["ctrl+c"]}"#,
        );
        let outcome = runner
            .render_entry(0, "note", ENTRY_NOTE, "{}")
            .expect("render");
        let EntryRenderOutcome::Component { handle, .. } = outcome else {
            panic!("expected a component, got {outcome:?}");
        };
        // keyText joins the chords with "/" ("" for unknown ids); keyHint is
        // fg("dim", keyText) + fg("muted", " " + description) — identity
        // through the default theme closures.
        assert_eq!(
            runner.component_render(handle, 40).expect("render"),
            vec![
                "enter/alt+enter".to_string(),
                String::new(),
                "ctrl+c quit".to_string()
            ]
        );
    }

    #[test]
    fn theme_surface_maps_tokens_for_entry_renderers() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerEntryRenderer("note", (entry, options, theme) => ({
    render: () => [theme.fg("error", "X")],
    invalidate: () => {},
  }));
}
"#;
        // Without a surface the theme closures are identity: the token is
        // dropped and the text passes through (`theme.ts:297/:303` upstream).
        let runner = runner_with(&[EXT]);
        let outcome = runner
            .render_entry(0, "note", ENTRY_NOTE, "{}")
            .expect("render");
        let EntryRenderOutcome::Component { handle, .. } = outcome else {
            panic!("expected a component, got {outcome:?}");
        };
        assert_eq!(
            runner.component_render(handle, 10).expect("render"),
            vec!["X".to_string()]
        );
        // The installed surface maps the token per call; bg stays identity.
        let mut themed = runner_with(&[EXT]);
        themed.set_theme_surface(
            std::sync::Arc::new(|token: &str, text: &str| format!("<{token}>{text}</{token}>")),
            std::sync::Arc::new(|_token: &str, text: &str| text.to_string()),
        );
        let outcome = themed
            .render_entry(0, "note", ENTRY_NOTE, "{}")
            .expect("render");
        let EntryRenderOutcome::Component { handle, .. } = outcome else {
            panic!("expected a component, got {outcome:?}");
        };
        assert_eq!(
            themed.component_render(handle, 10).expect("render"),
            vec!["<error>X</error>".to_string()]
        );
    }

    // ---- E3 slice 4: message renderers ----

    /// The custom-message record `__piRenderMessage` JSON.parses and hands
    /// the renderer (`core/messages.ts:46-54` — role, customType, content,
    /// display, details, timestamp).
    const MESSAGE_NOTE: &str = r#"{"role":"custom","customType":"note","content":"hello body","display":true,"details":{"k":1},"timestamp":1767225600000}"#;

    /// The first extension in load order renders a custom message
    /// (`getMessageRenderer`, runner.ts:782-790).
    #[test]
    fn message_renderer_first_extension_wins() {
        const EXT_A: &str = r#"
export default function (pi) {
  pi.registerMessageRenderer("note", () => ({
    render: (width) => ["a-0 w" + width],
    invalidate: () => {},
  }));
}
"#;
        const EXT_B: &str = r#"
export default function (pi) {
  pi.registerMessageRenderer("note", () => ({
    render: (width) => ["b-0 w" + width],
    invalidate: () => {},
  }));
}
"#;
        let runner = runner_with(&[EXT_A, EXT_B]);
        assert_eq!(runner.get_message_renderer("note"), Some(0));
        assert_eq!(runner.get_message_renderer("other"), None);
        let outcome = runner
            .render_message(
                0,
                "note",
                MESSAGE_NOTE,
                r#"{"expanded":true,"outputPad":2}"#,
            )
            .expect("render");
        let EntryRenderOutcome::Component { handle, has_input } = outcome else {
            panic!("expected a component, got {outcome:?}");
        };
        assert!(!has_input);
        assert_eq!(
            runner.component_render(handle, 10).expect("render"),
            vec!["a-0 w10".to_string()]
        );
    }

    /// A message renderer returning nothing adopts nothing — the return is
    /// typed `Component | undefined` (custom-message.ts:87-92).
    #[test]
    fn render_message_none_without_renderer_and_undefined_result() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerMessageRenderer("note", () => undefined);
}
"#;
        let runner = runner_with(&[EXT]);
        // A renderer returning undefined adopts nothing.
        let outcome = runner
            .render_message(0, "note", MESSAGE_NOTE, "{}")
            .expect("render");
        assert!(matches!(outcome, EntryRenderOutcome::None), "{outcome:?}");
        // No renderer for the type is the same outcome.
        let outcome = runner
            .render_message(0, "missing", MESSAGE_NOTE, "{}")
            .expect("render");
        assert!(matches!(outcome, EntryRenderOutcome::None), "{outcome:?}");
    }

    /// The glue catch maps a renderer throw to {kind:"error"} and surfaces
    /// it as `Failed(message)` here; upstream the host's custom-message
    /// component falls through to the default rendering silently
    /// (custom-message.ts:93-95). Adoption validation throws into the same
    /// catch.
    #[test]
    fn render_message_failed_on_renderer_throw_and_adoption_validation() {
        const EXT_THROW: &str = r#"
export default function (pi) {
  pi.registerMessageRenderer("note", () => {
    throw new Error("boom-renderer");
  });
}
"#;
        const EXT_NOFN: &str = r#"
export default function (pi) {
  // Neither render nor invalidate — adoption rejects it (`tui.ts:124-155`).
  pi.registerMessageRenderer("note", () => ({}));
}
"#;
        let runner = runner_with(&[EXT_THROW, EXT_NOFN]);
        let thrown = runner
            .render_message(0, "note", MESSAGE_NOTE, "{}")
            .expect("render");
        let EntryRenderOutcome::Failed(message) = thrown else {
            panic!("expected a failure, got {thrown:?}");
        };
        assert_eq!(message, "boom-renderer");
        // The adopt validation throws into the same catch.
        let rejected = runner
            .render_message(1, "note", MESSAGE_NOTE, "{}")
            .expect("render");
        let EntryRenderOutcome::Failed(message) = rejected else {
            panic!("expected a failure, got {rejected:?}");
        };
        assert_eq!(
            message,
            "extension components must implement render(width) and invalidate()"
        );
    }

    /// The custom-message record crosses the boundary with its
    /// `role:"custom"` intact (`core/messages.ts:46-54`); the component
    /// handle round-trip is the slice-3 bridge (`__piComponent*`).
    #[test]
    fn message_round_trip_render_input_invalidate_release() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerMessageRenderer("note", (message) => ({
    render: (width) => ["w:" + width, message.customType, String(message.role), message.content],
    invalidate: () => {},
    handleInput: (data) => data === "yes",
  }));
}
"#;
        let runner = runner_with(&[EXT]);
        let outcome = runner
            .render_message(0, "note", MESSAGE_NOTE, r#"{"expanded":true}"#)
            .expect("render");
        let EntryRenderOutcome::Component { handle, has_input } = outcome else {
            panic!("expected a component, got {outcome:?}");
        };
        assert!(has_input);
        // The width and the parsed record — role:"custom" and all — cross
        // the boundary.
        assert_eq!(
            runner.component_render(handle, 42).expect("render"),
            vec![
                "w:42".to_string(),
                "note".to_string(),
                "custom".to_string(),
                "hello body".to_string()
            ]
        );
        // `handleInput(data) === true` decides the consume flag
        // (`__piComponentHandleInput`).
        assert!(runner.component_handle_input(handle, "yes").expect("input"));
        assert!(!runner.component_handle_input(handle, "no").expect("input"));
        runner.component_invalidate(handle).expect("invalidate");
        runner.component_release(handle).expect("release");
        // The registry slot is gone: renders come back empty.
        assert_eq!(
            runner.component_render(handle, 42).expect("render"),
            Vec::<String>::new()
        );
    }

    /// `MessageRenderOptions` adds `outputPad` to the entry hints
    /// (types.ts:1556-1560); keyText joins the chords with "/" and is ""
    /// for unknown ids (keybinding-hints.ts:34-44).
    #[test]
    fn message_options_cross_outputpad_expanded_and_keybindings() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerMessageRenderer("note", (message, options) => ({
    render: () => [
      String(options.outputPad),
      String(options.expanded),
      options.keyText("pi.editor.submit"),
      options.keyText("pi.app.unknown"),
      options.keyHint("pi.app.exit", "quit"),
    ],
    invalidate: () => {},
  }));
}
"#;
        let mut runner = runner_with(&[EXT]);
        // The keybinding table crosses before the render (`getKeybindings()`
        // upstream) — keyHint is fg("dim", keyText) + fg("muted", " " +
        // description), identity through the default theme closures.
        runner.set_app_keybindings(
            r#"{"pi.editor.submit":["enter","alt+enter"],"pi.app.exit":["ctrl+c"]}"#,
        );
        let outcome = runner
            .render_message(
                0,
                "note",
                MESSAGE_NOTE,
                r#"{"expanded":true,"outputPad":3}"#,
            )
            .expect("render");
        let EntryRenderOutcome::Component { handle, .. } = outcome else {
            panic!("expected a component, got {outcome:?}");
        };
        assert_eq!(
            runner.component_render(handle, 40).expect("render"),
            vec![
                "3".to_string(),
                "true".to_string(),
                "enter/alt+enter".to_string(),
                String::new(),
                "ctrl+c quit".to_string()
            ]
        );
    }

    /// The theme object styles synchronously through ops (theme.ts:297/:303)
    /// — the same surface the entry renderers use (custom-message.ts:85).
    #[test]
    fn message_theme_surface_maps_tokens_for_renderers() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerMessageRenderer("note", (message, options, theme) => ({
    render: () => [theme.fg("customMessageText", "X")],
    invalidate: () => {},
  }));
}
"#;
        // Without a surface the theme closures are identity: the token is
        // dropped and the text passes through.
        let runner = runner_with(&[EXT]);
        let outcome = runner
            .render_message(0, "note", MESSAGE_NOTE, "{}")
            .expect("render");
        let EntryRenderOutcome::Component { handle, .. } = outcome else {
            panic!("expected a component, got {outcome:?}");
        };
        assert_eq!(
            runner.component_render(handle, 10).expect("render"),
            vec!["X".to_string()]
        );
        // The installed surface maps the token per call; bg stays identity.
        let mut themed = runner_with(&[EXT]);
        themed.set_theme_surface(
            std::sync::Arc::new(|token: &str, text: &str| format!("<{token}>{text}</{token}>")),
            std::sync::Arc::new(|_token: &str, text: &str| text.to_string()),
        );
        let outcome = themed
            .render_message(0, "note", MESSAGE_NOTE, "{}")
            .expect("render");
        let EntryRenderOutcome::Component { handle, .. } = outcome else {
            panic!("expected a component, got {outcome:?}");
        };
        assert_eq!(
            themed.component_render(handle, 10).expect("render"),
            vec!["<customMessageText>X</customMessageText>".to_string()]
        );
    }

    // ---- E3 slice 5: tool renderers ----

    /// A passthrough resolver adopts the base verbatim
    /// (`resolveToolRenderers`, runner.ts:798-806): a known tool's base
    /// carries the built-in slots (`withBuiltInRenderers`,
    /// renderers/index.ts:51-63), which cross as the `builtin`
    /// classification — and a builtin slot never crosses the render
    /// bridge; the row renders Rust-side.
    #[test]
    fn tool_renderer_chain_passthrough_yields_builtin_slots() {
        const EXT: &str = r"
export default function (pi) {
  pi.registerToolRenderer((toolName, next) => next());
}
";
        let runner = runner_with(&[EXT]);
        let outcome = runner
            .resolve_tool_renderers("read", true)
            .expect("resolve");
        let ToolRenderersOutcome::Renderers(resolved) = outcome else {
            panic!("expected renderers, got {outcome:?}");
        };
        assert_eq!(resolved.render_call, ToolRenderSlot::Builtin);
        assert_eq!(resolved.render_result, ToolRenderSlot::Builtin);
        assert!(!resolved.render_shell_self);
        let outcome = runner
            .tool_render_call(resolved.stash_id, "{}", "{}")
            .expect("render call");
        assert!(matches!(outcome, EntryRenderOutcome::None), "{outcome:?}");
    }

    /// Resolvers run in load order and the first non-undefined return stops
    /// the chain (`registerToolRenderer`, loader.ts:379-383;
    /// `resolveToolRenderers`, runner.ts:798-806) — the second extension's
    /// throwing resolver must never run, or the resolve would surface its
    /// failure instead.
    #[test]
    fn tool_renderer_first_resolver_wins_and_stops_the_chain() {
        const EXT_A: &str = r#"
export default function (pi) {
  pi.registerToolRenderer((toolName, next) => ({
    renderCall: (args, theme, context) => ({
      render: () => ["a-call"],
      invalidate: () => {},
    }),
  }));
}
"#;
        const EXT_B: &str = r#"
export default function (pi) {
  pi.registerToolRenderer(() => {
    throw new Error("chain-reached-b");
  });
}
"#;
        let runner = runner_with(&[EXT_A, EXT_B]);
        let outcome = runner
            .resolve_tool_renderers("synth", false)
            .expect("resolve");
        let ToolRenderersOutcome::Renderers(resolved) = outcome else {
            panic!("expected renderers, got {outcome:?}");
        };
        assert_eq!(resolved.render_call, ToolRenderSlot::Custom);
        assert_eq!(resolved.render_result, ToolRenderSlot::Absent);
        let outcome = runner
            .tool_render_call(resolved.stash_id, "{}", "{}")
            .expect("render call");
        let EntryRenderOutcome::Component { handle, .. } = outcome else {
            panic!("expected a component, got {outcome:?}");
        };
        assert_eq!(
            runner.component_render(handle, 40).expect("render"),
            vec!["a-call".to_string()]
        );
    }

    /// An unknown tool's base is undefined (`withBuiltInRenderers` has no
    /// built-in for it, renderers/index.ts:51-63), so a passthrough
    /// resolver adopts nothing; `renderShell: "self"` crosses as the
    /// self-shell flag and an explicit `renderResult: undefined` is the
    /// absent slot (types.ts:616/:693).
    #[test]
    fn tool_renderer_unknown_tool_base_is_undefined_and_shell_self_crosses() {
        const EXT_PASS: &str = r"
export default function (pi) {
  pi.registerToolRenderer((toolName, next) => next());
}
";
        const EXT_SELF: &str = r#"
export default function (pi) {
  pi.registerToolRenderer(() => ({
    renderShell: "self",
    renderCall: (args) => ({ render: () => ["self-row"], invalidate: () => {} }),
    renderResult: undefined,
  }));
}
"#;
        let runner = runner_with(&[EXT_PASS]);
        let outcome = runner
            .resolve_tool_renderers("synth", false)
            .expect("resolve");
        assert!(matches!(outcome, ToolRenderersOutcome::None), "{outcome:?}");
        let runner = runner_with(&[EXT_SELF]);
        let outcome = runner
            .resolve_tool_renderers("synth", false)
            .expect("resolve");
        let ToolRenderersOutcome::Renderers(resolved) = outcome else {
            panic!("expected renderers, got {outcome:?}");
        };
        assert!(resolved.render_shell_self);
        assert_eq!(resolved.render_call, ToolRenderSlot::Custom);
        assert_eq!(resolved.render_result, ToolRenderSlot::Absent);
    }

    /// `renderCall(args, theme, context)` receives the full
    /// `ToolRenderContext` (tool-execution.ts:122-144): args/cwd/
    /// toolCallId/outputPad cross Rust-side, and `state` is the row's
    /// shared object (tool-execution.ts:39/:131) that survives across
    /// calls — the second call sees the component the first stashed.
    #[test]
    fn tool_render_call_round_trip_context_state_and_last_component() {
        const CONTEXT: &str = r#"{"toolCallId":"call-1","cwd":"/w","executionStarted":true,"argsComplete":true,"isPartial":true,"expanded":false,"showImages":false,"isError":false,"outputPad":3}"#;
        const EXT: &str = r#"
export default function (pi) {
  pi.registerToolRenderer(() => ({
    renderCall: (args, theme, context) => {
      const last = context.state.comp ? "last:same" : "last:none";
      const component = {
        render: () => [
          String(args.k),
          context.cwd,
          context.toolCallId,
          String(context.outputPad),
          "n=" + String(context.state.n),
          last,
        ],
        invalidate: () => {},
      };
      context.state.comp = component;
      return component;
    },
  }));
}
"#;
        let runner = runner_with(&[EXT]);
        let outcome = runner
            .resolve_tool_renderers("read", true)
            .expect("resolve");
        let ToolRenderersOutcome::Renderers(resolved) = outcome else {
            panic!("expected renderers, got {outcome:?}");
        };
        // First call: the row's state starts empty, so the marker is
        // "last:none", and the unset `state.n` stringifies as "undefined".
        let outcome = runner
            .tool_render_call(resolved.stash_id, r#"{"k":7}"#, CONTEXT)
            .expect("render call");
        let EntryRenderOutcome::Component { handle, .. } = outcome else {
            panic!("expected a component, got {outcome:?}");
        };
        assert_eq!(
            runner.component_render(handle, 40).expect("render"),
            vec![
                "7".to_string(),
                "/w".to_string(),
                "call-1".to_string(),
                "3".to_string(),
                "n=undefined".to_string(),
                "last:none".to_string()
            ]
        );
        // Second call on the same row: the shared `state` kept the first
        // call's component, so the marker flips to "last:same" while
        // everything else renders identically.
        let outcome = runner
            .tool_render_call(resolved.stash_id, r#"{"k":7}"#, CONTEXT)
            .expect("render call");
        let EntryRenderOutcome::Component { handle, .. } = outcome else {
            panic!("expected a component, got {outcome:?}");
        };
        assert_eq!(
            runner.component_render(handle, 40).expect("render"),
            vec![
                "7".to_string(),
                "/w".to_string(),
                "call-1".to_string(),
                "3".to_string(),
                "n=undefined".to_string(),
                "last:same".to_string()
            ]
        );
    }

    /// `renderResult(result, options, theme, context)` receives the tool
    /// result record and the `{expanded, isPartial}` options beside the
    /// context flags (tool-execution.ts:326-331).
    #[test]
    fn tool_render_result_options_and_content_cross() {
        const CONTEXT: &str = r#"{"toolCallId":"call-1","cwd":"/w","executionStarted":true,"argsComplete":true,"isPartial":false,"expanded":true,"showImages":false,"isError":false,"outputPad":1}"#;
        const EXT: &str = r"
export default function (pi) {
  pi.registerToolRenderer(() => ({
    renderResult: (result, options, theme, context) => ({
      render: () => [
        String(options.expanded),
        String(options.isPartial),
        result.content[0].text,
        String(context.isError),
        String(context.isPartial),
      ],
      invalidate: () => {},
    }),
  }));
}
";
        let runner = runner_with(&[EXT]);
        let outcome = runner
            .resolve_tool_renderers("read", true)
            .expect("resolve");
        let ToolRenderersOutcome::Renderers(resolved) = outcome else {
            panic!("expected renderers, got {outcome:?}");
        };
        let outcome = runner
            .tool_render_result(
                resolved.stash_id,
                "{}",
                r#"{"content":[{"type":"text","text":"body text"}],"details":null}"#,
                r#"{"expanded":true,"isPartial":false}"#,
                CONTEXT,
            )
            .expect("render result");
        let EntryRenderOutcome::Component { handle, .. } = outcome else {
            panic!("expected a component, got {outcome:?}");
        };
        assert_eq!(
            runner.component_render(handle, 40).expect("render"),
            vec![
                "true".to_string(),
                "false".to_string(),
                "body text".to_string(),
                "false".to_string(),
                "false".to_string()
            ]
        );
    }

    /// A renderer throw crosses as `Failed(message)` — upstream the catch
    /// falls back to the generic call line (tool-execution.ts:309-313) —
    /// and `tool_renderer_release` drops the stash entry, so a later call
    /// on the released row finds nothing.
    #[test]
    fn tool_render_throw_falls_to_failed_and_release_empties_the_stash() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerToolRenderer(() => ({
    renderCall: () => {
      throw new Error("boom-call");
    },
  }));
}
"#;
        let runner = runner_with(&[EXT]);
        let outcome = runner
            .resolve_tool_renderers("read", true)
            .expect("resolve");
        let ToolRenderersOutcome::Renderers(resolved) = outcome else {
            panic!("expected renderers, got {outcome:?}");
        };
        let outcome = runner
            .tool_render_call(resolved.stash_id, "{}", "{}")
            .expect("render call");
        let EntryRenderOutcome::Failed(message) = outcome else {
            panic!("expected a failure, got {outcome:?}");
        };
        assert_eq!(message, "boom-call");
        runner
            .tool_renderer_release(resolved.stash_id)
            .expect("release");
        let outcome = runner
            .tool_render_call(resolved.stash_id, "{}", "{}")
            .expect("render call");
        assert!(matches!(outcome, EntryRenderOutcome::None), "{outcome:?}");
    }
}
