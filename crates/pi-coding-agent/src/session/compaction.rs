//! Context compaction (`core/compaction/compaction.ts` + `utils.ts`).
//!
//! Pure functions for compaction logic plus the summarization LLM call. The
//! session manager handles I/O ([`crate::session::store::SessionManager::
//! append_compaction`]); after compaction the session reloads through the
//! projection.
//!
//! Port deviations (documented in HANDOFF): the summarization call is a
//! single attempt (upstream wraps `retryAssistantCall` with the retry
//! settings), the `session_before_compact` gate emits cancel-only —
//! extension-provided compaction results are not plumbed yet — and
//! prompt-cache retention is left to the adapter (upstream forces
//! `cacheRetention: "none"` on the one-off summary).

use std::collections::BTreeSet;

use serde::Serialize;

use pi_agent::types::AssistantMessageEvent;
use pi_agent::{
    AgentMessage, StreamFn, StreamRequest, ThinkingLevel, default_convert_to_llm,
    types::AbortSignal,
};
use pi_ai::types::{
    AssistantContent, InputContent, Model, StopReason, StringOrTextBlocks, Usage, UserContent,
};

use super::records::{ContextReplacement, SessionEntry};
use super::store::{
    ProjectedSessionEntry, SessionProjection, build_session_projection_from,
    session_entry_to_context_messages,
};

// ============================================================================
// Settings
// ============================================================================

/// `CompactionSettings` (`compaction.ts:120-124`). `Serialize` serves the
/// `session_before_compact` gate payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
}

/// `DEFAULT_COMPACTION_SETTINGS` (`compaction.ts:126-130`).
pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    reserve_tokens: 16_384,
    keep_recent_tokens: 20_000,
};

// ============================================================================
// File operation tracking (`utils.ts:13-88`)
// ============================================================================

/// File operations extracted from tool calls; `CompactionDetails` carries the
/// final lists. `Serialize` serves the `session_before_compact` payload.
#[derive(Debug, Default, Clone, Serialize)]
pub struct FileOperations {
    read: BTreeSet<String>,
    written: BTreeSet<String>,
    edited: BTreeSet<String>,
}

impl FileOperations {
    fn add_tool_call(&mut self, tool_name: &str, path: Option<&str>) {
        let Some(path) = path else {
            return;
        };
        match tool_name {
            "read" => {
                self.read.insert(path.to_string());
            }
            "write" => {
                self.written.insert(path.to_string());
            }
            "edit" => {
                self.edited.insert(path.to_string());
            }
            _ => {}
        }
    }

    /// `extractFileOpsFromMessage` (`utils.ts:31-46`): tool calls from
    /// assistant messages, plus the nested calls recorded on tool results.
    pub fn extract_from_message(&mut self, message: &AgentMessage) {
        match message {
            AgentMessage::ToolResult(result) => {
                if let Some(nested) = &result.nested_calls {
                    for call in &nested.calls {
                        let path = call
                            .arguments
                            .as_ref()
                            .and_then(|arguments| arguments.get("path"))
                            .and_then(pi_ai::types::JsonValue::as_str);
                        self.add_tool_call(&call.name, path);
                    }
                }
            }
            AgentMessage::Assistant(assistant) => {
                for block in &assistant.content {
                    if let AssistantContent::ToolCall(call) = block {
                        let path = call
                            .arguments
                            .get("path")
                            .and_then(pi_ai::types::JsonValue::as_str);
                        self.add_tool_call(&call.name, path);
                    }
                }
            }
            _ => {}
        }
    }
    /// Seed recorded file lists from a nested branch summary's details
    /// (`branch-summarization.ts:215-229`): read files go into `read`;
    /// modified files into both `edited` and `written` for deduplication.
    pub(crate) fn seed_from_summary_details(
        &mut self,
        read_files: &[String],
        modified_files: &[String],
    ) {
        for file in read_files {
            self.read.insert(file.clone());
        }
        for file in modified_files {
            self.edited.insert(file.clone());
            self.written.insert(file.clone());
        }
    }
}

/// `computeFileLists` (`utils.ts:68-73`): files only read, and files
/// modified (edited or written), both sorted.
pub fn compute_file_lists(file_ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let modified: BTreeSet<String> = file_ops
        .edited
        .iter()
        .chain(&file_ops.written)
        .cloned()
        .collect();
    let read_files: Vec<String> = file_ops
        .read
        .iter()
        .filter(|file| !modified.contains(*file))
        .cloned()
        .collect();
    (read_files, modified.into_iter().collect())
}

/// `formatFileOperations` (`utils.ts:78-88`): XML tags appended to summaries.
pub fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", sections.join("\n\n"))
    }
}

fn extract_file_operations(
    messages: &[AgentMessage],
    entries: &[&SessionEntry],
    prev_compaction_index: Option<usize>,
) -> FileOperations {
    let mut file_ops = FileOperations::default();

    // Collect from the previous compaction's details (if pi-generated).
    if let Some(index) = prev_compaction_index
        && let SessionEntry::Compaction(prev) = entries[index]
        && !prev.from_hook.unwrap_or(false)
        && let Some(details) = &prev.details
    {
        if let Some(read_files) = details
            .get("readFiles")
            .and_then(pi_ai::types::JsonValue::as_array)
        {
            for file in read_files {
                if let Some(text) = file.as_str() {
                    file_ops.read.insert(text.to_string());
                }
            }
        }
        if let Some(modified) = details
            .get("modifiedFiles")
            .and_then(pi_ai::types::JsonValue::as_array)
        {
            for file in modified {
                if let Some(text) = file.as_str() {
                    file_ops.edited.insert(text.to_string());
                }
            }
        }
    }

    for message in messages {
        file_ops.extract_from_message(message);
    }

    file_ops
}

