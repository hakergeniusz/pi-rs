//! RPC mode: drive the agent over JSONL on stdin/stdout.
//!
//! Port target: upstream `rpc-entry.ts` + the RPC protocol the TUI host
//! speaks (JSONL requests in, JSONL responses and agent events out). Wire
//! shapes below are rebuilt from pi's protocol conventions (`type` +
//! `camelCase`, request ids echoed verbatim); they must be reconciled line
//! by line against the reference checkout before the P5 exit gate.
//!
//! Slice 4 adds the extension dialog subprotocol (`modes/rpc/rpc-mode.ts:
//! 80-145, 759-824`): `extension_ui_request` lines go out through the
//! [`RpcUiSink`] the extension host binds at load, `extension_ui_response`
//! lines resolve the pending dialog, and a dialog's `timeout` resolves to
//! the method default. The reader thread owns stdin so responses can
//! arrive while the loop is inside a command — upstream routes both
//! through the same stdin handler on its event loop.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Mutex, mpsc};
#[cfg(feature = "ext-js")]
use std::time::Duration;

use pi_agent::agent_loop::{AgentLoopConfig, DefaultConvertToLlm};
use pi_agent::agent_message::AgentMessage;
use pi_agent::events::{AgentEvent, AgentEventSink};
use pi_agent::queues::Queues;
use pi_agent::transcript::tool_declaration;
use pi_agent::types::{AbortSignal, AgentContext, ToolExecutionMode, ToolSpec};
use pi_ai::types::JsonObject;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::modes::print_mode::{PrintRuntime, now_millis};
use crate::modes::print_mode::{parse_model_pattern, resolve_model};
use crate::session::store::build_session_context_from;

/// One request line: `{"type":"command","id":N,"command":"<name>",...}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcRequest {
    #[serde(rename = "type")]
    pub kind: RpcRequestKind,
    /// Request id, echoed in every response for that request.
    #[serde(default)]
    pub id: u64,
    #[serde(flatten)]
    pub command: RpcCommand,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RpcRequestKind {
    #[serde(rename = "command")]
    Command,
}

/// The commands RPC accepts (`rpc-entry.ts` command table). `serde(tag)`
/// keeps the `command` field as the discriminant like upstream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "command")]
pub enum RpcCommand {
    /// Queue the user message and run the agent to a stop.
    #[serde(rename = "prompt")]
    Prompt { message: String },
    /// Like `prompt`, but delivered while the agent is still running
    /// (steering); pi queues it into the current run.
    #[serde(rename = "steerable_prompt")]
    SteerablePrompt { message: String },
    /// Queue a follow-up that runs after the current run settles.
    #[serde(rename = "followup_prompt")]
    FollowupPrompt { message: String },
    /// Abort the current run; responds with the resulting stop reason.
    #[serde(rename = "abort")]
    Abort,
    /// Switch models live (`provider/id` or bare id like the CLI flag).
    #[serde(rename = "set_model")]
    SetModel { model: String },
    /// Switch thinking level live (off/minimal/low/medium/high).
    #[serde(rename = "set_thinking_level")]
    SetThinkingLevel { level: String },
    /// Full agent state snapshot: model, thinking level, session path, tools.
    #[serde(rename = "get_state")]
    GetState,
    /// The persisted transcript so far.
    #[serde(rename = "get_messages")]
    GetMessages,
    /// Stop the RPC host; responds before exiting.
    #[serde(rename = "shutdown")]
    Shutdown,
}

/// One response line: `{"type":"response","id":N,...}` on success,
/// `{"type":"error","id":N,"error":"..."}` on failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum RpcResponse {
    #[serde(rename = "response")]
    Response {
        id: u64,
        #[serde(flatten)]
        payload: RpcResponsePayload,
    },
    #[serde(rename = "error")]
    Error { id: u64, error: String },
}

