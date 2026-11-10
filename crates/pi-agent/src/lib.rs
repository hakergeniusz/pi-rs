//! Agent runtime: the loop, tool calling, state management, steering.
//!
//! Rust port of `@mariozechner/pi-agent-core` (TS source:
//! `packages/agent/src`: `agent.ts`, `agent-loop.ts`, `types.ts`,
//! `stream-fn.ts`, `proxy.ts`).
//!
//! The crate owns the loop and nothing else. The two seams it is written
//! against are [`types::StreamFn`] (the provider) and [`types::ToolExecutor`]
//! (the tools), so the loop is testable end to end without a network or a
//! process. `pi-coding-agent` is the host; it wires a real provider and the
//! built-in tools.
//!
//! Layout:
//!
//! * [`agent_message`] — the transcript message union (`AgentMessage`).
//! * [`types`] — the seams, hooks, queues, and value types.
//! * [`events`] — the `AgentEvent` union every consumer observes.
//! * [`agent_loop`] — the loop itself (`run_agent_loop`, `run_agent_loop_continue`).
//! * [`transcript`] — system-prompt and tool-declaration replay.
//! * [`agent`] — the stateful facade (`Agent`).
//! * [`state`] — pi's `Agent.state`, and the transitions that mutate it.
//! * [`queues`] — pi's steering and follow-up queues, their transitions, and
//!   the out-of-band [`queues::QueueHandle`] a caller steers through.
//! * [`live_settings`] — the model and reasoning level the loop reads per
//!   request, and the out-of-band [`live_settings::SettingsHandle`] a caller
//!   writes them through mid-run.
//! * [`json`] — serde for the crate's value types in pi's wire shapes, so
//!   hosts encode events instead of reaching for `Debug`.

#![forbid(unsafe_code)]

pub mod agent;
pub mod agent_loop;
pub mod agent_message;
pub mod events;
pub mod json;
pub mod live_settings;
pub mod queues;
pub mod state;
pub mod transcript;
pub mod types;

pub use agent::Agent;
pub use agent_loop::empty_assistant_message;
pub use agent_loop::{
    AgentLoopConfig, AsyncConvertToLlm, ConvertToLlm, DefaultConvertToLlm, FinishTurn,
    PrepareNextTurn, PrepareRequest, TransformContext, default_convert_to_llm, run_agent_loop,
    run_agent_loop_continue,
};
pub use events::{AgentEvent, AgentEventSink, CollectingSink, event_kinds};
pub use live_settings::SettingsHandle;
pub use queues::{ContinueDrain, QueueHandle, Queues};
pub use state::AgentState;
pub use transcript::{
    create_initial_system_message, current_system_message, current_system_prompt, current_tools,
    declare_tool_changes, tool_declaration, tool_state_changes, user_message,
    user_message_with_images,
};
pub use types::{
    AbortSignal, AfterToolCall, AfterToolCallOverride, AgentContext, AgentToolCall,
    AgentToolResult, AgentTurnContext, BeforeToolCall, BeforeToolCallDecision, BoxFuture,
    ConstrainedSampling, GrammarFormat, GrammarVariants, MessageQueue, PendingMessageQueue,
    PrepareNextTurnContext, PrepareRequestContext, QueueMode, QueueSource, StreamFn, StreamRequest,
    StrictMode, ThinkingLevel, ToolCallHooks, ToolExecutionMode, ToolExecutor, ToolSpec,
    TurnDecision, TurnUpdate,
};

pub use agent_message::{
    AgentMessage, BashExecutionMessage, BranchSummaryMessage, CompactionSummaryMessage,
    CustomMessage,
};
