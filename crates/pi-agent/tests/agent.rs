//! The `Agent` facade, ported from `packages/agent/test/agent.test.ts`.

mod common;

use std::sync::Arc;

use common::*;
use pi_agent::agent_message::AgentMessage;
use pi_agent::{Agent, AgentEvent, QueueMode, ThinkingLevel, ToolExecutionMode};
use pi_ai::types::UserContent;

fn agent_with(
    tools: Arc<ScriptedTools>,
    responses: Vec<Vec<pi_ai::types::AssistantMessageEvent>>,
) -> Agent {
    let stream = ScriptedStream::new(responses);
    Agent::new(test_model(), Some("You are terse."), tools, stream)
}

#[tokio::test]
async fn converts_the_prompt_and_tools_into_transcript_state() {
    let tools = ScriptedTools::with_tool("bash");
    let agent = agent_with(tools, vec![]);

    assert_eq!(
        summarize(agent.messages()),
        vec!["system[You are terse.](bash)"],
        "the system message carries both the prompt and the tool declarations"
    );
    assert_eq!(agent.tools().len(), 1);
    assert!(!agent.is_streaming());
    assert_eq!(agent.error_message(), None);
}

#[tokio::test]
async fn prompt_appends_the_turn_to_the_transcript() {
    let tools = ScriptedTools::with_tool("bash");
    let mut agent = agent_with(tools, vec![ScriptedStream::text_reply("hello")]);
    let recorder = Recorder::new();
    recorder.attach(&mut agent);

    agent.prompt_text("hi").await.expect("prompt runs");

    assert_eq!(
        summarize(agent.messages()),
        vec![
            "system[You are terse.](bash)",
            "user[hi]",
            "assistant[hello]"
        ]
    );
    assert!(!agent.is_streaming());
    assert!(agent.pending_tool_calls().is_empty());
}

#[tokio::test]
async fn listeners_observe_every_event_in_order() {
    let tools = ScriptedTools::new();
    let mut agent = agent_with(tools, vec![ScriptedStream::text_reply("hello")]);
    let recorder = Recorder::new();
    recorder.attach(&mut agent);

    agent.prompt_text("hi").await.expect("prompt runs");

    assert_eq!(
        recorder.kinds(),
        vec![
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_update",
            "message_update",
            "message_update",
            "message_end",
            "turn_end",
            "agent_end",
        ]
    );
}

#[tokio::test]
async fn a_provider_failure_becomes_state_error_message() {
    let tools = ScriptedTools::new();
    let mut agent = agent_with(tools, vec![ScriptedStream::error_reply("boom")]);
    let recorder = Recorder::new();
    recorder.attach(&mut agent);

    agent.prompt_text("hi").await.expect("prompt runs");

    assert_eq!(agent.error_message(), Some("boom"));
    assert_eq!(
        summarize(agent.messages()),
        vec!["system[You are terse.]()", "user[hi]", "assistant[]:error"]
    );
    assert_eq!(recorder.kinds().last(), Some(&"agent_end"));
    assert!(recorder.kinds().contains(&"turn_end"));
}

#[tokio::test]
async fn steering_queued_before_a_run_is_delivered_in_the_same_run() {
    let tools = ScriptedTools::new();
    let mut agent = agent_with(tools, vec![ScriptedStream::text_reply("one")]);
    agent.steer([user("also this")]);

    agent.prompt_text("hi").await.expect("prompt runs");

    // Steering is polled once at the start of the loop, so the message rides
    // along with the prompt instead of triggering a turn of its own.
    assert_eq!(
        summarize(agent.messages()),
        vec![
            "system[You are terse.]()",
            "user[hi]",
            "user[also this]",
            "assistant[one]"
        ]
    );
}

