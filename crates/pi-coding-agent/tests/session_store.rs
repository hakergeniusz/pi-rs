//! Ports of the session-manager test suites that exercise the manager itself:
//! `tree-traversal.test.ts`, `save-entry.test.ts`, `labels.test.ts`,
//! `file-operations.test.ts`, `custom-session-id.test.ts`, and
//! `load-entries.test.ts`. All fixtures are synthetic tempdir content.

use std::fs;
use std::path::Path;

use pi_agent::AgentMessage;
use pi_ai::types::{
    AssistantContent, AssistantMessage, InputContent, StopReason, TextContent, Usage, UsageCost,
    UserContent, UserMessage,
};
use pi_coding_agent::session::{
    NewSessionOptions, SessionEntry, SessionError, SessionManager, load_entries_from_file,
};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn usage(total_tokens: u64) -> Usage {
    Usage {
        input: 1,
        output: 1,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens,
        cost: UsageCost::default(),
    }
}

fn big_usage() -> Usage {
    Usage {
        input: 10,
        output: 20,
        cache_read: 30,
        cache_write: 40,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 100,
        cost: UsageCost {
            input: 0.1,
            output: 0.2,
            cache_read: 0.3,
            cache_write: 0.4,
            total: 1.0,
            total_from_provider: None,
        },
    }
}

fn user_msg(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        content: UserContent::Text(text.to_string()),
        timestamp: 1,
    })
}

fn user_msg_blocks(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        content: UserContent::Blocks(vec![InputContent::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })]),
        timestamp: 1,
    })
}

fn assistant_msg(text: &str) -> AgentMessage {
    AgentMessage::Assistant(AssistantMessage {
        content: vec![AssistantContent::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        model: "test".into(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        thinking_level: None,
        diagnostics: Vec::new(),
        usage: usage(2),
        stop_reason: StopReason::Stop,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 2,
        duration_ms: None,
    })
}

fn tool_result_msg() -> AgentMessage {
    AgentMessage::ToolResult(pi_ai::types::ToolResultMessage {
        tool_call_id: "call-1".into(),
        tool_name: "nested-model".into(),
        content: vec![InputContent::Text(TextContent {
            text: "result".into(),
            text_signature: None,
        })],
        details: None,
        usage: Some(big_usage()),
        nested_calls: None,
        is_error: false,
        timestamp: 3,
        duration_ms: None,
    })
}

fn tempdir(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("pi-rs-{tag}-"))
        .tempdir()
        .unwrap()
}

fn text_of(message: &AgentMessage) -> String {
    match message {
        AgentMessage::User(user) => match &user.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| block.as_text())
                .collect::<Vec<_>>()
                .join(" "),
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

/// `readSessionFileRoles` (`test/utilities.ts:159-167`): the message role for
/// message entries, else the entry type.
fn read_session_file_roles(file: &Path) -> Vec<String> {
    fs::read_to_string(file)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            value["message"]["role"]
                .as_str()
                .or(value["type"].as_str())
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// append operations (tree-traversal.test.ts)
// ---------------------------------------------------------------------------

#[test]
fn append_message_creates_the_parent_id_chain() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();

    let id1 = session
        .append_message(user_msg("first"))
        .unwrap()
        .id()
        .to_string();
    let id2 = session
        .append_message(assistant_msg("second"))
        .unwrap()
        .id()
        .to_string();
    let id3 = session
        .append_message(user_msg("third"))
        .unwrap()
        .id()
        .to_string();

    let entries = session.get_entries();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].id(), id1);
    assert_eq!(entries[0].parent_id(), None);
    assert_eq!(entries[0].kind(), "message");
    assert_eq!(entries[1].id(), id2);
    assert_eq!(entries[1].parent_id(), Some(id1.as_str()));
    assert_eq!(entries[2].id(), id3);
    assert_eq!(entries[2].parent_id(), Some(id2.as_str()));
}

#[test]
fn append_thinking_level_change_integrates_into_the_tree() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let msg_id = session
        .append_message(user_msg("hello"))
        .unwrap()
        .id()
        .to_string();
    let thinking_id = session
        .append_thinking_level_change("high")
        .unwrap()
        .id()
        .to_string();
    let _ = session
        .append_message(assistant_msg("response"))
        .unwrap()
        .id()
        .to_string();

    let entries = session.get_entries();
    assert_eq!(entries.len(), 3);
    let thinking = entries
        .iter()
        .find(|e| e.kind() == "thinking_level_change")
        .unwrap();
    assert_eq!(thinking.id(), thinking_id);
    assert_eq!(thinking.parent_id(), Some(msg_id.as_str()));
    assert_eq!(entries[2].parent_id(), Some(thinking_id.as_str()));
}

#[test]
fn append_model_change_integrates_into_the_tree() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let msg_id = session
        .append_message(user_msg("hello"))
        .unwrap()
        .id()
        .to_string();
    let model_id = session
        .append_model_change("openai", "gpt-4")
        .unwrap()
        .id()
        .to_string();
    let _ = session
        .append_message(assistant_msg("response"))
        .unwrap()
        .id()
        .to_string();

    let entries = session.get_entries();
    let SessionEntry::ModelChange(change) = session.get_entry(&model_id).unwrap() else {
        panic!("expected model change");
    };
    assert_eq!(change.provider, "openai");
    assert_eq!(change.model_id, "gpt-4");
    assert_eq!(
        entries
            .iter()
            .find(|e| e.kind() == "model_change")
            .unwrap()
            .parent_id(),
        Some(msg_id.as_str())
    );
    assert_eq!(entries[2].parent_id(), Some(model_id.as_str()));
}

#[test]
fn append_compaction_integrates_into_the_tree() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let id1 = session
        .append_message(user_msg("1"))
        .unwrap()
        .id()
        .to_string();
    let id2 = session
        .append_message(assistant_msg("2"))
        .unwrap()
        .id()
        .to_string();
    let compaction_id = session
        .append_compaction(
            "summary",
            Some(&id1),
            1000,
            None,
            Some(false),
            Some(big_usage()),
        )
        .unwrap()
        .id()
        .to_string();
    let _ = session
        .append_message(user_msg("3"))
        .unwrap()
        .id()
        .to_string();

    let entries = session.get_entries();
    let SessionEntry::Compaction(compaction) = session.get_entry(&compaction_id).unwrap() else {
        panic!("expected compaction");
    };
    assert_eq!(compaction.base.parent_id, Some(id2.clone()));
    assert_eq!(compaction.summary, "summary");
    assert_eq!(compaction.first_kept_entry_id, id1);
    assert_eq!(compaction.tokens_before, 1000.0);
    assert_eq!(compaction.usage.map(|u| u.total_tokens), Some(100));
    assert_eq!(entries[3].parent_id(), Some(compaction_id.as_str()));
}

#[test]
fn append_custom_entry_integrates_into_the_tree() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let msg_id = session
        .append_message(user_msg("hello"))
        .unwrap()
        .id()
        .to_string();
    let custom_id = session
        .append_custom_entry("my_data", Some(serde_json::json!({"key": "value"})))
        .unwrap();
    let _ = session
        .append_message(assistant_msg("response"))
        .unwrap()
        .id()
        .to_string();

    let entries = session.get_entries();
    assert_eq!(entries.len(), 3);
    let SessionEntry::Custom(custom) = session.get_entry(&custom_id).unwrap() else {
        panic!("expected custom entry");
    };
    assert_eq!(custom.base.parent_id, Some(msg_id.clone()));
    assert_eq!(custom.custom_type, "my_data");
    assert_eq!(custom.data, Some(serde_json::json!({"key": "value"})));
    assert_eq!(entries[2].parent_id(), Some(custom_id.as_str()));
}

