//! Prompt-cache warming: keep the last real request's cache entry alive by
//! replaying it with a one-token output cap before it expires.
//!
//! Ground truth:
//! * `packages/coding-agent/src/core/cache-warmer.ts:1-356` — the host-side
//!   [`CacheWarmer`]: run lifecycle, scheduling, the extension decision hook,
//!   the persisted `cache_warm` usage, and every stop reason/status string.
//! * `packages/ai/src/cache-warming.ts:31-123` — the policy numbers and pure
//!   helpers (`CACHE_WARMING_*`, `getCacheWarmingDelayMs`,
//!   `getPromptCacheTtlMs`, `isCacheWarmingReplayable`,
//!   `getCacheWarmingRequestOptions`, `evaluateCacheWarming`).
//! * `packages/coding-agent/src/core/sdk.ts:334-339,359-373,412-423` — where
//!   the warmer is constructed and started from the agent's `streamFn` (only
//!   for session requests, with the `cacheContextIsCurrent` predicate).
//! * `packages/coding-agent/src/core/agent-session.ts:1139-1140,1493-1522` —
//!   the lifecycle hooks (`onAgentSettled`, `cancel`, `status`,
//!   `onModeChanged`) the host must forward.
//!
//! All I/O (the clock, the replay stream, the persisted usage, the extension
//! decision, and the mode/branch reads) is injected through [`CacheWarmerHost`]
//! so the state machine is testable with synthetic responses and no provider.
//! Time and timer arming are likewise injected: the host reads
//! [`CacheWarmer::next_warm_at`] and calls [`CacheWarmer::refresh`] when that
//! instant arrives (see `WIRING.md`). This is the one structural deviation
//! from upstream, whose warmer owns a `setTimeout` directly.

#![forbid(unsafe_code)]
// Owned-value signatures are dictated by the 'static futures and by upstream's
// owned usage entries; borrowing would either borrow past the future or change
// the persisted shape.
#![allow(clippy::needless_pass_by_value)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::BoxFuture;

use crate::session::records::SessionEntry;
use pi_agent::AgentMessage;
use pi_ai::cost::calculate_cost;
use pi_ai::types::{CacheRetention, Context, Model, StopReason, StreamOptions, Usage};

// ---------------------------------------------------------------------------
// Policy constants (pi-ai `cache-warming.ts:31-43`).
// ---------------------------------------------------------------------------

/// Streaming warming never continues past this long after the real request
/// (`cache-warming.ts:31`).
pub const CACHE_WARMING_MAX_AGE_MS: i64 = 60 * 60 * 1000;

/// Idle warming uses a shorter horizon (`cache-warming.ts:34`).
pub const CACHE_WARMING_MAX_IDLE_AGE_MS: i64 = 30 * 60 * 1000;

/// A refresh is sent only when it is expected to save at least this many
/// dollars (`cache-warming.ts:37`).
pub const CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS: f64 = 0.05;

/// Chance of a real request before expiry while idle (`cache-warming.ts:43`).
pub const CACHE_WARMING_IDLE_CONTINUATION_PROBABILITY: f64 = 0.15;

// ---------------------------------------------------------------------------
// Small value types.
// ---------------------------------------------------------------------------

/// Whether a refresh is sent or warming ends (`CacheWarmingAction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheWarmingAction {
    Warm,
    Stop,
}

impl CacheWarmingAction {
    /// The upstream string discriminant (`"warm"` / `"stop"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Warm => "warm",
            Self::Stop => "stop",
        }
    }
}

/// Persisted warming profile (`CacheWarmingMode`, settings-manager.ts:53-54).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CacheWarmingMode {
    Off,
    Streaming,
    #[default]
    Idle,
}

impl CacheWarmingMode {
    /// Maps an arbitrary settings string like upstream's
    /// `getCacheWarmingMode`: only the three known values are honoured, the
    /// rest (including invalid values) fall back to `"idle"`.
    #[must_use]
    pub const fn from_mode_str(value: &str) -> Self {
        match value {
            "off" => Self::Off,
            "streaming" => Self::Streaming,
            _ => Self::Idle,
        }
    }

    /// The upstream string discriminant (`"off"` / `"streaming"` / `"idle"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Streaming => "streaming",
            Self::Idle => "idle",
        }
    }
}

/// `"streaming"` while the originating agent run is active, `"idle"` after it
/// settled (`CacheWarmingPhase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheWarmingPhase {
    Streaming,
    Idle,
}

/// Inputs and outcome of one warm-or-stop decision (`CacheWarmingDecision`,
/// cache-warming.ts:14-28).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheWarmingDecision {
    pub phase: CacheWarmingPhase,
    /// Price of this refresh: a cache read of the prompt plus one output token.
    pub warm_cost: f64,
    /// Extra price of the next real request if the cache entry is lost.
    pub miss_cost: f64,
    /// Estimated chance a real request arrives before expiry.
    pub continuation_probability: f64,
    /// `continuation_probability * miss_cost - warm_cost`.
    pub expected_savings: f64,
    /// False when the prompt size or the model's prices are unknown.
    pub economics_available: bool,
    pub action: CacheWarmingAction,
}

/// Payload of the `cache_warming_decision` extension event
/// (`CacheWarmingDecisionEvent`, cache-warmer.ts:39-42). The `type` field is
/// constant `"cache_warming_decision"` and is therefore implicit here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheWarmingDecisionEvent {
    pub warm_cost: f64,
    pub miss_cost: f64,
    pub continuation_probability: f64,
    pub action: CacheWarmingAction,
}

/// Observable warmer state (`CacheWarmingStatus`, cache-warmer.ts:49-59).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CacheWarmingStatus {
    pub state: CacheWarmingState,
    /// Why nothing is scheduled.
    pub reason: Option<String>,
    pub next_warm_at: Option<i64>,
    /// The pending decision, or the decision that stopped warming.
    pub decision: Option<CacheWarmingDecision>,
    /// True when an extension changed `decision.action`.
    pub extension_override: bool,
}

/// `CacheWarmingStatus["state"]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CacheWarmingState {
    #[default]
    Inactive,
    Scheduled,
    Refreshing,
}

impl CacheWarmingState {
    /// The upstream string discriminant.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inactive => "inactive",
            Self::Scheduled => "scheduled",
            Self::Refreshing => "refreshing",
        }
    }
}

/// The request whose cache entry must be kept warm, exactly as it was sent
/// (`CacheWarmRequest`, cache-warmer.ts:61-66).
#[derive(Debug, Clone)]
pub struct CacheWarmRequest {
    pub model: Model,
    pub context: Context,
    pub options: StreamOptions,
}

