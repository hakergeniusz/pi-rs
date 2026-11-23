//! Session migrations v1 → v2 → v3.
//!
//! Ground truth: `migrateV1ToV2` / `migrateV2ToV3` / `migrateToCurrentVersion`
//! in `packages/coding-agent/src/core/session-manager.ts:286-370` (the
//! `firstKeptEntryIndex` target lookup is `:305-306`).
//!
//! Each applied migration is followed by a full compact rewrite, so an upgraded
//! file is byte-identical to a fresh v3 write. Both migrations mutate the
//! records in place, which means *new keys are appended* exactly like a JS
//! property assignment — a v1 header gains `version` at the end, not after
//! `type`.

use super::records::{CURRENT_SESSION_VERSION, Record, generate_entry_id};

/// `header?.version ?? 1` (`migrateToCurrentVersion`, `:337-347`).
pub fn version_of(entries: &[Record]) -> u32 {
    entries
        .iter()
        .find(|entry| entry.kind() == "session")
        .and_then(|entry| entry.get("version"))
        .and_then(serde_json::Value::as_u64)
        .map_or(1, |value| value as u32)
}

/// Bring `entries` to [`CURRENT_SESSION_VERSION`]. Returns `true` when a
/// migration ran, i.e. when the caller must rewrite the file.
pub fn migrate_to_current_version(entries: &mut [Record]) -> bool {
    let version = version_of(entries);
    if version >= CURRENT_SESSION_VERSION {
        return false;
    }
    if version < 2 {
        migrate_v1_to_v2(entries);
    }
    if version < 3 {
        migrate_v2_to_v3(entries);
    }
    true
}

/// v1 → v2: add the id/parentId tree, resolve compaction `firstKeptEntryIndex`
/// (`migrateV1ToV2`, `:287-313`).
fn migrate_v1_to_v2(entries: &mut [Record]) {
    let mut ids: Vec<String> = Vec::new();
    let mut previous_id: Option<String> = None;
    // Upstream reads `entries[comp.firstKeptEntryIndex]` from the original
    // array (`:305-306`): the header is index 0, *any* existing non-header
    // target is accepted, and the id visible at that moment is the generated
    // one for an already-visited entry (including the compaction itself) or
    // the v1 file's own id for a forward reference. Snapshot kinds and any
    // pre-existing ids before the loop mutates the records.
    let original_kinds: Vec<String> = entries
        .iter()
        .map(|entry| entry.kind().to_string())
        .collect();
    let original_ids: Vec<Option<String>> = entries
        .iter()
        .map(|entry| entry.get_str("id").map(str::to_string))
        .collect();

    for (index, entry) in entries.iter_mut().enumerate() {
        if entry.kind() == "session" {
            entry.set("version", 2u32);
            continue;
        }

        let id = generate_entry_id(&|candidate| ids.iter().any(|taken| taken == candidate));
        ids.push(id.clone());
        entry.set("id", id.as_str());
        entry.set(
            "parentId",
            match &previous_id {
                Some(parent) => serde_json::Value::String(parent.clone()),
                None => serde_json::Value::Null,
            },
        );
        previous_id = Some(id.clone());

        // Convert firstKeptEntryIndex to firstKeptEntryId for compaction.
        if entry.kind() == "compaction" {
            let index_value = entry
                .get("firstKeptEntryIndex")
                .and_then(serde_json::Value::as_i64)
                .map(|value| value as usize);
            if let Some(index_value) = index_value {
                // The target must exist in the original array and must not be
                // the header. Forward references keep whatever id the v1 entry
                // had (normally none, which upstream writes as `undefined` and
                // JSON.stringify omits); backward references use the generated
                // id from `ids`, and a self-reference uses the id just assigned
                // to this compaction entry.
                let is_non_header_target = original_kinds
                    .get(index_value)
                    .is_some_and(|kind| kind != "session");
                if is_non_header_target {
                    let target_id = match index_value.cmp(&index) {
                        std::cmp::Ordering::Equal => Some(id.clone()),
                        std::cmp::Ordering::Less => index_value
                            .checked_sub(1)
                            .and_then(|slot| ids.get(slot).cloned()),
                        std::cmp::Ordering::Greater => original_ids[index_value].clone(),
                    };
                    if let Some(target_id) = target_id {
                        entry.set("firstKeptEntryId", target_id.as_str());
                    }
                }
                entry.remove("firstKeptEntryIndex");
            }
        }
    }
}

/// v2 → v3: the `hookMessage` role becomes `custom`
/// (`migrateV2ToV3`, `:316-331`).
fn migrate_v2_to_v3(entries: &mut [Record]) {
    for entry in entries.iter_mut() {
        if entry.kind() == "session" {
            entry.set("version", 3u32);
            continue;
        }
        if entry.kind() == "message"
            && let Some(message) = entry.object_mut().get_mut("message")
            && message.get("role").and_then(serde_json::Value::as_str) == Some("hookMessage")
            && let Some(object) = message.as_object_mut()
        {
            object.insert("role".into(), "custom".into());
        }
    }
}

/// `parseSessionEntries` (`:355-370`): trim, split on `\n`, skip blanks and
/// unparsable lines.
pub fn parse_session_entries(content: &str) -> Vec<Record> {
    content
        .trim()
        .lines()
        .filter_map(Record::parse_line)
        .collect()
}

