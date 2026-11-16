//! The automatic compaction machine (`agent-session.ts`'s
//! `_checkCompaction` :3119-3253, `_runAutoCompaction` :3265-3412, and
//! `_compactBeforeNextAssistantResponse` :839-848), shared by the
//! print/JSON/RPC run tails; `pi-ai`'s overflow classifiers feed the
//! overflow cases. Upstream drives this from the agent loop's post-run
//! hook; the port spreads the agent-session across the modes, so the
//! controller lives here and each run tail drives it (the `retry.rs`
//! precedent). Upstream `_handlePostAgentRun` (:2062-2098) decides the
//! retry FIRST — compaction only runs when no retry was scheduled.
//!
//! Port adaptations (see HANDOFF.md): the run tails persist messages after
//! the loop, so at decision time the failed response is unpersisted —
//! upstream's entry-id projection guards (`assistantIsProjected`,
//! `_omitRecoveryAttempt` :3149-3170) collapse to "projected and retained";
//! the omission is the port simply not persisting the dropped attempt. The
//! `session_before_compact`/`session_compact`/`session_compact_failed`
//! extension events ride the E3 bridge. The queued-messages continue
//! (`:3379`) is the run tail's loop structure itself. The interactive
//! pump's integration lands with its `AgentRun` rework.

use pi_agent::agent_message::AgentMessage;
use pi_agent::events::{AgentEvent, AgentEventSink, CompactionReason, CompactionResultEvent};
use pi_agent::types::AbortSignal;
use pi_agent::types::{StreamFn, ThinkingLevel};
use pi_ai::types::Model;

use crate::session::compaction::{
    self, CompactionError, CompactionSettings, DEFAULT_COMPACTION_SETTINGS,
};
use crate::session::records::CompactionEntry;
use crate::session::store::SessionManager;

/// Post-run compaction decision (`_checkCompaction`'s outcome :3145-3252).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// No automatic compaction.
    None,
    /// Context overflow: compact; `will_retry` re-runs the turn once
    /// (upstream `_runAutoCompaction("overflow", true)` → `agent.continue()`).
    Overflow { will_retry: bool },
    /// Threshold crossed: compact without retrying the response
    /// (`_runAutoCompaction("threshold", false)`).
    Threshold,
    /// The one overflow compact-and-retry attempt already failed
    /// (`_overflowRecoveryAttempted` :3187-3207); the caller emits the
    /// verbatim failure and ends the run.
    RecoveryFailed { message: String },
}

/// One post-run decision pass over a run's messages (attempts reset when a
/// run succeeds — the overflow-retry budget is one per run).
pub struct AutoCompaction {
    settings: CompactionSettings,
    overflow_recovery_attempted: bool,
}

impl AutoCompaction {
    pub fn new(settings: CompactionSettings) -> Self {
        Self {
            settings,
            overflow_recovery_attempted: false,
        }
    }

    /// `exceedsCompactionThreshold` (`agent-session.ts:829-837`): a
    /// zero/unknown window never compacts.
    pub fn exceeds_threshold(&self, context_tokens: u64, context_window: u64) -> bool {
        if context_window == 0 {
            return false;
        }
        compaction::should_compact(context_tokens, context_window, &self.settings)
    }

    /// `_compactBeforeNextAssistantResponse` (:839-848) for the
    /// engine-free shape: the threshold gate keys on the model about to
    /// answer, estimated over the session projection. Under a virtual
    /// selection upstream re-checks against the ROUTED model after the
    /// resolve (:901-904) — deferred with the prepare-hook rework (see
    /// HANDOFF).
    pub fn exceeds_threshold_for_session(&self, session: &SessionManager, model: &Model) -> bool {
        if model.context_window == 0 {
            return false;
        }
        let Ok(branch) = session.get_branch(None) else {
            return false;
        };
        let projection = crate::session::store::build_session_projection_from(&branch, None);
        let tokens = compaction::estimate_projected_context_tokens(&projection, &branch).tokens;
        self.exceeds_threshold(tokens, model.context_window)
    }

