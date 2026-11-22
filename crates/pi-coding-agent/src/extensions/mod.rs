//! Extensions host wiring (E2 slice 3).
//!
//! Ports the CLI loading path `main.ts` assembles around the runtime:
//! discovery with `--no-extensions` semantics (`resource-loader.ts:609-615`,
//! CLI `-e` paths still load), the pre-trust bootstrap pass
//! (`resource-loader.ts:533-553`) in which global/CLI extensions may handle
//! `project_trust` (`project-trust.ts:55-71`), the final load that reuses
//! bootstrap-loaded modules and never retries bootstrap failures
//! (`resource-loader.ts:791-833`), bind, extension flag claiming
//! (`agent-session-services.ts:82-129`), and the extension-tool half of the
//! tool loadout (`agent-session.ts:1600-1716`) reduced to the print-path
//! subset. Load failures surface as the upstream diagnostics
//! (`main.ts:861-869`); error-level diagnostics abort startup with the
//! load-failure hint (`main.ts:986-990`).
//!
//! Slice 4 adds the runtime events over that spine: the `session_start`
//! emission at the end of bind (`bindExtensions`, `agent-session.ts:3445`),
//! the `user_bash` interception for interactive `!` commands
//! (`handleBashCommand`, `interactive-mode.ts:7172-7189` — the operations
//! path composes through the ported `executeBashWithOperations`,
//! `bash-executor.ts`), the `transformContext` hook into the agent loop
//! (`sdk.ts:430-434`), the `session_before_switch`/`session_before_compact`
//! gates at the reachable switch points (`agent-session-runtime.ts:133-166`,
//! `agent-session.ts:2964-2982`), and the UI-context binding the RPC mode
//! drives (`modes/rpc.rs`). The MCP slice adds the missing `bindCore` call
//! and the extension-registered MCP servers connecting on `session_start`
//! (`extensions/mcp/index.ts:1112-1149`, the manager in `pi-ext-js`).
//!
//! Deliberately slice-4 scoped (§8 of `docs/EXTENSIONS.md`): the full
//! active-loadout machinery (`prepareLoadout`, pending tools), the
//! `session_before_fork`/`session_before_tree` gates (no fork/tree in the
//! runtime yet), and extension-provided compaction results stay open.
//!
//! The `ext-js` feature gates the engine. Without it the host keeps
//! upstream's shape with zero extensions: nothing loads, trust resolves
//! without the hook, and every captured unknown flag stays unclaimed.

use std::path::Path;
use std::sync::Arc;

#[cfg(feature = "ext-js")]
use std::collections::HashSet;
#[cfg(feature = "ext-js")]
use std::path::PathBuf;
#[cfg(feature = "ext-js")]
use std::sync::Mutex;
#[cfg(feature = "ext-js")]
use std::time::Instant;

use pi_agent::types::ToolExecutor;
#[cfg(feature = "ext-js")]
use pi_agent::types::{AgentToolCall, AgentToolResult, BoxFuture, ToolSpec};
#[cfg(feature = "ext-js")]
use serde_json::{Value, json};

use crate::cli::args::{Args, Diagnostic, DiagnosticKind};
#[cfg(feature = "ext-js")]
use crate::cli::project_trust::has_trust_requiring_project_resources;
use crate::cli::project_trust::{
    DefaultProjectTrust, ProjectTrustStore, ResolveProjectTrustedOptions, resolve_project_trusted,
};
use crate::config::settings::SettingsManager;
#[cfg(feature = "ext-js")]
use crate::tools::index::BuiltinToolExecutor;
use crate::tools::index::{ToolDefinition, ToolRegistry};
#[cfg(feature = "ext-js")]
use crate::tools::renderers::ToolRenderers;
#[cfg(feature = "ext-js")]
use crate::tools::schema::validate_tool_arguments;
#[cfg(feature = "ext-js")]
use crate::tools::truncate::{DEFAULT_MAX_BYTES, TruncationOptions, truncate_tail};

/// `EXTENSION_LOAD_FAILURE_HINT` (`main.ts:77`), printed after the
/// diagnostics when an extension failed to load (`main.ts:986-990`).
pub const EXTENSION_LOAD_FAILURE_HINT: &str = "Hint: Start without extensions using \"pi -ne\".";

/// Extension tools carry no `renderCall`/`renderResult` upstream; the
/// renderers are therefore undefined and the interactive component's
/// no-renderer fallback (plain text, `tool_execution.rs`) shapes them. The
/// definition field is not optional, so the placeholder pair here must not
/// be reached in E2 modes (only the interactive component renders, and it
/// resolves renderers through [`crate::tools::renderers::renderers_for`]).
#[cfg(feature = "ext-js")]
const EXTENSION_RENDERERS: ToolRenderers = ToolRenderers {
    render_call: |_args, _context| String::new(),
    render_result: |_args, _result, _context| String::new(),
};

pub fn error(message: String) -> Diagnostic {
    Diagnostic {
        kind: DiagnosticKind::Error,
        message,
    }
}

pub fn warning(message: String) -> Diagnostic {
    Diagnostic {
        kind: DiagnosticKind::Warning,
        message,
    }
}

/// The loaded extension host: the runner behind a mutex plus the extension
/// tool candidates for tool composition. Shared with the agent loop —
/// `ExtensionRunner` is `Send` (rquickjs's `parallel` feature; `Send`
/// error listeners and host actions), and the print/json/rpc modes run on
/// one thread, so the mutex never contends.
pub struct ExtensionHost {
    #[cfg(feature = "ext-js")]
    runner: Option<Arc<Mutex<pi_ext_js::ExtensionRunner>>>,
    /// Registered extension tools eligible for declaration — default
    /// active, non-hidden, exposure `direct`/`model-only` (the active rule
    /// the runner bind applies) — first-wins in load order.
    extension_tools: Vec<ToolDefinition>,
}

impl ExtensionHost {
    /// The host compiled without the JS engine: no runner, no tools.
    #[cfg(not(feature = "ext-js"))]
    fn empty() -> Self {
        Self {
            #[cfg(feature = "ext-js")]
            runner: None,
            extension_tools: Vec::new(),
        }
    }

    /// Extension tool candidates for [`select_tools`] — unfiltered by the
    /// CLI tool flags; the selection applies the allowlist/exclude rules.
    #[must_use]
    pub fn tool_definitions(&self) -> &[ToolDefinition] {
        &self.extension_tools
    }

