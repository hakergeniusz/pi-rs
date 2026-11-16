//! The agent loop.
//!
//! Ground truth: pi-mono `packages/agent/src/agent-loop.ts` (940 lines). The
//! control flow is ported statement by statement, including the parts that look
//! redundant:
//!
//! * the outer loop exists only for follow-up messages that arrive after the
//!   agent would otherwise stop;
//! * `explicit_continuation` fulfils a `finishTurn: continue` decision with one
//!   context-only turn, and is cleared again as soon as anything else supplies a
//!   reason to run;
//! * `declareToolChanges` runs before *every* request, not once per run;
//! * a `length` stop fails the whole tool batch instead of executing calls
//!   whose arguments may be truncated.
//!
//! One intentional difference: pi's stream function returns a stream plus a
//! `result()` promise that resolves the final message. Here the terminal
//! `done`/`error` event carries the final message, so the loop reads it from
//! there; a stream that ends without a terminal event is reported as an error
//! message rather than silently reusing the last partial.

use std::future::Future;
use std::sync::Arc;

use crate::agent_message::{AgentMessage, BashExecutionMessage};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use pi_ai::types::AssistantMessageEvent;
use pi_ai::types::Model;
use pi_ai::types::{AssistantMessage, ContentBlock, ToolCall, Usage, UserMessage};
use pi_ai::types::{JsonObject, StopReason, UserContent};
use serde_json::Value;

use crate::events::{AgentEvent, AgentEventSink};
use crate::transcript;
use crate::types::{
    AbortSignal, AfterToolCallOverride, AgentContext, AgentToolCall, AgentToolResult,
    AgentTurnContext, BoxFuture, PrepareNextTurnContext, PrepareRequestContext, StreamFn,
    StreamRequest, ThinkingLevel, ToolCallHooks, ToolExecutionMode, ToolExecutor, TurnDecision,
    TurnUpdate,
};

/// Hooks the host can install around the loop (`AgentLoopConfig` in `types.ts`).
#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    /// The live cells pi's session hands the loop on every turn
    /// (`agent-session.ts:899-900`, applied at `agent-loop.ts:186-199`).
    /// `Some` means a caller outside the agent may write the level (or switch
    /// the model) while this run is in flight and the **next** provider
    /// request of the same run reads it; `None` freezes the snapshot above,
    /// which is what a host gets when it drives the loop directly.
    pub live_settings: Option<crate::live_settings::SettingsHandle>,
    pub session_id: Option<String>,
    /// Wire credential, resolved through pi's auth ladder.
    pub api_key: Option<String>,
    /// OAuth bearer credential (pi's `accessToken`): takes precedence over
    /// `api_key` in the providers that support it.
    pub access_token: Option<String>,
    pub tool_execution: ToolExecutionMode,
    pub hooks: ToolCallHooks,
    pub finish_turn: Option<Arc<dyn FinishTurn>>,
    pub prepare_request: Option<Arc<dyn PrepareRequest>>,
    pub prepare_next_turn: Option<Arc<dyn PrepareNextTurn>>,
    pub transform_context: Option<Arc<dyn TransformContext>>,
    /// pi's `Agent.convertToLlm` (`agent.ts:194`, `agent.ts:234`): the
    /// conversion boundary from session messages to LLM messages. Defaults to
    /// [`DefaultConvertToLlm`] (pi's `defaultConvertToLlm`); a host installs
    /// its own to handle custom message variants.
    pub convert_to_llm: Arc<dyn ConvertToLlm>,
}

impl Default for AgentLoopConfig {
    /// The model is left empty; hosts always set it from `Agent.state.model`.
    fn default() -> Self {
        AgentLoopConfig {
            model: Model::default(),
            thinking_level: ThinkingLevel::Off,
            live_settings: None,
            session_id: None,
            api_key: None,
            access_token: None,
            tool_execution: ToolExecutionMode::Parallel,
            hooks: ToolCallHooks::default(),
            finish_turn: None,
            prepare_request: None,
            prepare_next_turn: None,
            transform_context: None,
            convert_to_llm: DEFAULT_CONVERT_TO_LLM.clone(),
        }
    }
}

impl std::fmt::Debug for AgentLoopConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `api_key` is deliberately rendered as a presence flag, never a value.
        f.debug_struct("AgentLoopConfig")
            .field("model", &self.model.id)
            .field("thinking_level", &self.thinking_level)
            .field("live_settings", &self.live_settings.is_some())
            .field("session_id", &self.session_id)
            .field("api_key_set", &self.api_key.is_some())
            .field("access_token_set", &self.access_token.is_some())
            .field("tool_execution", &self.tool_execution)
            .field("hooks", &self.hooks)
            .field("finish_turn", &self.finish_turn.is_some())
            .field("prepare_request", &self.prepare_request.is_some())
            .field("prepare_next_turn", &self.prepare_next_turn.is_some())
            .field("transform_context", &self.transform_context.is_some())
            .field(
                "convert_to_llm_default",
                &std::ptr::eq(
                    Arc::as_ptr(&self.convert_to_llm),
                    Arc::as_ptr(&DEFAULT_CONVERT_TO_LLM),
                ),
            )
            .finish()
    }
}

