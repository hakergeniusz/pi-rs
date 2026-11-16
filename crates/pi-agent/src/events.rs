//! `AgentEvent` — the agent's public event stream.
//!
//! Ground truth: pi-mono `packages/agent/src/types.ts`, the `AgentEvent` union
//! at the bottom of the file, plus the two shapes the JSON protocol adds on top
//! (`agent_end.willRetry` and `agent_settled` live in
//! `packages/coding-agent/src/core/agent-session.ts`).

use crate::agent_message::AgentMessage;
use pi_ai::types::AssistantMessage;
use pi_ai::types::AssistantMessageEvent;
use pi_ai::types::JsonObject;

use crate::types::AgentToolResult;

/// Events emitted by the agent. `AgentEnd` is the last event of a run; the
/// agent only becomes idle after its listeners settle.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    // Agent lifecycle
    AgentStart,
    AgentEnd {
        messages: Vec<AgentMessage>,
    },
    // Turn lifecycle — one assistant response plus its tool calls and results
    TurnStart,
    TurnEnd {
        message: AgentMessage,
        tool_results: Vec<AgentMessage>,
    },
    // Message lifecycle — system, user, assistant, and tool-result messages
    MessageStart {
        message: AgentMessage,
    },
    /// Only emitted for assistant messages during streaming.
    MessageUpdate {
        message: std::sync::Arc<AssistantMessage>,
        assistant_message_event: AssistantMessageEvent,
    },
    MessageEnd {
        message: AgentMessage,
    },
    // Tool execution lifecycle.
    //
    // `parent_tool_call_id` is pi's `parentToolCallId`
    // (`nested-tool-calls.ts:112-128`): a nested call's three events name the
    // model-issued call that reached it through `ctx.executeTool()`, and the
    // loop's own calls carry `None` — pi omits the key entirely there.
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: JsonObject,
        parent_tool_call_id: Option<String>,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: JsonObject,
        partial_result: AgentToolResult,
        parent_tool_call_id: Option<String>,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
        parent_tool_call_id: Option<String>,
    },
    // Auto-retry lifecycle (`agent-session.ts:235-237`): the retry machine
    // schedules a re-run around a transient provider error.
    AutoRetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    AutoRetryEnd {
        success: bool,
        attempt: u32,
        final_error: Option<String>,
    },
    // Compaction lifecycle (`agent-session.ts:223-248`): manual `/compact`
    // and the automatic threshold/overflow machine share the reason enum;
    // `result` is absent on failure or abortion.
    CompactionStart {
        reason: CompactionReason,
    },
    CompactionEnd {
        reason: CompactionReason,
        result: Option<CompactionResultEvent>,
        aborted: bool,
        will_retry: bool,
        error_message: Option<String>,
    },
    // A session entry landed on the branch while the agent-session drove
    // it (`agent-session.ts:224`): the virtual-model-state custom entries,
    // boundary drafts, recovery omissions, and the cache warmer. Regular
    // messages and model/thinking/compaction records surface through
    // their own events instead.
    EntryAppended {
        /// The serialized entry record, pi's on-disk shape.
        entry: serde_json::Value,
    },
}

/// The compaction trigger (`agent-session.ts:223`): manual `/compact`, the
/// automatic threshold, or overflow recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

impl CompactionReason {
    /// The wire discriminant, identical to pi's reason strings.
    pub fn as_str(&self) -> &'static str {
        match self {
            CompactionReason::Manual => "manual",
            CompactionReason::Threshold => "threshold",
            CompactionReason::Overflow => "overflow",
        }
    }
}

/// The successful `compaction_end` result payload (`agent-session.ts`
/// `CompactionResult`): summary, the retained range's first entry, the
/// token estimate before, the post-compaction estimate, the summarization
/// usage, and the extension details object.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CompactionResultEvent {
    pub summary: String,
    #[serde(rename = "firstKeptEntryId")]
    pub first_kept_entry_id: String,
    #[serde(rename = "tokensBefore")]
    pub tokens_before: u64,
    #[serde(rename = "estimatedTokensAfter")]
    pub estimated_tokens_after: Option<u64>,
    pub usage: Option<pi_ai::types::Usage>,
    pub details: serde_json::Value,
}

impl AgentEvent {
    /// The wire type name, identical to pi's discriminant strings.
    pub fn kind(&self) -> &'static str {
        match self {
            AgentEvent::AgentStart => "agent_start",
            AgentEvent::AgentEnd { .. } => "agent_end",
            AgentEvent::TurnStart => "turn_start",
            AgentEvent::TurnEnd { .. } => "turn_end",
            AgentEvent::MessageStart { .. } => "message_start",
            AgentEvent::MessageUpdate { .. } => "message_update",
            AgentEvent::MessageEnd { .. } => "message_end",
            AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
            AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
            AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
            AgentEvent::AutoRetryStart { .. } => "auto_retry_start",
            AgentEvent::AutoRetryEnd { .. } => "auto_retry_end",
            AgentEvent::CompactionStart { .. } => "compaction_start",
            AgentEvent::CompactionEnd { .. } => "compaction_end",
            AgentEvent::EntryAppended { .. } => "entry_appended",
        }
    }
}

/// Sink the loop emits through. Awaiting a call means the listeners have run,
/// which is what makes the ordering guarantees in the goldens hold.
pub trait AgentEventSink {
    fn emit(&mut self, event: AgentEvent) -> crate::types::BoxFuture<'_, ()>;
}

/// Collects events into a list. Used by tests and by pi-cli's JSON mode.
#[derive(Debug, Default)]
pub struct CollectingSink {
    pub events: Vec<AgentEvent>,
}

impl AgentEventSink for CollectingSink {
    fn emit(&mut self, event: AgentEvent) -> crate::types::BoxFuture<'_, ()> {
        self.events.push(event);
        Box::pin(async {})
    }
}

/// Convenience for tests: the sequence of event type names.
pub fn event_kinds(events: &[AgentEvent]) -> Vec<&'static str> {
    events.iter().map(AgentEvent::kind).collect()
}
