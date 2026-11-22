//! Host wiring for the `codemode` tool: declarations, nested tool adapters,
//! and the `execute.ts` orchestration on the in-process `QuickJS` sandbox.
//!
//! Ground truth (TypeScript, earendil-works/pi 1.5.2):
//! * `packages/coding-agent/src/extensions/codemode/tool.ts:44-461` —
//!   constants, `toCodemodeDeclaration`, `getCodemodeCallableTools`,
//!   `createCodemodeDescription`, `selectCatalog`, `prepareCodemodeLoadout`,
//!   `createCodemodeToolDefinition`;
//! * `packages/coding-agent/src/extensions/codemode/execute.ts:47-652` —
//!   `readCodemodeStore`, `formatOutput`, `joinAdjacentText`, `formatError`,
//!   `toScriptValue`, `executeCodemode`, `createDiscoveryGlobals`;
//! * `packages/coding-agent/src/extensions/codemode/index.ts:14-47` — the
//!   extension registration and its `codemode.mode` / `inlineBudget` settings;
//! * `packages/coding-agent/src/core/settings-defaults.ts:57-60` — the default
//!   `codemode.inlineBudget` of 3000 tokens.
//!
//! # Seams and deviations
//!
//! Upstream's tool is async (`ctx.executeTool`, `tool_call`/`tool_result`
//! hooks, a worker thread, `appendEntry` on the session). The Rust
//! `pi_codemode::Tool` seam is synchronous, so callers provide a blocking
//! [`ToolInvoker`]; the helper [`execute_via_executor`] adapts the agent's
//! `ToolExecutor` to it with the process-wide runtime. Store writes and reads
//! are callbacks ([`CodemodeHost::with_store_reader`] /
//! [`CodemodeHost::with_append_entry`]) so the coordinator can bind them to
//! `SessionManager::get_branch` / `append_custom_entry`.
//!
//! Two upstream features are deliberately out of scope here and documented in
//! `WIRING.md`: the `models.*` globals (no classifier/image model registry seam
//! exists in this port yet) and worker-pool admission/streaming `onUpdate`
//! progress (the in-process VM is synchronous). The `models` option only
//! affects the declared globals and is off by default in this host.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use base64::Engine;
use pi_agent::types::AgentToolResult;
use pi_ai::types::{ContentBlock, ImageContent, TextContent};
use pi_codemode::{
    AbortFlag, CodemodeError, CodemodeErrorKind, CodemodeResult, CodemodeStoreWrites,
    DEFAULT_INTERRUPT_BUDGET, ExecuteOptions, FnTool, OutputItem, RenderTool, RenderToolOptions,
    Sandbox, SandboxOptions, Tool as CodemodeTool, ToolContext, ToolError, parse_codemode_source,
    render_tool_sample, to_codemode_identifier,
};
use serde::Serialize;
use serde_json::{Map, Value as Json, json};

use crate::session::SessionEntry;
use crate::tools::renderers::{CallContext, ResultContext, ToolRenderers};

/// `CODEMODE_TOOL_NAME` (`tool.ts:44`).
pub const CODEMODE_TOOL_NAME: &str = "codemode";
/// `CODEMODE_STORE_ENTRY_TYPE` (`tool.ts:47`).
pub const CODEMODE_STORE_ENTRY_TYPE: &str = "codemode-store";
/// `SETTINGS_DEFAULTS.codemode.inlineBudget` (`settings-defaults.ts:59`).
pub const DEFAULT_INLINE_BUDGET: u64 = 3000;
/// `DEFAULT_MAX_OUTPUT_TOKENS` (`execute.ts:306`).
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 10_000;
/// `CODEMODE_MEMORY_LIMIT_BYTES` (`execute.ts:56`).
pub const CODEMODE_MEMORY_LIMIT_BYTES: u64 = 256 * 1024 * 1024;
/// `ARGS_PREVIEW_CHARS` (`execute.ts:47`).
const ARGS_PREVIEW_CHARS: usize = 200;
/// `ERROR_PREVIEW_CHARS` (`execute.ts:48`).
const ERROR_PREVIEW_CHARS: usize = 500;
/// `CHARS_PER_TOKEN` (`tool.ts:144`, `execute.ts:308`).
const CHARS_PER_TOKEN: usize = 4;
/// `DEFAULT_TOOL_SEARCH_LIMIT` (`tool-search/tool.ts`).
const DEFAULT_TOOL_SEARCH_LIMIT: usize = 10;

const DESCRIPTION_INTRO: &str = r#"Run JavaScript that calls other tools. The input is raw JavaScript (not JSON, no code fence), run as an async function body in a QuickJS sandbox: top-level `await` and `return` work. No Node, file system, network, or timers.
- `await tools.<name>({ ...args })` resolves to a string, or an object if the tool's declaration says so, and rejects with an Error on failure. A tool whose declaration returns an object resolves on failure too when it returned that object; check its fields. Calls still running when the script ends are cancelled.
- Optional first line: `// @options: {"max_output_tokens": 10000, "timeout_ms": 60000}`"#;

/// `CodemodeMode` (`core/settings-manager.ts`): how the tool presents the
/// loadout while active. Mirrors `codemode.mode` (`"on"` / `"only"`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CodemodeMode {
    /// Declare direct tools as usual; list only script-only tools.
    #[default]
    On,
    /// Hide direct declarations and list every callable tool.
    Only,
}

impl CodemodeMode {
    /// Parse an upstream settings value; anything else falls back to `on`
    /// (`index.ts:23-25`).
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("only") => Self::Only,
            _ => Self::On,
        }
    }

    /// The settings wire value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Only => "only",
        }
    }
}

/// `ToolNamespace` (`core/extensions/types.ts`), reduced to the fields the
/// script-visible declarations need.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NamespaceInfo {
    pub name: String,
    pub description: Option<String>,
    pub instructions: Option<String>,
    pub aliases: Vec<String>,
}

/// A tool's exposure to the model and to scripts (`ToolExposure`). `Hidden`
/// tools are not callable; `Deferred` tools are callable but never listed in
/// the codemode description.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Exposure {
    /// Declared to the model and callable from scripts.
    #[default]
    Direct,
    /// Callable only from scripts (the default for codemode MCP servers).
    Codemode,
    /// Callable only after an explicit load; never listed here.
    Deferred,
    /// Not callable from scripts.
    Hidden,
}

impl Exposure {
    /// Parse a normalized `mcp.json`/extension exposure value.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("deferred" | "codemode-deferred") => Self::Deferred,
            Some("hidden") => Self::Hidden,
            // `codemode` and `direct` both reach scripts.
            _ => Self::Direct,
        }
    }
}

/// One callable agent tool, as the script/declaration layers see it. Mirrors
/// the subset of `AgentTool` used by `toCodemodeDeclaration` and
/// `renderToolSample` (`tool.ts:150-161`, `declarations.rs:44-58`).
#[derive(Debug, Clone, PartialEq)]
pub struct CallableTool {
    pub name: String,
    pub description: String,
    pub parameters: Json,
    pub output_schema: Option<Json>,
    /// Prompt guidelines appended to the description (listed tools only).
    pub guidelines: Vec<String>,
    pub exposure: Exposure,
    pub namespace: Option<NamespaceInfo>,
}

