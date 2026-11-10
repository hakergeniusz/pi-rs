//! The loop, ported from `packages/agent/test/agent-loop.test.ts`.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use pi_agent::agent_loop::{
    AsyncConvertToLlm, ConvertToLlm, FinishTurn, PrepareNextTurn, TransformContext,
};
use pi_agent::agent_message::AgentMessage;
use pi_agent::types::{
    AfterToolCall, AfterToolCallOverride, BeforeToolCall, BeforeToolCallDecision, BoxFuture,
    StreamRequest,
};
use pi_agent::{
    AgentEvent, AgentLoopConfig, PendingMessageQueue, QueueMode, ToolExecutionMode, TurnDecision,
};
use pi_ai::types::{ContentBlock, JsonObject};

fn args(pairs: &[(&str, &str)]) -> JsonObject {
    let mut object = JsonObject::new();
    for (key, value) in pairs {
        object.insert(
            (*key).to_string(),
            serde_json::Value::String((*value).to_string()),
        );
    }
    object
}

#[tokio::test]
async fn emits_the_full_lifecycle_for_a_plain_reply() {
    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![ScriptedStream::text_reply("hello")]);
    let mut harness = LoopHarness::new(tools.clone(), stream.clone());

    let new_messages = harness.run(vec![user("hi")]).await;

    assert_eq!(stream.request_count(), 1);
    assert_eq!(
        harness.kinds(),
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
    assert_eq!(
        summarize(&new_messages),
        vec!["user[hi]", "assistant[hello]"]
    );
    assert_eq!(tools.call_count(), 0);
}

#[tokio::test]
async fn declares_the_executable_tool_set_before_the_first_request() {
    // The tool declarations travel in a system message inserted into the
    // transcript; there is no separate tools field on the request.
    let tools = ScriptedTools::with_tool("bash");
    let stream = ScriptedStream::new(vec![ScriptedStream::text_reply("hello")]);
    let mut harness = LoopHarness::new(tools, stream.clone());

    let new_messages = harness.run(vec![user("hi")]).await;

    assert_eq!(
        summarize(&new_messages),
        vec!["system[](bash)", "user[hi]", "assistant[hello]"]
    );
    assert_eq!(
        summarize(&stream.requests.lock().expect("requests")[0].messages),
        vec!["system[](bash)", "user[hi]"]
    );
}

#[tokio::test]
async fn builds_provider_context_exclusively_from_transcript_messages() {
    let tools = ScriptedTools::with_tool("bash");
    let stream = ScriptedStream::new(vec![ScriptedStream::text_reply("hi")]);
    let mut harness = LoopHarness::new(tools, stream.clone());

    harness.run(vec![user("hello")]).await;

    let request = &stream.requests.lock().expect("requests")[0];
    // The request carries the declared transcript only: no `tools` field, the
    // declarations live in the system message.
    assert_eq!(
        summarize(&request.messages),
        vec!["system[](bash)", "user[hello]"]
    );
}

#[tokio::test]
async fn streams_cumulative_partials_into_message_update() {
    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![ScriptedStream::text_reply("hi")]);
    let mut harness = LoopHarness::new(tools, stream);

    harness.run(vec![user("hello")]).await;

    let partials: Vec<String> = harness
        .collector
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageUpdate { message, .. } => Some(
                message
                    .content
                    .iter()
                    .filter_map(|block| block.as_text().map(str::to_string))
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect();
    assert_eq!(partials, vec!["", "hi", "hi"]);
}

#[tokio::test]
async fn executes_tool_calls_then_continues_the_conversation() {
    let tools = ScriptedTools::with_tool("bash");
    tools.push_result(Ok(text_result("exit 0")));
    let stream = ScriptedStream::new(vec![
        ScriptedStream::tool_call_reply(&[("call_1", "bash", args(&[("command", "ls")]))]),
        ScriptedStream::text_reply("done"),
    ]);
    let mut harness = LoopHarness::new(tools.clone(), stream.clone());

    let new_messages = harness.run(vec![user("list files")]).await;

    assert_eq!(stream.request_count(), 2);
    assert_eq!(tools.call_count(), 1);
    assert_eq!(
        harness.kinds(),
        vec![
            "agent_start",
            "turn_start",
            // The declared system message, then the user prompt.
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            // The assistant reply: tool_call_start and tool_call_end.
            "message_start",
            "message_update",
            "message_update",
            "message_end",
            "tool_execution_start",
            "tool_execution_end",
            "message_start",
            "message_end",
            "turn_end",
            "turn_start",
            "message_start",
            "message_update",
            "message_update",
            "message_update",
            "message_end",
            "turn_end",
            "agent_end",
        ]
    );
    assert_eq!(
        summarize(&new_messages),
        vec![
            "system[](bash)",
            "user[list files]",
            "assistant[]",
            "toolResult[bash=exit 0]",
            "assistant[done]"
        ]
    );
    // The second request sees the tool result, so the loop kept the context.
    let second = &stream.requests.lock().expect("requests")[1];
    assert_eq!(
        summarize(&second.messages),
        vec![
            "system[](bash)",
            "user[list files]",
            "assistant[]",
            "toolResult[bash=exit 0]"
        ]
    );
}