/// A replay request handed to the provider: the original request capped at
/// one output token, without continuation, with retries disabled, and with a
/// cancellation flag that [`CacheWarmer::clear_run`] flips (`AbortSignal` in
/// upstream's `getCacheWarmingRequestOptions`, cache-warming.ts:78-80).
#[derive(Debug, Clone)]
pub struct WarmStreamRequest {
    pub model: Model,
    pub context: Context,
    pub options: StreamOptions,
    /// Upstream's `maxRetries: 0` — kept explicit because `StreamOptions` has
    /// no retry field.
    pub max_retries: u8,
    pub aborted: Arc<AtomicBool>,
}

/// The subset of the replayed assistant message the warmer needs
/// (cache-warmer.ts:260-271).
#[derive(Debug, Clone, PartialEq)]
pub struct WarmOutcome {
    pub provider: String,
    /// `message.responseModel ?? message.model`.
    pub model: String,
    pub usage: Usage,
    pub stop_reason: StopReason,
}

/// A pending `cache_warm` usage entry (`sessionManager.appendUsage` at
/// cache-warmer.ts:265-271).
#[derive(Debug, Clone, PartialEq)]
pub struct WarmUsageEntry {
    pub kind: String,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    /// `"extension override"` when an extension changed the decision.
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------
// Injected I/O.
// ---------------------------------------------------------------------------

/// Everything the warmer needs from its host. Every method is injected so
/// tests need no live provider, clock, settings file, or session on disk.
pub trait CacheWarmerHost: Send + Sync {
    /// Monotonic wall-clock milliseconds (`Date.now()`).
    fn now_ms(&self) -> i64;

    /// Current warming mode (`settingsManager.getCacheWarmingMode()`).
    fn mode(&self) -> CacheWarmingMode;

    /// Prompt size of the most recent real request on the branch (upstream
    /// calls `lastPromptTokens(sessionManager.getBranch())`). A host with a
    /// `SessionManager` should call [`last_prompt_tokens`] on its branch.
    fn last_prompt_tokens(&self) -> u64;

    /// Ask extensions to override the decision. An error means "extension
    /// failed", which falls back to pi's own decision (cache-warmer.ts:243).
    fn decide(
        &self,
        event: CacheWarmingDecisionEvent,
    ) -> BoxFuture<'static, Result<CacheWarmingAction, ()>>;

    /// Replay the request. `Err` is swallowed: warming is best-effort
    /// (cache-warmer.ts:274-276).
    fn stream_warm(
        &self,
        request: WarmStreamRequest,
    ) -> BoxFuture<'static, Result<WarmOutcome, String>>;

    /// Persist usage (`sessionManager.appendUsage("cache_warm", …)`).
    fn append_usage(&self, entry: WarmUsageEntry);

    /// Notified after a successful refresh (`onWarmed`).
    fn on_warmed(&self, entry: WarmUsageEntry);
}

/// Lets a caller pass `&host` (or `&&host`) as the injected host. Every method
/// forwards to the inner implementation; the `'static` futures it returns
/// never borrow the reference.
impl<T: CacheWarmerHost + ?Sized> CacheWarmerHost for &T {
    fn now_ms(&self) -> i64 {
        (**self).now_ms()
    }

    fn mode(&self) -> CacheWarmingMode {
        (**self).mode()
    }

    fn last_prompt_tokens(&self) -> u64 {
        (**self).last_prompt_tokens()
    }

    fn decide(
        &self,
        event: CacheWarmingDecisionEvent,
    ) -> BoxFuture<'static, Result<CacheWarmingAction, ()>> {
        (**self).decide(event)
    }

    fn stream_warm(
        &self,
        request: WarmStreamRequest,
    ) -> BoxFuture<'static, Result<WarmOutcome, String>> {
        (**self).stream_warm(request)
    }

    fn append_usage(&self, entry: WarmUsageEntry) {
        (**self).append_usage(entry);
    }

    fn on_warmed(&self, entry: WarmUsageEntry) {
        (**self).on_warmed(entry);
    }
}

// ---------------------------------------------------------------------------
// Pure policy helpers (pi-ai cache-warming.ts + cache-warmer.ts:23-33).
// ---------------------------------------------------------------------------

/// `lastPromptTokens` (cache-warmer.ts:23-33): prompt size of the most recent
/// assistant message on the branch, as reported by the provider.
#[must_use]
pub fn last_prompt_tokens(entries: &[SessionEntry]) -> u64 {
    for entry in entries.iter().rev() {
        if let SessionEntry::Message(entry) = entry
            && let AgentMessage::Assistant(message) = &entry.message
        {
            let usage = &message.usage;
            return usage.input + usage.cache_read + usage.cache_write;
        }
    }
    0
}

/// `getCacheWarmingDelayMs` (cache-warming.ts:45-49): refresh at 90% of the TTL
/// while preserving at least ten seconds of margin, never earlier than 1 ms.
#[must_use]
pub const fn get_cache_warming_delay_ms(ttl_ms: u64) -> Option<u64> {
    if ttl_ms <= 10_000 {
        return None;
    }
    // `ttl * 9 / 10` is exactly `Math.floor(ttl * 0.9)` in integer arithmetic;
    // dividing before multiplying would lose up to 8 ms for some TTLs.
    let ninety_percent = ttl_ms.saturating_mul(9) / 10;
    Some(ninety_percent.min(ttl_ms - 10_000).max(1))
}

/// `getPromptCacheTtlMs` (cache-warming.ts:51-62): lifetime of the cache entry
/// a request writes for its retention tier.
///
/// Deviation: the port's `StreamOptions` has no `env` field, so the
/// `PI_CACHE_RETENTION=long` override is not honoured here; the default tier is
/// `short`. The Rust `CacheRetention` enum also has no `"none"` variant, so an
/// explicitly disabled cache is reported as "lifetime unavailable" by the
/// caller rather than "request disabled prompt caching".
#[must_use]
pub fn get_prompt_cache_ttl_ms(model: &Model, options: &StreamOptions) -> Option<u64> {
    let cache = model.prompt_cache.as_ref()?;
    let seconds = match options.cache_retention.unwrap_or(CacheRetention::Short) {
        CacheRetention::Short => cache.short,
        CacheRetention::Long => cache.long,
    }?;
    seconds.checked_mul(1000)
}

/// `isCacheWarmingReplayable` (cache-warming.ts:64-72): Anthropic budget
/// thinking derives `budget_tokens` from `max_tokens`, so a one-token replay
/// would change the cached budget unless adaptive thinking is forced.
#[must_use]
pub fn is_cache_warming_replayable(model: &Model, options: &StreamOptions) -> bool {
    if options.reasoning.is_none() || model.api != "anthropic-messages" {
        return true;
    }
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.get("forceAdaptiveThinking"))
        .is_some_and(|value| value.as_bool() == Some(true))
}

/// `getCacheWarmingRequestOptions` (cache-warming.ts:74-80), reduced to the
/// fields `StreamOptions` can express. `maxRetries: 0` and the abort signal
/// live on [`WarmStreamRequest`] and are applied by [`build_stream_request`].
#[must_use]
pub fn cache_warming_request_options(options: &StreamOptions) -> StreamOptions {
    let mut warm = options.clone();
    warm.max_tokens = Some(1);
    warm.disable_continuation = true;
    warm
}

