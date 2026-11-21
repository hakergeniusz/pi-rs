//! The session record envelope and the 11 entry types, byte-compatible with
//! the upstream JSONL format.
//!
//! Ground truth: `packages/coding-agent/src/core/session-manager.ts:41-227`
//! (`SessionHeader`, `SessionEntryBase`, the entry interfaces, `SessionEntry`
//! union) and `docs/session-format.md`. Messages ride in
//! [`pi_agent::AgentMessage`], which serializes in the upstream shapes.
//!
//! # Why `Record` wraps an ordered map
//!
//! The byte-compatibility criterion is *byte-identical* re-serialization, and pi
//! writes `JSON.stringify(entry)` — key order = object literal insertion order
//! at write time, not declaration order in the TypeScript interface. Extension
//! entries in the wild put their payload first and the base fields last, and
//! `AssistantMessage` writes `usage,stopReason,timestamp,...` rather than the
//! interface order. A `#[derive(Serialize)]` struct can therefore never be
//! byte-exact for *loaded* files.
//!
//! So the canonical representation of an entry is [`Record`]: an ordered
//! `serde_json::Map` (the workspace enables `preserve_order`) serialized with
//! [`Record::to_line`] (V8 formatting via [`super::json`]). The typed structs
//! below are lossless *views*: every field pi declares is modelled, everything
//! else lands in `unknown` and comes back out untouched, and any entry whose
//! payload does not decode stays a verbatim [`SessionEntry::Other`]. That is
//! what makes unknown `type` values (extension-specific) survive a
//! load/append/rewrite cycle untouched.
//!
//! # Typed re-encoding is byte-exact too
//!
//! A `#[derive(Serialize)]` struct would emit fields in *declaration* order,
//! which diverges from pi's *write-site* order in three places:
//! `custom`/`custom_message` put their payload before the base fields, nested
//! `message` objects are built by the provider stream (`usage` before
//! `responseId`, system `timestamp` before `toolsAdded`, …), and
//! `SystemMessage.sections` is a JS object whose insertion order pi-ai flattens
//! into a `BTreeMap`. Every typed entry therefore implements a manual
//! [`Serialize`] that emits fields in the exact upstream object-literal order
//! (`session-manager.ts:1215-1312`, `1350-1367`, `1399-1407`, `1452-1464`,
//! `:1623-1633`, `agent-session.ts:1839`, `openai-completions.ts:321-337`).
//! Nested message keys are reordered by [`serialize_message_ordered`]; the
//! one thing a `BTreeMap` cannot rebuild — `sections` insertion order — is
//! captured from the wire in `unknown` under [`SECTION_ORDER_KEY`] and restored
//! on the way out. See `WIRING.md` for the golden-fixture proof.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use pi_agent::AgentMessage;
use pi_ai::types::{SystemMessage, Usage, UserContent};

use super::json;
use super::{SessionError, SessionResult};

/// Object with preserved key insertion order.
pub type JsonObject = Map<String, Value>;

// ---------------------------------------------------------------------------
// Record: the canonical, byte-exact representation
// ---------------------------------------------------------------------------

/// One physical JSONL record, order-preserving.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    object: JsonObject,
}

impl Record {
    /// An empty record.
    pub fn new() -> Self {
        Self {
            object: JsonObject::new(),
        }
    }

    /// Wrap an object that already carries the correct key order.
    pub fn from_object(object: JsonObject) -> Self {
        Self { object }
    }

    /// Borrow the ordered fields (e.g. to inspect unmodelled keys).
    pub fn object(&self) -> &JsonObject {
        &self.object
    }

    /// Mutably borrow the ordered fields.
    pub fn object_mut(&mut self) -> &mut JsonObject {
        &mut self.object
    }

    /// Consume the record into its ordered fields — the shape
    /// `JSON.stringify(entry)` serializes, key order included.
    pub fn into_object(self) -> JsonObject {
        self.object
    }

    /// Build a record from `type` plus fields, putting `type` first — matching
    /// pi's object-literal order for every entry pi itself writes.
    pub fn new_with_kind(kind: &str) -> Self {
        let mut object = JsonObject::new();
        object.insert("type".to_string(), Value::String(kind.to_string()));
        Self { object }
    }

    /// pi-mono `parseSessionEntryLine`: blank and malformed lines are skipped.
    pub fn parse_line(line: &str) -> Option<Record> {
        Self::parse_line_strict(line).ok()
    }

    /// Like [`Record::parse_line`] but surfaces the JSON error.
    pub fn parse_line_strict(line: &str) -> SessionResult<Record> {
        if line.trim().is_empty() {
            return Err(SessionError::BlankLine);
        }
        let value: Value = serde_json::from_str(line)?;
        match value {
            Value::Object(object) => Ok(Record { object }),
            other => Err(SessionError::Migration(format!(
                "record is not an object: {other}"
            ))),
        }
    }

    /// `JSON.stringify(record)` without the newline — compact, no spaces, V8
    /// number formatting (see [`super::json`]).
    pub fn to_line(&self) -> String {
        json::to_string(&Value::Object(self.object.clone()))
    }

    /// The `type` discriminator, or `""` when absent.
    pub fn kind(&self) -> &str {
        self.object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    /// The `id` field, when present and a string.
    pub fn id(&self) -> Option<&str> {
        self.object.get("id").and_then(Value::as_str)
    }

    /// `parentId` is always present on pi-written entries (`null` at the root)
    /// but older or extension-written entries have been seen without the key.
    pub fn parent_id(&self) -> Option<&str> {
        self.object.get("parentId").and_then(Value::as_str)
    }

    /// The `timestamp` field, when present and a string.
    pub fn timestamp(&self) -> Option<&str> {
        self.object.get("timestamp").and_then(Value::as_str)
    }

    /// Read an arbitrary field.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.object.get(key)
    }

