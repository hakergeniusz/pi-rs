//! Prompt-cache waste accounting for `/session`'s "Cache Re-billed" line.
//!
//! Ground truth: `core/cache-stats.ts` — `CACHE_TTL_MS`, `CacheMiss`,
//! `CacheWasteTotals`, `detectMiss`, `scan`, and `computeCacheWaste`. The
//! remaining exports (`collectCacheMisses`, `detectCacheMiss`) re-derive
//! per-message notices for the transcript and the cache warmer; they land
//! with those features.

use crate::session::records::SessionEntry;
use crate::session::store::iso_to_ms;
use pi_agent::agent_message::AgentMessage;

/// Prompt-cache TTL: idle gaps longer than this are worth mentioning as the
/// likely cause of a miss. Anthropic's default cache TTL is 5 minutes
/// (`cache-stats.ts:8`).
pub const CACHE_TTL_MS: i64 = 5 * 60 * 1000;

/// Per-turn misses at or below this are cache breakpoint granularity noise
/// (`cache-stats.ts:11`).
const NOISE_FLOOR_TOKENS: u64 = 1024;

/// A counted cache miss on a single assistant message (`CacheMiss`,
/// `cache-stats.ts:14-25`).
#[derive(Debug, Clone, PartialEq)]
pub struct CacheMiss {
    /// Prompt tokens that were in the previous turn's prompt but not read
    /// from cache.
    pub missed_tokens: u64,
    /// Extra dollars paid vs. a full cache hit; 0 when pricing is unknown.
    pub missed_cost: f64,
    /// Milliseconds since the previous request (which last refreshed the
    /// cache).
    pub idle_ms: i64,
    /// True when the model changed relative to the previous request.
    pub model_changed: bool,
}

/// Cumulative waste across a session (`CacheWasteTotals`,
/// `cache-stats.ts:27-33`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CacheWasteTotals {
    pub missed_tokens: u64,
    pub missed_cost: f64,
    /// Number of counted misses (turns above the noise floor).
    pub miss_count: u64,
}

/// Minimal pricing lookup, satisfied by `ModelRuntime` upstream — here by
/// the embedded catalog (`ModelPriceSource`, `cache-stats.ts:36-39`).
pub trait ModelPriceSource {
    /// `getModel(provider, modelId)?.cost.cacheRead` — $/million tokens.
    fn cache_read_cost(&self, provider: &str, model_id: &str) -> Option<f64>;
}

impl ModelPriceSource for pi_ai::catalog::ModelCatalog {
    fn cache_read_cost(&self, provider: &str, model_id: &str) -> Option<f64> {
        self.get(provider, model_id)
            .map(|model| model.cost.cache_read)
    }
}

/// The last request seen by the scan; everything in its prompt should be
/// cached (`PreviousRequest`, `cache-stats.ts:42-53`).
struct PreviousRequest {
    prompt_tokens: u64,
    model_key: String,
    timestamp: i64,
    /// Sticky: some earlier request in this scan segment reported cache
    /// activity. Distinguishes a total miss on a cache-read-only provider
    /// (OpenAI-style, writes unreported) from a provider that never reports
    /// caching at all.
    reported_cache: bool,
}