#[test]
fn leaf_pointer_advances_after_each_append() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(session.get_leaf_id(), None);

    let id1 = session
        .append_message(user_msg("1"))
        .unwrap()
        .id()
        .to_string();
    assert_eq!(session.get_leaf_id(), Some(id1.as_str()));
    let id2 = session
        .append_message(assistant_msg("2"))
        .unwrap()
        .id()
        .to_string();
    assert_eq!(session.get_leaf_id(), Some(id2.as_str()));
    let id3 = session
        .append_thinking_level_change("high")
        .unwrap()
        .id()
        .to_string();
    assert_eq!(session.get_leaf_id(), Some(id3.as_str()));
}

// ---------------------------------------------------------------------------
// saveCustomEntry (save-entry.test.ts)
// ---------------------------------------------------------------------------

#[test]
fn custom_entries_participate_in_tree_traversal_but_not_in_context() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let first_msg_id = session
        .append_message(user_msg("hello"))
        .unwrap()
        .id()
        .to_string();
    let custom_id = session
        .append_custom_entry("my_data", Some(serde_json::json!({"foo": "bar"})))
        .unwrap();
    let second_msg_id = session
        .append_message(assistant_msg("hi"))
        .unwrap()
        .id()
        .to_string();

    let entries = session.get_entries();
    assert_eq!(entries.len(), 3);
    let SessionEntry::Custom(custom) = entries.iter().find(|e| e.kind() == "custom").unwrap()
    else {
        panic!("expected custom entry");
    };
    assert_eq!(custom.custom_type, "my_data");
    assert_eq!(custom.data, Some(serde_json::json!({"foo": "bar"})));
    assert_eq!(custom.base.id, custom_id);
    assert_eq!(custom.base.parent_id, Some(first_msg_id.clone()));

    let path = session.get_branch(None).unwrap();
    assert_eq!(path.len(), 3);
    assert_eq!(path[0].id(), first_msg_id);
    assert_eq!(path[1].id(), custom_id);
    assert_eq!(path[2].id(), second_msg_id);

    // Custom entries are skipped in messages.
    let ctx = session.build_session_context().unwrap();
    assert_eq!(ctx.messages.len(), 2);
}

// ---------------------------------------------------------------------------
// getBranch / getTree / branch / branchWithSummary (tree-traversal.test.ts)
// ---------------------------------------------------------------------------

#[test]
fn get_branch_returns_empty_for_an_empty_session() {
    let session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(session.get_branch(None).unwrap().len(), 0);
}

#[test]
fn get_branch_returns_the_root_to_leaf_path() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let id1 = session
        .append_message(user_msg("1"))
        .unwrap()
        .id()
        .to_string();
    let id2 = session
        .append_message(assistant_msg("2"))
        .unwrap()
        .id()
        .to_string();
    let id3 = session
        .append_thinking_level_change("high")
        .unwrap()
        .id()
        .to_string();
    let id4 = session
        .append_message(user_msg("3"))
        .unwrap()
        .id()
        .to_string();

    let path = session.get_branch(None).unwrap();
    assert_eq!(path.len(), 4);
    assert_eq!(
        path.iter().map(|e| e.id().to_string()).collect::<Vec<_>>(),
        vec![id1.clone(), id2.clone(), id3.clone(), id4.clone()]
    );

    // A path from a specified entry to the root.
    let path = session.get_branch(Some(&id2)).unwrap();
    assert_eq!(path.len(), 2);
    assert_eq!(
        path.iter().map(|e| e.id().to_string()).collect::<Vec<_>>(),
        vec![id1, id2]
    );
}

#[test]
fn get_tree_returns_a_single_root_for_a_linear_session() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let id1 = session
        .append_message(user_msg("1"))
        .unwrap()
        .id()
        .to_string();
    let id2 = session
        .append_message(assistant_msg("2"))
        .unwrap()
        .id()
        .to_string();
    let id3 = session
        .append_message(user_msg("3"))
        .unwrap()
        .id()
        .to_string();

    let tree = session.get_tree();
    assert_eq!(tree.len(), 1);
    assert_eq!(tree[0].entry.id(), id1);
    assert_eq!(tree[0].children.len(), 1);
    assert_eq!(tree[0].children[0].entry.id(), id2);
    assert_eq!(tree[0].children[0].children[0].entry.id(), id3);
    assert_eq!(tree[0].children[0].children[0].children.len(), 0);
}

#[test]
fn get_tree_returns_branches_after_branch() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let _id1 = session
        .append_message(user_msg("1"))
        .unwrap()
        .id()
        .to_string();
    let id2 = session
        .append_message(assistant_msg("2"))
        .unwrap()
        .id()
        .to_string();
    let id3 = session
        .append_message(user_msg("3"))
        .unwrap()
        .id()
        .to_string();

    // Branch from id2, add a new path: 2 -> 4.
    session.branch(&id2).unwrap();
    let id4 = session
        .append_message(user_msg("4-branch"))
        .unwrap()
        .id()
        .to_string();

    let tree = session.get_tree();
    assert_eq!(tree.len(), 1);
    let node2 = &tree[0].children[0];
    assert_eq!(node2.entry.id(), id2);
    assert_eq!(node2.children.len(), 2); // id3 and id4 are siblings
    let mut child_ids: Vec<String> = node2
        .children
        .iter()
        .map(|c| c.entry.id().to_string())
        .collect();
    child_ids.sort();
    let mut expected = vec![id3.clone(), id4.clone()];
    expected.sort();
    assert_eq!(child_ids, expected);
}

#[test]
fn get_tree_handles_multiple_branches_at_the_same_point() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let _ = session
        .append_message(user_msg("root"))
        .unwrap()
        .id()
        .to_string();
    let id2 = session
        .append_message(assistant_msg("response"))
        .unwrap()
        .id()
        .to_string();

    let id_a = {
        session.branch(&id2).unwrap();
        session
            .append_message(user_msg("branch-A"))
            .unwrap()
            .id()
            .to_string()
    };
    let id_b = {
        session.branch(&id2).unwrap();
        session
            .append_message(user_msg("branch-B"))
            .unwrap()
            .id()
            .to_string()
    };
    let id_c = {
        session.branch(&id2).unwrap();
        session
            .append_message(user_msg("branch-C"))
            .unwrap()
            .id()
            .to_string()
    };

    let tree = session.get_tree();
    let node2 = &tree[0].children[0];
    assert_eq!(node2.entry.id(), id2);
    assert_eq!(node2.children.len(), 3);
    let mut branch_ids: Vec<String> = node2
        .children
        .iter()
        .map(|c| c.entry.id().to_string())
        .collect();
    branch_ids.sort();
    let mut expected = vec![id_a, id_b, id_c];
    expected.sort();
    assert_eq!(branch_ids, expected);
}

#[test]
fn branch_moves_the_leaf_pointer_and_throws_for_unknown_entries() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let id1 = session
        .append_message(user_msg("1"))
        .unwrap()
        .id()
        .to_string();
    let _ = session
        .append_message(assistant_msg("2"))
        .unwrap()
        .id()
        .to_string();
    let id3 = session
        .append_message(user_msg("3"))
        .unwrap()
        .id()
        .to_string();
    assert_eq!(session.get_leaf_id(), Some(id3.as_str()));

    session.branch(&id1).unwrap();
    assert_eq!(session.get_leaf_id(), Some(id1.as_str()));

    let error = session.branch("nonexistent").unwrap_err();
    assert_eq!(error.to_string(), "Entry nonexistent not found");
}

#[test]
fn new_appends_become_children_of_the_branch_point() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let id1 = session
        .append_message(user_msg("1"))
        .unwrap()
        .id()
        .to_string();
    let _ = session
        .append_message(assistant_msg("2"))
        .unwrap()
        .id()
        .to_string();

    session.branch(&id1).unwrap();
    let id3 = session
        .append_message(user_msg("branched"))
        .unwrap()
        .id()
        .to_string();
    assert_eq!(
        session.get_entry(&id3).unwrap().parent_id(),
        Some(id1.as_str())
    );
}

#[test]
fn branch_with_summary_throws_for_unknown_entries() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    session
        .append_message(user_msg("hello"))
        .unwrap()
        .id()
        .to_string();
    let error = session
        .branch_with_summary(Some("nonexistent"), "summary", None, None, None)
        .unwrap_err();
    assert_eq!(error.to_string(), "Entry nonexistent not found");
}