#[tokio::test]
async fn follow_up_messages_extend_the_run_after_the_agent_would_stop() {
    let tools = ScriptedTools::new();
    let mut agent = agent_with(
        tools,
        vec![
            ScriptedStream::text_reply("one"),
            ScriptedStream::text_reply("two"),
        ],
    );

    // pi polls the follow-up queue only once the agent would otherwise stop,
    // so the follow-up lands in a new turn of the same run.
    agent.follow_up([user("follow up")]);
    agent.prompt_text("hi").await.expect("prompt runs");

    assert_eq!(
        summarize(agent.messages()),
        vec![
            "system[You are terse.]()",
            "user[hi]",
            "assistant[one]",
            "user[follow up]",
            "assistant[two]",
        ]
    );
}

#[tokio::test]
async fn queue_modes_deliver_one_at_a_time_or_all() {
    let tools = ScriptedTools::new();
    let mut agent = agent_with(
        tools,
        vec![
            ScriptedStream::text_reply("one"),
            ScriptedStream::text_reply("two"),
            ScriptedStream::text_reply("three"),
        ],
    );
    agent.set_queue_modes(QueueMode::All, QueueMode::All);
    agent.steer([user("a"), user("b")]);

    agent.prompt_text("hi").await.expect("prompt runs");

    // `all`: both steering messages ride along with the prompt, in one turn.
    assert_eq!(
        summarize(agent.messages()),
        vec![
            "system[You are terse.]()",
            "user[hi]",
            "user[a]",
            "user[b]",
            "assistant[one]",
        ]
    );
}

#[tokio::test]
async fn reset_restores_the_transcript_baseline() {
    let tools = ScriptedTools::with_tool("bash");
    let mut agent = agent_with(tools, vec![ScriptedStream::text_reply("hello")]);

    agent.prompt_text("hi").await.expect("prompt runs");
    assert_eq!(agent.messages().len(), 3);

    agent.reset();

    assert_eq!(
        summarize(agent.messages()),
        vec!["system[You are terse.](bash)"],
        "the prompt and tool baseline survive, the conversation does not"
    );
    assert_eq!(agent.error_message(), None);
}

#[tokio::test]
async fn continue_requires_a_non_assistant_trailing_message() {
    let tools = ScriptedTools::new();
    let mut agent = agent_with(tools, vec![ScriptedStream::text_reply("hello")]);

    // Empty transcript except the system message.
    assert_eq!(
        agent.continue_run().await.unwrap_err(),
        "No messages to continue from"
    );

    agent.prompt_text("hi").await.expect("prompt runs");

    assert_eq!(
        agent.continue_run().await.unwrap_err(),
        "Cannot continue from message role: assistant"
    );
}

#[tokio::test]
async fn continue_from_a_user_message_adds_no_new_message() {
    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![ScriptedStream::text_reply("answer")]);
    let mut agent = Agent::new(test_model(), None, tools, stream.clone());

    // pi allows continuing from a user message that has not been answered yet;
    // the continuation adds no message of its own.
    agent.set_messages(vec![user("hi")]);
    let recorder = Recorder::new();
    recorder.attach(&mut agent);

    agent.continue_run().await.expect("continuation runs");

    assert_eq!(
        summarize(agent.messages()),
        vec!["user[hi]", "assistant[answer]"]
    );
    // No message_start/message_end pair for a new prompt: the run starts at the
    // turn.
    assert_eq!(
        recorder.kinds()[..3],
        ["agent_start", "turn_start", "message_start"]
    );
    assert_eq!(
        stream.requests.lock().expect("requests")[0].messages.len(),
        1
    );
}

#[tokio::test]
async fn continue_from_an_assistant_message_prefers_queued_steering() {
    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![
        ScriptedStream::text_reply("first"),
        ScriptedStream::text_reply("second"),
    ]);
    let mut agent = Agent::new(test_model(), None, tools, stream.clone());
    agent.set_messages(vec![user("hi")]);

    agent.continue_run().await.expect("first continuation");
    assert_eq!(
        summarize(agent.messages()),
        vec!["user[hi]", "assistant[first]"]
    );

    agent.steer([user("actually, also this")]);
    agent.continue_run().await.expect("steering continuation");

    assert_eq!(
        summarize(agent.messages()),
        vec![
            "user[hi]",
            "assistant[first]",
            "user[actually, also this]",
            "assistant[second]"
        ]
    );
}

