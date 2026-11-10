//! The live-settings handle: `/thinking` and `/model` applied mid-run.
//!
//! pi's `handleThinkingCommand` (`interactive-mode.ts:5066-5081`) writes the
//! level on the SESSION (`agent-session.ts:2565`), and pi's loop re-reads the
//! session's state on every turn - `model: this.agent.state.model,
//! thinkingLevel: this.agent.state.thinkingLevel` (`agent-session.ts:899-900`)
//! applied at `agent-loop.ts:186-199` - so the next LLM call in the run that is
//! already streaming uses it. `/model` is the same shape (`setModel`,
//! `agent-session.ts:2430-2437`). In Rust the run future owns `&mut Agent`, so
//! [`SettingsHandle`] is that reachability, and the loop reads it per turn.

mod common;

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use common::*;
use pi_agent::types::{AgentToolCall, AgentToolResult, BoxFuture, ToolExecutor, ToolSpec};
use pi_agent::{Agent, AgentEvent, SettingsHandle, ThinkingLevel};

/// A tool that stays inside the run long enough for another thread to act while
/// the future owns `&mut Agent`. pi's equivalent seam is any tool awaiting I/O.
struct SlowTool {
    inner: Arc<ScriptedTools>,
    delay: Duration,
}

impl ToolExecutor for SlowTool {
    fn tools(&self) -> Vec<ToolSpec> {
        self.inner.tools()
    }

    fn execution_mode(&self, name: &str) -> Option<pi_agent::ToolExecutionMode> {
        self.inner.execution_mode(name)
    }

    fn execute(
        &self,
        call: AgentToolCall,
        updates: tokio::sync::mpsc::UnboundedSender<AgentToolResult>,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        let inner = Arc::clone(&self.inner);
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            inner.execute(call, updates).await
        })
    }
}

fn slow_tool_agent(
    responses: Vec<Vec<pi_ai::types::AssistantMessageEvent>>,
) -> (Agent, Arc<ScriptedStream>) {
    let tools = ScriptedTools::with_tool("bash");
    let stream = ScriptedStream::new(responses);
    let agent = Agent::new(
        test_model(),
        Some("You are terse."),
        Arc::new(SlowTool {
            inner: tools,
            delay: Duration::from_millis(60),
        }),
        Arc::clone(&stream) as Arc<dyn pi_agent::StreamFn>,
    );
    (agent, stream)
}

fn requested_level(stream: &ScriptedStream, request: usize) -> ThinkingLevel {
    let requests = stream.requests.lock().expect("requests");
    requests[request].thinking_level
}

fn requested_model(stream: &ScriptedStream, request: usize) -> String {
    let requests = stream.requests.lock().expect("requests");
    requests[request].model.id.clone()
}

fn count(events: &[AgentEvent], kind: &str) -> usize {
    pi_agent::event_kinds(events)
        .iter()
        .filter(|seen| **seen == kind)
        .count()
}

/// The handle and the agent write the same cell, so a level set through
/// `&mut Agent` is what an out-of-band caller reads, and the other way round.
/// pi has one field (`state.thinkingLevel`, `types.ts:393`) and both paths
/// reach it; this is that field, shared.
#[test]
fn the_handle_and_the_agent_are_one_level() {
    let (mut agent, _stream) = slow_tool_agent(vec![]);
    let handle = agent.live_settings();

    agent.set_thinking_level(ThinkingLevel::Medium);
    assert_eq!(
        handle.thinking_level(),
        ThinkingLevel::Medium,
        "the `&mut self` setter reached the handle"
    );

    handle.set_thinking_level(ThinkingLevel::High);
    assert_eq!(
        agent.thinking_level(),
        ThinkingLevel::High,
        "and the handle reached the agent"
    );

    // The builder is the same write (pi: `Agent.create({ thinkingLevel })`).
    let rebuilt = Agent::new(
        test_model(),
        None,
        ScriptedTools::new(),
        ScriptedStream::new(vec![]),
    )
    .with_thinking_level(ThinkingLevel::Low);
    assert_eq!(rebuilt.thinking_level(), ThinkingLevel::Low);
    assert_eq!(
        rebuilt.live_settings().thinking_level(),
        ThinkingLevel::Low,
        "each agent has its own level: pi gives every `Agent.create` a fresh \
         `MutableAgentState` (`agent.ts:87`)"
    );
}

/// pi's `/thinking` mid-run: the level the run's NEXT provider request carries
/// is the one just typed. The writer is another thread, acting while the live
/// future owns `&mut Agent` - the seam the handle exists for.
#[tokio::test]
async fn a_level_set_mid_run_lands_on_the_next_request_of_that_run() {
    let (mut agent, stream) = slow_tool_agent(vec![
        ScriptedStream::tool_call_reply(&[("call_1", "bash", serde_json::Map::new())]),
        ScriptedStream::text_reply("two"),
    ]);
    assert_eq!(agent.thinking_level(), ThinkingLevel::Off);

    // A run that answers, calls one tool, and answers again: three requests
    // would be a third turn, so two.
    let handle = agent.live_settings();
    let (mid_run_tx, mid_run_rx) = mpsc::channel::<()>();
    let signal = mid_run_tx.clone();
    agent.subscribe(move |event: &AgentEvent, _signal| {
        // `ToolExecutionStart` is inside the run, before the tool resolves.
        if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
            let _ = signal.send(());
        }
    });
    let writer = std::thread::spawn(move || {
        mid_run_rx.recv().expect("the run to reach its tool");
        handle.set_thinking_level(ThinkingLevel::High);
    });

    agent.prompt_text("hi").await.expect("prompt runs");
    writer.join().expect("the writer thread");

    assert_eq!(
        requested_level(&stream, 0),
        ThinkingLevel::Off,
        "the request already in flight when the level was typed keeps the old one"
    );
    assert_eq!(
        requested_level(&stream, 1),
        ThinkingLevel::High,
        "the SECOND provider request of the SAME run carries the new level"
    );
    assert_eq!(stream.request_count(), 2, "one run, two requests");
}