    /// `_checkCompaction` (:3119-3252): overflow first (cases 1 and 2),
    /// then the threshold estimate (case 3). `messages` is the run's own
    /// output (the last assistant decides); `full_messages` is the whole
    /// conversation state the case-3 estimate reads
    /// (`agent-session.ts:3228`). `model` is the selection; the message's
    /// own model resolves through the composed catalog
    /// (`_modelForMessage` :653-657) and gates the usage-based checks.
    pub fn post_run_decision(
        &mut self,
        aborted: bool,
        messages: &[AgentMessage],
        full_messages: &[AgentMessage],
        session: &SessionManager,
        model: &Model,
    ) -> Decision {
        if !self.settings.enabled {
            return Decision::None;
        }
        // Skip if message was aborted (user cancelled) - unless
        // skipAbortedCheck is false (:3124; the port always skips).
        if aborted {
            return Decision::None;
        }
        let Some(assistant) = last_assistant(messages) else {
            return Decision::None;
        };

        // Skip compaction checks if this assistant message is older than
        // the latest compaction boundary (:3138-3143) — a stale
        // pre-compaction usage/error must not retrigger compaction on the
        // first prompt after compaction.
        let Ok(branch) = session.get_branch(None) else {
            return Decision::None;
        };
        if let Some(compaction_entry) = crate::session::store::get_latest_compaction_entry(&branch)
        {
            let boundary_ms = crate::session::store::iso_to_ms(&compaction_entry.base.timestamp);
            if assistant.timestamp <= boundary_ms {
                return Decision::None;
            }
        }

        // `_modelForMessage` (:653-657): under a virtual selection the
        // physical model that produced the message supplies the limits;
        // otherwise the selection must match the message's provider/model.
        let message_model = message_model(assistant, model);
        let same_model = message_model.is_some();
        let context_window = message_model
            .as_ref()
            .map_or(model.context_window, |m| m.context_window);

        // Automatic cases 1 and 2: context overflow (:3145-3215). The port's
        // failed response is unpersisted at decision time, so it is always
        // projected and retained for recovery (see the module docs).
        let explicit_overflow = assistant.stop_reason == pi_ai::types::StopReason::Error
            && pi_ai::overflow::is_context_overflow(assistant, None);
        let context_overflow = same_model
            && (explicit_overflow
                || pi_ai::overflow::is_context_overflow(assistant, Some(context_window)));
        let recoverable_length = same_model
            && message_model.is_some_and(|message_model| {
                pi_ai::overflow::is_recoverable_length(assistant, message_model.max_tokens)
            });
        if context_overflow || recoverable_length {
            let will_retry = assistant.stop_reason != pi_ai::types::StopReason::Stop;
            // Case 2: the response completed successfully. Compact, but do
            // not retry because agent.continue() cannot continue from a
            // completed assistant response (:3181-3185).
            if !will_retry {
                return Decision::Overflow { will_retry: false };
            }
            if self.overflow_recovery_attempted {
                let message = if context_overflow {
                    "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model."
                } else {
                    "Truncated response recovery failed after one compact-and-retry attempt."
                };
                return Decision::RecoveryFailed {
                    message: message.to_string(),
                };
            }
            // Persistently omit the selected final attempt before post-run
            // recovery compaction (:3209-3211) — in the port the attempt is
            // simply not persisted when the loop re-runs.
            self.overflow_recovery_attempted = true;
            return Decision::Overflow { will_retry: true };
        }

        // Case 3: threshold compaction without retry (:3217-3252). For
        // error messages or all-zero usage messages, estimate from the last
        // valid response; with context edits the projection estimate wins.
        let projection = crate::session::store::build_session_projection_from(&branch, None);
        let direct_context_tokens = compaction::calculate_context_tokens(&assistant.usage);
        let has_context_edits = projection.entries.iter().any(|entry| {
            matches!(
                entry.source_entry,
                crate::session::records::SessionEntry::ContextEdit(_)
            )
        });
        let context_tokens = if has_context_edits {
            compaction::estimate_projected_context_tokens(&projection, &branch).tokens
        } else if assistant.stop_reason == pi_ai::types::StopReason::Error
            || direct_context_tokens == 0
        {
            let estimate = compaction::estimate_context_tokens(full_messages);
            // Without provider usage, estimate.tokens is the pure
            // message-size estimate. Only usage-backed estimates need the
            // stale pre-compaction check (:3232-3244).
            if let Some(last_usage_index) = estimate.last_usage_index {
                let usage_message = &full_messages[last_usage_index];
                // Only assistant usage sources carry the staleness check
                // (`usageMsg.role === "assistant"`, :3237-3240).
                let stale = match (compaction_entry_in(&branch), usage_message) {
                    (Some(entry), AgentMessage::Assistant(a)) => {
                        a.timestamp <= crate::session::store::iso_to_ms(&entry.base.timestamp)
                    }
                    _ => false,
                };
                if stale {
                    return Decision::None;
                }
            }
            estimate.tokens
        } else {
            direct_context_tokens
        };
        if self.exceeds_threshold(context_tokens, context_window) {
            return Decision::Threshold;
        }
        Decision::None
    }