// ============================================================================
// Token calculation (`compaction.ts:133-270`)
// ============================================================================

/// `calculateContextTokens` (`compaction.ts:140-142`): the native total when
/// present, else the component sum.
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

const ESTIMATED_IMAGE_CHARS: u64 = 4800;

fn user_content_chars(content: &UserContent) -> u64 {
    match content {
        UserContent::Text(text) => text.chars().count() as u64,
        UserContent::Blocks(blocks) => input_blocks_chars(blocks),
    }
}

fn input_blocks_chars(blocks: &[InputContent]) -> u64 {
    blocks
        .iter()
        .map(|block| match block {
            InputContent::Text(text) => text.text.chars().count() as u64,
            InputContent::Image(_) => ESTIMATED_IMAGE_CHARS,
        })
        .sum()
}

fn system_content_chars(content: &StringOrTextBlocks) -> u64 {
    match content {
        StringOrTextBlocks::Text(text) => text.chars().count() as u64,
        StringOrTextBlocks::Blocks(blocks) => blocks
            .iter()
            .map(|block| block.text.chars().count() as u64)
            .sum(),
    }
}

/// `estimateTokens` (`compaction.ts:298-349`): the chars/4 heuristic.
pub fn estimate_tokens(message: &AgentMessage) -> u64 {
    let chars: u64 = match message {
        AgentMessage::System(system) => {
            let mut chars = system_content_chars(&system.content);
            if let Some(sections) = &system.sections {
                for section in sections.values().flatten() {
                    chars += section.chars().count() as u64;
                }
            }
            if let Some(tools) = &system.tools_added {
                chars += serde_json::to_string(tools).map_or(0, |text| text.chars().count() as u64);
            }
            chars
        }
        AgentMessage::User(user) => user_content_chars(&user.content),
        AgentMessage::Assistant(assistant) => assistant
            .content
            .iter()
            .map(|block| match block {
                AssistantContent::Text(text) => text.text.chars().count() as u64,
                AssistantContent::Thinking(thinking) => thinking.thinking.chars().count() as u64,
                AssistantContent::ToolCall(call) => {
                    call.name.chars().count() as u64
                        + serde_json::to_string(&call.arguments)
                            .map_or(0, |text| text.chars().count() as u64)
                }
            })
            .sum(),
        AgentMessage::ToolResult(result) => input_blocks_chars(&result.content),
        AgentMessage::BashExecution(bash) => {
            (bash.command.chars().count() + bash.output.chars().count()) as u64
        }
        AgentMessage::Custom(custom) => user_content_chars(&custom.content),
        AgentMessage::BranchSummary(summary) => summary.summary.chars().count() as u64,
        AgentMessage::CompactionSummary(summary) => summary.summary.chars().count() as u64,
        // Extension-declared roles ride as raw JSON; estimate from its size.
        AgentMessage::Unknown { raw, .. } => {
            serde_json::to_string(raw).map_or(0, |text| text.chars().count() as u64)
        }
    };
    chars.div_ceil(4)
}

/// `getAssistantUsage` (`compaction.ts:148-161`): usage from a valid
/// assistant message (skips aborted, error, and all-zero usage).
fn assistant_usage(message: &AgentMessage) -> Option<&Usage> {
    let AgentMessage::Assistant(assistant) = message else {
        return None;
    };
    if matches!(
        assistant.stop_reason,
        StopReason::Aborted | StopReason::Error
    ) {
        return None;
    }
    if calculate_context_tokens(&assistant.usage) == 0 {
        return None;
    }
    Some(&assistant.usage)
}

/// `getLastAssistantUsageInfo` (`compaction.ts:184-190`).
fn last_assistant_usage_info(messages: &[AgentMessage]) -> Option<(&Usage, usize)> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| assistant_usage(message).map(|usage| (usage, index)))
}

/// `ContextUsageEstimate` (`compaction.ts:177-182`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsageEstimate {
    pub tokens: u64,
    pub usage_tokens: u64,
    pub trailing_tokens: u64,
    pub last_usage_index: Option<usize>,
}

/// `estimateContextTokens` (`compaction.ts:196-224`): the last assistant
/// usage when available, trailing messages estimated with the heuristic.
pub fn estimate_context_tokens(messages: &[AgentMessage]) -> ContextUsageEstimate {
    let Some((usage, index)) = last_assistant_usage_info(messages) else {
        let estimated: u64 = messages.iter().map(estimate_tokens).sum();
        return ContextUsageEstimate {
            tokens: estimated,
            usage_tokens: 0,
            trailing_tokens: estimated,
            last_usage_index: None,
        };
    };
    let usage_tokens = calculate_context_tokens(usage);
    let trailing_tokens: u64 = messages[index + 1..].iter().map(estimate_tokens).sum();
    ContextUsageEstimate {
        tokens: usage_tokens + trailing_tokens,
        usage_tokens,
        trailing_tokens,
        last_usage_index: Some(index),
    }
}

