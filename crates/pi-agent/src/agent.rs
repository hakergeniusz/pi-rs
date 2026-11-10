//! The `Agent` facade.
//!
//! Ground truth: pi-mono `packages/agent/src/agent.ts`.
//!
//! pi's `Agent` is stateful: it holds the message list, the tool set, the
//! steering/follow-up queues, one active run at a time, and a list of listeners
//! that are awaited in order. `AgentSession` in `packages/coding-agent` adds
//! persistence, compaction, and retries on top; none of that belongs here.
//!
//! Differences from the TS class, all forced by ownership:
//!
//! * listeners are synchronous (`Fn(&AgentEvent)`) and run in order. pi's are
//!   async so a listener can await I/O; `pi-cli` writes through a blocking
//!   handle instead.
//! * the state object is [`AgentState`], as in pi; its transitions live with
//!   it in [`crate::state`], and every accessor here delegates.
//! * the abort signal is cooperative: the loop checks it where pi checks
//!   `signal.aborted`, and `tokio::select!` cancels a running tool.
//! * a run can be *dropped* mid-flight, which pi's promise cannot be. pi's
//!   `runWithLifecycle` ends in `finally { this.finishRun() }`
//!   (`agent.ts:526-528`), so the run always finishes; [`RunGuard`](crate::state::RunGuard) finishes a
//!   dropped one, and the run-lifecycle fields sit behind an `Arc` so it can.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::agent_message::AgentMessage;
use pi_ai::types::Model;
use pi_ai::types::{AssistantMessage, StopReason, Usage};

use futures::FutureExt;

use crate::agent_loop::{AgentLoopConfig, run_agent_loop, run_agent_loop_continue};
use crate::events::{AgentEvent, AgentEventSink};
use crate::live_settings::SettingsHandle;
use crate::queues::{ContinueDrain, QueueHandle};
use crate::state::AgentState;
use crate::types::{
    AbortSignal, BoxFuture, QueueMode, StreamFn, ThinkingLevel, ToolExecutionMode, ToolExecutor,
};

/// A listener. pi's is `(event, signal) => Promise<void> | void`
/// (`agent.ts:266`); the signal is **the active abort signal for the current
/// run**, so a listener can cancel from inside an event without reaching for
/// the agent. pi also returns an unsubscribe function from `subscribe`; that
/// is [`Agent::subscribe`]'s [`ListenerId`] here, redeemed by
/// [`Agent::unsubscribe`].
pub type Listener = Arc<dyn Fn(&AgentEvent, &AbortSignal) + Send + Sync>;

/// The handle [`Agent::subscribe`] returns, standing in for pi's unsubscribe
/// closure. [`Agent::unsubscribe`] removes exactly this listener and leaves the
/// rest in subscription order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ListenerId(u64);

/// Drives the loop's events into agent state and then into the listeners.
struct LoopSink<'a> {
    state: &'a mut AgentState,
    listeners: &'a [(ListenerId, Listener)],
    signal: &'a AbortSignal,
}

impl AgentEventSink for LoopSink<'_> {
    fn emit(&mut self, event: AgentEvent) -> BoxFuture<'_, ()> {
        // pi reduces state first (`processEvents`, `agent.ts:570-608`), then
        // awaits the listeners; the switch now lives with the state it writes.
        self.state.apply_event(&event);
        for (_, listener) in self.listeners {
            listener(&event, self.signal);
        }
        Box::pin(async {})
    }
}

/// The agent. One run at a time: `prompt` and `continue_run` fail while a run
/// is active, and `steer`/`follow_up` are the way in during one.
pub struct Agent {
    state: AgentState,
    config: AgentLoopConfig,
    /// pi's two queues (`agent.ts:191-192`), behind the handle so a caller
    /// can reach them while a run holds `&mut Agent`.
    queues: QueueHandle,
    listeners: Vec<(ListenerId, Listener)>,
    next_listener_id: u64,
    executor: Arc<dyn ToolExecutor>,
    stream_fn: Arc<dyn StreamFn>,
    signal: AbortSignal,
    /// pi's `skipInitialSteeringPoll`: `continue()` from an assistant message
    /// re-enters through `prompt` with the already-drained steering queue.
    skip_initial_steering_poll: bool,
}

