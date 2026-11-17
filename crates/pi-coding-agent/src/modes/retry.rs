//! The auto-retry machine (`agent-session.ts`'s retry state machine +
//! `pi-ai`'s `retryAssistantCall` policy), shared by the print/JSON/RPC run
//! tails.
//!
//! Upstream owns this in `agent-session.ts` (`_retryAttempt`,
//! `_willRetryAfterAgentEnd` `:1266-1281`, `_handlePostAgentRun`
//! `:2060-2094`): after a run, the last assistant message decides — a
//! retryable error within budget schedules a backoff and re-runs the same
//! prompt; success, a deterministic error, budget exhaustion, or an abort
//! ends the machine. The port spreads the agent-session across the modes, so
//! the controller lives here and each run tail drives it. The interactive
//! pump's integration lands with its `AgentRun` rework (the failed response
//! is not persisted on either side).

use pi_agent::AgentMessage;
use pi_agent::types::AbortSignal;
use pi_ai::types::StopReason;

use crate::config::settings::RetrySettings;
use crate::session::records::ContextReplacement;
use std::sync::Arc;

use pi_agent::events::{AgentEvent, AgentEventSink};

/// One retry budget over a run (attempts reset when a run succeeds).
pub struct AgentRetry {
    settings: RetrySettings,
    attempt: u32,
}

impl AgentRetry {
    pub fn new(settings: RetrySettings) -> Self {
        Self {
            settings,
            attempt: 0,
        }
    }

    /// `_willRetryAfterAgentEnd` (`agent-session.ts:1266-1281`): the last
    /// assistant message of the run decides; aborts never retry.
    pub fn will_retry(&self, aborted: bool, messages: &[AgentMessage]) -> bool {
        will_retry_after_agent_end(aborted, self.attempt, &self.settings, messages)
    }

    /// Advance the attempt and compute the backoff. Returns
    /// `(attempt, maxAttempts, delayMs, errorMessage)` for
    /// `auto_retry_start`.
    pub fn schedule(&mut self, messages: &[AgentMessage]) -> (u32, u32, u64, String) {
        self.attempt += 1;
        let policy = pi_ai::retry::RetryPolicy {
            enabled: self.settings.enabled,
            max_retries: self.max_attempts(),
            base_delay_ms: self.settings.base_delay_ms.max(0) as u64,
            max_agent_delay_ms: Some(self.settings.max_agent_delay_ms.max(0) as u64),
        };
        let delay_ms = pi_ai::retry::retry_delay_ms(&policy, self.attempt);
        (
            self.attempt,
            self.max_attempts(),
            delay_ms,
            last_error_text(messages),
        )
    }

    /// Attempts made so far (`agent-session.ts` `_retryAttempt`).
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    pub fn max_attempts(&self) -> u32 {
        self.settings.max_retries.max(0) as u32
    }

    /// The backoff sleep, abortable (`retry.ts` races the sleep against the
    /// signal; the port polls in 50 ms slices). Returns `false` when aborted.
    pub fn sleep(&self, delay_ms: u64, signal: &AbortSignal) -> bool {
        sleep_abortable(delay_ms, signal)
    }

    /// Reset the budget after a successful run (`agent-session.ts:1252`).
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// The last assistant message of a run (`agent-session.ts:1270-1277`
/// scans backwards).
fn last_assistant(messages: &[AgentMessage]) -> Option<&pi_ai::types::AssistantMessage> {
    messages.iter().rev().find_map(|message| match message {
        AgentMessage::Assistant(assistant) => Some(assistant),
        _ => None,
    })
}

/// `lastRetry.errorMessage` — `errorMessage || "Unknown error"`
/// (`agent-session.ts:1213` region).
pub fn last_error_text(messages: &[AgentMessage]) -> String {
    last_assistant(messages)
        .and_then(|assistant| assistant.error_message.clone())
        .unwrap_or_else(|| "Unknown error".to_string())
}

/// Merges `willRetry` into the streamed `agent_end` (`agent-session.ts`
/// :1211 wraps the emit; the sink does it at the stream boundary). The
/// verdict reads the machine's current budget — the same inputs upstream's
/// `_willRetryAfterAgentEnd` (:1267-1281) uses. The core loop emits
/// `false`; without this wrapper the wire would never see a retry.
pub struct RetryAwareSink<'a, S: AgentEventSink> {
    pub inner: &'a mut S,
    /// The machine's attempt count so far, copied into the run thread.
    pub retry_attempt: u32,
    pub settings: RetrySettings,
    pub signal: Arc<AbortSignal>,
}