/// `estimateProjectedContextTokens` (`compaction.ts:227-262`): distrust usage
/// captured before a later edit or compaction.
pub fn estimate_projected_context_tokens(
    projection: &SessionProjection,
    branch_entries: &[SessionEntry],
) -> ContextUsageEstimate {
    let estimate = estimate_context_tokens(&projection.messages);
    if let Some(last_usage_index) = estimate.last_usage_index {
        let mut projected_message_index = 0usize;
        let mut usage_entry_id: Option<&str> = None;
        for entry in &projection.entries {
            let next_message_index = projected_message_index + entry.messages.len();
            if last_usage_index < next_message_index {
                usage_entry_id = Some(entry.source_entry.id());
                break;
            }
            projected_message_index = next_message_index;
        }

        let usage_entry_index =
            usage_entry_id.and_then(|id| branch_entries.iter().position(|entry| entry.id() == id));
        let latest_invalidating_entry_index = branch_entries.iter().rposition(|entry| {
            matches!(
                entry,
                SessionEntry::ContextEdit(_) | SessionEntry::Compaction(_)
            )
        });
        if let Some(usage_entry_index) = usage_entry_index
            && latest_invalidating_entry_index.is_none_or(|latest| usage_entry_index > latest)
        {
            return estimate;
        }
    }

    // The current system prompt is prompt state, not conversation.
    let mut tokens = pi_agent::current_system_message(&projection.messages).map_or(0, |system| {
        estimate_tokens(&AgentMessage::System(system.clone()))
    });
    for message in &projection.messages {
        if !matches!(message, AgentMessage::System(_)) {
            tokens += estimate_tokens(message);
        }
    }
    ContextUsageEstimate {
        tokens,
        usage_tokens: 0,
        trailing_tokens: tokens,
        last_usage_index: None,
    }
}

/// `shouldCompact` (`compaction.ts:267-270`).
pub fn should_compact(
    context_tokens: u64,
    context_window: u64,
    settings: &CompactionSettings,
) -> bool {
    settings.enabled && context_tokens > context_window.saturating_sub(settings.reserve_tokens)
}

// ============================================================================
// Cut point detection (`compaction.ts:272-501`, projected variant)
// ============================================================================

/// `isCutPointMessage` (`compaction.ts:351-364`): never cut at tool results
/// (they must follow their tool call) or system messages.
fn is_cut_point_message(message: &AgentMessage) -> bool {
    !matches!(
        message,
        AgentMessage::ToolResult(_) | AgentMessage::System(_)
    )
}

/// `isTurnStartMessage` (`compaction.ts:366-379`); extension-declared roles
/// count as their own turn starts, like `custom`.
fn is_turn_start_message(message: &AgentMessage) -> bool {
    matches!(
        message,
        AgentMessage::User(_)
            | AgentMessage::BashExecution(_)
            | AgentMessage::Custom(_)
            | AgentMessage::BranchSummary(_)
            | AgentMessage::CompactionSummary(_)
            | AgentMessage::Unknown { .. }
    )
}

fn is_projected_turn_start(entry: &ProjectedSessionEntry) -> bool {
    if matches!(entry.source_entry, SessionEntry::Compaction(_)) {
        return false;
    }
    entry.messages.iter().any(is_turn_start_message)
}

fn find_projected_turn_start_index(
    entries: &[ProjectedSessionEntry],
    entry_index: usize,
    start_index: usize,
) -> Option<usize> {
    (start_index..=entry_index)
        .rev()
        .find(|&index| is_projected_turn_start(&entries[index]))
}

/// `CutPointResult` (`compaction.ts:421-428`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutPointResult {
    pub first_kept_entry_index: usize,
    pub turn_start_index: Option<usize>,
    pub is_split_turn: bool,
}