impl CallableTool {
    /// Build a descriptor from a built-in tool definition.
    #[must_use]
    pub fn from_definition(definition: &crate::tools::index::ToolDefinition) -> Self {
        Self {
            name: definition.name.to_string(),
            description: definition.description.clone(),
            parameters: definition.parameters.clone(),
            output_schema: definition.output_schema.clone(),
            guidelines: definition.prompt_guidelines.clone(),
            exposure: Exposure::Direct,
            namespace: None,
        }
    }

    /// A synthetic tool for tests and lightweight hosts.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Json,
        output_schema: Option<Json>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            output_schema,
            guidelines: Vec::new(),
            exposure: Exposure::Codemode,
            namespace: None,
        }
    }

    /// Whether scripts may call this tool (`getCodemodeCallableTools`,
    /// `tool.ts:164-166`, plus the exposure filter).
    #[must_use]
    pub fn is_callable(&self) -> bool {
        self.exposure != Exposure::Hidden && self.name != CODEMODE_TOOL_NAME
    }

    fn description_with_guidelines(&self) -> String {
        let bullets: Vec<String> = self
            .guidelines
            .iter()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .map(|line| format!("- {line}"))
            .collect();
        let head = self.description.trim();
        if bullets.is_empty() {
            head.to_string()
        } else {
            format!("{head}\n\n{}", bullets.join("\n"))
        }
    }
}

/// The host's synchronous tool-call seam. An async agent executor adapts to it
/// with [`execute_via_executor`] (or any other blocking bridge).
pub trait ToolInvoker: Send + Sync {
    /// Call one agent tool and return its (already validated) result.
    fn invoke(
        &self,
        name: &str,
        args: Json,
        call_id: &str,
        signal: &AbortFlag,
    ) -> Result<AgentToolResult, String>;
}

/// Adapt pi-agent's async [`pi_agent::types::ToolExecutor`] to [`ToolInvoker`].
/// This blocks on the shared process runtime; call it from the synchronous
/// codemode VM, never from inside a tokio worker.
pub fn execute_via_executor(
    executor: &dyn pi_agent::types::ToolExecutor,
    name: &str,
    args: Json,
    call_id: &str,
    _signal: &AbortFlag,
) -> Result<AgentToolResult, String> {
    use pi_ai::types::JsonObject;
    use pi_ai::types::ToolCall;

    let arguments: JsonObject = match args {
        Json::Object(map) => map,
        _ => JsonObject::new(),
    };
    let call = ToolCall {
        id: call_id.to_string(),
        name: name.to_string(),
        arguments,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    };
    let (updates, _rx) = tokio::sync::mpsc::unbounded_channel();
    crate::runtime::block_on(executor.execute(call, updates))
}

/// Host-level options, from the extension registration (`index.ts:14-47`).
#[derive(Debug, Clone)]
pub struct CodemodeOptions {
    /// When false, the host neither declares nor executes the tool. The CLI
    /// gates it through `--tools`/`--no-tools` (upstream registers it
    /// `defaultActive: false`); there is no `--no-codemode` flag upstream.
    pub enabled: bool,
    /// `codemode.mode` (`on` / `only`).
    pub mode: CodemodeMode,
    /// `codemode.inlineBudget`, in estimated tokens.
    pub inline_budget: Option<u64>,
    /// Declare `models.*` globals. Off here: no model runtime seam in the
    /// agent host yet (documented deviation).
    pub models: bool,
    /// Per-script default from `// @options:`.
    pub max_output_tokens: u64,
}

impl Default for CodemodeOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: CodemodeMode::On,
            inline_budget: Some(DEFAULT_INLINE_BUDGET),
            models: false,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        }
    }
}

/// `CodemodeNestedCallStatus` (`tool.ts:91`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NestedCallStatus {
    Running,
    Ok,
    Error,
    Cancelled,
}

/// `CodemodeNestedCall` (`tool.ts:93-105`), also the shape of the UI details.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NestedCall {
    pub id: String,
    pub name: String,
    pub args: String,
    pub status: NestedCallStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct CallRecorder {
    calls: Arc<Mutex<Vec<NestedCall>>>,
}

impl CallRecorder {
    fn push(&self, call: NestedCall) -> usize {
        let mut calls = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        calls.push(call);
        calls.len() - 1
    }

    fn update(&self, index: usize, change: impl FnOnce(&mut NestedCall)) {
        let mut calls = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(call) = calls.get_mut(index) {
            change(call);
        }
    }

    fn snapshot(&self) -> Vec<NestedCall> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn cancel_running(&self) {
        let mut calls = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for call in calls
            .iter_mut()
            .filter(|call| call.status == NestedCallStatus::Running)
        {
            call.status = NestedCallStatus::Cancelled;
        }
    }
}

/// `AgentTool` → `pi_codemode::Tool` adapter (`execute.ts:525-552`). Execution
/// goes through [`ToolInvoker`], i.e. the agent loop's tool executor seam, so
/// validation and hooks still apply to nested calls.
struct AgentToolAdapter {
    tool: CallableTool,
    /// Upstream passes `renderToolSample(...)` as the tool's description so
    /// `ALL_TOOLS` carries declarations (`execute.ts:521-527`).
    description: String,
    tool_call_id: String,
    invoker: Arc<dyn ToolInvoker>,
    recorder: CallRecorder,
    sequence: AtomicU64,
}

impl CodemodeTool for AgentToolAdapter {
    fn name(&self) -> &str {
        &self.tool.name
    }

    fn description(&self) -> Option<&str> {
        Some(&self.description)
    }

    fn input_schema(&self) -> Option<&Json> {
        Some(&self.tool.parameters)
    }

    fn output_schema(&self) -> Option<&Json> {
        self.tool.output_schema.as_ref()
    }

    fn execute(&self, args: Json, context: &ToolContext) -> Result<Json, ToolError> {
        let nested_id = format!(
            "{}/{}",
            self.tool_call_id,
            self.sequence.fetch_add(1, Ordering::SeqCst) + 1
        );
        let index = self.recorder.push(NestedCall {
            id: nested_id.clone(),
            name: self.tool.name.clone(),
            args: preview_args(&args),
            status: NestedCallStatus::Running,
            duration_ms: None,
            error: None,
        });
        let started = Instant::now();
        let outcome = (self
            .invoker
            .invoke(&self.tool.name, args, &nested_id, &context.signal))
        .map_err(ToolError::new);
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match outcome {
            Ok(result) => {
                let failed = result.is_error;
                let value = to_script_value(&self.tool, &result);
                let error_text = if failed {
                    Some(truncate_text(
                        &text_of(&result)
                            .unwrap_or_else(|| format!("Tool \"{}\" failed", self.tool.name)),
                        ERROR_PREVIEW_CHARS,
                    ))
                } else {
                    None
                };
                self.recorder.update(index, |call| {
                    call.status = if failed {
                        NestedCallStatus::Error
                    } else {
                        NestedCallStatus::Ok
                    };
                    call.duration_ms = Some(duration_ms);
                    call.error = error_text;
                });
                if failed {
                    Err(ToolError::new(text_of(&result).unwrap_or_else(|| {
                        format!("Tool \"{}\" failed", self.tool.name)
                    })))
                } else {
                    Ok(value)
                }
            }
            Err(error) => {
                self.recorder.update(index, |call| {
                    call.status = NestedCallStatus::Error;
                    call.duration_ms = Some(duration_ms);
                    call.error = Some(truncate_text(&error.message, ERROR_PREVIEW_CHARS));
                });
                Err(error)
            }
        }
    }
}