    /// The executor for the selected tool list: extension tool calls
    /// dispatch into the runner, every other name to the builtin executor.
    #[must_use]
    pub fn executor(&self, selected: &[ToolDefinition], cwd: &str) -> Arc<dyn ToolExecutor> {
        #[cfg(feature = "ext-js")]
        {
            let extension_selected: Vec<ToolDefinition> = {
                let names: Vec<&str> = self.extension_tools.iter().map(|d| d.name).collect();
                selected
                    .iter()
                    .filter(|definition| names.contains(&definition.name))
                    .cloned()
                    .collect()
            };
            Arc::new(ExtensionAwareExecutor {
                builtin: builtin_executor_over(selected, &self.extension_tools, cwd),
                runner: self.runner.clone(),
                extension: extension_selected,
            })
        }
        #[cfg(not(feature = "ext-js"))]
        ToolRegistry::new(selected.to_vec()).executor(cwd)
    }

    /// The runner handle the modes share: the RPC mode binds the UI sink,
    /// interactive mode intercepts `!` commands and consults the
    /// `session_before_*` gates. `None` without the engine or when the
    /// host is compiled without it.
    #[cfg(feature = "ext-js")]
    #[must_use]
    pub fn runner(&self) -> Option<Arc<Mutex<pi_ext_js::ExtensionRunner>>> {
        self.runner.clone()
    }

    /// Binds the UI surface (`setUIContext`, `runner.ts:570-575`): the RPC
    /// mode passes its `extension_ui_request` bridge; print/json stay
    /// headless (upstream's non-interactive paths pass no `uiContext`).
    #[cfg(feature = "ext-js")]
    pub fn set_ui_context(&self, sink: Arc<dyn pi_ext_js::UiSink>, mode: &str) {
        if let Some(runner) = &self.runner {
            runner
                .lock()
                .expect("extension runner mutex poisoned")
                .set_ui_context(sink, mode);
        }
    }

    /// Unbinds the UI surface (`setUIContext(undefined)`): back to the
    /// headless matrix.
    #[cfg(feature = "ext-js")]
    pub fn clear_ui_context(&self) {
        if let Some(runner) = &self.runner {
            runner
                .lock()
                .expect("extension runner mutex poisoned")
                .clear_ui_context();
        }
    }

    /// The `!`-command interception point (`handleBashCommand`,
    /// `interactive-mode.ts:7172-7189`). `None` means headless (no runner —
    /// execute locally); `Some(Err(message))` means a handler failed (the
    /// runner already reported it — do **not** fall back to local
    /// execution, upstream returns early); `Some(Ok(outcome))` carries the
    /// decision.
    #[cfg(feature = "ext-js")]
    pub fn user_bash(
        &self,
        command: &str,
        exclude_from_context: bool,
        cwd: &str,
    ) -> Option<Result<UserBashOutcome, String>> {
        user_bash(self.runner.as_ref()?, command, exclude_from_context, cwd)
    }

    /// One host-driven MCP tick (`index.ts:1154-1175`,
    /// `runtime.ts:382-384`, `:432-442`): advance the bounded startup wait,
    /// register the tools of settled connections — including servers whose
    /// per-server `ready` settles after the wait timed out (`index.ts:112-113`,
    /// `:594-608`) — and drain `notifications/tools/list_changed`
    /// re-registrations through the worker's non-blocking drain event. The
    /// host decides when to call this — no timer thread or `sleep` lives in
    /// the runner.
    #[cfg(feature = "ext-js")]
    pub fn mcp_tick(&self, now: Instant) -> Result<pi_ext_js::runner::McpTickOutcome, String> {
        let Some(runner) = &self.runner else {
            return Ok(pi_ext_js::runner::McpTickOutcome::default());
        };
        runner
            .lock()
            .expect("extension runner mutex poisoned")
            .mcp_tick(now)
            .map_err(|err| err.to_string())
    }

    /// Emits one gate/teardown event (`session_before_*`,
    /// `session_shutdown`) and reports whether a handler voted
    /// `cancel: true` (`emitBeforeSwitch`, `agent-session-runtime.ts:133-166`;
    /// the `session_before_*` emit family keeps the last handler result and
    /// short-circuits on `cancel`, `runner.ts:1103-1132`). Headless or
    /// handler-less hosts never cancel. Handler-level failures surface as
    /// the runner error report plus `Err`; the caller decides severity.
    #[cfg(feature = "ext-js")]
    pub fn emit_gate(&self, event: &Value) -> Result<bool, String> {
        let Some(runner) = &self.runner else {
            return Ok(false);
        };
        emit_gate(runner, event)
    }
}

/// The `user_bash` interception over a shared runner (the host method
/// delegates here; the interactive orchestrator calls this directly).
#[cfg(feature = "ext-js")]
pub fn user_bash(
    runner: &Arc<Mutex<pi_ext_js::ExtensionRunner>>,
    command: &str,
    exclude_from_context: bool,
    cwd: &str,
) -> Option<Result<UserBashOutcome, String>> {
    let mut runner = runner.lock().expect("extension runner mutex poisoned");
    if !runner.has_handlers("user_bash") {
        return Some(Ok(UserBashOutcome::Local));
    }
    let decision = runner
        .emit_user_bash(command, exclude_from_context, cwd)
        .map_err(|err| err.to_string());
    Some(match decision {
        Err(message) => Err(message),
        Ok(None) => Ok(UserBashOutcome::Local),
        Ok(Some(pi_ext_js::UserBashDecision::Operations)) => {
            execute_bash_with_operations(&mut runner, command, cwd).map(UserBashOutcome::Executed)
        }
        Ok(Some(pi_ext_js::UserBashDecision::Result(result))) => {
            Ok(UserBashOutcome::Result(result))
        }
    })
}

/// The gate/teardown emit over a shared runner (the host method delegates
/// here; the interactive orchestrator calls this directly).
#[cfg(feature = "ext-js")]
pub fn emit_gate(
    runner: &Arc<Mutex<pi_ext_js::ExtensionRunner>>,
    event: &Value,
) -> Result<bool, String> {
    let event_name = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut runner = runner.lock().expect("extension runner mutex poisoned");
    if !runner.has_handlers(&event_name) {
        return Ok(false);
    }
    let result = runner.emit(event).map_err(|err| err.to_string())?;
    Ok(result
        .as_ref()
        .and_then(|result| result.get("cancel"))
        .and_then(Value::as_bool)
        .unwrap_or(false))
}