/// The run is still one run: one `agent_end`, one transcript, no second
/// `prompt`. pi extends the loop it is in (`agent-loop.ts:186-199` rewrites
/// `config` mid-run), it does not start another.
#[tokio::test]
async fn a_level_set_mid_run_keeps_the_run_it_landed_in() {
    let (mut agent, stream) = slow_tool_agent(vec![
        ScriptedStream::tool_call_reply(&[("call_1", "bash", serde_json::Map::new())]),
        ScriptedStream::text_reply("two"),
    ]);
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    let handle = agent.live_settings();
    let typed = handle.clone();
    let once = std::sync::atomic::AtomicBool::new(false);
    agent.subscribe(move |event: &AgentEvent, _signal| {
        if matches!(event, AgentEvent::ToolExecutionStart { .. })
            && !once.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            typed.set_thinking_level(ThinkingLevel::Xhigh);
        }
    });
    let sink = Arc::clone(&events);
    agent.subscribe(move |event: &AgentEvent, _signal| {
        sink.lock().expect("events").push(event.clone());
    });

    agent.prompt_text("hi").await.expect("prompt runs");

    let events = events.lock().expect("events").clone();
    assert_eq!(count(&events, "agent_end"), 1, "still one agent_end");
    assert_eq!(
        count(&events, "turn_start"),
        2,
        "the tool made a second turn"
    );
    assert_eq!(
        summarize(agent.messages()),
        vec![
            "system[You are terse.](bash)",
            "user[hi]",
            "assistant[]",
            "toolResult[bash=ran bash]",
            "assistant[two]",
        ],
        "one transcript, one run"
    );
    assert_eq!(requested_level(&stream, 1), ThinkingLevel::Xhigh);
}

/// pi's `/model` mid-run: `session.setModel(model)` writes `agent.state.model`
/// (`agent-session.ts:2437`) and the loop picks it up on its next turn, so the
/// rest of the run streams from the new model. The port cannot write
/// `state.model` while a run holds the agent - the handle records it and the
/// loop applies it, which is the same request stream.
#[tokio::test]
async fn a_model_set_mid_run_lands_on_the_next_request_of_that_run() {
    let (mut agent, stream) = slow_tool_agent(vec![
        ScriptedStream::tool_call_reply(&[("call_1", "bash", serde_json::Map::new())]),
        ScriptedStream::text_reply("two"),
    ]);
    let before = agent.model().id.clone();

    let handle = agent.live_settings();
    let (mid_run_tx, mid_run_rx) = mpsc::channel::<()>();
    let signal = mid_run_tx.clone();
    agent.subscribe(move |event: &AgentEvent, _signal| {
        if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
            let _ = signal.send(());
        }
    });
    let writer = std::thread::spawn(move || {
        mid_run_rx.recv().expect("the run to reach its tool");
        let mut switched = test_model();
        switched.id = "switched-model".to_string();
        handle.set_model(switched);
    });

    agent.prompt_text("hi").await.expect("prompt runs");
    writer.join().expect("the writer thread");

    assert_eq!(requested_model(&stream, 0), before, "the request in flight");
    assert_eq!(
        requested_model(&stream, 1),
        "switched-model",
        "the SECOND request of the same run uses the switched model"
    );
    assert_eq!(
        agent.model().id,
        "switched-model",
        "and the agent's own model caught up when the run settled"
    );
}

/// An explicit `&mut self` set wins over a pending mid-run write: pi writes
/// `state.model` at once, so there is never a second copy to reconcile.
#[test]
fn an_explicit_set_replaces_a_pending_model() {
    let (mut agent, _stream) = slow_tool_agent(vec![]);
    let handle = agent.live_settings();
    let mut switched = test_model();
    switched.id = "switched-model".to_string();
    handle.set_model(switched);
    assert_eq!(
        handle.pending_model().map(|model| model.id),
        Some("switched-model".to_string())
    );

    let mut explicit = test_model();
    explicit.id = "explicit-model".to_string();
    agent.set_model(explicit.clone());
    assert_eq!(
        handle.pending_model(),
        None,
        "the explicit write is the only one left"
    );
    agent.set_model_and_thinking(explicit, ThinkingLevel::Low);
    assert_eq!(handle.thinking_level(), ThinkingLevel::Low);
    assert_eq!(handle.pending_model(), None);
}

/// The handle is `Send + Sync` and cloneable, which is what lets the CLI hold
/// one for the whole session; pi needs no such statement because the session
/// object is itself shared.
#[test]
fn the_handle_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync + Clone>() {}
    assert_send_sync::<SettingsHandle>();
}
