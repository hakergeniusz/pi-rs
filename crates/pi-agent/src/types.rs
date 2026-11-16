//! Agent types: the injected stream seam, the tool seam, and the loop config.
//!
//! Ground truth: pi-mono `packages/agent/src/types.ts` (+ `stream-fn.ts`).
//!
//! Two seams make the whole crate testable without a provider or a process:
//!
//! * [`StreamFn`] — pi's injected `streamFn`. It must never fail hard:
//!   request, model, and runtime failures come back as an
//!   `AssistantMessageEvent::Error` carrying a message with
//!   `stopReason: "error"`/`"aborted"`, exactly as `StreamFn`'s contract in
//!   `types.ts` requires.
//! * [`ToolExecutor`] — pi's `AgentTool.execute`. Errors are values: a tool that
//!   fails returns `is_error: true` (or an `Err`, which the loop converts into
//!   an error tool result), never a panic.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::agent_message::AgentMessage;
use futures::stream::BoxStream;
use pi_ai::types::JsonObject;
use pi_ai::types::{AssistantMessage, ContentBlock, ToolCall, Usage};
use serde::Serialize;
use serde_json::Value;

/// A boxed future, used instead of `async_trait` to keep the dependency list
/// exactly as scaffolded.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// pi's `ThinkingLevel` (`types.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    /// The wire name: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`.
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingLevel::Off => "off",
            ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "xhigh",
            ThinkingLevel::Max => "max",
        }
    }

    /// Parse a level from its wire name ([`ThinkingLevel::as_str`]).
    pub fn parse(value: &str) -> Option<ThinkingLevel> {
        match value {
            "off" => Some(ThinkingLevel::Off),
            "minimal" => Some(ThinkingLevel::Minimal),
            "low" => Some(ThinkingLevel::Low),
            "medium" => Some(ThinkingLevel::Medium),
            "high" => Some(ThinkingLevel::High),
            "xhigh" => Some(ThinkingLevel::Xhigh),
            "max" => Some(ThinkingLevel::Max),
            _ => None,
        }
    }

    /// The levels in pi's `EXTENDED_THINKING_LEVELS` order (`models.ts:1294`),
    /// lowest first.
    pub const EXTENDED_ORDER: [ThinkingLevel; 7] = [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
        ThinkingLevel::Max,
    ];
}

/// `clampThinkingLevel` (`packages/ai/src/models.ts:1315-1334`): snap a
/// requested level onto a model's supported levels — the level itself when
/// supported, else the nearest supported level by index distance
/// (high-before-low on ties from the requested index outward), else the
/// lowest supported level.
pub fn clamp_thinking_level(model: &pi_ai::types::Model, level: ThinkingLevel) -> ThinkingLevel {
    let available = pi_ai::get_supported_thinking_levels(model);
    let parse = |name: &str| ThinkingLevel::parse(name).unwrap_or(ThinkingLevel::Off);
    if available
        .iter()
        .any(|candidate| *candidate == level.as_str())
    {
        return level;
    }
    let requested = ThinkingLevel::EXTENDED_ORDER
        .iter()
        .position(|candidate| *candidate == level);
    let Some(requested) = requested else {
        return available
            .first()
            .map_or(ThinkingLevel::Off, |name| parse(name));
    };
    for candidate in ThinkingLevel::EXTENDED_ORDER.iter().skip(requested) {
        if available.contains(&candidate.as_str()) {
            return *candidate;
        }
    }
    for candidate in ThinkingLevel::EXTENDED_ORDER.iter().take(requested).rev() {
        if available.contains(&candidate.as_str()) {
            return *candidate;
        }
    }
    available
        .first()
        .map_or(ThinkingLevel::Off, |name| parse(name))
}

/// pi's `ToolExecutionMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolExecutionMode {
    Sequential,
    #[default]
    Parallel,
}

/// pi's `QueueMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

