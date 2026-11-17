//! Session migrations v1 → v2 → v3.
//!
//! Ground truth: `migrateV1ToV2` / `migrateV2ToV3` / `migrateToCurrentVersion`
//! in `packages/coding-agent/src/core/session-manager.ts:286-370`.
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
        previous_id = Some(id);

        // Convert firstKeptEntryIndex to firstKeptEntryId for compaction.
        if entry.kind() == "compaction" {
            let index_value = entry
                .get("firstKeptEntryIndex")
                .and_then(serde_json::Value::as_i64)
                .map(|value| value as usize);
            if let Some(index_value) = index_value {
                // pi indexes `entries` (header included) and only converts when
                // the target exists and is not the header. Reading the target
                // from the array while iterating it mutably is why the id is
                // taken from `ids` instead.
                let target_id = (index_value > 0 && index_value < index)
                    .then(|| ids.get(index_value - 1).cloned())
                    .flatten();
                if let Some(target_id) = target_id {
                    entry.set("firstKeptEntryId", target_id);
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