/// The shared default converter instance, so `Debug` can recognize it.
static DEFAULT_CONVERT_TO_LLM: std::sync::LazyLock<Arc<dyn ConvertToLlm>> =
    std::sync::LazyLock::new(|| Arc::new(DefaultConvertToLlm));

/// `FinishTurn` — called after a turn's messages, before `turn_end`.
pub trait FinishTurn: Send + Sync {
    fn call<'a>(&'a self, turn: &'a AgentTurnContext) -> BoxFuture<'a, Option<TurnDecision>>;
}

/// `PrepareRequest` — called before every conversational request.
///
/// pi passes one argument, `PrepareRequestContext` (`types.ts:186-190`), and a
/// signal beside it. pi hands over the loop's live `currentContext`, so an
/// in-place edit of `request.context.messages` reaches the request; the port
/// borrows the same context for the duration of the call.
pub trait PrepareRequest: Send + Sync {
    fn call<'a>(&'a self, request: PrepareRequestContext<'a>) -> BoxFuture<'a, TurnUpdate>;
}

/// `PrepareNextTurn` — called after `turn_end` when the loop will continue.
pub trait PrepareNextTurn: Send + Sync {
    fn call<'a>(&'a self, context: &'a PrepareNextTurnContext)
    -> BoxFuture<'a, Option<TurnUpdate>>;
}

/// `transformContext` — `AgentMessage[]` to `AgentMessage[]` before conversion.
pub trait TransformContext: Send + Sync {
    fn call(&self, messages: Vec<AgentMessage>) -> BoxFuture<'_, Vec<AgentMessage>>;
}

/// `convertToLlm` — the session-to-LLM conversion boundary. pi exposes it as
/// a public mutable field on `Agent` that defaults to `defaultConvertToLlm`
/// (`agent.ts:194`), so a host can swap in a converter that understands its
/// own message variants; the loop calls it before every request.
///
/// pi's signature is `(messages: AgentMessage[]) => Message[] | Promise<Message[]>`
/// (`types.ts:222`) and the loop **awaits** it (`agent-loop.ts:395`), so this
/// trait returns a [`BoxFuture`] rather than the messages directly. The future
/// is boxed because the seam lives behind `Arc<dyn ConvertToLlm>` and a plain
/// `async fn` in a trait is not object-safe; the shape matches the crate's
/// other async seams ([`TransformContext`], [`PrepareRequest`]). Like pi's,
/// the call takes no abort signal.
pub trait ConvertToLlm: Send + Sync {
    fn call(&self, messages: Vec<AgentMessage>) -> BoxFuture<'_, Vec<AgentMessage>>;
}

/// The default [`ConvertToLlm`]: [`default_convert_to_llm`].
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultConvertToLlm;

impl ConvertToLlm for DefaultConvertToLlm {
    fn call(&self, messages: Vec<AgentMessage>) -> BoxFuture<'_, Vec<AgentMessage>> {
        Box::pin(std::future::ready(default_convert_to_llm(messages)))
    }
}

/// The sync convenience impl, unchanged in shape from before the seam became
/// async: any `Fn(Vec<AgentMessage>) -> Vec<AgentMessage>` is a
/// [`ConvertToLlm`], and the loop awaits the already-computed value. This is
/// pi's *first* arm of `Message[] | Promise<Message[]>` — the common path.
impl<F> ConvertToLlm for F
where
    F: Fn(Vec<AgentMessage>) -> Vec<AgentMessage> + Send + Sync,
{
    fn call(&self, messages: Vec<AgentMessage>) -> BoxFuture<'_, Vec<AgentMessage>> {
        Box::pin(std::future::ready(self(messages)))
    }
}

/// The async convenience adapter: wraps a closure that returns a future, so a
/// converter that genuinely awaits (a fetch, a timer) can be installed without
/// hand-writing the boxed future. This is pi's *second* arm of
/// `Message[] | Promise<Message[]>`.
///
/// ```
/// # use std::sync::Arc;
/// # use pi_agent::AsyncConvertToLlm;
/// # use pi_agent::AgentMessage;
/// let converter = AsyncConvertToLlm(|messages: Vec<AgentMessage>| async move {
///     messages
/// });
/// let _seam: Arc<dyn pi_agent::ConvertToLlm> = Arc::new(converter);
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct AsyncConvertToLlm<F>(pub F);

impl<F, Fut> ConvertToLlm for AsyncConvertToLlm<F>
where
    F: Fn(Vec<AgentMessage>) -> Fut + Send + Sync,
    Fut: Future<Output = Vec<AgentMessage>> + Send + 'static,
{
    fn call(&self, messages: Vec<AgentMessage>) -> BoxFuture<'_, Vec<AgentMessage>> {
        Box::pin(async move { (self.0)(messages).await })
    }
}