/// `detectMiss` (`cache-stats.ts:62-92`): the cache miss for one assistant
/// message relative to the previous request. `None` when nothing is counted:
/// first turn, after a reset, no cache activity ever reported (provider
/// without cache support), or miss below the noise floor.
fn detect_miss(
    prev: Option<&PreviousRequest>,
    message: &pi_ai::types::AssistantMessage,
    models: &dyn ModelPriceSource,
) -> Option<CacheMiss> {
    let Some(prev) = prev else {
        return None;
    };
    let usage = &message.usage;
    let prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
    // A zero-cache turn only counts when cache activity was reported before:
    // on cache-read-only providers that is a total miss, while on providers
    // that never report caching it means nothing.
    if prompt_tokens == 0 || (usage.cache_read + usage.cache_write == 0 && !prev.reported_cache) {
        return None;
    }

    let missed_tokens = prev.prompt_tokens.min(prompt_tokens) - usage.cache_read;
    if missed_tokens <= NOISE_FLOOR_TOKENS {
        return None;
    }

    // Extra cost = missed tokens billed at the actual paid rate
    // (input/cacheWrite, incl. write premium) instead of the cache-read
    // rate. Missed tokens can only land in the input or cacheWrite buckets,
    // so the paid rate comes straight from this message's own cost
    // breakdown.
    let paid_tokens = usage.input + usage.cache_write;
    let paid_per_token = if paid_tokens > 0 {
        (usage.cost.input + usage.cost.cache_write) / paid_tokens as f64
    } else {
        0.0
    };
    let read_per_token = if usage.cache_read > 0 {
        usage.cost.cache_read / usage.cache_read as f64
    } else {
        models
            .cache_read_cost(&message.provider, &message.model)
            .unwrap_or(0.0)
            / 1_000_000.0
    };

    Some(CacheMiss {
        missed_tokens,
        missed_cost: missed_tokens as f64 * (paid_per_token - read_per_token).max(0.0),
        idle_ms: (message.timestamp - prev.timestamp).max(0),
        model_changed: format!("{}/{}", message.provider, message.model) != prev.model_key,
    })
}

/// `asPreviousRequest` (`cache-stats.ts:94-106`).
fn as_previous_request(
    message: &pi_ai::types::AssistantMessage,
    reported_cache: bool,
) -> Option<PreviousRequest> {
    let usage = &message.usage;
    let prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
    if prompt_tokens == 0 {
        return None;
    }
    Some(PreviousRequest {
        prompt_tokens,
        model_key: format!("{}/{}", message.provider, message.model),
        timestamp: message.timestamp,
        reported_cache: reported_cache || usage.cache_read + usage.cache_write > 0,
    })
}

/// `scan` (`cache-stats.ts:108-143`), reduced to the totals (the per-message
/// miss map feeds the transcript notices).
fn scan(entries: &[SessionEntry], models: &dyn ModelPriceSource) -> CacheWasteTotals {
    let mut prev: Option<PreviousRequest> = None;
    let mut totals = CacheWasteTotals::default();

    for entry in entries {
        match entry {
            // The context legitimately changed; the next turn's prompt is new
            // content, not re-billed content. Model switches are NOT exempt:
            // they re-bill the full prompt and should be counted.
            SessionEntry::Compaction(_) | SessionEntry::BranchSummary(_) => {
                prev = None;
            }
            SessionEntry::Usage(entry) if entry.kind == "cache_warm" => {
                let prompt_tokens =
                    entry.usage.input + entry.usage.cache_read + entry.usage.cache_write;
                if prompt_tokens > 0 {
                    prev = Some(PreviousRequest {
                        prompt_tokens,
                        model_key: format!("{}/{}", entry.provider, entry.model),
                        timestamp: iso_to_ms(&entry.base.timestamp),
                        reported_cache: true,
                    });
                }
            }
            SessionEntry::Message(entry) => {
                let AgentMessage::Assistant(message) = &entry.message else {
                    continue;
                };
                if let Some(miss) = detect_miss(prev.as_ref(), message, models) {
                    totals.missed_tokens += miss.missed_tokens;
                    totals.missed_cost += miss.missed_cost;
                    totals.miss_count += 1;
                }
                prev =
                    as_previous_request(message, prev.as_ref().is_some_and(|p| p.reported_cache))
                        .or(prev);
            }
            _ => {}
        }
    }
    totals
}

