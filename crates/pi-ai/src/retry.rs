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
//! classification + policy surface that machine consumes. `retryProviderRequest`
//! additionally ports the SDK-level request wrapper
//! (`packages/ai/src/utils/provider-retry.ts:1-128`) used by the wire adapters.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::LazyLock;
use std::time::Duration;

use crate::types::{AssistantMessage, StopReason, now_ms};

/// `Number.MAX_SAFE_INTEGER` — the JS-side clamp for the computed delay.
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// `DEFAULT_MAX_AGENT_RETRY_DELAY_MS` (`retry.ts:151`).
pub const DEFAULT_MAX_AGENT_RETRY_DELAY_MS: u64 = 60_000;

/// `DEFAULT_MAX_RETRY_DELAY_MS` (`utils/provider-retry.ts:1`), the cap for
/// server-requested provider delays. Same value as the agent-level cap but a
/// distinct upstream constant.
pub const DEFAULT_MAX_PROVIDER_RETRY_DELAY_MS: u64 = 60_000;

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

/// A provider error carrying the HTTP metadata the SDK retry policy reads
/// (`utils/provider-retry.ts:11-22`). Header names are lowercased, like the JS
/// `Headers` object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    pub status: Option<u16>,
    pub headers: BTreeMap<String, String>,
    pub message: String,
}

/// `ProviderRetryOptions` (`utils/provider-retry.ts:3-9`). Defaults mirror
/// upstream's `retryProviderRequest`: no retries unless the caller opts in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderRetryOptions {
    pub max_retries: u32,
    pub max_retry_delay_ms: Option<u64>,
    /// Statuses that fail at once although the default policy would retry
    /// them (`utils/provider-retry.ts:8,121`).
    pub no_retry_statuses: Vec<u16>,
}

fn response_header<'a>(error: &'a ProviderError, name: &str) -> Option<&'a str> {
    error.headers.get(name).map(String::as_str)
}

/// `isRetryableProviderError` (`utils/provider-retry.ts:24-37`): the SDK's
/// `x-should-retry` override wins, a missing status is a transport failure and
/// is retryable, then 408/409/429/5xx.
#[must_use]
pub fn is_retryable_provider_error(error: &ProviderError) -> bool {
    match response_header(error, "x-should-retry") {
        Some("true") => return true,
        Some("false") => return false,
        _ => {}
    }
    let Some(status) = error.status else {
        return true;
    };
    status == 408 || status == 409 || status == 429 || status >= 500
}

fn validate_server_delay(delay_ms: u64, max: Option<u64>, message: &str) -> Result<u64, String> {
    let max = max.unwrap_or(DEFAULT_MAX_PROVIDER_RETRY_DELAY_MS);
    if max > 0 && delay_ms > max {
        return Err(format!(
            "Server requested {}s retry delay (max: {}s). {message}",
            delay_ms.div_ceil(1000),
            max.div_ceil(1000)
        ));
    }
    Ok(delay_ms)
}

/// `getRetryDelayMs` (`utils/provider-retry.ts:53-69`): server-provided
/// `retry-after-ms` / `retry-after` (number of seconds) take precedence over
/// an exponential `min(0.5 * 2^index, 8) * 1000` backoff with up to 25% jitter.
/// The HTTP-date form of `retry-after` is not parsed (no HTTP-date dependency
/// in this crate) and falls through to exponential backoff.
pub fn provider_retry_delay_ms(
    error: &ProviderError,
    retry_index: u32,
    max: Option<u64>,
) -> Result<u64, String> {
    if let Some(value) = response_header(error, "retry-after-ms")
        && let Ok(ms) = value.parse::<f64>()
        && ms.is_finite()
    {
        return validate_server_delay(ms.round() as u64, max, &error.message);
    }
    if let Some(value) = response_header(error, "retry-after")
        && let Ok(seconds) = value.parse::<f64>()
        && seconds.is_finite()
    {
        return validate_server_delay((seconds * 1000.0).round() as u64, max, &error.message);
    }
    let exponent = i32::try_from(retry_index).unwrap_or(i32::MAX);
    let base = (0.5_f64 * 2_f64.powi(exponent)).min(8.0) * 1000.0;
    let jitter = (now_ms().unsigned_abs() % 251) as f64 / 1000.0;
    Ok((base * (1.0 - jitter)).round() as u64)
}