/// `findProjectedCutPoint` (`compaction.ts:789-857`): walk backwards from the
/// newest entry accumulating estimated sizes; cut at the closest valid cut
/// point once the budget is exceeded.
pub fn find_projected_cut_point(
    entries: &[ProjectedSessionEntry],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: u64,
) -> CutPointResult {
    let cut_points: Vec<usize> = (start_index..end_index)
        .filter(|&index| {
            !matches!(entries[index].source_entry, SessionEntry::Compaction(_))
                && entries[index].messages.iter().any(is_cut_point_message)
        })
        .collect();
    if cut_points.is_empty() {
        return CutPointResult {
            first_kept_entry_index: start_index,
            turn_start_index: None,
            is_split_turn: false,
        };
    }

    let mut accumulated_tokens = 0u64;
    let mut exceeded_budget = false;
    let mut cut_index = cut_points[0];
    for index in (start_index..end_index).rev() {
        let message_tokens: u64 = entries[index].messages.iter().map(estimate_tokens).sum();
        if message_tokens == 0 {
            continue;
        }
        accumulated_tokens += message_tokens;
        if accumulated_tokens >= keep_recent_tokens {
            exceeded_budget = true;
            cut_index = cut_points
                .iter()
                .find(|candidate| **candidate >= index)
                .copied()
                .unwrap_or(*cut_points.last().expect("non-empty"));
            break;
        }
    }

    // A recovery attempt and its omission edits are context-invisible after
    // the last visible input; advance only for a closed suffix containing an
    // omitted assistant attempt (`compaction.ts:819-843`).
    if end_index > cut_index + 1 {
        let suffix = &entries[cut_index + 1..end_index];
        let is_intrinsically_visible = |entry: &ProjectedSessionEntry| -> bool {
            !matches!(entry.source_entry, SessionEntry::ContextEdit(_))
                && !session_entry_to_context_messages(&entry.source_entry).is_empty()
        };
        let is_omitted = |entry: &ProjectedSessionEntry| -> bool {
            is_intrinsically_visible(entry) && entry.messages.is_empty()
        };
        let omitted_suffix_ids: BTreeSet<&str> = suffix
            .iter()
            .filter(|entry| is_omitted(entry))
            .map(|entry| entry.source_entry.id())
            .collect();
        let has_external_replacement = suffix.iter().any(|entry| {
            let SessionEntry::ContextEdit(edit) = &entry.source_entry else {
                return false;
            };
            matches!(edit.replacement, ContextReplacement::Replace { .. })
                && !omitted_suffix_ids.contains(edit.target_id.as_str())
        });
        let is_recovery_omission_suffix = exceeded_budget
            && !has_external_replacement
            && suffix.iter().any(|entry| {
                matches!(
                    &entry.source_entry,
                    SessionEntry::Message(message)
                        if matches!(message.message, AgentMessage::Assistant(_))
                ) && is_omitted(entry)
            })
            && suffix.iter().all(|entry| {
                !matches!(entry.source_entry, SessionEntry::Compaction(_))
                    && (!is_intrinsically_visible(entry) || is_omitted(entry))
            });
        if is_recovery_omission_suffix {
            cut_index += 1;
        }
    }

    // Include adjacent metadata entries that do not affect context.
    while cut_index > start_index {
        let previous = &entries[cut_index - 1];
        if matches!(previous.source_entry, SessionEntry::Compaction(_))
            || !previous.messages.is_empty()
        {
            break;
        }
        cut_index -= 1;
    }

    let starts_turn = is_projected_turn_start(&entries[cut_index]);
    let turn_start_index = if starts_turn {
        None
    } else {
        find_projected_turn_start_index(entries, cut_index, start_index)
    };
    CutPointResult {
        first_kept_entry_index: cut_index,
        turn_start_index,
        is_split_turn: !starts_turn && turn_start_index.is_some(),
    }
}

// ============================================================================
// Summarization (`compaction.ts:503-753`, `utils.ts:90-165`)
// ============================================================================

/// `SUMMARIZATION_PROMPT` (`compaction.ts:507-538`).
pub const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or \"(none)\" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or \"(none)\" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

/// `UPDATE_SUMMARIZATION_PROMPT` (`compaction.ts:540-579`).
pub const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed
- UPDATE \"Next Steps\" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

/// `TURN_PREFIX_SUMMARIZATION_PROMPT` (`compaction.ts:929-942`).
pub const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "The messages above are earlier context from an ongoing conversation. Later messages are stored separately and do not need to be reconstructed.

Create a concise checkpoint of the user's request and the progress shown above. This checkpoint will be placed before the later messages so the conversation can continue with the necessary context.

## Original Request
[What did the user ask for?]

## Progress So Far
- [Key decisions and work completed in these messages]

## Context Needed to Continue
- [Information from these messages needed to understand the later work]

Only summarize information explicitly present above. Do not infer or recreate later messages.";

/// `SUMMARIZATION_SYSTEM_PROMPT` (`utils.ts:162-164`).
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

/// Maximum characters for a tool result in serialized summaries
/// (`utils.ts:95`).
const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// `truncateForSummary` (`utils.ts:101-105`).
fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated_chars = text.chars().count() - max_chars;
    format!(
        "{}\n\n[... {} more characters truncated]",
        crate::tools::truncate::slice_head(text, max_chars),
        truncated_chars
    )
}

fn input_content_text(blocks: &[InputContent]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            InputContent::Text(text) => Some(text.text.clone()),
            InputContent::Image(_) => None,
        })
        .collect::<String>()
}