    /// Read an arbitrary string field.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.object.get(key).and_then(Value::as_str)
    }

    /// Insert a key. A *new* key is appended, exactly like assigning a property
    /// on a JS object — this is what makes v1→v2 migration byte-faithful.
    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        self.object.insert(key.to_string(), value.into());
    }

    /// Insert only when `value` is `Some` (JS `undefined` means the key is
    /// omitted by `JSON.stringify`).
    pub fn set_opt(&mut self, key: &str, value: Option<impl Into<Value>>) {
        if let Some(value) = value {
            self.set(key, value.into());
        }
    }

    /// JS `delete obj.key`. The map is rebuilt so the remaining keys keep their
    /// relative order (`serde_json::Map::remove` is a swap-remove and would
    /// move the last key into the hole).
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        if !self.object.contains_key(key) {
            return None;
        }
        let mut kept = JsonObject::with_capacity(self.object.len().saturating_sub(1));
        let mut removed = None;
        for (existing, value) in std::mem::take(&mut self.object) {
            if existing == key {
                removed = Some(value);
            } else {
                kept.insert(existing, value);
            }
        }
        self.object = kept;
        removed
    }
}

impl Default for Record {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

/// `type: "session"` — line 1 of every file (`session-manager.ts:43-50`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SessionHeader {
    /// Absent means v1 (v1 sessions predate the version field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    pub id: String,
    pub timestamp: String,
    /// Tolerated as absent on hand-edited files; readers treat empty like the
    /// missing case (`getSessionHeaderCwd`, `:740-743`).
    #[serde(default)]
    pub cwd: String,
    /// Absolute path of the source file when forked; omitted otherwise.
    #[serde(
        default,
        rename = "parentSession",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_session: Option<String>,
    /// Readers ignore unknown header keys, so they must round-trip.
    #[serde(flatten)]
    pub unknown: JsonObject,
}

impl SessionHeader {
    /// `header?.version ?? 1` (`migrateToCurrentVersion`, `:337-347`).
    pub fn version_or_v1(&self) -> u32 {
        self.version.unwrap_or(1)
    }
}

/// The version pi writes (`CURRENT_SESSION_VERSION`, `:41`).
pub const CURRENT_SESSION_VERSION: u32 = 3;

// ---------------------------------------------------------------------------
// Entry base
// ---------------------------------------------------------------------------

/// Fields every non-header entry carries: `id`, `parentId`, `timestamp`
/// (`SessionEntryBase`, `:57-62`).
///
/// `id`/`timestamp` default to empty and `parentId` to `None` because legacy
/// and extension-written files in the wild omit them; serialization always
/// writes `parentId`, as `null` for a root entry.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct EntryBase {
    #[serde(default)]
    pub id: String,
    #[serde(default, rename = "parentId")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
}

impl Serialize for EntryBase {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("id", &self.id)?;
        map.serialize_entry("parentId", &self.parent_id.as_deref())?;
        map.serialize_entry("timestamp", &self.timestamp)?;
        map.end()
    }
}

// ---------------------------------------------------------------------------
// Write-site ordering (typed re-encode)
// ---------------------------------------------------------------------------

/// Reserved `unknown` key carrying the wire insertion order of a system
/// message's `sections` map. pi writes a JS object (insertion order); pi-ai
/// decodes it into a `BTreeMap`, which alphabetizes. The hint is never emitted
/// as a JSON field — it only drives the reordered `sections` value.
const SECTION_ORDER_KEY: &str = "\u{0}pi-rs.sections-order";

/// Pull the ordered list of `sections` keys captured for `which` at decode time.
fn section_hint(unknown: &JsonObject, which: &str) -> Option<Vec<String>> {
    let hints = unknown.get(SECTION_ORDER_KEY)?.as_object()?;
    let array = hints.get(which)?.as_array()?;
    Some(
        array
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    )
}

/// Capture the `sections` key order from a raw record, for both a top-level
/// `message` entry and a compaction's embedded `systemMessage`.
fn capture_section_order(record: &Record) -> Value {
    fn keys_of(value: Option<&Value>) -> Vec<String> {
        value
            .and_then(Value::as_object)
            .map(|object| object.keys().cloned().collect())
            .unwrap_or_default()
    }
    let mut hints = JsonObject::new();
    let message = keys_of(
        record
            .object()
            .get("message")
            .and_then(|message| message.get("sections")),
    );
    if !message.is_empty() {
        hints.insert(
            "message".to_string(),
            Value::Array(message.into_iter().map(Value::String).collect()),
        );
    }
    let system_message = keys_of(
        record
            .object()
            .get("systemMessage")
            .and_then(|message| message.get("sections")),
    );
    if !system_message.is_empty() {
        hints.insert(
            "systemMessage".to_string(),
            Value::Array(system_message.into_iter().map(Value::String).collect()),
        );
    }
    Value::Object(hints)
}

/// Reorder an object so `order` keys come first (in that order); every other
/// key — including extension fields — keeps its original relative order.
fn reorder_object(source: &JsonObject, order: &[&str]) -> JsonObject {
    let mut result = JsonObject::with_capacity(source.len());
    for key in order {
        if let Some(value) = source.get(*key) {
            result.insert((*key).to_string(), value.clone());
        }
    }
    for (key, value) in source {
        if !result.contains_key(key) {
            result.insert(key.clone(), value.clone());
        }
    }
    result
}

/// Restore a `sections` object's original insertion order from a captured hint.
fn reorder_sections(value: &mut Value, hint: Option<Vec<String>>) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let Some(keys) = hint else {
        return;
    };
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    *object = reorder_object(object, &refs);
}