impl Agent {
    /// pi's `Agent.create({ initialState })`. `api_key` is the wire credential
    /// the host resolved through pi's auth ladder.
    pub fn new(
        model: Model,
        system_prompt: Option<&str>,
        executor: Arc<dyn ToolExecutor>,
        stream_fn: Arc<dyn StreamFn>,
    ) -> Agent {
        let tools = executor.tools();
        let mut messages = Vec::new();
        let declarations: Vec<pi_ai::types::Tool> = tools
            .iter()
            .map(crate::transcript::tool_declaration)
            .collect();
        if let Some(system) =
            crate::transcript::create_initial_system_message(system_prompt, &declarations)
        {
            messages.push(AgentMessage::System(system));
        }
        Agent {
            state: AgentState::new(messages, tools, model),
            config: AgentLoopConfig::default(),
            queues: QueueHandle::new(),
            listeners: Vec::new(),
            next_listener_id: 0,
            executor,
            stream_fn,
            signal: AbortSignal::new(),
            skip_initial_steering_poll: false,
        }
    }

    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Agent {
        self.config.session_id = Some(session_id.into());
        self
    }

    #[must_use]
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Agent {
        self.config.api_key = Some(api_key.into());
        self
    }

    #[must_use]
    pub fn with_thinking_level(mut self, thinking_level: ThinkingLevel) -> Agent {
        self.state.set_thinking_level(thinking_level);
        self
    }

    #[must_use]
    pub fn with_tool_execution(mut self, mode: ToolExecutionMode) -> Agent {
        self.state.set_tool_execution(mode);
        self
    }

    #[must_use]
    pub fn with_hooks(mut self, hooks: crate::types::ToolCallHooks) -> Agent {
        self.config.hooks = hooks;
        self
    }

    #[must_use]
    pub fn with_finish_turn(
        mut self,
        finish_turn: Arc<dyn crate::agent_loop::FinishTurn>,
    ) -> Agent {
        self.config.finish_turn = Some(finish_turn);
        self
    }

    #[must_use]
    pub fn with_prepare_request(
        mut self,
        prepare_request: Arc<dyn crate::agent_loop::PrepareRequest>,
    ) -> Agent {
        self.config.prepare_request = Some(prepare_request);
        self
    }

    #[must_use]
    pub fn with_prepare_next_turn(
        mut self,
        prepare_next_turn: Arc<dyn crate::agent_loop::PrepareNextTurn>,
    ) -> Agent {
        self.config.prepare_next_turn = Some(prepare_next_turn);
        self
    }

    #[must_use]
    pub fn with_transform_context(
        mut self,
        transform_context: Arc<dyn crate::agent_loop::TransformContext>,
    ) -> Agent {
        self.config.transform_context = Some(transform_context);
        self
    }

    /// pi's mutable `Agent.convertToLlm` field (`agent.ts:194`, assigned at
    /// `agent.ts:234`). Replaces the session-to-LLM converter for every request
    /// the agent makes.
    ///
    /// Construction-time form of the same assignment; see
    /// [`Agent::set_convert_to_llm`] for the field-shaped one.
    #[must_use]
    pub fn with_convert_to_llm(
        mut self,
        convert_to_llm: Arc<dyn crate::agent_loop::ConvertToLlm>,
    ) -> Agent {
        self.config.convert_to_llm = convert_to_llm;
        self
    }

    /// pi's public mutable `convertToLlm` field, assigned after construction —
    /// what the coding-agent extensions do (`examples/extensions/handoff.ts`,
    /// `custom-compaction.ts`). The new converter is read into the loop config
    /// by [`Agent::apply_state_to_config`] at the start of the next run, so it
    /// applies from the next request on.
    ///
    /// pi builds `createLoopConfig()` once per run (`agent.ts:440`, `:452`) and
    /// captures `this.convertToLlm` there, so a swap mid-run does not reach the
    /// run in flight; `&mut self` here gives the same boundary, since a run
    /// holds the agent mutably.
    pub fn set_convert_to_llm<F>(&mut self, convert_to_llm: F)
    where
        F: Fn(Vec<AgentMessage>) -> Vec<AgentMessage> + Send + Sync + 'static,
    {
        self.config.convert_to_llm = Arc::new(convert_to_llm);
    }

