//! Port of `test/session-manager/migration.test.ts` plus the byte-level
//! migration pins from `docs/session-format.md`.

use pi_coding_agent::session::{
    migrate_to_current_version, parse_session_entries, serialize_entries, version_of,
};

#[test]
fn adds_id_and_parent_id_to_v1_entries() {
    let mut entries = parse_session_entries(concat!(
        "{\"type\":\"session\",\"id\":\"sess-1\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\",\"timestamp\":\"2025-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n",
        "{\"type\":\"message\",\"timestamp\":\"2025-01-01T00:00:02Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}],\"api\":\"test\",\"provider\":\"test\",\"model\":\"test\",\"usage\":{\"input\":1,\"output\":1,\"cacheRead\":0,\"cacheWrite\":0},\"stopReason\":\"stop\",\"timestamp\":2}}\n",
    ));

    migrate_to_current_version(&mut entries);

    // Header version set (v3 is current after hookMessage->custom migration).
    assert_eq!(
        entries[0]
            .get("version")
            .and_then(serde_json::Value::as_u64),
        Some(3)
    );

    // Entries have id/parentId, 8 chars for generated ids, null root parent.
    let msg1 = &entries[1];
    let msg2 = &entries[2];
    let id1 = msg1.id().unwrap().to_string();
    let id2 = msg2.id().unwrap().to_string();
    assert_eq!(id1.len(), 8);
    assert_eq!(msg1.parent_id(), None);
    assert_eq!(id2.len(), 8);
    assert_eq!(msg2.parent_id(), Some(id1.as_str()));
}

#[test]
fn is_idempotent_for_already_migrated_entries() {
    let mut entries = parse_session_entries(concat!(
        "{\"type\":\"session\",\"id\":\"sess-1\",\"version\":2,\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\",\"id\":\"abc12345\",\"parentId\":null,\"timestamp\":\"2025-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n",
        "{\"type\":\"message\",\"id\":\"def67890\",\"parentId\":\"abc12345\",\"timestamp\":\"2025-01-01T00:00:02Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}],\"api\":\"test\",\"provider\":\"test\",\"model\":\"test\",\"usage\":{\"input\":1,\"output\":1,\"cacheRead\":0,\"cacheWrite\":0},\"stopReason\":\"stop\",\"timestamp\":2}}\n",
    ));

    migrate_to_current_version(&mut entries);

    // IDs unchanged.
    assert_eq!(entries[1].id(), Some("abc12345"));
    assert_eq!(entries[2].id(), Some("def67890"));
    assert_eq!(entries[2].parent_id(), Some("abc12345"));
}

#[test]
fn v1_header_gains_version_appended_like_a_js_property() {
    let mut entries = parse_session_entries(
        "{\"type\":\"session\",\"id\":\"s1\",\"timestamp\":\"2025-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}\n",
    );
    assert_eq!(version_of(&entries), 1);
    assert!(migrate_to_current_version(&mut entries));
    assert_eq!(
        serialize_entries(&entries),
        "{\"type\":\"session\",\"id\":\"s1\",\"timestamp\":\"2025-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\",\"version\":3}\n"
    );
}

#[test]
fn hook_message_role_becomes_custom_in_place() {
    let mut entries = parse_session_entries(concat!(
        "{\"type\":\"session\",\"version\":2,\"id\":\"s1\",\"timestamp\":\"t\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\",\"id\":\"a1\",\"parentId\":null,\"timestamp\":\"t\",\"message\":{\"role\":\"hookMessage\",\"content\":\"hi\",\"timestamp\":1}}\n",
    ));
    assert!(migrate_to_current_version(&mut entries));
    let message = entries[1].get("message").unwrap();
    assert_eq!(
        message.get("role").and_then(serde_json::Value::as_str),
        Some("custom")
    );
    // The role key stays in its original position.
    let keys: Vec<&String> = message.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["role", "content", "timestamp"]);
}

#[test]
fn v1_compaction_first_kept_entry_index_becomes_an_id() {
    let mut entries = parse_session_entries(concat!(
        "{\"type\":\"session\",\"id\":\"s1\",\"timestamp\":\"t\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\",\"timestamp\":\"t\",\"message\":{\"role\":\"user\",\"content\":\"a\",\"timestamp\":1}}\n",
        "{\"type\":\"message\",\"timestamp\":\"t\",\"message\":{\"role\":\"user\",\"content\":\"b\",\"timestamp\":2}}\n",
        "{\"type\":\"compaction\",\"timestamp\":\"t\",\"summary\":\"s\",\"firstKeptEntryIndex\":2,\"tokensBefore\":10}\n",
    ));
    assert!(migrate_to_current_version(&mut entries));
    let second = entries[2].id().unwrap().to_string();
    let compaction = &entries[3];
    assert_eq!(
        compaction.get_str("firstKeptEntryId"),
        Some(second.as_str())
    );
    assert!(compaction.get("firstKeptEntryIndex").is_none());
}

#[test]
fn parse_session_entries_skips_blank_and_malformed_lines() {
    let entries = parse_session_entries(concat!(
        "\n",
        "not json\n",
        "{\"type\":\"session\",\"id\":\"s1\",\"timestamp\":\"t\",\"cwd\":\"/tmp\"}\n",
        "\n",
    ));
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind(), "session");
}

#[test]
fn loaded_v2_message_entries_round_trip_through_the_typed_view() {
    let line = "{\"type\":\"message\",\"id\":\"a1\",\"parentId\":null,\"timestamp\":\"t\",\"message\":{\"role\":\"hookMessage\",\"content\":\"hi\",\"timestamp\":1}}";
    let mut entries = parse_session_entries(line);
    // Pre-migration, the unknown role stays verbatim (the TS type union has no
    // hookMessage, so the entry's payload is opaque there too).
    let entry = pi_coding_agent::session::SessionEntry::from_record(&entries[0]);
    assert_eq!(entry.to_record().to_line(), line);
    assert!(migrate_to_current_version(&mut entries));
    let migrated = entries[0].to_line();
    assert!(migrated.contains("\"role\":\"custom\""));
}
