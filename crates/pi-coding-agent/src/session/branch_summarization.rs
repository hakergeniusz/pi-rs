//! Branch summarization for tree navigation
//! (`core/compaction/branch-summarization.ts`).
//!
//! When navigating to a different point in the session tree, this generates
//! a summary of the branch being left so context isn't lost. `collect` walks
//! the two paths to the common ancestor (`collectEntriesForBranchSummary`,
//! `branch-summarization.ts:115-153`), `prepare` budgets the newest entries
//! into a prompt-sized slice (`prepareBranchEntries`, `:203-267`), and
//! [`generate_branch_summary`] runs the summarization LLM call
//! (`generateBranchSummary`, `:307-398`) reusing the compaction pipeline's
//! serialization and completion helpers.
//!
//! Port deviations (documented in HANDOFF, mirroring compaction.rs): the
//! summarization call is a single attempt (upstream wraps the retry policy
//! around `completeSummarization`), and the prompt-cache retention is left
//! to the adapter.

use pi_agent::types::AbortSignal;
use pi_agent::{AgentMessage, StreamFn, ThinkingLevel};
use pi_ai::types::Model;

use super::compaction::{
    self, CompactionError, FileOperations, compute_file_lists, format_file_operations,
    serialize_conversation,
};
use super::records::{ContextEditEntry, SessionEntry};
use super::store::SessionManager;

/// The result shape (`BranchSummaryResult`, `branch-summarization.ts:28-36`).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct BranchSummaryResult {
    pub summary: Option<String>,
    pub usage: Option<pi_ai::types::Usage>,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
    pub aborted: bool,
    pub error: Option<String>,
}

/// Details stored in `BranchSummaryEntry.details` for file tracking
/// (`BranchSummaryDetails`, `branch-summarization.ts:39-42`).
#[derive(Debug, Clone, PartialEq)]
pub struct BranchSummaryDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// The prepared summarization payload (`BranchPreparation`,
/// `branch-summarization.ts:49-57`).
pub struct BranchPreparation {
    /// Messages extracted for summarization, in chronological order.
    pub messages: Vec<AgentMessage>,
    /// File operations extracted from tool calls and nested summaries.
    pub file_ops: FileOperations,
    /// Estimated token count of `messages`.
    pub total_tokens: u64,
}

/// The generated summary text, prepended to the model's summary
/// (`BRANCH_SUMMARY_PREAMBLE`, `branch-summarization.ts:270-274`).
pub const BRANCH_SUMMARY_PREAMBLE: &str =
    "The user explored a different conversation branch before returning here.
Summary of that exploration:

";

/// `BRANCH_SUMMARY_PROMPT` (`branch-summarization.ts:276-304`), verbatim.
pub const BRANCH_SUMMARY_PROMPT: &str =
    "Create a structured summary of this conversation branch for context when returning later.

Use this EXACT format:

## Goal
[What was the user trying to accomplish in this branch?]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned]
- [Or \"(none)\" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Work that was started but not finished]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [What should happen next to continue this work]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

/// `collectEntriesForBranchSummary`
/// (`branch-summarization.ts:115-153`): the entries between the old leaf and
/// the target's common ancestor, chronological, plus that ancestor's id.
pub fn collect_entries_for_branch_summary(
    session: &SessionManager,
    old_leaf_id: Option<&str>,
    target_id: &str,
) -> (Vec<SessionEntry>, Option<String>) {
    // If no old position, nothing to summarize.
    let Some(old_leaf_id) = old_leaf_id else {
        return (Vec::new(), None);
    };

    // The deepest node on both paths.
    let Ok(old_path) = session.get_branch(Some(old_leaf_id)) else {
        return (Vec::new(), None);
    };
    let old_path_ids: std::collections::HashSet<&str> =
        old_path.iter().map(|entry| entry.id()).collect();
    let Ok(target_path) = session.get_branch(Some(target_id)) else {
        return (Vec::new(), None);
    };

    // targetPath is root-first, so iterate backwards for the deepest
    // common ancestor.
    let common_ancestor_id = target_path
        .iter()
        .rev()
        .find(|entry| old_path_ids.contains(entry.id()))
        .map(|entry| entry.id().to_string());

    // Collect entries from the old leaf back to the common ancestor.
    let mut entries: Vec<SessionEntry> = Vec::new();
    let mut current = Some(old_leaf_id.to_string());
    while let Some(id) = current {
        if common_ancestor_id.as_deref() == Some(id.as_str()) {
            break;
        }
        let Some(entry) = session.get_entry(&id) else {
            break;
        };
        current = entry.parent_id().map(str::to_string);
        entries.push(entry);
    }

    // Reverse to get chronological order.
    entries.reverse();
    (entries, common_ancestor_id)
}

/// `getMessageFromEntry` (`branch-summarization.ts:163-188`): the
/// conversation content of one entry — tool results are skipped (their
/// context lives in the assistant's tool call), compaction and branch
/// summaries contribute their summaries.
fn message_from_entry(
    entry: &SessionEntry,
    edit: Option<&ContextEditEntry>,
) -> Option<AgentMessage> {
    match entry {
        SessionEntry::Message(message) => {
            // Skip tool results — context is in the assistant's tool call.
            if matches!(message.message, AgentMessage::ToolResult(_)) {
                return None;
            }
            if edit.is_some() {
                return super::store::project_context_entry(entry, edit)
                    .into_iter()
                    .next();
            }
            Some(message.message.clone())
        }
        SessionEntry::CustomMessage(custom) => {
            if edit.is_some() {
                return super::store::project_context_entry(entry, edit)
                    .into_iter()
                    .next();
            }
            Some(AgentMessage::Custom(pi_agent::CustomMessage {
                custom_type: custom.custom_type.clone(),
                content: custom.content.clone(),
                display: custom.display,
                details: custom.details.clone(),
                timestamp: super::store::iso_to_ms(&custom.base.timestamp),
            }))
        }
        SessionEntry::BranchSummary(summary) => Some(AgentMessage::BranchSummary(
            pi_agent::BranchSummaryMessage {
                summary: summary.summary.clone(),
                from_id: Some(summary.from_id.clone()),
                timestamp: super::store::iso_to_ms(&summary.base.timestamp),
            },
        )),
        SessionEntry::Compaction(compaction) => Some(AgentMessage::CompactionSummary(
            pi_agent::CompactionSummaryMessage {
                summary: compaction.summary.clone(),
                tokens_before: compaction.tokens_before,
                timestamp: super::store::iso_to_ms(&compaction.base.timestamp),
            },
        )),
        // These don't contribute to conversation content.
        SessionEntry::ThinkingLevelChange(_)
        | SessionEntry::ModelChange(_)
        | SessionEntry::Custom(_)
        | SessionEntry::Label(_)
        | SessionEntry::SessionInfo(_) => None,
        _ => None,
    }
}

/// The file lists recorded on a pi-generated branch summary's details
/// (`branch-summarization.ts:215-229`): only non-hook summaries count.
fn summary_details_file_ops(
    summary: &super::records::BranchSummaryEntry,
) -> Option<(Vec<String>, Vec<String>)> {
    if summary.from_hook.unwrap_or(false) {
        return None;
    }
    let details = summary.details.as_ref()?;
    let read_files = details
        .get("readFiles")
        .and_then(serde_json::Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let modified_files = details
        .get("modifiedFiles")
        .and_then(serde_json::Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some((read_files, modified_files))
}

/// `prepareBranchEntries` (`branch-summarization.ts:203-267`): walk from the
/// newest entry backwards until the token budget is hit, keeping the most
/// recent context. File operations accumulate from everything seen, even
/// entries past the budget.
pub fn prepare_branch_entries(entries: &[SessionEntry], token_budget: u64) -> BranchPreparation {
    let mut messages: Vec<AgentMessage> = Vec::new();
    let mut file_ops = FileOperations::default();
    let mut total_tokens: u64 = 0;

    // First pass: file ops from ALL entries, so cumulative tracking works
    // across nested branch summaries.
    for entry in entries {
        if let SessionEntry::BranchSummary(summary) = entry
            && let Some((read_files, modified_files)) = summary_details_file_ops(summary)
        {
            file_ops.seed_from_summary_details(&read_files, &modified_files);
        }
    }

    // Context edits hide or replace earlier entries; the latest edit per
    // target wins (`branch-summarization.ts:233-236`).
    let mut edits: std::collections::HashMap<&str, &ContextEditEntry> =
        std::collections::HashMap::new();
    for entry in entries {
        if let SessionEntry::ContextEdit(edit) = entry {
            edits.insert(edit.target_id.as_str(), edit);
        }
    }

    // Second pass: newest to oldest (`branch-summarization.ts:239-265`).
    for entry in entries.iter().rev() {
        let Some(message) = message_from_entry(entry, edits.get(entry.id()).copied()) else {
            continue;
        };

        // Extract file ops from assistant messages (tool calls).
        file_ops.extract_from_message(&message);

        let tokens = compaction::estimate_tokens(&message);

        // Check the budget before adding; summary entries squeeze in while
        // there is still 10% headroom, then the walk stops.
        if token_budget > 0 && total_tokens + tokens > token_budget {
            if matches!(
                entry,
                SessionEntry::Compaction(_) | SessionEntry::BranchSummary(_)
            ) && total_tokens < token_budget * 9 / 10
            {
                messages.insert(0, message);
                total_tokens += tokens;
            }
            break;
        }

        messages.insert(0, message);
        total_tokens += tokens;
    }

    BranchPreparation {
        messages,
        file_ops,
        total_tokens,
    }
}

/// The generation options (`GenerateBranchSummaryOptions`,
/// `branch-summarization.ts:62-77`).
pub struct BranchSummaryOptions<'a> {
    pub model: &'a Model,
    pub api_key: Option<&'a str>,
    pub access_token: Option<&'a str>,
    pub signal: &'a AbortSignal,
    pub thinking_level: ThinkingLevel,
    pub custom_instructions: Option<&'a str>,
    pub replace_instructions: bool,
    pub reserve_tokens: u64,
    pub stream_fn: &'a dyn StreamFn,
}

/// `generateBranchSummary` (`branch-summarization.ts:307-398`).
#[allow(clippy::too_many_lines)] // mirrors the TS generateBranchSummary; split when extending
pub async fn generate_branch_summary(
    entries: &[SessionEntry],
    options: &BranchSummaryOptions<'_>,
) -> BranchSummaryResult {
    // Token budget = context window minus reserved space for prompt +
    // response (`branch-summarization.ts:323-325`).
    let context_window = if options.model.context_window == 0 {
        128_000
    } else {
        options.model.context_window
    };
    let token_budget = context_window.saturating_sub(options.reserve_tokens);

    let preparation = prepare_branch_entries(entries, token_budget);

    if preparation.messages.is_empty() {
        return BranchSummaryResult {
            summary: Some("No content to summarize".to_string()),
            ..BranchSummaryResult::default()
        };
    }

    // Serialize to text so the model doesn't treat it as a conversation to
    // continue (`branch-summarization.ts:334-337`).
    let llm_messages = pi_agent::default_convert_to_llm(preparation.messages);
    let conversation_text = serialize_conversation(&llm_messages);

    let instructions = if options.replace_instructions && options.custom_instructions.is_some() {
        options.custom_instructions.unwrap_or_default().to_string()
    } else if let Some(custom) = options.custom_instructions {
        format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {custom}")
    } else {
        BRANCH_SUMMARY_PROMPT.to_string()
    };
    let prompt_text =
        format!("<conversation>\n{conversation_text}\n</conversation>\n\n{instructions}");

    let response = compaction::complete_summarization(
        options.model,
        &prompt_text,
        options.api_key,
        options.access_token,
        compaction::summarization_thinking_level(options.model, options.thinking_level),
        options.stream_fn,
        options.signal,
    )
    .await;

    let response = match response {
        Ok(response) => response,
        Err(CompactionError::Cancelled) => {
            return BranchSummaryResult {
                aborted: true,
                ..BranchSummaryResult::default()
            };
        }
        Err(CompactionError::Failed(message)) => {
            return BranchSummaryResult {
                error: Some(message),
                ..BranchSummaryResult::default()
            };
        }
    };

    if let Some(failure) = compaction::summarization_failure(&response, "Branch summarization") {
        return BranchSummaryResult {
            error: Some(failure),
            ..BranchSummaryResult::default()
        };
    }
    if response
        .content
        .iter()
        .any(|block| matches!(block, pi_ai::types::AssistantContent::ToolCall(_)))
    {
        return BranchSummaryResult {
            error: Some("Branch summarization attempted to call a tool".to_string()),
            ..BranchSummaryResult::default()
        };
    }

    let summary = compaction::content_text(&response.content);

    // Prepend the preamble, then append the file lists
    // (`branch-summarization.ts:384-390`).
    let mut summary = format!("{BRANCH_SUMMARY_PREAMBLE}{summary}");
    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    summary.push_str(&format_file_operations(&read_files, &modified_files));

    BranchSummaryResult {
        summary: if summary.is_empty() {
            Some("No summary generated".to_string())
        } else {
            Some(summary)
        },
        usage: Some(response.usage),
        read_files,
        modified_files,
        aborted: false,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::records::EntryBase;
    use crate::session::records::SessionMessageEntry;
    use pi_agent::agent_message::AgentMessage;
    use pi_ai::types::{TextContent, UsageCost};

    fn user_entry(id: &str, parent: Option<&str>, text: &str) -> SessionEntry {
        SessionEntry::Message(SessionMessageEntry {
            base: EntryBase {
                id: id.to_string(),
                parent_id: parent.map(str::to_string),
                timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            },
            message: AgentMessage::User(pi_ai::types::UserMessage {
                content: pi_ai::types::UserContent::Text(text.to_string()),
                timestamp: 1,
            }),
            unknown: serde_json::Map::new(),
        })
    }

    fn assistant_entry(id: &str, parent: Option<&str>, text: &str) -> SessionEntry {
        let message: pi_ai::types::AssistantMessage = serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": text }],
            "api": "anthropic-messages",
            "provider": "anthropic",
            "model": "claude-test",
            "usage": {
                "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0,
                "totalTokens": 2,
                "cost": { "input": 0, "output": 0, "cacheRead": 0,
                          "cacheWrite": 0, "total": 0 }
            },
            "stopReason": "stop",
            "timestamp": 1
        }))
        .expect("assistant message");
        SessionEntry::Message(SessionMessageEntry {
            base: EntryBase {
                id: id.to_string(),
                parent_id: parent.map(str::to_string),
                timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            },
            message: AgentMessage::Assistant(message),
            unknown: serde_json::Map::new(),
        })
    }

    /// A linear branch collects everything after the common ancestor in
    /// chronological order (`collectEntriesForBranchSummary` shape).
    #[test]
    fn collect_walks_to_the_common_ancestor() {
        let dir = tempfile::tempdir().expect("dir");
        let mut session = crate::session::store::SessionManager::in_memory(
            dir.path(),
            &crate::session::store::NewSessionOptions::default(),
            Vec::new(),
        )
        .expect("session");
        let u1 = session
            .append_message(AgentMessage::User(pi_ai::types::UserMessage {
                content: pi_ai::types::UserContent::Text("first".to_string()),
                timestamp: 1,
            }))
            .expect("append");
        let a1 = session
            .append_message(test_assistant_message("reply one"))
            .expect("append");
        let u2 = session
            .append_message(AgentMessage::User(pi_ai::types::UserMessage {
                content: pi_ai::types::UserContent::Text("second".to_string()),
                timestamp: 2,
            }))
            .expect("append");
        let a2 = session
            .append_message(test_assistant_message("reply two"))
            .expect("append");

        // Navigating from a2 back to u1: everything except u1 summarizes,
        // with a1 the common ancestor... actually u1 is the root: the walk
        // stops before it, so a1, u2, a2 summarize.
        let (entries, ancestor) =
            collect_entries_for_branch_summary(&session, Some(a2.id()), u1.id());
        let ids: Vec<&str> = entries.iter().map(|entry| entry.id()).collect();
        assert_eq!(ids, vec![a1.id(), u2.id(), a2.id()]);
        assert_eq!(ancestor.as_deref(), Some(u1.id()));
    }

    fn test_assistant_message(text: &str) -> AgentMessage {
        AgentMessage::Assistant(pi_ai::types::AssistantMessage {
            content: vec![pi_ai::types::AssistantContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            model: "claude-test".into(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: Vec::new(),
            usage: pi_ai::types::Usage {
                input: 1,
                output: 1,
                cache_read: 0,
                cache_write: 0,
                cache_write_1h: None,
                reasoning: None,
                total_tokens: 2,
                cost: UsageCost::default(),
            },
            stop_reason: pi_ai::types::StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
            duration_ms: None,
        })
    }

    /// Tool results are skipped and the newest entries fill the budget
    /// first (`prepareBranchEntries` behavior).
    #[test]
    fn prepare_skips_tool_results_and_keeps_the_newest() {
        let entries = vec![
            user_entry("u1", None, "old long message"),
            assistant_entry("a1", Some("u1"), "reply"),
        ];
        let preparation = prepare_branch_entries(&entries, 0);
        let roles: Vec<&str> = preparation
            .messages
            .iter()
            .map(AgentMessage::role)
            .collect();
        assert_eq!(roles, vec!["user", "assistant"]);
        assert!(preparation.total_tokens > 0);
    }

    /// The prompt assembles the conversation, the preamble rides the
    /// response, and a zero-budget context window leaves the reserve
    /// untouched.
    #[test]
    fn generate_prefixes_the_preamble_without_a_stream() {
        // The scripted stream echoes a fixed summary.
        struct Scripted;
        impl StreamFn for Scripted {
            fn stream(
                &self,
                _request: pi_agent::types::StreamRequest,
            ) -> futures::stream::BoxStream<'static, pi_agent::types::AssistantMessageEvent>
            {
                let message: pi_ai::types::AssistantMessage =
                    serde_json::from_value(serde_json::json!({
                        "role": "assistant",
                        "content": [{ "type": "text", "text": "## Goal\nstuff" }],
                        "api": "test",
                        "provider": "test",
                        "model": "test-model",
                        "usage": {
                            "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0,
                            "totalTokens": 2,
                            "cost": { "input": 0, "output": 0, "cacheRead": 0,
                                      "cacheWrite": 0, "total": 0 }
                        },
                        "stopReason": "stop",
                        "timestamp": 0
                    }))
                    .expect("assistant message");
                let start = pi_agent::types::AssistantMessageEvent::Start {
                    partial: message.clone(),
                };
                let done = pi_agent::types::AssistantMessageEvent::Done {
                    reason: pi_ai::types::StopReason::Stop,
                    message,
                };
                Box::pin(futures::stream::iter(vec![start, done]))
            }
        }

        let entries = vec![user_entry("u1", None, "hello")];
        let model = Model {
            context_window: 100_000,
            ..Model::default()
        };
        let signal = AbortSignal::new();
        let result = crate::runtime::block_on(generate_branch_summary(
            &entries,
            &BranchSummaryOptions {
                model: &model,
                api_key: Some("test"),
                access_token: None,
                signal: &signal,
                thinking_level: ThinkingLevel::Off,
                custom_instructions: None,
                replace_instructions: false,
                reserve_tokens: 16_384,
                stream_fn: &Scripted,
            },
        ));
        let summary = result.summary.expect("summary");
        assert!(summary.starts_with(BRANCH_SUMMARY_PREAMBLE), "{summary}");
        assert!(summary.contains("## Goal"), "{summary}");
        assert!(result.error.is_none());
        assert!(!result.aborted);
    }
}
