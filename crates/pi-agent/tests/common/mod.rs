//! Shared test doubles for the agent tests.
//!
//! Ground truth for the shapes: `packages/agent/test/agent-loop.test.ts` and
//! `agent.test.ts` both drive the loop with a scripted stream function and a
//! scriptable tool, which is exactly what these two types provide here.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use futures::stream::BoxStream;
use pi_agent::agent_message::AgentMessage;
use pi_agent::events::{AgentEvent, AgentEventSink};
use pi_agent::types::{
    AgentToolCall, AgentToolResult, BoxFuture, StreamFn, StreamRequest, ToolExecutor, ToolSpec,
};
use pi_agent::{AbortSignal, Agent, QueueMode};
use pi_ai::types::AssistantMessageEvent;
use pi_ai::types::Model;
use pi_ai::types::{
    AssistantContent, AssistantMessage, ContentBlock, InputContent, JsonObject, TextContent, Usage,
    UserContent,
};

/// A provider that replays scripted responses, one per request.
pub struct ScriptedStream {
    responses: Mutex<Vec<Vec<AssistantMessageEvent>>>,
    pub requests: Mutex<Vec<StreamRequest>>,
}

impl ScriptedStream {
    pub fn new(responses: Vec<Vec<AssistantMessageEvent>>) -> Arc<ScriptedStream> {
        Arc::new(ScriptedStream {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        })
    }

    /// A single assistant message with one text block, streamed as
    /// start → text start → delta → text end → done.
    pub fn text_reply(text: &str) -> Vec<AssistantMessageEvent> {
        let empty = partial();
        let with_text = partial_with(vec![AssistantContent::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })]);
        vec![
            AssistantMessageEvent::Start {
                partial: empty.clone(),
            },
            // pi's assembler has already opened the text block by the time
            // `text_start` fires, so the partial carries the (empty) block.
            AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: partial_with(vec![AssistantContent::Text(TextContent {
                    text: String::new(),
                    text_signature: None,
                })]),
            },
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: text.to_string(),
                partial: with_text.clone(),
            },
            AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: text.to_string(),
                partial: with_text.clone(),
            },
            AssistantMessageEvent::Done {
                reason: pi_ai::types::StopReason::Stop,
                message: done_message(vec![AssistantContent::Text(TextContent {
                    text: text.to_string(),
                    text_signature: None,
                })]),
            },
        ]
    }

    /// A reply that issues `calls`, then nothing else.
    pub fn tool_call_reply(calls: &[(&str, &str, JsonObject)]) -> Vec<AssistantMessageEvent> {
        let mut events = vec![AssistantMessageEvent::Start { partial: partial() }];
        let mut blocks = Vec::new();
        for (id, name, arguments) in calls {
            let block = AssistantContent::ToolCall(pi_ai::types::ToolCall {
                id: (*id).to_string(),
                name: (*name).to_string(),
                arguments: arguments.clone(),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            });
            events.push(AssistantMessageEvent::ToolcallStart {
                content_index: blocks.len(),
                partial: partial_with(blocks.clone()),
            });
            events.push(AssistantMessageEvent::ToolcallEnd {
                content_index: blocks.len(),
                tool_call: match &block {
                    AssistantContent::ToolCall(call) => call.clone(),
                    _ => unreachable!(),
                },
                partial: partial_with({
                    blocks.push(block);
                    blocks.clone()
                }),
            });
        }
        events.push(AssistantMessageEvent::Done {
            reason: pi_ai::types::StopReason::ToolUse,
            message: done_message(blocks),
        });
        events
    }

    /// A reply cut off by the output token limit.
    pub fn truncated_tool_call_reply() -> Vec<AssistantMessageEvent> {
        let block = AssistantContent::ToolCall(pi_ai::types::ToolCall {
            id: "call_1".to_string(),
            name: "bash".to_string(),
            arguments: JsonObject::new(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        });
        vec![
            AssistantMessageEvent::Start { partial: partial() },
            AssistantMessageEvent::ToolcallStart {
                content_index: 0,
                partial: partial(),
            },
            AssistantMessageEvent::ToolcallEnd {
                content_index: 0,
                tool_call: match &block {
                    AssistantContent::ToolCall(call) => call.clone(),
                    _ => unreachable!(),
                },
                partial: partial_with(vec![block.clone()]),
            },
            AssistantMessageEvent::Done {
                reason: pi_ai::types::StopReason::Length,
                message: AssistantMessage {
                    content: vec![block],
                    stop_reason: pi_ai::types::StopReason::Length,
                    ..done_message(Vec::new())
                },
            },
        ]
    }

    /// A transport failure.
    pub fn error_reply(message: &str) -> Vec<AssistantMessageEvent> {
        vec![AssistantMessageEvent::Error {
            reason: pi_ai::types::StopReason::Error,
            error: AssistantMessage {
                stop_reason: pi_ai::types::StopReason::Error,
                error_message: Some(message.to_string()),
                ..done_message(Vec::new())
            },
        }]
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().expect("requests").len()
    }
}