/// Per-command response payloads. `Empty` is for commands whose only signal
/// is that they succeeded (upstream replies with an empty object).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum RpcResponsePayload {
    Empty,
    State {
        model: String,
        thinking_level: String,
        session_path: Option<String>,
        tools: Vec<String>,
        running: bool,
    },
    Messages {
        messages: Vec<serde_json::Value>,
    },
    Model {
        provider: String,
        id: String,
    },
    ThinkingLevel {
        level: String,
    },
}

impl RpcResponse {
    fn ok(id: u64, payload: RpcResponsePayload) -> Self {
        Self::Response { id, payload }
    }

    fn err(id: u64, error: impl Into<String>) -> Self {
        Self::Error {
            id,
            error: error.into(),
        }
    }
}

/// JSON schema descriptions for every command, served for RPC clients that
/// build UIs off them (upstream exposes the same table from the RPC host).
#[must_use]
pub fn command_schemas() -> JsonObject {
    let entry = |command: &str, description: &str, params: JsonObject| {
        let mut schema = JsonObject::new();
        schema.insert("command".to_string(), serde_json::json!(command));
        schema.insert("description".to_string(), serde_json::json!(description));
        schema.insert("params".to_string(), serde_json::Value::Object(params));
        schema
    };
    let schemas = vec![
        entry(
            "prompt",
            "Send a user message and run the agent.",
            params(&[("message", "string")]),
        ),
        entry(
            "steerable_prompt",
            "Steer the running agent with a new message.",
            params(&[("message", "string")]),
        ),
        entry(
            "followup_prompt",
            "Queue a message that runs after the current run settles.",
            params(&[("message", "string")]),
        ),
        entry("abort", "Abort the current run.", params(&[])),
        entry(
            "set_model",
            "Switch models live.",
            params(&[("model", "provider/id or bare id")]),
        ),
        entry(
            "set_thinking_level",
            "Switch thinking level live.",
            params(&[("level", "off|minimal|low|medium|high")]),
        ),
        entry("get_state", "Agent state snapshot.", params(&[])),
        entry("get_messages", "Persisted transcript.", params(&[])),
        entry("shutdown", "Stop the RPC host.", params(&[])),
    ];
    let mut object = JsonObject::new();
    object.insert("commands".to_string(), serde_json::json!(schemas));
    object
}

fn params(entries: &[(&str, &str)]) -> JsonObject {
    let mut object = JsonObject::new();
    for (name, kind) in entries {
        object.insert((*name).to_string(), serde_json::json!(kind));
    }
    object
}

/// Sink streaming agent events to the RPC client as JSON lines, using the
/// same serialization as JSON mode. Each event takes the shared output
/// lock briefly: an extension dialog opened mid-run (context transform,
/// tool call) writes through the same stream between events without
/// deadlocking.
struct RpcEventSink {
    writer: RpcOut,
}

impl AgentEventSink for RpcEventSink {
    fn emit(&mut self, event: AgentEvent) -> pi_agent::types::BoxFuture<'_, ()> {
        let line =
            serde_json::Value::Object(crate::modes::json_event::event_to_json(&event)).to_string();
        let mut writer = self.writer.lock().expect("rpc output");
        if writeln!(writer, "{line}").is_err() {
            // Client closed the pipe: upstream aborts the run via its output
            // guard (P7 scope).
        }
        Box::pin(async {})
    }
}

/// `runRpcMode`: read request lines, dispatch, stream events. Blocks on the
/// current thread per request, like print mode.
/// A `Send` stdout writer for the bridge: [`std::io::Stdout`] itself is
/// `Send`, and the lock is taken per write, so the guard never crosses a
/// thread (`StdoutLock` is not `Send`).
pub struct RpcStdout(std::io::Stdout);

impl RpcStdout {
    #[must_use]
    pub fn new() -> Self {
        Self(std::io::stdout())
    }
}

impl Default for RpcStdout {
    fn default() -> Self {
        Self::new()
    }
}