/// `computeCacheWaste` (`cache-stats.ts:150-153`): prompt tokens that should
/// have been cache reads (they were in the previous turn's prompt) but were
/// re-billed.
pub fn compute_cache_waste(
    entries: &[SessionEntry],
    models: &dyn ModelPriceSource,
) -> CacheWasteTotals {
    scan(entries, models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::records::{SessionEntry, SessionMessageEntry};
    use pi_ai::types::{AssistantContent, AssistantMessage, TextContent, Usage, UsageCost};

    /// The pricing source used by the tests: no catalog entry, so the
    /// fallback read rate is 0 (unknown pricing) unless overridden.
    struct NoPrices;
    impl ModelPriceSource for NoPrices {
        fn cache_read_cost(&self, _provider: &str, _model_id: &str) -> Option<f64> {
            None
        }
    }

    fn usage(input: u64, cache_read: u64, cache_write: u64) -> Usage {
        Usage {
            input,
            output: 10,
            cache_read,
            cache_write,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: input + cache_read + cache_write + 10,
            cost: UsageCost {
                input: input as f64 / 1_000_000.0 * 3.0,
                output: 0.0,
                cache_read: cache_read as f64 / 1_000_000.0 * 0.3,
                cache_write: cache_write as f64 / 1_000_000.0 * 3.75,
                total: 0.0,
                total_from_provider: None,
            },
        }
    }

    fn assistant(provider: &str, model: &str, usage: Usage) -> SessionEntry {
        SessionEntry::Message(SessionMessageEntry {
            message: AgentMessage::Assistant(AssistantMessage {
                content: vec![AssistantContent::Text(TextContent {
                    text: "reply".to_string(),
                    text_signature: None,
                })],
                api: "test".to_string(),
                provider: provider.to_string(),
                model: model.to_string(),
                response_model: None,
                response_id: None,
                provider_thinking_level: None,
                thinking_level: None,
                diagnostics: Vec::new(),
                usage,
                stop_reason: pi_ai::types::StopReason::Stop,
                deferred: None,
                error_message: None,
                raw_stop_reason: None,
                end_turn: None,
                timestamp: 1_730_000_000_000,
                duration_ms: None,
            }),
            base: Default::default(),
            unknown: Default::default(),
        })
    }

    /// First turn never counts; the second turn re-bills the first turn's
    /// prompt above the noise floor (`cache-stats.ts:62-92`): 10k prompt in
    /// turn 1, 8k read back of a 13k turn-2 prompt → 2k missed.
    #[test]
    fn second_turn_re_bills_the_first_prompt() {
        let entries = vec![
            assistant("anthropic", "claude-test", usage(10_000, 0, 0)),
            assistant("anthropic", "claude-test", usage(5_000, 8_000, 0)),
        ];
        let totals = compute_cache_waste(&entries, &NoPrices);
        assert_eq!(totals.miss_count, 1);
        assert_eq!(totals.missed_tokens, 2_000);
        // 2k tokens at the paid input rate ($3/M) instead of the cache-read
        // rate ($0.3/M).
        assert!(
            (totals.missed_cost - 0.0054).abs() < 1e-9,
            "{}",
            totals.missed_cost
        );
    }

    /// A provider that never reports caching means nothing until some turn
    /// reports cache activity (`prev.reportedCache`).
    #[test]
    fn zero_cache_without_reported_activity_is_not_a_miss() {
        let entries = vec![
            assistant("openai", "gpt-test", usage(10_000, 0, 0)),
            assistant("openai", "gpt-test", usage(10_000, 0, 0)),
        ];
        assert_eq!(
            compute_cache_waste(&entries, &NoPrices),
            CacheWasteTotals::default()
        );
    }

    /// Misses at or below 1024 tokens are breakpoint noise.
    #[test]
    fn noise_floor_is_not_counted() {
        let entries = vec![
            assistant("anthropic", "claude-test", usage(2_000, 0, 0)),
            assistant("anthropic", "claude-test", usage(1_000, 1_000, 0)),
        ];
        assert_eq!(
            compute_cache_waste(&entries, &NoPrices),
            CacheWasteTotals::default()
        );
    }

    /// A compaction resets the scan: the next turn's prompt is new content.
    #[test]
    fn compaction_resets_the_scan() {
        let compaction = crate::session::records::CompactionEntry {
            base: Default::default(),
            summary: "summary".to_string(),
            first_kept_entry_id: String::new(),
            tokens_before: 0.0,
            details: None,
            usage: None,
            from_hook: None,
            system_message: None,
            unknown: Default::default(),
        };
        let entries = vec![
            assistant("anthropic", "claude-test", usage(10_000, 0, 0)),
            SessionEntry::Compaction(compaction),
            assistant("anthropic", "claude-test", usage(1_000, 10_000, 0)),
        ];
        assert_eq!(
            compute_cache_waste(&entries, &NoPrices),
            CacheWasteTotals::default()
        );
    }
}