#[test]
fn get_leaf_entry_and_get_entry() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(session.get_leaf_entry(), None);
    session
        .append_message(user_msg("1"))
        .unwrap()
        .id()
        .to_string();
    let id2 = session
        .append_message(assistant_msg("2"))
        .unwrap()
        .id()
        .to_string();
    assert_eq!(session.get_leaf_entry().unwrap().id(), id2);

    assert_eq!(session.get_entry("nonexistent"), None);
    let id1 = session.get_entries()[0].id().to_string();
    let entry = session.get_entry(&id1).unwrap();
    assert_eq!(entry.kind(), "message");
    assert_eq!(entry.as_message().map(text_of).as_deref(), Some("1"));
}

#[test]
fn build_session_context_returns_messages_from_the_current_branch_only() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let _ = session
        .append_message(user_msg("msg1"))
        .unwrap()
        .id()
        .to_string();
    let id2 = session
        .append_message(assistant_msg("msg2"))
        .unwrap()
        .id()
        .to_string();
    let _ = session
        .append_message(user_msg("msg3"))
        .unwrap()
        .id()
        .to_string();

    session.branch(&id2).unwrap();
    session
        .append_message(assistant_msg("msg4-branch"))
        .unwrap()
        .id()
        .to_string();

    let ctx = session.build_session_context().unwrap();
    assert_eq!(ctx.messages.len(), 3); // msg1, msg2, msg4-branch (not msg3)
    assert_eq!(text_of(&ctx.messages[0]), "msg1");
    assert_eq!(text_of(&ctx.messages[1]), "msg2");
    assert_eq!(text_of(&ctx.messages[2]), "msg4-branch");
}

// ---------------------------------------------------------------------------
// labels (labels.test.ts)
// ---------------------------------------------------------------------------

#[test]
fn sets_and_gets_labels() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let msg_id = session
        .append_message(user_msg("hello"))
        .unwrap()
        .id()
        .to_string();
    assert_eq!(session.get_label(&msg_id), None);

    let label_id = session
        .append_label_change(&msg_id, Some("checkpoint"))
        .unwrap();
    assert_eq!(session.get_label(&msg_id), Some("checkpoint"));

    let SessionEntry::Label(label) = session.get_entry(&label_id).unwrap() else {
        panic!("expected label entry");
    };
    assert_eq!(label.target_id, msg_id);
    assert_eq!(label.label.as_deref(), Some("checkpoint"));
}

#[test]
fn clears_labels_with_none() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let msg_id = session
        .append_message(user_msg("hello"))
        .unwrap()
        .id()
        .to_string();
    session
        .append_label_change(&msg_id, Some("checkpoint"))
        .unwrap();
    assert_eq!(session.get_label(&msg_id), Some("checkpoint"));
    session.append_label_change(&msg_id, None).unwrap();
    assert_eq!(session.get_label(&msg_id), None);
}

#[test]
fn last_label_wins_and_the_tree_carries_the_label_timestamp() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let msg_id = session
        .append_message(user_msg("hello"))
        .unwrap()
        .id()
        .to_string();
    session.append_label_change(&msg_id, Some("first")).unwrap();
    session
        .append_label_change(&msg_id, Some("second"))
        .unwrap();
    let last_label_id = session.append_label_change(&msg_id, Some("third")).unwrap();
    assert_eq!(session.get_label(&msg_id), Some("third"));

    let SessionEntry::Label(last) = session.get_entry(&last_label_id).unwrap() else {
        panic!("expected label entry");
    };
    let tree = session.get_tree();
    let node = tree.iter().find(|n| n.entry.id() == msg_id).unwrap();
    assert_eq!(
        node.label_timestamp.as_deref(),
        Some(last.base.timestamp.as_str())
    );
}

#[test]
fn labels_are_included_in_tree_nodes() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let msg1_id = session
        .append_message(user_msg("hello"))
        .unwrap()
        .id()
        .to_string();
    let msg2_id = session
        .append_message(assistant_msg("hi"))
        .unwrap()
        .id()
        .to_string();
    let msg1_label_id = session
        .append_label_change(&msg1_id, Some("start"))
        .unwrap();
    let msg2_label_id = session
        .append_label_change(&msg2_id, Some("response"))
        .unwrap();

    let ts1 = match session.get_entry(&msg1_label_id).unwrap() {
        SessionEntry::Label(label) => label.base.timestamp,
        _ => panic!("expected label entry"),
    };
    let ts2 = match session.get_entry(&msg2_label_id).unwrap() {
        SessionEntry::Label(label) => label.base.timestamp,
        _ => panic!("expected label entry"),
    };

    let tree = session.get_tree();
    let msg1_node = tree.iter().find(|n| n.entry.id() == msg1_id).unwrap();
    assert_eq!(msg1_node.label.as_deref(), Some("start"));
    assert_eq!(msg1_node.label_timestamp.as_deref(), Some(ts1.as_str()));
    let msg2_node = msg1_node
        .children
        .iter()
        .find(|n| n.entry.id() == msg2_id)
        .unwrap();
    assert_eq!(msg2_node.label.as_deref(), Some("response"));
    assert_eq!(msg2_node.label_timestamp.as_deref(), Some(ts2.as_str()));
}

#[test]
fn labels_are_preserved_in_create_branched_session() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let msg1_id = session
        .append_message(user_msg("hello"))
        .unwrap()
        .id()
        .to_string();
    let msg2_id = session
        .append_message(assistant_msg("hi"))
        .unwrap()
        .id()
        .to_string();
    let msg1_label_id = session
        .append_label_change(&msg1_id, Some("important"))
        .unwrap();
    let msg2_label_id = session
        .append_label_change(&msg2_id, Some("also-important"))
        .unwrap();
    let ts1 = match session.get_entry(&msg1_label_id).unwrap() {
        SessionEntry::Label(label) => label.base.timestamp,
        _ => panic!("expected label entry"),
    };
    let ts2 = match session.get_entry(&msg2_label_id).unwrap() {
        SessionEntry::Label(label) => label.base.timestamp,
        _ => panic!("expected label entry"),
    };

    let result = session.create_branched_session(&msg2_id).unwrap();
    assert_eq!(result, None); // in-memory returns none

    assert_eq!(session.get_label(&msg1_id), Some("important"));
    assert_eq!(session.get_label(&msg2_id), Some("also-important"));
    let entries = session.get_entries();
    let label_entries: Vec<&SessionEntry> =
        entries.iter().filter(|e| e.kind() == "label").collect();
    assert_eq!(label_entries.len(), 2);
    let tree = session.get_tree();
    let msg1_node = tree.iter().find(|n| n.entry.id() == msg1_id).unwrap();
    let msg2_node = msg1_node
        .children
        .iter()
        .find(|n| n.entry.id() == msg2_id)
        .unwrap();
    assert_eq!(msg1_node.label_timestamp.as_deref(), Some(ts1.as_str()));
    assert_eq!(msg2_node.label_timestamp.as_deref(), Some(ts2.as_str()));
}

#[test]
fn rewires_children_of_removed_labels_when_forking() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let msg1_id = session
        .append_message(user_msg("hello"))
        .unwrap()
        .id()
        .to_string();
    session
        .append_label_change(&msg1_id, Some("checkpoint"))
        .unwrap();
    let model_change_id = session
        .append_model_change("anthropic", "claude-test")
        .unwrap()
        .id()
        .to_string();
    let msg2_id = session
        .append_message(user_msg("followup"))
        .unwrap()
        .id()
        .to_string();

    session.create_branched_session(&msg2_id).unwrap();
    assert_eq!(
        session.get_entry(&model_change_id).unwrap().parent_id(),
        Some(msg1_id.as_str())
    );
}

