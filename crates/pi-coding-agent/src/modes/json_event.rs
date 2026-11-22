//! JSON mode: run the agent loop and emit every event as a JSON line on
//! stdout.
//!
//! Port of `modes/json-event.ts` (61 lines) plus print-mode.ts's JSON
//! prelude (`:129-134`) on top of the same runtime resolution as print
//! mode. Each line is one event serialized with pi's wire shape
//! (`snake_case` `type` discriminant, `camelCase` payload; the
//! `message_update` line carries `usage` plus the `partial`-less assistant
//! event, exactly `json-event.ts`). Remaining deviation: upstream
//! subscribes mid-loop (events stream as they happen); the port emits
//! through a sink that writes synchronously through
//! [`crate::output_guard`] (the `core/output-guard.ts` port):
//! `writeRawStdout` per line (`print-mode.ts:117`, `:132`) and
//! `flushRawStdout` at the tail (`:180`) — the same visible byte stream
//! for a non-aborted run.

use std::sync::Arc;

use pi_agent::agent_loop::AgentLoopConfig;
use pi_agent::agent_message::AgentMessage;
use pi_agent::events::{AgentEvent, AgentEventSink};
use pi_agent::queues::Queues;
use pi_agent::transcript::tool_declaration;
use pi_agent::types::{AbortSignal, AgentContext, ToolExecutionMode, ToolSpec};
use pi_ai::types::{JsonObject, StopReason};

use crate::modes::print_mode::{PrintModeOptions, PrintRuntime, now_millis};
use crate::session::store::build_session_context_from;