/// `serializeConversation` (`utils.ts:121-156`): render messages as text so
/// the model summarizes instead of continuing the conversation. Thinking is
/// omitted; tool results are truncated.
pub fn serialize_conversation(messages: &[AgentMessage]) -> String {
    let mut parts: Vec<String> = Vec::new();

    for message in messages {
        match message {
            AgentMessage::User(user) => {
                let content = match &user.content {
                    UserContent::Text(text) => text.clone(),
                    UserContent::Blocks(blocks) => input_content_text(blocks),
                };
                if !content.is_empty() {
                    parts.push(format!("[User]: {content}"));
                }
            }
            AgentMessage::Assistant(assistant) => {
                let mut tool_calls = Vec::new();
                for block in &assistant.content {
                    if let AssistantContent::ToolCall(call) = block {
                        let args = call
                            .arguments
                            .iter()
                            .map(|(key, value)| {
                                format!(
                                    "{key}={}",
                                    serde_json::to_string(value).unwrap_or_default()
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        tool_calls.push(format!("{}({args})", call.name));
                    }
                }
                if assistant
                    .content
                    .iter()
                    .any(|block| matches!(block, AssistantContent::Text(_)))
                {
                    let text = assistant
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            AssistantContent::Text(text) => Some(text.text.clone()),
                            _ => None,
                        })
                        .collect::<String>();
                    parts.push(format!("[Assistant]: {text}"));
                }
                if !tool_calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
                }
            }
            AgentMessage::ToolResult(result) => {
                let content = input_content_text(&result.content);
                if !content.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&content, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
            _ => {}
        }
    }

    parts.join("\n\n")
}

// ============================================================================
// Compaction preparation + result (`compaction.ts:104-114`, `:759-923`)
// ============================================================================

/// `CompactionResult` (`compaction.ts:105-114`).
#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    pub estimated_tokens_after: Option<u64>,
    pub usage: Option<Usage>,
    /// `CompactionDetails`.
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// `CompactionPreparation` (`compaction.ts:759-775`). `Serialize` serves
/// the `session_before_compact` gate payload (`agent-session.ts:2964-2977`).
#[derive(Debug, Clone, Serialize)]
pub struct CompactionPreparation {
    pub first_kept_entry_id: String,
    pub messages_to_summarize: Vec<AgentMessage>,
    pub turn_prefix_messages: Vec<AgentMessage>,
    pub is_split_turn: bool,
    pub tokens_before: u64,
    pub previous_summary: Option<String>,
    pub file_ops: FileOperations,
    pub settings: CompactionSettings,
}

/// `getMessagesFromProjectedEntryForCompaction` (`compaction.ts:98-102`):
/// system messages are prompt state, not conversation.
fn compaction_messages(entry: &ProjectedSessionEntry) -> Vec<AgentMessage> {
    if matches!(entry.source_entry, SessionEntry::Compaction(_)) {
        return Vec::new();
    }
    entry
        .messages
        .iter()
        .filter(|message| !matches!(message, AgentMessage::System(_)))
        .cloned()
        .collect()
}

/// `prepareCompaction` (`compaction.ts:859-923`).
pub fn prepare_compaction(
    path_entries: &[SessionEntry],
    settings: CompactionSettings,
) -> Option<CompactionPreparation> {
    if matches!(path_entries.last(), Some(SessionEntry::Compaction(_))) {
        return None;
    }

    let projection = build_session_projection_from(path_entries, None);
    let projected_entries = &projection.entries;
    let source_entries: Vec<&SessionEntry> = projected_entries
        .iter()
        .map(|entry| &entry.source_entry)
        .collect();
    // The canonical projection keeps only the newest compaction's summary
    // contribution; older compactions inside its retained range project as
    // empty.
    let prev_compaction_index = projected_entries.iter().position(|entry| {
        matches!(entry.source_entry, SessionEntry::Compaction(_)) && !entry.messages.is_empty()
    });

    let mut previous_summary = None;
    let mut boundary_start = 0usize;
    if let Some(index) = prev_compaction_index
        && let SessionEntry::Compaction(prev) = &projected_entries[index].source_entry
    {
        previous_summary = Some(prev.summary.clone());
        boundary_start = index + 1;
    }
    let boundary_end = projected_entries.len();
    let tokens_before = estimate_projected_context_tokens(&projection, path_entries).tokens;
    let cut_point = find_projected_cut_point(
        projected_entries,
        boundary_start,
        boundary_end,
        settings.keep_recent_tokens,
    );

    let first_kept_entry = projected_entries
        .get(cut_point.first_kept_entry_index)
        .map(|entry| &entry.source_entry)?;
    let first_kept_entry_id = first_kept_entry.id().to_string();
    let history_end = if cut_point.is_split_turn {
        cut_point.turn_start_index?
    } else {
        cut_point.first_kept_entry_index
    };

    let messages_to_summarize: Vec<AgentMessage> = projected_entries[boundary_start..history_end]
        .iter()
        .flat_map(compaction_messages)
        .collect();
    let turn_prefix_messages: Vec<AgentMessage> = if cut_point.is_split_turn {
        projected_entries[cut_point.turn_start_index?..cut_point.first_kept_entry_index]
            .iter()
            .flat_map(compaction_messages)
            .collect()
    } else {
        Vec::new()
    };

    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return None;
    }

    let mut file_ops = extract_file_operations(
        &messages_to_summarize,
        &source_entries,
        prev_compaction_index,
    );
    if cut_point.is_split_turn {
        for message in &turn_prefix_messages {
            file_ops.extract_from_message(message);
        }
    }

    Some(CompactionPreparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn: cut_point.is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings,
    })
}

// ============================================================================
// The summarization call (`compaction.ts:581-753`, `:952-1104`)
// ============================================================================

/// Compaction failures, with pi's user-facing messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionError {
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for CompactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompactionError::Cancelled => write!(f, "Compaction cancelled"),
            CompactionError::Failed(message) => write!(f, "{message}"),
        }
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
}