/// Drives the runner's host-owned MCP startup wait before the first prompt
/// (`index.ts:1154-1175`) and reports whether the bound elapsed with servers
/// still connecting. The runner only advances on an explicit tick, so the
/// host supplies `now` on each pass; the short yield between passes lets the
/// manager's worker make progress without a timer thread.
#[cfg(feature = "ext-js")]
fn settle_mcp_startup(runner: &mut pi_ext_js::ExtensionRunner) -> Result<Option<String>, String> {
    use pi_ext_js::mcp_manager::StartupTick;

    // Safety valve for the host loop: the runner's own bound is authoritative
    // and normally shorter, this only guards against a stalled tick.
    let deadline = Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let outcome = runner
            .mcp_tick(Instant::now())
            .map_err(|err| err.to_string())?;
        match outcome.startup {
            None | Some(StartupTick::Ready) => return Ok(None),
            Some(StartupTick::TimedOut) => {
                return Ok(Some(
                    "MCP servers are still connecting; their tools become available once connected."
                        .to_string(),
                ));
            }
            Some(StartupTick::Waiting { .. }) => {
                if Instant::now() >= deadline {
                    return Ok(Some(
                        "MCP servers are still connecting; their tools become available once connected."
                            .to_string(),
                    ));
                }
                // The worker connects on another thread; a short sleep keeps
                // the host loop from spinning for the whole wait window.
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }
}

/// The upstream attention notice for failed MCP connections
/// (`reportProblems`, `index.ts:641-653`), built from the settled reports.
#[cfg(feature = "ext-js")]
fn mcp_attention_diagnostic(reports: &[pi_ext_js::runner::McpServerReport]) -> Option<Diagnostic> {
    let failures: Vec<String> = reports
        .iter()
        .filter(|report| report.state == "failed")
        .map(|report| {
            let error = report.error.as_deref().unwrap_or("unknown error");
            let first_line = error.split('\n').next().unwrap_or(error);
            format!("  {}: failed: {first_line}", report.name)
        })
        .collect();
    (!failures.is_empty()).then(|| {
        warning(format!(
            "MCP servers need attention:\n{}\nRun /mcp to fix.",
            failures.join("\n")
        ))
    })
}

/// What a `user_bash` interception decided (`UserBashEventResult`,
/// `extensions/types.ts:1487-1494`). Not engine-gated: the interactive
/// host's interception seam references it in both builds and answers
/// `Local`-shaped fallthrough headlessly.
#[derive(Debug, Clone)]
pub enum UserBashOutcome {
    /// No handler intercepted; the host executes locally.
    Local,
    /// The handler supplied `operations`; the host executed them through
    /// the stashed `exec` and composed the `BashResult`
    /// (`executeBashWithOperations`, `bash-executor.ts:57-135`).
    Executed(serde_json::Value),
    /// The handler executed the command; its `BashResult` verbatim.
    Result(serde_json::Value),
}

/// `executeBashWithOperations` (`bash-executor.ts:57-135`) reduced to the
/// no-streaming shape the engine boundary gives us: the extension's exec
/// runs once via [`pi_ext_js::ExtensionRunner::call_bash_exec`], its
/// chunks sanitize per `appendText` (`stripAnsi` → `sanitizeBinaryOutput`
/// → `\r` removal), a rolling window keeps the tail under twice
/// `DEFAULT_MAX_BYTES` (upstream measures `text.length`, UTF-16 units —
/// the port counts chars, identical below the astral plane), the tail
/// truncation applies, and the temp file spills once the stream crosses
/// the threshold (upstream `ensureTempFile`). `cancelled` is always false:
/// the abort signal does not cross the engine boundary (HANDOFF, slice 1).
#[cfg(feature = "ext-js")]
fn execute_bash_with_operations(
    runner: &mut pi_ext_js::ExtensionRunner,
    command: &str,
    cwd: &str,
) -> Result<Value, String> {
    use crate::tools::render_utils::{sanitize_binary_output, strip_ansi};
    use std::io::Write as _;

    let (result, chunks) = runner
        .call_bash_exec(command, cwd, &json!({}))
        .map_err(|err| err.to_string())?;
    if let Some(message) = result.get("error").and_then(Value::as_str) {
        // An exec failure rethrows upstream (`bash-executor.ts:113-119`).
        return Err(message.to_string());
    }
    let exit_code = result.get("exitCode").and_then(Value::as_i64);

    let mut window: Vec<String> = Vec::new();
    let mut window_chars = 0usize;
    let mut streamed_chars = 0usize;
    let mut discarded = false;
    let mut temp: Option<(std::path::PathBuf, std::fs::File)> = None;
    for raw in chunks {
        let text = sanitize_binary_output(&strip_ansi(&raw)).replace('\r', "");
        if text.is_empty() {
            continue;
        }
        streamed_chars += text.chars().count();
        if temp.is_none() && streamed_chars > DEFAULT_MAX_BYTES {
            // ensureTempFile: the buffered window rides along so the file
            // opens with everything streamed so far.
            let path = crate::tools::output_accumulator::temp_file_path("pi-bash");
            let file = std::fs::File::create(&path)
                .map_err(|err| format!("Failed to create the bash output file: {err}"))?;
            let mut file = file;
            for buffered in &window {
                let _ = file.write_all(buffered.as_bytes());
            }
            temp = Some((path, file));
        }
        if let Some((_, file)) = temp.as_mut() {
            let _ = file.write_all(text.as_bytes());
        }
        window_chars += text.chars().count();
        window.push(text);
        while window_chars > DEFAULT_MAX_BYTES * 2 && window.len() > 1 {
            let removed = window.remove(0);
            window_chars -= removed.chars().count();
            discarded = true;
        }
    }

    let full = window.join("");
    let truncation = truncate_tail(&full, TruncationOptions::default());
    let mut full_output_path = None;
    if truncation.truncated {
        let path = if let Some((path, file)) = &temp {
            file.sync_all().ok();
            path.clone()
        } else {
            let path = crate::tools::output_accumulator::temp_file_path("pi-bash");
            std::fs::write(&path, &full)
                .map_err(|err| format!("Failed to write the bash output file: {err}"))?;
            path
        };
        full_output_path = Some(path);
    }
    if let Some((_, file)) = temp {
        let _ = file.sync_all();
    }

    // `exitCode: undefined` and the exec's `null` both drop from the
    // upstream result (`exitCode ?? undefined`).
    let mut bash_result = serde_json::Map::new();
    bash_result.insert("output".to_string(), json!(truncation.content));
    if let Some(code) = exit_code {
        bash_result.insert("exitCode".to_string(), json!(code));
    }
    bash_result.insert("cancelled".to_string(), json!(false));
    bash_result.insert(
        "truncated".to_string(),
        json!(discarded || truncation.truncated),
    );
    if let Some(path) = full_output_path {
        bash_result.insert(
            "fullOutputPath".to_string(),
            json!(path.to_string_lossy().into_owned()),
        );
    }
    Ok(Value::Object(bash_result))
}

/// The `transformContext` hook (`sdk.ts:430-434`): the agent loop's context
/// crosses to [`pi_ext_js::ExtensionRunner::emit_context`] before
/// serialization — `context` handlers see the conversation without system
/// messages, `context_with_system` handlers the full transcript
/// (`runner.ts:1312-1370`, ported in the runner). Installed only when such
/// handlers exist; otherwise the identity, like upstream's no-op run.
#[cfg(feature = "ext-js")]
pub fn context_transform(
    runner: Arc<Mutex<pi_ext_js::ExtensionRunner>>,
) -> Option<Arc<dyn pi_agent::TransformContext>> {
    {
        let runner = runner.lock().expect("extension runner mutex poisoned");
        if !runner.has_handlers("context") && !runner.has_handlers("context_with_system") {
            return None;
        }
    }
    Some(Arc::new(ExtensionContextTransform { runner }) as Arc<dyn pi_agent::TransformContext>)
}

/// The [`pi_agent::TransformContext`] implementation: `AgentMessage`s
/// serialize to the session JSONL v3 shapes the runner already emits in
/// tool/session events, round-trip through `emit_context`, and come back.
#[cfg(feature = "ext-js")]
struct ExtensionContextTransform {
    runner: Arc<Mutex<pi_ext_js::ExtensionRunner>>,
}

#[cfg(feature = "ext-js")]
impl pi_agent::TransformContext for ExtensionContextTransform {
    fn call(
        &self,
        messages: Vec<pi_agent::agent_message::AgentMessage>,
    ) -> BoxFuture<'_, Vec<pi_agent::agent_message::AgentMessage>> {
        Box::pin(async move {
            // `transformContext` runs once per agent-loop turn, so it is the
            // run-loop tick point for MCP notifications even when no
            // extension tool is called this turn (`index.ts:382-384`).
            let _ = self
                .runner
                .lock()
                .expect("extension runner mutex poisoned")
                .mcp_tick(Instant::now());
            let values: Vec<Value> = messages
                .iter()
                .map(|message| serde_json::to_value(message).unwrap_or(Value::Null))
                .collect();
            let transformed = {
                let mut runner = self.runner.lock().expect("extension runner mutex poisoned");
                runner
                    .emit_context(&values)
                    .map_err(|err| err.to_string())
                    .unwrap_or_else(|error| {
                        // Handler errors report inside the runner; a
                        // context transform failure keeps the input, like
                        // upstream's per-handler catch (`runner.ts:1347`).
                        eprintln!("Warning: Extension context error: {error}");
                        values.clone()
                    })
            };
            transformed
                .into_iter()
                .filter_map(|value| serde_json::from_value(value).ok())
                .collect()
        })
    }
}

