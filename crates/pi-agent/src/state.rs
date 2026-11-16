//! pi's agent state object: `_state` (`agent.ts:186`) plus the transitions that
//! mutate it.
//!
//! Ground truth: pi-mono `packages/agent/src/agent.ts` (`MutableAgentState`,
//! `createMutableAgentState`, `runWithLifecycle`, `finishRun`, `processEvents`,
//! `reset`) and the read-only view of the same data, `AgentState` in
//! `types.ts:382`.
//!
//! pi keeps one object behind `Agent.state`. Every mutation of it is a method
//! on `Agent` - `runWithLifecycle` sets `isStreaming`, `processEvents` reduces
//! each loop event into it, `finishRun` clears the runtime fields, `reset`
//! truncates the transcript to its baseline. This module is that object: the
//! fields, and the transitions that are pi's transition methods verbatim.
//! [`crate::agent::Agent`] keeps the loops, the queues, the listeners and the
//! wiring, and delegates.
//!
//! What is *not* here, because pi does not put it in `_state` either: the two
//! [`PendingMessageQueue`](crate::types::PendingMessageQueue)s and the active
//! run's abort controller are `Agent` fields in TS (`agent.ts:191-196`,
//! `:210`), not state.
//!
//! The one shape change forced by Rust ownership: pi's three run-owned fields
//! (`isStreaming`, `streamingMessage`, `pendingToolCalls`) sit in
//! [`RunLifecycle`] behind an [`Arc`], because [`RunGuard`] has to finish a run
//! whose future is being dropped and pi's `finally` cannot. Everything else is
//! a plain field.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::agent_message::AgentMessage;
use pi_ai::types::AssistantMessage;
use pi_ai::types::Model;

use crate::events::AgentEvent;
use crate::types::{AgentContext, ThinkingLevel, ToolExecutionMode, ToolSpec};

/// The three fields `finishRun()` clears, in one interior-mutable box.
///
/// pi keeps them on `_state` and reaches them from `finishRun` in a `finally`
/// that always runs (`agent.ts:526-528`, `:550-556`). A Rust future carries no
/// such guarantee: the caller can drop it - `tokio::time::timeout(d,
/// agent.prompt(..))` is the ordinary way to bound a run - and then there is no
/// `finally` left to run. So the state the run clears has to be reachable
/// without the `&mut Agent` borrow the run future holds, which means `Arc` plus
/// interior mutability rather than three plain fields.
///
/// The reads the loop itself makes are unchanged in behaviour; only the way the
/// box is reached changed.
#[derive(Debug, Default)]
pub(crate) struct RunLifecycle {
    pub(crate) is_streaming: AtomicBool,
    pub(crate) streaming_message: Mutex<Option<AssistantMessage>>,
    pub(crate) pending_tool_calls: Mutex<BTreeSet<String>>,
}

impl RunLifecycle {
    /// pi's `this._state.isStreaming = true; this._state.streamingMessage =
    /// undefined; this._state.errorMessage = undefined;` at the top of
    /// `runWithLifecycle` (`agent.ts:517-519`), minus `errorMessage`, which pi
    /// clears on the agent's plain state and this port clears at the same
    /// place ([`AgentState::begin_run`]).
    fn start(&self) {
        self.is_streaming.store(true, Ordering::SeqCst);
        lock(&self.streaming_message).take();
    }

    /// pi's `finishRun()` (`agent.ts:550-556`), minus `activeRun`, which
    /// `waitForIdle` owns on the other branch.
    pub(crate) fn finish(&self) {
        self.is_streaming.store(false, Ordering::SeqCst);
        lock(&self.streaming_message).take();
        lock(&self.pending_tool_calls).clear();
    }
}

/// A poisoned lock means a listener panicked while it held the field; the
/// value inside is still the one the loop wrote, and a panic elsewhere is
/// reported through the run, not through this box.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Stands in for pi's `finally { this.finishRun() }`.
///
/// pi's promise cannot be cancelled, so `finishRun` always runs. This guard is
/// armed when a run starts and disarmed once the run has finished normally or
/// unwound into the caller; if the future is dropped instead, `Drop` runs
/// `finish_run`'s half and the agent is idle again. That is the whole of
/// audit-9 D5: without it a dropped run leaves `is_streaming` set, every later
/// prompt is refused with "Agent is already processing", and `reset()` is a
/// no-op for the rest of the session.
pub(crate) struct RunGuard {
    run: Arc<RunLifecycle>,
    armed: bool,
}

impl RunGuard {
    pub(crate) fn new(run: Arc<RunLifecycle>) -> RunGuard {
        run.start();
        RunGuard { run, armed: true }
    }

    /// The run reached `finish_run` on its own; `Drop` has nothing to repair.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if self.armed {
            self.run.finish();
        }
    }
}

