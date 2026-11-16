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
        if aborted {
            return false;
        }
        if !self.settings.enabled || self.attempt >= self.max_attempts() {
            return false;
        }
        last_assistant(messages)
            .map(|assistant| pi_ai::retry::is_retryable_assistant_error(assistant))
            .unwrap_or(false)
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
        let mut disabled = AgentRetry::new(settings(false));
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
        let mut fresh = AgentRetry::new(settings(true));
        assert!(!fresh.will_retry(false, &quota));
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