/// pi-mono `core/messages.ts` `COMPACTION_SUMMARY_PREFIX` / `_SUFFIX`.
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
/// pi-mono `core/messages.ts` `COMPACTION_SUMMARY_SUFFIX`.
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
/// pi-mono `core/messages.ts` `BRANCH_SUMMARY_PREFIX`.
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
/// pi-mono `core/messages.ts` `BRANCH_SUMMARY_SUFFIX`.
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// pi's `defaultConvertToLlm` as pi-mono spells it (`convertToLlm` in
/// `coding-agent/src/core/messages.ts`): the four LLM roles pass through, and
/// the session-derived roles become user messages.
///
/// Before phase 3f this dropped everything that was not one of the four LLM
/// roles, which silently deleted a compaction summary from the transcript pi
/// rebuilds after a compaction — the conversation could not continue from it.
/// Roles pi does not model at all stay dropped, as pi's `default` branch does.
pub fn default_convert_to_llm(messages: Vec<AgentMessage>) -> Vec<AgentMessage> {
    messages
        .into_iter()
        .filter_map(|message| match message {
            AgentMessage::BashExecution(bash) => {
                // `!!` marks a command excluded from context.
                if bash.exclude_from_context.unwrap_or(false) {
                    return None;
                }
                Some(user_text_message(
                    &bash_execution_to_text(&bash),
                    bash.timestamp,
                ))
            }
            AgentMessage::Custom(custom) => Some(AgentMessage::User(UserMessage {
                content: custom.content,
                timestamp: custom.timestamp,
            })),
            AgentMessage::BranchSummary(summary) => Some(user_text_message(
                &format!(
                    "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
                    summary.summary
                ),
                summary.timestamp,
            )),
            AgentMessage::CompactionSummary(summary) => Some(user_text_message(
                &format!(
                    "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                    summary.summary
                ),
                summary.timestamp,
            )),
            // Roles pi does not model are not LLM messages.
            AgentMessage::Unknown { .. } => None,
            other => Some(other),
        })
        .collect()
}

/// pi's `bashExecutionToText`.
fn bash_execution_to_text(message: &BashExecutionMessage) -> String {
    let mut text = format!("Ran `{}`\n", message.command);
    if message.output.is_empty() {
        text.push_str("(no output)");
    } else {
        text.push_str("```\n");
        text.push_str(&message.output);
        text.push_str("\n```");
    }
    if message.cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if message.exit_code.unwrap_or(0) != 0 {
        text.push_str("\n\nCommand exited with code ");
        text.push_str(&message.exit_code.unwrap_or(0).to_string());
    }
    if message.truncated {
        if let Some(path) = &message.full_output_path {
            text.push_str("\n\n[Output truncated. Full output: ");
            text.push_str(path);
            text.push(']');
        }
    }
    text
}

fn user_text_message(text: &str, timestamp: i64) -> AgentMessage {
    AgentMessage::User(UserMessage {
        content: UserContent::Blocks(vec![pi_ai::types::InputContent::Text(
            pi_ai::types::TextContent {
                text: text.to_string(),
                text_signature: None,
            },
        )]),
        timestamp,
    })
}

/// Start a run with new prompt messages. `agentLoop` in `agent-loop.ts`.
// The parameter list is pi's own; bundling it into a struct would be a
// rewrite of the port, not a fix.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    executor: &dyn ToolExecutor,
    config: &AgentLoopConfig,
    queues: &mut dyn crate::types::QueueSource,
    emit: &mut dyn AgentEventSink,
    signal: &AbortSignal,
    stream_fn: &dyn StreamFn,
) -> Vec<AgentMessage> {
    let initial_messages = transcript::declare_tool_changes(&context, executor, prompts);
    let mut new_messages = initial_messages.clone();
    let mut current_context = AgentContext {
        messages: context.messages.clone(),
        tools: context.tools.clone(),
    };
    current_context
        .messages
        .extend(initial_messages.iter().cloned());

    emit.emit(AgentEvent::AgentStart).await;
    emit.emit(AgentEvent::TurnStart).await;
    for message in &initial_messages {
        emit.emit(AgentEvent::MessageStart {
            message: message.clone(),
        })
        .await;
        emit.emit(AgentEvent::MessageEnd {
            message: message.clone(),
        })
        .await;
    }

    run_loop(
        current_context,
        &mut new_messages,
        executor,
        config,
        queues,
        emit,
        signal,
        stream_fn,
    )
    .await;
    new_messages
}

/// Continue from the current transcript without adding a message.
/// `agentLoopContinue` in `agent-loop.ts`.
pub async fn run_agent_loop_continue(
    context: AgentContext,
    executor: &dyn ToolExecutor,
    config: &AgentLoopConfig,
    queues: &mut dyn crate::types::QueueSource,
    emit: &mut dyn AgentEventSink,
    signal: &AbortSignal,
    stream_fn: &dyn StreamFn,
) -> Result<Vec<AgentMessage>, String> {
    match context.messages.last() {
        None => return Err("Cannot continue: no messages in context".to_string()),
        Some(message) if message.role() == "assistant" => {
            return Err("Cannot continue from message role: assistant".to_string());
        }
        _ => {}
    }

    let mut new_messages = Vec::new();
    let current_context = context;
    emit.emit(AgentEvent::AgentStart).await;
    emit.emit(AgentEvent::TurnStart).await;

    run_loop(
        current_context,
        &mut new_messages,
        executor,
        config,
        queues,
        emit,
        signal,
        stream_fn,
    )
    .await;
    Ok(new_messages)
}