/// A `toolCall` content block of an assistant message.
pub type AgentToolCall = ToolCall;

/// pi's `AgentToolResult` (`types.ts`).
///
/// Wire shape: `content`/`details` always written; the optionals only when
/// set; `isError`/`terminate` only when true (TS drops the falsy values).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolResult {
    /// Text or image content returned to the model.
    pub content: Vec<ContentBlock>,
    /// Structured details for logs/UI; never sent to the model.
    pub details: Value,
    /// Machine-readable result matching the tool's `outputSchema`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    /// Usage from the tool execution itself, not from LLM context accounting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Report a failure without throwing.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
    /// Hint that the agent should stop after the current tool batch.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub terminate: bool,
}

impl Default for AgentToolResult {
    fn default() -> Self {
        AgentToolResult {
            content: Vec::new(),
            details: Value::Object(JsonObject::new()),
            structured_content: None,
            usage: None,
            is_error: false,
            terminate: false,
        }
    }
}

impl AgentToolResult {
    /// `createErrorToolResult`: a text result with empty details. pi does not
    /// set `isError` here — the caller carries it separately.
    pub fn error(message: impl Into<String>) -> AgentToolResult {
        AgentToolResult {
            content: vec![ContentBlock::Text(pi_ai::types::TextContent {
                text: message.into(),
                text_signature: None,
            })],
            ..AgentToolResult::default()
        }
    }
}

/// A tool as the model sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: Value,
    /// Per-tool override of the run's execution mode.
    pub execution_mode: Option<ToolExecutionMode>,
    /// pi's `constrainedSampling` (`types.ts:716-719`): what the provider is
    /// asked to enforce while the model writes the arguments. `None` is pi's
    /// `undefined` — no constraint, and no `constrainedSampling` key in the
    /// declaration at all.
    pub constrained_sampling: Option<ConstrainedSampling>,
}

/// Result of the after-hook merge (`AfterToolCallResult`).
#[derive(Debug, Clone, Default)]
pub struct AfterToolCallOverride {
    pub content: Option<Vec<ContentBlock>>,
    pub details: Option<Value>,
    pub structured_content: Option<Value>,
    pub is_error: Option<bool>,
    pub usage: Option<Usage>,
    pub terminate: Option<bool>,
}

/// Result of the before-hook (`BeforeToolCallResult`).
#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallDecision {
    pub block: bool,
    pub reason: Option<String>,
    pub terminate: bool,
}

/// `AgentTurnContext` (`types.ts`).
#[derive(Debug, Clone)]
pub struct AgentTurnContext {
    pub message: AgentMessage,
    pub tool_results: Vec<AgentMessage>,
    /// Messages this loop invocation will return if it exits here.
    pub new_messages: Vec<AgentMessage>,
}

/// `FinishTurn`'s decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnDecision {
    Continue,
    End,
}

/// `AgentLoopTurnUpdate` / `AgentRequestUpdate`.
#[derive(Debug, Clone, Default)]
pub struct TurnUpdate {
    /// Replacement context for the next request. pi's update is read as
    /// `requestUpdate.context ?? currentContext` (`agent-loop.ts:227`), so a
    /// host can swap the transcript the loop is about to send.
    pub context: Option<AgentContext>,
    /// Replacement model for the next request.
    pub model: Option<pi_ai::types::Model>,
    /// Replacement reasoning level; `Off` clears it.
    pub thinking_level: Option<ThinkingLevel>,
}

