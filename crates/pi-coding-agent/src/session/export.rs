//! Session JSONL export: serialize the current branch to a portable file.
//!
//! Ground truth: `packages/coding-agent/src/core/session-export.ts` (the
//! whole file). The `createTrailingEntries` callback parameter is not
//! carried over: its only upstream caller is `/share`'s
//! `createShareTrailingEntries` (`session-share.ts`), which pi-rs skips —
//! see the `/share` note in `modes/interactive/interactive_mode.rs`.
//!
//! Lines are written through [`crate::session::json::to_string`] so the file
//! matches `JSON.stringify` byte-for-byte, like every other session writer
//! in this crate.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::json;
use super::store::{SessionManager, resolve_path};
use super::{CURRENT_SESSION_VERSION, Record, SessionError};

/// `serializeSessionBranch` (`session-export.ts:9-29`): the freshly stamped
/// header followed by every entry on the current branch, each entry's
/// `parentId` rewritten to chain it in branch order.
pub fn serialize_session_branch(session_manager: &SessionManager) -> String {
    let timestamp = super::store::now_iso8601();
    // The header literal's key order is the serialization order
    // (`session-export.ts:14-20`).
    let mut header = serde_json::Map::new();
    header.insert("type".to_string(), Value::String("session".to_string()));
    header.insert("version".to_string(), Value::from(CURRENT_SESSION_VERSION));
    header.insert(
        "id".to_string(),
        Value::String(session_manager.get_session_id().to_string()),
    );
    header.insert("timestamp".to_string(), Value::String(timestamp));
    header.insert(
        "cwd".to_string(),
        Value::String(session_manager.get_cwd().to_string_lossy().into_owned()),
    );

    let mut entries: Vec<Value> = vec![Value::Object(header)];
    // JS `let parentId: string | null = null` — the first branch entry is
    // chained with an explicit `null`. An entry without an id yields
    // `undefined`, which `JSON.stringify` omits (`session-export.ts:22-26`).
    let mut parent_id = Some(Value::Null);
    for mut record in session_manager.get_branch_records(None) {
        match &parent_id {
            Some(value) => record.set("parentId", value.clone()),
            None => {
                record.remove("parentId");
            }
        }
        parent_id = record.id().map(|id| Value::String(id.to_string()));
        entries.push(Value::Object(record.into_object()));
    }
    let mut out = String::new();
    for entry in &entries {
        out.push_str(&json::to_string(entry));
        out.push('\n');
    }
    out
}

/// `session-${new Date().toISOString().replace(/[:.]/g, "-")}.jsonl`
/// (`session-export.ts:38`).
fn default_jsonl_export_name() -> String {
    format!(
        "session-{}.jsonl",
        super::store::now_iso8601().replace([':', '.'], "-")
    )
}