/// Price a synthetic usage against a model's catalog cost, mirroring
/// `price` (cache-warming.ts:109-123).
fn price(model: &Model, input: u64, output: u64, cache_read: u64, cache_write: u64) -> f64 {
    let mut usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        ..Usage::default()
    };
    calculate_cost(&mut usage, &model.cost);
    usage.cost.total
}

/// `evaluateCacheWarming` (cache-warming.ts:82-107): decide whether a refresh
/// pays for itself in the given phase.
#[must_use]
pub fn evaluate_cache_warming(
    model: &Model,
    prompt_tokens: u64,
    phase: CacheWarmingPhase,
) -> CacheWarmingDecision {
    let cache_hit_cost = price(model, 0, 0, prompt_tokens, 0);
    let cache_miss_cost = if model.cost.cache_write > 0.0 {
        price(model, 0, 0, 0, prompt_tokens)
    } else {
        price(model, prompt_tokens, 0, 0, 0)
    };
    let warm_cost = price(model, 0, 1, prompt_tokens, 0);
    let miss_cost = (cache_miss_cost - cache_hit_cost).max(0.0);
    let continuation_probability = if phase == CacheWarmingPhase::Idle {
        CACHE_WARMING_IDLE_CONTINUATION_PROBABILITY
    } else {
        1.0
    };
    let economics_available = prompt_tokens > 0 && (cache_hit_cost > 0.0 || cache_miss_cost > 0.0);
    let expected_savings = continuation_probability * miss_cost - warm_cost;
    let action = if expected_savings >= CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS {
        CacheWarmingAction::Warm
    } else {
        CacheWarmingAction::Stop
    };
    CacheWarmingDecision {
        phase,
        warm_cost,
        miss_cost,
        continuation_probability,
        expected_savings,
        economics_available,
        action,
    }
}

// ---------------------------------------------------------------------------
// The state machine.
// ---------------------------------------------------------------------------

/// One active warm run (`ActiveRun`, cache-warmer.ts:68-82).
struct ActiveRun {
    /// Generation id, used instead of JS object identity for validation.
    id: u64,
    request: CacheWarmRequest,
    /// False once the session's model or messages no longer match the request.
    is_current: Arc<dyn Fn() -> bool + Send + Sync>,
    ttl_ms: u64,
    delay_ms: u64,
    /// Latest safe time to send this refresh, leaving half the original
    /// pre-expiry margin for delay and dispatch.
    refresh_deadline_at: i64,
    started_at: i64,
    aborted: Arc<AtomicBool>,
    phase: CacheWarmingPhase,
    next_warm_at: i64,
    /// True while a refresh is in flight (`run.timer === undefined` upstream).
    refreshing: bool,
    /// Set while a refresh an extension forced is in flight.
    extension_override: bool,
}

/// Keeps one prompt cache entry alive by re-sending its request with a
/// one-token output cap before the entry expires. `start` replaces any
/// previous run; warm requests never extend the fixed safety windows.
pub struct CacheWarmer<H: CacheWarmerHost> {
    host: H,
    run: Option<ActiveRun>,
    next_id: u64,
    inactive: CacheWarmingStatus,
}

impl<H: CacheWarmerHost> CacheWarmer<H> {
    /// Build a warmer. Starts inactive with the upstream's initial reason.
    #[must_use]
    pub fn new(host: H) -> Self {
        Self {
            host,
            run: None,
            next_id: 0,
            inactive: CacheWarmingStatus {
                state: CacheWarmingState::Inactive,
                reason: Some("waiting for first request".to_string()),
                ..CacheWarmingStatus::default()
            },
        }
    }

    /// Current status (the `status` getter, cache-warmer.ts:113-129).
    #[must_use]
    pub fn status(&self) -> CacheWarmingStatus {
        if self.host.mode() == CacheWarmingMode::Off {
            return CacheWarmingStatus {
                state: CacheWarmingState::Inactive,
                reason: Some("cache warming disabled".to_string()),
                ..CacheWarmingStatus::default()
            };
        }
        let Some(run) = self.run.as_ref() else {
            return self.inactive.clone();
        };
        if !(run.is_current)() {
            return CacheWarmingStatus {
                state: CacheWarmingState::Inactive,
                reason: Some("conversation context changed".to_string()),
                ..CacheWarmingStatus::default()
            };
        }
        let decision = self.evaluate(run);
        if !decision.economics_available && !run.refreshing {
            return CacheWarmingStatus {
                state: CacheWarmingState::Inactive,
                reason: Some("cache economics unavailable".to_string()),
                ..CacheWarmingStatus::default()
            };
        }
        CacheWarmingStatus {
            state: if run.refreshing {
                CacheWarmingState::Refreshing
            } else {
                CacheWarmingState::Scheduled
            },
            next_warm_at: Some(run.next_warm_at),
            decision: Some(decision),
            extension_override: run.extension_override,
            ..CacheWarmingStatus::default()
        }
    }

    /// When the host should next call [`Self::refresh`], in epoch
    /// milliseconds. `None` when no refresh is armed. The host owns the timer
    /// (see `WIRING.md`); this is the port's one structural deviation from
    /// upstream's internal `setTimeout`.
    #[must_use]
    pub fn next_warm_at(&self) -> Option<i64> {
        self.run
            .as_ref()
            .filter(|run| !run.refreshing && (run.is_current)())
            .map(|run| run.next_warm_at)
    }

    /// Keep the cache entry written by `request` warm while `is_current`
    /// holds (cache-warmer.ts:131-170).
    pub fn start(
        &mut self,
        request: CacheWarmRequest,
        is_current: impl Fn() -> bool + Send + Sync + 'static,
    ) {
        self.clear_run();
        if self.host.mode() == CacheWarmingMode::Off {
            self.stop("cache warming disabled", None);
            return;
        }
        if !is_cache_warming_replayable(&request.model, &request.options) {
            self.stop("request cannot be replayed safely", None);
            return;
        }
        let Some(ttl_ms) = get_prompt_cache_ttl_ms(&request.model, &request.options) else {
            // `cacheRetention === "none"` is not representable in the port's
            // CacheRetention enum; both cases collapse to this reason.
            self.stop("cache lifetime unavailable", None);
            return;
        };
        let Some(delay_ms) = get_cache_warming_delay_ms(ttl_ms) else {
            self.stop("cache lifetime unavailable", None);
            return;
        };
        let id = self.next_id;
        self.next_id += 1;
        self.run = Some(ActiveRun {
            id,
            request,
            is_current: Arc::new(is_current),
            ttl_ms,
            delay_ms,
            refresh_deadline_at: 0,
            started_at: self.host.now_ms(),
            aborted: Arc::new(AtomicBool::new(false)),
            phase: CacheWarmingPhase::Streaming,
            next_warm_at: 0,
            refreshing: false,
            extension_override: false,
        });
        self.schedule(id);
    }