#[tokio::test]
async fn tool_calls_appear_in_the_transcript_with_results() {
    let tools = ScriptedTools::with_tool("bash");
    tools.push_result(Ok(text_result("exit 0")));
    let mut agent = agent_with(
        tools,
        vec![
            ScriptedStream::tool_call_reply(&[(
                "call_1",
                "bash",
                serde_json::json!({"command": "ls"})
                    .as_object()
                    .unwrap()
                    .clone(),
            )]),
            ScriptedStream::text_reply("done"),
        ],
    );
    let recorder = Recorder::new();
    recorder.attach(&mut agent);

    agent.prompt_text("list files").await.expect("prompt runs");

    assert_eq!(
        summarize(agent.messages()),
        vec![
            "system[You are terse.](bash)",
            "user[list files]",
            "assistant[]",
            "toolResult[bash=exit 0]",
            "assistant[done]",
        ]
    );
    let ends: Vec<&str> = recorder
        .all()
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionEnd { is_error, .. } => Some(*is_error),
            _ => None,
        })
        .map(|is_error| if is_error { "error" } else { "ok" })
        .collect();
    assert_eq!(ends, vec!["ok"]);
}

#[tokio::test]
async fn builder_options_reach_the_loop() {
    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![ScriptedStream::text_reply("hi")]);
    let mut agent = Agent::new(test_model(), None, tools, stream.clone())
        .with_session_id("session-7")
        .with_api_key("sk-x")
        .with_thinking_level(ThinkingLevel::High)
        .with_tool_execution(ToolExecutionMode::Sequential);

    agent.prompt_text("hi").await.expect("prompt runs");

    let requests = stream.requests.lock().expect("requests");
    let request = &requests[0];
    assert_eq!(request.session_id.as_deref(), Some("session-7"));
    assert_eq!(request.api_key.as_deref(), Some("sk-x"));
    assert_eq!(request.thinking_level, ThinkingLevel::High);
}

/// A run that throws still leaves the agent ready for the next prompt.
///
/// pi's `runWithLifecycle` ends in `finally { this.finishRun() }`
/// (`agent.ts:526-528`), so whatever a run throws, the run is over and the
/// agent answers the next prompt. Without it `is_streaming` stays set and every
/// later prompt is refused with "Agent is already processing" - the session is
/// dead even though the throw has been reported.
#[tokio::test]
async fn a_run_that_throws_leaves_the_agent_ready_for_the_next_prompt() {
    use futures::FutureExt;

    let tools = ScriptedTools::new();
    let mut agent = Agent::new(
        test_model(),
        Some("You are terse."),
        tools,
        ThrowingStream::new(),
    );
    let recorder = Recorder::new();
    recorder.attach(&mut agent);

    let outcome = std::panic::AssertUnwindSafe(agent.prompt_text("hi"))
        .catch_unwind()
        .await;
    assert!(
        outcome.is_err(),
        "the throw reached the caller instead of being swallowed"
    );

    assert!(
        !agent.is_streaming(),
        "the run finished as far as the next prompt is concerned"
    );

    // And the listener the run had taken is back: a second run is observed.
    agent
        .prompt_text("again")
        .await
        .expect("the next prompt runs");
    assert!(
        recorder
            .kinds()
            .iter()
            .filter(|kind| **kind == "turn_end")
            .count()
            == 1,
        "the listener came back with the run that threw: {:?}",
        recorder.kinds()
    );
    assert!(
        summarize(agent.messages())
            .iter()
            .any(|line| line.contains("after the throw")),
        "the second run completed: {:?}",
        summarize(agent.messages())
    );
}