/// Everything `Agent.state` holds, plus pi's transitions over it.
///
/// Field-for-field with `MutableAgentState` (`agent.ts:68-72`, built by
/// `createMutableAgentState` at `agent.ts:87-112`) and the `AgentState` view
/// in `types.ts:382-420`:
///
/// | pi | here |
/// | --- | --- |
/// | `systemPrompt` (derived) | [`AgentState::system_prompt`] |
/// | `model` | `model` (a mid-run write goes through [`SettingsHandle`] first) |
/// | `thinkingLevel` | [`SettingsHandle`] |
/// | `tools` | `tools` |
/// | `messages` | `messages` |
/// | `isStreaming` | [`RunLifecycle`] / [`AgentState::is_streaming`] |
/// | `streamingMessage` | [`AgentState::streaming_message`] |
/// | `pendingToolCalls` | [`AgentState::pending_tool_calls`] |
/// | `errorMessage` | `error_message` |
///
/// pi's assigning `tools`/`messages` copies the top-level array; Rust's
/// ownership already makes that unobservable, so [`AgentState::set_messages`]
/// takes the vector by value.
#[derive(Debug)]
pub struct AgentState {
    pub(crate) messages: Vec<AgentMessage>,
    pub(crate) tools: Vec<ToolSpec>,
    pub(crate) model: Model,
    /// pi's `thinkingLevel`, behind the handle so the loop reads it per
    /// request and a caller can write it mid-run
    /// (`agent-loop.ts:186-199`, `agent-session.ts:899-900`).
    pub(crate) live: crate::live_settings::SettingsHandle,
    pub(crate) tool_execution: ToolExecutionMode,
    pub(crate) run: Arc<RunLifecycle>,
    pub(crate) error_message: Option<String>,
}

impl AgentState {
    /// pi's `createMutableAgentState` (`agent.ts:87`): `isStreaming` false,
    /// `streamingMessage` and `errorMessage` undefined, `pendingToolCalls` an
    /// empty set, `thinkingLevel` `"off"`.
    pub(crate) fn new(
        messages: Vec<AgentMessage>,
        tools: Vec<ToolSpec>,
        model: Model,
    ) -> AgentState {
        AgentState {
            messages,
            tools,
            model,
            live: crate::live_settings::SettingsHandle::new(),
            tool_execution: ToolExecutionMode::Parallel,
            run: Arc::new(RunLifecycle::default()),
            error_message: None,
        }
    }

    /// `Agent.state.systemPrompt` (`types.ts:389`): the prompt replayed from
    /// the transcript's system messages, so it already includes every sectioned
    /// prompt patch. pi exposes it as a getter over `messages`.
    pub fn system_prompt(&self) -> String {
        crate::transcript::current_system_prompt(&self.messages)
    }

    /// `Agent.state.messages` (`types.ts:407`).
    pub fn messages(&self) -> &[AgentMessage] {
        &self.messages
    }

    /// `Agent.setState`'s message half (`agent.ts:465-467`): replace the
    /// transcript without running the loop.
    pub fn set_messages(&mut self, messages: Vec<AgentMessage>) {
        self.messages = messages;
    }

    /// `Agent.state.tools` (`types.ts:400`).
    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    /// `Agent.state.model` (`types.ts:391`).
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Set `Agent.state.model` for the next request.
    pub fn set_model(&mut self, model: Model) {
        self.model = model;
    }

    /// `Agent.state.thinkingLevel` (`types.ts:393`). Read through the handle,
    /// so this is the level the **next provider request** will use - including
    /// a request inside a run that is already in flight.
    pub fn thinking_level(&self) -> ThinkingLevel {
        self.live.thinking_level()
    }

    /// Set `Agent.state.thinkingLevel` through the live cell, so a run in
    /// flight reads it on its next request.
    pub fn set_thinking_level(&mut self, level: ThinkingLevel) {
        self.live.set_thinking_level(level);
    }

    /// The handle a caller writes the level (and a mid-run model switch)
    /// through while a run holds the agent. Same cell this state's setters
    /// write, so the two paths never disagree.
    pub fn live_settings(&self) -> crate::live_settings::SettingsHandle {
        self.live.clone()
    }

    /// `Agent.toolExecution` (`agent.ts:234`), read into the loop config for
    /// the next turn. pi keeps this on the agent rather than on `_state`; it
    /// lives here because that is where this port keeps it, and it is the same
    /// per-turn setting either way.
    pub fn tool_execution(&self) -> ToolExecutionMode {
        self.tool_execution
    }

    /// Set `Agent.toolExecution`, read into the loop config for the next turn.
    pub fn set_tool_execution(&mut self, mode: ToolExecutionMode) {
        self.tool_execution = mode;
    }