    /// `_runAutoCompaction` (:3265-3412): prepare, run the summarization,
    /// append the record, and emit the lifecycle events. Returns whether
    /// the interrupted turn should re-run (only the overflow retry).
    /// Upstream's `session_before_compact`/`session_compact`/
    /// `session_compact_failed` extension events ride the E3 bridge.
    #[allow(clippy::too_many_lines)] // mirrors the TS run loop; split when extending
    #[allow(clippy::too_many_arguments)] // mirrors _runAutoCompaction's inputs
    pub fn run(
        &mut self,
        reason: CompactionReason,
        will_retry: bool,
        session: &mut SessionManager,
        model: &Model,
        api_key: Option<&str>,
        access_token: Option<&str>,
        thinking_level: ThinkingLevel,
        stream_fn: &dyn StreamFn,
        sink: &mut dyn AgentEventSink,
    ) -> bool {
        let Ok(branch) = session.get_branch(None) else {
            return false;
        };
        let Some(preparation) = compaction::prepare_compaction(&branch, self.settings) else {
            return false;
        };
        let signal = AbortSignal::new();
        crate::runtime::block_on(sink.emit(AgentEvent::CompactionStart { reason }));
        match crate::runtime::block_on(compaction::compact(
            &preparation,
            model,
            api_key,
            access_token,
            None,
            &signal,
            thinking_level,
            stream_fn,
        )) {
            Ok(result) => {
                let details = compaction::details_json(&result);
                let summary = result.summary.clone();
                let first_kept_entry_id = result.first_kept_entry_id.clone();
                let tokens_before = result.tokens_before;
                let usage = result.usage;
                match session.append_compaction(
                    &summary,
                    Some(&first_kept_entry_id),
                    tokens_before,
                    Some(details.clone()),
                    // fromExtension=false — the shared default generator.
                    Some(false),
                    usage,
                ) {
                    Ok(_entry) => {
                        // `estimatedTokensAfter` over the post-compaction
                        // projection (:3348).
                        let estimated_tokens_after = session.get_branch(None).ok().map(|branch| {
                            let projection =
                                crate::session::store::build_session_projection_from(&branch, None);
                            compaction::estimate_context_tokens(&projection.messages).tokens
                        });
                        crate::runtime::block_on(sink.emit(AgentEvent::CompactionEnd {
                            reason,
                            result: Some(CompactionResultEvent {
                                summary,
                                first_kept_entry_id,
                                tokens_before,
                                estimated_tokens_after,
                                usage,
                                details,
                            }),
                            aborted: false,
                            will_retry,
                            error_message: None,
                        }));
                        will_retry
                    }
                    Err(err) => Self::emit_failure(reason, sink, &err.to_string()),
                }
            }
            Err(err) => match err {
                CompactionError::Cancelled => {
                    // `aborted` with no error message (:3382-3388).
                    crate::runtime::block_on(sink.emit(AgentEvent::CompactionEnd {
                        reason,
                        result: None,
                        aborted: true,
                        will_retry: false,
                        error_message: None,
                    }));
                    false
                }
                CompactionError::Failed(message) => Self::emit_failure(reason, sink, &message),
            },
        }
    }

    /// The failure `compaction_end` (:3383-3396): the reason-specific
    /// verbatim prefix, `willRetry: false`.
    fn emit_failure(
        reason: CompactionReason,
        sink: &mut dyn AgentEventSink,
        message: &str,
    ) -> bool {
        let error_message = if reason == CompactionReason::Overflow {
            format!("Context overflow recovery failed: {message}")
        } else {
            format!("Auto-compaction failed: {message}")
        };
        crate::runtime::block_on(sink.emit(AgentEvent::CompactionEnd {
            reason,
            result: None,
            aborted: false,
            will_retry: false,
            error_message: Some(error_message),
        }));
        false
    }