impl<S: AgentEventSink> AgentEventSink for RetryAwareSink<'_, S> {
    fn emit(&mut self, event: AgentEvent) -> pi_agent::types::BoxFuture<'_, ()> {
        let event = match event {
            AgentEvent::AgentEnd { messages, .. } => AgentEvent::AgentEnd {
                will_retry: will_retry_after_agent_end(
                    self.signal.aborted(),
                    self.retry_attempt,
                    &self.settings,
                    &messages,
                ),
                messages,
            },
            event => event,
        };
        self.inner.emit(event)
    }
}

/// The backoff sleep, abortable (`retry.ts` races the sleep against the
/// signal; the port polls in 50 ms slices). Returns `false` when aborted.
pub fn sleep_abortable(delay_ms: u64, signal: &AbortSignal) -> bool {
    let mut remaining = delay_ms;
    while remaining > 0 {
        if signal.aborted() {
            return false;
        }
        let step = remaining.min(50);
        crate::runtime::block_on(tokio::time::sleep(std::time::Duration::from_millis(step)));
        remaining -= step;
    }
    !signal.aborted()
}

/// `_willRetryAfterAgentEnd` (`agent-session.ts:1267-1281`): the emit
/// wrapper's verdict — aborts, the budget, and the last assistant decide.
pub fn will_retry_after_agent_end(
    aborted: bool,
    retry_attempt: u32,
    settings: &RetrySettings,
    messages: &[AgentMessage],
) -> bool {
    if aborted {
        return false;
    }
    if !settings.enabled || retry_attempt >= settings.max_retries.max(0) as u32 {
        return false;
    }
    last_assistant(messages).is_some_and(pi_ai::retry::is_retryable_assistant_error)
}

/// `_omitRecoveryAttempt` (`agent-session.ts:1311-1336`): the failed
/// attempt stays in raw history but is omitted from the model projection.
/// Upstream persists messages on `MessageEnd` mid-run; the port's tails
/// persist after the loop, so the omission persists the failed assistant
/// and its orphaned tool results here, then appends the `context_edit`
/// omission records. Returns the serialized edit records for the
/// `entry_appended` emissions (:1336).
pub fn omit_attempt(
    messages: &[AgentMessage],
    session: &mut crate::session::SessionManager,
) -> Vec<serde_json::Value> {
    let Some(index) = messages
        .iter()
        .rposition(|message| matches!(message, AgentMessage::Assistant(_)))
    else {
        return Vec::new();
    };
    let AgentMessage::Assistant(assistant) = &messages[index] else {
        return Vec::new();
    };
    let tool_call_ids: std::collections::HashSet<&str> = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            pi_ai::types::AssistantContent::ToolCall(call) => Some(call.id.as_str()),
            _ => None,
        })
        .collect();
    let mut targets: Vec<&AgentMessage> = vec![&messages[index]];
    for message in &messages[index + 1..] {
        match message {
            AgentMessage::Assistant(_) => break,
            AgentMessage::ToolResult(result)
                if tool_call_ids.contains(result.tool_call_id.as_str()) =>
            {
                targets.push(message);
            }
            _ => {}
        }
    }
    let mut records = Vec::new();
    for target in targets {
        let Ok(entry) = session.append_message(target.clone()) else {
            continue;
        };
        let entry_id = entry.id().to_string();
        if let Ok(edit_id) = session.append_context_edit(&entry_id, &ContextReplacement::Omit)
            && let Some(edit_entry) = session.get_entry(&edit_id)
        {
            records.push(serde_json::Value::Object(
                edit_entry.to_record().object().clone(),
            ));
        }
    }
    records
}