impl Write for RpcStdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().flush()
    }
}

/// The RPC stream handle: every writer takes the lock per line, never
/// across a call that can write again (dialog requests interleave with
/// agent events on the same stream — `rpc-mode.ts`'s single `output`).
type RpcOut = Arc<Mutex<Box<dyn Write + Send>>>;

/// The RPC plumbing shared by the command loop and the dialog bridge.
pub struct RpcBridge {
    /// Command lines the reader thread forwards — everything that is not
    /// an `extension_ui_response` (`rpc-mode.ts:782-799` routes by type on
    /// one stdin stream).
    pub commands: Receiver<String>,
    /// The RPC stream, shared with the dialog sink.
    pub out: RpcOut,
    /// The UI sink the extension host binds at load
    /// (`bindExtensions`'s `uiContext`, before `session_start`).
    #[cfg(feature = "ext-js")]
    pub sink: Arc<dyn pi_ext_js::UiSink>,
}

/// Wires the bridge: the reader thread owns the input and routes by
/// `type`, the sink shares the output. Dropping `commands` ends the loop
/// (stdin closed — `onInputEnd`, `rpc-mode.ts:822-824`).
pub fn create_bridge(input: Box<dyn BufRead + Send>, output: Box<dyn Write + Send>) -> RpcBridge {
    let out: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(output));
    let pending: Arc<Mutex<HashMap<String, SyncSender<Value>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (commands_tx, commands_rx) = mpsc::channel::<String>();
    let reader_out = Arc::clone(&out);
    let reader_pending = Arc::clone(&pending);
    std::thread::Builder::new()
        .name("rpc-stdin".to_string())
        .spawn(move || {
            for line in input.lines() {
                let Ok(line) = line else { break };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                // Extension UI responses resolve their dialog; everything
                // else is a command (`rpc-mode.ts:782-799`).
                let routed = serde_json::from_str::<Value>(line).ok().filter(|value| {
                    value.get("type").and_then(Value::as_str) == Some("extension_ui_response")
                });
                if let Some(response) = routed {
                    let id = response
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let sender = reader_pending.lock().expect("rpc ui pending").remove(&id);
                    if let Some(sender) = sender {
                        // A late response after the timeout finds no
                        // receiver and drops, like upstream's cleaned-up
                        // pending entry.
                        let _ = sender.send(response);
                    }
                    continue;
                }
                if commands_tx.send(line.to_string()).is_err() {
                    break;
                }
            }
            let _ = reader_out;
        })
        .expect("spawn the rpc stdin reader");
    RpcBridge {
        commands: commands_rx,
        out: Arc::clone(&out),
        #[cfg(feature = "ext-js")]
        sink: Arc::new(RpcUiSink {
            out,
            pending,
            receivers: Mutex::new(HashMap::new()),
            timeouts: Mutex::new(HashMap::new()),
        }),
    }
}

/// The `extension_ui_request` bridge (`createExtensionUIContext` +
/// `createDialogPromise`, `rpc-mode.ts:94-145`): dialog requests carry the
/// wire shape `{type, id, method, ...}`; the matching
/// `extension_ui_response` resolves them, and a `timeout` resolves to the
/// method default — the JS glue parses the empty payload into that
/// default, so [`Value::Null`] is the timeout answer. Fire-and-forget
/// methods (`notify`, `setStatus`, `setWidget`, `setTitle`,
/// `set_editor_text`) only write.
#[cfg(feature = "ext-js")]
struct RpcUiSink {
    out: Arc<Mutex<Box<dyn Write + Send>>>,
    pending: Arc<Mutex<HashMap<String, SyncSender<Value>>>>,
    /// One-slot receiver per dialog, registered at request time so a fast
    /// response can never race the wait.
    receivers: Mutex<HashMap<String, Receiver<Value>>>,
    timeouts: Mutex<HashMap<String, Option<u64>>>,
}