    /// `Agent.state.isStreaming` (`types.ts:416`): true from the start of a run
    /// until `finish_run`, whatever ended it.
    pub fn is_streaming(&self) -> bool {
        self.run.is_streaming.load(Ordering::SeqCst)
    }

    /// pi's `Agent.state.streamingMessage` (`types.ts:419`). Cloned out rather
    /// than borrowed: the field lives behind the run lifecycle box that
    /// [`RunGuard`] finishes, so there is no borrow to hand out.
    pub fn streaming_message(&self) -> Option<AssistantMessage> {
        lock(&self.run.streaming_message).clone()
    }

    /// `Agent.state.pendingToolCalls` (`types.ts:421`). Cloned for the same
    /// reason as [`AgentState::streaming_message`].
    pub fn pending_tool_calls(&self) -> BTreeSet<String> {
        lock(&self.run.pending_tool_calls).clone()
    }

    /// `Agent.state.errorMessage` (`types.ts:423`): the error from the most
    /// recent failed or aborted assistant turn.
    pub fn error_message(&self) -> Option<&str> {
        self.error_message.as_deref()
    }

    /// pi's `createContextSnapshot` (`agent.ts:453-459`): the messages and
    /// tools a run starts from. A copy, so the transcript can keep growing
    /// under the run while the loop works on the snapshot.
    pub(crate) fn context_snapshot(&self) -> AgentContext {
        AgentContext {
            messages: self.messages.clone(),
            tools: self.tools.clone(),
        }
    }

    /// The prologue of `runWithLifecycle` (`agent.ts:517-519`):
    /// `isStreaming = true; streamingMessage = undefined;
    /// errorMessage = undefined`. Returns the guard that finishes the run if
    /// the future is dropped instead of returning.
    pub(crate) fn begin_run(&mut self) -> RunGuard {
        self.error_message = None;
        RunGuard::new(Arc::clone(&self.run))
    }

    /// `finishRun()` (`agent.ts:550-553`): the agent is idle once the loop has
    /// returned; tool calls still marked pending were cancelled.
    ///
    /// Every exit path out of a run must reach this - see [`RunGuard`].
    pub(crate) fn finish_run(&mut self) {
        self.run.finish();
    }

