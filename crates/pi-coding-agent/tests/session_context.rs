//! Port of `test/session-manager/build-context.test.ts`: the compaction-aware
//! context building over hand-built entry trees.
//!
//! Fixtures are synthetic, mirroring the TS helpers (`msg`, `compaction`,
//! `branchSummary`, `custom`, `thinkingLevel`, `modelChange`).

use pi_agent::AgentMessage;
use pi_ai::types::{
    AssistantContent, AssistantMessage, StopReason, TextContent, Usage, UsageCost, UserContent,
    UserMessage,
};
use pi_coding_agent::session::{
    ModelRef, SessionEntry, build_session_context_from, get_latest_compaction_entry,
};

const TS: &str = "2025-01-01T00:00:00Z";

fn usage() -> Usage {
    Usage {
        input: 1,
        output: 1,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 2,
        cost: UsageCost::default(),
    }
}

fn msg(id: &str, parent: Option<&str>, role: &str, text: &str) -> SessionEntry {
    let base = pi_coding_agent::session::EntryBase {
        id: id.to_string(),
        parent_id: parent.map(str::to_string),
        timestamp: TS.to_string(),
    };
    let message = if role == "user" {
        AgentMessage::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 1,
        })
    } else {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
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
            usage: usage(),
            stop_reason: StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
            duration_ms: None,
        })
    };
    SessionEntry::Message(pi_coding_agent::session::SessionMessageEntry {
        base,
        message,
        unknown: serde_json::Map::new(),
    })
}

fn compaction(
    id: &str,
    parent: Option<&str>,
    summary: &str,
    first_kept_entry_id: &str,
) -> SessionEntry {
    SessionEntry::Compaction(pi_coding_agent::session::CompactionEntry {
        base: pi_coding_agent::session::EntryBase {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: TS.to_string(),
        },
        summary: summary.to_string(),
        first_kept_entry_id: first_kept_entry_id.to_string(),
        tokens_before: 1000.0,
        details: None,
        usage: None,
        from_hook: None,
        system_message: None,
        unknown: serde_json::Map::new(),
    })
}

fn branch_summary(id: &str, parent: Option<&str>, summary: &str, from_id: &str) -> SessionEntry {
    SessionEntry::BranchSummary(pi_coding_agent::session::BranchSummaryEntry {
        base: pi_coding_agent::session::EntryBase {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: TS.to_string(),
        },
        from_id: from_id.to_string(),
        summary: summary.to_string(),
        details: None,
        usage: None,
        from_hook: None,
        unknown: serde_json::Map::new(),
    })
}

fn custom(
    id: &str,
    parent: Option<&str>,
    custom_type: &str,
    data: serde_json::Value,
) -> SessionEntry {
    SessionEntry::Custom(pi_coding_agent::session::CustomEntry {
        base: pi_coding_agent::session::EntryBase {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: TS.to_string(),
        },
        custom_type: custom_type.to_string(),
        data: Some(data),
        unknown: serde_json::Map::new(),
    })
}

fn thinking_level(id: &str, parent: Option<&str>, level: &str) -> SessionEntry {
    SessionEntry::ThinkingLevelChange(pi_coding_agent::session::ThinkingLevelChangeEntry {
        base: pi_coding_agent::session::EntryBase {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: TS.to_string(),
        },
        thinking_level: level.to_string(),
        unknown: serde_json::Map::new(),
    })
}

fn model_change(id: &str, parent: Option<&str>, provider: &str, model_id: &str) -> SessionEntry {
    SessionEntry::ModelChange(pi_coding_agent::session::ModelChangeEntry {
        base: pi_coding_agent::session::EntryBase {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: TS.to_string(),
        },
        provider: provider.to_string(),
        model_id: model_id.to_string(),
        unknown: serde_json::Map::new(),
    })
}

fn roles(ctx: &pi_coding_agent::session::SessionContext) -> Vec<&str> {
    ctx.messages.iter().map(AgentMessage::role).collect()
}

