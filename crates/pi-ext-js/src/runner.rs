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
//! fan out through error listeners (`emitError`).

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::Value as Json;

use crate::engine::{ContextKind, HostCore, JsHost};
use crate::error::{ExtensionError, JsHostError};
use crate::events::{
    self,
    parse_session_outcome,
    BeforeAgentStartCombined,
    BeforeAgentStartResult,
    BeforeAgentStartTrigger,
    BoundaryDispatchResult,
    InputResult,
    ProjectTrustResult,
    SessionOutcome,
    SessionBeforeResult,
    ToolCallResult,
    ToolResultPatch,
};

/// One registered tool as the runner sees it (the E1 `ToolInfo`:
/// name, description, parameters, exposure, namespace — §4.2 of
/// `docs/EXTENSIONS.md`; `promptGuidelines`/`reachesTools`/`annotations`
/// are E2 host concerns).
#[derive(Debug, Clone, Deserialize)]
pub struct ToolInfo {
    /// The tool name.
    pub name: String,
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

/// The runner's error listeners (`onError`).
type ErrorListeners = Vec<Box<dyn Fn(&ExtensionError)>>;

/// The runner: dispatch semantics over the loaded extensions.
pub struct ExtensionRunner {
    host: JsHost,
    core: std::sync::Arc<Mutex<HostCore>>,
    extensions: Vec<ExtensionSnapshot>,
    error_listeners: ErrorListeners,
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
        })
    }

    /// The shared host core, for host-level state assertions in tests.
    pub fn core(&self) -> &std::sync::Arc<Mutex<HostCore>> {
        &self.core
    }

    /// Registers an error listener (`onError`); every handler error and
    /// load-time failure reaches it.
    pub fn on_error(&mut self, listener: Box<dyn Fn(&ExtensionError)>) {
        self.error_listeners.push(listener);
    }

    /// Paths of the loaded extensions, in load order.
    pub fn extension_paths(&self) -> Vec<String> {
        self.extensions.iter().map(|ext| ext.path.clone()).collect()
    }

    /// Whether any extension registered a handler for `event`.
    pub fn has_handlers(&self, event: &str) -> bool {
        self.extensions
            .iter()
            .any(|ext| ext.events.get(event).is_some_and(|handlers| !handlers.is_empty()))
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
    /// extensions in load order, and the initial active set is every tool
    /// with `defaultActive !== false`.
    pub fn bind(&mut self) -> Result<(), JsHostError> {
        self.refresh_snapshots()?;
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
                if tool.default_active && tool.exposure != "hidden" {
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

    /// Re-reads the JS-side registries into the runner's snapshots.
    pub fn refresh_snapshots(&mut self) -> Result<(), JsHostError> {
        let raw = self.host.inspect()?;
        self.extensions = serde_json::from_str(&raw).map_err(|err| JsHostError::Job(err.to_string()))?;
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
        self.all_registered_tools().into_iter().find(|tool| tool.name == name)
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
            .ok_or_else(|| JsHostError::Job(format!("extension `{}` vanished", command.extension_path)))?;
        let id = self.host.session_begin("{}", ContextKind::Command, None)?;
        self.host.session_call_command(id, index, &command.name, args)?;
        self.host.drive_jobs()?;
        let outcome = parse_session_outcome(&self.host.session_last(id)?)?;
        self.host.session_end(id)?;
        Self::outcome_result(outcome)?;
        Ok(true)
    }

    /// Executes a registered tool: first-wins lookup, the session
    /// machinery calls `execute(toolCallId, params, signal, onUpdate,
    /// ctx)`. The returned JSON is the `AgentToolResult` the host records.
    pub fn execute_tool(&mut self, name: &str, tool_call_id: &str, args: &Json) -> Result<Json, ToolExecutionError> {
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
            .session_begin(&serde_json::to_string(args).unwrap_or_default(), ContextKind::Tool, Some(tool_call_id))
            .map_err(|err| ToolExecutionError {
                name: name.to_string(),
                message: err.to_string(),
            })?;
        let call = self.host.session_call_tool(id, index, name, tool_call_id);
        let driven = call.and_then(|()| self.host.drive_jobs());
        let outcome = match driven.and_then(|()| self.host.session_last(id)) {
            Ok(raw) => parse_session_outcome(&raw),
            Err(err) => {
                let _ = self.host.session_end(id);
                return Err(ToolExecutionError {
                    name: name.to_string(),
                    message: err.to_string(),
                });
            }
        };
        let _ = self.host.session_end(id);
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

    fn report_error(&self, extension_index: usize, event: &str, error: crate::events::HandlerErrorJson) {
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
        let payload = serde_json::to_string(event).map_err(|err| JsHostError::Job(err.to_string()))?;
        let id = self.host.session_begin(&payload, kind, tool_call_id)?;
        let mut outcomes = Vec::with_capacity(handlers.len());
        let mut fatal: Option<JsHostError> = None;
        for (index, handler) in handlers {
            if fatal.is_some() {
                break;
            }
            let call = self.host.session_call(id, *index, event_name, *handler);
            if let Err(err) = call.and_then(|()| self.host.drive_jobs()) {
                fatal = Some(err);
                break;
            }
            match parse_session_outcome(&self.host.session_last(id)?) {
                Ok(outcome) => outcomes.push((*index, outcome)),
                Err(err) => fatal = Some(JsHostError::Job(err.to_string())),
            }
        }
        let _ = self.host.session_end(id);
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
            if is_session_before {
                if let Some(value) = outcome.value {
                    last = Some(value.clone());
                    if value.get("cancel").and_then(Json::as_bool) == Some(true) {
                        break;
                    }
                }
            }
        }
        Ok(if is_session_before { last } else { None })
    }

    /// The `session_before_*` emit with the parsed cancel result.
    pub fn emit_session_before(&mut self, event: &Json) -> Result<SessionBeforeResult, JsHostError> {
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
        let payload = serde_json::to_string(&event).map_err(|err| JsHostError::Job(err.to_string()))?;
        let id = self.host.session_begin(&payload, ContextKind::Event, None)?;
        let mut result: Option<ToolCallResult> = None;
        let mut fatal: Option<JsHostError> = None;
        for (index, handler) in handlers {
            if let Err(err) = self
                .host
                .session_call(id, index, "tool_call", handler)
                .and_then(|()| self.host.drive_jobs())
            {
                let _ = self.host.session_end(id);
                fatal = Some(err);
                break;
            }
            let outcome = match parse_session_outcome(&self.host.session_last(id)?) {
                Ok(outcome) => outcome,
                Err(err) => {
                    let _ = self.host.session_end(id);
                    fatal = Some(JsHostError::Job(err.to_string()));
                    break;
                }
            };
            if let Some(error) = outcome.error {
                // No catch upstream: the error propagates to the caller.
                let _ = self.host.session_end(id);
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
        let event: Json =
            serde_json::from_str(&self.host.session_event(id)?).map_err(|err| JsHostError::Job(err.to_string()))?;
        self.host.session_end(id)?;
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
            let payload = serde_json::to_string(&current).map_err(|err| JsHostError::Job(err.to_string()))?;
            let id = self.host.session_begin(&payload, ContextKind::Event, None)?;
            self.host.session_call(id, index, "tool_result", handler)?;
            self.host.drive_jobs()?;
            let outcome = parse_session_outcome(&self.host.session_last(id)?);
            self.host.session_end(id)?;
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
        let original_text = current.get("text").and_then(Json::as_str).unwrap_or_default().to_string();
        let mut images_supplied = false;
        for (index, handler) in handlers {
            let payload = serde_json::to_string(&current).map_err(|err| JsHostError::Job(err.to_string()))?;
            let id = self.host.session_begin(&payload, ContextKind::Event, None)?;
            self.host.session_call(id, index, "input", handler)?;
            self.host.drive_jobs()?;
            let outcome = parse_session_outcome(&self.host.session_last(id)?);
            self.host.session_end(id)?;
            let outcome = outcome?;
            if let Some(error) = outcome.error {
                self.report_error(index, "input", error);
                continue;
            }
            let Some(value) = outcome.value else {
                continue;
            };
            let action = value.get("action").and_then(Json::as_str).unwrap_or_default().to_string();
            if action == "handled" {
                return Ok(InputResult::Handled { result: value });
            }
            if action == "transform" {
                if let Some(text) = value.get("text").and_then(Json::as_str) {
                    current["text"] = Json::String(text.to_string());
                }
                if let Some(images) = value.get("images") {
                    if !images.is_null() {
                        current["images"] = images.clone();
                        images_supplied = true;
                    }
                }
            }
        }
        let text = current.get("text").and_then(Json::as_str).unwrap_or_default().to_string();
        if text != original_text || images_supplied {
            let images = current.get("images").filter(|images| !images.is_null()).cloned();
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
            object.insert("type".to_string(), Json::String("before_agent_start".to_string()));
            object.insert("systemPrompt".to_string(), Json::String(render(&options)));
            object.insert("systemPromptOptions".to_string(), options.clone());
            let payload = serde_json::to_string(&event).map_err(|err| JsHostError::Job(err.to_string()))?;
            let id = self.host.session_begin(&payload, ContextKind::Event, None)?;
            self.host.session_call(id, index, "before_agent_start", handler)?;
            self.host.drive_jobs()?;
            let outcome = parse_session_outcome(&self.host.session_last(id)?);
            self.host.session_end(id)?;
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
            let payload = serde_json::to_string(&event).map_err(|err| JsHostError::Job(err.to_string()))?;
            let id = self.host.session_begin(&payload, ContextKind::Event, None)?;
            self.host.session_call(id, index, "message_end", handler)?;
            self.host.drive_jobs()?;
            let outcome = parse_session_outcome(&self.host.session_last(id)?);
            self.host.session_end(id)?;
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
                        message: "message_end handlers must return a message with the same role".to_string(),
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
            let payload = serde_json::to_string(&event).map_err(|err| JsHostError::Job(err.to_string()))?;
            let id = self.host.session_begin(&payload, ContextKind::Event, None)?;
            self.host.session_call(id, index, &event_type, handler)?;
            self.host.drive_jobs()?;
            let outcome = parse_session_outcome(&self.host.session_last(id)?);
            self.host.session_end(id)?;
            if let Ok(outcome) = &outcome {
                if let Some(error) = &outcome.error {
                    self.report_error(index, &event_type, error.clone());
                } else if let Some(value) = &outcome.value {
                    if let Some(handler_entries) = value.get("entries") {
                        if !handler_entries.is_null() {
                            entries = handler_entries.clone();
                        }
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
    pub fn emit_project_trust(&mut self, cwd: &str) -> Result<(Option<ProjectTrustResult>, Vec<ExtensionError>), JsHostError>
    {
        let handlers = self.handlers_for("project_trust");
        let event = events::project_trust(cwd);
        let mut errors = Vec::new();
        for (index, handler) in handlers {
            let payload = serde_json::to_string(&event).map_err(|err| JsHostError::Job(err.to_string()))?;
            let id = self.host.session_begin(&payload, ContextKind::ProjectTrust, None)?;
            self.host.session_call(id, index, "project_trust", handler)?;
            self.host.drive_jobs()?;
            let outcome = parse_session_outcome(&self.host.session_last(id)?);
            self.host.session_end(id)?;
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
}