/// `textOf` (`execute.ts:127-132`): join every text block with newlines.
fn text_of(result: &AgentToolResult) -> Option<String> {
    let texts: Vec<&str> = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

/// `toScriptValue` (`execute.ts:480-486`): structured content for tools with an
/// output schema, text otherwise, and a rejected promise on failure.
fn to_script_value(tool: &CallableTool, result: &AgentToolResult) -> Json {
    if tool.output_schema.is_some()
        && let Some(structured) = &result.structured_content
    {
        return structured.clone();
    }
    Json::String(text_of(result).unwrap_or_default())
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() > max_chars {
        format!("{}...", slice_head(text, max_chars - 3))
    } else {
        text.to_string()
    }
}

fn slice_head(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

fn slice_tail(text: &str, chars: usize) -> String {
    let mut reversed: Vec<char> = text.chars().rev().take(chars).collect();
    reversed.reverse();
    reversed.into_iter().collect()
}

fn preview_args(args: &Json) -> String {
    let text = serde_json::to_string(args).unwrap_or_default();
    truncate_text(&text, ARGS_PREVIEW_CHARS)
}

fn value_text(value: &Json) -> String {
    match value {
        Json::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// `toCodemodeDeclaration` (`tool.ts:150-161`) rendered through the crate's
/// `renderToolSample` (`declarations.rs:87-97`).
fn tool_sample(tool: &CallableTool) -> String {
    let text_schema = json!({ "type": "string" });
    let description = tool.description_with_guidelines();
    let declaration = RenderTool::new(&tool.name)
        .description(&description)
        .input_schema(&tool.parameters)
        .output_schema(tool.output_schema.as_ref().unwrap_or(&text_schema));
    render_tool_sample(&declaration, &RenderToolOptions::default())
}

fn render_tool_section(tool: &CallableTool) -> String {
    let id = to_codemode_identifier(&tool.name);
    let heading = if id == tool.name {
        format!("### `{id}`")
    } else {
        format!("### `{id}` (`{}`)", tool.name)
    };
    format!("{heading}\n{}", tool_sample(tool).trim())
}

/// `describeGlobals` (`tool.ts:128-139`).
fn describe_globals(models: bool) -> String {
    let mut lines = vec![
        "Globals:".to_string(),
        "- `text(value)`, `image(dataUrlOrImageBlock)`, `console.log(...)`, and top-level `return` add output; `exit()` ends the script. With several text items, each starts with a `==> text N/M <==` line, and `console` lines follow the other output in one `<console_output>` block. `image()` also saves the image to a temp file and the result names its path.".to_string(),
        "- `store(key, value)` and `load(key)` keep JSON values across codemode calls.".to_string(),
        "- `ALL_TOOLS`, `await searchTools(query, { limit?, namespace? })`, `await describeTool(name)`, `await describeNamespace(name)`: find unlisted tools, such as MCP tools.".to_string(),
    ];
    if models {
        lines.push(
            "- `models`: classifiers and image generation. Read the codemode docs first."
                .to_string(),
        );
    }
    lines.join("\n")
}

/// Options of [`create_codemode_description`] (`tool.ts:168-182`).
#[derive(Debug, Clone)]
pub struct DescriptionOptions<'a> {
    /// Declare the `models` namespace.
    pub models: bool,
    /// Callable but never listed (`deferred` exposure).
    pub deferred: &'a BTreeSet<String>,
    /// Estimated tokens the tool sections may use.
    pub inline_budget: Option<u64>,
}

#[derive(Debug, Clone)]
struct CatalogEntry {
    name: String,
    section: String,
    cost: usize,
}

#[derive(Debug, Clone)]
struct CatalogGroup {
    namespace: Option<NamespaceInfo>,
    entries: Vec<CatalogEntry>,
}

/// `selectCatalog` (`tool.ts:208-225`): round-robin across groups, cheapest
/// first, so every namespace is represented before any is complete.
fn select_catalog(groups: &[CatalogGroup], budget: Option<u64>) -> BTreeSet<String> {
    let Some(budget) = budget else {
        return groups
            .iter()
            .flat_map(|group| group.entries.iter().map(|entry| entry.name.clone()))
            .collect();
    };
    let mut queues: Vec<Vec<CatalogEntry>> = groups
        .iter()
        .map(|group| {
            let mut entries = group.entries.clone();
            entries.sort_by_key(|entry| entry.cost);
            entries
        })
        .collect();
    let mut shown = BTreeSet::new();
    let mut remaining = usize::try_from(budget).unwrap_or(usize::MAX);
    loop {
        let mut active = false;
        for queue in &mut queues {
            let Some(next) = queue.first() else { continue };
            if next.cost > remaining {
                continue;
            }
            remaining -= next.cost;
            shown.insert(next.name.clone());
            queue.remove(0);
            active = true;
        }
        if !active {
            break;
        }
    }
    shown
}

/// `createCodemodeDescription` (`tool.ts:233-275`) with the loadout mode
/// applied by the caller. Deferred and hidden tools are never listed.
#[must_use]
#[allow(clippy::too_many_lines)] // mirrors the TS createCodemodeDescription; split when extending
pub fn create_codemode_description(
    tools: &[CallableTool],
    options: &DescriptionOptions<'_>,
) -> String {
    let declarations: Vec<&CallableTool> = tools
        .iter()
        .filter(|tool| tool.is_callable() && !options.deferred.contains(&tool.name))
        .collect();
    let mut groups: Vec<CatalogGroup> = vec![CatalogGroup {
        namespace: None,
        entries: Vec::new(),
    }];
    for tool in &declarations {
        let section = render_tool_section(tool);
        let entry = CatalogEntry {
            name: tool.name.clone(),
            cost: section.chars().count().div_ceil(CHARS_PER_TOKEN),
            section,
        };
        match &tool.namespace {
            Some(namespace) => {
                if let Some(group) = groups.iter_mut().find(|group| {
                    group
                        .namespace
                        .as_ref()
                        .is_some_and(|n| n.name == namespace.name)
                }) {
                    group.entries.push(entry);
                } else {
                    groups.push(CatalogGroup {
                        namespace: Some(namespace.clone()),
                        entries: vec![entry],
                    });
                }
            }
            None => groups[0].entries.push(entry),
        }
    }
    groups.sort_by(|a, b| match (&a.namespace, &b.namespace) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(a), Some(b)) => a.name.cmp(&b.name),
    });

    let sections = [
        DESCRIPTION_INTRO.to_string(),
        describe_globals(options.models),
    ];
    if declarations.is_empty() {
        return sections.join("\n\n");
    }
    let shown = select_catalog(&groups, options.inline_budget);
    let mut tool_sections = vec!["Nested tools:".to_string()];
    for group in &groups {
        let visible: Vec<&CatalogEntry> = group
            .entries
            .iter()
            .filter(|entry| shown.contains(&entry.name))
            .collect();
        if let Some(namespace) = &group.namespace {
            let listing = if visible.len() == group.entries.len() {
                ""
            } else if visible.is_empty() {
                " (tools not listed)"
            } else {
                " (some tools not listed)"
            };
            let mut heading = format!("## {}{listing}", namespace.name);
            if let Some(description) = &namespace.description
                && !description.trim().is_empty()
            {
                heading.push('\n');
                heading.push_str(description.trim());
            }
            tool_sections.push(heading);
        }
        for entry in visible {
            tool_sections.push(entry.section.clone());
        }
    }
    let mut result = sections.to_vec();
    result.push(tool_sections.join("\n\n"));
    result.join("\n\n")
}

fn empty_store() -> BTreeMap<String, Json> {
    BTreeMap::new()
}

/// `readCodemodeStore` (`execute.ts:293-303`): fold the branch's
/// `codemode-store` entries from root to leaf.
#[must_use]
pub fn read_store(branch: &[SessionEntry]) -> BTreeMap<String, Json> {
    let mut store = BTreeMap::new();
    for entry in branch {
        let SessionEntry::Custom(custom) = entry else {
            continue;
        };
        if custom.custom_type != CODEMODE_STORE_ENTRY_TYPE {
            continue;
        }
        let Some(data) = &custom.data else { continue };
        let Some(delete) = data.get("delete").and_then(Json::as_array) else {
            continue;
        };
        let Some(set) = data.get("set").and_then(Json::as_object) else {
            continue;
        };
        for key in delete.iter().filter_map(Json::as_str) {
            store.remove(key);
        }
        for (key, value) in set {
            store.insert(key.clone(), value.clone());
        }
    }
    store
}

/// The JSON payload written with `appendEntry` (`tool.ts:49-52`).
#[must_use]
pub fn store_entry_data(writes: &CodemodeStoreWrites) -> Json {
    json!({ "set": writes.set, "delete": writes.delete })
}

/// `formatOutput` (`execute.ts:322-341`), including the `console_output`
/// block and `==> text N/M <==` separators.
fn format_output(output: &[OutputItem]) -> Vec<OutputItem> {
    let total = output
        .iter()
        .filter(|item| matches!(item, OutputItem::Text { console: false, .. }))
        .count();
    let mut items = Vec::new();
    let mut console_lines = Vec::new();
    let mut index = 0;
    for item in output {
        match item {
            OutputItem::Image { .. } => items.push(item.clone()),
            OutputItem::Text {
                text,
                console: true,
            } => console_lines.push(text.clone()),
            OutputItem::Text {
                text,
                console: false,
            } => {
                index += 1;
                let body = if total > 1 {
                    format!("==> text {index}/{total} <==\n{text}")
                } else {
                    text.clone()
                };
                items.push(OutputItem::text(body));
            }
        }
    }
    if !console_lines.is_empty() {
        items.push(OutputItem::text(format!(
            "<console_output>\n{}\n</console_output>",
            console_lines.join("\n")
        )));
    }
    items
}

/// `joinAdjacentText` (`execute.ts:348-372`): providers may join adjacent
/// text blocks, so insert explicit newlines between parts.
fn join_adjacent_text(items: Vec<OutputItem>) -> Vec<OutputItem> {
    let mut joined: Vec<OutputItem> = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    let mut last_part = String::new();
    for item in items {
        match item {
            OutputItem::Text { text, .. } => {
                if !last_part.is_empty() && !last_part.ends_with('\n') {
                    parts.push("\n".to_string());
                    last_part = "\n".to_string();
                }
                if !text.is_empty() {
                    last_part.clone_from(&text);
                }
                parts.push(text);
            }
            other @ OutputItem::Image { .. } => {
                if !parts.is_empty() {
                    joined.push(OutputItem::text(std::mem::take(&mut parts).join("")));
                }
                last_part.clear();
                joined.push(other);
            }
        }
    }
    if !parts.is_empty() {
        joined.push(OutputItem::text(parts.join("")));
    }
    joined
}

impl NestedCall {
    fn status_label(&self) -> &'static str {
        match self.status {
            NestedCallStatus::Running => "running",
            NestedCallStatus::Ok => "ok",
            NestedCallStatus::Error => "error",
            NestedCallStatus::Cancelled => "cancelled",
        }
    }
}