/// Serialize an [`AgentMessage`] with the key order its upstream write site
/// produces, not the order pi-ai declares its structs in. `role` first, then
/// the provider-stream fields, then the fields appended after the stream by
/// the agent loop (`thinkingLevel` last, per `generation.ts`).
fn serialize_message_ordered(message: &AgentMessage, section_order: Option<Vec<String>>) -> Value {
    let value = message.to_value();
    let Some(object) = value.as_object() else {
        return value;
    };
    let role = object.get("role").and_then(Value::as_str).unwrap_or("");
    // `openai-completions.ts:321-337` + the stream/agent-loop assignments;
    // `agent-session.ts:1839` for system; `toolResult` from the golden lines.
    let order: &[&str] = match role {
        "system" => &[
            "role",
            "content",
            "sections",
            "timestamp",
            "toolsAdded",
            "toolsRemoved",
        ],
        "user" => &["role", "content", "timestamp"],
        "assistant" => &[
            "role",
            "content",
            "api",
            "provider",
            "model",
            "usage",
            "stopReason",
            "timestamp",
            "responseId",
            "responseModel",
            "providerThinkingLevel",
            "diagnostics",
            "errorMessage",
            "rawStopReason",
            "endTurn",
            "durationMs",
            "thinkingLevel",
            "deferred",
        ],
        "toolResult" => &[
            "role",
            "toolCallId",
            "toolName",
            "content",
            "details",
            "usage",
            "nestedCalls",
            "isError",
            "durationMs",
            "timestamp",
        ],
        // Unknown roles have no upstream shape to mirror; keep pi-agent's order.
        _ => return value,
    };
    let mut reordered = reorder_object(object, order);
    if role == "system" {
        if let (Some(sections), Some(hint)) = (reordered.get_mut("sections"), section_order) {
            reorder_sections(sections, Some(hint));
        }
    }
    Value::Object(reordered)
}

/// `id`, `parentId`, `timestamp` in upstream base-field order (`null` parent).
fn base_fields(base: &EntryBase) -> [(&'static str, Value); 3] {
    [
        ("id", Value::String(base.id.clone())),
        (
            "parentId",
            base.parent_id.clone().map_or(Value::Null, Value::String),
        ),
        ("timestamp", Value::String(base.timestamp.clone())),
    ]
}

/// Append the order-preserving `unknown` tail, skipping the discriminator
/// (always emitted first) and the private section-order hint.
fn finish_entry(mut object: JsonObject, unknown: &JsonObject) -> JsonObject {
    for (key, value) in unknown {
        if key == "type" || key == SECTION_ORDER_KEY || object.contains_key(key) {
            continue;
        }
        object.insert(key.clone(), value.clone());
    }
    object
}

/// Entry shape for the nine types whose payload follows `EntryBase`
/// (`session-manager.ts:1215-1305`, `:1357-1473`, `:1623-1633`).
fn base_then_payload(
    kind: &str,
    base: &EntryBase,
    fields: &[(&str, Value)],
    unknown: &JsonObject,
) -> JsonObject {
    let mut object = JsonObject::new();
    object.insert("type".to_string(), Value::String(kind.to_string()));
    for (key, value) in base_fields(base) {
        object.insert(key.to_string(), value);
    }
    for (key, value) in fields {
        object.insert((*key).to_string(), value.clone());
    }
    finish_entry(object, unknown)
}

/// Entry shape for `custom` and `custom_message`, whose payload comes *before*
/// the base fields (`session-manager.ts:1302-1309`, `:1356-1365`).
fn payload_then_base(
    kind: &str,
    fields: &[(&str, Value)],
    base: &EntryBase,
    unknown: &JsonObject,
) -> JsonObject {
    let mut object = JsonObject::new();
    object.insert("type".to_string(), Value::String(kind.to_string()));
    for (key, value) in fields {
        object.insert((*key).to_string(), value.clone());
    }
    for (key, value) in base_fields(base) {
        object.insert(key.to_string(), value);
    }
    finish_entry(object, unknown)
}

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

/// `type: "message"` — the only entry carrying LLM content
/// (`SessionMessageEntry`, `:64-67`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SessionMessageEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    #[serde(deserialize_with = "deserialize_tolerant_message")]
    pub message: AgentMessage,
    #[serde(flatten)]
    pub unknown: JsonObject,
}

/// `type: "thinking_level_change"` (`:69-72`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThinkingLevelChangeEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    #[serde(rename = "thinkingLevel")]
    pub thinking_level: String,
    #[serde(flatten)]
    pub unknown: JsonObject,
}

/// `type: "model_change"` (`:74-78`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ModelChangeEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub provider: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
    #[serde(flatten)]
    pub unknown: JsonObject,
}

/// `type: "usage"` — model-attributed usage that is not an assistant message
/// and does not participate in LLM context (`:80-89`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct UsageEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    /// Arbitrary usage category, such as `cache_warm`.
    pub kind: String,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    /// Optional human-readable qualifier for usage notices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(flatten)]
    pub unknown: JsonObject,
}

/// `type: "compaction"` — created when context is compacted (`:91-104`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CompactionEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub summary: String,
    /// First entry retained from before the compaction; a retain-none
    /// compaction stores its own id here.
    #[serde(rename = "firstKeptEntryId")]
    pub first_kept_entry_id: String,
    #[serde(rename = "tokensBefore")]
    pub tokens_before: f64,
    /// Extension-specific data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Usage from the LLM call(s) that generated this summary, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// True if generated by an extension, `false`/absent if pi-generated.
    #[serde(default, rename = "fromHook", skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
    /// Complete prompt and tool state at this compaction boundary.
    #[serde(
        default,
        rename = "systemMessage",
        skip_serializing_if = "Option::is_none"
    )]
    pub system_message: Option<TaggedSystemMessage>,
    #[serde(flatten)]
    pub unknown: JsonObject,
}

/// `type: "branch_summary"` — created when switching branches via `/tree`
/// (`:106-116`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BranchSummaryEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    /// The previous leaf whose abandoned path was summarized, or `root`.
    #[serde(rename = "fromId")]
    pub from_id: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, rename = "fromHook", skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
    #[serde(flatten)]
    pub unknown: JsonObject,
}

/// `type: "custom"` — extension state persistence; does NOT participate in LLM
/// context (`:128-132`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CustomEntry {
    #[serde(rename = "customType")]
    pub custom_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(flatten)]
    pub base: EntryBase,
    #[serde(flatten)]
    pub unknown: JsonObject,
}

/// `type: "custom_message"` — extension-injected messages that DO participate
/// in LLM context (`:159-165`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CustomMessageEntry {
    #[serde(rename = "customType")]
    pub custom_type: String,
    /// `string | (TextContent | ImageContent)[]`.
    #[serde(deserialize_with = "deserialize_tolerant_content")]
    pub content: UserContent,
    pub display: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(flatten)]
    pub base: EntryBase,
    #[serde(flatten)]
    pub unknown: JsonObject,
}