/// pi's `PrepareRequestContext` (`types.ts:173-177`): the runtime state
/// immediately before a conversational provider request. `context` is the
/// transcript as it stands after the pending and queued messages have been
/// appended, not the transcript as the turn began.
///
/// pi hands the hook the loop's own `currentContext`, so a hook that mutates
/// `request.context.messages` in place changes what the request carries
/// (`agent-loop.ts:219-225`); the borrow is what makes that work here. The
/// other half — a returned update replacing the context wholesale
/// (`agent-loop.ts:228`) — is unchanged.
#[derive(Debug)]
pub struct PrepareRequestContext<'a> {
    pub context: &'a mut AgentContext,
    pub model: pi_ai::types::Model,
    pub thinking_level: ThinkingLevel,
    /// The live run's abort signal (`prepareRequest(request, signal)`,
    /// `agent-loop.ts:222`); hooks hand it to routing and fetches.
    pub signal: &'a AbortSignal,
}

/// Context handed to `prepareNextTurn`.
#[derive(Debug, Clone)]
pub struct PrepareNextTurnContext {
    pub turn: AgentTurnContext,
}

/// Hooks the host can install around tool execution.
#[derive(Clone, Default)]
pub struct ToolCallHooks {
    pub before: Option<Arc<dyn BeforeToolCall>>,
    pub after: Option<Arc<dyn AfterToolCall>>,
}

impl std::fmt::Debug for ToolCallHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCallHooks")
            .field("before", &self.before.is_some())
            .field("after", &self.after.is_some())
            .finish()
    }
}

/// `beforeToolCall`.
pub trait BeforeToolCall: Send + Sync {
    fn call<'a>(
        &'a self,
        assistant_message: &'a AssistantMessage,
        tool_call: &'a AgentToolCall,
    ) -> BoxFuture<'a, Option<BeforeToolCallDecision>>;
}

/// `afterToolCall`.
pub trait AfterToolCall: Send + Sync {
    fn call<'a>(
        &'a self,
        assistant_message: &'a AssistantMessage,
        tool_call: &'a AgentToolCall,
        result: &'a AgentToolResult,
        is_error: bool,
    ) -> BoxFuture<'a, Option<AfterToolCallOverride>>;
}

/// The tool seam. `pi-tools` implements this in a later phase; the tests here
/// implement it directly.
pub trait ToolExecutor: Send + Sync {
    /// Tools this executor can run, in declaration order.
    fn tools(&self) -> Vec<ToolSpec>;

    /// Per-tool execution mode override, if any.
    fn execution_mode(&self, _name: &str) -> Option<ToolExecutionMode> {
        None
    }

    /// Execute one tool call. Partial results may be published on `updates` at
    /// any time; the loop drains them after the call settles and ignores any
    /// that arrive later, exactly like pi's `acceptingUpdates` flag.
    fn execute(
        &self,
        call: AgentToolCall,
        updates: tokio::sync::mpsc::UnboundedSender<AgentToolResult>,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>>;
}

/// Everything one provider request needs. Mirrors pi's `StreamFn` arguments.
#[derive(Debug, Clone)]
pub struct StreamRequest {
    pub model: pi_ai::types::Model,
    /// The normalized transcript: the system prompt and tool declarations travel
    /// as system messages, never as separate fields.
    pub messages: Vec<AgentMessage>,
    pub thinking_level: ThinkingLevel,
    pub session_id: Option<String>,
    /// Wire credential, already resolved through pi's auth ladder.
    pub api_key: Option<String>,
    /// OAuth bearer credential (pi's `accessToken`); wins over `api_key`.
    pub access_token: Option<String>,
}

/// The injected stream seam.
pub trait StreamFn: Send + Sync {
    /// Must not panic. Every failure mode — auth, transport, parse — must be
    /// encoded as a terminal `Error` event.
    fn stream(&self, request: StreamRequest) -> BoxStream<'static, AssistantMessageEvent>;
}

pub use pi_ai::types::AssistantMessageEvent;

// pi keeps `Tool.constrainedSampling` and the resolver that turns it into a
// wire format in one package (`packages/ai`), so the config type lives with
// the provider and is re-exported here for tools to declare.
pub use pi_ai::constrained_sampling::{
    ConstrainedSampling, GrammarFormat, GrammarVariants, StrictMode,
};

/// Cooperative cancellation, standing in for pi's `AbortSignal`.
///
/// pi threads a real `AbortSignal` into the stream function and every tool. The
/// loop here checks this flag at the same points pi checks `signal.aborted`;
/// dropping the stream stops the provider read, which is the same observable
/// behavior for the loop itself.
#[derive(Debug, Clone, Default)]
pub struct AbortSignal {
    aborted: Arc<std::sync::atomic::AtomicBool>,
}

impl AbortSignal {
    /// A signal that is not aborted.
    pub fn new() -> AbortSignal {
        AbortSignal::default()
    }