/// `formatCallSummary` / `formatError` (`execute.ts:374-390`).
fn format_error(error: &CodemodeError, calls: &[NestedCall]) -> String {
    let head = match error.kind {
        CodemodeErrorKind::Script => error.stack.clone().unwrap_or_else(|| {
            let name = error.name.as_deref().unwrap_or("Error");
            format!("{name}: {}", error.message)
        }),
        CodemodeErrorKind::Timeout => format!("Script timed out: {}", error.message),
        CodemodeErrorKind::Aborted => format!("Script aborted: {}", error.message),
        CodemodeErrorKind::Sandbox => format!("Script sandbox failed: {}", error.message),
    };
    let summary = if calls.is_empty() {
        "No tool calls were made.".to_string()
    } else {
        let list = calls
            .iter()
            .map(|call| format!("{} ({})", call.name, call.status_label()))
            .collect::<Vec<_>>()
            .join(", ");
        format!("Tool calls made before the failure (they are not undone): {list}")
    };
    format!("{head}\n\n{summary}")
}

fn write_output_file(prefix: &str, extension: &str, bytes: &[u8]) -> Result<String, String> {
    let path = std::env::temp_dir().join(format!("{prefix}-{}.{extension}", uuid::Uuid::new_v4()));
    std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

/// `saveImages` (`execute.ts:414-442`): put a text path label before each
/// image; the model has no other way to reach the bytes.
fn save_images(items: Vec<OutputItem>) -> Vec<OutputItem> {
    let mut saved: BTreeMap<String, String> = BTreeMap::new();
    let mut result = Vec::new();
    for item in items {
        let OutputItem::Image { data, mime_type } = &item else {
            result.push(item);
            continue;
        };
        let extension = match mime_type.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => "bin",
        };
        let label = saved.entry(data.clone()).or_insert_with(|| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap_or_default();
            let size = format_size(bytes.len());
            match write_output_file("pi-codemode", extension, &bytes) {
                Ok(path) => format!("[Image saved to {path} ({mime_type}, {size})]"),
                Err(error) => format!("[Image ({mime_type}, {size}) could not be saved: {error}]"),
            }
        });
        result.push(OutputItem::text(label.clone()));
        result.push(item);
    }
    result
}

fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} bytes")
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

/// `truncateOutput` (`execute.ts:449-472`): keep head and tail, spill the full
/// text to a temp file, and name that file in the result.
fn truncate_output(items: Vec<OutputItem>, max_tokens: u64) -> (Vec<OutputItem>, Option<String>) {
    let texts: Vec<String> = items
        .iter()
        .filter_map(|item| match item {
            OutputItem::Text { text, .. } => Some(text.clone()),
            OutputItem::Image { .. } => None,
        })
        .collect();
    let combined = texts.join("\n");
    let budget = usize::try_from(max_tokens).unwrap_or(usize::MAX) * CHARS_PER_TOKEN;
    if texts.is_empty() || combined.chars().count() <= budget {
        return (items, None);
    }
    let head_chars = budget / 2;
    let tail_chars = budget - head_chars;
    let removed = combined.chars().count() - head_chars - tail_chars;
    let mut text = format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: {}\n\n{}…{} tokens truncated…{}",
        combined.chars().count().div_ceil(CHARS_PER_TOKEN),
        combined.lines().count(),
        slice_head(&combined, head_chars),
        removed.div_ceil(CHARS_PER_TOKEN),
        slice_tail(&combined, tail_chars),
    );
    let full = match write_output_file("pi-codemode", "txt", combined.as_bytes()) {
        Ok(path) => {
            let _ = write!(text, "\n\n[Full output: {path} (read with offset/limit)]");
            Some(path)
        }
        Err(error) => {
            let _ = write!(text, "\n\n[Could not save the full output: {error}]");
            None
        }
    };
    let mut result = vec![OutputItem::text(text)];
    result.extend(
        items
            .into_iter()
            .filter(|item| matches!(item, OutputItem::Image { .. })),
    );
    (result, full)
}

