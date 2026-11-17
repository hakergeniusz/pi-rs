//! Context-overflow classifiers (`packages/ai/src/utils/overflow.ts`).
//!
//! `isContextOverflow` (:144-178) decides from an assistant message whether
//! the context window is exhausted: an error message matching a provider
//! overflow pattern (minus the non-overflow exclusions, e.g. Bedrock
//! throttling that also says "too many tokens"), a silent overflow where a
//! successful response's input usage exceeds the window (z.ai), or a
//! length-stop with zero output and the window filled (Xiaomi MiMo). `isRecoverableLength` (:186-188) marks a length stop that ended below the
//! caller's intended output limit — one bounded compact-and-retry
//! candidate. The patterns are the verbatim upstream list (:39-86).

use std::sync::LazyLock;

use fancy_regex::Regex;

use crate::types::{AssistantMessage, StopReason};

/// Provider overflow error-message patterns (`overflow.ts:39-69`), verbatim.
static OVERFLOW_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)prompt (?:is )?too long",   // Anthropic and z.ai token overflow
        r"(?i)prompt exceeds max length", // z.ai CN endpoint token overflow
        r"(?i)request_too_large",         // Anthropic request byte-size overflow (HTTP 413)
        r"(?i)input is too long for requested model", // Amazon Bedrock
        r"(?i)exceeds the context window", // OpenAI (Completions & Responses API)
        r"(?i)exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))", // OpenAI-compatible proxies (LiteLLM)
        r"(?i)context window exceeded", // LiteLLM pre-call check (ContextWindowExceededError)
        r"(?i)input token count.*exceeds the maximum", // Google (Gemini)
        r"(?i)maximum prompt length is \d+", // xAI (Grok)
        r"(?i)reduce the length of the messages", // Groq
        r"(?i)maximum context length is \d+ tokens", // OpenRouter (most backends)
        r"(?i)exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?", // OpenRouter/Poolside
        r"(?i)input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)", // Together AI
        r"(?i)exceeds the limit of \d+",           // GitHub Copilot
        r"(?i)exceeds the available context size", // llama.cpp server
        r"(?i)greater than the context length",    // LM Studio
        r"(?i)context window exceeds limit",       // MiniMax
        r"(?i)exceeded model token limit",         // Kimi For Coding
        r"(?i)too large for model with \d+ maximum context length", // Mistral
        r"(?i)prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?", // DS4 server
        r"(?i)model_context_window_exceeded", // z.ai non-standard finish_reason surfaced as error text
        r"(?i)prompt too long; exceeded (?:max )?context length", // Ollama explicit overflow error
        r"(?i)range of input length should be", // DashScope / Qwen Token Plan
        r"(?i)context[_ ]length[_ ]exceeded", // Generic fallback
        r"(?i)too many tokens",               // Generic fallback
        r"(?i)token limit exceeded",          // Generic fallback
        // HTTP 413 means request-size overflow regardless of provider or body text.
        // Match raw SDK errors and formatProviderError's prefix, not a status mentioned in the body.
        r"(?i)^(?:[\w -]+ API error \(413\):|413(?:\s|:|$))",
    ]
    .into_iter()
    .map(|pattern| Regex::new(pattern).expect("overflow pattern"))
    .collect()
});

/// Cerebras's bodyless 400/413 overflow (`overflow.ts:71`).
static CEREBRAS_BODYLESS_OVERFLOW_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^4(?:00|13)\s*(?:status code)?\s*\(no body\)").expect("cerebras pattern")
});

/// Non-overflow exclusions (`overflow.ts:82-86`): rate limiting and server
/// errors that would otherwise match an overflow pattern.
static NON_OVERFLOW_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)^(Throttling error|Service unavailable):", // AWS Bedrock non-overflow errors (formatBedrockError)
        r"(?i)rate limit",                               // Generic rate limiting
        r"(?i)too many requests",                        // Generic HTTP 429 style
    ]
    .into_iter()
    .map(|pattern| Regex::new(pattern).expect("non-overflow pattern"))
    .collect()
});

/// `isContextOverflow` (`overflow.ts:144-178`): error-message patterns, the
/// silent-overflow usage check (`input + cacheRead > contextWindow`), and
/// the length-stop case where the input fills the window (≥ 0.99×) with no
/// output. `context_window` is `Some` when the caller knows the model's
/// window — the usage-based cases need it.
pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool {
    // Case 1: Check error message patterns
    if message.stop_reason == StopReason::Error
        && let Some(error_message) = &message.error_message
    {
        // Skip messages matching known non-overflow patterns (e.g.
        // throttling / rate-limit)
        let is_non_overflow = NON_OVERFLOW_PATTERNS
            .iter()
            .any(|pattern| pattern.is_match(error_message).unwrap_or(false));
        if !is_non_overflow {
            if OVERFLOW_PATTERNS
                .iter()
                .any(|pattern| pattern.is_match(error_message).unwrap_or(false))
            {
                return true;
            }
            if message.provider == "cerebras"
                && CEREBRAS_BODYLESS_OVERFLOW_PATTERN
                    .is_match(error_message)
                    .unwrap_or(false)
            {
                return true;
            }
        }
    }

    // Case 2: Silent overflow (z.ai style) - successful but usage exceeds context
    if let Some(context_window) = context_window
        && message.stop_reason == StopReason::Stop
    {
        let input_tokens = message.usage.input + message.usage.cache_read;
        if input_tokens > context_window {
            return true;
        }
    }

    // Case 3: Length-stop overflow (Xiaomi MiMo style) - server truncates
    // oversized input to fit the context window, leaving no room for output.
    // Returns stopReason "length" with output=0 and input+cacheRead filling
    // the context window.
    if let Some(context_window) = context_window
        && message.stop_reason == StopReason::Length
        && message.usage.output == 0
    {
        let input_tokens = message.usage.input + message.usage.cache_read;
        if input_tokens as f64 >= context_window as f64 * 0.99 {
            return true;
        }
    }

    false
}

