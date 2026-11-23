//! Golden write-path coverage: bytes produced by *our* `SessionManager`
//! appends, compared against a file written by the real TypeScript pi.
//!
//! Ground truth: `packages/coding-agent/src/core/session-manager.ts:1215-1225`
//! (`appendMessage`) and `:1068-1091` (header creation). The reference file is
//! `tests/fixtures/golden/01-plain-user-assistant.jsonl`, committed verbatim
//! from `@earendil-works/pi-coding-agent@1.5.2` print mode. Its leading system
//! message is the *structured* shape from `system-prompt.ts:198-203`:
//! `{ role: "system", content: "", sections: { ... } }`.
//!
//! The existing golden tests only prove a load->serialize round trip of
//! TS-written bytes. Here a manager starts from that file's records minus its
//! last entry, appends the final assistant message through [`SessionManager::
//! append_message`], and the resulting bytes must equal the upstream-written
//! file. Entry ids and timestamps are generated at write time, so the expected
//! line is the upstream line with only those three volatile fields replaced by
//! the values the manager actually wrote; every other byte (key order, the
//! structured `sections` object, V8 float spelling) comes from the TS file.
//!
//! Real divergence found while writing this test: our
//! `pi_agent::transcript::create_initial_system_message` (transcript.rs:103-122)
//! emits `content: <rendered prompt>, sections: None` instead of upstream's
//! `content: "", sections: {...}`. That helper lives outside the session
//! layer, so this test builds the structured system message explicitly (by
//! seeding its TS-written record) and pins the wire shape the writer must
//! produce. The helper itself still needs a port-side fix.

use std::path::Path;

use pi_agent::AgentMessage;
use pi_ai::types::StringOrTextBlocks;
use pi_coding_agent::session::{
    NewSessionOptions, Record, SessionEntry, SessionManager, json, serialize_entries,
};
use serde_json::Value;

fn fixtures_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden")
}

fn replace_field(line: &str, key: &str, old: &Value, new: &Value) -> String {
    let from = format!("\"{key}\":{}", json::to_string(old));
    let to = format!("\"{key}\":{}", json::to_string(new));
    line.replace(&from, &to)
}

/// Replace only the volatile `id`, `parentId` and `timestamp` fields of an
/// upstream-written line with the values our manager generated.
fn retarget_upstream_line(template: &str, actual: &Record) -> String {
    let original = Record::parse_line_strict(template).unwrap();
    let mut line = template.to_string();
    for key in ["id", "parentId", "timestamp"] {
        let old = original.get(key).cloned().unwrap_or(Value::Null);
        let new = actual.get(key).cloned().unwrap_or(Value::Null);
        line = replace_field(&line, key, &old, &new);
    }
    line
}

#[test]
fn manager_append_reproduces_upstream_written_bytes() {
    let name = "01-plain-user-assistant.jsonl";
    let text = std::fs::read_to_string(fixtures_dir().join(name)).unwrap();
    let records: Vec<Record> = text
        .lines()
        .map(|line| Record::parse_line_strict(line).unwrap())
        .collect();
    let body: Vec<&str> = text.lines().skip(1).collect();
    assert_eq!(body.len(), records.len() - 1);

    // The structured system message is the TS-written shape, including six
    // insertion-ordered sections and the tools added at the first turn.
    let SessionEntry::Message(system_entry) = SessionEntry::from_record(&records[3]) else {
        panic!("expected the structured system message");
    };
    let AgentMessage::System(system) = &system_entry.message else {
        panic!("expected a system message");
    };
    assert_eq!(system.content, StringOrTextBlocks::Text(String::new()));
    let sections = system.sections.as_ref().unwrap();
    assert_eq!(sections.len(), 6);
    for name in ["preamble", "tools", "rules", "docs", "skills", "cwd"] {
        assert!(sections.contains_key(name), "missing section {name}");
    }
    // The typed view is alphabetical; the TS-written wire keeps insertion
    // order, which the byte comparison below pins end to end.
    let system_wire = records[3].to_line();
    assert!(system_wire.find("\"preamble\":").unwrap() < system_wire.find("\"tools\":").unwrap());
    assert!(system_wire.find("\"skills\":").unwrap() < system_wire.find("\"cwd\":").unwrap());

    // Seed every entry but the last, then append the final assistant message
    // through the manager's real write path (new id + current timestamp).
    let mut manager = SessionManager::in_memory(
        Path::new("/tmp/golden-cwd/01-plain"),
        &NewSessionOptions::default(),
        records[..records.len() - 1].to_vec(),
    )
    .unwrap();
    let SessionEntry::Message(last) = SessionEntry::from_record(records.last().unwrap()) else {
        panic!("expected the final assistant message");
    };
    manager.append_message(last.message).unwrap();

    let appended = manager.get_branch_records(None);
    let written = serialize_entries(&appended);
    let mut expected = String::new();
    for (index, template) in body.iter().enumerate() {
        if index + 1 == body.len() {
            expected.push_str(&retarget_upstream_line(template, &appended[index]));
        } else {
            expected.push_str(template);
        }
        expected.push('\n');
    }
    assert_eq!(
        written, expected,
        "manager append diverged from TS-written bytes"
    );

    // The appended line's parent is the previous leaf and its message keeps the
    // upstream field order (usage before responseId, thinkingLevel last).
    let last_written = appended.last().unwrap().to_line();
    assert!(last_written.contains("\"message\":{\"role\":\"assistant\",\"content\":"));
    assert!(last_written.contains("\"usage\":{"));
    assert!(
        last_written.find("\"usage\":").unwrap() < last_written.find("\"responseId\":").unwrap()
    );
    assert!(
        last_written.find("\"durationMs\":").unwrap()
            < last_written.find("\"thinkingLevel\":").unwrap()
    );
}

/// The upstream initial-system-message helper sets `content: ""` with a
/// structured `sections` map; our port's helper does not. Pin the bytes our
/// writer emits for the structured shape so a future fix to the helper is
/// checked against this exact wire form.
#[test]
fn structured_system_message_write_shape_is_pinned() {
    let name = "01-plain-user-assistant.jsonl";
    let text = std::fs::read_to_string(fixtures_dir().join(name)).unwrap();
    let system_line = text
        .lines()
        .find(|line| line.contains("\"role\":\"system\""))
        .unwrap();
    assert!(
        system_line.contains("\"message\":{\"role\":\"system\",\"content\":\"\",\"sections\":{")
    );
    let system_record = Record::parse_line_strict(system_line).unwrap();
    let manager = SessionManager::in_memory(
        Path::new("/tmp/golden-cwd/01-plain"),
        &NewSessionOptions::default(),
        vec![system_record],
    )
    .unwrap();
    let written = serialize_entries(&manager.get_branch_records(None));
    assert_eq!(written, format!("{system_line}\n"));
}