/// The builtin executor over `selected` minus the extension names, so the
/// builtin registry never double-reports an extension tool.
#[cfg(feature = "ext-js")]
fn builtin_executor_over(
    selected: &[ToolDefinition],
    extension: &[ToolDefinition],
    cwd: &str,
) -> Arc<BuiltinToolExecutor> {
    let names: Vec<&str> = extension.iter().map(|definition| definition.name).collect();
    let builtin_defs: Vec<ToolDefinition> = selected
        .iter()
        .filter(|definition| !names.contains(&definition.name))
        .cloned()
        .collect();
    ToolRegistry::new(builtin_defs).executor(cwd)
}

/// The extension `ToolResult` → `AgentToolResult` (upstream `ToolResult`:
/// `content` as string-or-blocks, `details`, `structuredContent`, `usage`,
/// `isError`).
#[cfg(feature = "ext-js")]
fn tool_result_from_extension(value: &Value) -> AgentToolResult {
    let content = match value.get("content") {
        Some(Value::String(text)) => vec![pi_ai::types::ContentBlock::Text(
            pi_ai::types::TextContent {
                text: text.clone(),
                text_signature: None,
            },
        )],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| serde_json::from_value(block.clone()).ok())
            .collect(),
        _ => Vec::new(),
    };
    AgentToolResult {
        content,
        details: value.get("details").cloned().unwrap_or_else(|| json!({})),
        structured_content: value
            .get("structuredContent")
            .cloned()
            .filter(|value| !value.is_null()),
        usage: value
            .get("usage")
            .and_then(|usage| serde_json::from_value(usage.clone()).ok()),
        is_error: value
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        terminate: false,
    }
}

/// The composite [`ToolExecutor`]: extension tool calls validate against
/// their schema and run through the runner (`ExtensionRunner::execute_tool`,
/// the `toolCall` dispatch session), everything else delegates to the
/// builtin executor.
#[cfg(feature = "ext-js")]
struct ExtensionAwareExecutor {
    builtin: Arc<BuiltinToolExecutor>,
    runner: Option<Arc<Mutex<pi_ext_js::ExtensionRunner>>>,
    /// The selected extension tools (the dispatch set).
    extension: Vec<ToolDefinition>,
}

#[cfg(feature = "ext-js")]
impl ToolExecutor for ExtensionAwareExecutor {
    fn tools(&self) -> Vec<ToolSpec> {
        let mut specs = self.builtin.tools();
        specs.extend(self.extension.iter().map(|definition| ToolSpec {
            name: definition.name.to_string(),
            description: definition.description.clone(),
            parameters: definition.parameters.clone(),
            execution_mode: definition.execution_mode,
            constrained_sampling: definition.constrained_sampling.clone(),
        }));
        specs
    }

    fn execution_mode(&self, name: &str) -> Option<pi_agent::types::ToolExecutionMode> {
        self.builtin.execution_mode(name).or_else(|| {
            self.extension
                .iter()
                .find(|definition| definition.name == name)
                .and_then(|definition| definition.execution_mode)
        })
    }

    fn execute(
        &self,
        call: AgentToolCall,
        updates: tokio::sync::mpsc::UnboundedSender<AgentToolResult>,
    ) -> pi_agent::types::BoxFuture<'_, Result<AgentToolResult, String>> {
        if self
            .extension
            .iter()
            .any(|definition| definition.name == call.name)
        {
            let name = call.name.clone();
            let call_id = call.id.clone();
            let arguments = Value::Object(call.arguments);
            Box::pin(async move { self.execute_extension(&name, &call_id, &arguments) })
        } else {
            self.builtin.execute(call, updates)
        }
    }
}

