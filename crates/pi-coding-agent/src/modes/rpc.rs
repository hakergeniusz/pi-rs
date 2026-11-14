//! RPC mode: drive the agent over JSONL on stdin/stdout.
//!
//! Port target: upstream `rpc-entry.ts` + the RPC protocol the TUI host
//! speaks (JSONL requests in, JSONL responses and agent events out). Wire
//! shapes below are rebuilt from pi's protocol conventions (`type` +
//! `camelCase`, request ids echoed verbatim); they must be reconciled line
//! by line against the reference checkout before the P5 exit gate.

use std::io::{BufRead, Write};
use std::sync::Arc;

use pi_agent::agent_loop::{AgentLoopConfig, DefaultConvertToLlm};
use pi_agent::agent_message::AgentMessage;
use pi_agent::events::{AgentEvent, AgentEventSink};
use pi_agent::queues::Queues;
use pi_agent::transcript::tool_declaration;
use pi_agent::types::{AbortSignal, AgentContext, ToolExecutionMode, ToolExecutor, ToolSpec};
use pi_ai::types::JsonObject;
use serde::{Deserialize, Serialize};

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
/// same serialization as JSON mode.
struct RpcEventSink<W: Write> {
    writer: W,
}

impl<W: Write> AgentEventSink for RpcEventSink<W> {
    fn emit(&mut self, event: AgentEvent) -> pi_agent::types::BoxFuture<'_, ()> {
        let line =
            serde_json::Value::Object(crate::modes::json_event::event_to_json(&event)).to_string();
        if writeln!(self.writer, "{line}").is_err() {
            // Client closed the pipe: upstream aborts the run via its output
            // guard (P7 scope).
        }
        Box::pin(async {})
    }
}

/// `runRpcMode`: read request lines, dispatch, stream events. Blocks on the
/// current thread per request, like print mode.
pub fn run_rpc_mode(
    runtime: &mut PrintRuntime,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<i32, String> {
    let mut shutdown = false;
    for line in input.lines() {
        let Ok(line) = line else {
            break; // stdin closed: stop serving.
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let request: RpcRequest = match serde_json::from_str(line) {
            Ok(request) => request,
            Err(error) => {
                write_line(
                    output,
                    &RpcResponse::err(0, format!("invalid request: {error}")),
                )?;
                continue;
            }
        };
        let id = request.id;
        match handle_command(runtime, request.command, output) {
            Ok(Some(payload)) => write_line(output, &RpcResponse::ok(id, payload))?,
            Ok(None) => {
                shutdown = true;
                write_line(output, &RpcResponse::ok(id, RpcResponsePayload::Empty))?;
                break;
            }
            Err(error) => write_line(output, &RpcResponse::err(id, error))?,
        }
    }
    let _ = shutdown;
    Ok(0)
}

fn write_line(out: &mut dyn Write, response: &RpcResponse) -> Result<(), String> {
    let line = serde_json::to_string(response).map_err(|error| error.to_string())?;
    writeln!(out, "{line}").map_err(|error| error.to_string())?;
    out.flush().map_err(|error| error.to_string())
}

/// Runs one command against the runtime. `Ok(None)` means shutdown.
// The per-command bodies mirror upstream's dispatcher; kept in one function
// for protocol-auditability.
#[allow(clippy::too_many_lines)]
fn handle_command(
    runtime: &mut PrintRuntime,
    command: RpcCommand,
    output: &mut dyn Write,
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
fn run_agent_turn(
    runtime: &mut PrintRuntime,
    prompts: Vec<String>,
    output: &mut dyn Write,
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
        prepare_request: None,
        prepare_next_turn: None,
        transform_context: None,
        convert_to_llm: Arc::new(DefaultConvertToLlm),
    };

    let new_messages = futures::executor::block_on(pi_agent::run_agent_loop(
        user_prompts,
        context,
        executor.as_ref(),
        &config,
        &mut queues,
        &mut sink,
        &signal,
        runtime.stream_fn.as_ref(),
    ));

    for message in &new_messages {
        runtime
            .session
            .append_message(message.clone())
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}