/// One summarization request over the injected stream seam: a single
/// `completeSimple`-shaped call collecting the final assistant message.
pub(crate) async fn complete_summarization(
    model: &Model,
    prompt_text: &str,
    api_key: Option<&str>,
    access_token: Option<&str>,
    thinking_level: ThinkingLevel,
    stream_fn: &dyn StreamFn,
    signal: &AbortSignal,
) -> Result<pi_ai::types::AssistantMessage, CompactionError> {
    let system = pi_ai::types::SystemMessage {
        content: StringOrTextBlocks::Text(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        sections: None,
        tools_added: None,
        tools_removed: None,
        timestamp: now_millis(),
    };
    let user = pi_agent::transcript::user_message(prompt_text, now_millis());
    let request = StreamRequest {
        model: model.clone(),
        messages: vec![AgentMessage::System(system), user],
        thinking_level,
        session_id: None,
        api_key: api_key.map(str::to_string),
        access_token: access_token.map(str::to_string),
        signal: None,
    };
    let mut stream = stream_fn.stream(request);
    let mut partial: Option<pi_ai::types::AssistantMessage> = None;
    while let Some(event) = futures::StreamExt::next(&mut stream).await {
        if signal.aborted() {
            return Err(CompactionError::Cancelled);
        }
        match event {
            AssistantMessageEvent::Done { message, .. } => return Ok(message),
            AssistantMessageEvent::Error { reason, error } => {
                if reason == StopReason::Aborted {
                    return Err(CompactionError::Cancelled);
                }
                // An errored stream yields the error-shaped assistant
                // message; `summarization_failure` classifies it, exactly
                // like upstream's `completeSimple().result()` path.
                return Ok(error);
            }
            event => partial = Some(event_partial(&event)),
        }
    }
    if signal.aborted() {
        return Err(CompactionError::Cancelled);
    }
    partial.map_or_else(
        || {
            Err(CompactionError::Failed(
                "Summarization failed: no response".to_string(),
            ))
        },
        Ok,
    )
}

fn event_partial(event: &AssistantMessageEvent) -> pi_ai::types::AssistantMessage {
    match event {
        AssistantMessageEvent::Start { partial }
        | AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolcallStart { partial, .. }
        | AssistantMessageEvent::ToolcallDelta { partial, .. }
        | AssistantMessageEvent::ToolcallEnd { partial, .. } => partial.clone(),
        AssistantMessageEvent::Done { message, .. }
        | AssistantMessageEvent::Error { error: message, .. } => message.clone(),
    }
}

/// `getSummarizationFailure` (`compaction.ts:585-593`): a length stop
/// contains partial text and must not become a session checkpoint.
pub(crate) fn summarization_failure(
    response: &pi_ai::types::AssistantMessage,
    label: &str,
) -> Option<String> {
    match response.stop_reason {
        StopReason::Error => Some(format!(
            "{label} failed: {}",
            response
                .error_message
                .clone()
                .unwrap_or_else(|| "Unknown error".to_string())
        )),
        StopReason::Length => Some(format!(
            "{label} failed: generation hit the token cap and the summary is incomplete"
        )),
        _ => None,
    }
}

/// The summarization thinking level: only when the model supports reasoning
/// (`createSummarizationOptions`, `compaction.ts:595-609`).
pub(crate) fn summarization_thinking_level(
    model: &Model,
    thinking_level: ThinkingLevel,
) -> ThinkingLevel {
    if model.reasoning && thinking_level != ThinkingLevel::Off {
        thinking_level
    } else {
        ThinkingLevel::Off
    }
}

/// `contentText` (`pi-ai`): visible text of assistant content blocks.
pub(crate) fn content_text(content: &[AssistantContent]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<String>()
}

/// `combineUsage` (`usage-totals.ts`): component-wise sum.
fn combine_usage(mut a: Usage, b: Usage) -> Usage {
    a.input += b.input;
    a.output += b.output;
    a.cache_read += b.cache_read;
    a.cache_write += b.cache_write;
    a.total_tokens += b.total_tokens;
    a.cost.total += b.cost.total;
    a
}

/// `generateSummaryWithUsage` (`compaction.ts:696-753`).
#[allow(clippy::too_many_arguments)]
async fn generate_summary_with_usage(
    current_messages: &[AgentMessage],
    model: &Model,
    api_key: Option<&str>,
    access_token: Option<&str>,
    signal: &AbortSignal,
    custom_instructions: Option<&str>,
    previous_summary: Option<&str>,
    thinking_level: ThinkingLevel,
    stream_fn: &dyn StreamFn,
) -> Result<(String, Usage), CompactionError> {
    let base_prompt = if previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT
    } else {
        SUMMARIZATION_PROMPT
    };
    let mut base_prompt = base_prompt.to_string();
    if let Some(custom) = custom_instructions {
        base_prompt = format!("{base_prompt}\n\nAdditional focus: {custom}");
    }

    // Serialize the conversation to text so the model doesn't try to
    // continue it; `convertToLlm` first (handles bashExecution/custom).
    let llm_messages = default_convert_to_llm(current_messages.to_vec());
    let conversation_text = serialize_conversation(&llm_messages);

    let mut prompt_text = format!("<conversation>\n{conversation_text}\n</conversation>\n\n");
    if let Some(previous) = previous_summary {
        prompt_text.push_str("<previous-summary>\n");
        prompt_text.push_str(previous);
        prompt_text.push_str("\n</previous-summary>\n\n");
    }
    prompt_text.push_str(&base_prompt);

    let response = complete_summarization(
        model,
        &prompt_text,
        api_key,
        access_token,
        summarization_thinking_level(model, thinking_level),
        stream_fn,
        signal,
    )
    .await?;

    if let Some(failure) = summarization_failure(&response, "Summarization") {
        return Err(CompactionError::Failed(failure));
    }
    if response
        .content
        .iter()
        .any(|block| matches!(block, AssistantContent::ToolCall(_)))
    {
        return Err(CompactionError::Failed(
            "Summarization attempted to call a tool".to_string(),
        ));
    }
    Ok((content_text(&response.content), response.usage))
}