#[test]
fn labels_not_on_the_path_are_not_preserved() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let msg1_id = session
        .append_message(user_msg("hello"))
        .unwrap()
        .id()
        .to_string();
    let msg2_id = session
        .append_message(assistant_msg("hi"))
        .unwrap()
        .id()
        .to_string();
    let msg3_id = session
        .append_message(user_msg("followup"))
        .unwrap()
        .id()
        .to_string();

    session
        .append_label_change(&msg1_id, Some("first"))
        .unwrap();
    session
        .append_label_change(&msg2_id, Some("second"))
        .unwrap();
    session
        .append_label_change(&msg3_id, Some("third"))
        .unwrap();

    // Branch from msg2 (excludes msg3).
    session.create_branched_session(&msg2_id).unwrap();
    assert_eq!(session.get_label(&msg1_id), Some("first"));
    assert_eq!(session.get_label(&msg2_id), Some("second"));
    assert_eq!(session.get_label(&msg3_id), None);
}

#[test]
fn labels_are_not_included_in_build_session_context() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let msg_id = session
        .append_message(user_msg("hello"))
        .unwrap()
        .id()
        .to_string();
    session
        .append_label_change(&msg_id, Some("checkpoint"))
        .unwrap();
    let ctx = session.build_session_context().unwrap();
    assert_eq!(ctx.messages.len(), 1);
    assert_eq!(ctx.messages[0].role(), "user");
}

#[test]
fn labeling_a_non_existent_entry_throws() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let error = session
        .append_label_change("non-existent", Some("label"))
        .unwrap_err();
    assert_eq!(error.to_string(), "Entry non-existent not found");
}

// ---------------------------------------------------------------------------
// createBranchedSession (tree-traversal.test.ts)
// ---------------------------------------------------------------------------

#[test]
fn create_branched_session_throws_for_a_non_existent_entry() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    session
        .append_message(user_msg("hello"))
        .unwrap()
        .id()
        .to_string();
    let error = session.create_branched_session("nonexistent").unwrap_err();
    assert_eq!(error.to_string(), "Entry nonexistent not found");
}

#[test]
fn create_branched_session_extracts_the_path_from_a_branched_tree() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let id1 = session
        .append_message(user_msg("1"))
        .unwrap()
        .id()
        .to_string();
    let id2 = session
        .append_message(assistant_msg("2"))
        .unwrap()
        .id()
        .to_string();
    let _ = session
        .append_message(user_msg("3"))
        .unwrap()
        .id()
        .to_string();

    // Branch from 2: 2 -> 4 -> 5.
    session.branch(&id2).unwrap();
    let id4 = session
        .append_message(user_msg("4"))
        .unwrap()
        .id()
        .to_string();
    let id5 = session
        .append_message(assistant_msg("5"))
        .unwrap()
        .id()
        .to_string();

    // Branched session from id5 should have 1 -> 2 -> 4 -> 5.
    session.create_branched_session(&id5).unwrap();
    let entries = session.get_entries();
    assert_eq!(entries.len(), 4);
    assert_eq!(
        entries
            .iter()
            .map(|e| e.id().to_string())
            .collect::<Vec<_>>(),
        vec![id1, id2, id4, id5]
    );
}

#[test]
fn create_branched_session_keeps_only_the_path_to_the_leaf() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let id1 = session
        .append_message(user_msg("1"))
        .unwrap()
        .id()
        .to_string();
    let id2 = session
        .append_message(assistant_msg("2"))
        .unwrap()
        .id()
        .to_string();
    let id3 = session
        .append_message(user_msg("3"))
        .unwrap()
        .id()
        .to_string();
    session
        .append_message(assistant_msg("4"))
        .unwrap()
        .id()
        .to_string();

    session.branch(&id3).unwrap();
    let _ = session
        .append_message(user_msg("5"))
        .unwrap()
        .id()
        .to_string();

    // Branched session from id2 (should only have 1 -> 2); in-memory returns none.
    let result = session.create_branched_session(&id2).unwrap();
    assert_eq!(result, None);
    let entries = session.get_entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].id(), id1);
    assert_eq!(entries[1].id(), id2);
}

#[test]
fn create_branched_session_writes_the_file_only_once_a_conversation_exists() {
    let dir = tempdir("fork-dedup");
    let mut session =
        SessionManager::create(dir.path(), Some(dir.path()), &NewSessionOptions::default())
            .unwrap();
    let model_change_id = session
        .append_model_change("anthropic", "claude-sonnet-4-5")
        .unwrap()
        .id()
        .to_string();
    session
        .append_message(user_msg("first question"))
        .unwrap()
        .id()
        .to_string();
    session
        .append_message(assistant_msg("first answer"))
        .unwrap();

    // Fork from a setup entry (no user/assistant message in the branched path).
    let new_file = session
        .create_branched_session(&model_change_id)
        .unwrap()
        .unwrap();
    // Nothing to save yet, so the file is created later by the first user message.
    assert!(!new_file.exists());
    session
        .append_message(user_msg("new question"))
        .unwrap()
        .id()
        .to_string();
    assert!(new_file.exists());

    session
        .append_custom_entry("preset-state", Some(serde_json::json!({"name": "plan"})))
        .unwrap();
    session
        .append_message(assistant_msg("new answer"))
        .unwrap()
        .id()
        .to_string();

    // Exactly one header and each entry written once.
    assert_eq!(
        read_session_file_roles(&new_file),
        vec!["session", "model_change", "user", "custom", "assistant"]
    );
}

#[test]
fn create_branched_session_writes_immediately_when_forking_at_a_user_message() {
    let dir = tempdir("fork-with-user");
    let mut session =
        SessionManager::create(dir.path(), Some(dir.path()), &NewSessionOptions::default())
            .unwrap();
    let id1 = session
        .append_message(user_msg("first question"))
        .unwrap()
        .id()
        .to_string();
    session
        .append_message(assistant_msg("first answer"))
        .unwrap();

    let new_file = session.create_branched_session(&id1).unwrap().unwrap();
    assert!(new_file.exists());
    session
        .append_message(assistant_msg("new answer"))
        .unwrap()
        .id()
        .to_string();
    assert_eq!(
        read_session_file_roles(&new_file),
        vec!["session", "user", "assistant"]
    );
}

#[test]
fn usage_round_trips_through_a_file_backed_reload() {
    let dir = tempdir("usage-roundtrip");
    let mut session =
        SessionManager::create(dir.path(), Some(dir.path()), &NewSessionOptions::default())
            .unwrap();
    let root_id = session
        .append_message(user_msg("question"))
        .unwrap()
        .id()
        .to_string();
    session
        .append_message(assistant_msg("answer"))
        .unwrap()
        .id()
        .to_string();
    let _ = session.append_message(tool_result_msg());
    session
        .append_compaction(
            "summary",
            Some(&root_id),
            100,
            None,
            Some(false),
            Some(big_usage()),
        )
        .unwrap();
    session
        .branch_with_summary(
            Some(&root_id),
            "branch summary",
            None,
            Some(false),
            Some(big_usage()),
        )
        .unwrap();

    let file = session.get_session_file().unwrap().to_path_buf();
    let reopened = SessionManager::open(&file, Some(dir.path()), None).unwrap();
    let entries = reopened.get_entries();

    let compaction = entries.iter().find(|e| e.kind() == "compaction").unwrap();
    let SessionEntry::Compaction(compaction) = compaction else {
        panic!()
    };
    assert_eq!(compaction.usage.map(|u| u.total_tokens), Some(100));

    let summary = entries
        .iter()
        .find(|e| e.kind() == "branch_summary")
        .unwrap();
    let SessionEntry::BranchSummary(summary) = summary else {
        panic!()
    };
    assert_eq!(summary.usage.map(|u| u.total_tokens), Some(100));

    let tool_result = entries
        .iter()
        .find(|e| e.as_message().is_some_and(|m| m.role() == "toolResult"))
        .and_then(|e| e.as_message().cloned())
        .unwrap();
    let AgentMessage::ToolResult(result) = tool_result else {
        panic!()
    };
    assert_eq!(result.usage.map(|u| u.total_tokens), Some(100));
}

