//! JSON mode: run the agent loop and emit every event as a JSON line on
//! stdout.
//!
//! Port of `modes/json-event.ts` (61 lines) on top of the same runtime
//! resolution as print mode. Each line is one `AgentEvent` serialized with
//! pi's wire casing (`camelCase`, discriminant in `type`). Deviations, to
//! reconcile when the reference checkout is back:
//! - upstream emits the coding-agent's extended `agent_end` (with
//!   `willRetry`); the port emits the loop's plain event.
//! - upstream subscribes mid-loop (events stream as they happen); the port
//!   emits through a sink that writes synchronously, which is the same
//!   visible byte stream for a non-aborted run.

use std::io::{self, BufWriter, Write};
use std::sync::Arc;

use pi_agent::agent_loop::{AgentLoopConfig, DefaultConvertToLlm};
use pi_agent::agent_message::AgentMessage;
use pi_agent::events::{AgentEvent, AgentEventSink};
use pi_agent::queues::Queues;
use pi_agent::transcript::tool_declaration;
use pi_agent::types::{AbortSignal, AgentContext, ToolExecutionMode, ToolExecutor, ToolSpec};
use pi_ai::types::{JsonObject, StopReason};

use crate::modes::print_mode::{PrintModeOptions, PrintRuntime, now_millis};
use crate::session::store::build_session_context_from;

/// Serializes one [`AgentEvent`] into its wire object: `type` plus the
/// payload fields, `camelCase` like pi's JSON protocol.
// Upstream `json-event.ts` serializes the same union; this is the explicit
// shape-by-shape port of it.
pub fn event_to_json(event: &AgentEvent) -> JsonObject {
    let mut object = JsonObject::new();
    object.insert("type".to_string(), serde_json::json!(event.kind()));
    match event {
        AgentEvent::AgentStart | AgentEvent::TurnStart => {}
        AgentEvent::AgentEnd { messages } => {
            object.insert(
                "messages".to_string(),
                serde_json::to_value(messages).unwrap_or_default(),
            );
        }
        AgentEvent::TurnEnd {
            message,
            tool_results,
        } => {
            object.insert(
                "message".to_string(),
                serde_json::to_value(message).unwrap_or_default(),
            );
            object.insert(
                "toolResults".to_string(),
                serde_json::to_value(tool_results).unwrap_or_default(),
            );
        }
        AgentEvent::MessageStart { message } | AgentEvent::MessageEnd { message } => {
            object.insert(
                "message".to_string(),
                serde_json::to_value(message).unwrap_or_default(),
            );
        }
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => {
            // `message_update` flattens the assistant message event's own
            // fields (pi forwards the update object verbatim).
            if let Ok(serde_json::Value::Object(fields)) =
                serde_json::to_value(assistant_message_event)
            {
                for (key, value) in fields {
                    object.insert(key, value);
                }
            }
        }
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
            parent_tool_call_id,
        } => {
            insert_tool_identity(
                &mut object,
                tool_call_id,
                tool_name,
                args,
                parent_tool_call_id.as_deref(),
            );
        }
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            partial_result,
            parent_tool_call_id,
        } => {
            insert_tool_identity(
                &mut object,
                tool_call_id,
                tool_name,
                args,
                parent_tool_call_id.as_deref(),
            );
            object.insert(
                "partialResult".to_string(),
                serde_json::to_value(partial_result).unwrap_or_default(),
            );
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
            parent_tool_call_id,
        } => {
            insert_tool_identity(
                &mut object,
                tool_call_id,
                tool_name,
                &serde_json::json!({})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                parent_tool_call_id.as_deref(),
            );
            object.insert(
                "result".to_string(),
                serde_json::to_value(result).unwrap_or_default(),
            );
            object.insert("isError".to_string(), serde_json::json!(is_error));
        }
    }
    object
}

fn insert_tool_identity(
    object: &mut JsonObject,
    tool_call_id: &str,
    tool_name: &str,
    args: &JsonObject,
    parent_tool_call_id: Option<&str>,
) {
    object.insert("toolCallId".to_string(), serde_json::json!(tool_call_id));
    object.insert("toolName".to_string(), serde_json::json!(tool_name));
    object.insert("args".to_string(), serde_json::Value::Object(args.clone()));
    match parent_tool_call_id {
        // pi omits the key entirely for the loop's own calls.
        None => {}
        Some(id) => {
            object.insert("parentToolCallId".to_string(), serde_json::json!(id));
        }
    }
}

/// Sink writing one JSON line per event (`json-event.ts:20-40`).
pub struct JsonEventSink<W: Write> {
    writer: BufWriter<W>,
}

impl<W: Write> JsonEventSink<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer: BufWriter::new(writer),
        }
    }

    fn write_event(&mut self, event: &AgentEvent) {
        let line = serde_json::Value::Object(event_to_json(event)).to_string();
        if writeln!(self.writer, "{line}").is_err() {
            // stdout closed: upstream's output guard (P7) aborts the run; the
            // port's signal handling lands with it.
        }
    }

    fn flush(&mut self) {
        let _ = self.writer.flush();
    }
}

impl<W: Write> AgentEventSink for JsonEventSink<W> {
    fn emit(&mut self, event: AgentEvent) -> pi_agent::types::BoxFuture<'_, ()> {
        self.write_event(&event);
        Box::pin(async {})
    }
}

/// `runJsonMode` on the print runtime's seams: same startup as
/// [`crate::modes::print_mode::run_print_mode`], JSONL output. Returns the
/// process exit code with the same contract as text mode: 1 when the last
/// assistant message errored or was aborted, else 0.
pub fn run_json_mode(
    runtime: &mut PrintRuntime,
    options: &PrintModeOptions,
) -> Result<i32, String> {
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

    let session_model = session_context.model.clone();
    let model_changed = session_model.as_ref().is_none_or(|reference| {
        reference.provider != runtime.model.provider || reference.model_id != runtime.model.id
    });
    if model_changed {
        runtime
            .session
            .append_model_change(&runtime.model.provider, &runtime.model.id)
            .map_err(|error| error.to_string())?;
    }
    let requested_level = runtime.thinking_level.as_str().to_string();
    if session_context.thinking_level != requested_level {
        runtime
            .session
            .append_thinking_level_change(&requested_level)
            .map_err(|error| error.to_string())?;
    }

    let mut prompts: Vec<AgentMessage> = Vec::new();
    let now = now_millis();
    if let Some(initial) = &options.initial_message {
        prompts.push(pi_agent::transcript::user_message(initial, now));
    }
    for message in &options.messages {
        prompts.push(pi_agent::transcript::user_message(message, now));
    }

    let mut queues = Queues::new();
    let mut sink = JsonEventSink::new(io::stdout().lock());
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
        prompts,
        context,
        executor.as_ref(),
        &config,
        &mut queues,
        &mut sink,
        &signal,
        runtime.stream_fn.as_ref(),
    ));
    sink.flush();

    for message in &new_messages {
        runtime
            .session
            .append_message(message.clone())
            .map_err(|error| error.to_string())?;
    }

    let exit_code = match new_messages.iter().rev().find_map(|message| match message {
        AgentMessage::Assistant(assistant) => Some(assistant.stop_reason),
        _ => None,
    }) {
        Some(StopReason::Error | StopReason::Aborted) => 1,
        _ => 0,
    };
    Ok(exit_code)
}