/// `exportSessionToJsonl` (`session-export.ts:32-45`): write the current
/// branch to `output_path`, defaulting to a timestamped file in the working
/// directory. Returns the resolved file path.
pub fn export_session_to_jsonl(
    session_manager: &SessionManager,
    output_path: Option<&Path>,
) -> SessionResult<PathBuf> {
    let default_name = default_jsonl_export_name();
    let file_path = resolve_path(output_path.unwrap_or(Path::new(&default_name)));
    let dir = file_path
        .parent()
        .map_or_else(PathBuf::new, Path::to_path_buf);
    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(|source| SessionError::Io {
            action: "create export directory",
            path: dir.clone(),
            source,
        })?;
    }
    let contents = serialize_session_branch(session_manager);
    std::fs::write(&file_path, contents).map_err(|source| SessionError::Io {
        action: "write exported session",
        path: file_path.clone(),
        source,
    })?;
    Ok(file_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::store::NewSessionOptions;
    use pi_agent::{AgentMessage, transcript::user_message};

    #[test]
    fn branch_export_chains_parent_ids_and_rewrites_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session =
            SessionManager::create(dir.path(), Some(dir.path()), &NewSessionOptions::default())
                .expect("session");
        let first = session
            .append_message(user_message("one", 1))
            .expect("append");
        let second = session
            .append_message(AgentMessage::User(pi_ai::types::UserMessage {
                content: pi_ai::types::UserContent::Text("two".to_string()),
                timestamp: 1,
            }))
            .expect("append");

        let serialized = serialize_session_branch(&session);
        let lines: Vec<&str> = serialized.lines().collect();
        assert_eq!(lines.len(), 3, "{serialized}");
        // The header is rebuilt with the current version, the same session
        // id, a fresh timestamp, and the session cwd — in that key order.
        assert!(
            lines[0].starts_with("{\"type\":\"session\",\"version\":3,"),
            "{}",
            lines[0]
        );
        let header: serde_json::Value = serde_json::from_str(lines[0]).expect("header json");
        assert_eq!(header["id"], session.get_session_id());
        assert_eq!(header["cwd"], dir.path().to_string_lossy().to_string());
        // Branch entries chain through `parentId`, `null` at the root.
        let first_entry: serde_json::Value = serde_json::from_str(lines[1]).expect("first json");
        assert_eq!(first_entry["parentId"], Value::Null);
        assert_eq!(first_entry["id"], first.id.as_str());
        let second_entry: serde_json::Value = serde_json::from_str(lines[2]).expect("second json");
        assert_eq!(second_entry["parentId"], first.id.as_str());
        assert_eq!(second_entry["id"], second.id.as_str());
    }

    #[test]
    fn exported_jsonl_reopens_with_the_same_session_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session =
            SessionManager::create(dir.path(), Some(dir.path()), &NewSessionOptions::default())
                .expect("session");
        session.append_message(user_message("hi", 1)).expect("append");
        let exported = dir.path().join("out").join("export.jsonl");
        let path = export_session_to_jsonl(&session, Some(&exported)).expect("export");
        assert_eq!(path, exported);
        assert!(path.is_absolute(), "{path:?}");
        let reopened = SessionManager::open(&path, None, None).expect("reopen exported session");
        assert_eq!(reopened.get_session_id(), session.get_session_id());
        assert_eq!(reopened.get_entry_count(), 1);
    }

    #[test]
    fn default_export_name_stamps_the_timestamp() {
        let name = default_jsonl_export_name();
        assert!(name.starts_with("session-"), "{name}");
        assert!(name.ends_with(".jsonl"), "{name}");
        // `replace(/[:.]/g, "-")` — no `:` or `.` survives from the ISO stamp.
        assert!(!name.contains(':') && !name.contains('.'), "{name}");
    }

    /// The canonical path: an export of a real pi-written session must
    /// reproduce the fixture's entry lines byte-for-byte (the header is
    /// rebuilt with a fresh timestamp, everything else is copied through the
    /// ordered-map records untouched).
    #[test]
    fn export_of_a_golden_session_rewrites_only_the_header() {
        let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/golden/01-plain-user-assistant.jsonl");
        let original = std::fs::read_to_string(&fixture_path).expect("fixture");
        let dir = tempfile::tempdir().expect("tempdir");
        let session = SessionManager::open(&fixture_path, Some(dir.path()), None).expect("open");

        let out = dir.path().join("exported.jsonl");
        let exported_path = export_session_to_jsonl(&session, Some(&out)).expect("export");
        let exported = std::fs::read_to_string(&exported_path).expect("exported file");

        let original_lines: Vec<&str> = original.lines().collect();
        let exported_lines: Vec<&str> = exported.lines().collect();
        assert_eq!(exported_lines.len(), original_lines.len());
        // The rebuilt header keeps the fixture's id and cwd in the same key
        // order, with only the timestamp refreshed.
        let header: serde_json::Value = serde_json::from_str(exported_lines[0]).expect("header");
        let original_header: serde_json::Value =
            serde_json::from_str(original_lines[0]).expect("fixture header");
        assert_eq!(header["id"], original_header["id"]);
        assert_eq!(header["cwd"], original_header["cwd"]);
        assert_eq!(header["version"], 3);
        assert_eq!(exported_lines[1..], original_lines[1..]);
        assert!(exported.ends_with('\n'), "pi ends the file with a newline");
    }
}