#[cfg(feature = "ext-js")]
impl ExtensionAwareExecutor {
    /// One extension tool call: argument validation like the builtins
    /// (`BuiltinToolExecutor::run`), then the runner dispatch. The runner
    /// lock is held only across the synchronous JS execution.
    fn execute_extension(
        &self,
        name: &str,
        call_id: &str,
        arguments: &Value,
    ) -> Result<AgentToolResult, String> {
        let definition = self
            .extension
            .iter()
            .find(|definition| definition.name == name)
            .expect("extension dispatch set holds the tool");
        let arguments = validate_tool_arguments(
            name,
            &definition.parameters,
            arguments,
            definition.prepare_arguments,
        )?;
        let runner = self
            .runner
            .as_ref()
            .expect("extension tools exist only with a runner");
        let mut runner = runner.lock().expect("extension runner mutex poisoned");
        // The run loop is the host's tick point: a `list_changed`
        // notification must be reflected before a codemode script or a
        // direct MCP call reaches the tool it names (`runtime.ts:382-384`).
        // A refresh failure is a server-state value upstream, never a tool
        // error, so the tick's result is deliberately dropped here.
        let _ = runner.mcp_tick(Instant::now());
        let value = runner
            .execute_tool(name, call_id, &arguments)
            .map_err(|error| error.to_string())?;
        Ok(tool_result_from_extension(&value))
    }
}

/// The load outcome: the host plus the resolved trust decision.
pub struct Loaded {
    pub host: ExtensionHost,
    pub project_trusted: bool,
}

/// Discovery + load + bind + flag claiming
/// (`createAgentSessionServices` + `resource-loader.reload`). Returns the
/// diagnostics in report order; the caller prints warnings and aborts on
/// error-level entries. `ui` carries the mode's UI surface — the RPC
/// bridge — applied before bind so `session_start` handlers can already
/// prompt (`bindExtensions` applies the bindings, then emits,
/// `agent-session.ts:3429-3449`).
///
/// # Errors
/// Runtime creation, bind, and trust-store failures propagate; load
/// failures come back as error diagnostics instead.
#[allow(clippy::too_many_arguments)] // mirrors upstream's load pipeline; split when extending
pub fn load(
    args: &Args,
    cwd: &Path,
    agent_dir: &Path,
    trust_store: &ProjectTrustStore,
    default_project_trust: Option<DefaultProjectTrust>,
    startup_settings: &SettingsManager,
    mode: &str,
    #[cfg(feature = "ext-js")] ui: Option<(Arc<dyn pi_ext_js::UiSink>, String)>,
) -> Result<(Loaded, Vec<Diagnostic>), String> {
    #[cfg(feature = "ext-js")]
    {
        load_with_engine(
            args,
            cwd,
            agent_dir,
            trust_store,
            default_project_trust,
            startup_settings,
            mode,
            ui,
        )
    }
    #[cfg(not(feature = "ext-js"))]
    {
        let _ = (agent_dir, startup_settings, mode);
        let mut diagnostics = Vec::new();
        let project_trusted = resolve_project_trusted(&mut ResolveProjectTrustedOptions {
            cwd,
            store: trust_store,
            trust_override: args.project_trust_override,
            default_project_trust,
            extension_trust: None,
        })?;
        // Without the engine nothing claims flags: every captured unknown
        // flag stays unclaimed (`agent-session-services.ts:123-128`).
        let unclaimed: Vec<&str> = args
            .unknown_flags
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        if let Some(diagnostic) = unknown_options_diagnostic(&unclaimed) {
            diagnostics.push(diagnostic);
        }
        Ok((
            Loaded {
                host: ExtensionHost::empty(),
                project_trusted,
            },
            diagnostics,
        ))
    }
}

/// `/reload`'s extension half, over the session's existing runner: the
/// re-discovery against the reloaded settings, the incremental final-set
/// load, the re-bind, `session_start(reason:"reload")`, and the MCP
/// reconnect (`resourceLoader.reload()` → `loadFinalExtensionSet` →
/// `bindExtensions` → `agent-session.ts:3871-3872`). Deviation
/// (HANDOFF.md): upstream clears the module cache and re-evaluates every
/// extension file; the port has no per-path unload, so already-loaded
/// paths keep their registered handlers and only newly-discovered paths
/// load.
///
/// The trust decision is the session's — upstream's reload keeps
/// `SettingsManager.projectTrusted` (`resource-loader.ts:555`) — so the
/// reloaded settings view and the project filter ride it.
#[cfg(feature = "ext-js")]
pub fn reload(
    cli_extensions: &[String],
    no_extensions: bool,
    cwd: &Path,
    agent_dir: &Path,
    project_trusted: bool,
    settings: &SettingsManager,
    runner: &Arc<Mutex<pi_ext_js::ExtensionRunner>>,
) -> Vec<Diagnostic> {
    use pi_ext_js::loader::{discover_cli_paths, discover_extensions};

    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    // `--no-extensions` still loads CLI extensions
    // (`resource-loader.ts:609-610`); the settings-configured sources
    // resolve inside the discovery against the reloaded settings.
    let configured: Vec<String> = cli_extensions.to_vec();
    let discovered = if no_extensions {
        discover_cli_paths(&configured, cwd)
    } else {
        discover_extensions(&configured, cwd, agent_dir)
    };

    let mut runner = runner.lock().expect("extension runner mutex poisoned");
    // The settings view flips with the reload (`setProjectTrusted` +
    // `reload`).
    runner.core().lock().expect("host core").settings = Value::Object(settings.get_settings());

    // Upstream clears the extension factory cache and re-evaluates every
    // file on /reload (`resource-loader.ts:541-545`); no loaded-set filter.
    let paths: Vec<PathBuf> = discovered
        .iter()
        .filter(|entry| project_trusted || entry.origin != pi_ext_js::ExtensionOrigin::Project)
        .map(|entry| entry.path.clone())
        .collect();
    let result = runner.reload(&paths);
    for load_error in &result.errors {
        diagnostics.push(error(format!(
            "Failed to load extension \"{}\": {}",
            load_error.path, load_error.error
        )));
    }

    // `bindExtensions` (`agent-session.ts:3429-3449`): the re-bind re-arms
    // the runtime after the invalidation, then `session_start` fires with
    // the reload reason (`agent-session.ts:3871`).
    if let Err(err) = runner.bind() {
        diagnostics.push(warning(format!("Extension error: bind: {err}")));
    }
    if let Err(err) = runner.emit(&json!({ "type": "session_start", "reason": "reload" })) {
        diagnostics.push(warning(format!("Extension error: session_start: {err}")));
    }

    // The MCP reconnect (`reportUnhandledMcpServers`,
    // `agent-session.ts:3872`): the extension closed its connections on
    // `session_shutdown`. Connections start in the background and the host
    // drives the bounded wait before the first prompt after the reload.
    match runner.connect_mcp_servers() {
        Ok(_) => match settle_mcp_startup(&mut runner) {
            Ok(notice) => {
                if let Some(notice) = notice {
                    diagnostics.push(warning(notice));
                }
            }
            Err(err) => {
                diagnostics.push(warning(format!("Extension error: MCP startup wait: {err}")));
            }
        },
        Err(err) => diagnostics.push(warning(format!("Extension error: MCP connect: {err}"))),
    }
    if let Some(diagnostic) = mcp_attention_diagnostic(&runner.mcp_server_reports()) {
        diagnostics.push(diagnostic);
    }

    // Bind-time registration failures report without failing the reload
    // (the bindCore `emitError` reports, `runner.ts:470-521`).
    for bind_error in runner.provider_registration_errors() {
        diagnostics.push(warning(format!(
            "Extension \"{}\" {} error: {}",
            bind_error.extension_path, bind_error.event, bind_error.error
        )));
    }
    diagnostics
}