/// `generateTurnPrefixSummary` (`compaction.ts:1062-1104`).
#[allow(clippy::too_many_arguments)]
async fn generate_turn_prefix_summary(
    messages: &[AgentMessage],
    model: &Model,
    api_key: Option<&str>,
    access_token: Option<&str>,
    signal: &AbortSignal,
    thinking_level: ThinkingLevel,
    stream_fn: &dyn StreamFn,
    custom_instructions: Option<&str>,
) -> Result<(String, Usage), CompactionError> {
    let llm_messages = default_convert_to_llm(messages.to_vec());
    let conversation_text = serialize_conversation(&llm_messages);
    let mut prompt_text = format!(
        "# Conversation\n{conversation_text}\n\n# Instructions\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
    );
    if let Some(custom) = custom_instructions {
        prompt_text = format!("{prompt_text}\n\nAdditional focus: {custom}");
    }

    let response = complete_summarization(
        model,
        &prompt_text,
        api_key,
        access_token,
        summarization_thinking_level(model, thinking_level),
        stream_fn,
        signal,
    )
    .await?;
    if let Some(failure) = summarization_failure(&response, "Turn prefix summarization") {
        return Err(CompactionError::Failed(failure));
    }
    if response
        .content
        .iter()
        .any(|block| matches!(block, AssistantContent::ToolCall(_)))
    {
        return Err(CompactionError::Failed(
            "Turn prefix summarization attempted to call a tool".to_string(),
        ));
    }
    Ok((content_text(&response.content), response.usage))
}

/// `compact` (`compaction.ts:952-1057`): generate summaries from prepared
/// data. The session manager adds ids/parents when saving.
#[allow(clippy::too_many_arguments)]
pub async fn compact(
    preparation: &CompactionPreparation,
    model: &Model,
    api_key: Option<&str>,
    access_token: Option<&str>,
    custom_instructions: Option<&str>,
    signal: &AbortSignal,
    thinking_level: ThinkingLevel,
    stream_fn: &dyn StreamFn,
) -> Result<CompactionResult, CompactionError> {
    let summary_usage: Usage;
    let summary: String;

    if preparation.is_split_turn && !preparation.turn_prefix_messages.is_empty() {
        let mut history_text = preparation
            .previous_summary
            .clone()
            .unwrap_or_else(|| "No prior history.".to_string());
        let mut history_usage: Option<Usage> = None;
        if !preparation.messages_to_summarize.is_empty() {
            let (text, usage) = generate_summary_with_usage(
                &preparation.messages_to_summarize,
                model,
                api_key,
                access_token,
                signal,
                custom_instructions,
                preparation.previous_summary.as_deref(),
                thinking_level,
                stream_fn,
            )
            .await?;
            history_text = text;
            history_usage = Some(usage);
        }
        let (turn_prefix_text, turn_prefix_usage) = generate_turn_prefix_summary(
            &preparation.turn_prefix_messages,
            model,
            api_key,
            access_token,
            signal,
            thinking_level,
            stream_fn,
            custom_instructions,
        )
        .await?;
        summary = format!(
            "{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{turn_prefix_text}"
        );
        summary_usage = history_usage.map_or(turn_prefix_usage, |history| {
            combine_usage(history, turn_prefix_usage)
        });
    } else {
        let (text, usage) = generate_summary_with_usage(
            &preparation.messages_to_summarize,
            model,
            api_key,
            access_token,
            signal,
            custom_instructions,
            preparation.previous_summary.as_deref(),
            thinking_level,
            stream_fn,
        )
        .await?;
        summary = text;
        summary_usage = usage;
    }

    // Compute file lists and append to the summary.
    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    let summary = format!(
        "{summary}{}",
        format_file_operations(&read_files, &modified_files)
    );

    if preparation.first_kept_entry_id.is_empty() {
        return Err(CompactionError::Failed(
            "First kept entry has no UUID - session may need migration".to_string(),
        ));
    }

    Ok(CompactionResult {
        summary,
        first_kept_entry_id: preparation.first_kept_entry_id.clone(),
        tokens_before: preparation.tokens_before,
        estimated_tokens_after: None,
        usage: Some(summary_usage),
        read_files,
        modified_files,
    })
}

/// The `details` JSON stored on the compaction entry (`CompactionDetails`).
pub fn details_json(result: &CompactionResult) -> serde_json::Value {
    serde_json::json!({
        "readFiles": result.read_files,
        "modifiedFiles": result.modified_files,
    })
}

#[cfg(test)]
#[cfg_attr(test, allow(clippy::assert_is_empty))]
#[allow(clippy::too_many_lines)]
mod tests {
    use super::*;
    use crate::session::records::{EntryBase, SessionMessageEntry};
    use pi_ai::types::{
        AssistantMessage, JsonObject, TextContent, ToolCall, UserContent as TestUserContent,
        UserMessage,
    };