    /// Called when the originating agent run settles (cache-warmer.ts:172-184).
    pub fn on_agent_settled(&mut self) {
        let Some(id) = self.run.as_ref().map(|run| run.id) else {
            return;
        };
        if self.host.mode() == CacheWarmingMode::Streaming {
            self.stop("agent run settled", None);
            return;
        }
        if let Some(run) = self.run.as_mut().filter(|run| run.id == id) {
            run.phase = CacheWarmingPhase::Idle;
        }
        // Upstream measures the idle horizon from the real request that
        // started the run, not from the moment the agent settled.
        let deadline =
            self.run.as_ref().map_or(0, |run| run.started_at) + CACHE_WARMING_MAX_IDLE_AGE_MS;
        if self
            .run
            .as_ref()
            .is_some_and(|run| run.next_warm_at > deadline || self.host.now_ms() >= deadline)
        {
            self.stop("30-minute idle safety limit reached", None);
        }
    }

    /// Reconcile an active run after the persisted mode changes
    /// (cache-warmer.ts:186-192).
    pub fn on_mode_changed(&mut self) {
        if self.run.is_some()
            && let Some(reason) = self.get_mode_stop_reason()
        {
            self.stop(reason, None);
        }
    }

    /// Stop warming (cache-warmer.ts:194-196).
    pub fn cancel(&mut self) {
        self.stop("inactive", None);
    }

    /// Perform one scheduled refresh (cache-warmer.ts:228-278). The host calls
    /// this when [`Self::next_warm_at`] arrives; a successful refresh arms the
    /// next one, a failure is swallowed and also reschedules.
    pub async fn refresh(&mut self) {
        let Some(id) = self.run.as_ref().map(|run| run.id) else {
            return;
        };
        if let Some(run) = self.run.as_mut().filter(|run| run.id == id) {
            run.refreshing = true;
        }
        if !self.validate_run(id) || self.refresh_deadline_missed(id) {
            return;
        }
        let decision = match self.run.as_ref() {
            Some(run) => self.evaluate(run),
            None => return,
        };
        let event = CacheWarmingDecisionEvent {
            warm_cost: decision.warm_cost,
            miss_cost: decision.miss_cost,
            continuation_probability: decision.continuation_probability,
            action: decision.action,
        };
        let action = match self.host.decide(event).await {
            Ok(action) => action,
            Err(()) => decision.action,
        };
        if !self.validate_run(id) || self.refresh_deadline_missed(id) {
            return;
        }
        let extension_override = action != decision.action;
        if action == CacheWarmingAction::Stop {
            let reason = if extension_override {
                "stopped by extension"
            } else if decision.economics_available {
                "expected savings below threshold"
            } else {
                "cache economics unavailable"
            };
            self.stop(reason, Some((decision, extension_override)));
            return;
        }
        if let Some(run) = self.run.as_mut().filter(|run| run.id == id) {
            run.extension_override = extension_override;
        }
        let request = self.run.as_ref().map(build_stream_request);
        if let Some(request) = request {
            match self.host.stream_warm(request).await {
                Ok(outcome) => {
                    if !self.validate_run(id) {
                        return;
                    }
                    if !matches!(outcome.stop_reason, StopReason::Error | StopReason::Aborted) {
                        let entry = WarmUsageEntry {
                            kind: "cache_warm".to_string(),
                            provider: outcome.provider,
                            model: outcome.model,
                            usage: outcome.usage,
                            note: extension_override.then(|| "extension override".to_string()),
                        };
                        self.host.append_usage(entry.clone());
                        self.host.on_warmed(entry);
                    }
                }
                // Cache warming is best-effort and must not affect the agent.
                Err(_) => {}
            }
        }
        if self.run.as_ref().is_some_and(|run| run.id == id) {
            self.schedule(id);
        }
    }

    fn clear_run(&mut self) {
        if let Some(run) = self.run.take() {
            run.aborted.store(true, Ordering::SeqCst);
        }
    }

    fn stop(&mut self, reason: &str, stopped: Option<(CacheWarmingDecision, bool)>) {
        self.clear_run();
        self.inactive = CacheWarmingStatus {
            state: CacheWarmingState::Inactive,
            reason: Some(reason.to_string()),
            decision: stopped.map(|(decision, _)| decision),
            extension_override: stopped.is_some_and(|(_, overridden)| overridden),
            ..CacheWarmingStatus::default()
        };
    }

    /// Arm the next refresh (`schedule`, cache-warmer.ts:211-226). With the
    /// host-owned timer this computes the due instant and safety deadlines
    /// instead of calling `setTimeout`.
    fn schedule(&mut self, id: u64) {
        let Some(run) = self.run.as_mut().filter(|run| run.id == id) else {
            return;
        };
        run.extension_override = false;
        let now = self.host.now_ms();
        run.next_warm_at = now + run.delay_ms as i64;
        // Keep half of the planned pre-expiry margin for a late timer.
        run.refresh_deadline_at = run.next_warm_at + (run.ttl_ms - run.delay_ms) as i64 / 2;
        let deadline = run.started_at
            + if run.phase == CacheWarmingPhase::Idle {
                CACHE_WARMING_MAX_IDLE_AGE_MS
            } else {
                CACHE_WARMING_MAX_AGE_MS
            };
        if run.next_warm_at > deadline || now >= deadline {
            let reason = if run.phase == CacheWarmingPhase::Idle {
                "30-minute idle safety limit reached"
            } else {
                "one-hour safety limit reached"
            };
            self.stop(reason, None);
            return;
        }
        run.refreshing = false;
    }

    fn refresh_deadline_missed(&mut self, id: u64) -> bool {
        let missed = self
            .run
            .as_ref()
            .is_some_and(|run| run.id == id && self.host.now_ms() > run.refresh_deadline_at);
        if missed {
            self.stop("cache refresh deadline missed", None);
        }
        missed
    }

    fn validate_run(&mut self, id: u64) -> bool {
        if self.run.as_ref().is_none_or(|run| run.id != id) {
            return false;
        }
        let reason = self.get_mode_stop_reason().or_else(|| {
            (!self.run.as_ref().is_some_and(|run| (run.is_current)()))
                .then_some("conversation context changed")
        });
        if let Some(reason) = reason {
            self.stop(reason, None);
            return false;
        }
        true
    }

    fn get_mode_stop_reason(&self) -> Option<&'static str> {
        match self.host.mode() {
            CacheWarmingMode::Off => Some("cache warming disabled"),
            CacheWarmingMode::Streaming
                if self
                    .run
                    .as_ref()
                    .is_some_and(|run| run.phase == CacheWarmingPhase::Idle) =>
            {
                Some("agent run settled")
            }
            CacheWarmingMode::Streaming | CacheWarmingMode::Idle => None,
        }
    }

    fn evaluate(&self, run: &ActiveRun) -> CacheWarmingDecision {
        evaluate_cache_warming(
            &run.request.model,
            self.host.last_prompt_tokens(),
            run.phase,
        )
    }
}