#[cfg(feature = "ext-js")]
impl pi_ext_js::UiSink for RpcUiSink {
    fn request(&self, request: &Value) {
        let id = request
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // Only dialog methods can be waited on (`createDialogPromise`,
        // rpc-mode.ts:94-145); fire-and-forget requests are write-only, so
        // they register nothing (a wait on one answers the default).
        let is_dialog = matches!(
            request.get("method").and_then(Value::as_str),
            Some("select" | "confirm" | "input" | "editor")
        );
        let timeout = request.get("timeout").and_then(Value::as_u64);
        if is_dialog {
            self.timeouts
                .lock()
                .expect("rpc ui timeouts")
                .insert(id.clone(), timeout);
            let (sender, receiver) = mpsc::sync_channel(1);
            self.pending
                .lock()
                .expect("rpc ui pending")
                .insert(id.clone(), sender);
            self.receivers
                .lock()
                .expect("rpc ui receivers")
                .insert(id, receiver);
        }
        let mut wire = serde_json::Map::new();
        wire.insert(
            "type".to_string(),
            Value::String("extension_ui_request".to_string()),
        );
        if let Some(object) = request.as_object() {
            for (key, value) in object {
                wire.insert(key.clone(), value.clone());
            }
        }
        let mut out = self.out.lock().expect("rpc output");
        let _ = writeln!(out, "{}", Value::Object(wire));
        let _ = out.flush();
    }

    fn wait_response(&self, id: &str, _method: &str) -> Value {
        let receiver = self.receivers.lock().expect("rpc ui receivers").remove(id);
        let timeout = self
            .timeouts
            .lock()
            .expect("rpc ui timeouts")
            .remove(id)
            .flatten();
        let Some(receiver) = receiver else {
            return Value::Null;
        };
        let response = match timeout {
            None => receiver.recv().ok(),
            Some(millis) => receiver.recv_timeout(Duration::from_millis(millis)).ok(),
        };
        response.unwrap_or(Value::Null)
    }
}

/// Serves commands until stdin closes or the host asks for shutdown.
/// The extension UI stays bound for the whole run: dialogs opened by any
/// command's extension events cross the stream (`setUIContext` applies at
/// load, upstream's `rebindSession` re-binds per session — the port has
/// one session per run).
pub fn run_rpc_mode(runtime: &mut PrintRuntime, bridge: &RpcBridge) -> Result<i32, String> {
    for line in &bridge.commands {
        let request: RpcRequest = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                write_line(
                    &bridge.out,
                    &RpcResponse::err(0, format!("invalid request: {error}")),
                )?;
                continue;
            }
        };
        let id = request.id;
        match handle_command(runtime, request.command, Arc::clone(&bridge.out)) {
            Ok(Some(payload)) => write_line(&bridge.out, &RpcResponse::ok(id, payload))?,
            Ok(None) => {
                write_line(&bridge.out, &RpcResponse::ok(id, RpcResponsePayload::Empty))?;
                clear_extension_ui(runtime);
                break;
            }
            Err(error) => write_line(&bridge.out, &RpcResponse::err(id, error))?,
        }
    }
    clear_extension_ui(runtime);
    Ok(0)
}

/// Unbinds the dialog surface (`setUIContext(undefined)`) — the runner
/// returns to the headless matrix for whatever runs after the mode.
#[cfg(feature = "ext-js")]
fn clear_extension_ui(runtime: &PrintRuntime) {
    if let Some(runner) = &runtime.extension_runner {
        runner
            .lock()
            .expect("extension runner mutex poisoned")
            .clear_ui_context();
    }
}

#[cfg(not(feature = "ext-js"))]
fn clear_extension_ui(runtime: &PrintRuntime) {
    let _ = runtime;
}