/// The real path with the engine.
#[cfg(feature = "ext-js")]
#[allow(clippy::too_many_lines)] // the upstream reload() sequence, kept in one pass
#[allow(clippy::too_many_arguments)] // mirrors upstream's load pipeline; split when extending
fn load_with_engine(
    args: &Args,
    cwd: &Path,
    agent_dir: &Path,
    trust_store: &ProjectTrustStore,
    default_project_trust: Option<DefaultProjectTrust>,
    startup_settings: &SettingsManager,
    mode: &str,
    ui: Option<(Arc<dyn pi_ext_js::UiSink>, String)>,
) -> Result<(Loaded, Vec<Diagnostic>), String> {
    use pi_ext_js::loader::{discover_cli_paths, discover_extensions, load_paths, split_for_trust};

    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let configured: Vec<String> = args.extensions.clone().unwrap_or_default();

    // `--no-extensions` still loads CLI extensions
    // (`resource-loader.ts:609-610`).
    let discovered = if args.no_extensions {
        discover_cli_paths(&configured, cwd)
    } else {
        discover_extensions(&configured, cwd, agent_dir)
    };

    // Local CLI paths that do not exist error without attempting a load
    // (`resource-loader.ts:620-627`).
    let mut missing: HashSet<PathBuf> = HashSet::new();
    for configured in &configured {
        if !is_local_path(configured) {
            continue;
        }
        let resolved = if Path::new(configured).is_absolute() {
            PathBuf::from(configured)
        } else {
            cwd.join(configured)
        };
        if !resolved.exists() {
            missing.insert(resolved.clone());
            diagnostics.push(error(format!(
                "Failed to load extension \"{}\": Extension path does not exist: {}",
                resolved.display(),
                resolved.display()
            )));
        }
    }

    // The bootstrap runner sees the untrusted settings, exactly like the
    // forced-untrusted pass (`resource-loader.ts:536-538`).
    let mut runner = pi_ext_js::ExtensionRunner::new(pi_ext_js::HostCore {
        cwd: cwd.to_string_lossy().into_owned(),
        mode: mode.to_string(),
        settings: Value::Object(startup_settings.get_settings()),
        mcp_auth_dir: Some(agent_dir.to_path_buf()),
        ..Default::default()
    })
    .map_err(|err| format!("Failed to initialize the extension runtime: {err}"))?;

    // The pre-trust pass runs only when trust actually needs resolving
    // (`main.ts:804-808`): no `--approve` override and trust-requiring
    // project resources.
    let should_resolve =
        args.project_trust_override.is_none() && has_trust_requiring_project_resources(cwd);
    let bootstrap: Vec<PathBuf> = split_for_trust(&discovered, false)
        .0
        .into_iter()
        .filter(|path| !missing.contains(path))
        .collect();
    let mut bootstrap_errors: Vec<pi_ext_js::LoadError> = Vec::new();
    if should_resolve {
        let result = load_paths(&mut runner, &bootstrap);
        bootstrap_errors = result.errors;
        for load_error in &bootstrap_errors {
            diagnostics.push(error(format!(
                "Failed to load extension \"{}\": {}",
                load_error.path, load_error.error
            )));
        }
    }

    // The trust hook fires `project_trust` in the bootstrap-loaded
    // extensions (`project-trust.ts:55-71`); handler errors report as
    // warnings.
    let project_trusted = {
        let mut hook = |hook_cwd: &Path| -> Result<Option<(bool, bool)>, String> {
            let (result, errors) = runner
                .emit_project_trust(&hook_cwd.to_string_lossy())
                .map_err(|err| err.to_string())?;
            for ext_error in errors {
                diagnostics.push(warning(format!(
                    "Extension \"{}\" project_trust error: {}",
                    ext_error.extension_path, ext_error.error
                )));
            }
            Ok(result.map(|decision| (decision.trusted == "yes", decision.remember)))
        };
        resolve_project_trusted(&mut ResolveProjectTrustedOptions {
            cwd,
            store: trust_store,
            trust_override: args.project_trust_override,
            default_project_trust,
            extension_trust: Some(&mut hook),
        })?
    };

    // The settings view flips with the trust decision
    // (`setProjectTrusted` + `reload`).
    {
        let trusted_settings = SettingsManager::create(
            cwd,
            agent_dir,
            crate::config::settings::SettingsManagerCreateOptions {
                project_trusted: Some(project_trusted),
            },
        );
        runner.core().lock().expect("host core").settings =
            Value::Object(trusted_settings.get_settings());
    }

    // The final pass: everything trusted allows, minus already-loaded
    // modules and never-retried bootstrap failures
    // (`loadFinalExtensionSet`, `resource-loader.ts:791-833`).
    let loaded: HashSet<String> = runner.extension_paths().into_iter().collect();
    let failed: HashSet<String> = bootstrap_errors.iter().map(|e| e.path.clone()).collect();
    let remaining: Vec<PathBuf> = discovered
        .iter()
        .filter(|entry| project_trusted || entry.origin != pi_ext_js::ExtensionOrigin::Project)
        .map(|entry| entry.path.clone())
        .filter(|path| {
            let key = path.to_string_lossy().into_owned();
            !loaded.contains(&key) && !failed.contains(&key) && !missing.contains(path)
        })
        .collect();
    let result = load_paths(&mut runner, &remaining);
    for load_error in &result.errors {
        diagnostics.push(error(format!(
            "Failed to load extension \"{}\": {}",
            load_error.path, load_error.error
        )));
    }

    // `bindExtensions` (`agent-session.ts:3429-3449`): the mode's UI
    // bindings apply first, then `session_start` fires — handlers can
    // prompt during the event. The RPC mode passes its bridge; print/json
    // stay headless (upstream's non-interactive paths pass no `uiContext`)
    // and interactive binds its TUI surface with E3.
    if let Some((sink, ui_mode)) = ui {
        runner.set_ui_context(sink, &ui_mode);
    }
    // bindCore (`runner.ts:417-551`): the runtime binds once loading
    // completes — action methods stop throwing, the MCP change listener
    // arms, and queued provider registrations flush.
    if let Err(err) = runner.bind() {
        diagnostics.push(warning(format!("Extension error: bind: {err}")));
    }
    if let Err(err) = runner.emit(&json!({ "type": "session_start", "reason": "startup" })) {
        diagnostics.push(warning(format!("Extension error: session_start: {err}")));
    }
    // The reason is "startup"; the "reload" path arrives with the
    // session-reload work. `resources_discover` is P5.

    // The MCP servers registered through `pi.registerMcpServer` start
    // connecting on session_start (`index.ts:1112-1149`, the registered-
    // servers half — `mcp.json` entries are P5). Connections are background
    // work; the first prompt's bounded wait is driven by the host below,
    // and the tools of settled connections register on that tick. Failed
    // connections are values: one attention notice (`reportProblems`,
    // `index.ts:641-653`) and no tools for the affected server.
    match runner.connect_mcp_servers() {
        Ok(_) => match settle_mcp_startup(&mut runner) {
            Ok(notice) => {
                if let Some(notice) = notice {
                    diagnostics.push(warning(notice));
                }
            }
            Err(err) => {
                diagnostics.push(warning(format!("Extension error: MCP startup wait: {err}")));
            }
        },
        Err(err) => diagnostics.push(warning(format!("Extension error: MCP connect: {err}"))),
    }
    if let Some(diagnostic) = mcp_attention_diagnostic(&runner.mcp_server_reports()) {
        diagnostics.push(diagnostic);
    }

    // Bind-time registration failures report without failing the load
    // (the bindCore `emitError` reports, `runner.ts:470-521`).
    for bind_error in runner.provider_registration_errors() {
        diagnostics.push(warning(format!(
            "Extension \"{}\" {} error: {}",
            bind_error.extension_path, bind_error.event, bind_error.error
        )));
    }

    // Flag claiming (`applyExtensionFlagValues`,
    // `agent-session-services.ts:82-129`).
    let registered = runner.flags();
    let mut unclaimed: Vec<&str> = Vec::new();
    for (name, value) in &args.unknown_flags {
        match registered.iter().find(|flag| flag.name == *name) {
            None => unclaimed.push(name.as_str()),
            // Booleans set `true` regardless of the captured value.
            Some(flag) if flag.kind == "boolean" => runner.set_flag_value(name, json!(true)),
            Some(_) => match value {
                Some(value) => runner.set_flag_value(name, json!(value)),
                None => diagnostics.push(error(format!(
                    "Extension flag \"--{name}\" requires a value"
                ))),
            },
        }
    }
    if let Some(diagnostic) = unknown_options_diagnostic(&unclaimed) {
        diagnostics.push(diagnostic);
    }

    // Tool candidates: default-active, non-hidden, `direct`/`model-only`
    // exposure — the runner bind()'s active rule (`agent-session.ts`
    // `_applyToolLoadout` reduces to this for the print path).
    let extension_tools = runner
        .all_registered_tools()
        .into_iter()
        .filter(|tool| {
            tool.default_active && matches!(tool.exposure.as_str(), "direct" | "model-only")
        })
        .map(tool_definition)
        .collect::<Vec<_>>();

    Ok((
        Loaded {
            host: ExtensionHost {
                runner: Some(Arc::new(Mutex::new(runner))),
                extension_tools,
            },
            project_trusted,
        },
        diagnostics,
    ))
}