/// Dropping the prompt future mid-run finishes the run anyway.
///
/// pi's `prompt()` hands back a promise no caller can cancel, and
/// `runWithLifecycle` ends in `finally { this.finishRun() }`
/// (`agent.ts:526-528`), so the run always finishes. A Rust future can be
/// dropped - `tokio::time::timeout(d, agent.prompt(..))` is the ordinary way to
/// bound a run, and a `select!` arm losing its race drops it too - and then
/// there is no `finally` left. Without a drop guard the agent keeps
/// `is_streaming`, every later prompt is refused with "Agent is already
/// processing", `reset()` is a no-op, and a tool call left pending is never
/// cancelled.
#[tokio::test]
async fn a_prompt_future_dropped_mid_run_finishes_the_run() {
    use std::time::Duration;

    let tools = ScriptedTools::with_tool("bash");
    tools.stall_next_call();
    let mut agent = Agent::new(
        test_model(),
        Some("You are terse."),
        tools,
        StallingStream::new(),
    );
    let recorder = Recorder::new();
    recorder.attach(&mut agent);

    // The run gets as far as the tool call, then stalls there for good.
    let outcome = tokio::time::timeout(Duration::from_millis(50), agent.prompt_text("hi")).await;
    assert!(
        outcome.is_err(),
        "the run was still in flight when the timeout dropped it: {outcome:?}"
    );
    assert_eq!(
        recorder.kinds(),
        vec![
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_update",
            "message_update",
            "message_end",
            "tool_execution_start",
        ],
        "the drop landed mid-turn, with the tool call pending: {:?}",
        recorder.kinds()
    );

    // pi's `finally` ran: the agent is idle, the pending tool call is
    // cancelled, and the half-streamed message is not still reported as live.
    assert!(
        !agent.is_streaming(),
        "the dropped run left the agent idle, as `finishRun` in the `finally` does"
    );
    assert!(
        agent.pending_tool_calls().is_empty(),
        "the tool call the dropped run left pending was cancelled: {:?}",
        agent.pending_tool_calls()
    );
    assert_eq!(agent.streaming_message(), None);

    // And the agent still answers: the next prompt is not refused.
    agent
        .prompt_text("again")
        .await
        .expect("the next prompt runs");
    assert!(
        summarize(agent.messages())
            .iter()
            .any(|line| line.contains("after the drop")),
        "the second run completed: {:?}",
        summarize(agent.messages())
    );
    assert!(
        recorder.kinds().contains(&"turn_end"),
        "the listener set survived the dropped run: {:?}",
        recorder.kinds()
    );
}

#[tokio::test]
async fn the_converter_can_be_swapped_after_construction_like_pi_s_field() {
    // pi assigns `agent.convertToLlm = ...` on a live agent (what the
    // coding-agent extensions do in `examples/extensions/handoff.ts`).
    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![
        ScriptedStream::text_reply("one"),
        ScriptedStream::text_reply("two"),
    ]);
    let mut agent = Agent::new(test_model(), None, tools, stream.clone());

    agent.prompt_text("hi").await.expect("the first run");
    assert_eq!(
        summarize(&stream.requests.lock().expect("requests")[0].messages),
        vec!["user[hi]"],
        "the default converter passes the user message through"
    );

    agent.set_convert_to_llm(|_messages: Vec<AgentMessage>| {
        vec![AgentMessage::User(pi_ai::types::UserMessage {
            content: UserContent::Text("swapped".to_string()),
            timestamp: 1_700_000_000_000,
        })]
    });

    agent.prompt_text("hi again").await.expect("the second run");
    let requests = stream.requests.lock().expect("requests");
    assert_eq!(
        summarize(&requests[1].messages),
        vec!["user[swapped]"],
        "the swapped-in converter is read into the loop config for the next run"
    );
    // pi captures the field when the run's loop config is built, so a swap
    // cannot reach a run already in flight; `&mut self` gives the same
    // boundary, and `convert_to_llm` reads back what the next run will use.
    assert!(Arc::strong_count(agent.convert_to_llm()) >= 1);
}
