//! The state object as pi exposes it: `Agent.state` read through
//! [`pi_agent::Agent::state`], and the two invariants that live in it.
//!
//! The unit tests next to `AgentState` and `Queues` cover the transitions
//! directly; these cover what a host can see of them through the agent, which
//! is the only view `pi-cli` and `pi-ext` have.

mod common;

use std::time::Duration;

use common::*;
use pi_agent::{Agent, QueueMode, ThinkingLevel};

/// `Agent.state` is the one object pi exposes for reads (`agent.ts:280-282`),
/// and every field on it is reachable through it.
#[tokio::test]
async fn the_state_object_reads_like_pis() {
    let tools = ScriptedTools::with_tool("bash");
    let agent = Agent::new(
        test_model(),
        Some("You are terse."),
        tools.clone(),
        ScriptedStream::new(vec![ScriptedStream::text_reply("hello")]),
    )
    .with_thinking_level(ThinkingLevel::High);

    let state = agent.state();
    assert_eq!(state.system_prompt(), "You are terse.");
    assert_eq!(
        state.messages().len(),
        1,
        "system, user and assistant follow"
    );
    assert_eq!(state.tools().len(), 1);
    assert_eq!(state.model().id, "test-model");
    assert_eq!(state.thinking_level(), ThinkingLevel::High);
    assert_eq!(
        state.tool_execution(),
        pi_agent::ToolExecutionMode::Parallel
    );
    assert!(!state.is_streaming(), "no run has started yet");
    assert_eq!(state.streaming_message(), None);
    assert!(state.pending_tool_calls().is_empty());
    assert_eq!(state.error_message(), None);
    assert_eq!(
        state.messages().len(),
        agent.messages().len(),
        "the state object and the agent accessor are the same data"
    );
}

/// The transcript the loop grows during a run is the state's, and the run's
/// half-streamed message is live in the state while it streams.
#[tokio::test]
async fn a_run_reduces_into_the_state_object() {
    let tools = ScriptedTools::with_tool("bash");
    let mut agent = Agent::new(
        test_model(),
        Some("You are terse."),
        tools.clone(),
        ScriptedStream::new(vec![ScriptedStream::tool_call_reply(&[(
            "bash",
            "bash",
            serde_json::Map::new(),
        )])]),
    );
    tools.push_result(Ok(text_result("done")));

    agent.prompt_text("hi").await.expect("prompt runs");

    assert_eq!(
        agent.state().messages().len(),
        5,
        "system, user, assistant, tool"
    );
    assert_eq!(
        agent.state().streaming_message(),
        None,
        "`finishRun` cleared the partial message"
    );
    assert!(
        agent.state().pending_tool_calls().is_empty(),
        "the tool call finished, so nothing is pending"
    );
    assert!(!agent.state().is_streaming());
}

/// A dropped prompt future still finishes the run, and the state object says
/// so: pi's `finally { this.finishRun() }` (`agent.ts:526-528`) has no
/// equivalent here, so the guard armed by `begin_run` runs it on `Drop`
/// (audit-9 D5).
#[tokio::test]
async fn a_dropped_run_leaves_the_state_idle() {
    let tools = ScriptedTools::with_tool("bash");
    tools.stall_next_call();
    let mut agent = Agent::new(
        test_model(),
        Some("You are terse."),
        tools,
        StallingStream::new(),
    );

    {
        let run = agent.prompt_text("hi");
        tokio::pin!(run);
        let outcome = tokio::time::timeout(Duration::from_millis(50), run).await;
        assert!(outcome.is_err(), "the run was still in flight");
        // The future is dropped here, with the tool call pending.
    }

    assert!(!agent.state().is_streaming(), "the guard finished the run");
    assert!(
        agent.state().pending_tool_calls().is_empty(),
        "the cancelled tool call was cleared"
    );
    assert_eq!(agent.state().streaming_message(), None);
    assert!(
        summarize(agent.messages())
            .iter()
            .any(|line| line.contains("hi")),
        "the dropped run's user message stayed in the transcript, as pi's would: {:?}",
        summarize(agent.messages())
    );
}

/// Queues live beside the state, not in it, and a reset clears both - in pi's
/// order, follow-up then steering (`agent.ts:364-365`).
#[tokio::test]
async fn reset_clears_the_queues_as_well_as_the_transcript() {
    let tools = ScriptedTools::with_tool("bash");
    let mut agent = Agent::new(
        test_model(),
        Some("You are terse."),
        tools,
        ScriptedStream::new(vec![ScriptedStream::text_reply("hello")]),
    );
    agent.prompt_text("hi").await.expect("prompt runs");
    agent.set_queue_modes(QueueMode::All, QueueMode::All);
    agent.steer([pi_agent::transcript::user_message("steer me", 0)]);
    agent.follow_up([pi_agent::transcript::user_message("follow me", 0)]);
    assert!(agent.has_queued_messages());

    agent.reset();

    assert!(!agent.has_queued_messages(), "both queues were cleared");
    assert_eq!(agent.peek_queued_messages(), Vec::new());
    assert_eq!(
        agent.state().messages().len(),
        1,
        "only the system baseline survives: {:?}",
        summarize(agent.messages())
    );
    assert_eq!(agent.state().error_message(), None);
}