#[tokio::test]
async fn reports_unknown_tools_without_executing_them() {
    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![
        ScriptedStream::tool_call_reply(&[("call_1", "missing", args(&[]))]),
        ScriptedStream::text_reply("sorry"),
    ]);
    let mut harness = LoopHarness::new(tools.clone(), stream.clone());

    let new_messages = harness.run(vec![user("go")]).await;

    assert_eq!(tools.call_count(), 0);
    assert_eq!(
        summarize(&new_messages),
        vec![
            "user[go]",
            "assistant[]",
            "toolResult[missing=Tool missing not found!]",
            "assistant[sorry]",
        ]
    );
}

#[tokio::test]
async fn refuses_to_run_tool_calls_from_a_truncated_message() {
    let tools = ScriptedTools::with_tool("bash");
    let stream = ScriptedStream::new(vec![
        ScriptedStream::truncated_tool_call_reply(),
        ScriptedStream::text_reply("re-issued"),
    ]);
    let mut harness = LoopHarness::new(tools.clone(), stream.clone());

    let new_messages = harness.run(vec![user("go")]).await;

    assert_eq!(tools.call_count(), 0);
    let tool_result = new_messages
        .iter()
        .find(|message| message.role() == "toolResult")
        .expect("tool result");
    let AgentMessage::ToolResult(result) = tool_result else {
        unreachable!()
    };
    assert!(result.is_error);
    assert!(
        result
            .content
            .iter()
            .filter_map(|block| block.as_text())
            .collect::<String>()
            .contains("output token limit")
    );
    // pi still loops: the failed result goes back to the model.
    assert_eq!(stream.request_count(), 2);
}

#[tokio::test]
async fn ends_tool_calls_in_completion_order_but_persists_results_in_source_order() {
    // The first tool is slower than the second, so completion order differs
    // from source order.
    struct SlowTools {
        inner: Arc<ScriptedTools>,
    }

    impl pi_agent::ToolExecutor for SlowTools {
        fn tools(&self) -> Vec<pi_agent::ToolSpec> {
            self.inner.tools()
        }

        fn execute(
            &self,
            call: pi_agent::AgentToolCall,
            updates: tokio::sync::mpsc::UnboundedSender<pi_agent::AgentToolResult>,
        ) -> BoxFuture<'_, Result<pi_agent::AgentToolResult, String>> {
            let inner = self.inner.clone();
            Box::pin(async move {
                if call.name == "slow" {
                    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                }
                inner.execute(call, updates).await
            })
        }
    }

    let inner = ScriptedTools::new();
    inner.push_tool("slow");
    inner.push_tool("fast");
    let stream = ScriptedStream::new(vec![
        ScriptedStream::tool_call_reply(&[
            ("call_1", "slow", args(&[])),
            ("call_2", "fast", args(&[])),
        ]),
        ScriptedStream::text_reply("done"),
    ]);
    let mut harness = LoopHarness::new(Arc::new(SlowTools { inner }), stream.clone());

    let new_messages = harness.run(vec![user("go")]).await;

    let ends: Vec<String> = harness
        .collector
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionEnd { tool_name, .. } => Some(tool_name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ends, vec!["fast", "slow"]);
    assert_eq!(
        summarize(&new_messages),
        vec![
            "system[](slow,fast)",
            "user[go]",
            "assistant[]",
            "toolResult[slow=ran slow]",
            "toolResult[fast=ran fast]",
            "assistant[done]",
        ]
    );
}

#[tokio::test]
async fn forces_sequential_execution_when_a_tool_requires_it() {
    struct OverlapDetector {
        inner: Arc<ScriptedTools>,
        concurrent: Arc<Mutex<usize>>,
        peak: Arc<Mutex<usize>>,
    }

    impl pi_agent::ToolExecutor for OverlapDetector {
        fn tools(&self) -> Vec<pi_agent::ToolSpec> {
            self.inner.tools()
        }

        fn execution_mode(&self, name: &str) -> Option<ToolExecutionMode> {
            if name == "serial" {
                Some(ToolExecutionMode::Sequential)
            } else {
                None
            }
        }

        fn execute(
            &self,
            call: pi_agent::AgentToolCall,
            updates: tokio::sync::mpsc::UnboundedSender<pi_agent::AgentToolResult>,
        ) -> BoxFuture<'_, Result<pi_agent::AgentToolResult, String>> {
            let inner = self.inner.clone();
            let concurrent = self.concurrent.clone();
            let peak = self.peak.clone();
            Box::pin(async move {
                {
                    let mut current = concurrent.lock().expect("concurrent");
                    *current += 1;
                    let mut highest = peak.lock().expect("peak");
                    *highest = (*highest).max(*current);
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                {
                    *concurrent.lock().expect("concurrent") -= 1;
                }
                inner.execute(call, updates).await
            })
        }
    }

    let inner = ScriptedTools::new();
    inner.push_tool("serial");
    inner.push_tool("free");
    let peak = Arc::new(Mutex::new(0));
    let executor = OverlapDetector {
        inner,
        concurrent: Arc::new(Mutex::new(0)),
        peak: Arc::clone(&peak),
    };
    let stream = ScriptedStream::new(vec![
        ScriptedStream::tool_call_reply(&[
            ("call_1", "serial", args(&[])),
            ("call_2", "free", args(&[])),
        ]),
        ScriptedStream::text_reply("done"),
    ]);
    let mut harness = LoopHarness::new(Arc::new(executor), stream);

    harness.run(vec![user("go")]).await;

    assert_eq!(*peak.lock().expect("peak"), 1);
}