    /// The converter the next run will use: pi's `agent.convertToLlm` field
    /// read.
    pub fn convert_to_llm(&self) -> &Arc<dyn crate::agent_loop::ConvertToLlm> {
        &self.config.convert_to_llm
    }

    /// `Agent.subscribe` (`agent.ts:266-269`). Listeners run in registration
    /// order; each one receives the run's abort signal alongside the event.
    /// Returns the handle that unsubscribes just this listener.
    pub fn subscribe<F>(&mut self, listener: F) -> ListenerId
    where
        F: Fn(&AgentEvent, &AbortSignal) + Send + Sync + 'static,
    {
        let id = ListenerId(self.next_listener_id);
        self.next_listener_id += 1;
        self.listeners.push((id, Arc::new(listener)));
        id
    }

    /// pi's unsubscribe closure: removes one listener, keeps the rest.
    pub fn unsubscribe(&mut self, id: ListenerId) {
        self.listeners.retain(|(listener_id, _)| *listener_id != id);
    }

    /// Drop every listener. pi has no single call for this; it is the Rust
    /// stand-in for holding every unsubscribe function and calling it.
    pub fn clear_listeners(&mut self) {
        self.listeners.clear();
    }

    /// pi's `get state()` (`agent.ts:280-282`): the object every read below
    /// delegates to, and the only mutable state the agent owns.
    pub fn state(&self) -> &AgentState {
        &self.state
    }

    /// `Agent.state.systemPrompt`: the prompt replayed from the transcript's
    /// system messages, so it already includes every sectioned prompt patch.
    pub fn system_prompt(&self) -> String {
        self.state.system_prompt()
    }

    /// `Agent.state.messages`.
    pub fn messages(&self) -> &[AgentMessage] {
        self.state.messages()
    }

    /// `Agent.state.tools`.
    pub fn tools(&self) -> &[crate::types::ToolSpec] {
        self.state.tools()
    }

    pub fn is_streaming(&self) -> bool {
        self.state.is_streaming()
    }

    /// pi's `Agent.state.streamingMessage`. Cloned out rather than borrowed:
    /// the field is behind the run lifecycle box that [`RunGuard`](crate::state::RunGuard) finishes,
    /// so there is no borrow to hand out.
    pub fn streaming_message(&self) -> Option<AssistantMessage> {
        self.state.streaming_message()
    }

    /// `Agent.state.pendingToolCalls`. Cloned for the same reason as
    /// [`Agent::streaming_message`].
    pub fn pending_tool_calls(&self) -> BTreeSet<String> {
        self.state.pending_tool_calls()
    }

    /// `Agent.state.errorMessage`.
    pub fn error_message(&self) -> Option<&str> {
        self.state.error_message()
    }

    pub fn model(&self) -> &Model {
        self.state.model()
    }

    pub fn set_model(&mut self, model: Model) {
        // A model written through the handle mid-run is a pending override
        // until the loop or this call takes it; an explicit set through the
        // agent wins, so the pending cell is dropped here.
        self.state.live.clear_pending_model();
        self.state.set_model(model);
    }

    pub fn set_thinking_level(&mut self, level: ThinkingLevel) {
        self.state.set_thinking_level(level);
    }

    /// The level the next request will ask for. Live: a change written through
    /// [`Agent::live_settings`] mid-run is what the run's **next** provider
    /// request uses (`agent-loop.ts:186-199`).
    pub fn thinking_level(&self) -> ThinkingLevel {
        self.state.thinking_level()
    }

    /// The out-of-band handle to the settings pi keeps live during a run: the
    /// reasoning level and the model. `session.setThinkingLevel(...)`
    /// (`agent-session.ts:2565`) and `session.setModel(...)` (`:2430`) are
    /// plain methods on an object every caller shares, so they work mid-run and
    /// the next LLM call in that run reads them; a Rust `prompt` holds
    /// `&mut Agent` for the whole run, so the same reachability is this
    /// handle. Clone it before starting a run and keep it - every `&mut self`
    /// setter below writes the same cells.
    pub fn live_settings(&self) -> SettingsHandle {
        self.state.live.clone()
    }