/// The loop shared by `agentLoop` and `agentLoopContinue` (`runLoop`).
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
async fn run_loop(
    initial_context: AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    executor: &dyn ToolExecutor,
    config: &AgentLoopConfig,
    queues: &mut dyn crate::types::QueueSource,
    emit: &mut dyn AgentEventSink,
    signal: &AbortSignal,
    stream_fn: &dyn StreamFn,
) {
    let mut current_context = initial_context;
    let mut model = config.model.clone();
    let mut thinking_level = config.thinking_level;
    let mut last_completed_turn: Option<PrepareNextTurnContext> = None;
    let mut explicit_continuation = false;
    // Steering may already be queued: the user can type while the agent waits.
    let mut pending_messages: Vec<AgentMessage> = queues.drain_steering();

    // Outer loop: continues when follow-up messages arrive after the agent
    // would otherwise stop.
    loop {
        let mut has_more_tool_calls = true;

        // Inner loop: tool results and steering messages.
        while has_more_tool_calls || !pending_messages.is_empty() {
            // pi's session re-reads its state on every turn - `model:
            // this.agent.state.model, thinkingLevel: this.agent.state
            // .thinkingLevel` (`agent-session.ts:899-900`) - and the loop
            // applies that snapshot before the request (`agent-loop.ts:186-199`),
            // so a `/thinking` or `/model` typed mid-run lands on the next
            // provider request of the run already in flight. Here those two
            // cells are the handle the host writes from outside the agent; a
            // hook's own update below still wins, as pi's snapshot does.
            if let Some(live) = config.live_settings.as_ref() {
                thinking_level = live.thinking_level();
                if let Some(next_model) = live.pending_model() {
                    model = next_model;
                }
            }
            let prepared_messages: Vec<AgentMessage> = Vec::new();
            if let Some(previous) = last_completed_turn.take() {
                if let Some(prepare) = config.prepare_next_turn.as_ref() {
                    if let Some(update) = prepare.call(&previous).await {
                        if let Some(next_model) = update.model {
                            model = next_model;
                        }
                        if let Some(level) = update.thinking_level {
                            thinking_level = level;
                        }
                    }
                }
                // Preparation can be long (compaction), so steering queued while
                // it ran is picked up here — but only when the earlier poll
                // found nothing, or `one-at-a-time` would deliver two messages
                // in one turn.
                if pending_messages.is_empty() {
                    pending_messages = queues.drain_steering();
                }
                emit.emit(AgentEvent::TurnStart).await;
            }

            // Process prepared and queued messages before the next response.
            let mut to_declare = prepared_messages.clone();
            to_declare.append(&mut pending_messages);
            // Prepared and queued messages are declared into the transcript
            // before the next request, so the model sees the tool set change.
            for message in transcript::declare_tool_changes(&current_context, executor, to_declare)
            {
                emit.emit(AgentEvent::MessageStart {
                    message: message.clone(),
                })
                .await;
                emit.emit(AgentEvent::MessageEnd {
                    message: message.clone(),
                })
                .await;
                current_context.messages.push(message.clone());
                new_messages.push(message);
            }
            // `pending_messages` is empty again here: the drain above moved it.

            if let Some(prepare) = config.prepare_request.as_ref() {
                // pi: `await config.prepareRequest?.({ context: currentContext,
                // model, thinkingLevel }, signal)` — the same object, so the
                // hook can edit the transcript in place.
                let request = PrepareRequestContext {
                    context: &mut current_context,
                    model: model.clone(),
                    thinking_level,
                    signal,
                };
                let update = prepare.call(request).await;
                // pi: `currentContext = requestUpdate.context ?? currentContext`
                // (`agent-loop.ts:228`) — the returned context wins only when
                // the hook actually returned one; otherwise the (possibly
                // mutated) live context stands.
                current_context = update.context.unwrap_or(current_context);
                if let Some(next_model) = update.model {
                    model = next_model;
                }
                if let Some(level) = update.thinking_level {
                    thinking_level = level;
                }
            }

            let message = stream_assistant_response(
                &mut current_context,
                &model,
                thinking_level,
                executor,
                config,
                emit,
                signal,
                stream_fn,
            )
            .await;
            new_messages.push(message.clone());

            let stop_reason = assistant_stop_reason(&message);
            if matches!(stop_reason, StopReason::Error | StopReason::Aborted) {
                last_completed_turn = Some(PrepareNextTurnContext {
                    turn: AgentTurnContext {
                        message: message.clone(),
                        tool_results: Vec::new(),
                        new_messages: new_messages.clone(),
                    },
                });
                if let Some(finish) = config.finish_turn.as_ref() {
                    finish
                        .call(&last_completed_turn.as_ref().expect("turn").turn)
                        .await;
                }
                emit.emit(AgentEvent::TurnEnd {
                    message: message.clone(),
                    tool_results: Vec::new(),
                })
                .await;
                emit.emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                    will_retry: false,
                })
                .await;
                return;
            }

            let tool_calls = tool_calls_of(&message);
            let mut tool_results: Vec<AgentMessage> = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                // A "length" stop means the output was cut off by the token
                // limit, so every tool call may carry truncated arguments.
                let batch = if stop_reason == StopReason::Length {
                    fail_tool_calls_from_truncated_message(&tool_calls, emit).await
                } else {
                    execute_tool_calls(&message, executor, config, signal, emit).await
                };
                has_more_tool_calls = !batch.terminate;
                for result in batch.messages {
                    current_context.messages.push(result.clone());
                    new_messages.push(result.clone());
                    tool_results.push(result);
                }
            }

            let turn = AgentTurnContext {
                message: message.clone(),
                tool_results: tool_results.clone(),
                new_messages: new_messages.clone(),
            };
            let decision = match config.finish_turn.as_ref() {
                Some(finish) => finish.call(&turn).await,
                None => None,
            };
            emit.emit(AgentEvent::TurnEnd {
                message: message.clone(),
                tool_results,
            })
            .await;

            if decision == Some(TurnDecision::End) {
                emit.emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                    will_retry: false,
                })
                .await;
                return;
            }

            last_completed_turn = Some(PrepareNextTurnContext { turn });
            explicit_continuation = decision == Some(TurnDecision::Continue);
            pending_messages = queues.drain_steering();
            if has_more_tool_calls || !pending_messages.is_empty() {
                explicit_continuation = false;
            }
        }

        // The agent would stop here. Check for follow-up messages.
        let follow_up_messages = queues.drain_follow_up();
        if !follow_up_messages.is_empty() {
            explicit_continuation = false;
            pending_messages = follow_up_messages;
            continue;
        }

        // Nothing natural was selected, so fulfil the continuation decision with
        // one context-only turn.
        if explicit_continuation {
            explicit_continuation = false;
            continue;
        }

        break;
    }

    emit.emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
        will_retry: false,
    })
    .await;
}

