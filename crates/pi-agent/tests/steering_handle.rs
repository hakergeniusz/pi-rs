//! The out-of-band steering handle: `agent.steer(...)` / `agent.followUp(...)`
//! reachable while a run is in flight.
//!
//! pi's two enqueues are plain methods on the agent object
//! (`agent.ts:299-306`), and the loop polls the queues between events
//! (`agent.ts:500-503`), so any caller can queue mid-run - that is how
//! `--mode rpc` delivers `steer` / `followUp` while a prompt streams. In Rust
//! the run future holds `&mut Agent`, so the handle is the reachability.

mod common;

use std::sync::Arc;
use std::time::Duration;

use std::sync::atomic::{AtomicBool, Ordering};

use common::*;
use pi_agent::{Agent, AgentEvent, QueueHandle, QueueMode};

fn agent_with(
    tools: Arc<ScriptedTools>,
    responses: Vec<Vec<pi_ai::types::AssistantMessageEvent>>,
) -> (Agent, Arc<ScriptedStream>) {
    let stream = ScriptedStream::new(responses);
    let agent = Agent::new(
        test_model(),
        Some("You are terse."),
        tools,
        Arc::clone(&stream) as Arc<dyn pi_agent::StreamFn>,
    );
    (agent, stream)
}

/// Every message the provider was asked about, in request order.
fn request_messages(stream: &ScriptedStream, request: usize) -> Vec<String> {
    let requests = stream.requests.lock().expect("requests");
    summarize(&requests[request].messages)
}

fn count(events: &[AgentEvent], kind: &str) -> usize {
    pi_agent::event_kinds(events)
        .iter()
        .filter(|seen| **seen == kind)
        .count()
}

/// Steer and follow-up both enqueue with no result, like pi: `Agent.steer` and
/// `Agent.followUp` are `void` (`agent.ts:299-306`). The `"handled" |
/// "queued"` disposition a host reports is `AgentSession`'s, not the agent's
/// (`agent-session.ts:294`, `:2129-2157`), so there is nothing here to return.
#[test]
fn the_enqueue_returns_nothing() {
    let handle = QueueHandle::new();
    let steered: () = handle.steer([user("steered")]);
    let followed: () = handle.follow_up([user("followed")]);
    assert_eq!(steered, (), "pi's steer returns void");
    assert_eq!(followed, (), "pi's followUp returns void");
}

/// The handle and the agent are the same queues, not a copy: an enqueue made
/// through one is visible through the other, at pi's own read points.
#[test]
fn the_handle_and_the_agent_share_one_pair_of_queues() {
    let tools = ScriptedTools::new();
    let (mut agent, _stream) = agent_with(tools, vec![]);
    let handle = agent.queue_handle();

    handle.steer([user("steered")]);
    handle.follow_up([user("followed")]);
    assert!(agent.has_queued_messages());
    assert_eq!(
        summarize(&agent.peek_queued_messages()),
        vec!["user[steered]"],
        "pi's peek previews the steering queue first (`agent.ts:330-332`)"
    );

    handle.clear_steering();
    assert_eq!(
        summarize(&agent.peek_queued_messages()),
        vec!["user[followed]"],
        "steering cleared, the follow-up surfaces"
    );

    handle.set_follow_up_mode(QueueMode::All);
    handle.follow_up([user("second")]);
    assert_eq!(
        summarize(&agent.peek_queued_messages()),
        vec!["user[followed]", "user[second]"],
        "the mode setter reached the agent's queue too"
    );

    agent.clear_all_queues();
    assert!(
        !handle.has_items(),
        "and the agent's clears reached the handle"
    );
}

/// A steering message queued while the run is in flight is delivered by THAT
/// run, before its next model call: the loop drains the steering queue at
/// `agent-loop.ts`'s post-turn poll (`agent_loop.rs:498`), which is the point
/// pi's `getSteeringMessages` hook sits (`agent.ts:500-502`).
#[tokio::test]
async fn a_steer_queued_mid_run_reaches_the_run_that_is_already_going() {
    let tools = ScriptedTools::new();
    let (mut agent, stream) = agent_with(
        tools,
        vec![
            ScriptedStream::text_reply("one"),
            ScriptedStream::text_reply("two"),
        ],
    );
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    // The listener runs inside the run, on the run's thread, with the agent
    // mutably borrowed by the future above it - which is exactly the seam the
    // handle exists for.
    let handle = agent.queue_handle();
    let steer_at_turn_end = handle.clone();
    let steered_once = AtomicBool::new(false);
    agent.subscribe(move |event: &AgentEvent, _signal| {
        if matches!(event, AgentEvent::TurnEnd { .. }) && !steered_once.swap(true, Ordering::SeqCst)
        {
            steer_at_turn_end.steer([user("mid-run steer")]);
        }
    });
    let sink = Arc::clone(&events);
    agent.subscribe(move |event: &AgentEvent, _signal| {
        sink.lock().expect("events").push(event.clone());
    });

    agent.prompt_text("hi").await.expect("prompt runs");

    // The steer is in the SECOND request, i.e. it was consumed before that
    // model call and not left for a later prompt.
    assert!(
        request_messages(&stream, 1)
            .iter()
            .any(|message| message == "user[mid-run steer]"),
        "second request: {:?}",
        request_messages(&stream, 1)
    );
    assert_eq!(
        summarize(agent.messages()),
        vec![
            "system[You are terse.]()",
            "user[hi]",
            "assistant[one]",
            "user[mid-run steer]",
            "assistant[two]",
        ],
        "one run, one agent_end, the steer inside it"
    );
    let events = events.lock().expect("events").clone();
    assert_eq!(count(&events, "agent_end"), 1);
    assert!(!handle.has_items(), "the run consumed the queue");
}

