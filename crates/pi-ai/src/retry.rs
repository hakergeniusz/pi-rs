//! Retry classification and policy (`packages/ai/src/utils/retry.ts`).
//!
//! The two error patterns port upstream's pattern arrays verbatim and are
//! compiled case-insensitively like `buildProviderErrorPattern`
//! (`retry.ts:6-8`). They use lookarounds (`retry.ts:62`, `retry.ts:66`,
//! `retry.ts:119`), so the engine is `fancy-regex` — the Rust `regex`
//! crate has no lookaround support.
//!
//! `retryAssistantCall`'s loop with the abortable backoff sleep lands with
//! the agent-session retry machine (E2 slice 6e part 2); this module is the
//! classification + policy surface that machine consumes.

use std::sync::LazyLock;

use crate::types::{AssistantMessage, StopReason};

/// `Number.MAX_SAFE_INTEGER` — the JS-side clamp for the computed delay.
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// `DEFAULT_MAX_AGENT_RETRY_DELAY_MS` (`retry.ts:151`).
pub const DEFAULT_MAX_AGENT_RETRY_DELAY_MS: u64 = 60_000;

/// `RetryPolicy` (`retry.ts:143-150`): bounded attempts with exponential
/// backoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    pub enabled: bool,
    /// Max retry attempts (0 = no retries). The initial call never counts.
    pub max_retries: u32,
    /// Per-attempt delay is `baseDelayMs * 2^(attempt-1)` before the cap.
    pub base_delay_ms: u64,
    /// Cap for agent-level retry delays; defaults to
    /// [`DEFAULT_MAX_AGENT_RETRY_DELAY_MS`].
    pub max_agent_delay_ms: Option<u64>,
}

/// `NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERN` (`retry.ts:10-58`):
/// subscription/account limits and quota/billing exhaustion — deterministic
/// failures that must never be retried.
static NON_RETRYABLE_PATTERN: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::RegexBuilder::new(
        &[
            // OpenCode Go/free-tier limits returned as 429 JSON error types.
            r"GoUsageLimitError",
            r"FreeUsageLimitError",
            // OpenCode Go subscription-limit text.
            r"Monthly usage limit reached",
            r"available balance",
            // Generic quota/budget/billing exhaustion.
            r"insufficient_quota",
            r"out of budget",
            r"quota exceeded",
            r"billing",
            // Sign in with ChatGPT shared/admin usage limits.
            r"subscription_sharing_usage_limit_exceeded",
            r"subscription_sharing_workplace_individual_usage_limit_exceeded",
            // Zhipu/Z.ai error code 1308 sent as HTTP 429 (#11537).
            r#""code"\s*:\s*"?1308(?![0-9])"#,
        ]
        .join("|"),
    )
    .case_insensitive(true)
    .build()
    .expect("non-retryable pattern is valid")
});

/// `RETRYABLE_PROVIDER_ERROR_PATTERN` (`retry.ts:60-140`): transient
/// provider, transport, and stream failures.
static RETRYABLE_PATTERN: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::RegexBuilder::new(
        &[
            // Generic provider load, HTTP status, server-side transients.
            r"overloaded",
            r"server_busy",
            r"servers are currently busy",
            r"currently experiencing high demand",
            r"model is at capacity",
            r"rate.?limit",
            r"too many requests",
            // Status codes must not be part of a longer word or number
            // (#11057).
            r"(?<![a-z0-9])(?:429|500|502|503|504|520|524)(?![a-z0-9])",
            // "The service is temporarily unavailable" (#10965).
            r"service.?(?:is.?)?(?:temporarily.?)?unavailable",
            r"server.?error",
            r"internal.?error",
            // Wrapper/provider text for transient upstream failures (#2264,
            // #10842).
            r"provider.?returned.?(?:an.?)?error",
            r"exceeded request buffer limit while retrying upstream",
            // Gateway failures relayed as `response.failed` events (#10920).
            r"unable to forward upstream",
            r"upstream.?request.?failed",
            r"upstream_stream_read_error",
            r"stream was interrupted",
            r"http/?2 stream failed",
            // Network, proxy, and fetch transport failures (#733, #3317).
            r"network.?error",
            r"connection.?error",
            r"connection.?refused",
            r"connection.?lost",
            // Mid-stream drop relayed as an Anthropic-style api_error (#12064).
            r"connection.?closed",
            r"other side closed",
            r"fetch failed",
            // node-fetch system errors (#10996).
            r"request to \S+ failed, reason:",
            r"getaddrinfo",
            // Bare Node socket error codes, not part of a longer identifier
            // (#12128).
            r"(?<![a-z0-9_])(?:ECONNRESET|ECONNABORTED|ECONNREFUSED|ETIMEDOUT|EPIPE)(?![a-z0-9_])",
            r"connection reset by peer",
            r"ENOTFOUND",
            r"EAI_AGAIN",
            r"upstream.?connect",
            r"reset before headers",
            r"socket hang up",
            r"socket connection was closed",
            r"timed? out",
            r"timeout",
            r"terminated",
            // WebSocket close/error text.
            r"websocket.?closed",
            r"websocket.?error",
            // Premature stream endings (#4433, #11057, #11879, #10379).
            r"ended without",
            r"stream ended before message_stop",
            r"stream ended before a terminal response event",
            r"http2 request did not get a response",
            r"stream disconnected before completion",
            r"cut short after the reply had begun",
            r"pending stream has been canceled",
            // Provider-requested retry delay cap failures flow through the
            // outer retry policy (#1123).
            r"retry delay",
            // Explicit mid-stream retry guidance (#6019).
            r"you can retry your request",
            r"try your request again",
            r"please retry your request",
            // gRPC-based providers (e.g. NVIDIA NIM).
            r"ResourceExhausted",
            // Sign in with ChatGPT: usage or user data temporarily
            // unavailable (#6019 follow-up).
            r"subscription_sharing_usage_unavailable",
            r"subscription_sharing_user_unavailable",
        ]
        .join("|"),
    )
    .case_insensitive(true)
    .build()
    .expect("retryable pattern is valid")
});