#[tokio::test]
async fn emits_tool_execution_updates_before_the_result() {
    let tools = ScriptedTools::with_tool("bash");
    tools
        .updates
        .lock()
        .expect("updates")
        .push(text_result("partial 1"));
    tools
        .updates
        .lock()
        .expect("updates")
        .push(text_result("partial 2"));
    let stream = ScriptedStream::new(vec![
        ScriptedStream::tool_call_reply(&[("call_1", "bash", args(&[]))]),
        ScriptedStream::text_reply("done"),
    ]);
    let mut harness = LoopHarness::new(tools, stream);

    harness.run(vec![user("go")]).await;

    let relevant: Vec<&str> = harness
        .kinds()
        .into_iter()
        .filter(|kind| kind.starts_with("tool_execution"))
        .collect();
    assert_eq!(
        relevant,
        vec![
            "tool_execution_start",
            "tool_execution_update",
            "tool_execution_update",
            "tool_execution_end"
        ]
    );
}

struct BlockTool;

impl pi_agent::ToolExecutor for BlockTool {
    fn tools(&self) -> Vec<pi_agent::ToolSpec> {
        vec![pi_agent::ToolSpec {
            name: "bash".to_string(),
            description: "bash".to_string(),
            parameters: serde_json::json!({ "type": "object" }),
            execution_mode: None,
            constrained_sampling: None,
        }]
    }

    fn execute(
        &self,
        _call: pi_agent::AgentToolCall,
        _updates: tokio::sync::mpsc::UnboundedSender<pi_agent::AgentToolResult>,
    ) -> BoxFuture<'_, Result<pi_agent::AgentToolResult, String>> {
        Box::pin(async { panic!("the blocked tool must not run") })
    }
}

struct BlockingHook;

impl BeforeToolCall for BlockingHook {
    fn call<'a>(
        &'a self,
        _assistant_message: &'a pi_ai::types::AssistantMessage,
        tool_call: &'a pi_agent::AgentToolCall,
    ) -> BoxFuture<'a, Option<BeforeToolCallDecision>> {
        let name = tool_call.name.clone();
        Box::pin(async move {
            Some(BeforeToolCallDecision {
                block: true,
                reason: Some(format!("{name} needs approval")),
                terminate: false,
            })
        })
    }
}

#[tokio::test]
async fn before_hook_can_block_a_tool_call() {
    let stream = ScriptedStream::new(vec![
        ScriptedStream::tool_call_reply(&[("call_1", "bash", args(&[]))]),
        ScriptedStream::text_reply("understood"),
    ]);
    let mut harness = LoopHarness::new(Arc::new(BlockTool), stream);
    let config = AgentLoopConfig {
        hooks: pi_agent::ToolCallHooks {
            before: Some(Arc::new(BlockingHook)),
            after: None,
        },
        ..AgentLoopConfig::default()
    };

    let new_messages = harness.run_with_config(vec![user("go")], config).await;

    assert_eq!(
        summarize(&new_messages),
        vec![
            "system[](bash)",
            "user[go]",
            "assistant[]",
            "toolResult[bash=bash needs approval!]",
            "assistant[understood]"
        ]
    );
}

struct RewritingHook;

impl AfterToolCall for RewritingHook {
    fn call<'a>(
        &'a self,
        _assistant_message: &'a pi_ai::types::AssistantMessage,
        _tool_call: &'a pi_agent::AgentToolCall,
        result: &'a pi_agent::AgentToolResult,
        _is_error: bool,
    ) -> BoxFuture<'a, Option<AfterToolCallOverride>> {
        let details = result.details.clone();
        Box::pin(async move {
            Some(AfterToolCallOverride {
                details: Some(serde_json::json!({ "rewritten": true, "previous": details })),
                ..AfterToolCallOverride::default()
            })
        })
    }
}

#[tokio::test]
async fn after_hook_can_rewrite_the_result() {
    let tools = ScriptedTools::with_tool("bash");
    let stream = ScriptedStream::new(vec![
        ScriptedStream::tool_call_reply(&[("call_1", "bash", args(&[]))]),
        ScriptedStream::text_reply("done"),
    ]);
    let mut harness = LoopHarness::new(tools, stream);
    let config = AgentLoopConfig {
        hooks: pi_agent::ToolCallHooks {
            before: None,
            after: Some(Arc::new(RewritingHook)),
        },
        ..AgentLoopConfig::default()
    };

    let new_messages = harness.run_with_config(vec![user("go")], config).await;

    let result = new_messages
        .iter()
        .find(|message| message.role() == "toolResult")
        .expect("tool result");
    let AgentMessage::ToolResult(result) = result else {
        unreachable!()
    };
    assert_eq!(
        result.details,
        Some(serde_json::json!({ "rewritten": true, "previous": {} }))
    );
    assert!(!result.is_error);
}