/// A minimal port of `renderer.ts:71-166`: the call row previews the script,
/// the result row lists nested calls and the script output without its header.
/// The full component-based renderer belongs to the TUI bridging slice.
const CODEMODE_RENDERERS: ToolRenderers = ToolRenderers {
    render_call: |args: Option<&Json>, context: &CallContext<'_>| {
        let code = args
            .and_then(|value| value.get("code"))
            .and_then(Json::as_str)
            .unwrap_or("");
        let preview = code.lines().take(10).collect::<Vec<_>>().join("\n");
        if context.expanded {
            format!("codemode\n{preview}")
        } else {
            format!("codemode {preview}")
        }
    },
    render_result: |_args: Option<&Json>, result: &AgentToolResult, context: &ResultContext<'_>| {
        let mut lines = Vec::new();
        if let Some(calls) = result.details.get("calls").and_then(Json::as_array) {
            for call in calls {
                let name = call["name"].as_str().unwrap_or("?");
                let status = call["status"].as_str().unwrap_or("?");
                lines.push(format!("{name} ({status})"));
            }
        }
        for block in &result.content {
            let Some(text) = block.as_text() else {
                continue;
            };
            if text.starts_with("Script completed\n") || text.starts_with("Script failed\n") {
                continue;
            }
            lines.push(text.to_string());
        }
        if let Some(timer) = &context.timer_text {
            lines.push(timer.clone());
        }
        lines.join("\n")
    },
};

/// The codemode tool host. Cloneable handles share tools/invoker/settings;
/// execution state (nested calls) is per call.
/// Callback the host supplies to persist an entry into the session.
type AppendEntryFn = Arc<dyn Fn(&str, Json) + Send + Sync>;

#[derive(Clone)]
pub struct CodemodeHost {
    tools: Vec<CallableTool>,
    invoker: Arc<dyn ToolInvoker>,
    options: CodemodeOptions,
    store_reader: Arc<dyn Fn() -> BTreeMap<String, Json> + Send + Sync>,
    append_entry: Option<AppendEntryFn>,
}

impl std::fmt::Debug for CodemodeHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodemodeHost")
            .field("tools", &self.tools)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl CodemodeHost {
    /// Create a host over the callable tools and a blocking tool invoker.
    #[must_use]
    pub fn new(tools: Vec<CallableTool>, invoker: impl ToolInvoker + 'static) -> Self {
        Self {
            tools: tools
                .into_iter()
                .filter(CallableTool::is_callable)
                .collect(),
            invoker: Arc::new(invoker),
            options: CodemodeOptions::default(),
            store_reader: Arc::new(empty_store),
            append_entry: None,
        }
    }

    /// Set the extension options (`mode`, `inlineBudget`, exposure gate).
    #[must_use]
    pub fn with_options(mut self, options: CodemodeOptions) -> Self {
        self.options = options;
        self
    }

    /// Read `load()` values from the current session branch.
    #[must_use]
    pub fn with_store_reader(
        mut self,
        reader: impl Fn() -> BTreeMap<String, Json> + Send + Sync + 'static,
    ) -> Self {
        self.store_reader = Arc::new(reader);
        self
    }

    /// Persist successful `store()` writes as `codemode-store` custom entries.
    #[must_use]
    pub fn with_append_entry(
        mut self,
        append: impl Fn(&str, Json) + Send + Sync + 'static,
    ) -> Self {
        self.append_entry = Some(Arc::new(append));
        self
    }

    /// The callable tools (after the codemode/self and hidden filters).
    #[must_use]
    pub fn tools(&self) -> &[CallableTool] {
        &self.tools
    }

    /// Whether the host is enabled by the exposure/CLI gate.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.options.enabled
    }

    /// The model-facing description, with `prepareCodemodeLoadout` applied:
    /// `on` lists only script-only tools, `only` lists every callable tool.
    #[must_use]
    pub fn description(&self) -> String {
        let deferred: BTreeSet<String> = self
            .tools
            .iter()
            .filter(|tool| tool.exposure == Exposure::Deferred)
            .map(|tool| tool.name.clone())
            .collect();
        let listed: Vec<CallableTool> = match self.options.mode {
            CodemodeMode::On => self
                .tools
                .iter()
                .filter(|tool| tool.exposure != Exposure::Direct)
                .cloned()
                .collect(),
            CodemodeMode::Only => self.tools.clone(),
        };
        create_codemode_description(
            &listed,
            &DescriptionOptions {
                models: self.options.models,
                deferred: &deferred,
                inline_budget: self.options.inline_budget,
            },
        )
    }

    /// The `ToolDefinition` the coordinator registers with the tool registry
    /// (`tool.ts:421-443`). Execution is dispatched by name to [`Self::execute`].
    #[must_use]
    pub fn tool_definition(&self) -> crate::tools::index::ToolDefinition {
        crate::tools::index::ToolDefinition {
            name: CODEMODE_TOOL_NAME,
            label: CODEMODE_TOOL_NAME,
            description: self.description(),
            parameters: json!({
                "type": "object",
                "properties": { "code": { "type": "string", "description": "Raw JavaScript source." } },
                "required": ["code"]
            }),
            output_schema: None,
            prompt_snippet: Some("Run JavaScript that calls other tools"),
            prompt_guidelines: vec![
                "Use codemode to batch independent tool calls (Promise.allSettled), chain them, or filter large output, instead of many separate calls.".to_string(),
            ],
            // `constrainedSampling`: the Lark source grammar cannot be
            // represented by the port's `ConstrainedSampling` enum without an
            // upstream wire-format change; documented as a deviation.
            constrained_sampling: None,
            prepare_arguments: None,
            // Scripts are one tool call that issues many nested calls.
            execution_mode: Some(pi_agent::types::ToolExecutionMode::Sequential),
            render_shell: None,
            renderers: CODEMODE_RENDERERS,
        }
    }

    /// Execute the tool input (`{"code": ...}`) produced by the model.
    #[must_use]
    pub fn execute_input(
        &self,
        tool_call_id: &str,
        input: &Json,
        signal: Option<AbortFlag>,
    ) -> AgentToolResult {
        let code = input.get("code").and_then(Json::as_str).unwrap_or("");
        self.execute_code(tool_call_id, code, signal)
    }

    /// Run one script (`execute.ts:492-652`), returning the model-facing result.
    #[must_use]
    #[allow(clippy::too_many_lines)] // mirrors the TS executeCodemode; split when extending
    pub fn execute_code(
        &self,
        tool_call_id: &str,
        code: &str,
        signal: Option<AbortFlag>,
    ) -> AgentToolResult {
        let started = Instant::now();
        if !self.options.enabled {
            return failure_result(
                tool_call_id,
                "Script error:\ncodemode is disabled by the tool exposure gate",
                &[],
                started,
            );
        }
        let parsed = match parse_codemode_source(code) {
            Ok(parsed) => parsed,
            Err(error) => {
                return failure_result(
                    tool_call_id,
                    &format!("Script error:\n{error}"),
                    &[],
                    started,
                );
            }
        };
        let recorder = CallRecorder::default();
        let sandbox_tools: Vec<Arc<dyn CodemodeTool>> = self
            .tools
            .iter()
            .map(|tool| {
                Arc::new(AgentToolAdapter {
                    tool: tool.clone(),
                    description: tool_sample(tool),
                    tool_call_id: tool_call_id.to_string(),
                    invoker: Arc::clone(&self.invoker),
                    recorder: recorder.clone(),
                    sequence: AtomicU64::new(0),
                }) as Arc<dyn CodemodeTool>
            })
            .collect();
        let globals = discovery_globals(&self.tools);
        let sandbox = match Sandbox::new(SandboxOptions {
            tools: sandbox_tools,
            globals,
            timeout_ms: parsed.options.timeout_ms,
            memory_limit_bytes: Some(CODEMODE_MEMORY_LIMIT_BYTES),
            interrupt_budget: DEFAULT_INTERRUPT_BUDGET,
        }) {
            Ok(sandbox) => sandbox,
            Err(error) => {
                return failure_result(
                    tool_call_id,
                    &format!("Script error:\nScript sandbox failed: {error}"),
                    &[],
                    started,
                );
            }
        };
        let store = (self.store_reader)();
        let result = sandbox.execute(
            &parsed.code,
            ExecuteOptions {
                store: Some(store),
                timeout_ms: parsed.options.timeout_ms,
                signal,
            },
        );
        sandbox.close();
        recorder.cancel_running();

        let mut output = result.output().to_vec();
        let mut return_value: Option<Json> = None;
        let mut is_error = false;
        match &result {
            CodemodeResult::Ok {
                value,
                store_writes: writes,
                ..
            } => {
                if (!writes.set.is_empty() || !writes.delete.is_empty())
                    && let Some(append) = &self.append_entry
                {
                    append(CODEMODE_STORE_ENTRY_TYPE, store_entry_data(writes));
                }
                if !value.is_null() {
                    return_value = Some(value.clone());
                }
            }
            CodemodeResult::Err { error, .. } => {
                is_error = true;
                output.push(OutputItem::text(format!(
                    "Script error:\n{}",
                    format_error(error, &recorder.snapshot())
                )));
            }
        }
        if let Some(value) = return_value {
            output.push(OutputItem::text(value_text(&value)));
        }
        let items = join_adjacent_text(format_output(&output));
        let (items, full_output_path) = truncate_output(
            items,
            parsed
                .options
                .max_output_tokens
                .unwrap_or(self.options.max_output_tokens),
        );
        let items = join_adjacent_text(save_images(items));

        let calls = recorder.snapshot();
        let header = format!(
            "Script {}\nWall time {:.1} seconds\nOutput:\n",
            if is_error { "failed" } else { "completed" },
            started.elapsed().as_secs_f64(),
        );
        let mut content = vec![ContentBlock::Text(TextContent {
            text: header,
            text_signature: None,
        })];
        for item in items {
            content.push(match item {
                OutputItem::Text { text, .. } => ContentBlock::Text(TextContent {
                    text,
                    text_signature: None,
                }),
                OutputItem::Image { data, mime_type } => {
                    ContentBlock::Image(ImageContent { data, mime_type })
                }
            });
        }
        let mut details = Map::new();
        details.insert(
            "calls".to_string(),
            serde_json::to_value(&calls).unwrap_or_else(|_| json!([])),
        );
        if let Some(path) = full_output_path {
            details.insert("fullOutputPath".to_string(), Json::String(path));
        }
        AgentToolResult {
            content,
            details: Json::Object(details),
            is_error,
            ..AgentToolResult::default()
        }
    }
}