/// Serialize entries the way `_rewriteFile` does: compact JSON, one `\n` each.
pub fn serialize_entries(entries: &[Record]) -> String {
    let mut out = String::new();
    for entry in entries {
        out.push_str(&entry.to_line());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &str) -> Vec<Record> {
        parse_session_entries(input)
    }

    #[test]
    fn v1_header_gains_version_at_the_end() {
        let mut entries = lines(
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
    fn hook_message_role_becomes_custom() {
        let mut entries = lines(concat!(
            "{\"type\":\"session\",\"version\":2,\"id\":\"s1\",\"timestamp\":\"t\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"id\":\"a1\",\"parentId\":null,\"timestamp\":\"t\",\"message\":{\"role\":\"hookMessage\",\"content\":\"hi\",\"timestamp\":1}}\n",
        ));
        assert!(migrate_to_current_version(&mut entries));
        let message = entries[1].get("message").unwrap();
        assert_eq!(
            message.get("role").and_then(serde_json::Value::as_str),
            Some("custom")
        );
    }

    #[test]
    fn v1_entries_get_linear_ids_and_index_becomes_id() {
        let mut entries = lines(concat!(
            "{\"type\":\"session\",\"id\":\"s1\",\"timestamp\":\"t\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"timestamp\":\"t\",\"message\":{\"role\":\"user\",\"content\":\"a\",\"timestamp\":1}}\n",
            "{\"type\":\"message\",\"timestamp\":\"t\",\"message\":{\"role\":\"user\",\"content\":\"b\",\"timestamp\":2}}\n",
            "{\"type\":\"compaction\",\"timestamp\":\"t\",\"summary\":\"s\",\"firstKeptEntryIndex\":2,\"tokensBefore\":10}\n",
        ));
        assert!(migrate_to_current_version(&mut entries));
        let first = entries[1].id().unwrap().to_string();
        let second = entries[2].id().unwrap().to_string();
        assert_eq!(entries[1].parent_id(), None);
        assert_eq!(entries[2].parent_id(), Some(first.as_str()));
        let compaction = &entries[3];
        assert_eq!(
            compaction.get_str("firstKeptEntryId"),
            Some(second.as_str())
        );
        assert!(compaction.get("firstKeptEntryIndex").is_none());
    }

    /// Upstream `entries[firstKeptEntryIndex]` accepts any existing non-header
    /// target, including a self-reference and a forward reference. A forward
    /// reference to a v1 entry (no id yet) becomes `undefined`, omitted on
    /// write; one that already carries an id copies it verbatim.
    #[test]
    fn v1_compaction_index_accepts_any_existing_non_header_target() {
        let mut entries = lines(concat!(
            "{\"type\":\"session\",\"id\":\"s1\",\"timestamp\":\"t\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"timestamp\":\"t\",\"message\":{\"role\":\"user\",\"content\":\"a\",\"timestamp\":1}}\n",
            "{\"type\":\"compaction\",\"timestamp\":\"t\",\"summary\":\"self\",\"firstKeptEntryIndex\":2,\"tokensBefore\":1}\n",
            "{\"type\":\"compaction\",\"timestamp\":\"t\",\"summary\":\"forward-no-id\",\"firstKeptEntryIndex\":4,\"tokensBefore\":2}\n",
            "{\"type\":\"message\",\"timestamp\":\"t\",\"message\":{\"role\":\"user\",\"content\":\"b\",\"timestamp\":2}}\n",
            "{\"type\":\"compaction\",\"timestamp\":\"t\",\"summary\":\"forward-with-id\",\"firstKeptEntryIndex\":6,\"tokensBefore\":3}\n",
            "{\"type\":\"message\",\"id\":\"already-had-one\",\"timestamp\":\"t\",\"message\":{\"role\":\"user\",\"content\":\"c\",\"timestamp\":3}}\n",
            "{\"type\":\"compaction\",\"timestamp\":\"t\",\"summary\":\"header\",\"firstKeptEntryIndex\":0,\"tokensBefore\":4}\n",
            "{\"type\":\"compaction\",\"timestamp\":\"t\",\"summary\":\"out-of-range\",\"firstKeptEntryIndex\":99,\"tokensBefore\":5}\n",
        ));
        assert!(migrate_to_current_version(&mut entries));
        // Index 2 is the compaction itself: upstream has already assigned its id.
        let self_id = entries[2].id().unwrap().to_string();
        assert_eq!(
            entries[2].get_str("firstKeptEntryId"),
            Some(self_id.as_str())
        );
        // Index 4 is a forward reference to an entry with no id yet.
        assert!(entries[3].get("firstKeptEntryId").is_none());
        // Index 6 is a forward reference to an entry that already had an id.
        assert_eq!(
            entries[5].get_str("firstKeptEntryId"),
            Some("already-had-one")
        );
        // Index 0 (header) and out-of-range targets are never resolved.
        assert!(entries[7].get("firstKeptEntryId").is_none());
        assert!(entries[8].get("firstKeptEntryId").is_none());
        // The index is dropped in every case.
        for index in [2, 3, 5, 7, 8] {
            assert!(entries[index].get("firstKeptEntryIndex").is_none());
        }
    }

    #[test]
    fn already_current_files_are_left_alone() {
        let mut entries = lines(
            "{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"t\",\"cwd\":\"/tmp\"}\n",
        );
        assert!(!migrate_to_current_version(&mut entries));
        assert_eq!(
            entries[0]
                .get("version")
                .and_then(serde_json::Value::as_u64),
            Some(3)
        );
    }

    #[test]
    fn v2_only_runs_the_role_rename() {
        let mut entries = lines(
            "{\"type\":\"session\",\"version\":2,\"id\":\"s1\",\"timestamp\":\"t\",\"cwd\":\"/tmp\"}\n",
        );
        assert!(migrate_to_current_version(&mut entries));
        assert_eq!(
            entries[0]
                .get("version")
                .and_then(serde_json::Value::as_u64),
            Some(3)
        );
    }
}