impl StreamFn for ScriptedStream {
    fn stream(&self, request: StreamRequest) -> BoxStream<'static, AssistantMessageEvent> {
        self.requests.lock().expect("requests").push(request);
        let mut responses = self.responses.lock().expect("responses");
        let next = if responses.is_empty() {
            Vec::new()
        } else {
            responses.remove(0)
        };
        futures::stream::iter(next).boxed()
    }
}

/// A provider that throws on its first run and answers normally after.
///
/// A panic is the closest thing this port has to the `throw` pi catches in
/// `runWithLifecycle` (`agent.ts:519-528`).
pub struct ThrowingStream {
    thrown: AtomicBool,
}

impl ThrowingStream {
    pub fn new() -> Arc<ThrowingStream> {
        Arc::new(ThrowingStream {
            thrown: AtomicBool::new(false),
        })
    }
}

impl StreamFn for ThrowingStream {
    fn stream(&self, _request: StreamRequest) -> BoxStream<'static, AssistantMessageEvent> {
        if !self.thrown.swap(true, Ordering::SeqCst) {
            return futures::stream::once(async { panic!("the provider threw") }).boxed();
        }
        futures::stream::iter(ScriptedStream::text_reply("after the throw")).boxed()
    }
}

/// A provider whose first request never finishes: it emits a tool call and
/// then stalls forever, which is the mid-turn state a dropped prompt future
/// lands in. pi's `prompt()` returns a promise nobody can cancel, so pi never
/// has to survive this; a Rust future is dropped here on purpose.
pub struct StallingStream {
    stalled: AtomicBool,
}

impl StallingStream {
    pub fn new() -> Arc<StallingStream> {
        Arc::new(StallingStream {
            stalled: AtomicBool::new(false),
        })
    }
}

impl StreamFn for StallingStream {
    fn stream(&self, _request: StreamRequest) -> BoxStream<'static, AssistantMessageEvent> {
        if !self.stalled.swap(true, Ordering::SeqCst) {
            let events =
                ScriptedStream::tool_call_reply(&[("call_1", "bash", serde_json::Map::new())]);
            return futures::stream::iter(events)
                .chain(futures::stream::pending())
                .boxed();
        }
        futures::stream::iter(ScriptedStream::text_reply("after the drop")).boxed()
    }
}

pub fn test_model() -> Model {
    Model {
        id: "test-model".to_string(),
        name: "Test Model".to_string(),
        api: "openai-completions".to_string(),
        provider: "opencode".to_string(),
        base_url: "https://example.invalid/v1".to_string(),
        r#type: Some("chat".to_string()),
        context_window: 128_000,
        max_tokens: 4096,
        reasoning: false,
        input: vec![pi_ai::types::ModelInputModality::Text],
        ..Default::default()
    }
}

pub fn partial() -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        stop_reason: pi_ai::types::StopReason::Pending,
        ..done_message(Vec::new())
    }
}

pub fn partial_with(content: Vec<AssistantContent>) -> AssistantMessage {
    AssistantMessage {
        content,
        stop_reason: pi_ai::types::StopReason::Pending,
        ..done_message(Vec::new())
    }
}

pub fn done_message(content: Vec<AssistantContent>) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "openai-completions".to_string(),
        provider: "opencode".to_string(),
        model: "test-model".to_string(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        thinking_level: None,
        diagnostics: Vec::new(),
        usage: Usage {
            input: 10,
            output: 5,
            ..Usage::default()
        },
        stop_reason: pi_ai::types::StopReason::Stop,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1_700_000_000_000,
        duration_ms: None,
    }
}