    /// pi's `processEvents` state reduction (`agent.ts:570-608`), without the
    /// listener fan-out that follows it in the same method. One arm per event
    /// type, same switch, same order.
    pub(crate) fn apply_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageStart {
                message: AgentMessage::Assistant(assistant),
            } => {
                *lock(&self.run.streaming_message) = Some(assistant.clone());
            }
            AgentEvent::MessageUpdate { message, .. } => {
                *lock(&self.run.streaming_message) = Some((**message).clone());
            }
            AgentEvent::MessageEnd { message } => {
                lock(&self.run.streaming_message).take();
                self.messages.push(message.clone());
            }
            AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                lock(&self.run.pending_tool_calls).insert(tool_call_id.clone());
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                lock(&self.run.pending_tool_calls).remove(tool_call_id);
            }
            AgentEvent::TurnEnd {
                message: AgentMessage::Assistant(assistant),
                ..
            } => {
                if let Some(error) = &assistant.error_message {
                    self.error_message = Some(error.clone());
                }
            }
            AgentEvent::AgentEnd { .. } => {
                lock(&self.run.streaming_message).take();
            }
            _ => {}
        }
    }

    /// `Agent.reset()`'s state half (`agent.ts:355-366`): keep the replayed
    /// system prompt, drop everything else.
    ///
    /// pi throws when a run is active; this port cannot fail a `&mut self`
    /// call that way without changing every caller, so it reports `false` and
    /// changes nothing instead. `Agent::reset` then leaves the queues alone,
    /// which is the same outcome as pi's throw.
    pub(crate) fn reset(&mut self) -> bool {
        if self.is_streaming() {
            return false;
        }
        let baseline = crate::transcript::current_system_message(&self.messages);
        self.messages = baseline.into_iter().map(AgentMessage::System).collect();
        self.run.finish();
        self.error_message = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::types::AssistantContent;

    fn state() -> AgentState {
        AgentState::new(Vec::new(), Vec::new(), Model::default())
    }

    fn assistant() -> AssistantMessage {
        crate::agent_loop::empty_assistant_message()
    }

    fn message(assistant: AssistantMessage) -> AgentMessage {
        AgentMessage::Assistant(assistant)
    }

    /// One arm per event type, in pi's order: the partial assistant message is
    /// live between `message_start` and `message_end`, the transcript grows on
    /// `message_end` only, pending tool calls come and go with their pair, and
    /// `turn_end` is where an assistant error lands.
    #[test]
    fn apply_event_reduces_every_arm() {
        let mut state = state();
        let update: AssistantMessage = AssistantMessage {
            content: vec![AssistantContent::Text(pi_ai::types::TextContent {
                text: "hi".to_string(),
                text_signature: None,
            })],
            ..assistant()
        };

        state.apply_event(&AgentEvent::MessageStart {
            message: message(assistant()),
        });
        assert!(
            state.streaming_message().is_some(),
            "message_start publishes the streaming message"
        );

        state.apply_event(&AgentEvent::MessageUpdate {
            message: std::sync::Arc::new(update.clone()),
            assistant_message_event: pi_ai::types::AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "hi".to_string(),
                partial: update.clone(),
            },
        });
        assert!(
            state.streaming_message().is_some(),
            "message_update keeps it live"
        );

        state.apply_event(&AgentEvent::MessageEnd {
            message: message(assistant()),
        });
        assert_eq!(state.streaming_message(), None);
        assert_eq!(state.messages().len(), 1, "message_end appends the message");

        state.apply_event(&AgentEvent::ToolExecutionStart {
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            args: serde_json::Map::new(),
            parent_tool_call_id: None,
        });
        assert_eq!(
            state.pending_tool_calls(),
            BTreeSet::from(["call-1".to_string()]),
            "tool_execution_start marks the call pending"
        );

        state.apply_event(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            result: crate::types::AgentToolResult::default(),
            is_error: false,
            duration_ms: None,
            parent_tool_call_id: None,
        });
        assert!(
            state.pending_tool_calls().is_empty(),
            "tool_execution_end clears it again"
        );

        state.apply_event(&AgentEvent::TurnEnd {
            message: message(AssistantMessage {
                error_message: Some("boom".to_string()),
                ..assistant()
            }),
            tool_results: Vec::new(),
        });
        assert_eq!(state.error_message(), Some("boom"));

        state.apply_event(&AgentEvent::AgentEnd {
            messages: Vec::new(),
            will_retry: false,
        });
        assert_eq!(state.streaming_message(), None);
    }

    /// `runWithLifecycle`'s prologue and `finishRun` (`agent.ts:517-519`,
    /// `:550-553`).
    #[test]
    fn begin_run_arms_and_finish_run_disarms() {
        let mut state = state();
        let mut failing = assistant();
        failing.error_message = Some("from the previous turn".to_string());
        state.apply_event(&AgentEvent::TurnEnd {
            message: message(failing),
            tool_results: Vec::new(),
        });

        let mut guard = state.begin_run();
        assert!(state.is_streaming());
        assert_eq!(
            state.error_message(),
            None,
            "a new run clears the previous turn's error"
        );

        state.finish_run();
        guard.disarm();
        assert!(!state.is_streaming(), "the run is over");
    }

    /// The dropped-future case, at the level it is actually implemented: the
    /// guard is armed by `begin_run` and its `Drop` is `finishRun`'s half
    /// (audit-9 D5).
    #[test]
    fn a_dropped_run_guard_finishes_the_run() {
        let mut state = state();
        let guard = state.begin_run();
        assert!(state.is_streaming());
        drop(guard);
        assert!(
            !state.is_streaming(),
            "a guard dropped while armed leaves the agent idle"
        );
    }

    /// pi's `reset` truncates the transcript to its system baseline and clears
    /// the runtime fields; it throws while a run is active, which here is a
    /// `false` return and no change at all.
    #[test]
    fn reset_keeps_the_baseline_and_is_refused_while_streaming() {
        let mut state = state();
        state.apply_event(&AgentEvent::MessageEnd {
            message: message(assistant()),
        });

        let run = state.begin_run();
        assert!(
            !state.reset(),
            "a reset that would race a run changes nothing"
        );
        assert_eq!(
            state.messages().len(),
            1,
            "the transcript still holds the message the refused reset kept"
        );

        // Dropping the guard runs finishRun, so the reset is accepted now.
        drop(run);
        assert!(state.reset());
        assert!(
            state.messages().is_empty(),
            "no system message means no baseline to keep: {:?}",
            state.messages()
        );
        assert_eq!(state.error_message(), None);
    }

    /// The baseline that survives a reset is the current system message.
    #[test]
    fn reset_keeps_the_system_message() {
        let mut state = state();
        let system = AgentMessage::System(
            crate::transcript::create_initial_system_message(Some("Be terse."), &[])
                .expect("a prompt yields a system message"),
        );
        state.set_messages(vec![system]);
        state.apply_event(&AgentEvent::MessageEnd {
            message: message(assistant()),
        });

        assert!(state.reset());
        assert_eq!(state.messages().len(), 1);
        assert_eq!(state.system_prompt(), "Be terse.");
    }
}