    /// The signal for the next run. Held by the host so a long-running CLI can
    /// cancel from outside the agent.
    pub fn abort_signal(&self) -> AbortSignal {
        self.signal.clone()
    }

    /// `Agent.abort()`: cancel the active run and arm a fresh signal.
    pub fn abort(&mut self) {
        self.signal.abort();
        self.signal = AbortSignal::new();
    }

    /// The out-of-band enqueue: pi's `agent.steer(...)` / `agent.followUp(...)`
    /// (`agent.ts:299-306`) are plain methods on an object JS lets every caller
    /// reach, so they work mid-run - pi's `--mode rpc` relies on it to deliver
    /// `steer` / `followUp` while a prompt streams. A Rust `prompt` holds
    /// `&mut Agent` for the whole run, so the same reachability is this
    /// handle: it shares the queues and nothing else, so it is `Send + Sync`
    /// while the agent stays `!Send`.
    ///
    /// Clone it before starting a run and keep it: [`Agent::steer`] and
    /// [`Agent::follow_up`] are the same enqueues for a caller that happens to
    /// hold the agent.
    pub fn queue_handle(&self) -> QueueHandle {
        self.queues.clone()
    }

    /// `Agent.steer(messages)`: queued now, injected at the next steering poll.
    pub fn steer(&mut self, messages: impl IntoIterator<Item = AgentMessage>) {
        self.queues.steer(messages);
    }

    /// `Agent.followUp(messages)`.
    pub fn follow_up(&mut self, messages: impl IntoIterator<Item = AgentMessage>) {
        self.queues.follow_up(messages);
    }

    /// `Agent.setState`: replace the transcript without running the loop. Hosts
    /// use it to restore a saved session or to hand-pick the message to
    /// continue from.
    pub fn set_messages(&mut self, messages: Vec<AgentMessage>) {
        self.state.set_messages(messages);
    }

    /// `Agent.setState` for the model and reasoning level.
    pub fn set_model_and_thinking(&mut self, model: Model, thinking_level: ThinkingLevel) {
        self.state.live.clear_pending_model();
        self.state.set_model(model);
        self.state.set_thinking_level(thinking_level);
    }

    /// `Agent.steeringMode` (`agent.ts:281-287`).
    pub fn steering_mode(&self) -> QueueMode {
        self.queues.steering_mode()
    }

    /// `Agent.followUpMode` (`agent.ts:290-296`).
    pub fn follow_up_mode(&self) -> QueueMode {
        self.queues.follow_up_mode()
    }

    /// `Agent.steeringMode = mode`.
    pub fn set_steering_queue_mode(&mut self, mode: QueueMode) {
        self.queues.set_steering_mode(mode);
    }

    /// `Agent.followUpMode = mode`.
    pub fn set_follow_up_queue_mode(&mut self, mode: QueueMode) {
        self.queues.set_follow_up_mode(mode);
    }

    /// Both mode setters in one call.
    pub fn set_queue_modes(&mut self, steering: QueueMode, follow_up: QueueMode) {
        self.queues.set_steering_mode(steering);
        self.queues.set_follow_up_mode(follow_up);
    }

    /// `Agent.clearSteeringQueue()`.
    pub fn clear_steering_queue(&mut self) {
        self.queues.clear_steering();
    }

    /// `Agent.clearFollowUpQueue()`.
    pub fn clear_follow_up_queue(&mut self) {
        self.queues.clear_follow_up();
    }

    /// `Agent.clearAllQueues()` (`agent.ts:319-322`): both queues, in pi's order.
    pub fn clear_all_queues(&mut self) {
        self.queues.clear_all();
    }

    /// `Agent.hasQueuedMessages()` (`agent.ts:325-327`): true when either
    /// queue still holds a message.
    pub fn has_queued_messages(&self) -> bool {
        self.queues.has_items()
    }