// ---------------------------------------------------------------------------
// session file creation + custom session ids (file-operations / custom-session-id)
// ---------------------------------------------------------------------------

#[test]
fn persisted_file_name_uses_the_munged_timestamp() {
    let dir = tempdir("custom-id");
    let session = SessionManager::create(
        dir.path(),
        Some(dir.path()),
        &NewSessionOptions {
            id: Some("created-session-id".to_string()),
            parent_session: None,
        },
    )
    .unwrap();
    assert_eq!(session.get_session_id(), "created-session-id");
    assert_eq!(session.get_header().unwrap().id, "created-session-id");
    let file = session.get_session_file().unwrap();
    let name = file.file_name().unwrap().to_string_lossy();
    let re = regex::Regex::new(
        r"^\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}-\d{3}Z_created-session-id\.jsonl$",
    )
    .unwrap();
    assert!(re.is_match(&name), "{name}");
    assert!(!file.exists());
}

#[test]
fn invalid_custom_session_ids_are_rejected() {
    for id in [
        "", "-abc", "abc-", "_abc", "abc_", ".abc", "abc.", "abc/def", "abc\\def", "abc def",
    ] {
        let error = SessionManager::in_memory(
            Path::new("/project"),
            &NewSessionOptions {
                id: Some(id.to_string()),
                parent_session: None,
            },
            Vec::new(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("Session id must be non-empty"),
            "{id}: {error}"
        );
    }
}

#[test]
fn generated_session_ids_are_uuid_v7_shaped() {
    let session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let id = session.get_session_id();
    let re =
        regex::Regex::new(r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$")
            .unwrap();
    assert!(re.is_match(id), "{id}");
    assert_eq!(session.get_header().unwrap().id, id);
}

#[test]
fn new_session_uses_the_provided_id_and_parent_session() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    session
        .new_session(&NewSessionOptions {
            id: Some("my-custom-id".to_string()),
            parent_session: Some("parent.jsonl".to_string()),
        })
        .unwrap();
    assert_eq!(session.get_session_id(), "my-custom-id");
    let header = session.get_header().unwrap();
    assert_eq!(header.id, "my-custom-id");
    assert_eq!(header.parent_session.as_deref(), Some("parent.jsonl"));
    assert_eq!(header.version_or_v1(), 3);
}

#[test]
fn fork_from_copies_all_entries_unchanged_into_the_forked_session() {
    let dir = tempdir("fork-copy");
    let source_path = dir.path().join("source.jsonl");
    let mut lines = vec![
        serde_json::json!({
            "type": "session", "version": 3, "id": "source-id",
            "timestamp": "2026-01-01T00:00:00.000Z", "cwd": dir.path().to_string_lossy(),
        })
        .to_string(),
    ];
    let mut parent: Option<String> = None;
    for i in 0..50 {
        let id = format!("e{i}");
        lines.push(
            serde_json::json!({
                "type": "message", "id": id,
                "parentId": parent,
                "timestamp": "2026-01-01T00:00:00.000Z",
                "message": {"role": "user", "content": format!("message {i}"), "timestamp": 0},
            })
            .to_string(),
        );
        parent = Some(id);
    }
    fs::write(&source_path, format!("{}\n", lines.join("\n"))).unwrap();

    let sessions_dir = dir.path().join("sessions");
    let forked = SessionManager::fork_from(
        &source_path,
        dir.path(),
        Some(&sessions_dir),
        &NewSessionOptions::default(),
    )
    .unwrap();

    let written = fs::read_to_string(forked.get_session_file().unwrap()).unwrap();
    let written: Vec<&str> = written.trim_end().split('\n').collect();
    assert_eq!(written.len(), 51);
    let header: serde_json::Value = serde_json::from_str(written[0]).unwrap();
    assert_eq!(
        header["parentSession"].as_str().unwrap(),
        source_path.to_string_lossy()
    );
    assert_eq!(&written[1..], &lines[1..]);
    assert_eq!(forked.get_entries().len(), 50);
}

#[test]
fn fork_from_takes_the_requested_id_and_records_the_parent_session() {
    let dir = tempdir("fork-id");
    let source_path = dir.path().join("source.jsonl");
    fs::write(
        &source_path,
        format!(
            "{}\n",
            serde_json::json!({
                "type": "session", "version": 3, "id": "source-session-id",
                "timestamp": "2026-01-01T00:00:00.000Z", "cwd": dir.path().to_string_lossy(),
            })
        ),
    )
    .unwrap();

    let forked = SessionManager::fork_from(
        &source_path,
        dir.path(),
        Some(dir.path()),
        &NewSessionOptions {
            id: Some("forked-session-id".to_string()),
            parent_session: None,
        },
    )
    .unwrap();
    let header = forked.get_header().unwrap();
    assert_eq!(header.id, "forked-session-id");
    assert_eq!(
        header.parent_session.as_deref(),
        Some(source_path.to_string_lossy().as_ref())
    );
    assert!(
        forked
            .get_session_file()
            .unwrap()
            .to_string_lossy()
            .contains("forked-session-id")
    );
}

#[test]
fn fork_from_rejects_an_empty_or_headerless_source() {
    let dir = tempdir("fork-invalid");
    let empty = dir.path().join("empty.jsonl");
    fs::write(&empty, "").unwrap();
    let headerless = dir.path().join("headerless.jsonl");
    fs::write(&headerless, "{\"type\":\"message\",\"id\":\"1\"}\n").unwrap();

    let error = SessionManager::fork_from(
        &empty,
        dir.path(),
        Some(dir.path()),
        &NewSessionOptions::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("Cannot fork"), "{error}");
    let error = SessionManager::fork_from(
        &headerless,
        dir.path(),
        Some(dir.path()),
        &NewSessionOptions::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("Cannot fork"), "{error}");
}

// ---------------------------------------------------------------------------
// loadEntriesFromFile + corrupted-file recovery (file-operations.test.ts)
// ---------------------------------------------------------------------------

fn write_session_header(file: &Path, cwd: &str, id: &str, prefix: &str) {
    fs::write(
        file,
        format!(
            "{prefix}{}\n",
            serde_json::json!({
                "type": "session", "version": 3, "id": id,
                "timestamp": "2025-01-01T00:00:00Z", "cwd": cwd,
            })
        ),
    )
    .unwrap();
}

#[test]
fn load_entries_from_file_edge_cases() {
    let dir = tempdir("load-entries");

    // Missing file.
    assert_eq!(
        load_entries_from_file(&dir.path().join("nonexistent.jsonl")).unwrap(),
        Vec::<pi_coding_agent::session::Record>::new()
    );

    // Empty file.
    let empty = dir.path().join("empty.jsonl");
    fs::write(&empty, "").unwrap();
    assert_eq!(load_entries_from_file(&empty).unwrap().len(), 0);

    // No valid session header.
    let no_header = dir.path().join("no-header.jsonl");
    fs::write(&no_header, "{\"type\":\"message\",\"id\":\"1\"}\n").unwrap();
    assert_eq!(load_entries_from_file(&no_header).unwrap().len(), 0);

    // Malformed JSON.
    let malformed = dir.path().join("malformed.jsonl");
    fs::write(&malformed, "not json\n").unwrap();
    assert_eq!(load_entries_from_file(&malformed).unwrap().len(), 0);

    // Valid session file.
    let valid = dir.path().join("valid.jsonl");
    fs::write(
        &valid,
        concat!(
            "{\"type\":\"session\",\"id\":\"abc\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"id\":\"1\",\"parentId\":null,\"timestamp\":\"2025-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n",
        ),
    )
    .unwrap();
    let entries = load_entries_from_file(&valid).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].kind(), "session");
    assert_eq!(entries[1].kind(), "message");

    // Skips malformed lines but keeps valid ones.
    let mixed = dir.path().join("mixed.jsonl");
    fs::write(
        &mixed,
        concat!(
            "{\"type\":\"session\",\"id\":\"abc\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "not valid json\n",
            "{\"type\":\"message\",\"id\":\"1\",\"parentId\":null,\"timestamp\":\"2025-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n",
        ),
    )
    .unwrap();
    assert_eq!(load_entries_from_file(&mixed).unwrap().len(), 2);
}