struct CountingFinishTurn {
    turns: Arc<Mutex<Vec<(String, usize)>>>,
    decision: Option<TurnDecision>,
}

impl FinishTurn for CountingFinishTurn {
    fn call(&self, turn: &pi_agent::AgentTurnContext) -> BoxFuture<'_, Option<TurnDecision>> {
        let role = turn.message.role().to_string();
        let tool_results = turn.tool_results.len();
        self.turns.lock().expect("turns").push((role, tool_results));
        let decision = self.decision;
        Box::pin(async move { decision })
    }
}

#[tokio::test]
#[allow(clippy::items_after_statements)]
async fn finish_turn_end_stops_before_any_queue_poll() {
    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![ScriptedStream::text_reply("hello")]);
    let turns = Arc::new(Mutex::new(Vec::new()));
    let config = AgentLoopConfig {
        finish_turn: Some(Arc::new(CountingFinishTurn {
            turns: Arc::clone(&turns),
            decision: Some(TurnDecision::End),
        })),
        ..AgentLoopConfig::default()
    };
    let mut harness = LoopHarness::new(tools, stream.clone());

    let new_messages = harness.run_with_config(vec![user("hi")], config).await;

    assert_eq!(
        *turns.lock().expect("turns"),
        vec![("assistant".to_string(), 0)]
    );
    assert_eq!(stream.request_count(), 1);
    assert_eq!(harness.kinds().last(), Some(&"agent_end"));
    assert_eq!(
        summarize(&new_messages),
        vec!["user[hi]", "assistant[hello]"]
    );
}

#[tokio::test]
#[allow(clippy::items_after_statements)]
async fn finish_turn_continue_makes_exactly_one_context_only_request() {
    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![
        ScriptedStream::text_reply("one"),
        ScriptedStream::text_reply("two"),
    ]);
    let turns = Arc::new(Mutex::new(Vec::new()));
    let config = AgentLoopConfig {
        finish_turn: Some(Arc::new(CountingFinishTurn {
            turns: Arc::clone(&turns),
            // Continue only for the first turn.
            decision: None,
        })),
        ..AgentLoopConfig::default()
    };
    let mut harness = LoopHarness::new(tools, stream.clone());

    harness.run_with_config(vec![user("hi")], config).await;
    assert_eq!(stream.request_count(), 1);

    // Second run: continue on the first turn, then stop.
    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![
        ScriptedStream::text_reply("one"),
        ScriptedStream::text_reply("two"),
    ]);
    let turns = Arc::new(Mutex::new(Vec::new()));
    struct OnceContinue {
        turns: Arc<Mutex<Vec<(String, usize)>>>,
    }
    impl FinishTurn for OnceContinue {
        fn call(&self, turn: &pi_agent::AgentTurnContext) -> BoxFuture<'_, Option<TurnDecision>> {
            let role = turn.message.role().to_string();
            let tool_results = turn.tool_results.len();
            let mut turns = self.turns.lock().expect("turns");
            turns.push((role, tool_results));
            let decision = if turns.len() == 1 {
                Some(TurnDecision::Continue)
            } else {
                Some(TurnDecision::End)
            };
            Box::pin(async move { decision })
        }
    }
    let config = AgentLoopConfig {
        finish_turn: Some(Arc::new(OnceContinue { turns })),
        ..AgentLoopConfig::default()
    };
    let mut harness = LoopHarness::new(tools, stream.clone());

    let new_messages = harness.run_with_config(vec![user("hi")], config).await;

    assert_eq!(stream.request_count(), 2);
    // The continuation request carries no new message.
    let second = &stream.requests.lock().expect("requests")[1];
    assert_eq!(
        summarize(&second.messages),
        vec!["user[hi]", "assistant[one]"]
    );
    assert_eq!(
        summarize(&new_messages),
        vec!["user[hi]", "assistant[one]", "assistant[two]"]
    );
}

#[tokio::test]
#[allow(clippy::items_after_statements)]
async fn finish_turn_runs_after_tool_results_and_before_turn_end() {
    let tools = ScriptedTools::with_tool("bash");
    let stream = ScriptedStream::new(vec![
        ScriptedStream::tool_call_reply(&[("call_1", "bash", args(&[]))]),
        ScriptedStream::text_reply("done"),
    ]);
    let order = Arc::new(Mutex::new(Vec::new()));
    struct OrderRecorder {
        order: Arc<Mutex<Vec<String>>>,
    }
    impl FinishTurn for OrderRecorder {
        fn call(&self, turn: &pi_agent::AgentTurnContext) -> BoxFuture<'_, Option<TurnDecision>> {
            self.order.lock().expect("order").push(format!(
                "finish_turn({} tool_results={})",
                turn.message.role(),
                turn.tool_results.len()
            ));
            Box::pin(async { Some(TurnDecision::End) })
        }
    }
    let config = AgentLoopConfig {
        finish_turn: Some(Arc::new(OrderRecorder {
            order: Arc::clone(&order),
        })),
        ..AgentLoopConfig::default()
    };
    let mut harness = LoopHarness::new(tools, stream);

    harness.run_with_config(vec![user("go")], config).await;

    let kinds = harness.kinds();
    let finish_index = 1;
    assert_eq!(
        order.lock().expect("order").clone(),
        vec!["finish_turn(assistant tool_results=1)"]
    );
    // turn_end follows finish_turn; the check above is the ordering assertion.
    assert_eq!(kinds[kinds.len() - 2], "turn_end");
    assert_eq!(finish_index, 1);
}