    /// `session.compact(customInstructions)` (`agent-session.ts:2936-3072`):
    /// the manual compaction behind the RPC `compact` command. `compaction_start`
    /// goes out before the gate, so even a refused compaction produces the
    /// start/end pair; the `compaction_end` failure line carries the manual
    /// `Compaction failed: ` prefix (:3060) while the returned error carries
    /// the verbatim refusal, which is what the RPC error response echoes.
    /// The port's `session_before_compact` handlers and the entry abort are
    /// not wired yet, so `fromExtension` is always `false` and the run is
    /// never aborted.
    #[allow(clippy::too_many_lines)] // mirrors session.compact; split when extending
    pub fn run_manual(
        &mut self,
        custom_instructions: Option<&str>,
        session: &mut SessionManager,
        model: &Model,
        api_key: Option<&str>,
        access_token: Option<&str>,
        thinking_level: ThinkingLevel,
        stream_fn: &dyn StreamFn,
        sink: &mut dyn AgentEventSink,
    ) -> Result<CompactionResultEvent, String> {
        crate::runtime::block_on(sink.emit(AgentEvent::CompactionStart {
            reason: CompactionReason::Manual,
        }));
        let fail = |sink: &mut dyn AgentEventSink, message: &str| {
            crate::runtime::block_on(sink.emit(AgentEvent::CompactionEnd {
                reason: CompactionReason::Manual,
                result: None,
                aborted: false,
                will_retry: false,
                error_message: Some(format!("Compaction failed: {message}")),
            }));
            message.to_string()
        };
        let branch = match session.get_branch(None) {
            Ok(branch) => branch,
            Err(err) => return Err(fail(sink, &err.to_string())),
        };
        let Some(preparation) = compaction::prepare_compaction(&branch, self.settings) else {
            // The two refusals (`agent-session.ts:2952-2960`).
            let message = match branch.last() {
                Some(crate::session::records::SessionEntry::Compaction(..)) => "Already compacted",
                _ => "Nothing to compact (session too small)",
            };
            return Err(fail(sink, message));
        };
        let signal = AbortSignal::new();
        match crate::runtime::block_on(compaction::compact(
            &preparation,
            model,
            api_key,
            access_token,
            custom_instructions,
            &signal,
            thinking_level,
            stream_fn,
        )) {
            Ok(result) => {
                let details = compaction::details_json(&result);
                let summary = result.summary.clone();
                let first_kept_entry_id = result.first_kept_entry_id.clone();
                let tokens_before = result.tokens_before;
                let usage = result.usage;
                match session.append_compaction(
                    &summary,
                    Some(&first_kept_entry_id),
                    tokens_before,
                    Some(details.clone()),
                    // fromExtension=false — the shared default generator.
                    Some(false),
                    usage,
                ) {
                    Ok(_entry) => {
                        let estimated_tokens_after = session.get_branch(None).ok().map(|branch| {
                            let projection =
                                crate::session::store::build_session_projection_from(&branch, None);
                            compaction::estimate_context_tokens(&projection.messages).tokens
                        });
                        let event = CompactionResultEvent {
                            summary,
                            first_kept_entry_id,
                            tokens_before,
                            estimated_tokens_after,
                            usage,
                            details,
                        };
                        crate::runtime::block_on(sink.emit(AgentEvent::CompactionEnd {
                            reason: CompactionReason::Manual,
                            result: Some(event.clone()),
                            aborted: false,
                            will_retry: false,
                            error_message: None,
                        }));
                        Ok(event)
                    }
                    Err(err) => Err(fail(sink, &err.to_string())),
                }
            }
            // The local signal is never aborted here; a cancelled compaction
            // or a model failure both surface through `Failed`.
            Err(err) => match err {
                CompactionError::Cancelled => Err(fail(sink, "Compaction cancelled")),
                CompactionError::Failed(message) => Err(fail(sink, &message)),
            },
        }
    }
}

/// The last assistant message of a run (`agent-session.ts:1270-1277`
/// scans backwards) — the same scan the retry machine uses.
pub(crate) fn last_assistant(messages: &[AgentMessage]) -> Option<&pi_ai::types::AssistantMessage> {
    messages.iter().rev().find_map(|message| match message {
        AgentMessage::Assistant(assistant) => Some(assistant),
        _ => None,
    })
}

/// `_modelForMessage` (:653-657): the physical model that produced the
/// message, or the selection when it matches. Without the engine the
/// composed catalog is the static one and lookups are equivalent.
fn message_model(assistant: &pi_ai::types::AssistantMessage, model: &Model) -> Option<Model> {
    #[cfg(feature = "ext-js")]
    {
        if crate::virtual_models::is_virtual_model(model) {
            return crate::models_state::lookup(&assistant.provider, &assistant.model);
        }
    }
    if model.provider == assistant.provider && model.id == assistant.model {
        Some(model.clone())
    } else {
        None
    }
}

/// The latest compaction entry of a branch, shared by the two stale checks.
fn compaction_entry_in(
    branch: &[crate::session::records::SessionEntry],
) -> Option<&CompactionEntry> {
    crate::session::store::get_latest_compaction_entry(branch)
}