/// A follow-up queued mid-run is NOT delivered before the next model call: the
/// loop polls the follow-up queue only where the agent would otherwise stop
/// (`agent_loop.rs:505`, the end-of-loop poll behind pi's `getFollowUpMessages`
/// hook, `agent.ts:503`), so it becomes a turn of its own inside the same run.
#[tokio::test]
async fn a_follow_up_queued_mid_run_waits_for_the_next_turn() {
    let tools = ScriptedTools::with_tool("bash");
    let (mut agent, stream) = agent_with(
        tools,
        vec![
            // Turn one keeps going: a tool call, so the agent has not stopped
            // when the follow-up is queued.
            ScriptedStream::tool_call_reply(&[("call_1", "bash", serde_json::Map::new())]),
            ScriptedStream::text_reply("one"),
            ScriptedStream::text_reply("two"),
        ],
    );
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    let handle = agent.queue_handle();
    let queue_at_turn_end = handle.clone();
    let queued_once = AtomicBool::new(false);
    agent.subscribe(move |event: &AgentEvent, _signal| {
        if matches!(event, AgentEvent::TurnEnd { .. }) && !queued_once.swap(true, Ordering::SeqCst)
        {
            queue_at_turn_end.follow_up([user("mid-run follow up")]);
        }
    });
    let sink = Arc::clone(&events);
    agent.subscribe(move |event: &AgentEvent, _signal| {
        sink.lock().expect("events").push(event.clone());
    });

    agent.prompt_text("hi").await.expect("prompt runs");

    assert_eq!(
        stream.request_count(),
        3,
        "the follow-up made a turn of its own"
    );
    assert!(
        !request_messages(&stream, 1)
            .iter()
            .any(|message| message == "user[mid-run follow up]"),
        "the turn that was already running did not get it injected: {:?}",
        request_messages(&stream, 1)
    );
    assert!(
        request_messages(&stream, 2)
            .iter()
            .any(|message| message == "user[mid-run follow up]"),
        "the turn after the agent would have stopped does: {:?}",
        request_messages(&stream, 2)
    );
    let events = events.lock().expect("events").clone();
    assert_eq!(
        count(&events, "agent_end"),
        1,
        "pi extends the run, it does not start another one"
    );
    assert!(!handle.has_items(), "the run consumed the queue");
}

/// pi's queues belong to the agent, not to a run (`agent.ts:191-192`), so a
/// run that never finishes - here a dropped future - leaves the enqueues
/// standing for the next one. pi cannot drop a run at all; the port can, and
/// must not lose the queue with it.
#[tokio::test]
async fn a_dropped_run_keeps_what_was_queued_against_it() {
    let tools = ScriptedTools::with_tool("bash");
    // The run gets as far as the tool call and then stalls there for good, so
    // the future below is in flight and cannot be finished.
    tools.stall_next_call();
    let mut agent = Agent::new(
        test_model(),
        Some("You are terse."),
        tools,
        StallingStream::new(),
    );
    let handle = agent.queue_handle();

    // Another thread, mid-run, while the live future owns `&mut Agent`: the
    // handle shares the queues and nothing else, so this needs no agent.
    let (mid_run_tx, mid_run_rx) = std::sync::mpsc::channel::<()>();
    let from_elsewhere = handle.clone();
    let enqueuer = std::thread::spawn(move || {
        mid_run_rx.recv().expect("the run to reach its first turn");
        from_elsewhere.steer([user("steered while it ran")]);
        from_elsewhere.follow_up([user("follow-up while it ran")]);
    });
    // `ToolExecutionStart` is the last event before the run stalls inside the
    // tool, so the enqueues are made while the run is in flight.
    let signal = mid_run_tx.clone();
    agent.subscribe(move |event: &AgentEvent, _signal| {
        if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
            let _ = signal.send(());
        }
    });

    let dropped = tokio::time::timeout(Duration::from_millis(50), agent.prompt_text("hi")).await;
    enqueuer.join().expect("the enqueue thread");
    assert!(
        dropped.is_err(),
        "the run was still in flight: {dropped:?} / {:?}",
        summarize(agent.messages())
    );
    assert!(
        agent.has_queued_messages(),
        "the enqueues landed while the run held `&mut Agent`, and the run's \
         own post-turn steering poll never reached them"
    );

    agent.prompt_text("again").await.expect("the next run");
    let transcript = summarize(agent.messages());
    let steered = transcript
        .iter()
        .position(|message| message == "user[steered while it ran]")
        .expect("the steer survived the drop");
    let answered = transcript
        .iter()
        .position(|message| message == "assistant[after the drop]")
        .expect("the next run answered");
    let followed = transcript
        .iter()
        .position(|message| message == "user[follow-up while it ran]")
        .expect("the follow-up survived the drop");
    assert!(
        steered < answered,
        "and the steer was consumed by the next run: {transcript:?}"
    );
    assert!(
        answered < followed,
        "the follow-up still waited for the point the agent would otherwise \
         stop, which is a turn of its own: {transcript:?}"
    );
    assert!(
        !agent.has_queued_messages(),
        "both queues drained: {transcript:?}"
    );
}