/// Stream one assistant response. `streamAssistantResponse` in `agent-loop.ts`.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
async fn stream_assistant_response(
    context: &mut AgentContext,
    model: &Model,
    thinking_level: ThinkingLevel,
    executor: &dyn ToolExecutor,
    config: &AgentLoopConfig,
    emit: &mut dyn AgentEventSink,
    signal: &AbortSignal,
    stream_fn: &dyn StreamFn,
) -> AgentMessage {
    let _ = executor;
    let messages = match config.transform_context.as_ref() {
        Some(transform) => transform.call(context.messages.clone()).await,
        None => context.messages.clone(),
    };
    // `await config.convertToLlm(messages)` (`agent-loop.ts:395`).
    let llm_messages = config.convert_to_llm.call(messages).await;

    let request = StreamRequest {
        model: model.clone(),
        messages: llm_messages,
        thinking_level,
        session_id: config.session_id.clone(),
        api_key: config.api_key.clone(),
        access_token: config.access_token.clone(),
    };

    let mut stream = stream_fn.stream(request);
    let mut partial: Option<AssistantMessage> = None;
    let mut added_partial = false;

    while let Some(event) = stream.next().await {
        if signal.aborted() {
            break;
        }
        match &event {
            AssistantMessageEvent::Start { partial: message } => {
                let message = message.clone();
                context
                    .messages
                    .push(AgentMessage::Assistant(message.clone()));
                added_partial = true;
                partial = Some(message.clone());
                emit.emit(AgentEvent::MessageStart {
                    message: AgentMessage::Assistant(message),
                })
                .await;
            }
            AssistantMessageEvent::TextStart {
                partial: message, ..
            }
            | AssistantMessageEvent::TextDelta {
                partial: message, ..
            }
            | AssistantMessageEvent::TextEnd {
                partial: message, ..
            }
            | AssistantMessageEvent::ThinkingStart {
                partial: message, ..
            }
            | AssistantMessageEvent::ThinkingDelta {
                partial: message, ..
            }
            | AssistantMessageEvent::ThinkingEnd {
                partial: message, ..
            }
            | AssistantMessageEvent::ToolcallStart {
                partial: message, ..
            }
            | AssistantMessageEvent::ToolcallDelta {
                partial: message, ..
            }
            | AssistantMessageEvent::ToolcallEnd {
                partial: message, ..
            } => {
                if partial.is_some() {
                    let message = message.clone();
                    replace_last_assistant(context, &message);
                    partial = Some(message.clone());
                    emit.emit(AgentEvent::MessageUpdate {
                        message: std::sync::Arc::new(message.clone()),
                        assistant_message_event: event.clone(),
                    })
                    .await;
                }
            }
            AssistantMessageEvent::Done { message, .. } => {
                let final_message = message.clone();
                return finish_assistant(context, emit, final_message, added_partial).await;
            }
            AssistantMessageEvent::Error { error, .. } => {
                let final_message = error.clone();
                return finish_assistant(context, emit, final_message, added_partial).await;
            }
        }
    }

    // The stream ended without a terminal event. pi's stream function always
    // emits one; when it does not, report it instead of reusing the partial.
    let final_message = match partial {
        Some(mut message) => {
            message.stop_reason = StopReason::Error;
            message.error_message = Some("stream ended without a terminal event".to_string());
            message
        }
        None => AssistantMessage {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error_message: Some("stream produced no events".to_string()),
            timestamp: now_millis(),
            ..empty_assistant_message()
        },
    };
    finish_assistant(context, emit, final_message, added_partial).await
}

async fn finish_assistant(
    context: &mut AgentContext,
    emit: &mut dyn AgentEventSink,
    final_message: AssistantMessage,
    added_partial: bool,
) -> AgentMessage {
    if added_partial {
        replace_last_assistant(context, &final_message);
    } else {
        context
            .messages
            .push(AgentMessage::Assistant(final_message.clone()));
        emit.emit(AgentEvent::MessageStart {
            message: AgentMessage::Assistant(final_message.clone()),
        })
        .await;
    }
    emit.emit(AgentEvent::MessageEnd {
        message: AgentMessage::Assistant(final_message.clone()),
    })
    .await;
    AgentMessage::Assistant(final_message)
}