/// Whether the run ended in success (`stopReason !== "error"`, the
/// `auto_retry_end` success rule at `agent-session.ts:1246`).
pub fn run_succeeded(messages: &[AgentMessage]) -> bool {
    match last_assistant(messages) {
        Some(assistant) => assistant.stop_reason != StopReason::Error,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::types::AssistantMessage;

    fn assistant_message(stop_reason: StopReason, error: Option<&str>) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            error_message: error.map(str::to_string),
            ..test_assistant(stop_reason)
        })
    }

    // Struct literal per pi-ai's `event_stream.rs` helper (the wire shape has
    // required fields beyond the ones under test).
    fn test_assistant(stop_reason: StopReason) -> AssistantMessage {
        AssistantMessage {
            content: vec![],
            api: "openai-completions".into(),
            provider: "test".into(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: vec![],
            usage: pi_ai::types::Usage::default(),
            stop_reason,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
            duration_ms: None,
        }
    }

    fn settings(enabled: bool) -> RetrySettings {
        RetrySettings {
            enabled,
            max_retries: 3,
            base_delay_ms: 2000,
            max_agent_delay_ms: 60_000,
        }
    }

    #[test]
    fn retryable_error_within_budget_schedules() {
        let mut retry = AgentRetry::new(settings(true));
        let messages = vec![assistant_message(StopReason::Error, Some("HTTP 503"))];
        assert!(retry.will_retry(false, &messages));
        let (attempt, max_attempts, delay_ms, error) = retry.schedule(&messages);
        assert_eq!((attempt, max_attempts, delay_ms), (1, 3, 2000));
        assert_eq!(error, "HTTP 503");
        assert!(retry.will_retry(false, &messages));
        let (attempt, _, delay_ms, _) = retry.schedule(&messages);
        assert_eq!((attempt, delay_ms), (2, 4000));
    }

    #[test]
    fn quota_exhaustion_and_disabled_and_budget_end_the_machine() {
        let disabled = AgentRetry::new(settings(false));
        let messages = vec![assistant_message(StopReason::Error, Some("HTTP 503"))];
        assert!(!disabled.will_retry(false, &messages));

        let mut retry = AgentRetry::new(settings(true));
        for _ in 0..3 {
            retry.schedule(&messages);
        }
        assert!(!retry.will_retry(false, &messages)); // budget exhausted

        let quota = vec![assistant_message(
            StopReason::Error,
            Some("insufficient_quota"),
        )];
        let fresh = AgentRetry::new(settings(true));
        assert!(!fresh.will_retry(false, &quota));
    }

    #[test]
    fn omit_attempt_persists_raw_and_appends_the_projection_edits() {
        let mut session = crate::session::SessionManager::in_memory(
            std::path::Path::new("/tmp"),
            &crate::session::store::NewSessionOptions::default(),
            Vec::new(),
        )
        .expect("session");
        // A failed assistant with a tool call whose result follows it —
        // both persist raw, both drop from the projection (:1318-1336).
        let mut failed = test_assistant(StopReason::Error);
        failed.error_message = Some("HTTP 503".to_string());
        failed.content = vec![pi_ai::types::AssistantContent::ToolCall(
            pi_ai::types::ToolCall {
                id: "t1".to_string(),
                name: "read".to_string(),
                arguments: serde_json::Map::new(),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            },
        )];
        let result = AgentMessage::ToolResult(pi_ai::types::ToolResultMessage {
            tool_call_id: "t1".to_string(),
            tool_name: "read".to_string(),
            content: vec![pi_ai::types::InputContent::Text(
                pi_ai::types::TextContent {
                    text: "ok".to_string(),
                    text_signature: None,
                },
            )],
            details: None,
            usage: None,
            nested_calls: None,
            is_error: false,
            timestamp: 0,
            duration_ms: None,
        });
        let messages = vec![AgentMessage::Assistant(failed), result];
        let records = omit_attempt(&messages, &mut session);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["type"], "context_edit");
        assert_eq!(records[0]["replacement"], serde_json::Value::Null);
        let branch = session.get_branch(None).expect("branch");
        // Raw history keeps the attempt and its result...
        let message_entries = branch
            .iter()
            .filter(|entry| matches!(entry, crate::session::records::SessionEntry::Message(_)))
            .count();
        assert_eq!(message_entries, 2);
        // ...and the projection omits both.
        let projection = crate::session::store::build_session_projection_from(&branch, None);
        assert!(
            projection
                .entries
                .iter()
                .all(|entry| entry.messages.is_empty())
        );
        // Both edits omit their targets (JSON null replacements).
        assert_eq!(records[1]["type"], "context_edit");
        assert_eq!(records[1]["replacement"], serde_json::Value::Null);
    }

    #[test]
    fn success_resets_and_aborts_never_retry() {
        let mut retry = AgentRetry::new(settings(true));
        let failed = vec![assistant_message(StopReason::Error, Some("HTTP 503"))];
        assert!(retry.will_retry(false, &failed));
        retry.schedule(&failed);
        let ok = vec![assistant_message(StopReason::Stop, None)];
        assert!(run_succeeded(&ok));
        retry.reset();
        assert_eq!(retry.attempt(), 0);
        let aborted = vec![assistant_message(StopReason::Aborted, Some("HTTP 503"))];
        assert!(!retry.will_retry(true, &aborted));
        assert!(!retry.will_retry(false, &aborted)); // aborts are terminal
    }
}