fn failure_result(
    _tool_call_id: &str,
    error_text: &str,
    calls: &[NestedCall],
    started: Instant,
) -> AgentToolResult {
    AgentToolResult {
        content: vec![
            ContentBlock::Text(TextContent {
                text: format!(
                    "Script failed\nWall time {:.1} seconds\nOutput:\n",
                    started.elapsed().as_secs_f64()
                ),
                text_signature: None,
            }),
            ContentBlock::Text(TextContent {
                text: error_text.to_string(),
                text_signature: None,
            }),
        ],
        details: json!({ "calls": calls }),
        is_error: true,
        ..AgentToolResult::default()
    }
}

/// `createDiscoveryGlobals` (`execute.ts:669-735`) on the in-process engine.
/// Ranking is a deterministic token-overlap score rather than upstream's BM25
/// (`tool-search/tool.ts`); the names, argument validation and return shapes
/// match upstream.
#[allow(clippy::too_many_lines)] // mirrors the TS createDiscoveryGlobals; split when extending
fn discovery_globals(tools: &[CallableTool]) -> Vec<Arc<dyn CodemodeTool>> {
    let samples: BTreeMap<String, String> = tools
        .iter()
        .map(|tool| (tool.name.clone(), tool_sample(tool)))
        .collect();
    let samples_for_search = samples.clone();

    let search_tools: Arc<dyn CodemodeTool> = Arc::new(
        FnTool::new(
            "searchTools",
            Box::new({
                let tools = tools.to_vec();
                let samples = samples_for_search;
                move |args: Json, _context: &ToolContext| {
                    let values = args.as_array().cloned().unwrap_or_default();
                    let Some(query) = values.first().and_then(Json::as_str) else {
                        return Err(ToolError::new("searchTools() expects a query string"));
                    };
                    let options = values.get(1);
                    let limit = options
                        .and_then(|value| value.get("limit"))
                        .and_then(Json::as_u64)
                        .unwrap_or(DEFAULT_TOOL_SEARCH_LIMIT as u64);
                    if limit == 0 {
                        return Err(ToolError::new(
                            "searchTools() limit must be a positive integer",
                        ));
                    }
                    let namespace = options
                        .and_then(|value| value.get("namespace"))
                        .and_then(Json::as_str);
                    let tokens: Vec<String> =
                        query.split_whitespace().map(str::to_lowercase).collect();
                    let mut matches: Vec<(u64, String)> = tools
                        .iter()
                        .filter(|tool| {
                            namespace.is_none()
                                || tool.namespace.as_ref().is_some_and(|info| {
                                    info.name.as_str() == namespace.unwrap_or("")
                                        || info
                                            .aliases
                                            .iter()
                                            .any(|alias| alias.as_str() == namespace.unwrap_or(""))
                                })
                        })
                        .filter_map(|tool| {
                            let haystack =
                                format!("{} {}", tool.name, tool.description).to_lowercase();
                            let score = tokens
                                .iter()
                                .map(|token| u64::from(haystack.contains(token)))
                                .sum::<u64>();
                            (score > 0).then(|| (score, to_codemode_identifier(&tool.name)))
                        })
                        .collect();
                    matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                    let matches: Vec<Json> = matches
                        .into_iter()
                        .take(usize::try_from(limit).unwrap_or(usize::MAX))
                        .map(|(_, name)| {
                            json!({ "name": name, "description": samples_get(&samples, &name) })
                        })
                        .collect();
                    Ok(Json::Array(matches))
                }
            }),
        )
        .spread(true),
    );

    let describe_tool: Arc<dyn CodemodeTool> = Arc::new(
        FnTool::new(
            "describeTool",
            Box::new({
                let tools = tools.to_vec();
                move |args: Json, _context: &ToolContext| {
                    let Some(name) = args
                        .as_array()
                        .and_then(|items| items.first())
                        .and_then(Json::as_str)
                    else {
                        return Err(ToolError::new("describeTool() expects a tool name"));
                    };
                    let found = tools.iter().find(|tool| {
                        tool.name == name || to_codemode_identifier(&tool.name) == name
                    });
                    Ok(found.map_or(Json::Null, |tool| {
                        Json::String(samples.get(&tool.name).cloned().unwrap_or_default())
                    }))
                }
            }),
        )
        .spread(true),
    );

    let describe_namespace: Arc<dyn CodemodeTool> = Arc::new(
        FnTool::new(
            "describeNamespace",
            Box::new({
                let tools = tools.to_vec();
                move |args: Json, _context: &ToolContext| {
                    let Some(name) = args
                        .as_array()
                        .and_then(|items| items.first())
                        .and_then(Json::as_str)
                    else {
                        return Err(ToolError::new(
                            "describeNamespace() expects a namespace name",
                        ));
                    };
                    let matching: Vec<&CallableTool> = tools
                        .iter()
                        .filter(|tool| {
                            tool.namespace.as_ref().is_some_and(|info| {
                                info.name == name
                                    || to_codemode_identifier(&info.name)
                                        == to_codemode_identifier(name)
                                    || info.aliases.iter().any(|alias| alias == name)
                                    || info.aliases.iter().any(|alias| {
                                        to_codemode_identifier(alias)
                                            == to_codemode_identifier(name)
                                    })
                            })
                        })
                        .collect();
                    let Some(info) = matching.first().and_then(|tool| tool.namespace.as_ref())
                    else {
                        return Ok(Json::Null);
                    };
                    let mut result = Map::new();
                    result.insert("name".to_string(), Json::String(info.name.clone()));
                    if let Some(description) = &info.description {
                        result.insert("description".to_string(), Json::String(description.clone()));
                    }
                    if let Some(instructions) = &info.instructions {
                        result.insert(
                            "instructions".to_string(),
                            Json::String(instructions.clone()),
                        );
                    }
                    result.insert(
                        "tools".to_string(),
                        Json::Array(
                            matching
                                .iter()
                                .map(|tool| Json::String(to_codemode_identifier(&tool.name)))
                                .collect(),
                        ),
                    );
                    Ok(Json::Object(result))
                }
            }),
        )
        .spread(true),
    );

    vec![search_tools, describe_tool, describe_namespace]
}