fn replace_last_assistant(context: &mut AgentContext, message: &AssistantMessage) {
    if let Some(last) = context.messages.last_mut() {
        if matches!(last, AgentMessage::Assistant(_)) {
            *last = AgentMessage::Assistant(message.clone());
        }
    }
}

/// A blank assistant message: empty content, empty api/provider/model fields.
pub fn empty_assistant_message() -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: String::new(),
        provider: String::new(),
        model: String::new(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        thinking_level: None,
        diagnostics: vec![],
        usage: Usage::default(),
        stop_reason: StopReason::Pending,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
        duration_ms: None,
    }
}

fn assistant_stop_reason(message: &AgentMessage) -> StopReason {
    match message {
        AgentMessage::Assistant(assistant) => assistant.stop_reason,
        _ => StopReason::Error,
    }
}

fn tool_calls_of(message: &AgentMessage) -> Vec<AgentToolCall> {
    match message {
        AgentMessage::Assistant(assistant) => assistant
            .content
            .iter()
            .filter_map(|block| match block {
                pi_ai::types::AssistantContent::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

struct ExecutedToolCallBatch {
    messages: Vec<AgentMessage>,
    terminate: bool,
}

/// A truncated response can yield tool calls whose arguments parse but are
/// silently incomplete. None of them are safe to run, so each is reported as an
/// error and the model can re-issue it.
async fn fail_tool_calls_from_truncated_message(
    tool_calls: &[AgentToolCall],
    emit: &mut dyn AgentEventSink,
) -> ExecutedToolCallBatch {
    let mut messages = Vec::new();
    for tool_call in tool_calls {
        emit.emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
            parent_tool_call_id: None,
        })
        .await;
        let finalized = FinalizedToolCall {
            tool_call: tool_call.clone(),
            result: AgentToolResult::error(format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                tool_call.name
            )),
            is_error: true,
            duration_ms: None,
        };
        emit_tool_execution_end(&finalized, emit).await;
        let message = create_tool_result_message(&finalized);
        emit_tool_result_message(&message, emit).await;
        messages.push(message);
    }
    ExecutedToolCallBatch {
        messages,
        terminate: false,
    }
}

async fn execute_tool_calls(
    assistant_message: &AgentMessage,
    executor: &dyn ToolExecutor,
    config: &AgentLoopConfig,
    signal: &AbortSignal,
    emit: &mut dyn AgentEventSink,
) -> ExecutedToolCallBatch {
    let tool_calls = tool_calls_of(assistant_message);
    let has_sequential = tool_calls
        .iter()
        .any(|call| executor.execution_mode(&call.name) == Some(ToolExecutionMode::Sequential));
    if config.tool_execution == ToolExecutionMode::Sequential || has_sequential {
        execute_tool_calls_sequential(
            assistant_message,
            &tool_calls,
            executor,
            config,
            signal,
            emit,
        )
        .await
    } else {
        execute_tool_calls_parallel(
            assistant_message,
            &tool_calls,
            executor,
            config,
            signal,
            emit,
        )
        .await
    }
}

struct FinalizedToolCall {
    tool_call: AgentToolCall,
    result: AgentToolResult,
    is_error: bool,
    duration_ms: Option<u64>,
}

fn assistant_of(message: &AgentMessage) -> AssistantMessage {
    match message {
        AgentMessage::Assistant(assistant) => assistant.clone(),
        _ => empty_assistant_message(),
    }
}

async fn execute_tool_calls_sequential(
    assistant_message: &AgentMessage,
    tool_calls: &[AgentToolCall],
    executor: &dyn ToolExecutor,
    config: &AgentLoopConfig,
    signal: &AbortSignal,
    emit: &mut dyn AgentEventSink,
) -> ExecutedToolCallBatch {
    let mut finalized_calls: Vec<FinalizedToolCall> = Vec::new();
    let mut messages = Vec::new();
    let assistant = assistant_of(assistant_message);

    for tool_call in tool_calls {
        emit.emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
            parent_tool_call_id: None,
        })
        .await;

        let prepared =
            prepare_tool_call(&assistant, tool_call, executor, &config.hooks, signal).await;
        #[allow(clippy::single_match_else)] // mirrors pi's if (prepared) / else
        let (finalized, updates): (FinalizedToolCall, Vec<AgentToolResult>) = match prepared {
            Some((result, is_error)) => (
                FinalizedToolCall {
                    tool_call: tool_call.clone(),
                    result,
                    is_error,
                    duration_ms: None,
                },
                Vec::new(),
            ),
            None => {
                let (result, is_error, updates) =
                    execute_prepared_tool_call(tool_call, executor, signal).await;
                let finalized =
                    finalize_executed_tool_call(&assistant, tool_call, result, is_error, config)
                        .await;
                (finalized, updates)
            }
        };
        for update in updates {
            emit_tool_execution_update(tool_call, update, emit).await;
        }
        emit_tool_execution_end(&finalized, emit).await;
        let message = create_tool_result_message(&finalized);
        emit_tool_result_message(&message, emit).await;
        finalized_calls.push(finalized);
        messages.push(message);

        if signal.aborted() {
            break;
        }
    }

    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized_calls),
    }
}