/// `type: "context_edit"` — append-only edit of one earlier context-producing
/// entry (`:175-180`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ContextEditEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    #[serde(rename = "targetId")]
    pub target_id: String,
    /// `null` omits the target from model context; a value replaces only its
    /// content.
    pub replacement: ContextReplacement,
    #[serde(flatten)]
    pub unknown: JsonObject,
}

/// The `replacement` payload of a [`ContextEditEntry`] (`ContextEditableContent`,
/// `:168-180`): `null` or `{ content: string | blocks }`. The content value is
/// kept raw — its concrete shape is validated and normalized per target role at
/// edit/projection time, exactly like the TS spread does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContextReplacement {
    /// JSON `null`: omit the target from model context.
    Omit,
    /// Replace the target message content.
    Replace { content: Value },
}

/// `type: "label"` — user-defined bookmark/marker on an entry (`:135-139`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LabelEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    #[serde(rename = "targetId")]
    pub target_id: String,
    /// `undefined`/absent clears the target's label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(flatten)]
    pub unknown: JsonObject,
}

/// `type: "session_info"` — session metadata, e.g. a user-defined display name
/// (`:142-145`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SessionInfoEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(flatten)]
    pub unknown: JsonObject,
}

// -- Manual `Serialize` impls: upstream write-site key order -----------------
//
// Each impl emits the exact object-literal order from the append* sites in
// `session-manager.ts`, then appends any extension keys from `unknown` in their
// original order. A derived impl would use declaration order instead, which is
// not byte-identical (notably for `custom`/`custom_message`).

impl Serialize for SessionHeader {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut object = JsonObject::new();
        object.insert("type".to_string(), Value::String("session".to_string()));
        if let Some(version) = self.version {
            object.insert("version".to_string(), Value::from(version));
        }
        object.insert("id".to_string(), Value::String(self.id.clone()));
        object.insert(
            "timestamp".to_string(),
            Value::String(self.timestamp.clone()),
        );
        object.insert("cwd".to_string(), Value::String(self.cwd.clone()));
        if let Some(parent) = &self.parent_session {
            object.insert("parentSession".to_string(), Value::String(parent.clone()));
        }
        for (key, value) in &self.unknown {
            if key == "type" || object.contains_key(key) {
                continue;
            }
            object.insert(key.clone(), value.clone());
        }
        Value::Object(object).serialize(serializer)
    }
}

/// Serialize a field value, falling back to `null` only for a non-serializable
/// value (impossible for the concrete field types here, but keeps the manual
/// impls total rather than panicking).
fn field<T: Serialize + ?Sized>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or_default()
}

impl Serialize for SessionMessageEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let hint = section_hint(&self.unknown, "message");
        let message = serialize_message_ordered(&self.message, hint);
        base_then_payload(
            "message",
            &self.base,
            &[("message", message)],
            &self.unknown,
        )
        .serialize(serializer)
    }
}

impl Serialize for ThinkingLevelChangeEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        base_then_payload(
            "thinking_level_change",
            &self.base,
            &[("thinkingLevel", field(&self.thinking_level))],
            &self.unknown,
        )
        .serialize(serializer)
    }
}

impl Serialize for ModelChangeEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        base_then_payload(
            "model_change",
            &self.base,
            &[
                ("provider", field(&self.provider)),
                ("modelId", field(&self.model_id)),
            ],
            &self.unknown,
        )
        .serialize(serializer)
    }
}

impl Serialize for UsageEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = vec![
            ("kind", field(&self.kind)),
            ("provider", field(&self.provider)),
            ("model", field(&self.model)),
            ("usage", field(&self.usage)),
        ];
        if let Some(note) = &self.note {
            fields.push(("note", field(note)));
        }
        base_then_payload("usage", &self.base, &fields, &self.unknown).serialize(serializer)
    }
}

impl Serialize for CompactionEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = vec![
            ("summary", field(&self.summary)),
            ("firstKeptEntryId", field(&self.first_kept_entry_id)),
            ("tokensBefore", field(&self.tokens_before)),
        ];
        if let Some(details) = &self.details {
            fields.push(("details", details.clone()));
        }
        if let Some(usage) = &self.usage {
            fields.push(("usage", field(usage)));
        }
        if let Some(from_hook) = self.from_hook {
            fields.push(("fromHook", Value::Bool(from_hook)));
        }
        if let Some(system) = &self.system_message {
            let hint = section_hint(&self.unknown, "systemMessage");
            let ordered = serialize_message_ordered(&AgentMessage::System(system.0.clone()), hint);
            fields.push(("systemMessage", ordered));
        }
        base_then_payload("compaction", &self.base, &fields, &self.unknown).serialize(serializer)
    }
}

impl Serialize for BranchSummaryEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = vec![
            ("fromId", field(&self.from_id)),
            ("summary", field(&self.summary)),
        ];
        if let Some(details) = &self.details {
            fields.push(("details", details.clone()));
        }
        if let Some(usage) = &self.usage {
            fields.push(("usage", field(usage)));
        }
        if let Some(from_hook) = self.from_hook {
            fields.push(("fromHook", Value::Bool(from_hook)));
        }
        base_then_payload("branch_summary", &self.base, &fields, &self.unknown)
            .serialize(serializer)
    }
}

impl Serialize for CustomEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = vec![("customType", field(&self.custom_type))];
        if let Some(data) = &self.data {
            fields.push(("data", data.clone()));
        }
        payload_then_base("custom", &fields, &self.base, &self.unknown).serialize(serializer)
    }
}

impl Serialize for CustomMessageEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = vec![
            ("customType", field(&self.custom_type)),
            ("content", field(&self.content)),
            ("display", Value::Bool(self.display)),
        ];
        if let Some(details) = &self.details {
            fields.push(("details", details.clone()));
        }
        payload_then_base("custom_message", &fields, &self.base, &self.unknown)
            .serialize(serializer)
    }
}

impl Serialize for ContextEditEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        base_then_payload(
            "context_edit",
            &self.base,
            &[
                ("targetId", field(&self.target_id)),
                ("replacement", field(&self.replacement)),
            ],
            &self.unknown,
        )
        .serialize(serializer)
    }
}