    /// `Agent.peekQueuedMessages()` (`agent.ts:330-332`): the messages the
    /// next turn would inject, **without consuming them**.
    ///
    /// Steering wins whenever it has anything, and each queue previews only as
    /// far as its own mode reaches: `one-at-a-time` yields the oldest message,
    /// `all` yields everything queued.
    pub fn peek_queued_messages(&self) -> Vec<AgentMessage> {
        self.queues.peek()
    }

    /// `Agent.reset()` (`agent.ts:355-366`): keep the replayed system prompt
    /// and tool baseline, drop everything else.
    ///
    /// pi throws when a run is active; here a state reset that would race a run
    /// reports false and changes nothing, so the queues are left alone as well.
    pub fn reset(&mut self) {
        if !self.state.reset() {
            return;
        }
        self.clear_follow_up_queue();
        self.clear_steering_queue();
    }

    /// `Agent.prompt(messages)`.
    ///
    /// The loop runs inside a catch. pi's `runWithLifecycle`
    /// (`agent.ts:507-528`) has the same shape: whatever the run throws, its
    /// `finally` calls `finishRun()` before the throw reaches the caller, so
    /// the agent is left usable by the next prompt and by `reset()`. Without
    /// that, an unwind out of the loop leaves `is_streaming` set - every later
    /// prompt then answers "Agent is already processing" - and the listeners
    /// stay taken, so nothing is recorded for the rest of the session. The
    /// panic is re-raised for the caller to report.
    ///
    /// pi's promise cannot be dropped, so its `finally` is enough. This future
    /// can be: `tokio::time::timeout(d, agent.prompt(..))` drops it mid-run.
    /// [`RunGuard`](crate::state::RunGuard) covers that case - dropping the future finishes the run -
    /// and the listeners are borrowed rather than taken, so a dropped run
    /// cannot take the listener set with it.
    pub async fn prompt(&mut self, messages: Vec<AgentMessage>) -> Result<(), String> {
        if self.state.is_streaming() {
            return Err(
                "Agent is already processing a prompt. Use steer() or follow_up() to queue messages, or wait for completion."
                    .to_string(),
            );
        }
        self.apply_state_to_config();
        let context = self.state.context_snapshot();
        let mut guard = self.state.begin_run();
        let mut sink = LoopSink {
            state: &mut self.state,
            listeners: &self.listeners,
            signal: &self.signal,
        };
        // The loop polls the same queues the agent does, through a clone of
        // the handle: one lock per poll, never held across an await, so an
        // out-of-band `steer` lands at the next poll the way pi's does.
        let mut queues = self.queues.clone();
        let run = std::panic::AssertUnwindSafe(run_agent_loop(
            messages,
            context,
            self.executor.as_ref(),
            &self.config,
            &mut queues,
            &mut sink,
            &self.signal,
            self.stream_fn.as_ref(),
        ))
        .catch_unwind()
        .await;
        self.finish_run();
        self.absorb_pending_model();
        guard.disarm();
        match run {
            Ok(_) => Ok(()),
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    /// `Agent.prompt(text)`.
    pub async fn prompt_text(&mut self, text: &str) -> Result<(), String> {
        self.prompt(vec![crate::transcript::user_message(
            text,
            crate::agent_loop::now_millis(),
        )])
        .await
    }

    /// `Agent.continue()`. The last message must be a user or tool-result
    /// message; from an assistant message, queued steering/follow-up messages
    /// take precedence, matching pi.
    pub async fn continue_run(&mut self) -> Result<(), String> {
        if self.state.is_streaming() {
            return Err(
                "Agent is already processing. Wait for completion before continuing.".to_string(),
            );
        }
        let last_role = self.state.messages().last().map(AgentMessage::role);
        if self.state.messages().is_empty()
            || self
                .state
                .messages()
                .iter()
                .all(|message| message.role() == "system")
        {
            return Err("No messages to continue from".to_string());
        }
        if last_role == Some("assistant") {
            // pi's `continue()` takes steering first and only then follow-up
            // (`agent.ts:381-395`). Only the steering branch re-enters through
            // `prompt` with `skipInitialSteeringPoll` set, because only that
            // branch has already drained the queue it is about to inject.
            match self.queues.drain_for_continue() {
                Some(ContinueDrain::Steering(messages)) => {
                    self.skip_initial_steering_poll = true;
                    let result = self.prompt(messages).await;
                    self.skip_initial_steering_poll = false;
                    return result;
                }
                Some(ContinueDrain::FollowUp(messages)) => return self.prompt(messages).await,
                None => return Err("Cannot continue from message role: assistant".to_string()),
            }
        }

        self.apply_state_to_config();
        let context = self.state.context_snapshot();
        let mut guard = self.state.begin_run();
        let mut sink = LoopSink {
            state: &mut self.state,
            listeners: &self.listeners,
            signal: &self.signal,
        };
        let mut queues = self.queues.clone();
        let result = run_agent_loop_continue(
            context,
            self.executor.as_ref(),
            &self.config,
            &mut queues,
            &mut sink,
            &self.signal,
            self.stream_fn.as_ref(),
        )
        .await;
        self.finish_run();
        self.absorb_pending_model();
        guard.disarm();
        result.map(|_| ())
    }

    /// `finishRun()`: the agent is idle once the loop has returned. Tool calls
    /// still marked pending were cancelled. The guard's `Drop` calls the same
    /// thing when the run future is dropped instead of returning.
    ///
    /// Every exit path out of a run lands here or on that `Drop`: the normal
    /// return, the panic the catch re-raises, and the dropped future. There is
    /// no fourth path - adding one without one of these two is the audit-9 D5
    /// regression.
    fn finish_run(&mut self) {
        self.state.finish_run();
    }

    /// The per-turn half of `createLoopConfig` that reads state: model, the
    /// reasoning level, and the tool execution strategy (`agent.ts:461-499`).
    fn apply_state_to_config(&mut self) {
        self.config.model = self.state.model().clone();
        self.config.thinking_level = self.state.thinking_level();
        self.config.tool_execution = self.state.tool_execution();
        // The loop's live half: pi reaches for the session's state on every
        // turn through `prepareNextTurn` (`agent-session.ts:899-900` applied
        // at `agent-loop.ts:186-199`); here it reads this handle.
        self.config.live_settings = Some(self.state.live.clone());
    }

    /// A model set through [`Agent::live_settings`] while a run held the agent
    /// is applied to `state.model` once the run settles, so the next run's
    /// `createLoopConfig` snapshot starts from what the loop already streamed.
    /// pi needs no equivalent: its write landed on `state.model` at once
    /// (`agent-session.ts:2437`).
    fn absorb_pending_model(&mut self) {
        if let Some(model) = self.state.live.take_pending_model() {
            self.state.set_model(model);
        }
    }

    /// `Agent.handleRunFailure`: an unexpected loop failure is still reported
    /// as a complete assistant message, not as a panic.
    pub fn handle_run_failure(&mut self, error: &str, aborted: bool) -> Vec<AgentEvent> {
        let failure = AssistantMessage {
            content: vec![pi_ai::types::AssistantContent::Text(
                pi_ai::types::TextContent {
                    text: String::new(),
                    text_signature: None,
                },
            )],
            api: self.state.model().api.clone(),
            provider: self.state.model().provider.clone(),
            model: self.state.model().id.clone(),
            usage: Usage::default(),
            stop_reason: if aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            },
            error_message: Some(error.to_string()),
            timestamp: crate::agent_loop::now_millis(),
            ..crate::agent_loop::empty_assistant_message()
        };
        let message = AgentMessage::Assistant(failure);
        vec![
            AgentEvent::MessageStart {
                message: message.clone(),
            },
            AgentEvent::MessageEnd {
                message: message.clone(),
            },
            AgentEvent::TurnEnd {
                message: message.clone(),
                tool_results: Vec::new(),
            },
            AgentEvent::AgentEnd {
                messages: vec![message],
            },
        ]
    }

    /// `skipInitialSteeringPoll`, exposed for the loop.
    pub fn takes_skip_initial_steering_poll(&mut self) -> bool {
        let skip = self.skip_initial_steering_poll;
        self.skip_initial_steering_poll = false;
        skip
    }
}