#[test]
fn load_entries_from_file_repairs_a_missing_trailing_newline() {
    let dir = tempdir("unterminated");
    let content = concat!(
        "{\"type\":\"session\",\"id\":\"abc\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\",\"id\":\"1\",\"parentId\":null,\"timestamp\":\"2025-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}",
    );
    let file = dir.path().join("unterminated.jsonl");
    fs::write(&file, content).unwrap();

    assert_eq!(load_entries_from_file(&file).unwrap().len(), 2);
    assert_eq!(fs::read_to_string(&file).unwrap(), format!("{content}\n"));
}

/// Upstream decides with `if (pending)` on the text after the last `\n`
/// (`session-manager.ts:669`), not on `endsWith("\n")`. Trailing spaces after
/// the final entry therefore still get a newline appended.
#[test]
fn load_entries_from_file_repairs_a_tail_with_only_trailing_spaces() {
    let dir = tempfile::Builder::new()
        .prefix("pi-rs-trailing-spaces-")
        .tempdir()
        .unwrap();
    let content = concat!(
        "{\"type\":\"session\",\"id\":\"abc\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\",\"id\":\"1\",\"parentId\":null,\"timestamp\":\"2025-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n",
        "   \t  ",
    );
    let file = dir.path().join("trailing-spaces.jsonl");
    fs::write(&file, content).unwrap();

    assert_eq!(load_entries_from_file(&file).unwrap().len(), 2);
    assert_eq!(fs::read_to_string(&file).unwrap(), format!("{content}\n"));
}

#[test]
fn load_entries_from_file_repairs_an_unterminated_malformed_tail() {
    let dir = tempdir("malformed-tail");
    let content = concat!(
        "{\"type\":\"session\",\"id\":\"abc\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\"",
    );
    let file = dir.path().join("malformed-tail.jsonl");
    fs::write(&file, content).unwrap();

    assert_eq!(load_entries_from_file(&file).unwrap().len(), 1);
    assert_eq!(fs::read_to_string(&file).unwrap(), format!("{content}\n"));
}

#[test]
fn load_entries_from_file_does_not_modify_an_unterminated_non_session_file() {
    let dir = tempdir("invalid");
    let content = "{\"type\":\"message\",\"id\":\"1\"}";
    let file = dir.path().join("invalid.jsonl");
    fs::write(&file, content).unwrap();

    assert_eq!(load_entries_from_file(&file).unwrap().len(), 0);
    assert_eq!(fs::read_to_string(&file).unwrap(), content);
}

#[test]
fn open_reads_the_cwd_from_headers_behind_blanks_and_garbage() {
    let dir = tempdir("header-open");
    let file = dir.path().join("header.jsonl");
    let stored_cwd = dir.path().join("stored-project");
    write_session_header(
        &file,
        stored_cwd.to_string_lossy().as_ref(),
        "a".repeat(8192).as_str(),
        // A header larger than one read buffer, after blank + malformed lines.
        "\n  \nnot json\n{broken json\n",
    );

    let session = SessionManager::open(&file, Some(dir.path()), None).unwrap();
    assert_eq!(session.get_session_id(), "a".repeat(8192));
    assert_eq!(session.get_cwd(), stored_cwd);

    // A cwd override wins over the stored header cwd.
    let override_cwd = dir.path().join("override-project");
    let session = SessionManager::open(&file, Some(dir.path()), Some(&override_cwd)).unwrap();
    assert_eq!(session.get_cwd(), override_cwd);
}

#[test]
fn open_falls_back_to_a_full_load_beyond_the_discovery_scan_limit() {
    let dir = tempdir("scan-limit");
    let stored_cwd = dir.path().join("stored-project");
    let override_cwd = dir.path().join("override-project");
    // A header whose id alone exceeds the 1 MiB scan limit.
    let file = dir.path().join("large-header.jsonl");
    write_session_header(
        &file,
        stored_cwd.to_string_lossy().as_ref(),
        "a".repeat(1024 * 1024 + 1).as_str(),
        "",
    );
    for cwd_override in [None, Some(override_cwd.as_path())] {
        let session = SessionManager::open(&file, Some(dir.path()), cwd_override).unwrap();
        assert_eq!(session.get_session_id(), "a".repeat(1024 * 1024 + 1));
        assert_eq!(
            session.get_cwd(),
            cwd_override.map_or_else(|| stored_cwd.clone(), Path::to_path_buf)
        );
    }
    // A malformed prefix that pushes the header beyond the scan limit.
    let file = dir.path().join("large-prefix.jsonl");
    write_session_header(
        &file,
        stored_cwd.to_string_lossy().as_ref(),
        "large-prefix",
        &format!("{}\n", "x".repeat(1024 * 1024 + 1)),
    );
    let session = SessionManager::open(&file, Some(dir.path()), None).unwrap();
    assert_eq!(session.get_session_id(), "large-prefix");
    assert_eq!(session.get_cwd(), stored_cwd);
}

// ---------------------------------------------------------------------------
// findMostRecentSession + corrupted file recovery
// ---------------------------------------------------------------------------