impl Serialize for LabelEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = vec![("targetId", field(&self.target_id))];
        if let Some(label) = &self.label {
            fields.push(("label", field(label)));
        }
        base_then_payload("label", &self.base, &fields, &self.unknown).serialize(serializer)
    }
}

impl Serialize for SessionInfoEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = Vec::new();
        if let Some(name) = &self.name {
            fields.push(("name", field(name)));
        }
        base_then_payload("session_info", &self.base, &fields, &self.unknown).serialize(serializer)
    }
}

/// A [`SystemMessage`] that serializes with its `role: "system"` tag first, as
/// pi writes it into a compaction entry (`appendCompaction`, `:1279-1305`).
#[derive(Debug, Clone, PartialEq)]
pub struct TaggedSystemMessage(pub SystemMessage);

impl Serialize for TaggedSystemMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // No wire hint here (the value is built fresh); callers serializing a
        // decoded compaction use `serialize_message_ordered` directly so the
        // captured `sections` order is honoured.
        serialize_message_ordered(&AgentMessage::System(self.0.clone()), None).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TaggedSystemMessage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // pi-ai's `SystemMessage` ignores unknown keys, so a leading `role` on
        // the wire is dropped here and re-added on serialization.
        SystemMessage::deserialize(deserializer).map(TaggedSystemMessage)
    }
}

// ---------------------------------------------------------------------------
// SessionEntry: the typed union over a record
// ---------------------------------------------------------------------------

/// A session entry — the typed union (`SessionEntry`, `:183-194`).
///
/// Every known `type` decodes into its variant (tolerantly: unknown keys are
/// kept, and a payload that does not match keeps the record verbatim as
/// [`SessionEntry::Other`], which serializes byte-identically).
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEntry {
    Message(SessionMessageEntry),
    ThinkingLevelChange(ThinkingLevelChangeEntry),
    ModelChange(ModelChangeEntry),
    Usage(UsageEntry),
    Compaction(CompactionEntry),
    BranchSummary(BranchSummaryEntry),
    Custom(CustomEntry),
    CustomMessage(CustomMessageEntry),
    ContextEdit(ContextEditEntry),
    Label(LabelEntry),
    SessionInfo(SessionInfoEntry),
    /// An entry pi does not model (extension-specific `type`), or one whose
    /// payload did not decode — kept verbatim.
    Other(Record),
}

impl SessionEntry {
    /// Decode a typed view of `record`, keeping it verbatim when the `type` is
    /// unknown or the payload does not fit.
    pub fn from_record(record: &Record) -> SessionEntry {
        let kind = record.kind();
        // `type` is consumed by the discriminator, so decode the payload
        // without it; unknown keys land in the `unknown` bag.
        let value = Value::Object(record.object().clone());
        let decoded: Option<SessionEntry> = match kind {
            "message" => serde_json::from_value(value)
                .ok()
                .map(SessionEntry::Message),
            "thinking_level_change" => serde_json::from_value(value)
                .ok()
                .map(SessionEntry::ThinkingLevelChange),
            "model_change" => serde_json::from_value(value)
                .ok()
                .map(SessionEntry::ModelChange),
            "usage" => serde_json::from_value(value).ok().map(SessionEntry::Usage),
            "compaction" => serde_json::from_value(value)
                .ok()
                .map(SessionEntry::Compaction),
            "branch_summary" => serde_json::from_value(value)
                .ok()
                .map(SessionEntry::BranchSummary),
            "custom" => serde_json::from_value(value).ok().map(SessionEntry::Custom),
            "custom_message" => serde_json::from_value(value)
                .ok()
                .map(SessionEntry::CustomMessage),
            "context_edit" => serde_json::from_value(value)
                .ok()
                .map(SessionEntry::ContextEdit),
            "label" => serde_json::from_value(value).ok().map(SessionEntry::Label),
            "session_info" => serde_json::from_value(value)
                .ok()
                .map(SessionEntry::SessionInfo),
            _ => None,
        };
        let mut decoded = decoded.unwrap_or_else(|| SessionEntry::Other(record.clone()));
        // pi-ai's BTreeMap loses the JS insertion order of `sections`. Stash the
        // wire order in `unknown`; the manual serializer restores it on the way
        // out so a load→typed→serialize cycle is byte-identical.
        if let Some(unknown) = entry_unknown_mut(&mut decoded) {
            let hint = capture_section_order(record);
            if hint.as_object().is_some_and(|object| !object.is_empty()) {
                unknown.insert(SECTION_ORDER_KEY.to_string(), hint);
            }
        }
        decoded
    }

    /// Encode back to the canonical record. Unknown/undecodable entries keep
    /// their verbatim bytes via [`SessionEntry::Other`]; known entries are
    /// re-encoded by their manual [`Serialize`] impls in upstream write-site
    /// order, with the captured `sections` insertion order restored. A
    /// load→typed→serialize cycle is therefore byte-identical to the raw
    /// [`Record`] path (proven against the golden fixtures).
    pub fn to_record(&self) -> Record {
        match self {
            SessionEntry::Other(record) => record.clone(),
            other => {
                let (kind, value) = match other {
                    SessionEntry::Message(entry) => ("message", serde_json::to_value(entry)),
                    SessionEntry::ThinkingLevelChange(entry) => {
                        ("thinking_level_change", serde_json::to_value(entry))
                    }
                    SessionEntry::ModelChange(entry) => {
                        ("model_change", serde_json::to_value(entry))
                    }
                    SessionEntry::Usage(entry) => ("usage", serde_json::to_value(entry)),
                    SessionEntry::Compaction(entry) => ("compaction", serde_json::to_value(entry)),
                    SessionEntry::BranchSummary(entry) => {
                        ("branch_summary", serde_json::to_value(entry))
                    }
                    SessionEntry::Custom(entry) => ("custom", serde_json::to_value(entry)),
                    SessionEntry::CustomMessage(entry) => {
                        ("custom_message", serde_json::to_value(entry))
                    }
                    SessionEntry::ContextEdit(entry) => {
                        ("context_edit", serde_json::to_value(entry))
                    }
                    SessionEntry::Label(entry) => ("label", serde_json::to_value(entry)),
                    SessionEntry::SessionInfo(entry) => {
                        ("session_info", serde_json::to_value(entry))
                    }
                    SessionEntry::Other(..) => unreachable!("handled above"),
                };
                let Ok(Value::Object(fields)) = value else {
                    return Record::new_with_kind(kind);
                };
                let mut record = Record::new_with_kind(kind);
                for (key, value) in fields {
                    record.object_mut().insert(key, value);
                }
                record
            }
        }
    }