    fn user_message(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: TestUserContent::Text(text.to_string()),
            timestamp: 0,
        })
    }

    fn entry_with_user(id: &str, parent: Option<&str>, text: &str) -> SessionEntry {
        SessionEntry::Message(SessionMessageEntry {
            base: EntryBase {
                id: id.to_string(),
                parent_id: parent.map(str::to_string),
                timestamp: String::new(),
            },
            message: user_message(text),
            unknown: JsonObject::new(),
        })
    }

    fn assistant_with(content: Vec<AssistantContent>) -> AgentMessage {
        AgentMessage::Assistant(
            serde_json::from_value(serde_json::json!({
                "content": [],
                "api": "anthropic-messages", "provider": "anthropic", "model": "m",
                "timestamp": 0,
                "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                          "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}},
                "stopReason": "stop"
            }))
            .map(|mut assistant: AssistantMessage| {
                assistant.content = content;
                assistant
            })
            .map_err(|error| error.to_string())
            .expect("assistant"),
        )
    }

    #[test]
    fn settings_defaults_match_upstream() {
        let settings = DEFAULT_COMPACTION_SETTINGS;
        assert_eq!(settings.reserve_tokens, 16_384);
        assert_eq!(settings.keep_recent_tokens, 20_000);
        assert!(settings.enabled);
    }

    #[test]
    fn estimate_tokens_uses_the_chars_over_four_heuristic() {
        assert_eq!(estimate_tokens(&user_message("12345678")), 2);
    }

    #[test]
    fn file_lists_exclude_modified_from_read() {
        let mut file_ops = FileOperations::default();
        file_ops.add_tool_call("read", Some("/a"));
        file_ops.add_tool_call("write", Some("/b"));
        file_ops.add_tool_call("edit", Some("/c"));
        file_ops.add_tool_call("grep", Some("/d"));
        let (read, modified) = compute_file_lists(&file_ops);
        assert_eq!(read, vec!["/a".to_string()]);
        assert_eq!(modified, vec!["/b".to_string(), "/c".to_string()]);
    }

    #[test]
    fn file_operation_format_matches_the_xml_shape() {
        assert_eq!(format_file_operations(&[], &[]), "");
        assert_eq!(
            format_file_operations(&["/a".to_string()], &["/b".to_string()]),
            "\n\n<read-files>\n/a\n</read-files>\n\n<modified-files>\n/b\n</modified-files>"
        );
    }

    #[test]
    fn serialization_truncates_long_tool_results() {
        let long = "x".repeat(TOOL_RESULT_MAX_CHARS + 100);
        let result = AgentMessage::ToolResult(pi_ai::types::ToolResultMessage {
            tool_call_id: "call".into(),
            tool_name: "read".into(),
            content: vec![InputContent::Text(TextContent {
                text: long,
                text_signature: None,
            })],
            details: None,
            usage: None,
            nested_calls: None,
            is_error: false,
            timestamp: 0,
            duration_ms: None,
        });
        let text = serialize_conversation(&[result]);
        assert!(
            text.contains("[... 100 more characters truncated]"),
            "{text}"
        );
    }

    #[test]
    fn serialization_renders_users_assistants_and_tool_calls() {
        let assistant = assistant_with(vec![
            AssistantContent::Text(TextContent {
                text: "hi".into(),
                text_signature: None,
            }),
            AssistantContent::ToolCall(ToolCall {
                id: "t1".into(),
                name: "read".into(),
                arguments: serde_json::from_value(serde_json::json!({"path": "/x"}))
                    .expect("arguments"),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            }),
        ]);
        let text = serialize_conversation(&[user_message("q"), assistant]);
        assert!(text.contains("[User]: q"), "{text}");
        assert!(text.contains("[Assistant]: hi"), "{text}");
        assert!(
            text.contains("[Assistant tool calls]: read(path=\"/x\")"),
            "{text}"
        );
    }

    #[test]
    fn summarization_level_off_for_non_reasoning_models() {
        let model: Model = serde_json::from_value(serde_json::json!({
            "id": "m", "name": "M", "api": "anthropic-messages", "provider": "anthropic",
            "baseUrl": "https://example.invalid", "input": ["text"],
            "reasoning": false, "contextWindow": 200_000, "maxTokens": 8_192
        }))
        .expect("model");
        assert_eq!(
            summarization_thinking_level(&model, ThinkingLevel::High),
            ThinkingLevel::Off
        );
        let mut reasoning = model.clone();
        reasoning.reasoning = true;
        assert_eq!(
            summarization_thinking_level(&reasoning, ThinkingLevel::High),
            ThinkingLevel::High
        );
    }

    #[test]
    fn prepare_rejects_an_empty_session() {
        assert!(prepare_compaction(&[], DEFAULT_COMPACTION_SETTINGS).is_none());
    }

    #[test]
    fn prepare_compacts_history_and_keeps_the_recent_tail() {
        let entries = vec![
            entry_with_user("e1", None, "hello"),
            entry_with_user("e2", Some("e1"), "middle"),
            entry_with_user("e3", Some("e2"), "recent"),
        ];
        // Budget keeps only the last entry's tokens: history = e1..e2.
        let preparation = prepare_compaction(
            &entries,
            CompactionSettings {
                keep_recent_tokens: estimate_tokens(&user_message("recent")),
                ..DEFAULT_COMPACTION_SETTINGS
            },
        )
        .expect("preparation");
        assert_eq!(preparation.first_kept_entry_id, "e3");
        assert!(!preparation.is_split_turn);
        assert_eq!(
            preparation
                .messages_to_summarize
                .iter()
                .filter(|message| matches!(message, AgentMessage::User(_)))
                .count(),
            2
        );
        assert!(preparation.turn_prefix_messages.is_empty());
    }

    #[test]
    fn should_compact_respects_the_reserve_budget() {
        let settings = DEFAULT_COMPACTION_SETTINGS;
        assert!(!should_compact(1000, 200_000, &settings));
        assert!(should_compact(190_000, 200_000, &settings));
        assert!(!should_compact(
            190_000,
            200_000,
            &CompactionSettings {
                enabled: false,
                ..settings
            }
        ));
    }
}