/// Build the replay request from a validated run (upstream passes
/// `getCacheWarmingRequestOptions(run.options, run.controller.signal)`).
fn build_stream_request(run: &ActiveRun) -> WarmStreamRequest {
    WarmStreamRequest {
        model: run.request.model.clone(),
        context: run.request.context.clone(),
        options: cache_warming_request_options(&run.request.options),
        max_retries: 0,
        aborted: run.aborted.clone(),
    }
}

// ---------------------------------------------------------------------------
// Formatting (cache-warmer.ts:306-356), for `/session` and the transcript.
// ---------------------------------------------------------------------------

fn format_dollars(value: f64) -> String {
    if value < 0.0 {
        format!("-${:.3}", -value)
    } else {
        format!("${value:.3}")
    }
}

fn format_cache_warming_economics(decision: &CacheWarmingDecision) -> String {
    if !decision.economics_available {
        return "cache economics unavailable".to_string();
    }
    let probability = (decision.continuation_probability * 100.0).round() as u64;
    let probability_text = if decision.phase == CacheWarmingPhase::Streaming {
        format!("{probability}% continuation probability while agent is running")
    } else {
        format!("{probability}% continuation probability")
    };
    let comparison = if decision.action == CacheWarmingAction::Warm {
        ">="
    } else {
        "<"
    };
    format!(
        "{probability_text}, expected savings {} {comparison} {}",
        format_dollars(decision.expected_savings),
        format_dollars(CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS)
    )
}

fn format_cache_warming_decision_time(next_warm_at: Option<i64>, now: i64) -> String {
    let Some(next_warm_at) = next_warm_at else {
        return "Decision now".to_string();
    };
    if next_warm_at <= now {
        return "Decision now".to_string();
    }
    let mut remaining_seconds = (next_warm_at - now).div_ceil(1000);
    let hours = remaining_seconds / 3600;
    remaining_seconds %= 3600;
    let minutes = remaining_seconds / 60;
    let seconds = remaining_seconds % 60;
    let mut parts: Vec<String> = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(format!("{seconds}s"));
    }
    format!("Decision in {}", parts.join(" "))
}

/// One-line status for `/session` (`formatCacheWarmingStatus`,
/// cache-warmer.ts:335-349).
#[must_use]
pub fn format_cache_warming_status(status: &CacheWarmingStatus, now: i64) -> String {
    let Some(decision) = status.decision else {
        return format!(
            "Inactive ({})",
            status.reason.as_deref().unwrap_or("unknown reason")
        );
    };
    if status.state == CacheWarmingState::Inactive
        && !decision.economics_available
        && !status.extension_override
    {
        return format!(
            "Inactive ({})",
            status.reason.as_deref().unwrap_or("unknown reason")
        );
    }
    let details = if status.extension_override {
        format!(
            "extension override, {}",
            format_cache_warming_economics(decision)
        )
    } else {
        format!(
            "{} -> {}",
            format_cache_warming_economics(decision),
            decision.action.as_str()
        )
    };
    match status.state {
        CacheWarmingState::Inactive => format!("Stopped ({details})"),
        CacheWarmingState::Refreshing => format!("Warming cache ({details})"),
        CacheWarmingState::Scheduled => format!(
            "{} ({details})",
            format_cache_warming_decision_time(status.next_warm_at, now)
        ),
    }
}

fn format_cost_six(cost: f64) -> String {
    let formatted = format!("{cost:.6}");
    let Some((whole, fraction)) = formatted.split_once('.') else {
        return formatted;
    };
    let mut fraction = fraction.to_string();
    while fraction.len() > 3 && fraction.ends_with('0') {
        fraction.pop();
    }
    format!("{whole}.{fraction}")
}