fn text_of(message: &AgentMessage) -> String {
    match message {
        AgentMessage::User(user) => match &user.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(_) => String::new(),
        },
        AgentMessage::Assistant(assistant) => assistant
            .content
            .iter()
            .filter_map(|block| block.as_text())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

// -- trivial cases ----------------------------------------------------------

#[test]
fn empty_entries_returns_empty_context() {
    let ctx = build_session_context_from(&[], None);
    assert_eq!(ctx.messages.len(), 0);
    assert_eq!(ctx.thinking_level, "off");
    assert_eq!(ctx.model, None);
}

#[test]
fn single_user_message() {
    let entries = vec![msg("1", None, "user", "hello")];
    let ctx = build_session_context_from(&entries, None);
    assert_eq!(ctx.messages.len(), 1);
    assert_eq!(ctx.messages[0].role(), "user");
}

#[test]
fn simple_conversation() {
    let entries = vec![
        msg("1", None, "user", "hello"),
        msg("2", Some("1"), "assistant", "hi there"),
        msg("3", Some("2"), "user", "how are you"),
        msg("4", Some("3"), "assistant", "great"),
    ];
    let ctx = build_session_context_from(&entries, None);
    assert_eq!(ctx.messages.len(), 4);
    assert_eq!(roles(&ctx), vec!["user", "assistant", "user", "assistant"]);
}

#[test]
fn tracks_thinking_level_changes() {
    let entries = vec![
        msg("1", None, "user", "hello"),
        thinking_level("2", Some("1"), "high"),
        msg("3", Some("2"), "assistant", "thinking hard"),
    ];
    let ctx = build_session_context_from(&entries, None);
    assert_eq!(ctx.thinking_level, "high");
    assert_eq!(ctx.messages.len(), 2);
}

#[test]
fn tracks_model_from_assistant_message() {
    let entries = vec![
        msg("1", None, "user", "hello"),
        msg("2", Some("1"), "assistant", "hi"),
    ];
    let ctx = build_session_context_from(&entries, None);
    assert_eq!(
        ctx.model,
        Some(ModelRef {
            provider: "anthropic".to_string(),
            model_id: "claude-test".to_string()
        })
    );
}

#[test]
fn tracks_model_from_model_change_entry() {
    let entries = vec![
        msg("1", None, "user", "hello"),
        model_change("2", Some("1"), "openai", "gpt-4"),
        msg("3", Some("2"), "assistant", "hi"),
    ];
    let ctx = build_session_context_from(&entries, None);
    // Assistant message overwrites model change.
    assert_eq!(
        ctx.model,
        Some(ModelRef {
            provider: "anthropic".to_string(),
            model_id: "claude-test".to_string()
        })
    );
}

// -- with compaction --------------------------------------------------------

#[test]
fn includes_summary_before_kept_messages() {
    let entries = vec![
        msg("1", None, "user", "first"),
        msg("2", Some("1"), "assistant", "response1"),
        msg("3", Some("2"), "user", "second"),
        msg("4", Some("3"), "assistant", "response2"),
        compaction("5", Some("4"), "Summary of first two turns", "3"),
        msg("6", Some("5"), "user", "third"),
        msg("7", Some("6"), "assistant", "response3"),
    ];
    let ctx = build_session_context_from(&entries, None);

    // Summary + kept (3,4) + after (6,7) = 5 messages.
    assert_eq!(ctx.messages.len(), 5);
    let pi_agent::AgentMessage::CompactionSummary(summary) = &ctx.messages[0] else {
        panic!("expected compaction summary");
    };
    assert!(summary.summary.contains("Summary of first two turns"));
    assert_eq!(text_of(&ctx.messages[1]), "second");
    assert_eq!(text_of(&ctx.messages[2]), "response2");
    assert_eq!(text_of(&ctx.messages[3]), "third");
    assert_eq!(text_of(&ctx.messages[4]), "response3");
}

#[test]
fn handles_compaction_keeping_from_first_message() {
    let entries = vec![
        msg("1", None, "user", "first"),
        msg("2", Some("1"), "assistant", "response"),
        compaction("3", Some("2"), "Empty summary", "1"),
        msg("4", Some("3"), "user", "second"),
    ];
    let ctx = build_session_context_from(&entries, None);

    // Summary + all messages (1,2,4).
    assert_eq!(ctx.messages.len(), 4);
    let pi_agent::AgentMessage::CompactionSummary(summary) = &ctx.messages[0] else {
        panic!("expected compaction summary");
    };
    assert!(summary.summary.contains("Empty summary"));
}

#[test]
fn multiple_compactions_use_the_latest() {
    let entries = vec![
        msg("1", None, "user", "a"),
        msg("2", Some("1"), "assistant", "b"),
        compaction("3", Some("2"), "First summary", "1"),
        msg("4", Some("3"), "user", "c"),
        msg("5", Some("4"), "assistant", "d"),
        compaction("6", Some("5"), "Second summary", "4"),
        msg("7", Some("6"), "user", "e"),
    ];
    let ctx = build_session_context_from(&entries, None);

    // Second summary, keep from 4.
    assert_eq!(ctx.messages.len(), 4);
    let pi_agent::AgentMessage::CompactionSummary(summary) = &ctx.messages[0] else {
        panic!("expected compaction summary");
    };
    assert!(summary.summary.contains("Second summary"));
}

#[test]
fn context_entries_include_custom_entries_from_the_kept_range() {
    let entries = vec![
        msg("1", None, "user", "first"),
        custom(
            "2",
            Some("1"),
            "old-state",
            serde_json::json!({"hidden": true}),
        ),
        msg("3", Some("2"), "assistant", "response1"),
        custom(
            "4",
            Some("3"),
            "kept-card",
            serde_json::json!({"title": "Kept"}),
        ),
        msg("5", Some("4"), "user", "second"),
        compaction("6", Some("5"), "Summary", "4"),
        custom(
            "7",
            Some("6"),
            "after-card",
            serde_json::json!({"title": "After"}),
        ),
        msg("8", Some("7"), "assistant", "response2"),
    ];

    let context = pi_coding_agent::session::build_context_entries_from(&entries, None);
    let ids: Vec<&str> = context.iter().map(SessionEntry::id).collect();
    assert_eq!(ids, vec!["6", "4", "5", "7", "8"]);
    let ctx = build_session_context_from(&entries, None);
    assert_eq!(roles(&ctx), vec!["compactionSummary", "user", "assistant"]);
}

#[test]
fn keeps_settings_from_the_full_path_after_compaction() {
    let entries = vec![
        msg("1", None, "user", "first"),
        thinking_level("2", Some("1"), "high"),
        msg("3", Some("2"), "assistant", "response1"),
        msg("4", Some("3"), "user", "second"),
        compaction("5", Some("4"), "Summary", "4"),
    ];

    let ctx = build_session_context_from(&entries, None);
    assert_eq!(ctx.thinking_level, "high");
    assert_eq!(roles(&ctx), vec!["compactionSummary", "user"]);
}

// -- with branches ----------------------------------------------------------

#[test]
fn follows_path_to_specified_leaf() {
    // Tree:
    //   1 -> 2 -> 3 (branch A)
    //         \-> 4 (branch B)
    let entries = vec![
        msg("1", None, "user", "start"),
        msg("2", Some("1"), "assistant", "response"),
        msg("3", Some("2"), "user", "branch A"),
        msg("4", Some("2"), "user", "branch B"),
    ];

    let ctx_a = build_session_context_from(&entries, Some("3"));
    assert_eq!(ctx_a.messages.len(), 3);
    assert_eq!(text_of(&ctx_a.messages[2]), "branch A");

    let ctx_b = build_session_context_from(&entries, Some("4"));
    assert_eq!(ctx_b.messages.len(), 3);
    assert_eq!(text_of(&ctx_b.messages[2]), "branch B");
}

#[test]
fn includes_branch_summary_in_path() {
    let entries = vec![
        msg("1", None, "user", "start"),
        msg("2", Some("1"), "assistant", "response"),
        msg("3", Some("2"), "user", "abandoned path"),
        branch_summary("4", Some("2"), "Summary of abandoned work", "3"),
        msg("5", Some("4"), "user", "new direction"),
    ];
    let ctx = build_session_context_from(&entries, Some("5"));

    assert_eq!(ctx.messages.len(), 4);
    let pi_agent::AgentMessage::BranchSummary(summary) = &ctx.messages[2] else {
        panic!("expected branch summary");
    };
    assert!(summary.summary.contains("Summary of abandoned work"));
    assert_eq!(text_of(&ctx.messages[3]), "new direction");
}

#[test]
fn complex_tree_with_multiple_branches_and_compaction() {
    // Tree:
    //   1 -> 2 -> 3 -> 4 -> compaction(5) -> 6 -> 7 (main path)
    //              \-> 8 -> 9 (abandoned branch)
    //                    \-> branchSummary(10) -> 11 (resumed from 3)
    let entries = vec![
        msg("1", None, "user", "start"),
        msg("2", Some("1"), "assistant", "r1"),
        msg("3", Some("2"), "user", "q2"),
        msg("4", Some("3"), "assistant", "r2"),
        compaction("5", Some("4"), "Compacted history", "3"),
        msg("6", Some("5"), "user", "q3"),
        msg("7", Some("6"), "assistant", "r3"),
        // Abandoned branch from 3
        msg("8", Some("3"), "user", "wrong path"),
        msg("9", Some("8"), "assistant", "wrong response"),
        // Branch summary resuming from 3
        branch_summary("10", Some("3"), "Tried wrong approach", "9"),
        msg("11", Some("10"), "user", "better approach"),
    ];

    // Main path to 7: summary + kept(3,4) + after(6,7).
    let ctx_main = build_session_context_from(&entries, Some("7"));
    assert_eq!(ctx_main.messages.len(), 5);
    let pi_agent::AgentMessage::CompactionSummary(summary) = &ctx_main.messages[0] else {
        panic!("expected compaction summary");
    };
    assert!(summary.summary.contains("Compacted history"));
    assert_eq!(text_of(&ctx_main.messages[1]), "q2");
    assert_eq!(text_of(&ctx_main.messages[2]), "r2");
    assert_eq!(text_of(&ctx_main.messages[3]), "q3");
    assert_eq!(text_of(&ctx_main.messages[4]), "r3");

    // Branch path to 11: 1,2,3 + branch_summary + 11.
    let ctx_branch = build_session_context_from(&entries, Some("11"));
    assert_eq!(ctx_branch.messages.len(), 5);
    assert_eq!(text_of(&ctx_branch.messages[0]), "start");
    assert_eq!(text_of(&ctx_branch.messages[1]), "r1");
    assert_eq!(text_of(&ctx_branch.messages[2]), "q2");
    let pi_agent::AgentMessage::BranchSummary(summary) = &ctx_branch.messages[3] else {
        panic!("expected branch summary");
    };
    assert!(summary.summary.contains("Tried wrong approach"));
    assert_eq!(text_of(&ctx_branch.messages[4]), "better approach");
}

// -- edge cases -------------------------------------------------------------

#[test]
fn uses_last_entry_when_leaf_id_not_found() {
    let entries = vec![
        msg("1", None, "user", "hello"),
        msg("2", Some("1"), "assistant", "hi"),
    ];
    let ctx = build_session_context_from(&entries, Some("nonexistent"));
    assert_eq!(ctx.messages.len(), 2);
}

#[test]
fn handles_orphaned_entries_gracefully() {
    let entries = vec![
        msg("1", None, "user", "hello"),
        msg("2", Some("missing"), "assistant", "orphan"), // parent doesn't exist
    ];
    let ctx = build_session_context_from(&entries, Some("2"));
    // Only the orphan: the parent chain is broken.
    assert_eq!(ctx.messages.len(), 1);
}

// -- getLatestCompactionEntry ------------------------------------------------

#[test]
fn get_latest_compaction_entry_returns_the_last_one_or_none() {
    let entries = vec![
        msg("1", None, "user", "a"),
        compaction("2", Some("1"), "First", "1"),
        msg("3", Some("2"), "user", "b"),
        compaction("4", Some("3"), "Second", "3"),
        msg("5", Some("4"), "user", "c"),
    ];
    let latest = get_latest_compaction_entry(&entries);
    assert_eq!(latest.map(|entry| entry.base.id.as_str()), Some("4"));
    assert_eq!(latest.map(|entry| entry.summary.as_str()), Some("Second"));
    assert!(get_latest_compaction_entry(&[msg("1", None, "user", "a")]).is_none());
    assert!(get_latest_compaction_entry(&[]).is_none());
}