/// The default machine (upstream builds per session from settings).
impl Default for AutoCompaction {
    fn default() -> Self {
        Self::new(DEFAULT_COMPACTION_SETTINGS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::types::{StopReason, Usage};

    fn assistant_message(stop_reason: StopReason, error: Option<&str>) -> AgentMessage {
        AgentMessage::Assistant(pi_ai::types::AssistantMessage {
            error_message: error.map(str::to_string),
            ..test_assistant(stop_reason)
        })
    }

    fn test_assistant(stop_reason: StopReason) -> pi_ai::types::AssistantMessage {
        pi_ai::types::AssistantMessage {
            content: vec![],
            api: "openai-completions".into(),
            provider: "test".into(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: vec![],
            usage: Usage {
                input: 0,
                output: 0,
                cache_read: 0,
                cache_write: 0,
                total_tokens: 0,
                ..Usage::default()
            },
            stop_reason,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
            duration_ms: None,
        }
    }

    fn settings() -> CompactionSettings {
        CompactionSettings {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
        }
    }

    fn model() -> Model {
        // Matches the test messages' provider/model so the same-model gate
        // (`_modelForMessage` :653-657) opens.
        Model {
            provider: "test".to_string(),
            id: "m".to_string(),
            context_window: 100_000,
            ..Model::default()
        }
    }

    #[test]
    fn threshold_needs_headroom_and_a_window() {
        let machine = AutoCompaction::new(settings());
        assert!(!machine.exceeds_threshold(80_000, 100_000));
        assert!(machine.exceeds_threshold(90_000, 100_000)); // > 100000-16384
        assert!(!machine.exceeds_threshold(90_000, 0));
    }

    #[test]
    fn disabled_or_aborted_never_compacts() {
        let mut machine = AutoCompaction::new(CompactionSettings {
            enabled: false,
            ..settings()
        });
        let messages = vec![assistant_message(StopReason::Stop, None)];
        assert_eq!(
            machine.post_run_decision(false, &messages, &messages, &no_session(), &model()),
            Decision::None
        );
        machine = AutoCompaction::new(settings());
        assert_eq!(
            machine.post_run_decision(true, &messages, &messages, &no_session(), &model()),
            Decision::None
        );
    }

    #[test]
    fn overflow_errors_decide_by_classifier_and_model_match() {
        let mut machine = AutoCompaction::new(settings());
        let messages = vec![assistant_message(
            StopReason::Error,
            Some("prompt is too long: 213462 tokens > 200000 maximum"),
        )];
        assert_eq!(
            machine.post_run_decision(false, &messages, &messages, &no_session(), &model()),
            Decision::Overflow { will_retry: true }
        );
        // A mismatched model disables every overflow check — the overflow
        // from a different model must not compact for this one
        // (:3126-3130, `sameModel` gates the explicit case too).
        let mut foreign = test_assistant(StopReason::Error);
        foreign.provider = "other".to_string();
        foreign.model = "m2".to_string();
        foreign.error_message = Some("prompt is too long".to_string());
        assert_eq!(
            machine.post_run_decision(
                false,
                &[AgentMessage::Assistant(foreign.clone())],
                &[AgentMessage::Assistant(foreign)],
                &no_session(),
                &model()
            ),
            Decision::None
        );
        // A successful stop is overflow without retry (case 2).
        let mut silent = test_assistant(StopReason::Stop);
        silent.usage.input = 300_000;
        assert_eq!(
            machine.post_run_decision(
                false,
                &[AgentMessage::Assistant(silent.clone())],
                &[AgentMessage::Assistant(silent)],
                &no_session(),
                &model()
            ),
            Decision::Overflow { will_retry: false }
        );
    }

    #[test]
    fn overflow_recovery_is_one_attempt_per_run() {
        let mut machine = AutoCompaction::new(settings());
        let messages = vec![assistant_message(
            StopReason::Error,
            Some("Your input exceeds the context window of this model"),
        )];
        assert_eq!(
            machine.post_run_decision(false, &messages, &messages, &no_session(), &model()),
            Decision::Overflow { will_retry: true }
        );
        match machine.post_run_decision(false, &messages, &messages, &no_session(), &model()) {
            Decision::RecoveryFailed { message } => {
                assert!(
                    message.contains("one compact-and-retry attempt"),
                    "{message}"
                );
            }
            decision => panic!("expected recovery failure, got {decision:?}"),
        }
    }

    #[test]
    fn no_assistant_message_decides_none() {
        let mut machine = AutoCompaction::new(settings());
        assert_eq!(
            machine.post_run_decision(false, &[], &[], &no_session(), &model()),
            Decision::None
        );
    }

    /// A fresh in-memory session (no branch entries) for the decision
    /// guards; compaction execution is covered by the store's tests.
    fn no_session() -> SessionManager {
        SessionManager::in_memory(
            std::path::Path::new("/tmp"),
            &crate::session::store::NewSessionOptions::default(),
            Vec::new(),
        )
        .expect("session")
    }
}