/// One-line transcript text for consecutive warms (`formatCacheWarmingUsage`,
/// cache-warmer.ts:351-356).
#[must_use]
pub fn format_cache_warming_usage(total_cost: f64, note: Option<&str>) -> String {
    let note_text = note.map_or(String::new(), |note| format!(" ({note})"));
    format!("Cache warmed{note_text}: ${}", format_cost_six(total_cost))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::records::{EntryBase, SessionMessageEntry};
    use futures::future::ready;
    use pi_agent::AgentMessage;
    use pi_ai::types::{AssistantContent, AssistantMessage, TextContent};
    use serde_json::json;
    use std::sync::Mutex;

    type DecideFn =
        dyn Fn(CacheWarmingDecisionEvent) -> Result<CacheWarmingAction, ()> + Send + Sync;
    type StreamFn = dyn Fn(WarmStreamRequest) -> Result<WarmOutcome, String> + Send + Sync;

    #[derive(Default)]
    struct TestState {
        now: i64,
        mode: CacheWarmingMode,
        prompt_tokens: u64,
        decide: Option<Box<DecideFn>>,
        stream: Option<Box<StreamFn>>,
        stream_requests: Vec<WarmStreamRequest>,
        appended: Vec<WarmUsageEntry>,
        warmed: Vec<WarmUsageEntry>,
    }

    struct TestHost {
        state: Mutex<TestState>,
    }

    impl TestHost {
        fn new() -> Self {
            Self {
                state: Mutex::new(TestState::default()),
            }
        }
    }

    impl CacheWarmerHost for TestHost {
        fn now_ms(&self) -> i64 {
            self.state.lock().unwrap().now
        }

        fn mode(&self) -> CacheWarmingMode {
            self.state.lock().unwrap().mode
        }

        fn last_prompt_tokens(&self) -> u64 {
            self.state.lock().unwrap().prompt_tokens
        }

        fn decide(
            &self,
            event: CacheWarmingDecisionEvent,
        ) -> BoxFuture<'static, Result<CacheWarmingAction, ()>> {
            let action = self
                .state
                .lock()
                .unwrap()
                .decide
                .as_ref()
                .map_or(Ok(event.action), |decide| decide(event));
            Box::pin(ready(action))
        }

        fn stream_warm(
            &self,
            request: WarmStreamRequest,
        ) -> BoxFuture<'static, Result<WarmOutcome, String>> {
            let mut state = self.state.lock().unwrap();
            state.stream_requests.push(request.clone());
            let outcome = state
                .stream
                .as_ref()
                .map_or_else(|| Ok(successful_outcome()), |stream| stream(request));
            Box::pin(ready(outcome))
        }

        fn append_usage(&self, entry: WarmUsageEntry) {
            self.state.lock().unwrap().appended.push(entry);
        }

        fn on_warmed(&self, entry: WarmUsageEntry) {
            self.state.lock().unwrap().warmed.push(entry);
        }
    }

    fn usage(input: u64, cache_read: u64, cache_write: u64) -> Usage {
        Usage {
            input,
            output: 1,
            cache_read,
            cache_write,
            ..Usage::default()
        }
    }

    fn successful_outcome() -> WarmOutcome {
        WarmOutcome {
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-5".to_string(),
            usage: usage(1000, 9000, 0),
            stop_reason: StopReason::Stop,
        }
    }

    fn model(cache_ttl_seconds: Option<u64>) -> Model {
        Model {
            id: "claude-test".to_string(),
            provider: "anthropic".to_string(),
            api: "anthropic-messages".to_string(),
            cost: pi_ai::types::ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
                ..Default::default()
            },
            prompt_cache: cache_ttl_seconds.map(|seconds| pi_ai::types::ModelPromptCache {
                short: Some(seconds),
                long: None,
            }),
            ..Model::default()
        }
    }

    fn request(cache_ttl_seconds: Option<u64>) -> CacheWarmRequest {
        CacheWarmRequest {
            model: model(cache_ttl_seconds),
            context: Context::default(),
            options: StreamOptions::default(),
        }
    }

    fn assistant(provider: &str, model_id: &str, usage: Usage) -> SessionEntry {
        SessionEntry::Message(SessionMessageEntry {
            message: AgentMessage::Assistant(AssistantMessage {
                content: vec![AssistantContent::Text(TextContent {
                    text: "reply".to_string(),
                    text_signature: None,
                })],
                api: "test".to_string(),
                provider: provider.to_string(),
                model: model_id.to_string(),
                response_model: None,
                response_id: None,
                provider_thinking_level: None,
                thinking_level: None,
                diagnostics: Vec::new(),
                usage,
                stop_reason: StopReason::Stop,
                deferred: None,
                error_message: None,
                raw_stop_reason: None,
                end_turn: None,
                timestamp: 1_730_000_000_000,
                duration_ms: None,
            }),
            base: EntryBase::default(),
            unknown: serde_json::Map::default(),
        })
    }

    fn current() -> Arc<dyn Fn() -> bool + Send + Sync> {
        Arc::new(|| true)
    }

    // --- pure policy -------------------------------------------------------

    #[test]
    fn last_prompt_tokens_reads_the_latest_assistant_message() {
        let entries = vec![
            assistant("anthropic", "claude-test", usage(1000, 2000, 3000)),
            assistant("anthropic", "claude-test", usage(500, 600, 700)),
        ];
        assert_eq!(last_prompt_tokens(&entries), 1800);
        assert_eq!(last_prompt_tokens(&[]), 0);
    }

    #[test]
    fn last_prompt_tokens_ignores_non_assistant_entries() {
        let entries = vec![
            assistant("anthropic", "claude-test", usage(1000, 0, 0)),
            SessionEntry::Usage(crate::session::records::UsageEntry {
                base: EntryBase::default(),
                kind: "cache_warm".to_string(),
                provider: "anthropic".to_string(),
                model: "claude-test".to_string(),
                usage: usage(9000, 0, 0),
                note: None,
                unknown: serde_json::Map::default(),
            }),
        ];
        assert_eq!(last_prompt_tokens(&entries), 1000);
    }

    #[test]
    fn ttl_and_delay_follow_the_90_percent_and_ten_second_rules() {
        let short = model(Some(300));
        assert_eq!(
            get_prompt_cache_ttl_ms(&short, &StreamOptions::default()),
            Some(300_000)
        );
        assert_eq!(get_cache_warming_delay_ms(300_000), Some(270_000));
        // <= 10 s has no safe refresh point; below ttl the 10 s margin wins.
        assert_eq!(get_cache_warming_delay_ms(10_000), None);
        assert_eq!(get_cache_warming_delay_ms(5_000), None);
        assert_eq!(get_cache_warming_delay_ms(11_000), Some(1_000));
        assert_eq!(get_cache_warming_delay_ms(11_500), Some(1_500));
        // 0.9 × 100009 = 90008.1 → floor 90008 (inclusive of the 10 s margin).
        assert_eq!(get_cache_warming_delay_ms(100_009), Some(90_008));
        assert_eq!(get_cache_warming_delay_ms(1), None);
    }

    #[test]
    fn long_retention_uses_the_long_tier() {
        let long_model = Model {
            prompt_cache: Some(pi_ai::types::ModelPromptCache {
                short: Some(300),
                long: Some(3600),
            }),
            ..model(None)
        };
        let options = StreamOptions {
            cache_retention: Some(CacheRetention::Long),
            ..StreamOptions::default()
        };
        assert_eq!(
            get_prompt_cache_ttl_ms(&long_model, &options),
            Some(3_600_000)
        );
        assert_eq!(get_prompt_cache_ttl_ms(&model(None), &options), None);
    }

    #[test]
    fn replayable_only_blocks_anthropic_budget_thinking() {
        let anthropic = model(Some(300));
        let reasoning = StreamOptions {
            reasoning: Some(pi_ai::types::ThinkingLevel::Medium),
            ..StreamOptions::default()
        };
        assert!(!is_cache_warming_replayable(&anthropic, &reasoning));
        let adaptive = Model {
            compat: Some(
                json!({ "forceAdaptiveThinking": true })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ..anthropic.clone()
        };
        assert!(is_cache_warming_replayable(&adaptive, &reasoning));
        let openai = Model {
            api: "openai-completions".to_string(),
            ..anthropic
        };
        assert!(is_cache_warming_replayable(&openai, &reasoning));
        assert!(is_cache_warming_replayable(
            &adaptive,
            &StreamOptions::default()
        ));
    }

    #[test]
    fn warm_request_options_are_capped_and_non_continuing() {
        let options = StreamOptions {
            max_tokens: Some(4096),
            disable_continuation: false,
            ..StreamOptions::default()
        };
        let warm = cache_warming_request_options(&options);
        assert_eq!(warm.max_tokens, Some(1));
        assert!(warm.disable_continuation);
        // The caller's other options survive.
        assert_eq!(options.max_tokens, Some(4096));
    }

    #[test]
    fn evaluation_uses_miss_cost_and_the_phase_probability() {
        let m = model(Some(300));
        let streaming = evaluate_cache_warming(&m, 10_000, CacheWarmingPhase::Streaming);
        let idle = evaluate_cache_warming(&m, 10_000, CacheWarmingPhase::Idle);
        assert!(streaming.economics_available);
        assert!((streaming.continuation_probability - 1.0).abs() < 1e-9);
        assert!((idle.continuation_probability - 0.15).abs() < 1e-9);
        // 10k tokens: write $0.0375, hit $0.003 → miss $0.0345;
        // warm $0.003 + $15/M × 1 = $0.003015; savings $0.031485 < $0.05.
        assert_eq!(streaming.action, CacheWarmingAction::Stop);
        assert_eq!(idle.action, CacheWarmingAction::Stop);
        assert!((streaming.warm_cost - 0.003015).abs() < 1e-9);

        let huge = evaluate_cache_warming(&m, 100_000, CacheWarmingPhase::Streaming);
        assert_eq!(huge.action, CacheWarmingAction::Warm);
    }

    #[test]
    fn evaluation_with_unknown_prices_is_unavailable() {
        let decision = evaluate_cache_warming(&Model::default(), 10_000, CacheWarmingPhase::Idle);
        assert!(!decision.economics_available);
        assert_eq!(decision.action, CacheWarmingAction::Stop);
        let empty = evaluate_cache_warming(&model(Some(300)), 0, CacheWarmingPhase::Idle);
        assert!(!empty.economics_available);
    }

    // --- settings gate / start --------------------------------------------

    #[test]
    fn mode_off_blocks_start_and_status() {
        let host = TestHost::new();
        host.state.lock().unwrap().mode = CacheWarmingMode::Off;
        let mut warmer = CacheWarmer::new(&host);
        warmer.start(request(Some(300)), current());
        assert_eq!(
            warmer.status().reason.as_deref(),
            Some("cache warming disabled")
        );
        assert_eq!(warmer.next_warm_at(), None);
    }

    #[test]
    fn non_replayable_request_is_rejected() {
        let host = TestHost::new();
        let mut warmer = CacheWarmer::new(&host);
        let mut req = request(Some(300));
        req.options.reasoning = Some(pi_ai::types::ThinkingLevel::Medium);
        warmer.start(req, current());
        assert_eq!(
            warmer.status().reason.as_deref(),
            Some("request cannot be replayed safely")
        );
    }

    #[test]
    fn missing_cache_lifetime_is_rejected() {
        let host = TestHost::new();
        let mut warmer = CacheWarmer::new(&host);
        warmer.start(request(None), current());
        assert_eq!(
            warmer.status().reason.as_deref(),
            Some("cache lifetime unavailable")
        );
    }

    #[test]
    fn start_schedules_a_refresh_with_a_decision() {
        let host = TestHost::new();
        host.state.lock().unwrap().prompt_tokens = 100_000;
        let mut warmer = CacheWarmer::new(&host);
        warmer.start(request(Some(300)), current());
        let status = warmer.status();
        assert_eq!(status.state, CacheWarmingState::Scheduled);
        assert_eq!(status.next_warm_at, Some(270_000));
        assert_eq!(
            status.decision.as_ref().map(|d| d.action),
            Some(CacheWarmingAction::Warm)
        );
        assert_eq!(warmer.next_warm_at(), Some(270_000));
    }

    #[test]
    fn zero_tokens_report_economics_unavailable() {
        let host = TestHost::new();
        let mut warmer = CacheWarmer::new(&host);
        warmer.start(request(Some(300)), current());
        assert_eq!(
            warmer.status().reason.as_deref(),
            Some("cache economics unavailable")
        );
        assert_eq!(warmer.next_warm_at(), Some(270_000));
    }

    // --- refresh -----------------------------------------------------------

    #[tokio::test]
    async fn refresh_appends_cache_warm_usage_and_reschedules() {
        let host = TestHost::new();
        {
            let mut state = host.state.lock().unwrap();
            state.prompt_tokens = 100_000;
            state.now = 100;
        }
        let mut warmer = CacheWarmer::new(&host);
        warmer.start(request(Some(300)), current());
        host.state.lock().unwrap().now = 270_100;
        warmer.refresh().await;

        let state = host.state.lock().unwrap();
        assert_eq!(state.stream_requests.len(), 1);
        let sent = &state.stream_requests[0];
        assert_eq!(sent.options.max_tokens, Some(1));
        assert!(sent.options.disable_continuation);
        assert_eq!(sent.max_retries, 0);
        assert_eq!(state.appended.len(), 1);
        assert_eq!(state.appended[0].kind, "cache_warm");
        assert_eq!(state.appended[0].provider, "anthropic");
        assert_eq!(state.appended[0].model, "claude-sonnet-4-5");
        assert_eq!(state.appended[0].usage, usage(1000, 9000, 0));
        assert_eq!(state.warmed.len(), 1);
        drop(state);
        assert_eq!(warmer.status().state, CacheWarmingState::Scheduled);
        assert_eq!(warmer.next_warm_at(), Some(540_100));
    }

    #[tokio::test]
    async fn error_and_aborted_outcomes_do_not_persist_usage() {
        for stop_reason in [StopReason::Error, StopReason::Aborted] {
            let host = TestHost::new();
            host.state.lock().unwrap().prompt_tokens = 100_000;
            host.state.lock().unwrap().stream = Some(Box::new(move |_| {
                Ok(WarmOutcome {
                    stop_reason,
                    ..successful_outcome()
                })
            }));
            let mut warmer = CacheWarmer::new(&host);
            warmer.start(request(Some(300)), current());
            warmer.refresh().await;
            assert_eq!(host.state.lock().unwrap().appended.len(), 0);
            assert_eq!(warmer.status().state, CacheWarmingState::Scheduled);
        }
    }

    #[tokio::test]
    async fn stream_failure_is_best_effort_and_reschedules() {
        let host = TestHost::new();
        host.state.lock().unwrap().prompt_tokens = 100_000;
        host.state.lock().unwrap().stream = Some(Box::new(|_| Err("boom".to_string())));
        let mut warmer = CacheWarmer::new(&host);
        warmer.start(request(Some(300)), current());
        warmer.refresh().await;
        assert_eq!(host.state.lock().unwrap().appended.len(), 0);
        assert_eq!(warmer.status().state, CacheWarmingState::Scheduled);
    }

    #[tokio::test]
    async fn extension_can_stop_warming() {
        let host = TestHost::new();
        host.state.lock().unwrap().prompt_tokens = 100_000;
        host.state.lock().unwrap().decide = Some(Box::new(|_| Ok(CacheWarmingAction::Stop)));
        let mut warmer = CacheWarmer::new(&host);
        warmer.start(request(Some(300)), current());
        warmer.refresh().await;
        let status = warmer.status();
        assert_eq!(status.state, CacheWarmingState::Inactive);
        assert_eq!(status.reason.as_deref(), Some("stopped by extension"));
        assert!(status.extension_override);
        assert_eq!(host.state.lock().unwrap().appended.len(), 0);
    }

    #[tokio::test]
    async fn decision_failure_falls_back_to_pi() {
        let host = TestHost::new();
        host.state.lock().unwrap().prompt_tokens = 100_000;
        host.state.lock().unwrap().decide = Some(Box::new(|_| Err(())));
        let mut warmer = CacheWarmer::new(&host);
        warmer.start(request(Some(300)), current());
        warmer.refresh().await;
        assert_eq!(host.state.lock().unwrap().appended.len(), 1);
        assert!(warmer.status().decision.is_some());
    }

    #[tokio::test]
    async fn extension_stop_note_is_persisted() {
        let host = TestHost::new();
        {
            let mut state = host.state.lock().unwrap();
            // One token is below the savings threshold → pi says "stop"; the
            // extension forces "warm" so the note must be persisted.
            state.prompt_tokens = 1;
            state.decide = Some(Box::new(|_| Ok(CacheWarmingAction::Warm)));
        }
        let mut warmer = CacheWarmer::new(&host);
        warmer.start(request(Some(300)), current());
        warmer.refresh().await;
        let state = host.state.lock().unwrap();
        assert_eq!(state.appended.len(), 1);
        assert_eq!(
            state.appended[0].note.as_deref(),
            Some("extension override")
        );
    }

    #[tokio::test]
    async fn missed_refresh_deadline_stops() {
        let host = TestHost::new();
        host.state.lock().unwrap().prompt_tokens = 100_000;
        let mut warmer = CacheWarmer::new(&host);
        warmer.start(request(Some(300)), current());
        // Deadline = next_warm_at + (ttl - delay)/2 = 270_000 + 15_000.
        host.state.lock().unwrap().now = 286_000;
        warmer.refresh().await;
        assert_eq!(
            warmer.status().reason.as_deref(),
            Some("cache refresh deadline missed")
        );
    }

    #[tokio::test]
    async fn context_change_stops_before_sending() {
        let host = TestHost::new();
        host.state.lock().unwrap().prompt_tokens = 100_000;
        let current = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let flag = current.clone();
        let mut warmer = CacheWarmer::new(&host);
        warmer.start(request(Some(300)), move || flag.load(Ordering::SeqCst));
        current.store(false, Ordering::SeqCst);
        warmer.refresh().await;
        assert_eq!(
            warmer.status().reason.as_deref(),
            Some("conversation context changed")
        );
        assert_eq!(host.state.lock().unwrap().stream_requests.len(), 0);
    }

    // --- lifecycle ---------------------------------------------------------

    #[test]
    fn streaming_mode_stops_when_the_agent_settles() {
        let host = TestHost::new();
        host.state.lock().unwrap().mode = CacheWarmingMode::Streaming;
        host.state.lock().unwrap().prompt_tokens = 100_000;
        let mut warmer = CacheWarmer::new(&host);
        warmer.start(request(Some(300)), current());
        warmer.on_agent_settled();
        assert_eq!(warmer.status().reason.as_deref(), Some("agent run settled"));
    }

    #[test]
    fn idle_mode_switches_phase_and_keeps_warming() {
        let host = TestHost::new();
        host.state.lock().unwrap().prompt_tokens = 100_000;
        let mut warmer = CacheWarmer::new(&host);
        warmer.start(request(Some(300)), current());
        warmer.on_agent_settled();
        let status = warmer.status();
        assert_eq!(status.state, CacheWarmingState::Scheduled);
        assert_eq!(
            status.decision.as_ref().map(|d| d.phase),
            Some(CacheWarmingPhase::Idle)
        );
        let probability = status
            .decision
            .as_ref()
            .map(|d| d.continuation_probability)
            .unwrap_or_default();
        assert!((probability - 0.15).abs() < 1e-9);
    }

    #[test]
    fn idle_safety_limit_stops_after_settling_late() {
        let host = TestHost::new();
        host.state.lock().unwrap().prompt_tokens = 100_000;
        let mut warmer = CacheWarmer::new(&host);
        warmer.start(request(Some(300)), current());
        host.state.lock().unwrap().now = 31 * 60 * 1000;
        warmer.on_agent_settled();
        assert_eq!(
            warmer.status().reason.as_deref(),
            Some("30-minute idle safety limit reached")
        );
    }

    #[test]
    fn one_hour_safety_limit_stops_at_start() {
        let host = TestHost::new();
        host.state.lock().unwrap().prompt_tokens = 100_000;
        let mut warmer = CacheWarmer::new(&host);
        // Two-hour TTL → 108-minute delay exceeds the one-hour window.
        warmer.start(request(Some(7200)), current());
        assert_eq!(
            warmer.status().reason.as_deref(),
            Some("one-hour safety limit reached")
        );
    }

    #[test]
    fn mode_change_and_cancel_stop_the_run() {
        let host = TestHost::new();
        host.state.lock().unwrap().prompt_tokens = 100_000;
        let mut warmer = CacheWarmer::new(&host);
        warmer.start(request(Some(300)), current());
        host.state.lock().unwrap().mode = CacheWarmingMode::Off;
        warmer.on_mode_changed();
        assert_eq!(
            warmer.status().reason.as_deref(),
            Some("cache warming disabled")
        );

        host.state.lock().unwrap().mode = CacheWarmingMode::Idle;
        warmer.start(request(Some(300)), current());
        warmer.cancel();
        assert_eq!(warmer.status().reason.as_deref(), Some("inactive"));
    }

    #[test]
    fn start_aborts_the_previous_run() {
        let host = TestHost::new();
        host.state.lock().unwrap().prompt_tokens = 100_000;
        let mut warmer = CacheWarmer::new(&host);
        warmer.start(request(Some(300)), current());
        // A second `start` replaces the run: only the new TTL's due time
        // survives. `clear_run` also flips the old run's abort flag, which a
        // real provider observes through `WarmStreamRequest::aborted`.
        warmer.start(request(Some(600)), current());
        assert_eq!(warmer.next_warm_at(), Some(540_000));
    }

    // --- formatting --------------------------------------------------------

    #[test]
    fn status_formatting_covers_inactive_scheduled_and_stopped() {
        assert_eq!(
            format_cache_warming_status(&CacheWarmingStatus::default(), 0),
            "Inactive (unknown reason)"
        );
        let decision = evaluate_cache_warming(&model(Some(300)), 100_000, CacheWarmingPhase::Idle);
        let status = CacheWarmingStatus {
            state: CacheWarmingState::Scheduled,
            next_warm_at: Some(3700),
            decision: Some(decision),
            ..CacheWarmingStatus::default()
        };
        // 3.7 s remaining → ceil to 4 s. 100k idle tokens: miss $0.345,
        // warm $0.030015, expected 0.15 * 0.345 - 0.030015 = $0.021735 → stop.
        assert_eq!(
            format_cache_warming_status(&status, 0),
            "Decision in 4s (15% continuation probability, expected savings $0.022 < $0.050 -> stop)"
        );
        let stopped = CacheWarmingStatus {
            state: CacheWarmingState::Inactive,
            reason: Some("expected savings below threshold".to_string()),
            ..status
        };
        assert!(format_cache_warming_status(&stopped, 0).starts_with("Stopped ("));
    }

    #[test]
    fn usage_formatting_trims_trailing_zeros() {
        assert_eq!(
            format_cache_warming_usage(0.0012, None),
            "Cache warmed: $0.0012"
        );
        assert_eq!(
            format_cache_warming_usage(0.0012, Some("extension override")),
            "Cache warmed (extension override): $0.0012"
        );
        assert_eq!(
            format_cache_warming_usage(0.0, None),
            "Cache warmed: $0.000"
        );
        assert_eq!(
            format_cache_warming_usage(1.23, None),
            "Cache warmed: $1.230"
        );
    }
}