/// Records the calls it is asked to run and returns scripted results.
#[derive(Default)]
pub struct ScriptedTools {
    pub calls: Mutex<Vec<(String, JsonObject)>>,
    pub results: Mutex<Vec<Result<AgentToolResult, String>>>,
    /// Publish this many updates before returning, to exercise
    /// `tool_execution_update`.
    pub updates: Mutex<Vec<AgentToolResult>>,
    pub specs: Mutex<Vec<ToolSpec>>,
    pub execution_modes: Mutex<Vec<(String, pi_agent::ToolExecutionMode)>>,
    /// Leave the next call unresolved, so the run is mid-tool when a future
    /// that owns it is dropped.
    pub stalling: AtomicBool,
}

impl ScriptedTools {
    pub fn new() -> Arc<ScriptedTools> {
        Arc::new(ScriptedTools::default())
    }

    pub fn with_tool(name: &str) -> Arc<ScriptedTools> {
        let tools = ScriptedTools::new();
        tools.push_tool(name);
        tools
    }

    pub fn push_tool(&self, name: &str) {
        self.specs.lock().expect("specs").push(ToolSpec {
            name: name.to_string(),
            description: format!("{name} tool"),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
            }),
            execution_mode: None,
            constrained_sampling: None,
        });
    }

    pub fn set_execution_mode(&self, name: &str, mode: pi_agent::ToolExecutionMode) {
        self.execution_modes
            .lock()
            .expect("modes")
            .push((name.to_string(), mode));
    }

    pub fn push_result(&self, result: Result<AgentToolResult, String>) {
        self.results.lock().expect("results").push(result);
    }

    pub fn call_names(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().expect("calls").len()
    }

    /// Make the next call hang forever, then answer normally.
    pub fn stall_next_call(&self) {
        self.stalling.store(true, Ordering::SeqCst);
    }
}

impl ToolExecutor for ScriptedTools {
    fn tools(&self) -> Vec<ToolSpec> {
        self.specs.lock().expect("specs").clone()
    }

    fn execution_mode(&self, name: &str) -> Option<pi_agent::ToolExecutionMode> {
        self.execution_modes
            .lock()
            .expect("modes")
            .iter()
            .find(|(tool, _)| tool == name)
            .map(|(_, mode)| *mode)
    }

    fn execute(
        &self,
        call: AgentToolCall,
        updates: tokio::sync::mpsc::UnboundedSender<AgentToolResult>,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        self.calls
            .lock()
            .expect("calls")
            .push((call.name.clone(), call.arguments.clone()));
        for update in self.updates.lock().expect("updates").drain(..) {
            let _ = updates.send(update);
        }
        if self.stalling.swap(false, Ordering::SeqCst) {
            return Box::pin(futures::future::pending());
        }
        let mut results = self.results.lock().expect("results");
        let result = if results.is_empty() {
            Ok(text_result(&format!("ran {}", call.name)))
        } else {
            results.remove(0)
        };
        Box::pin(async move { result })
    }
}

pub fn text_result(text: &str) -> AgentToolResult {
    AgentToolResult {
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        details: serde_json::json!({}),
        ..AgentToolResult::default()
    }
}

/// Collects every event, in order, while the agent runs, plus the abort signal
/// the listener was handed for each one (pi passes the run's signal to every
/// listener — `agent.ts:266`).
#[derive(Default, Clone)]
pub struct Recorder {
    pub events: Arc<Mutex<Vec<AgentEvent>>>,
    signals: Arc<Mutex<Vec<AbortSignal>>>,
}

impl Recorder {
    pub fn new() -> Recorder {
        Recorder {
            events: Arc::new(Mutex::new(Vec::new())),
            signals: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn attach(&self, agent: &mut Agent) {
        let events = self.events.clone();
        let signals = self.signals.clone();
        agent.subscribe(move |event: &AgentEvent, signal: &AbortSignal| {
            events.lock().expect("events").push(event.clone());
            signals.lock().expect("signals").push(signal.clone());
        });
    }

    /// The signal handed to the listener for event `index`.
    pub fn signal_for(&self, index: usize) -> Option<AbortSignal> {
        self.signals.lock().expect("signals").get(index).cloned()
    }

    /// True when every event so far was handed an aborted signal.
    pub fn every_signal_aborted(&self) -> bool {
        !self.signals.lock().expect("signals").is_empty()
            && self
                .signals
                .lock()
                .expect("signals")
                .iter()
                .all(AbortSignal::aborted)
    }

    pub fn kinds(&self) -> Vec<&'static str> {
        self.events
            .lock()
            .expect("events")
            .iter()
            .map(AgentEvent::kind)
            .collect()
    }

    pub fn all(&self) -> Vec<AgentEvent> {
        self.events.lock().expect("events").clone()
    }
}

/// Drives the loop directly, without the `Agent` facade.
pub struct LoopHarness {
    pub executor: Arc<dyn ToolExecutor>,
    pub stream: Arc<ScriptedStream>,
    pub collector: pi_agent::CollectingSink,
}

impl LoopHarness {
    pub fn new(executor: Arc<dyn ToolExecutor>, stream: Arc<ScriptedStream>) -> LoopHarness {
        LoopHarness {
            executor,
            stream,
            collector: pi_agent::CollectingSink::default(),
        }
    }