async fn execute_tool_calls_parallel(
    assistant_message: &AgentMessage,
    tool_calls: &[AgentToolCall],
    executor: &dyn ToolExecutor,
    config: &AgentLoopConfig,
    signal: &AbortSignal,
    emit: &mut dyn AgentEventSink,
) -> ExecutedToolCallBatch {
    let assistant = assistant_of(assistant_message);
    // Preflight is sequential: every `tool_execution_start` is emitted, and
    // every tool prepared, before any tool starts running. A call resolved
    // during preflight reports its result immediately.
    let mut ordered: Vec<Option<FinalizedToolCall>> = Vec::new();
    let mut pending: Vec<(usize, DeferredToolCall<'_>)> = Vec::new();

    for tool_call in tool_calls {
        emit.emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
            parent_tool_call_id: None,
        })
        .await;

        #[allow(clippy::single_match_else)] // mirrors pi's if (prepared) / else
        match prepare_tool_call(&assistant, tool_call, executor, &config.hooks, signal).await {
            Some((result, is_error)) => {
                let finalized = FinalizedToolCall {
                    tool_call: tool_call.clone(),
                    result,
                    is_error,
                    duration_ms: None,
                };
                emit_tool_execution_end(&finalized, emit).await;
                ordered.push(Some(finalized));
            }
            None => {
                let index = ordered.len();
                pending.push((
                    index,
                    DeferredToolCall {
                        assistant: assistant.clone(),
                        tool_call: tool_call.clone(),
                        executor,
                        config,
                        signal,
                    },
                ));
                ordered.push(None);
            }
        }
        if signal.aborted() {
            break;
        }
    }

    // Run the prepared tools concurrently; each one reports its own end event as
    // it completes, exactly like pi's `Promise.all` over the deferred closures.
    let mut queue = FuturesUnordered::new();
    for (index, deferred) in pending {
        queue.push(async move {
            let updates =
                execute_prepared_tool_call(&deferred.tool_call, deferred.executor, deferred.signal)
                    .await;
            let (result, is_error, updates) = updates;
            let finalized = finalize_executed_tool_call(
                &deferred.assistant,
                &deferred.tool_call,
                result,
                is_error,
                deferred.config,
            )
            .await;
            (index, finalized, updates)
        });
    }
    while let Some((index, finalized, updates)) = queue.next().await {
        for update in updates {
            emit_tool_execution_update(&finalized.tool_call, update, emit).await;
        }
        emit_tool_execution_end(&finalized, emit).await;
        if let Some(slot) = ordered.get_mut(index) {
            *slot = Some(finalized);
        }
    }

    // Tool-result message artifacts are emitted later, in assistant source order.
    let finalized_calls: Vec<FinalizedToolCall> = ordered.into_iter().flatten().collect();
    let mut messages = Vec::new();
    for finalized in &finalized_calls {
        let message = create_tool_result_message(finalized);
        emit_tool_result_message(&message, emit).await;
        messages.push(message);
    }

    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized_calls),
    }
}

struct DeferredToolCall<'a> {
    assistant: AssistantMessage,
    tool_call: AgentToolCall,
    executor: &'a dyn ToolExecutor,
    config: &'a AgentLoopConfig,
    signal: &'a AbortSignal,
}

/// Early termination only happens when *every* finalized result in the batch
/// asked for it.
fn should_terminate_tool_batch(finalized: &[FinalizedToolCall]) -> bool {
    !finalized.is_empty() && finalized.iter().all(|call| call.result.terminate)
}

/// `executePreparedToolCall`. Updates published before the call settles are
/// returned for emission; anything published afterwards is dropped, which is
/// pi's `acceptingUpdates = false`.
async fn execute_prepared_tool_call(
    tool_call: &AgentToolCall,
    executor: &dyn ToolExecutor,
    signal: &AbortSignal,
) -> (AgentToolResult, bool, Vec<AgentToolResult>) {
    let (updates, mut receiver) = tokio::sync::mpsc::unbounded_channel::<AgentToolResult>();
    let executing = executor.execute(tool_call.clone(), updates);
    tokio::pin!(executing);

    let outcome = tokio::select! {
        biased;
        () = wait_for_abort(signal.clone()) => {
            // The signal fired mid-execution: dropping the future cancels the
            // tool, the same observable effect as pi's aborted execute().
            (AgentToolResult::error("Operation aborted"), true)
        }
        result = &mut executing => match result {
            Ok(result) => {
                let is_error = result.is_error;
                (result, is_error)
            }
            Err(message) => (AgentToolResult::error(message), true),
        },
    };
    let mut collected = Vec::new();
    while let Ok(update) = receiver.try_recv() {
        collected.push(update);
    }
    // Dropping the receiver makes later sends fail, so a tool that keeps
    // publishing after it settled cannot reorder events.
    drop(receiver);
    let (result, is_error) = outcome;
    (result, is_error, collected)
}

/// `finalizeExecutedToolCall`. A hook that throws replaces the result with an
/// error result rather than failing the turn.
async fn finalize_executed_tool_call(
    assistant: &AssistantMessage,
    tool_call: &AgentToolCall,
    result: AgentToolResult,
    is_error: bool,
    config: &AgentLoopConfig,
) -> FinalizedToolCall {
    let mut result = result;
    let mut is_error = is_error;
    if let Some(after) = config.hooks.after.clone() {
        if let Some(overrides) = after.call(assistant, tool_call, &result, is_error).await {
            merge_after_tool_call(&mut result, &mut is_error, overrides);
        }
    }
    FinalizedToolCall {
        tool_call: tool_call.clone(),
        result,
        is_error,
        duration_ms: None,
    }
}