/// `retryProviderRequest` (`utils/provider-retry.ts:99-128`): retry a fresh
/// provider request for transient errors, honouring server-requested delays
/// and a bounded cap. Each failed attempt consumes one retry; a
/// server-requested delay above `max_retry_delay_ms` fails immediately.
pub async fn retry_provider_request<T, F, Fut>(
    mut request: F,
    options: &ProviderRetryOptions,
) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    let mut remaining = options.max_retries;
    loop {
        match request().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if remaining == 0
                    || !is_retryable_provider_error(&error)
                    || error
                        .status
                        .is_some_and(|status| options.no_retry_statuses.contains(&status))
                {
                    return Err(error.message);
                }
                let retry_index = options.max_retries - remaining;
                remaining -= 1;
                let delay =
                    provider_retry_delay_ms(&error, retry_index, options.max_retry_delay_ms)?;
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }
    }
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

    fn provider_error(status: Option<u16>, headers: &[(&str, &str)]) -> ProviderError {
        ProviderError {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                .collect(),
            message: "provider failed".to_string(),
        }
    }

    #[test]
    fn sdk_retry_policy_classifies_statuses_and_headers() {
        for status in [408, 409, 429, 500, 502, 503, 504] {
            assert!(is_retryable_provider_error(&provider_error(
                Some(status),
                &[]
            )));
        }
        for status in [400, 401, 403, 404, 422] {
            assert!(!is_retryable_provider_error(&provider_error(
                Some(status),
                &[]
            )));
        }
        // Transport failures have no status.
        assert!(is_retryable_provider_error(&provider_error(None, &[])));
        // x-should-retry overrides the status.
        assert!(is_retryable_provider_error(&provider_error(
            Some(400),
            &[("x-should-retry", "true")]
        )));
        assert!(!is_retryable_provider_error(&provider_error(
            Some(500),
            &[("x-should-retry", "false")]
        )));
    }

    #[test]
    fn server_retry_delays_are_honoured_or_capped() {
        let error = provider_error(
            Some(429),
            &[("retry-after-ms", "250"), ("retry-after", "30")],
        );
        assert_eq!(provider_retry_delay_ms(&error, 3, None).unwrap(), 250);
        let error = provider_error(Some(503), &[("retry-after", "2")]);
        assert_eq!(provider_retry_delay_ms(&error, 0, None).unwrap(), 2000);
        // A server delay beyond the cap fails like upstream.
        let error = provider_error(Some(503), &[("retry-after-ms", "90000")]);
        assert!(
            provider_retry_delay_ms(&error, 0, None)
                .unwrap_err()
                .contains("retry delay")
        );
        // Exponential fallback is 500ms..8s before jitter.
        let error = provider_error(Some(500), &[]);
        let delay = provider_retry_delay_ms(&error, 0, None).unwrap();
        assert!((375..=500).contains(&delay), "unexpected delay {delay}");
        let delay = provider_retry_delay_ms(&error, 9, None).unwrap();
        assert!((6000..=8000).contains(&delay), "unexpected delay {delay}");
    }

    #[tokio::test]
    async fn retry_wrapper_retries_then_succeeds() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = AtomicU32::new(0);
        let policy = ProviderRetryOptions {
            max_retries: 2,
            max_retry_delay_ms: Some(0), // no wall-clock sleep in the test
            ..Default::default()
        };
        let result = retry_provider_request(
            || {
                let attempt = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt < 2 {
                        Err(provider_error(Some(429), &[("retry-after-ms", "1")]))
                    } else {
                        Ok(attempt)
                    }
                }
            },
            &policy,
        )
        .await;
        assert_eq!(result.unwrap(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_wrapper_stops_at_budget_and_honours_no_retry_statuses() {
        let calls = std::cell::Cell::new(0);
        let policy = ProviderRetryOptions {
            max_retries: 2,
            max_retry_delay_ms: Some(0),
            no_retry_statuses: vec![504],
        };
        let error = retry_provider_request(
            || {
                calls.set(calls.get() + 1);
                async { Err::<(), _>(provider_error(Some(500), &[("retry-after-ms", "0")])) }
            },
            &policy,
        )
        .await
        .unwrap_err();
        assert_eq!(error, "provider failed");
        assert_eq!(calls.get(), 3, "initial call + two retries");

        let calls = std::cell::Cell::new(0);
        let _ = retry_provider_request(
            || {
                calls.set(calls.get() + 1);
                async { Err::<(), _>(provider_error(Some(504), &[])) }
            },
            &policy,
        )
        .await;
        assert_eq!(calls.get(), 1, "no_retry_statuses fails immediately");
    }
}