#[test]
fn find_most_recent_session_scenarios() {
    let dir = tempdir("recent");

    // Empty / missing directories.
    assert_eq!(
        pi_coding_agent::session::find_most_recent_session(dir.path(), None),
        None
    );
    assert_eq!(
        pi_coding_agent::session::find_most_recent_session(&dir.path().join("nonexistent"), None),
        None
    );

    // Non-jsonl files are ignored; so are files without a valid header.
    fs::write(dir.path().join("file.txt"), "hello").unwrap();
    fs::write(dir.path().join("file.json"), "{}").unwrap();
    let invalid = dir.path().join("invalid.jsonl");
    fs::write(&invalid, "{\"type\":\"message\"}\n").unwrap();
    assert_eq!(
        pi_coding_agent::session::find_most_recent_session(dir.path(), None),
        None
    );

    // A single valid file is returned; invalid files are skipped.
    let valid = dir.path().join("valid.jsonl");
    fs::write(
        &valid,
        "{\"type\":\"session\",\"id\":\"abc\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
    )
    .unwrap();
    assert_eq!(
        pi_coding_agent::session::find_most_recent_session(dir.path(), None),
        Some(valid.clone())
    );

    // The most recently modified one wins.
    let older = dir.path().join("older.jsonl");
    let newer = dir.path().join("newer.jsonl");
    fs::write(&older, "{\"type\":\"session\",\"id\":\"old\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    fs::write(&newer, "{\"type\":\"session\",\"id\":\"new\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n").unwrap();
    assert_eq!(
        pi_coding_agent::session::find_most_recent_session(dir.path(), None),
        Some(newer.clone())
    );

    // An oversized corrupt file is skipped in favor of a valid one.
    let oversized = dir.path().join("oversized.jsonl");
    fs::write(&oversized, "x".repeat(1024 * 1024 + 1)).unwrap();
    assert_eq!(
        pi_coding_agent::session::find_most_recent_session(dir.path(), None),
        Some(newer.clone())
    );

    // cwd filtering.
    let project_a = dir.path().join("project-a");
    let project_b = dir.path().join("project-b");
    let file_a = dir.path().join("a.jsonl");
    let file_b = dir.path().join("b.jsonl");
    fs::write(
        &file_a,
        serde_json::json!({"type": "session", "id": "a", "timestamp": "2025-01-01T00:00:00Z", "cwd": project_a.to_string_lossy()})
            .to_string()
            + "\n",
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    fs::write(
        &file_b,
        serde_json::json!({"type": "session", "id": "b", "timestamp": "2025-01-01T00:00:00Z", "cwd": project_b.to_string_lossy()})
            .to_string()
            + "\n",
    )
    .unwrap();
    assert_eq!(
        pi_coding_agent::session::find_most_recent_session(dir.path(), Some(&project_a)),
        Some(file_a.clone())
    );
    assert_eq!(
        pi_coding_agent::session::find_most_recent_session(dir.path(), Some(&project_b)),
        Some(file_b.clone())
    );
}

#[test]
fn open_recovers_an_empty_file_with_a_fresh_header() {
    let dir = tempdir("empty-open");
    let empty_file = dir.path().join("empty.jsonl");
    fs::write(&empty_file, "").unwrap();

    let sm = SessionManager::open(&empty_file, Some(dir.path()), None).unwrap();
    assert!(sm.get_session_id().len() >= 36);
    assert!(sm.get_header().is_some());

    // The file now contains a valid header for that same session.
    let content = fs::read_to_string(&empty_file).unwrap();
    let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1);
    let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(header["type"], "session");
    assert_eq!(header["id"].as_str().unwrap(), sm.get_session_id());

    // Subsequent loads reuse the id.
    let sm2 = SessionManager::open(&empty_file, Some(dir.path()), None).unwrap();
    assert_eq!(sm2.get_session_id(), sm.get_session_id());
}

#[test]
fn open_rejects_and_preserves_a_non_empty_file_without_a_valid_header() {
    let dir = tempdir("no-header-open");
    let no_header_file = dir.path().join("no-header.jsonl");
    let original_content = "{\"type\":\"message\",\"id\":\"abc\",\"parentId\":\"orphaned\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"message\":{\"role\":\"assistant\",\"content\":\"test\"}}\n";
    fs::write(&no_header_file, original_content).unwrap();

    let error = SessionManager::open(&no_header_file, Some(dir.path()), None).unwrap_err();
    let SessionError::NotASession(path) = &error else {
        panic!("expected NotASession, got {error:?}")
    };
    assert_eq!(path, &no_header_file);
    assert_eq!(
        error.to_string(),
        format!(
            "Session file is not a valid pi session: {}",
            no_header_file.display()
        )
    );
    assert_eq!(
        fs::read_to_string(&no_header_file).unwrap(),
        original_content
    );
}

// ---------------------------------------------------------------------------
// inMemory with preloaded entries (load-entries.test.ts)
// ---------------------------------------------------------------------------

fn stored_entries(build: impl FnOnce(&mut SessionManager)) -> Vec<SessionEntry> {
    let mut source = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    build(&mut source);
    source.get_entries()
}

fn to_records(entries: Vec<SessionEntry>) -> Vec<pi_coding_agent::session::Record> {
    entries.into_iter().map(|entry| entry.to_record()).collect()
}

#[test]
fn in_memory_adopts_preloaded_entries_verbatim() {
    let entries = stored_entries(|source| {
        source
            .append_message(user_msg_blocks("hello"))
            .unwrap()
            .id()
            .to_string();
        source
            .append_model_change("anthropic", "claude-opus-4-5")
            .unwrap();
        source
            .append_message(user_msg_blocks("again"))
            .unwrap()
            .id()
            .to_string();
    });
    let session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        to_records(entries.clone()),
    )
    .unwrap();
    let restored = session.get_entries();
    assert_eq!(restored.len(), entries.len());
    for (restored, original) in restored.iter().zip(entries.iter()) {
        assert_eq!(
            restored.to_record().to_line(),
            original.to_record().to_line()
        );
    }
}

#[test]
fn in_memory_keeps_the_loaded_leaf_so_appends_continue() {
    let entries = stored_entries(|source| {
        source
            .append_message(user_msg_blocks("hello"))
            .unwrap()
            .id()
            .to_string();
        source
            .append_message(user_msg_blocks("again"))
            .unwrap()
            .id()
            .to_string();
    });
    let last_id = entries.last().unwrap().id().to_string();
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        to_records(entries),
    )
    .unwrap();
    let appended = session
        .append_message(user_msg_blocks("continued"))
        .unwrap()
        .id()
        .to_string();
    assert_eq!(session.get_leaf_id(), Some(appended.as_str()));
    assert_eq!(
        session.get_entry(&appended).unwrap().parent_id(),
        Some(last_id.as_str())
    );
}

#[test]
fn in_memory_never_mints_an_id_that_collides_with_a_loaded_entry() {
    let entries = stored_entries(|source| {
        for i in 0..50 {
            source
                .append_message(user_msg_blocks(&format!("message {i}")))
                .unwrap();
        }
    });
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        to_records(entries.clone()),
    )
    .unwrap();
    let appended = session
        .append_message(user_msg_blocks("continued"))
        .unwrap()
        .id()
        .to_string();
    assert!(!entries.iter().any(|entry| entry.id() == appended));
}

#[test]
fn in_memory_rebuilds_the_branch_structure_rather_than_a_flat_chain() {
    let entries = stored_entries(|source| {
        let first = source
            .append_message(user_msg_blocks("hello"))
            .unwrap()
            .id()
            .to_string();
        source
            .append_message(user_msg_blocks("abandoned"))
            .unwrap()
            .id()
            .to_string();
        source.branch(&first).unwrap();
        source
            .append_message(user_msg_blocks("kept"))
            .unwrap()
            .id()
            .to_string();
    });
    let session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        to_records(entries),
    )
    .unwrap();
    let roots = session.get_tree();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].children.len(), 2);
}

#[test]
fn in_memory_rebuilds_labels() {
    let labelled_id = std::cell::RefCell::new(String::new());
    let entries = stored_entries(|source| {
        let id = source
            .append_message(user_msg_blocks("hello"))
            .unwrap()
            .id()
            .to_string();
        labelled_id.replace(id.clone());
        source.append_label_change(&id, Some("checkpoint")).unwrap();
    });
    let session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        to_records(entries),
    )
    .unwrap();
    assert_eq!(session.get_label(&labelled_id.borrow()), Some("checkpoint"));
}

#[test]
fn in_memory_resolves_a_compaction_against_its_written_entry() {
    let kept_id = std::cell::RefCell::new(String::new());
    let entries = stored_entries(|source| {
        source
            .append_message(user_msg_blocks("dropped"))
            .unwrap()
            .id()
            .to_string();
        let id = source
            .append_message(user_msg_blocks("kept"))
            .unwrap()
            .id()
            .to_string();
        kept_id.replace(id.clone());
        source
            .append_compaction("summary so far", Some(&id), 1000, None, None, None)
            .unwrap();
    });
    let session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        to_records(entries),
    )
    .unwrap();
    let context = session.build_context_entries();
    assert!(
        context
            .iter()
            .any(|entry| entry.id() == kept_id.borrow().as_str())
    );
}

#[test]
fn in_memory_creates_a_header_from_the_options_when_entries_carry_none() {
    let entries = stored_entries(|source| {
        source
            .append_message(user_msg_blocks("hello"))
            .unwrap()
            .id()
            .to_string();
    });
    let session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions {
            id: Some("restored-session".to_string()),
            parent_session: None,
        },
        to_records(entries),
    )
    .unwrap();
    assert_eq!(session.get_session_id(), "restored-session");
    let header = session.get_header().unwrap();
    assert_eq!(header.id, "restored-session");
    assert_eq!(header.cwd, "/project");
}

#[test]
fn in_memory_takes_the_session_identity_from_a_header_among_the_entries() {
    let body = stored_entries(|source| {
        source
            .append_message(user_msg_blocks("hello"))
            .unwrap()
            .id()
            .to_string();
    });
    let mut records = vec![pi_coding_agent::session::Record::from_object(
        serde_json::json!({
            "type": "session", "version": 3, "id": "stored-session",
            "timestamp": "2026-01-01T00:00:00Z", "cwd": "/stored",
        })
        .as_object()
        .unwrap()
        .clone(),
    )];
    records.extend(to_records(body));
    let session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions {
            id: Some("ignored".to_string()),
            parent_session: None,
        },
        records,
    )
    .unwrap();
    assert_eq!(session.get_session_id(), "stored-session");
    assert_eq!(session.get_header().unwrap().cwd, "/stored");
}