/// Field-by-field merge, including pi's rule that replacing `content` without
/// `structuredContent` drops the structured content.
fn merge_after_tool_call(
    result: &mut AgentToolResult,
    is_error: &mut bool,
    overrides: AfterToolCallOverride,
) {
    let structured_content = overrides.structured_content.clone().or_else(|| {
        if overrides.content.is_some() {
            None
        } else {
            result.structured_content.clone()
        }
    });
    if let Some(content) = overrides.content {
        result.content = content;
    }
    if let Some(details) = overrides.details {
        result.details = details;
    }
    if let Some(usage) = overrides.usage {
        result.usage = Some(usage);
    }
    if let Some(terminate) = overrides.terminate {
        result.terminate = terminate;
    }
    result.structured_content = structured_content;
    if let Some(flag) = overrides.is_error {
        *is_error = flag;
    }
}

/// `prepareToolCall`. Returns `Some((result, is_error))` when the call never
/// reaches `execute`: unknown tool, blocked by the before-hook, or aborted.
async fn prepare_tool_call(
    assistant: &AssistantMessage,
    tool_call: &AgentToolCall,
    executor: &dyn ToolExecutor,
    hooks: &ToolCallHooks,
    signal: &AbortSignal,
) -> Option<(AgentToolResult, bool)> {
    let known = executor
        .tools()
        .iter()
        .any(|tool| tool.name == tool_call.name);
    if !known {
        return Some((
            AgentToolResult::error(format!("Tool {} not found", tool_call.name)),
            true,
        ));
    }

    if let Some(before) = hooks.before.as_ref() {
        if let Some(decision) = before.call(assistant, tool_call).await {
            if signal.aborted() {
                return Some((AgentToolResult::error("Operation aborted"), true));
            }
            if decision.block {
                let mut result = AgentToolResult::error(
                    decision
                        .reason
                        .unwrap_or_else(|| "Tool execution was blocked".to_string()),
                );
                result.terminate = decision.terminate;
                return Some((result, true));
            }
        }
    }

    if signal.aborted() {
        return Some((AgentToolResult::error("Operation aborted"), true));
    }
    None
}

async fn emit_tool_execution_update(
    tool_call: &AgentToolCall,
    update: AgentToolResult,
    emit: &mut dyn AgentEventSink,
) {
    emit.emit(AgentEvent::ToolExecutionUpdate {
        tool_call_id: tool_call.id.clone(),
        tool_name: tool_call.name.clone(),
        args: tool_call.arguments.clone(),
        partial_result: update,
        parent_tool_call_id: None,
    })
    .await;
}

async fn wait_for_abort(signal: AbortSignal) {
    loop {
        if signal.aborted() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

async fn emit_tool_execution_end(finalized: &FinalizedToolCall, emit: &mut dyn AgentEventSink) {
    emit.emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        result: finalized.result.clone(),
        is_error: finalized.is_error,
        parent_tool_call_id: None,
    })
    .await;
}

fn create_tool_result_message(finalized: &FinalizedToolCall) -> AgentMessage {
    AgentMessage::ToolResult(pi_ai::types::ToolResultMessage {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        content: finalized
            .result
            .content
            .iter()
            .map(|block| match block {
                pi_ai::types::ContentBlock::Text(text) => {
                    pi_ai::types::InputContent::Text(text.clone())
                }
                pi_ai::types::ContentBlock::Image(image) => {
                    pi_ai::types::InputContent::Image(image.clone())
                }
                // A tool result can only carry model-visible text/images; anything
                // else is a tool bug and degrades to its text form.
                pi_ai::types::ContentBlock::Thinking(thinking) => {
                    pi_ai::types::InputContent::Text(pi_ai::types::TextContent {
                        text: thinking.thinking.clone(),
                        text_signature: None,
                    })
                }
                pi_ai::types::ContentBlock::ToolCall(call) => {
                    pi_ai::types::InputContent::Text(pi_ai::types::TextContent {
                        text: format!("{}({})", call.name, call.id),
                        text_signature: None,
                    })
                }
            })
            .collect(),
        details: Some(finalized.result.details.clone()),
        usage: finalized.result.usage,
        nested_calls: None,
        is_error: finalized.is_error,
        timestamp: now_millis(),
        duration_ms: finalized.duration_ms,
    })
}

async fn emit_tool_result_message(message: &AgentMessage, emit: &mut dyn AgentEventSink) {
    emit.emit(AgentEvent::MessageStart {
        message: message.clone(),
    })
    .await;
    emit.emit(AgentEvent::MessageEnd {
        message: message.clone(),
    })
    .await;
}

/// Unix milliseconds, like pi's `Date.now()`.
pub fn now_millis() -> i64 {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok();
    let millis = match since_epoch {
        Some(elapsed) => u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        None => u64::MAX,
    };
    i64::try_from(millis).unwrap_or(i64::MAX)
}

/// Build a tool call block, for hosts that synthesize calls.
pub fn tool_call(id: &str, name: &str, arguments: JsonObject) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

/// Silence the unused-import lint for `Value`, which appears in the hook types.
const _: Option<Value> = None;