fn write_line(out: &RpcOut, response: &RpcResponse) -> Result<(), String> {
    let line = serde_json::to_string(response).map_err(|error| error.to_string())?;
    let mut writer = out.lock().expect("rpc output");
    writeln!(writer, "{line}").map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())
}

/// Runs one command against the runtime. `Ok(None)` means shutdown.
// The per-command bodies mirror upstream's dispatcher; kept in one function
// for protocol-auditability.
#[allow(clippy::too_many_lines)]
fn handle_command(
    runtime: &mut PrintRuntime,
    command: RpcCommand,
    output: RpcOut,
) -> Result<Option<RpcResponsePayload>, String> {
    match command {
        RpcCommand::Prompt { message }
        | RpcCommand::SteerablePrompt { message }
        | RpcCommand::FollowupPrompt { message } => {
            // pi delivers steering through the run's queue; without a run in
            // flight the steerable/followup variants degrade to a normal
            // prompt. Queue plumbing lands with the interactive host (P6).
            run_agent_turn(runtime, vec![message], output)?;
            Ok(Some(RpcResponsePayload::Empty))
        }
        RpcCommand::Abort => {
            // No run is in flight under the thread-per-request model; the
            // signal plumbing wires in with P6/P7 queue work.
            Ok(Some(RpcResponsePayload::Empty))
        }
        RpcCommand::SetModel { model } => {
            let pattern = parse_model_pattern(&model);
            let resolved = resolve_model(Some(&pattern), None, None)?;
            runtime.model = resolved.clone();
            Ok(Some(RpcResponsePayload::Model {
                provider: resolved.provider.clone(),
                id: resolved.id.clone(),
            }))
        }
        RpcCommand::SetThinkingLevel { level } => {
            let parsed = crate::modes::print_mode::thinking_level_from_str(&level)?;
            runtime.thinking_level = parsed;
            Ok(Some(RpcResponsePayload::ThinkingLevel {
                level: parsed.as_str().to_string(),
            }))
        }
        RpcCommand::GetState => Ok(Some(RpcResponsePayload::State {
            model: format!("{}/{}", runtime.model.provider, runtime.model.id),
            thinking_level: runtime.thinking_level.as_str().to_string(),
            session_path: runtime
                .session
                .session_file()
                .map(|path| path.display().to_string()),
            tools: runtime.executor.tools().iter().map(tool_name).collect(),
            running: false,
        })),
        RpcCommand::GetMessages => {
            let entries = runtime.session.get_entries();
            let leaf_id = runtime.session.get_leaf_id().map(str::to_string);
            let context = build_session_context_from(&entries, leaf_id.as_deref());
            let values = context
                .messages
                .iter()
                .map(|message| serde_json::to_value(message).unwrap_or_default())
                .collect();
            Ok(Some(RpcResponsePayload::Messages { messages: values }))
        }
        RpcCommand::Shutdown => Ok(None),
    }
}

fn tool_name(spec: &ToolSpec) -> String {
    spec.name.clone()
}