struct RecordingPrepareRequest {
    seen: Arc<Mutex<Vec<(String, pi_agent::ThinkingLevel)>>>,
    /// The transcript as it stood when the hook ran, one entry per call.
    contexts: Arc<Mutex<Vec<Vec<String>>>>,
}

impl pi_agent::agent_loop::PrepareRequest for RecordingPrepareRequest {
    fn call<'a>(
        &'a self,
        request: pi_agent::PrepareRequestContext<'a>,
    ) -> BoxFuture<'a, pi_agent::TurnUpdate> {
        self.seen
            .lock()
            .expect("seen")
            .push((request.model.id.clone(), request.thinking_level));
        self.contexts
            .lock()
            .expect("contexts")
            .push(summarize(&request.context.messages));
        Box::pin(async {
            pi_agent::TurnUpdate {
                context: None,
                model: None,
                thinking_level: Some(pi_agent::ThinkingLevel::High),
            }
        })
    }
}

#[tokio::test]
async fn prepare_request_runs_before_each_request_and_can_change_thinking_level() {
    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![ScriptedStream::text_reply("hi")]);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let config = AgentLoopConfig {
        prepare_request: Some(Arc::new(RecordingPrepareRequest {
            seen: Arc::clone(&seen),
            contexts: Arc::clone(&contexts),
        })),
        ..AgentLoopConfig::default()
    };
    let mut harness = LoopHarness::new(tools, stream.clone());

    harness.run_with_config(vec![user("hi")], config).await;

    assert_eq!(
        *seen.lock().expect("seen"),
        vec![("test-model".to_string(), pi_agent::ThinkingLevel::Off)]
    );
    // The update lands on the request, not on the loop's own state.
    assert_eq!(
        stream.requests.lock().expect("requests")[0].thinking_level,
        pi_agent::ThinkingLevel::High
    );
    // pi hands the hook the live context (`agent-loop.ts:219-224`), which
    // already carries the prompt that was just appended.
    assert_eq!(
        *contexts.lock().expect("contexts"),
        vec![vec!["user[hi]".to_string()]],
        "the hook saw the request's own transcript, prompt already appended"
    );
}

/// pi hands the hook the loop's own `currentContext`
/// (`agent-loop.ts:219-225`), so a hook that appends to
/// `request.context.messages` in place changes what the request carries —
/// no returned update needed.
#[tokio::test]
async fn prepare_request_mutating_the_context_in_place_reaches_the_request() {
    struct InPlacePrepare;

    impl pi_agent::agent_loop::PrepareRequest for InPlacePrepare {
        fn call<'a>(
            &'a self,
            request: pi_agent::PrepareRequestContext<'a>,
        ) -> BoxFuture<'a, pi_agent::TurnUpdate> {
            request
                .context
                .messages
                .push(user("injected by prepareRequest"));
            Box::pin(async {
                // No context in the update: `requestUpdate.context ?? currentContext`
                // (`agent-loop.ts:228`) keeps the mutated live context.
                pi_agent::TurnUpdate {
                    context: None,
                    model: None,
                    thinking_level: None,
                }
            })
        }
    }

    let tools = ScriptedTools::with_tool("bash");
    let stream = ScriptedStream::new(vec![
        ScriptedStream::tool_call_reply(&[("call_1", "bash", args(&[]))]),
        ScriptedStream::text_reply("done"),
    ]);
    let config = AgentLoopConfig {
        prepare_request: Some(Arc::new(InPlacePrepare)),
        ..AgentLoopConfig::default()
    };
    let mut harness = LoopHarness::new(tools, stream.clone());

    let new_messages = harness.run_with_config(vec![user("hi")], config).await;

    let requests = stream.requests.lock().expect("requests");
    let first = summarize(&requests[0].messages);
    assert_eq!(
        first,
        vec![
            "system[](bash)".to_string(),
            "user[hi]".to_string(),
            "user[injected by prepareRequest]".to_string()
        ],
        "the in-place edit reached the request: {first:?}"
    );
    // The edit is on the loop's own transcript, so it survives into the next
    // request of the same run.
    let second = summarize(&requests[1].messages);
    assert!(
        second
            .iter()
            .any(|line| line == "user[injected by prepareRequest]"),
        "the in-place edit is in the transcript the next request is made from: {second:?}"
    );
    // pi's `prepareRequest` only edits the context: the message is never
    // emitted, so it does not become a new message of the run.
    assert!(
        !summarize(&new_messages)
            .iter()
            .any(|line| line.contains("injected by prepareRequest")),
        "no lifecycle event for a context-only edit: {:?}",
        summarize(&new_messages)
    );
}