    /// The `type` discriminator.
    pub fn kind(&self) -> &str {
        match self {
            SessionEntry::Message(..) => "message",
            SessionEntry::ThinkingLevelChange(..) => "thinking_level_change",
            SessionEntry::ModelChange(..) => "model_change",
            SessionEntry::Usage(..) => "usage",
            SessionEntry::Compaction(..) => "compaction",
            SessionEntry::BranchSummary(..) => "branch_summary",
            SessionEntry::Custom(..) => "custom",
            SessionEntry::CustomMessage(..) => "custom_message",
            SessionEntry::ContextEdit(..) => "context_edit",
            SessionEntry::Label(..) => "label",
            SessionEntry::SessionInfo(..) => "session_info",
            SessionEntry::Other(record) => record.kind(),
        }
    }

    /// The entry id, or `""` when absent (matches TS's `entry.id` for tree
    /// indexing purposes).
    pub fn id(&self) -> &str {
        match self {
            SessionEntry::Other(record) => record.id().unwrap_or(""),
            entry => entry_base(entry).map_or("", |base| base.id.as_str()),
        }
    }

    /// The entry's parent id (`null`/absent on a root entry).
    pub fn parent_id(&self) -> Option<&str> {
        match self {
            SessionEntry::Other(record) => record.parent_id(),
            entry => entry_base(entry).and_then(|base| base.parent_id.as_deref()),
        }
    }

    /// The entry's ISO-8601 timestamp, or `""` when absent.
    pub fn timestamp(&self) -> &str {
        match self {
            SessionEntry::Other(record) => record.timestamp().unwrap_or(""),
            entry => entry_base(entry).map_or("", |base| base.timestamp.as_str()),
        }
    }

    /// The stored [`AgentMessage`], for `message` entries.
    pub fn as_message(&self) -> Option<&AgentMessage> {
        match self {
            SessionEntry::Message(entry) => Some(&entry.message),
            _ => None,
        }
    }
}

/// The extension/unknown key bag of a typed entry, for carrying the wire
/// `sections` order hint across a decode/encode cycle.
fn entry_unknown_mut(entry: &mut SessionEntry) -> Option<&mut JsonObject> {
    match entry {
        SessionEntry::Message(e) => Some(&mut e.unknown),
        SessionEntry::ThinkingLevelChange(e) => Some(&mut e.unknown),
        SessionEntry::ModelChange(e) => Some(&mut e.unknown),
        SessionEntry::Usage(e) => Some(&mut e.unknown),
        SessionEntry::Compaction(e) => Some(&mut e.unknown),
        SessionEntry::BranchSummary(e) => Some(&mut e.unknown),
        SessionEntry::Custom(e) => Some(&mut e.unknown),
        SessionEntry::CustomMessage(e) => Some(&mut e.unknown),
        SessionEntry::ContextEdit(e) => Some(&mut e.unknown),
        SessionEntry::Label(e) => Some(&mut e.unknown),
        SessionEntry::SessionInfo(e) => Some(&mut e.unknown),
        SessionEntry::Other(..) => None,
    }
}

fn entry_base(entry: &SessionEntry) -> Option<&EntryBase> {
    match entry {
        SessionEntry::Message(e) => Some(&e.base),
        SessionEntry::ThinkingLevelChange(e) => Some(&e.base),
        SessionEntry::ModelChange(e) => Some(&e.base),
        SessionEntry::Usage(e) => Some(&e.base),
        SessionEntry::Compaction(e) => Some(&e.base),
        SessionEntry::BranchSummary(e) => Some(&e.base),
        SessionEntry::Custom(e) => Some(&e.base),
        SessionEntry::CustomMessage(e) => Some(&e.base),
        SessionEntry::ContextEdit(e) => Some(&e.base),
        SessionEntry::Label(e) => Some(&e.base),
        SessionEntry::SessionInfo(e) => Some(&e.base),
        SessionEntry::Other(..) => None,
    }
}

// ---------------------------------------------------------------------------
// Tolerant message decoding
// ---------------------------------------------------------------------------

/// Decode a wire message the way the TS session manager treats file content:
/// unvalidated. Known roles whose `content` is `null`/missing get the empty
/// default (`sessionEntryToContextMessages`, `:439-451`), and anything that
/// still does not fit a typed role is kept verbatim as the unknown role.
pub fn tolerant_agent_message(value: &Value) -> AgentMessage {
    if let Ok(message) = AgentMessage::from_value(value) {
        return message;
    }
    let mut copy = value.clone();
    let Some(object) = copy.as_object_mut() else {
        return AgentMessage::Unknown {
            role: String::new(),
            raw: JsonObject::new(),
        };
    };
    let role = object
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let content_missing = object.get("content").is_none_or(Value::is_null);
    if content_missing {
        let default = match role.as_str() {
            "system" => Value::String(String::new()),
            "user" | "assistant" | "toolResult" => Value::Array(Vec::new()),
            _ => {
                return AgentMessage::Unknown {
                    role,
                    raw: object.clone(),
                };
            }
        };
        object.insert("content".to_string(), default);
    }
    let raw = object.clone();
    AgentMessage::from_value(&copy).unwrap_or_else(move |_| AgentMessage::Unknown { role, raw })
}

fn deserialize_tolerant_message<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<AgentMessage, D::Error> {
    Ok(tolerant_agent_message(&Value::deserialize(deserializer)?))
}

/// `entry.content ?? []` for custom-message entries (`:455-457`): a missing or
/// null content decodes to an empty block list instead of failing.
fn deserialize_tolerant_content<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<UserContent, D::Error> {
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(UserContent::Blocks(Vec::new()));
    }
    UserContent::deserialize(value).map_err(serde::de::Error::custom)
}