/// `isLocalPath` (`utils/paths.ts:94-109`): the known non-local prefixes
/// are package-manager sources (P5); everything else is a filesystem path.
pub(crate) fn is_local_path(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.starts_with("npm:")
        && !trimmed.starts_with("git:")
        && !trimmed.starts_with("github:")
        && !trimmed.starts_with("http:")
        && !trimmed.starts_with("https:")
        && !trimmed.starts_with("ssh:")
        && !trimmed.starts_with("builtin:")
}

/// The unclaimed-flags diagnostic (`agent-session-services.ts:123-128`).
fn unknown_options_diagnostic(names: &[&str]) -> Option<Diagnostic> {
    if names.is_empty() {
        return None;
    }
    let list = names
        .iter()
        .map(|name| format!("--{name}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(error(format!(
        "Unknown option{}: {list}",
        if names.len() == 1 { "" } else { "s" }
    )))
}

/// The extension `ToolInfo` → `ToolDefinition`. Tool names leak to
/// `'static` on purpose: upstream keeps them for the process lifetime too,
/// and the count is bounded by the loaded extensions.
#[cfg(feature = "ext-js")]
fn tool_definition(info: &pi_ext_js::ToolInfo) -> ToolDefinition {
    let name: &'static str = Box::leak(info.name.clone().into_boxed_str());
    let label: &'static str = if info.label.is_empty() {
        name
    } else {
        Box::leak(info.label.clone().into_boxed_str())
    };
    ToolDefinition {
        name,
        label,
        description: info.description.clone(),
        parameters: info
            .parameters
            .clone()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        output_schema: None,
        prompt_snippet: None,
        prompt_guidelines: Vec::new(),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        render_shell: None,
        renderers: EXTENSION_RENDERERS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- engine-free pieces ----

    #[test]
    fn unknown_options_diagnostic_singular_and_plural() {
        let one = unknown_options_diagnostic(&["flag-a"]).expect("one");
        assert_eq!(one.kind, DiagnosticKind::Error);
        assert_eq!(one.message, "Unknown option: --flag-a");
        let many = unknown_options_diagnostic(&["flag-a", "flag-b"]).expect("many");
        assert_eq!(many.message, "Unknown options: --flag-a, --flag-b");
        assert!(unknown_options_diagnostic(&[]).is_none());
    }

    #[test]
    fn local_path_gate_matches_upstream_prefixes() {
        assert!(is_local_path("/abs/path.ts"));
        assert!(is_local_path("./rel.ts"));
        assert!(!is_local_path("git:github.com/x/y"));
        assert!(!is_local_path("npm:pkg"));
        assert!(!is_local_path("builtin:mcp"));
        assert!(!is_local_path("https://example.com/x.ts"));
    }

    // ---- runner-backed pieces (E2 slice 4) ----

    /// A runner over one inline extension, bound like the host does.
    fn host_with(source: &str) -> ExtensionHost {
        let mut runner = pi_ext_js::ExtensionRunner::new(pi_ext_js::HostCore {
            cwd: "/tmp".to_string(),
            mode: "print".to_string(),
            settings: json!({}),
            ..Default::default()
        })
        .expect("runner");
        runner.load_extension("<test>", Some(source)).expect("load");
        runner.bind().expect("bind");
        ExtensionHost {
            runner: Some(Arc::new(Mutex::new(runner))),
            extension_tools: Vec::new(),
        }
    }

    #[test]
    fn user_bash_result_replaces_local_execution() {
        let host = host_with(
            r#"
export default function (pi) {
  pi.on("user_bash", () => ({
    result: { output: "remote", exitCode: 0, cancelled: false, truncated: false },
  }));
}
"#,
        );
        let outcome = host
            .user_bash("ls", false, "/tmp")
            .expect("intercepted")
            .expect("handled");
        let UserBashOutcome::Result(result) = outcome else {
            panic!("expected a full result, got {outcome:?}");
        };
        assert_eq!(result.get("output").and_then(Value::as_str), Some("remote"));
    }

    #[test]
    fn user_bash_operations_execute_through_the_bridge() {
        let host = host_with(
            r#"
export default function (pi) {
  pi.on("user_bash", () => ({
    operations: {
      exec: async (command, cwd, opts) => {
        opts.onData("raw \u001b[31mred\u001b[0m\n");
        return { exitCode: 3 };
      },
    },
  }));
}
"#,
        );
        let outcome = host
            .user_bash("deploy", false, "/tmp")
            .expect("intercepted")
            .expect("handled");
        let UserBashOutcome::Executed(result) = outcome else {
            panic!("expected composed operations, got {outcome:?}");
        };
        // appendText sanitizes per chunk (`bash-executor.ts:90-92`).
        assert_eq!(
            result.get("output").and_then(Value::as_str),
            Some("raw red\n")
        );
        assert_eq!(result.get("exitCode").and_then(Value::as_i64), Some(3));
        assert_eq!(
            result.get("truncated").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            result.get("cancelled").and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn headless_host_runs_user_bash_locally() {
        let host = host_with("export default function (pi) {}");
        let outcome = host
            .user_bash("ls", false, "/tmp")
            .expect("runner present")
            .expect("no handlers");
        assert!(matches!(outcome, UserBashOutcome::Local));
    }

    #[test]
    fn emit_gate_reports_cancel_votes() {
        let host = host_with(
            r#"
export default function (pi) {
  pi.on("session_before_switch", () => ({ cancel: true }));
  pi.on("session_before_compact", () => ({}));
}
"#,
        );
        let cancelled = host
            .emit_gate(&json!({ "type": "session_before_switch", "reason": "new" }))
            .expect("gate");
        assert!(cancelled, "the cancel vote must reach the host");
        let cancelled = host
            .emit_gate(&json!({ "type": "session_before_compact" }))
            .expect("gate");
        assert!(!cancelled, "no vote is no cancel");
        let cancelled = host
            .emit_gate(&json!({ "type": "session_before_fork", "entryId": "e1" }))
            .expect("gate");
        assert!(!cancelled, "handler-less events never cancel");
    }

    #[test]
    fn host_mcp_tick_is_idle_without_servers() {
        let headless = ExtensionHost {
            runner: None,
            extension_tools: Vec::new(),
        };
        assert!(
            headless.mcp_tick(Instant::now()).expect("tick").is_idle(),
            "headless hosts never tick MCP"
        );

        let host = host_with("export default function (pi) {}");
        assert!(
            host.mcp_tick(Instant::now()).expect("tick").is_idle(),
            "a bound runner with no MCP servers is idle"
        );
    }

    /// A synthetic stdio server answering `initialize` and `tools/list`
    /// (the host-wiring half of the MCP fixture).
    const SYNTH_SERVER: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"synth","version":"1.0.0"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
  esac
done
"#;

    #[test]
    fn host_mcp_tick_settles_connections_and_registers_tools() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("synth.sh");
        std::fs::write(&path, SYNTH_SERVER).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod script");
        }
        let command = path.to_string_lossy();
        let source = format!(
            r#"export default function (pi) {{
  pi.registerMcpServer("synth", {{ command: "{command}", exposure: "direct", timeout: 5 }});
}}"#
        );
        let mut runner = pi_ext_js::ExtensionRunner::new(pi_ext_js::HostCore {
            cwd: directory.path().to_string_lossy().into_owned(),
            mode: "print".to_string(),
            settings: json!({}),
            ..Default::default()
        })
        .expect("runner");
        runner.load_extension("<mcp>", Some(&source)).expect("load");
        runner.bind().expect("bind");
        runner.connect_mcp_servers().expect("connect");
        let host = ExtensionHost {
            runner: Some(Arc::new(Mutex::new(runner))),
            extension_tools: Vec::new(),
        };
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let outcome = host.mcp_tick(Instant::now()).expect("tick");
            if !matches!(
                outcome.startup,
                Some(pi_ext_js::mcp_manager::StartupTick::Waiting { .. })
            ) {
                break;
            }
            assert!(Instant::now() < deadline, "the host tick never settled");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let runner = host.runner.expect("runner");
        let runner = runner.lock().expect("lock");
        assert!(runner.tool("mcp__synth__echo").is_some());
        assert!(
            runner
                .mcp_server_reports()
                .iter()
                .all(|report| report.state == "connected")
        );
    }

    #[test]
    fn context_transform_gates_on_handlers_and_round_trips() {
        let headless = host_with("export default function (pi) {}");
        let runner = headless.runner().expect("runner");
        assert!(context_transform(runner).is_none(), "no context handlers");

        let host = host_with(
            r#"
export default function (pi) {
  pi.on("context", (event) => ({ messages: event.messages.slice().reverse() }));
}
"#,
        );
        let runner = host.runner().expect("runner");
        let transform = context_transform(runner).expect("context handlers exist");
        let first = pi_agent::transcript::user_message("first", 1);
        let second = pi_agent::transcript::user_message("second", 2);
        let transformed = crate::runtime::block_on(transform.call(vec![first, second]));
        assert_eq!(transformed.len(), 2);
        let pi_agent::agent_message::AgentMessage::User(user) = &transformed[0] else {
            panic!("expected a user message");
        };
        assert!(
            serde_json::to_string(user)
                .unwrap_or_default()
                .contains("second"),
            "the handler reversed the conversation"
        );
    }
}