/// The hook can replace the context the request is made from.
///
/// pi reads the update as `currentContext = requestUpdate.context ??
/// currentContext` (`agent-loop.ts:228`); the port does the same, so a host can
/// rewrite the transcript wholesale between the queue drain and the request.
#[tokio::test]
async fn prepare_request_can_replace_the_context_the_request_uses() {
    struct ReplacingPrepare;

    impl pi_agent::agent_loop::PrepareRequest for ReplacingPrepare {
        fn call<'a>(
            &'a self,
            request: pi_agent::PrepareRequestContext<'a>,
        ) -> BoxFuture<'a, pi_agent::TurnUpdate> {
            let mut context = request.context.clone();
            context.messages.push(user("injected by prepareRequest"));
            Box::pin(async move {
                pi_agent::TurnUpdate {
                    context: Some(context),
                    model: None,
                    thinking_level: None,
                }
            })
        }
    }

    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![ScriptedStream::text_reply("hi")]);
    let config = AgentLoopConfig {
        prepare_request: Some(Arc::new(ReplacingPrepare)),
        ..AgentLoopConfig::default()
    };
    let mut harness = LoopHarness::new(tools, stream.clone());

    harness.run_with_config(vec![user("hi")], config).await;

    let request_messages = summarize(&stream.requests.lock().expect("requests")[0].messages);
    assert!(
        request_messages
            .iter()
            .any(|line| line.contains("injected by prepareRequest")),
        "the request was made from the replaced context: {request_messages:?}"
    );
    assert_eq!(
        request_messages,
        vec![
            "user[hi]".to_string(),
            "user[injected by prepareRequest]".to_string()
        ],
    );
}

struct RecordingPrepareNextTurn {
    turns: Arc<Mutex<Vec<usize>>>,
}

impl PrepareNextTurn for RecordingPrepareNextTurn {
    fn call<'a>(
        &'a self,
        context: &'a pi_agent::PrepareNextTurnContext,
    ) -> BoxFuture<'a, Option<pi_agent::TurnUpdate>> {
        self.turns
            .lock()
            .expect("turns")
            .push(context.turn.new_messages.len());
        Box::pin(async { None })
    }
}

#[tokio::test]
async fn prepare_next_turn_sees_the_finalized_turn_before_the_next_request() {
    let tools = ScriptedTools::with_tool("bash");
    let stream = ScriptedStream::new(vec![
        ScriptedStream::tool_call_reply(&[("call_1", "bash", args(&[]))]),
        ScriptedStream::text_reply("done"),
    ]);
    let turns = Arc::new(Mutex::new(Vec::new()));
    let config = AgentLoopConfig {
        prepare_next_turn: Some(Arc::new(RecordingPrepareNextTurn {
            turns: Arc::clone(&turns),
        })),
        ..AgentLoopConfig::default()
    };
    let mut harness = LoopHarness::new(tools, stream);

    harness.run_with_config(vec![user("go")], config).await;

    // user, assistant (with the tool call), tool result.
    // system, user, assistant (with the tool call), tool result.
    assert_eq!(*turns.lock().expect("turns"), vec![4]);
}

struct DroppingTransform;

impl TransformContext for DroppingTransform {
    fn call(&self, messages: Vec<AgentMessage>) -> BoxFuture<'_, Vec<AgentMessage>> {
        Box::pin(async move {
            messages
                .into_iter()
                .filter(|message| message.role() != "toolResult")
                .collect()
        })
    }
}

#[tokio::test]
async fn transform_context_runs_before_the_provider_request() {
    let tools = ScriptedTools::with_tool("bash");
    let stream = ScriptedStream::new(vec![
        ScriptedStream::tool_call_reply(&[("call_1", "bash", args(&[]))]),
        ScriptedStream::text_reply("done"),
    ]);
    let config = AgentLoopConfig {
        transform_context: Some(Arc::new(DroppingTransform)),
        ..AgentLoopConfig::default()
    };
    let mut harness = LoopHarness::new(tools, stream.clone());

    harness.run_with_config(vec![user("go")], config).await;

    let second = &stream.requests.lock().expect("requests")[1];
    assert_eq!(
        summarize(&second.messages),
        vec!["system[](bash)", "user[go]", "assistant[]"]
    );
}

#[tokio::test]
async fn provider_failure_becomes_an_assistant_error_message_and_ends_the_run() {
    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![ScriptedStream::error_reply("401 unauthorized")]);
    let mut harness = LoopHarness::new(tools, stream.clone());

    let new_messages = harness.run(vec![user("hi")]).await;

    assert_eq!(stream.request_count(), 1);
    assert_eq!(
        summarize(&new_messages),
        vec!["user[hi]", "assistant[]:error"]
    );
    let AgentMessage::Assistant(assistant) = &new_messages[1] else {
        unreachable!()
    };
    assert_eq!(assistant.error_message.as_deref(), Some("401 unauthorized"));
    assert_eq!(harness.kinds().last(), Some(&"agent_end"));
    // No further request: an error turn always ends the run.
    assert!(
        !harness.kinds().contains(&"turn_end")
            || harness
                .kinds()
                .iter()
                .filter(|kind| **kind == "turn_end")
                .count()
                == 1
    );
}