fn samples_get(samples: &BTreeMap<String, String>, identifier: &str) -> Json {
    samples
        .iter()
        .find(|(name, _)| to_codemode_identifier(name) == identifier)
        .map_or(Json::Null, |(_, sample)| Json::String(sample.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_agent::types::ToolSpec;

    fn ok_result(text: &str) -> AgentToolResult {
        AgentToolResult {
            content: vec![ContentBlock::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            ..AgentToolResult::default()
        }
    }

    struct FnInvoker<F>(F);

    impl<F> ToolInvoker for FnInvoker<F>
    where
        F: Fn(&str, Json, &str, &AbortFlag) -> Result<AgentToolResult, String> + Send + Sync,
    {
        fn invoke(
            &self,
            name: &str,
            args: Json,
            call_id: &str,
            signal: &AbortFlag,
        ) -> Result<AgentToolResult, String> {
            (self.0)(name, args, call_id, signal)
        }
    }

    fn echo_tool() -> CallableTool {
        CallableTool::new(
            "echo",
            "Echo a value back.",
            json!({ "type": "object", "properties": { "n": { "type": "number" } } }),
            Some(json!({ "type": "object", "properties": { "value": { "type": "number" } } })),
        )
    }

    fn host<F>(tools: Vec<CallableTool>, invoker: F) -> CodemodeHost
    where
        F: Fn(&str, Json, &str, &AbortFlag) -> Result<AgentToolResult, String>
            + Send
            + Sync
            + 'static,
    {
        CodemodeHost::new(tools, FnInvoker(invoker))
    }

    fn echo_invoker()
    -> impl Fn(&str, Json, &str, &AbortFlag) -> Result<AgentToolResult, String> + Send + Sync + 'static
    {
        |name, args, _call_id, _signal| {
            if name != "echo" {
                return Err(format!("Tool {name} not found"));
            }
            let n = args.get("n").and_then(Json::as_i64).unwrap_or(0);
            Ok(AgentToolResult {
                content: vec![ContentBlock::Text(TextContent {
                    text: format!("echo:{n}"),
                    text_signature: None,
                })],
                structured_content: Some(json!({ "value": n })),
                ..AgentToolResult::default()
            })
        }
    }

    fn text_of_result(result: &AgentToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn declaration_renders_name_schema_and_guidelines() {
        let mut tool = echo_tool();
        tool.guidelines = vec!["  Batch independent calls.  ".to_string(), " ".to_string()];
        let host = host(vec![tool], echo_invoker());
        let description = host.description();
        assert!(description.contains("### `echo`"), "{description}");
        assert!(description.contains("declare const tools"), "{description}");
        assert!(
            description.contains("Batch independent calls."),
            "{description}"
        );
        assert!(description.contains("Promise<"), "{description}");
    }

    #[test]
    fn description_hides_deferred_and_hidden_tools() {
        let mut deferred = echo_tool();
        deferred.name = "mcp__docs__search".to_string();
        deferred.exposure = Exposure::Deferred;
        let mut hidden = echo_tool();
        hidden.name = "secret".to_string();
        hidden.exposure = Exposure::Hidden;
        let host = host(vec![echo_tool(), deferred, hidden], echo_invoker());
        assert_eq!(host.tools().len(), 2); // echo + the deferred tool
        let description = host.description();
        assert!(description.contains("### `echo`"), "{description}");
        assert!(!description.contains("mcp__docs__search"), "{description}");
        assert!(!description.contains("secret"), "{description}");
    }

    #[test]
    fn mode_only_lists_direct_tools_but_on_does_not() {
        let mut direct = echo_tool();
        direct.exposure = Exposure::Direct;
        let options = CodemodeOptions {
            mode: CodemodeMode::Only,
            ..CodemodeOptions::default()
        };
        let only = host(vec![direct.clone()], echo_invoker()).with_options(options);
        assert!(
            only.description().contains("### `echo`"),
            "{}",
            only.description()
        );
        let on = host(vec![direct], echo_invoker());
        let description = on.description();
        assert!(!description.contains("### `echo`"), "{description}");
        assert!(description.contains("Run JavaScript"), "{description}");
    }

    #[test]
    fn inline_budget_drops_tools_that_do_not_fit() {
        let mut first = echo_tool();
        first.name = "alpha".to_string();
        let mut second = echo_tool();
        second.name = "zeta".to_string();
        let options = CodemodeOptions {
            inline_budget: Some(1),
            ..CodemodeOptions::default()
        };
        let host = host(vec![first, second], echo_invoker()).with_options(options);
        let description = host.description();
        assert!(!description.contains("### `alpha`"), "{description}");
        assert!(!description.contains("### `zeta`"), "{description}");
    }

    #[test]
    fn tool_call_round_trip_uses_structured_content() {
        let host = host(vec![echo_tool()], echo_invoker());
        let result = host.execute_code(
            "call-1",
            "const r = await tools.echo({ n: 21 }); return r.value * 2;",
            None,
        );
        let text = text_of_result(&result);
        assert!(!result.is_error, "{text}");
        assert!(text.contains("Script completed"), "{text}");
        assert!(text.contains("42"), "{text}");
        assert_eq!(result.details["calls"][0]["name"], "echo");
        assert_eq!(result.details["calls"][0]["status"], "ok");
    }

    #[test]
    fn failing_tool_rejects_and_marks_the_call_error() {
        let host = host(vec![echo_tool()], |_name, _args, _id, _signal| {
            Err("permission denied".to_string())
        });
        let result = host.execute_code(
            "call-2",
            "try { await tools.echo({ n: 1 }); } catch (e) { text(e.message); } return 'done';",
            None,
        );
        let text = text_of_result(&result);
        assert!(!result.is_error, "{text}");
        assert!(text.contains("permission denied"), "{text}");
        assert_eq!(result.details["calls"][0]["status"], "error");
        assert_eq!(result.details["calls"][0]["error"], "permission denied");
    }

    #[test]
    fn unhandled_tool_error_fails_the_script_with_a_call_summary() {
        let host = host(vec![echo_tool()], |_name, _args, _id, _signal| {
            Ok(AgentToolResult {
                content: vec![ContentBlock::Text(TextContent {
                    text: "nope".to_string(),
                    text_signature: None,
                })],
                is_error: true,
                ..AgentToolResult::default()
            })
        });
        let result =
            host.execute_code("call-3", "await tools.echo({ n: 1 }); text('never');", None);
        let text = text_of_result(&result);
        assert!(result.is_error, "{text}");
        assert!(text.contains("Script failed"), "{text}");
        assert!(text.contains("Script error:"), "{text}");
        assert!(
            text.contains("Tool calls made before the failure"),
            "{text}"
        );
    }

    #[test]
    fn store_writes_are_persisted_and_read_from_the_branch() {
        let appended: Arc<Mutex<Vec<(String, Json)>>> = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&appended);
        let host = host(vec![], |_name, _args, _id, _signal| Ok(ok_result("")))
            .with_store_reader(|| BTreeMap::from([("seed".to_string(), json!(5))]))
            .with_append_entry(move |kind, data| {
                capture
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((kind.to_string(), data));
            });
        let result = host.execute_code(
            "call-4",
            "store('answer', load('seed') + 1); store('gone', undefined); return load('answer');",
            None,
        );
        assert!(!result.is_error, "{}", text_of_result(&result));
        assert!(
            text_of_result(&result).contains('6'),
            "{}",
            text_of_result(&result)
        );
        let entries = appended
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, CODEMODE_STORE_ENTRY_TYPE);
        assert_eq!(entries[0].1["set"]["answer"], json!(6));
        assert_eq!(entries[0].1["delete"][0], "gone");
    }

    #[test]
    fn output_formatting_labels_multiple_text_items_and_console() {
        let host = host(vec![], |_name, _args, _id, _signal| Ok(ok_result("")));
        let result = host.execute_code(
            "call-5",
            "text('first'); console.log('debug'); text('second');",
            None,
        );
        let text = text_of_result(&result);
        assert!(text.contains("==> text 1/2 <=="), "{text}");
        assert!(text.contains("==> text 2/2 <=="), "{text}");
        assert!(
            text.contains("<console_output>\ndebug\n</console_output>"),
            "{text}"
        );
    }

    #[test]
    fn max_output_tokens_truncates_and_names_the_spill_file() {
        let host = host(vec![], |_name, _args, _id, _signal| Ok(ok_result("")));
        let result = host.execute_code(
            "call-6",
            &format!(
                "// @options: {{\"max_output_tokens\": 4}}\ntext('{}');",
                "x".repeat(400)
            ),
            None,
        );
        let text = text_of_result(&result);
        assert!(text.contains("Warning: truncated output"), "{text}");
        assert!(text.contains("[Full output:"), "{text}");
        assert!(result.details["fullOutputPath"].is_string(), "{text}");
    }

    #[test]
    fn discovery_globals_describe_and_search_tools() {
        let mut tool = echo_tool();
        tool.namespace = Some(NamespaceInfo {
            name: "dev-tools".to_string(),
            description: Some("Developer utilities".to_string()),
            ..NamespaceInfo::default()
        });
        let host = host(vec![tool], echo_invoker());
        let result = host.execute_code(
            "call-7",
            "const found = await searchTools('echo');\nconst described = await describeTool('echo');\nconst ns = await describeNamespace('dev_tools');\nreturn JSON.stringify({ found, described: !!described, ns });",
            None,
        );
        let text = text_of_result(&result);
        assert!(!result.is_error, "{text}");
        assert!(text.contains("echo"), "{text}");
        assert!(text.contains("Developer utilities"), "{text}");
        assert!(text.contains("\"described\":true"), "{text}");
    }

    #[test]
    fn exposure_gate_disables_execution() {
        let options = CodemodeOptions {
            enabled: false,
            ..CodemodeOptions::default()
        };
        let host = host(vec![echo_tool()], echo_invoker()).with_options(options);
        assert!(!host.is_enabled());
        let result = host.execute_code("call-8", "return 1;", None);
        assert!(result.is_error);
        assert!(text_of_result(&result).contains("disabled"));
    }

    #[test]
    fn invalid_source_returns_a_failure_result() {
        let host = host(vec![], |_name, _args, _id, _signal| Ok(ok_result("")));
        let result = host.execute_code("call-9", "   ", None);
        assert!(result.is_error);
        assert!(text_of_result(&result).contains("Expected JavaScript source"));
    }

    #[test]
    fn tool_definition_exposes_codemode_parameters_and_sequential_mode() {
        let host = host(vec![echo_tool()], echo_invoker());
        let definition = host.tool_definition();
        assert_eq!(definition.name, CODEMODE_TOOL_NAME);
        assert!(definition.parameters["properties"]["code"].is_object());
        assert_eq!(
            definition.execution_mode,
            Some(pi_agent::types::ToolExecutionMode::Sequential)
        );
        assert!(definition.description.contains("Run JavaScript"));
    }

    #[test]
    fn execute_via_executor_adapter_passes_the_call_through() {
        struct StubExecutor;
        impl pi_agent::types::ToolExecutor for StubExecutor {
            fn tools(&self) -> Vec<ToolSpec> {
                Vec::new()
            }

            fn execute(
                &self,
                call: pi_agent::types::AgentToolCall,
                _updates: tokio::sync::mpsc::UnboundedSender<AgentToolResult>,
            ) -> pi_agent::types::BoxFuture<'_, Result<AgentToolResult, String>> {
                Box::pin(
                    async move { Ok(ok_result(&format!("{}:{}", call.name, call.arguments["n"]))) },
                )
            }
        }
        let result = execute_via_executor(
            &StubExecutor,
            "echo",
            json!({ "n": 7 }),
            "call-10",
            &AbortFlag::new(),
        )
        .expect("executor");
        assert_eq!(text_of_result(&result), "echo:7");
    }
}