    pub async fn run(&mut self, prompts: Vec<AgentMessage>) -> Vec<AgentMessage> {
        self.run_with_config(prompts, pi_agent::AgentLoopConfig::default())
            .await
    }

    pub async fn run_with_config(
        &mut self,
        prompts: Vec<AgentMessage>,
        config: pi_agent::AgentLoopConfig,
    ) -> Vec<AgentMessage> {
        let mut config = config;
        config.model = test_model();
        let mut queues = TestQueues::default();
        pi_agent::run_agent_loop(
            prompts,
            pi_agent::AgentContext::default(),
            self.executor.as_ref(),
            &config,
            &mut queues,
            &mut self.collector,
            &AbortSignal::new(),
            self.stream.as_ref(),
        )
        .await
    }

    pub fn kinds(&self) -> Vec<&'static str> {
        pi_agent::event_kinds(&self.collector.events)
    }
}

#[derive(Default)]
pub struct TestQueues {
    pub steering: pi_agent::PendingMessageQueue,
    pub follow_up: pi_agent::PendingMessageQueue,
}

impl TestQueues {
    pub fn with_queue_mode(mode: QueueMode) -> TestQueues {
        TestQueues {
            steering: pi_agent::PendingMessageQueue::new(mode),
            follow_up: pi_agent::PendingMessageQueue::new(mode),
        }
    }
}

impl pi_agent::QueueSource for TestQueues {
    fn drain_steering(&mut self) -> Vec<AgentMessage> {
        pi_agent::PendingMessageQueue::drain(&mut self.steering)
    }

    fn drain_follow_up(&mut self) -> Vec<AgentMessage> {
        pi_agent::PendingMessageQueue::drain(&mut self.follow_up)
    }
}

/// Turn a message list into `"role:summary"` strings, which is what most
/// assertions care about.
pub fn summarize(messages: &[AgentMessage]) -> Vec<String> {
    messages
        .iter()
        .map(|message| match message {
            AgentMessage::System(system) => {
                let prompt = pi_agent::current_system_prompt(std::slice::from_ref(message));
                let tools = system
                    .tools_added
                    .as_ref()
                    .map(|tools| {
                        tools
                            .iter()
                            .map(|tool| tool.name.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                format!("system[{}]({tools})", prompt.trim())
            }
            AgentMessage::User(user) => match &user.content {
                UserContent::Text(text) => format!("user[{text}]"),
                UserContent::Blocks(blocks) => {
                    format!("user[{}]", message_text(blocks))
                }
            },
            AgentMessage::Assistant(assistant) => format!(
                "assistant[{}]{}",
                assistant
                    .content
                    .iter()
                    .filter_map(|block| block.as_text().map(str::to_string))
                    .collect::<String>(),
                if assistant.stop_reason == pi_ai::types::StopReason::Stop {
                    String::new()
                } else {
                    format!(":{}", assistant.stop_reason.as_str())
                }
            ),
            AgentMessage::ToolResult(result) => format!(
                "toolResult[{}={}{}]",
                result.tool_name,
                result
                    .content
                    .iter()
                    .filter_map(|block| block.as_text().map(str::to_string))
                    .collect::<String>(),
                if result.is_error { "!" } else { "" }
            ),
            other => format!("{}(?)", other.role()),
        })
        .collect()
}

pub fn user(text: &str) -> AgentMessage {
    pi_agent::transcript::user_message(text, 1_700_000_000_000)
}

fn message_text(blocks: &[InputContent]) -> String {
    blocks
        .iter()
        .filter_map(|block| block.as_text().map(str::to_string))
        .collect::<String>()
}

/// Collect into a sink that forwards to the recorder, for hosts that need both.
pub struct TeeSink<'a> {
    pub first: &'a mut Recorder,
}

impl AgentEventSink for TeeSink<'_> {
    fn emit(&mut self, event: AgentEvent) -> BoxFuture<'_, ()> {
        self.first.events.lock().expect("events").push(event);
        Box::pin(async {})
    }
}