/// `retryDelayMs` (`retry.ts:155-159`): `baseDelayMs * 2^(attempt-1)`,
/// clamped to the JS safe-integer range and the policy cap.
#[must_use]
pub fn retry_delay_ms(policy: &RetryPolicy, attempt: u32) -> u64 {
    let exponent = attempt.saturating_sub(1).min(53);
    let delay = policy
        .base_delay_ms
        .saturating_mul(1u64 << exponent)
        .min(MAX_SAFE_INTEGER);
    delay.min(
        policy
            .max_agent_delay_ms
            .unwrap_or(DEFAULT_MAX_AGENT_RETRY_DELAY_MS),
    )
}

/// `isRetryableAssistantError` (`retry.ts:266-276`): a failed assistant
/// message that looks like a transient provider or transport error. Callers
/// handle context overflow separately and apply their own retry budget.
#[must_use]
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(error_message) = message.error_message.as_ref() else {
        return false;
    };
    if error_message.is_empty() {
        return false;
    }
    if NON_RETRYABLE_PATTERN
        .is_match(error_message)
        .unwrap_or(false)
    {
        return false;
    }
    RETRYABLE_PATTERN.is_match(error_message).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssistantMessage, StopReason};

    fn error_message(text: &str) -> AssistantMessage {
        // Struct literal per `event_stream.rs`'s helper — the wire shape has
        // required fields beyond the ones under test.
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
            usage: crate::types::Usage::default(),
            stop_reason: StopReason::Error,
            deferred: None,
            error_message: Some(text.to_string()),
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
            duration_ms: None,
        }
    }

    #[test]
    fn classifies_transient_provider_errors_as_retryable() {
        for text in [
            "The model is overloaded, try again",
            "HTTP 500 from upstream",
            "status code 503 received",
            "request id 429 rejected",
            "read ECONNRESET",
            "connection reset by peer",
            "socket hang up",
            "Provider returned an error",
            "The service is temporarily unavailable",
            "stream ended before message_stop",
            "ResourceExhausted",
        ] {
            assert!(
                is_retryable_assistant_error(&error_message(text)),
                "expected retryable: {text}"
            );
        }
    }

    #[test]
    fn status_codes_must_not_match_inside_longer_words() {
        // `(?<![a-z0-9])...(?!...)` boundaries (#11057).
        assert!(!is_retryable_assistant_error(&error_message(
            "request id req500id"
        )));
        assert!(!is_retryable_assistant_error(&error_message(
            "4290 bytes transferred"
        )));
        // A quoted id with a colon boundary still matches.
        assert!(is_retryable_assistant_error(&error_message("id:429:retry")));
    }

    #[test]
    fn quota_and_billing_exhaustion_is_never_retryable() {
        for text in [
            "insufficient_quota: exceeded your current quota",
            "out of budget",
            "Monthly usage limit reached",
            "billing error on the account",
            r#"{"code": 1308}"#,
            "GoUsageLimitError: free tier exhausted",
        ] {
            assert!(
                !is_retryable_assistant_error(&error_message(text)),
                "expected non-retryable: {text}"
            );
        }
    }

    #[test]
    fn non_retryable_wins_over_retryable_in_mixed_text() {
        assert!(!is_retryable_assistant_error(&error_message(
            "overloaded, but the account is out of budget"
        )));
    }

    #[test]
    fn only_errored_messages_with_text_can_retry() {
        let mut message = error_message("overloaded");
        message.stop_reason = StopReason::Aborted;
        assert!(!is_retryable_assistant_error(&message));
        message.stop_reason = StopReason::ToolUse;
        assert!(!is_retryable_assistant_error(&message));
        message.stop_reason = StopReason::Error;
        message.error_message = None;
        assert!(!is_retryable_assistant_error(&message));
        message.error_message = Some(String::new());
        assert!(!is_retryable_assistant_error(&message));
    }

    #[test]
    fn retry_delay_is_exponential_and_capped() {
        let policy = RetryPolicy {
            enabled: true,
            max_retries: 5,
            base_delay_ms: 1000,
            max_agent_delay_ms: None,
        };
        assert_eq!(retry_delay_ms(&policy, 1), 1000);
        assert_eq!(retry_delay_ms(&policy, 2), 2000);
        assert_eq!(retry_delay_ms(&policy, 3), 4000);
        assert_eq!(retry_delay_ms(&policy, 9), 60_000); // default cap
        let capped = RetryPolicy {
            max_agent_delay_ms: Some(5000),
            ..policy.clone()
        };
        assert_eq!(retry_delay_ms(&capped, 4), 5000);
        // Enormous attempt numbers clamp instead of overflowing.
        assert_eq!(retry_delay_ms(&capped, u32::MAX), 5000);
    }
}