/// `isRecoverableLength` (`overflow.ts:186-188`): a length stop that ended
/// below the caller or model's intended output limit — one bounded
/// compact-and-retry attempt candidate. `desired_max_output` is the
/// original limit before any context-based clamping.
pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: u64) -> bool {
    message.stop_reason == StopReason::Length
        && desired_max_output > 0
        && message.usage.output < desired_max_output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Usage;

    fn assistant_message(stop_reason: StopReason, error: Option<&str>) -> AssistantMessage {
        AssistantMessage {
            error_message: error.map(str::to_string),
            ..test_assistant(stop_reason)
        }
    }

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
            usage: Usage::default(),
            stop_reason,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
            duration_ms: None,
        }
    }

    #[test]
    fn provider_overflow_messages_match() {
        let cases = [
            (
                "anthropic",
                "prompt is too long: 213462 tokens > 200000 maximum",
            ),
            (
                "anthropic",
                "413 {\"error\":{\"type\":\"request_too_large\"}}",
            ),
            (
                "openai",
                "Your input exceeds the context window of this model",
            ),
            (
                "litellm",
                "Requested token count exceeds the model's maximum context length of 131072 tokens",
            ),
            (
                "google",
                "The input token count (1196265) exceeds the maximum number of tokens allowed (1048575)",
            ),
            (
                "groq",
                "Please reduce the length of the messages or completion",
            ),
            ("copilot", "prompt token count of 10 exceeds the limit of 5"),
            ("cerebras", "413 status code (no body)"),
            ("zai", "{\"code\":\"1261\",\"message\":\"Prompt too long\"}"),
            ("dashscope", "Range of input length should be [1, 6000]"),
        ];
        for (provider, message) in cases {
            let assistant = AssistantMessage {
                provider: provider.to_string(),
                ..assistant_message(StopReason::Error, Some(message))
            };
            assert!(
                is_context_overflow(&assistant, None),
                "{provider}: {message}"
            );
        }
    }

    #[test]
    fn raw_413_prefix_matches_but_body_mentions_do_not() {
        let raw = AssistantMessage {
            provider: "copilot".to_string(),
            ..assistant_message(
                StopReason::Error,
                Some("github-copilot API error (413): {\"message\":\"failed\"}"),
            )
        };
        assert!(is_context_overflow(&raw, None));
        let body = assistant_message(
            StopReason::Error,
            Some("HTTP 200 referenced 413 in a comment"),
        );
        assert!(!is_context_overflow(&body, None));
    }

    #[test]
    fn non_overflow_exclusions_win() {
        // The exclusion matches formatBedrockError's human-readable
        // prefix; the raw provider string genuinely classifies as overflow
        // upstream (the /too many tokens/ pattern wins).
        let formatted = AssistantMessage {
            provider: "bedrock".to_string(),
            ..assistant_message(
                StopReason::Error,
                Some("Throttling error: Too many tokens, please wait before trying again."),
            )
        };
        assert!(!is_context_overflow(&formatted, None));
        let raw = AssistantMessage {
            provider: "bedrock".to_string(),
            ..assistant_message(
                StopReason::Error,
                Some("ThrottlingException: Too many tokens, please wait before trying again."),
            )
        };
        assert!(is_context_overflow(&raw, None));
        let rate_limit =
            assistant_message(StopReason::Error, Some("rate limit exceeded for context"));
        assert!(!is_context_overflow(&rate_limit, None));
    }

    #[test]
    fn silent_and_length_stop_overflows_need_the_window() {
        let mut silent = assistant_message(StopReason::Stop, None);
        silent.usage.input = 300_000;
        silent.usage.cache_read = 5_000;
        assert!(!is_context_overflow(&silent, None));
        assert!(is_context_overflow(&silent, Some(200_000)));
        assert!(!is_context_overflow(&silent, Some(400_000)));

        let mut under = assistant_message(StopReason::Length, None);
        under.usage.input = 190_000; // 0.95 × window — below the 0.99 fill
        assert!(!is_context_overflow(&under, Some(200_000)));
        let mut filled = assistant_message(StopReason::Length, None);
        filled.usage.input = 199_000; // 0.995 × window, zero output
        assert!(is_context_overflow(&filled, Some(200_000)));
        let mut with_output = assistant_message(StopReason::Length, None);
        with_output.usage.input = 199_000;
        with_output.usage.output = 10;
        assert!(!is_context_overflow(&with_output, Some(200_000)));
    }

    #[test]
    fn recoverable_length_needs_headroom() {
        let mut truncated = assistant_message(StopReason::Length, None);
        truncated.usage.output = 900;
        assert!(is_recoverable_length(&truncated, 4_096));
        assert!(!is_recoverable_length(&truncated, 900));
        assert!(!is_recoverable_length(&truncated, 0));
        let stopped = assistant_message(StopReason::Stop, None);
        assert!(!is_recoverable_length(&stopped, 4_096));
    }
}