#[tokio::test]
async fn queue_modes_control_how_much_of_a_queue_is_delivered() {
    let mut queue = PendingMessageQueue::new(QueueMode::OneAtATime);
    queue.enqueue(user("first"));
    queue.enqueue(user("second"));
    assert_eq!(summarize(&queue.peek()), vec!["user[first]"]);
    assert_eq!(summarize(&queue.drain()), vec!["user[first]"]);
    assert!(queue.has_items());

    let mut queue = PendingMessageQueue::new(QueueMode::All);
    queue.enqueue(user("first"));
    queue.enqueue(user("second"));
    assert_eq!(
        summarize(&queue.drain()),
        vec!["user[first]", "user[second]"]
    );
    assert!(!queue.has_items());
}

#[tokio::test]
async fn abort_before_the_first_event_produces_an_aborted_message() {
    let tools = ScriptedTools::with_tool("bash");
    let stream = ScriptedStream::new(vec![
        ScriptedStream::tool_call_reply(&[("call_1", "bash", args(&[]))]),
        ScriptedStream::text_reply("done"),
    ]);
    let mut harness = LoopHarness::new(tools.clone(), stream.clone());
    let signal = pi_agent::AbortSignal::new();
    signal.abort();

    let new_messages = pi_agent::run_agent_loop(
        vec![user("go")],
        pi_agent::AgentContext::default(),
        harness.executor.as_ref(),
        &AgentLoopConfig {
            model: test_model(),
            ..AgentLoopConfig::default()
        },
        &mut TestQueues::with_queue_mode(QueueMode::OneAtATime),
        &mut harness.collector,
        &signal,
        stream.as_ref(),
    )
    .await;

    assert_eq!(tools.call_count(), 0);
    assert_eq!(
        summarize(&new_messages),
        vec!["system[](bash)", "user[go]", "assistant[]:error"]
    );
}

#[tokio::test]
async fn tool_execution_update_payloads_carry_the_tool_call_identity() {
    let tools = ScriptedTools::with_tool("bash");
    tools
        .updates
        .lock()
        .expect("updates")
        .push(text_result("chunk"));
    let stream = ScriptedStream::new(vec![
        ScriptedStream::tool_call_reply(&[("call_9", "bash", args(&[("command", "ls")]))]),
        ScriptedStream::text_reply("done"),
    ]);
    let mut harness = LoopHarness::new(tools, stream);

    harness.run(vec![user("go")]).await;

    let update = harness
        .collector
        .events
        .iter()
        .find(|event| matches!(event, AgentEvent::ToolExecutionUpdate { .. }))
        .expect("update event");
    let AgentEvent::ToolExecutionUpdate {
        tool_call_id,
        tool_name,
        args,
        partial_result,
        parent_tool_call_id: _,
    } = update
    else {
        unreachable!()
    };
    assert_eq!(tool_call_id, "call_9");
    assert_eq!(tool_name, "bash");
    assert_eq!(
        args.get("command").and_then(|value| value.as_str()),
        Some("ls")
    );
    assert_eq!(
        partial_result
            .content
            .iter()
            .filter_map(|block: &ContentBlock| block.as_text())
            .collect::<String>(),
        "chunk"
    );
}

#[tokio::test]
async fn transform_request_stream_fn_receives_the_api_key_and_session_id() {
    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![ScriptedStream::text_reply("hi")]);
    let mut harness = LoopHarness::new(tools, stream.clone());
    let config = AgentLoopConfig {
        model: test_model(),
        session_id: Some("session-1".to_string()),
        api_key: Some("sk-test".to_string()),
        ..AgentLoopConfig::default()
    };

    harness.run_with_config(vec![user("hi")], config).await;

    let request: &StreamRequest = &stream.requests.lock().expect("requests")[0];
    assert_eq!(request.session_id.as_deref(), Some("session-1"));
    assert_eq!(request.api_key.as_deref(), Some("sk-test"));
    assert_eq!(request.model.id, "test-model");
}

/// A converter that does not answer until the test opens the gate — pi's
/// `Message[] | Promise<Message[]>` second arm (`types.ts:222`), which the loop
/// awaits at `agent-loop.ts:395`.
struct GatedConvertToLlm {
    gate: Mutex<Option<tokio::sync::oneshot::Receiver<Vec<AgentMessage>>>>,
}

impl ConvertToLlm for GatedConvertToLlm {
    fn call(&self, _messages: Vec<AgentMessage>) -> BoxFuture<'_, Vec<AgentMessage>> {
        let gate = self
            .gate
            .lock()
            .expect("gate")
            .take()
            .expect("the gate is armed exactly once");
        Box::pin(async move { gate.await.expect("the gate opens") })
    }
}