/// One full agent run, events streamed to `output` as they happen.
#[allow(clippy::too_many_lines)] // mirrors the TS run loop; split when extending
fn run_agent_turn(
    runtime: &mut PrintRuntime,
    prompts: Vec<String>,
    output: RpcOut,
) -> Result<(), String> {
    let executor = runtime.executor.clone();
    let specs: Vec<ToolSpec> = executor.tools();

    let leaf_id = runtime.session.get_leaf_id().map(str::to_string);
    let session_context =
        build_session_context_from(&runtime.session.get_entries(), leaf_id.as_deref());
    let mut context_messages = session_context.messages;
    if context_messages.is_empty() {
        if let Some(system) = pi_agent::transcript::create_initial_system_message(
            Some(&runtime.system_prompt),
            &specs.iter().map(tool_declaration).collect::<Vec<_>>(),
        ) {
            context_messages.push(AgentMessage::System(system));
        }
    }
    let context = AgentContext {
        messages: context_messages,
        tools: specs.clone(),
    };

    let now = now_millis();
    let user_prompts: Vec<AgentMessage> = prompts
        .into_iter()
        .map(|message| pi_agent::transcript::user_message(&message, now))
        .collect();

    let mut queues = Queues::new();
    let mut sink = RpcEventSink { writer: output };
    let signal = AbortSignal::new();
    // Under a virtual selection every request routes first
    // (`_installAgentRequestProjection`).
    #[cfg(feature = "ext-js")]
    let routing = crate::virtual_models::TurnRouting::install(
        &runtime.model,
        runtime.extension_runner.as_ref(),
        runtime.api_key.clone(),
        &runtime.session,
    );
    let config = AgentLoopConfig {
        model: runtime.model.clone(),
        thinking_level: runtime.thinking_level,
        live_settings: None,
        session_id: None,
        api_key: runtime.api_key.clone(),
        access_token: runtime.access_token.clone(),
        tool_execution: ToolExecutionMode::Parallel,
        hooks: pi_agent::types::ToolCallHooks {
            before: None,
            after: None,
        },
        finish_turn: None,
        #[cfg(feature = "ext-js")]
        prepare_request: routing.as_ref().map(|routing| {
            Arc::clone(&routing.hook) as Arc<dyn pi_agent::agent_loop::PrepareRequest>
        }),
        #[cfg(not(feature = "ext-js"))]
        prepare_request: None,
        prepare_next_turn: None,
        transform_context: runtime.transform_context(),
        convert_to_llm: Arc::new(DefaultConvertToLlm),
    };

    #[cfg(feature = "ext-js")]
    let loop_stream_fn: Arc<dyn pi_agent::types::StreamFn> = match &routing {
        Some(routing) => routing.stream_fn(),
        None => Arc::clone(&runtime.stream_fn),
    };
    #[cfg(not(feature = "ext-js"))]
    let loop_stream_fn: Arc<dyn pi_agent::types::StreamFn> = Arc::clone(&runtime.stream_fn);
    // The auto-retry machine (`agent-session.ts` `_handlePostAgentRun`); the
    // RPC sink forwards the events like any other.
    let mut retry = crate::modes::retry::AgentRetry::new(runtime.retry_settings);

    let mut compaction = crate::modes::compaction::AutoCompaction::new(runtime.compaction_settings);
    let new_messages = loop {
        // `_compactBeforeNextAssistantResponse` (`agent-session.ts:839-848`):
        // the threshold gate keys on the model about to answer; under a
        // virtual selection upstream re-checks the ROUTED model after the
        // resolve (:901-904) — deferred with the prepare-hook rework.
        if !crate::virtual_models::is_virtual_model(&runtime.model)
            && compaction.exceeds_threshold_for_session(&runtime.session, &runtime.model)
        {
            compaction.run(
                pi_agent::events::CompactionReason::Threshold,
                false,
                &mut runtime.session,
                &runtime.model,
                runtime.api_key.as_deref(),
                runtime.access_token.as_deref(),
                runtime.thinking_level,
                runtime.stream_fn.as_ref(),
                &mut sink,
            );
        }
        let messages = crate::runtime::block_on(pi_agent::run_agent_loop(
            user_prompts.clone(),
            context.clone(),
            executor.as_ref(),
            &config,
            &mut queues,
            &mut sink,
            &signal,
            loop_stream_fn.as_ref(),
        ));
        let aborted = signal.aborted();
        if retry.will_retry(aborted, &messages) {
            let (attempt, max_attempts, delay_ms, error_message) = retry.schedule(&messages);
            // `_prepareRetry` (`agent-session.ts:3959-3963`): the failed
            // attempt persists raw and drops out of the projection.
            for entry in crate::modes::retry::omit_attempt(&messages, &mut runtime.session) {
                crate::runtime::block_on(
                    sink.emit(pi_agent::events::AgentEvent::EntryAppended { entry }),
                );
            }
            crate::runtime::block_on(sink.emit(pi_agent::events::AgentEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                error_message: error_message.clone(),
            }));
            if retry.sleep(delay_ms, &signal) {
                continue;
            }
            crate::runtime::block_on(sink.emit(pi_agent::events::AgentEvent::AutoRetryEnd {
                success: false,
                attempt,
                final_error: Some(error_message),
            }));
            break messages;
        }
        if retry.attempt() > 0 {
            let success = crate::modes::retry::run_succeeded(&messages);
            crate::runtime::block_on(sink.emit(pi_agent::events::AgentEvent::AutoRetryEnd {
                success,
                attempt: retry.attempt(),
                final_error: if success {
                    None
                } else {
                    Some(crate::modes::retry::last_error_text(&messages))
                },
            }));
            if success {
                retry.reset();
            }
        }
        // Upstream `_handlePostAgentRun` (`agent-session.ts:2062-2098`)
        // decides the retry first — compaction only runs when no retry was
        // scheduled.
        let mut full_messages = context.messages.clone();
        full_messages.extend(messages.iter().cloned());
        match compaction.post_run_decision(
            aborted,
            &messages,
            &full_messages,
            &runtime.session,
            &runtime.model,
        ) {
            crate::modes::compaction::Decision::None => break messages,
            crate::modes::compaction::Decision::Threshold => {
                compaction.run(
                    pi_agent::events::CompactionReason::Threshold,
                    false,
                    &mut runtime.session,
                    &runtime.model,
                    runtime.api_key.as_deref(),
                    runtime.access_token.as_deref(),
                    runtime.thinking_level,
                    runtime.stream_fn.as_ref(),
                    &mut sink,
                );
                break messages;
            }
            crate::modes::compaction::Decision::Overflow { will_retry } => {
                if will_retry {
                    // `_omitRecoveryAttempt` before the recovery compaction
                    // (`agent-session.ts:3209-3211`).
                    for entry in crate::modes::retry::omit_attempt(&messages, &mut runtime.session)
                    {
                        crate::runtime::block_on(
                            sink.emit(pi_agent::events::AgentEvent::EntryAppended { entry }),
                        );
                    }
                }
                // The overflow compact-and-retry re-runs the turn once
                // (`agent.continue()`).
                let continued = compaction.run(
                    pi_agent::events::CompactionReason::Overflow,
                    will_retry,
                    &mut runtime.session,
                    &runtime.model,
                    runtime.api_key.as_deref(),
                    runtime.access_token.as_deref(),
                    runtime.thinking_level,
                    runtime.stream_fn.as_ref(),
                    &mut sink,
                );
                if will_retry && continued {
                    continue;
                }
                break messages;
            }
            crate::modes::compaction::Decision::RecoveryFailed { message } => {
                // The verbatim one-shot failure (`agent-session.ts:3187-3206`).
                crate::runtime::block_on(sink.emit(pi_agent::events::AgentEvent::CompactionEnd {
                    reason: pi_agent::events::CompactionReason::Overflow,
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(message),
                }));
                break messages;
            }
        }
    };
    #[cfg(feature = "ext-js")]
    if let Some(routing) = &routing {
        // `entry_appended` for each router-state record
        // (`agent-session.ts:894-897`).
        for entry in routing.persist_state(&mut runtime.session) {
            crate::runtime::block_on(
                sink.emit(pi_agent::events::AgentEvent::EntryAppended { entry }),
            );
        }
    }

    for message in &new_messages {
        runtime
            .session
            .append_message(message.clone())
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::BufReader;

    /// A process pipe pair drives the bridge deterministically: the test
    /// writes lines on the writer side while the reader thread consumes.
    struct BridgeFixture {
        bridge: RpcBridge,
        writer: std::io::PipeWriter,
        out: SharedBuf,
    }

    impl BridgeFixture {
        fn new() -> Self {
            let (reader, writer) = std::io::pipe().expect("pipe");
            let out = SharedBuf::default();
            let bridge = create_bridge(Box::new(BufReader::new(reader)), Box::new(out.clone()));
            Self {
                bridge,
                writer,
                out,
            }
        }

        fn write_line(&self, line: &str) {
            use std::io::Write as _;
            (&self.writer)
                .write_all(format!("{line}\n").as_bytes())
                .expect("pipe write");
        }

        fn output_lines(&self) -> Vec<String> {
            self.out.string().lines().map(str::to_string).collect()
        }
    }

    /// The bridge's output, shared so tests can assert on it after the run.
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("shared buf").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SharedBuf {
        fn string(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("shared buf")).into_owned()
        }
    }

    #[test]
    fn bridge_routes_commands_and_ignores_unknown_ui_responses() {
        let fixture = BridgeFixture::new();
        fixture.write_line(r#"{"type":"command","id":1,"command":"get_state"}"#);
        // No dialog is pending; the response drops on the floor.
        fixture.write_line(r#"{"type":"extension_ui_response","id":"ui-9","value":"x"}"#);
        fixture.write_line(r#"{"type":"command","id":2,"command":"get_state"}"#);
        let first = fixture.bridge.commands.recv().expect("command");
        assert!(first.contains("\"command\":\"get_state\""));
        let second = fixture.bridge.commands.recv().expect("command");
        assert!(second.contains("\"id\":2"));
        drop(fixture.writer);
        assert!(
            fixture.bridge.commands.recv().is_err(),
            "stdin close ends the command stream"
        );
    }

    #[cfg(feature = "ext-js")]
    #[test]
    fn sink_request_writes_wire_shape_and_response_resolves() {
        let fixture = BridgeFixture::new();
        fixture.bridge.sink.request(&json!({
            "id": "ui-1",
            "method": "select",
            "title": "Pick",
            "options": ["a", "b"],
        }));
        let lines = fixture.output_lines();
        assert_eq!(lines.len(), 1, "one request crossed");
        assert!(
            lines[0].starts_with(r#"{"type":"extension_ui_request","id":"ui-1","method":"select""#),
            "type leads the wire shape, like upstream's output object: {}",
            lines[0]
        );
        fixture.write_line(r#"{"type":"extension_ui_response","id":"ui-1","value":"a"}"#);
        let response = fixture.bridge.sink.wait_response("ui-1", "select");
        // The sink is transport-level: the whole frame crosses; the JS
        // glue's per-method parseResponse extracts the payload.
        assert_eq!(
            response,
            json!({"type":"extension_ui_response","id":"ui-1","value":"a"})
        );
    }

    #[cfg(feature = "ext-js")]
    #[test]
    fn sink_timeout_resolves_to_the_method_default() {
        let fixture = BridgeFixture::new();
        fixture.bridge.sink.request(&json!({
            "id": "ui-2",
            "method": "confirm",
            "title": "Proceed",
            "timeout": 40,
        }));
        let started = std::time::Instant::now();
        let response = fixture.bridge.sink.wait_response("ui-2", "confirm");
        assert_eq!(
            response,
            Value::Null,
            "the JS glue parses null to the default"
        );
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(40),
            "the timeout delayed the default"
        );
    }

    #[cfg(feature = "ext-js")]
    #[test]
    fn sink_fire_and_forget_requests_only_write() {
        let fixture = BridgeFixture::new();
        fixture.bridge.sink.request(&json!({
            "id": "ui-3",
            "method": "notify",
            "message": "hello",
            "notifyType": "info",
        }));
        assert_eq!(fixture.output_lines().len(), 1, "the notify crossed");
        // No waiter was registered: the wait returns the default at once.
        let response = fixture.bridge.sink.wait_response("ui-3", "notify");
        assert_eq!(response, Value::Null);
    }
}