#[test]
fn in_memory_migrates_entries_restored_with_an_older_header() {
    let records = vec![
        pi_coding_agent::session::Record::from_object(
            serde_json::json!({
                "type": "session", "version": 2, "id": "v2-session",
                "timestamp": "2026-01-01T00:00:00Z", "cwd": "/project",
            })
            .as_object()
            .unwrap()
            .clone(),
        ),
        pi_coding_agent::session::Record::parse_line_strict(
            "{\"type\":\"message\",\"id\":\"abc12345\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{\"role\":\"hookMessage\",\"content\":\"from a hook\",\"timestamp\":1}}",
        )
        .unwrap(),
    ];
    let session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        records,
    )
    .unwrap();
    assert_eq!(session.get_header().unwrap().version, Some(3));
    let restored = session.get_entries()[0].clone();
    let message = restored.as_message().unwrap();
    // The unknown `hookMessage` role is kept verbatim through the migration.
    assert_eq!(message.role(), "custom");
    assert_eq!(restored.id(), "abc12345");
}

#[test]
fn in_memory_stays_off_the_filesystem() {
    let entries = stored_entries(|source| {
        source
            .append_message(user_msg_blocks("hello"))
            .unwrap()
            .id()
            .to_string();
    });
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        to_records(entries),
    )
    .unwrap();
    session
        .append_message(user_msg_blocks("continued"))
        .unwrap();
    assert_eq!(session.get_session_file(), None);
    assert!(!session.is_persisted());
}

#[test]
fn in_memory_starts_an_empty_session_when_the_entries_are_empty() {
    let session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions {
            id: Some("empty-session".to_string()),
            parent_session: None,
        },
        Vec::new(),
    )
    .unwrap();
    assert_eq!(session.get_session_id(), "empty-session");
    assert_eq!(session.get_entries().len(), 0);
    assert_eq!(session.get_leaf_id(), None);
}

// ---------------------------------------------------------------------------
// session listing
// ---------------------------------------------------------------------------

#[test]
fn list_and_continue_recent_scope_by_cwd() {
    let dir = tempdir("list");
    let project_a = dir.path().join("project-a");
    let project_b = dir.path().join("project-b");
    fs::create_dir_all(&project_a).unwrap();
    fs::create_dir_all(&project_b).unwrap();

    let create_session = |cwd: &Path, label: &str| -> std::path::PathBuf {
        let mut session =
            SessionManager::create(cwd, Some(dir.path()), &NewSessionOptions::default()).unwrap();
        session
            .append_message(user_msg(label))
            .unwrap()
            .id()
            .to_string();
        session
            .append_message(assistant_msg(&format!("reply to {label}")))
            .unwrap();
        session.get_session_file().unwrap().to_path_buf()
    };
    let session_a = create_session(&project_a, "from A");
    std::thread::sleep(std::time::Duration::from_millis(30));
    let session_b = create_session(&project_b, "from B");

    let current_a = SessionManager::list(&project_a, Some(dir.path()));
    assert_eq!(
        current_a.iter().map(|s| s.path.clone()).collect::<Vec<_>>(),
        vec![session_a.clone()]
    );

    let all = SessionManager::list_all(Some(dir.path()));
    let mut paths: Vec<std::path::PathBuf> = all.iter().map(|s| s.path.clone()).collect();
    paths.sort();
    let mut expected = vec![session_a.clone(), session_b.clone()];
    expected.sort();
    assert_eq!(paths, expected);

    let continued_a = SessionManager::continue_recent(&project_a, Some(dir.path())).unwrap();
    assert_eq!(continued_a.get_session_file(), Some(session_a.as_path()));
}

#[test]
fn session_info_summarizes_the_transcript() {
    let dir = tempdir("info");
    let mut session =
        SessionManager::create(dir.path(), Some(dir.path()), &NewSessionOptions::default())
            .unwrap();
    session
        .append_message(user_msg("first question"))
        .unwrap()
        .id()
        .to_string();
    session
        .append_message(assistant_msg("first answer"))
        .unwrap();
    session
        .append_message(user_msg("second question"))
        .unwrap()
        .id()
        .to_string();
    session.append_session_info("My session").unwrap();

    let file = session.get_session_file().unwrap().to_path_buf();
    let infos = pi_coding_agent::session::list_sessions_in_dir(dir.path());
    assert_eq!(infos.len(), 1);
    let info = &infos[0];
    assert_eq!(info.path, file);
    assert_eq!(info.id, session.get_session_id());
    assert_eq!(info.cwd, dir.path().to_string_lossy());
    assert_eq!(info.name.as_deref(), Some("My session"));
    assert_eq!(info.message_count, 3);
    assert_eq!(info.first_message, "first question");
    assert_eq!(
        info.all_messages_text,
        "first question first answer second question"
    );
    assert!(info.created.timestamp_millis() > 0);
}

#[test]
fn find_by_id_matches_the_header_id() {
    let dir = tempdir("find-id");
    let mut session = SessionManager::create(
        dir.path(),
        Some(dir.path()),
        &NewSessionOptions {
            id: Some("wanted-id".to_string()),
            parent_session: None,
        },
    )
    .unwrap();
    session
        .append_message(user_msg("hello"))
        .unwrap()
        .id()
        .to_string();
    let file = session.get_session_file().unwrap().to_path_buf();
    assert_eq!(
        SessionManager::find_by_id(dir.path(), "wanted-id", Some(dir.path())),
        Some(file)
    );
    assert_eq!(
        SessionManager::find_by_id(dir.path(), "other-id", Some(dir.path())),
        None
    );
}

#[test]
fn append_session_info_sanitizes_newlines() {
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    session
        .append_session_info("line one\nline two\r\n\nline three")
        .unwrap();
    assert_eq!(
        session.get_session_name().as_deref(),
        Some("line one line two line three")
    );
    // An empty name explicitly clears the title.
    session.append_session_info("   ").unwrap();
    assert_eq!(session.get_session_name(), None);
}

#[test]
fn unknown_entry_types_survive_a_load_append_rewrite_cycle() {
    let dir = tempdir("unknown-kind");
    let file = dir.path().join("session.jsonl");
    fs::write(
        &file,
        concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2025-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n",
            "{\"type\":\"ext-thing\",\"payload\":[1],\"id\":\"e1\",\"parentId\":\"m1\",\"timestamp\":\"2025-01-01T00:00:02Z\"}\n",
        ),
    )
    .unwrap();

    let mut session = SessionManager::open(&file, Some(dir.path()), None).unwrap();
    assert_eq!(session.get_entry_count(), 2);
    session
        .append_message(user_msg("next"))
        .unwrap()
        .id()
        .to_string();

    let rewritten = fs::read_to_string(&file).unwrap();
    let lines: Vec<&str> = rewritten.trim_end().split('\n').collect();
    assert_eq!(lines.len(), 4);
    // The extension entry survives byte-for-byte.
    assert_eq!(
        lines[2],
        "{\"type\":\"ext-thing\",\"payload\":[1],\"id\":\"e1\",\"parentId\":\"m1\",\"timestamp\":\"2025-01-01T00:00:02Z\"}"
    );
}

#[test]
fn entry_lines_are_byte_compatible_with_the_session_format_doc() {
    // The header/message shapes from docs/session-format.md.
    let mut session = SessionManager::in_memory(
        Path::new("/project"),
        &NewSessionOptions::default(),
        Vec::new(),
    )
    .unwrap();
    let user_entry = session
        .append_message(user_msg("Hello"))
        .unwrap()
        .id()
        .to_string();
    let lines = session
        .get_entries()
        .iter()
        .map(|entry| entry.to_record().to_line())
        .collect::<Vec<_>>();
    // `{"type":"message",...,"message":{"role":"user","content":"Hello","timestamp":1}}`
    assert_eq!(
        lines[0],
        format!(
            "{{\"type\":\"message\",\"id\":\"{user_entry}\",\"parentId\":null,\"timestamp\":\"{}\",\"message\":{{\"role\":\"user\",\"content\":\"Hello\",\"timestamp\":1}}}}",
            session.get_entry(&user_entry).unwrap().timestamp()
        )
    );
}