/// Serializes one [`AgentEvent`] into its wire object: `type` plus the
/// payload fields, `camelCase` like pi's JSON protocol.
// Upstream `json-event.ts` serializes the same union; this is the explicit
// shape-by-shape port of it.
#[allow(clippy::too_many_lines)] // mirrors the TS union; split when extending
pub fn event_to_json(event: &AgentEvent) -> JsonObject {
    let mut object = JsonObject::new();
    object.insert("type".to_string(), serde_json::json!(event.kind()));
    match event {
        AgentEvent::AgentStart | AgentEvent::TurnStart => {}
        AgentEvent::AgentEnd {
            messages,
            will_retry,
        } => {
            object.insert(
                "messages".to_string(),
                serde_json::to_value(messages).unwrap_or_default(),
            );
            object.insert("willRetry".to_string(), serde_json::json!(will_retry));
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
            message,
            assistant_message_event,
        } => {
            // `message_update` does not forward the event verbatim: the wire
            // carries `usage` from the in-flight message plus the assistant
            // event with the cumulative `partial` snapshot removed
            // (`json-event.ts:46-61`); `toolcall_start` additionally carries
            // the started call's `id`/`toolName` so clients can react without
            // the snapshot (`json-event.ts:23-30`).
            object.insert(
                "usage".to_string(),
                serde_json::to_value(message.usage).unwrap_or_default(),
            );
            object.insert(
                "assistantMessageEvent".to_string(),
                json_assistant_message_event(assistant_message_event),
            );
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
            duration_ms,
            parent_tool_call_id,
        } => {
            // `tool_execution_end` carries no `args` (`types.ts:543-551`).
            object.insert("toolCallId".to_string(), serde_json::json!(tool_call_id));
            object.insert("toolName".to_string(), serde_json::json!(tool_name));
            object.insert(
                "result".to_string(),
                serde_json::to_value(result).unwrap_or_default(),
            );
            object.insert("isError".to_string(), serde_json::json!(is_error));
            if let Some(duration_ms) = duration_ms {
                object.insert("durationMs".to_string(), serde_json::json!(duration_ms));
            }
            if let Some(id) = parent_tool_call_id.as_deref() {
                object.insert("parentToolCallId".to_string(), serde_json::json!(id));
            }
        }
        AgentEvent::AutoRetryStart {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
        } => {
            object.insert("attempt".to_string(), serde_json::json!(attempt));
            object.insert("maxAttempts".to_string(), serde_json::json!(max_attempts));
            object.insert("delayMs".to_string(), serde_json::json!(delay_ms));
            object.insert("errorMessage".to_string(), serde_json::json!(error_message));
        }
        AgentEvent::AutoRetryEnd {
            success,
            attempt,
            final_error,
        } => {
            object.insert("success".to_string(), serde_json::json!(success));
            object.insert("attempt".to_string(), serde_json::json!(attempt));
            if let Some(final_error) = final_error {
                object.insert("finalError".to_string(), serde_json::json!(final_error));
            }
        }
        AgentEvent::CompactionStart { reason } => {
            object.insert("reason".to_string(), serde_json::json!(reason.as_str()));
        }
        AgentEvent::CompactionEnd {
            reason,
            result,
            aborted,
            will_retry,
            error_message,
        } => {
            object.insert("reason".to_string(), serde_json::json!(reason.as_str()));
            if let Some(result) = result {
                object.insert(
                    "result".to_string(),
                    serde_json::to_value(result).unwrap_or_default(),
                );
            }
            object.insert("aborted".to_string(), serde_json::json!(aborted));
            object.insert("willRetry".to_string(), serde_json::json!(will_retry));
            if let Some(error_message) = error_message {
                object.insert("errorMessage".to_string(), serde_json::json!(error_message));
            }
        }
        AgentEvent::EntryAppended { entry } => {
            object.insert("entry".to_string(), entry.clone());
        }
        AgentEvent::AgentSettled { aborted } => {
            object.insert("aborted".to_string(), serde_json::json!(aborted));
        }
        AgentEvent::QueueUpdate {
            steering,
            follow_up,
        } => {
            object.insert("steering".to_string(), serde_json::json!(steering));
            object.insert("followUp".to_string(), serde_json::json!(follow_up));
        }
        AgentEvent::ThinkingLevelChanged { level } => {
            object.insert("level".to_string(), serde_json::json!(level));
        }
        AgentEvent::SessionInfoChanged { name } => {
            if let Some(name) = name {
                object.insert("name".to_string(), serde_json::json!(name));
            }
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

/// `toJsonAssistantMessageEvent` (`json-event.ts:20-38`): the streaming event
/// without its cumulative `partial` snapshot; `toolcall_start` gains the
/// started call's `id`/`toolName` in their place.
fn json_assistant_message_event(event: &pi_ai::types::AssistantMessageEvent) -> serde_json::Value {
    let mut value = serde_json::to_value(event).unwrap_or_default();
    if let serde_json::Value::Object(fields) = &mut value {
        let partial = fields.remove("partial");
        if let pi_ai::types::AssistantMessageEvent::ToolcallStart { content_index, .. } = event {
            // Upstream throws when the content at the index is not a tool
            // call (`json-event.ts:25-27`); the loop's own invariant makes
            // the missing-call case unreachable, so the port leaves the ids
            // out rather than panic.
            let call = partial
                .as_ref()
                .and_then(|partial| partial.get("content"))
                .and_then(|content| content.get(*content_index))
                .filter(|block| {
                    block.get("type").and_then(serde_json::Value::as_str) == Some("toolCall")
                });
            if let Some(call) = call {
                if let Some(id) = call.get("id") {
                    fields.insert("id".to_string(), id.clone());
                }
                if let Some(name) = call.get("name") {
                    fields.insert("toolName".to_string(), name.clone());
                }
            }
        }
    }
    value
}

/// Sink writing one JSON line per event (`json-event.ts:20-40`): every
/// line rides the stdout guard, like upstream's `writeRawStdout` call
/// sites (`print-mode.ts:117`).
pub struct JsonEventSink;

impl JsonEventSink {
    fn write_event(event: &AgentEvent) {
        let line = serde_json::Value::Object(event_to_json(event)).to_string();
        // `writeRawStdout(`${JSON.stringify(toJsonEvent(event))}\n`)`
        // (`print-mode.ts:117`); a failed write exits inside the guard
        // (`output-guard.ts:90-92`).
        crate::output_guard::write_raw_stdout(&format!("{line}\n"));
    }

    /// Writes one pre-serialized value as a JSON line. JSON mode's prelude is
    /// the session header (`print-mode.ts:129-134`).
    fn write_value(value: &serde_json::Value) {
        crate::output_guard::write_raw_stdout(&format!("{value}\n"));
    }
}

impl AgentEventSink for JsonEventSink {
    fn emit(&mut self, event: AgentEvent) -> pi_agent::types::BoxFuture<'_, ()> {
        Self::write_event(&event);
        Box::pin(async {})
    }
}

/// `runJsonMode` on the print runtime's seams: same startup as
/// [`crate::modes::print_mode::run_print_mode`], JSONL output. Returns the
/// process exit code with the same contract as text mode: 1 when the last
/// assistant message errored or was aborted, else 0.
#[allow(clippy::too_many_lines)] // mirrors print-mode.ts's run loop; split when extending
pub fn run_json_mode(
    runtime: &mut PrintRuntime,
    options: &PrintModeOptions,
    block_images: bool,
) -> Result<i32, String> {
    let executor = runtime.executor.clone();
    let specs: Vec<ToolSpec> = executor.tools();

    let leaf_id = runtime.session.get_leaf_id().map(str::to_string);
    let session_context =
        build_session_context_from(&runtime.session.get_entries(), leaf_id.as_deref());
    let mut context_messages = session_context.messages;
    if context_messages.is_empty()
        && let Some(system) = pi_agent::transcript::create_initial_system_message(
            Some(&runtime.system_prompt),
            &specs.iter().map(tool_declaration).collect::<Vec<_>>(),
        )
    {
        context_messages.push(AgentMessage::System(system));
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
        if options.initial_images.is_empty() {
            prompts.push(pi_agent::transcript::user_message(initial, now));
        } else {
            prompts.push(pi_agent::transcript::user_message_with_images(
                initial,
                &options.initial_images,
                now,
            ));
        }
    }
    for message in &options.messages {
        prompts.push(pi_agent::transcript::user_message(message, now));
    }

    let mut queues = Queues::new();
    let mut sink = JsonEventSink;
    // The session header prelude, before any event (`print-mode.ts:129-134`).
    if let Some(header) = runtime.session.get_header()
        && let Ok(header) = serde_json::to_value(&header)
    {
        JsonEventSink::write_value(&header);
    }
    let signal = Arc::new(AbortSignal::new());
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
        // Same `blockImages` convertToLlm seam as print/RPC (sdk.ts:296-331).
        convert_to_llm: crate::images::block_images_converter(block_images),
    };

    #[cfg(feature = "ext-js")]
    let loop_stream_fn: Arc<dyn pi_agent::types::StreamFn> = match &routing {
        Some(routing) => routing.stream_fn(),
        None => Arc::clone(&runtime.stream_fn),
    };
    #[cfg(not(feature = "ext-js"))]
    let loop_stream_fn: Arc<dyn pi_agent::types::StreamFn> = Arc::clone(&runtime.stream_fn);
    // The auto-retry machine (`agent-session.ts` `_handlePostAgentRun`),
    // emitting `auto_retry_start`/`auto_retry_end` lines like upstream's
    // event stream (`agent-session.ts:235-237`).
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
        let messages = {
            // `agent_end` carries the merged `willRetry`
            // (`agent-session.ts:1211`).
            let mut aware = crate::modes::retry::RetryAwareSink {
                inner: &mut sink,
                retry_attempt: retry.attempt(),
                settings: runtime.retry_settings,
                signal: Arc::clone(&signal),
            };
            crate::runtime::block_on(pi_agent::run_agent_loop(
                prompts.clone(),
                context.clone(),
                executor.as_ref(),
                &config,
                &mut queues,
                &mut aware,
                &signal,
                loop_stream_fn.as_ref(),
            ))
        };
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
    // `agent_settled` closes the run (`agent-session.ts:1141-1152`): the
    // run's abort request decides `aborted`.
    crate::runtime::block_on(sink.emit(pi_agent::events::AgentEvent::AgentSettled {
        aborted: signal.aborted(),
    }));
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

    let exit_code = match new_messages.iter().rev().find_map(|message| match message {
        AgentMessage::Assistant(assistant) => Some(assistant.stop_reason),
        _ => None,
    }) {
        Some(StopReason::Error | StopReason::Aborted) => 1,
        _ => 0,
    };
    // The `finally { await flushRawStdout(); }` tail (`print-mode.ts:180`):
    // a flush error overrides the outcome, like a rejected finally.
    crate::output_guard::flush_raw_stdout().map_err(|error| error.to_string())?;
    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JSON mode installs the same shared `block_images_converter` seam as
    /// print/RPC; this guards its enabled/disabled behavior at the mode level.
    #[test]
    fn json_convert_to_llm_honors_block_images_snapshot() {
        use pi_ai::types::{ImageContent, InputContent, TextContent, UserContent, UserMessage};
        let image = InputContent::Image(ImageContent {
            data: "abc".to_string(),
            mime_type: "image/png".to_string(),
        });
        let text = InputContent::Text(TextContent {
            text: "hello".to_string(),
            text_signature: None,
        });
        let message = AgentMessage::User(UserMessage {
            content: UserContent::Blocks(vec![image.clone(), image, text]),
            timestamp: 1,
        });

        let blocked = crate::runtime::block_on(
            crate::images::block_images_converter(true).call(vec![message.clone()]),
        );
        let AgentMessage::User(user) = &blocked[0] else {
            panic!("user message stays a user message");
        };
        let UserContent::Blocks(blocks) = &user.content else {
            panic!("blocks stay blocks");
        };
        assert_eq!(blocks.len(), 2, "consecutive placeholders are deduped");
        assert_eq!(
            blocks[0].as_text(),
            Some(crate::images::IMAGE_READING_DISABLED)
        );
        assert_eq!(blocks[1].as_text(), Some("hello"));

        let passed = crate::runtime::block_on(
            crate::images::block_images_converter(false).call(vec![message.clone()]),
        );
        assert_eq!(passed, vec![message]);
    }
}