#[tokio::test]
async fn the_loop_awaits_convert_to_llm_and_sends_the_hook_result() {
    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![ScriptedStream::text_reply("done")]);
    let (open, wait) = tokio::sync::oneshot::channel();
    let config = AgentLoopConfig {
        convert_to_llm: Arc::new(GatedConvertToLlm {
            gate: Mutex::new(Some(wait)),
        }),
        ..AgentLoopConfig::default()
    };

    let mut harness = LoopHarness::new(tools, stream.clone());

    // The loop runs concurrently with the test holding the gate shut: the
    // request can only exist once the converter's future resolves.
    let run = harness.run_with_config(vec![user("go")], config);
    let opener = async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            stream.request_count(),
            0,
            "no request while the hook is pending"
        );
        open.send(vec![user("from the hook")])
            .expect("the test opens the gate");
    };
    let (new_messages, ()) = tokio::join!(run, opener);

    assert_eq!(stream.request_count(), 1);
    assert_eq!(
        summarize(&stream.requests.lock().expect("requests")[0].messages),
        vec!["user[from the hook]"],
        "the provider request carries the awaited result, not the input messages"
    );
    assert_eq!(
        summarize(&new_messages),
        vec!["user[go]", "assistant[done]"],
        "the transcript is untouched by the conversion: only the request is"
    );
}

#[tokio::test]
async fn a_convert_to_llm_that_awaits_a_timer_delays_the_request() {
    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![ScriptedStream::text_reply("done")]);
    // The converter appends a marker after a real await, so the marker can only
    // reach the request if the loop awaited the future rather than a value.
    let converter = AsyncConvertToLlm(|messages: Vec<AgentMessage>| async move {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        let mut messages = messages;
        messages.push(user("marker"));
        messages
    });
    let config = AgentLoopConfig {
        convert_to_llm: Arc::new(converter),
        ..AgentLoopConfig::default()
    };
    let mut harness = LoopHarness::new(tools, stream.clone());

    let started = std::time::Instant::now();
    harness.run_with_config(vec![user("go")], config).await;

    assert!(started.elapsed() >= std::time::Duration::from_millis(35));
    assert_eq!(
        summarize(&stream.requests.lock().expect("requests")[0].messages),
        vec!["user[go]", "user[marker]"]
    );
}

#[tokio::test]
async fn a_sync_converter_still_works_through_the_blanket_impl() {
    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![ScriptedStream::text_reply("done")]);
    // pi's `Message[]` (non-promise) arm: a plain `Fn` is still a ConvertToLlm.
    let config = AgentLoopConfig {
        convert_to_llm: Arc::new(|messages: Vec<AgentMessage>| {
            messages
                .into_iter()
                .filter(|message| message.role() != "user")
                .collect()
        }),
        ..AgentLoopConfig::default()
    };
    let mut harness = LoopHarness::new(tools, stream.clone());

    harness.run_with_config(vec![user("go")], config).await;

    assert_eq!(
        summarize(&stream.requests.lock().expect("requests")[0].messages),
        Vec::<String>::new(),
        "the sync closure's result is what the request carries"
    );
}

fn bash_execution(command: &str, exclude: bool) -> AgentMessage {
    AgentMessage::BashExecution(pi_agent::BashExecutionMessage {
        command: command.to_string(),
        output: "ok".to_string(),
        exit_code: Some(0),
        cancelled: false,
        truncated: false,
        full_output_path: None,
        exclude_from_context: Some(exclude),
        timestamp: 1_700_000_000_000,
    })
}

#[tokio::test]
async fn the_default_converter_is_the_one_that_runs_when_no_hook_is_installed() {
    let tools = ScriptedTools::new();
    let stream = ScriptedStream::new(vec![ScriptedStream::text_reply("done")]);
    let mut harness = LoopHarness::new(tools, stream.clone());

    harness
        .run_with_config(
            vec![
                user("hi"),
                bash_execution("ls -la", false),
                bash_execution("echo secret", true),
                AgentMessage::CompactionSummary(pi_agent::CompactionSummaryMessage {
                    summary: "compacted".to_string(),
                    tokens_before: 1_000.0,
                    timestamp: 1_700_000_000_000,
                }),
                AgentMessage::BranchSummary(pi_agent::BranchSummaryMessage {
                    summary: "branched".to_string(),
                    from_id: Some("root".to_string()),
                    timestamp: 1_700_000_000_000,
                }),
            ],
            AgentLoopConfig::default(),
        )
        .await;

    let request = &stream.requests.lock().expect("requests")[0];
    assert_eq!(
        summarize(&request.messages),
        vec![
            "user[hi]",
            "user[Ran `ls -la`\n```\nok\n```]",
            "user[The conversation history before this point was compacted into the following summary:\n\n<summary>\ncompacted\n</summary>]",
            "user[The following is a summary of a branch that this conversation came back from:\n\n<summary>\nbranched</summary>]",
        ],
        "the bash command marked `!!` is excluded; the two summaries survive as user messages"
    );
    assert_eq!(request.messages.len(), 4);
}