    /// Flag the signal: every check from now on reports aborted.
    pub fn abort(&self) {
        self.aborted
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether [`AbortSignal::abort`] has been called.
    pub fn aborted(&self) -> bool {
        self.aborted.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The flag behind this signal, for the APIs that take pi's bare
    /// `AbortSignal` (pi-rs's shell executor reads it on the same 50 ms poll).
    pub fn flag(&self) -> &std::sync::atomic::AtomicBool {
        &self.aborted
    }
}

/// pi's `AgentContext`.
#[derive(Debug, Clone, Default)]
pub struct AgentContext {
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<ToolSpec>,
}

/// A message queue drained by the loop (`PendingMessageQueue` in `agent.ts`).
pub trait MessageQueue: Send {
    fn drain(&mut self) -> Vec<AgentMessage>;
}

/// The two queues the loop polls, matching pi's `Agent` fields.
///
/// The two polls are the loop's `getSteeringMessages` / `getFollowUpMessages`
/// hooks (`agent.ts:500-503`). They hand back owned message lists, not
/// borrows, because pi's queues are plain arrays on an object any caller can
/// reach (`agent.ts:191-192`, `:302-309`) while the loop is running; the Rust
/// side of that is [`crate::queues::QueueHandle`], which drains through a lock
/// and so cannot lend a queue reference out from under it.
pub trait QueueSource: Send {
    /// `getSteeringMessages` (`agent.ts:500-502`): the messages this poll
    /// takes, mode-bounded by the steering queue's own mode.
    fn drain_steering(&mut self) -> Vec<AgentMessage>;

    /// `getFollowUpMessages` (`agent.ts:503`).
    fn drain_follow_up(&mut self) -> Vec<AgentMessage>;
}

/// pi's `PendingMessageQueue`, ported verbatim.
#[derive(Debug, Default)]
pub struct PendingMessageQueue {
    messages: Vec<AgentMessage>,
    pub mode: QueueMode,
}

impl MessageQueue for PendingMessageQueue {
    fn drain(&mut self) -> Vec<AgentMessage> {
        PendingMessageQueue::drain(self)
    }
}

impl PendingMessageQueue {
    /// An empty queue with the given drain mode.
    pub fn new(mode: QueueMode) -> PendingMessageQueue {
        PendingMessageQueue {
            messages: Vec::new(),
            mode,
        }
    }

    /// Append a message to the end of the queue.
    pub fn enqueue(&mut self, message: AgentMessage) {
        self.messages.push(message);
    }

    /// Whether any message is queued.
    pub fn has_items(&self) -> bool {
        !self.messages.is_empty()
    }

    /// `peek`: `all` drains everything, `one-at-a-time` only the oldest.
    pub fn peek(&self) -> Vec<AgentMessage> {
        match self.mode {
            QueueMode::All => self.messages.clone(),
            QueueMode::OneAtATime => self.messages.first().cloned().into_iter().collect(),
        }
    }

    /// Take what [`PendingMessageQueue::peek`] would return, removing it.
    pub fn drain(&mut self) -> Vec<AgentMessage> {
        let drained = self.peek();
        self.messages.drain(..drained.len());
        drained
    }

    /// Drop every queued message.
    pub fn clear(&mut self) {
        self.messages.clear();
    }
}