/// `generateId` (`:277-284`): 8 lowercase hex chars from a UUID prefix,
/// collision-checked, with a full-UUID fallback after 100 tries.
pub fn generate_entry_id(taken: &dyn Fn(&str) -> bool) -> String {
    for _ in 0..100 {
        let candidate = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        if !taken(&candidate) {
            return candidate;
        }
    }
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::types::UsageCost;

    #[test]
    fn unknown_keys_and_order_survive_a_view_roundtrip() {
        // Shape seen from extension-written entries: payload first, base last.
        let line = r#"{"type":"custom","customType":"caveman-mode","data":{"enabled":true},"id":"dece980c","parentId":"85cef923","timestamp":"2026-10-02T16:42:21.140Z","weird":{"a":[1,2]}}"#;
        let record = Record::parse_line_strict(line).unwrap();
        assert!(matches!(
            SessionEntry::from_record(&record),
            SessionEntry::Custom(CustomEntry { .. })
        ));
        assert_eq!(
            SessionEntry::from_record(&record).to_record().to_line(),
            line
        );
    }

    #[test]
    fn set_appends_like_a_js_property_assignment() {
        let mut record = Record::new_with_kind("message");
        record.set("timestamp", "t");
        record.set("id", "abc");
        assert_eq!(
            record.to_line(),
            r#"{"type":"message","timestamp":"t","id":"abc"}"#
        );
    }

    #[test]
    fn remove_keeps_the_relative_order_of_remaining_keys() {
        let mut record = Record::new_with_kind("compaction");
        record.set("tokensBefore", 10);
        record.set("firstKeptEntryIndex", 2);
        record.set("summary", "s");
        record.remove("firstKeptEntryIndex");
        assert_eq!(
            record.to_line(),
            r#"{"type":"compaction","tokensBefore":10,"summary":"s"}"#
        );
    }

    #[test]
    fn unknown_entry_types_are_kept_verbatim() {
        let line =
            r#"{"type":"ext-thing","payload":[1],"id":"a1","parentId":null,"timestamp":"t"}"#;
        let record = Record::parse_line_strict(line).unwrap();
        let entry = SessionEntry::from_record(&record);
        assert_eq!(entry.kind(), "ext-thing");
        assert_eq!(entry.id(), "a1");
        assert_eq!(entry.parent_id(), None);
        assert_eq!(entry.to_record().to_line(), line);
    }

    #[test]
    fn message_entries_round_trip_through_the_typed_view() {
        let line = concat!(
            r#"{"type":"message","id":"b2c3d4e5","parentId":"a1b2c3d4","timestamp":"2024-12-03T14:00:02.000Z","message":{"#,
            r#""role":"assistant","content":[{"type":"text","text":"Hi!"}],"api":"anthropic-messages","provider":"anthropic","#,
            r#""model":"claude-sonnet-4-5","usage":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":3,"#,
            r#""cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":1733234402000}}"#
        );
        let record = Record::parse_line_strict(line).unwrap();
        let entry = SessionEntry::from_record(&record);
        assert_eq!(entry.to_record().to_line(), line);
        let SessionEntry::Message(message) = entry else {
            panic!("expected message entry");
        };
        assert_eq!(message.base.id, "b2c3d4e5");
        assert_eq!(message.message.role(), "assistant");
    }

    #[test]
    fn messages_with_null_content_decode_with_the_empty_default() {
        let line = r#"{"type":"message","id":"1","parentId":null,"timestamp":"t","message":{"role":"user","content":null,"timestamp":1}}"#;
        let record = Record::parse_line_strict(line).unwrap();
        let SessionEntry::Message(entry) = SessionEntry::from_record(&record) else {
            panic!("expected message entry");
        };
        assert_eq!(
            entry.message.to_value()["content"],
            Value::Array(Vec::new())
        );
        // The canonical record is untouched: still `null` on the wire.
        assert_eq!(record.to_line(), line);
    }

    #[test]
    fn entry_base_serializes_parent_id_null_for_roots() {
        let entry = CustomEntry {
            custom_type: "x".to_string(),
            data: Some(serde_json::json!({"count": 42})),
            base: EntryBase {
                id: "h8i9j0k1".into(),
                parent_id: None,
                timestamp: "t".into(),
            },
            unknown: JsonObject::new(),
        };
        let record = SessionEntry::Custom(entry).to_record();
        assert_eq!(
            record.to_line(),
            r#"{"type":"custom","customType":"x","data":{"count":42},"id":"h8i9j0k1","parentId":null,"timestamp":"t"}"#
        );
    }

    #[test]
    fn compaction_system_message_keeps_the_role_tag() {
        let message = TaggedSystemMessage(SystemMessage {
            content: pi_ai::types::StringOrTextBlocks::Text("You are pi.".into()),
            sections: None,
            tools_added: None,
            tools_removed: None,
            timestamp: 17_332_350_000_000,
        });
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["role"], "system");
        assert_eq!(value["content"], "You are pi.");
        let decoded: TaggedSystemMessage = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn context_replacement_null_and_object_decode() {
        let omitted: ContextReplacement = serde_json::from_str("null").unwrap();
        assert_eq!(omitted, ContextReplacement::Omit);
        let replaced: ContextReplacement =
            serde_json::from_str(r#"{"content":"new text"}"#).unwrap();
        assert_eq!(
            replaced,
            ContextReplacement::Replace {
                content: Value::String("new text".into())
            }
        );
        assert_eq!(
            serde_json::to_value(&ContextReplacement::Omit).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn generate_entry_id_is_eight_hex_chars() {
        let id = generate_entry_id(&|_| false);
        assert_eq!(id.len(), 8);
        assert!(
            id.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn usage_entry_view_decodes_pi_usage_shapes() {
        let line = concat!(
            r#"{"type":"usage","id":"f6g7h8i9","parentId":null,"timestamp":"2024-12-03T14:08:00.000Z","#,
            r#""kind":"cache_warm","provider":"anthropic","model":"claude-sonnet-4-5","#,
            r#""usage":{"input":0,"output":0,"cacheRead":50000,"cacheWrite":0,"totalTokens":50000,"#,
            r#""cost":{"input":0,"output":0,"cacheRead":0.015,"cacheWrite":0,"total":0.015}},"note":"n"}"#
        );
        let record = Record::parse_line_strict(line).unwrap();
        let SessionEntry::Usage(entry) = SessionEntry::from_record(&record) else {
            panic!("expected usage entry");
        };
        assert_eq!(entry.kind, "cache_warm");
        assert_eq!(entry.usage.cache_read, 50000);
        assert_eq!(
            entry.usage.cost,
            UsageCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.015,
                cache_write: 0.0,
                total: 0.015,
                total_from_provider: None
            }
        );
        assert_eq!(entry.note.as_deref(), Some("n"));
        assert_eq!(SessionEntry::Usage(entry).to_record().to_line(), line);
    }

    // -- byte-exact typed re-encode (write-site order) -----------------------

    /// Every golden line — including the nested `message` objects and their
    /// `sections` maps — must survive load→typed→serialize byte-identically.
    /// This is the finding recorded in HANDOFF "Golden session fixtures".
    #[test]
    fn golden_lines_are_byte_exact_through_the_typed_view() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/golden");
        let fixtures = [
            "01-plain-user-assistant.jsonl",
            "02-tool-read-round-trip.jsonl",
            "03-model-change-thinking-low.jsonl",
            "04-multi-turn-with-name.jsonl",
            "05-resume-thinking-level-change.jsonl",
        ];
        let mut checked = 0usize;
        for name in fixtures {
            let path = std::path::Path::new(dir).join(name);
            let text = std::fs::read_to_string(&path).unwrap();
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let record = Record::parse_line_strict(line).unwrap();
                let typed = SessionEntry::from_record(&record);
                assert_eq!(
                    typed.to_record().to_line(),
                    record.to_line(),
                    "typed view changed a line in {name}"
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 38, "all lines of the five golden fixtures");
    }

    #[test]
    fn system_sections_keep_wire_insertion_order_not_alphabetical() {
        let line = concat!(
            r#"{"type":"message","id":"a","parentId":null,"timestamp":"t","message":{"#,
            r#""role":"system","content":"","sections":{"zed":"z","alpha":"a","mid":"m"},"#,
            r#""timestamp":1}}"#
        );
        let record = Record::parse_line_strict(line).unwrap();
        // The decoded BTreeMap alphabetizes; the wire hint must restore order.
        let SessionEntry::Message(entry) = SessionEntry::from_record(&record) else {
            panic!("expected message entry");
        };
        let AgentMessage::System(system) = &entry.message else {
            panic!("expected system message");
        };
        let sections = system.sections.as_ref().unwrap();
        assert_eq!(
            sections.keys().map(String::as_str).collect::<Vec<_>>(),
            ["alpha", "mid", "zed"]
        );
        assert_eq!(SessionEntry::Message(entry).to_record().to_line(), line);
    }

    #[test]
    fn assistant_message_uses_write_site_order_not_declaration_order() {
        let line = concat!(
            r#"{"type":"message","id":"b","parentId":"a","timestamp":"t","message":{"#,
            r#""role":"assistant","content":[{"type":"text","text":"4"}],"#,
            r#""api":"openai-completions","provider":"llama-swap","model":"m","#,
            r#""usage":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":3,"#,
            r#""cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"#,
            r#""stopReason":"stop","timestamp":1,"responseId":"id","responseModel":"model","#,
            r#""rawStopReason":"stop","durationMs":12,"thinkingLevel":"off"}}"#
        );
        let record = Record::parse_line_strict(line).unwrap();
        assert_eq!(
            SessionEntry::from_record(&record).to_record().to_line(),
            line
        );
    }

    #[test]
    fn tool_result_message_uses_upstream_field_order() {
        let line = concat!(
            r#"{"type":"message","id":"c","parentId":"b","timestamp":"t","message":{"#,
            r#""role":"toolResult","toolCallId":"tc","toolName":"read","#,
            r#""content":[{"type":"text","text":"ok"}],"isError":false,"#,
            r#""durationMs":3,"timestamp":7}}"#
        );
        let record = Record::parse_line_strict(line).unwrap();
        assert_eq!(
            SessionEntry::from_record(&record).to_record().to_line(),
            line
        );
    }

    #[test]
    fn custom_entries_keep_payload_before_base_fields() {
        for line in [
            r#"{"type":"custom","customType":"ct","data":{"n":1},"id":"d","parentId":null,"timestamp":"t","extra":true}"#,
            r#"{"type":"custom_message","customType":"ct","content":"hi","display":true,"details":{"x":1},"id":"e","parentId":"d","timestamp":"t"}"#,
        ] {
            let record = Record::parse_line_strict(line).unwrap();
            assert_eq!(
                SessionEntry::from_record(&record).to_record().to_line(),
                line
            );
        }
    }

    #[test]
    fn freshly_built_entries_emit_write_site_order_without_a_wire_hint() {
        let entry = SessionMessageEntry {
            base: EntryBase {
                id: "fresh".into(),
                parent_id: None,
                timestamp: "t".into(),
            },
            message: AgentMessage::System(SystemMessage {
                content: pi_ai::types::StringOrTextBlocks::Text(String::new()),
                sections: None,
                tools_added: None,
                tools_removed: None,
                timestamp: 1,
            }),
            unknown: JsonObject::new(),
        };
        let line = SessionEntry::Message(entry).to_record().to_line();
        // `timestamp` before `toolsAdded` is the upstream system write order;
        // no private hint key may leak into the serialized JSON.
        assert_eq!(
            line,
            r#"{"type":"message","id":"fresh","parentId":null,"timestamp":"t","message":{"role":"system","content":"","timestamp":1}}"#
        );
        assert!(!line.contains("pi-rs"));
    }

    #[test]
    fn compaction_system_message_sections_keep_wire_order() {
        let line = concat!(
            r#"{"type":"compaction","id":"g","parentId":null,"timestamp":"t","#,
            r#""summary":"s","firstKeptEntryId":"g","tokensBefore":10,"#,
            r#""systemMessage":{"role":"system","content":"","sections":{"cwd":"/x","preamble":"p"},"#,
            r#""timestamp":1}}"#
        );
        let record = Record::parse_line_strict(line).unwrap();
        assert_eq!(
            SessionEntry::from_record(&record).to_record().to_line(),
            line
        );
    }
}
