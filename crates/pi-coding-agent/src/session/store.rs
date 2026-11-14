//! Append-only session JSONL store and tree navigation.
//!
//! Ground truth: `packages/coding-agent/src/core/session-manager.ts` (the
//! `SessionManager` class and its statics, `:995-2036`, plus the free
//! context-building functions `:286-603`), and `docs/session-format.md`.
//!
//! Invariants reproduced from pi:
//!
//! * directory `sessions/--<cwd, leading `/` dropped, `/ \ :` → `-` >--`
//! * filename `<ISO ts with `:` and `.` → `-`>_<uuidv7>.jsonl`
//! * compact JSON + `\n`, header first, `parentId` always present
//! * **lazy first flush**: the file is created with `O_EXCL` only once a user
//!   or assistant message exists (`_hasConversation`, `#10000`); setup-only
//!   sessions never touch disk
//! * a failed write rolls the entry back out of memory so no later entry can
//!   point at a missing parent (`_appendEntry`, `#10166`)
//! * branching = re-parenting: `branch()` only moves the leaf pointer and
//!   writes nothing; a new *file* gets a header with `parentSession`
//! * v1→v3 migrations run on load and rewrite the file when they apply

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use pi_agent::AgentMessage;
use pi_ai::types::{Model, Usage};
use serde_json::Value;

use super::migrate::migrate_to_current_version;
use super::records::{
    BranchSummaryEntry, CURRENT_SESSION_VERSION, CompactionEntry, ContextEditEntry,
    ContextReplacement, CustomEntry, CustomMessageEntry, EntryBase, JsonObject, LabelEntry,
    ModelChangeEntry, Record, SessionEntry, SessionHeader, SessionInfoEntry, SessionMessageEntry,
    TaggedSystemMessage, ThinkingLevelChangeEntry, UsageEntry, generate_entry_id,
};
use super::{SessionError, SessionResult};

/// Bound synchronous header discovery while allowing large cwd and custom
/// metadata fields (`MAX_SESSION_HEADER_SCAN_BYTES`, `:608`).
pub const MAX_SESSION_HEADER_SCAN_BYTES: usize = 1024 * 1024;

const SESSION_HEADER_READ_BUFFER_SIZE: usize = 4096;

// ---------------------------------------------------------------------------
// Time and path helpers
// ---------------------------------------------------------------------------

/// `node:path.resolve` — absolutize against cwd and lexically normalize.
/// Symlinks are *not* resolved, exactly like `path.resolve`.
pub fn resolve_path(input: &Path) -> PathBuf {
    let absolute = if input.is_absolute() {
        input.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(input)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out
}

/// `pathsEqual` (`utils/paths.ts`): compare resolved absolute paths. Windows
/// case-folding is not portable here and is omitted.
pub fn paths_equal(left: &Path, right: &Path) -> bool {
    resolve_path(left) == resolve_path(right)
}

/// `new Date().toISOString()` — `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn iso8601_millis(time: std::time::SystemTime) -> String {
    let since_epoch = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let total_millis = since_epoch.as_millis() as i64;
    let day = 86_400_000i64;
    let (days, millis_of_day) = (total_millis.div_euclid(day), total_millis.rem_euclid(day));
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 {
        yoe + era * 400 + 1
    } else {
        yoe + era * 400
    };
    format!(
        "{year:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        millis_of_day / 3_600_000,
        (millis_of_day / 60_000) % 60,
        (millis_of_day / 1000) % 60,
        millis_of_day % 1000,
    )
}

/// `Date.now()` as an ISO string.
pub fn now_iso8601() -> String {
    iso8601_millis(std::time::SystemTime::now())
}

/// `new Date(iso).getTime()` for the ISO strings pi writes. Unparsable input
/// yields `0` (TS yields `NaN`, which never compares equal or greater — `0`
/// keeps the arithmetic total-order safe).
pub fn iso_to_ms(iso: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(iso).map_or(0, |dt| dt.timestamp_millis())
}

/// pi-mono `assertValidSessionId` (`:268-274`).
pub fn assert_valid_session_id(id: &str) -> SessionResult<()> {
    let mut chars = id.chars();
    let first_ok = matches!(chars.next(), Some(first) if first.is_ascii_alphanumeric());
    // A one-character id has no separate last character.
    let last_ok = match chars.next_back() {
        Some(last) => last.is_ascii_alphanumeric(),
        None => first_ok,
    };
    let valid = first_ok
        && last_ok
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if valid {
        Ok(())
    } else {
        Err(SessionError::InvalidSessionId)
    }
}

/// `uuidv7()` session ids (`createSessionId`, `:264-266`).
///
/// The workspace pins `uuid` to the `v4` feature, so the v7 layout (48-bit
/// big-endian unix-millisecond timestamp, version 7, RFC 4122 variant) is
/// assembled here over `new_v4`'s random bytes. Matches the
/// `UUIDv7_RE` shape the TS tests pin.
pub fn create_session_id() -> String {
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        });
    let random = uuid::Uuid::new_v4();
    let mut bytes = *random.as_bytes();
    // 48-bit timestamp, then version 7, then the 10xx variant.
    for (index, byte) in unix_ms.to_be_bytes()[2..8].iter().enumerate() {
        bytes[index] = *byte;
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

// ---------------------------------------------------------------------------
// Session layout
// ---------------------------------------------------------------------------

/// Where session files live for one parent directory.
///
/// Under the XDG layout the sessions root is
/// `~/.local/share/pi-rs/sessions` (or the agent override's `sessions/`),
/// not the config root — see [`crate::config::get_sessions_dir`].
#[derive(Clone, Debug)]
pub struct SessionLayout {
    agent_dir: PathBuf,
}

impl SessionLayout {
    /// Pin the layout to an explicit parent dir (tests pass a tempdir).
    pub fn new(agent_dir: impl Into<PathBuf>) -> Self {
        Self {
            agent_dir: agent_dir.into(),
        }
    }

    /// The agent dir this layout resolves against.
    pub fn agent_dir(&self) -> &Path {
        &self.agent_dir
    }

    /// `<agentDir>/sessions` (`getSessionsDir`, `config.ts:674-676`).
    pub fn sessions_dir(&self) -> PathBuf {
        self.agent_dir.join("sessions")
    }

    /// `--${resolvedCwd.replace(/^[/\\]/, "").replace(/[/\\:]/g, "-")}--`
    /// (`getDefaultSessionDirPath`, `:590-595`).
    pub fn dir_name(cwd: &Path) -> String {
        let resolved = resolve_path(cwd);
        let text = resolved.to_string_lossy();
        let mut munged = String::with_capacity(text.len() + 4);
        munged.push_str("--");
        for (index, ch) in text.chars().enumerate() {
            if index == 0 && (ch == '/' || ch == '\\') {
                continue; // only a *leading* separator is dropped
            }
            match ch {
                '/' | '\\' | ':' => munged.push('-'),
                other => munged.push(other),
            }
        }
        munged.push_str("--");
        munged
    }

    /// The per-project session directory (not created).
    pub fn dir(&self, cwd: &Path) -> PathBuf {
        self.sessions_dir().join(Self::dir_name(cwd))
    }

    /// `${isoTimestamp.replace(/[:.]/g, "-")}_${sessionId}.jsonl` (`:1087-1088`).
    pub fn file_name(timestamp: &str, session_id: &str) -> String {
        let mut file_timestamp = String::with_capacity(timestamp.len());
        for ch in timestamp.chars() {
            match ch {
                ':' | '.' => file_timestamp.push('-'),
                other => file_timestamp.push(other),
            }
        }
        format!("{file_timestamp}_{session_id}.jsonl")
    }

    /// Full path a session started at `timestamp` with `session_id` gets.
    pub fn path_for(&self, cwd: &Path, timestamp: &str, session_id: &str) -> PathBuf {
        self.dir(cwd).join(Self::file_name(timestamp, session_id))
    }
}

/// `getDefaultSessionDirPath` (`:590-595`): encode the cwd into a safe
/// directory name under the given sessions root. Upstream builds this off
/// `getSessionsDir()`, so pass [`crate::config::get_sessions_dir`] (which
/// follows the XDG data root / both env overrides).
pub fn default_session_dir_path(cwd: &Path, sessions_dir: &Path) -> PathBuf {
    let resolved_cwd = resolve_path(cwd);
    let resolved_sessions_dir = resolve_path(sessions_dir);
    resolved_sessions_dir.join(SessionLayout::dir_name(&resolved_cwd))
}

/// `getDefaultSessionDir` (`:597-603`): create the per-cwd directory when
/// missing, then return it.
pub fn default_session_dir(cwd: &Path) -> SessionResult<PathBuf> {
    let dir = default_session_dir_path(cwd, &crate::config::get_sessions_dir());
    fs::create_dir_all(&dir).map_err(|source| SessionError::Io {
        action: "create session dir",
        path: dir.clone(),
        source,
    })?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// File IO
// ---------------------------------------------------------------------------

fn write_all_entries(file: File, entries: &[Record], path: &Path) -> SessionResult<()> {
    let mut writer = BufWriter::new(file);
    for entry in entries {
        writer
            .write_all(entry.to_line().as_bytes())
            .and_then(|()| writer.write_all(b"\n"))
            .map_err(|source| SessionError::Io {
                action: "write session file",
                path: path.to_path_buf(),
                source,
            })?;
    }
    writer.flush().map_err(|source| SessionError::Io {
        action: "flush session file",
        path: path.to_path_buf(),
        source,
    })
}

/// pi-mono `loadEntriesFromFile` (`:628-671`): skip blank and malformed lines,
/// require a valid session header, and repair a missing trailing newline.
pub fn load_entries_from_file(path: &Path) -> SessionResult<Vec<Record>> {
    let contents = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(SessionError::Io {
                action: "read session file",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let text = String::from_utf8_lossy(&contents);
    let has_trailing_newline = text.ends_with('\n');
    let entries: Vec<Record> = text.split('\n').filter_map(Record::parse_line).collect();
    if entries.is_empty() {
        return Ok(entries);
    }
    let header = &entries[0];
    if header.kind() != "session" || header.id().is_none() {
        // Not a pi session: return nothing and leave the file untouched.
        return Ok(Vec::new());
    }
    if !has_trailing_newline {
        // pi repairs the file by appending the missing newline.
        let mut file = OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|source| SessionError::Io {
                action: "append trailing newline",
                path: path.to_path_buf(),
                source,
            })?;
        file.write_all(b"\n").map_err(|source| SessionError::Io {
            action: "append trailing newline",
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(entries)
}

/// `parseSessionHeaderCandidate`'s decision (`:678-684`): keep scanning on a
/// blank or malformed line, stop without a header on a parsed non-header
/// entry, or stop with the header.
enum HeaderCandidate {
    KeepScanning,
    NotHeader,
    Header(SessionHeader),
}

fn parse_session_header_candidate(line: &str) -> HeaderCandidate {
    if line.trim().is_empty() {
        return HeaderCandidate::KeepScanning;
    }
    let Some(record) = Record::parse_line(line) else {
        return HeaderCandidate::KeepScanning;
    };
    if record.kind() != "session" || record.id().is_none() {
        return HeaderCandidate::NotHeader;
    }
    serde_json::from_value(Value::Object(record.object().clone()))
        .ok()
        .map_or(HeaderCandidate::NotHeader, HeaderCandidate::Header)
}

/// `readSessionHeader` (`:686-728`): bounded first-line discovery. Errors with
/// [`SessionError::HeaderScanLimit`] only when the limit is exceeded with more
/// data behind it.
pub fn read_session_header(path: &Path) -> SessionResult<Option<SessionHeader>> {
    let mut file = File::open(path).map_err(|source| SessionError::Io {
        action: "open session file",
        path: path.to_path_buf(),
        source,
    })?;
    let mut buffer = String::new();
    let mut chunk = vec![0u8; SESSION_HEADER_READ_BUFFER_SIZE];
    let mut scanned = 0usize;
    loop {
        let read =
            std::io::Read::read(&mut file, &mut chunk).map_err(|source| SessionError::Io {
                action: "read session header",
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            // EOF: the tail is a final line.
            return Ok(match parse_session_header_candidate(&buffer) {
                HeaderCandidate::Header(header) => Some(header),
                _ => None,
            });
        }
        scanned += read;
        buffer.push_str(&String::from_utf8_lossy(&chunk[..read]));
        while let Some(newline) = buffer.find('\n') {
            let line: String = buffer.drain(..=newline).collect();
            let line = &line[..line.len() - 1];
            match parse_session_header_candidate(line) {
                HeaderCandidate::Header(header) => return Ok(Some(header)),
                HeaderCandidate::NotHeader => return Ok(None),
                HeaderCandidate::KeepScanning => {}
            }
        }
        if scanned >= MAX_SESSION_HEADER_SCAN_BYTES {
            // Probe for EOF so a final header without a newline is allowed
            // when it ends exactly at the scan limit.
            let mut probe = [0u8; 1];
            let more =
                std::io::Read::read(&mut file, &mut probe).map_err(|source| SessionError::Io {
                    action: "read session header",
                    path: path.to_path_buf(),
                    source,
                })?;
            if more == 0 {
                return Ok(match parse_session_header_candidate(&buffer) {
                    HeaderCandidate::Header(header) => Some(header),
                    _ => None,
                });
            }
            return Err(SessionError::HeaderScanLimit(path.to_path_buf()));
        }
    }
}

/// `readSessionHeaderForDiscovery` (`:730-738`): best-effort, errors become
/// `None` so one corrupt file cannot block discovery.
fn read_session_header_for_discovery(path: &Path) -> Option<SessionHeader> {
    read_session_header(path).ok().flatten()
}

/// `sessionCwdMatches` (`:745-747`).
fn session_cwd_matches(cwd: Option<&str>, resolved_cwd: &Path) -> bool {
    match cwd {
        Some(cwd) if !cwd.is_empty() => paths_equal(Path::new(cwd), resolved_cwd),
        _ => false,
    }
}

/// `findMostRecentSession` (`:750-769`): most recently modified `.jsonl` file
/// in `session_dir` whose header matches `cwd` (when given).
pub fn find_most_recent_session(session_dir: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    let resolved_dir = resolve_path(session_dir);
    let resolved_cwd = cwd.map(resolve_path);
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = fs::read_dir(&resolved_dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|path| {
            let mtime = fs::metadata(&path).ok()?.modified().ok()?;
            Some((path, mtime))
        })
        .collect();
    files.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
    for (path, _) in files {
        let Some(header) = read_session_header_for_discovery(&path) else {
            continue;
        };
        let header_cwd = header.cwd.as_str();
        let matches = match &resolved_cwd {
            None => true,
            Some(resolved) => session_cwd_matches(Some(header_cwd), resolved),
        };
        if matches {
            return Some(path);
        }
    }
    None
}

/// `extractTextContent` (`:775-784`): the message's text blocks joined with a
/// space, or the string content itself.
fn extract_text_content(message: &AgentMessage) -> String {
    match message {
        AgentMessage::User(message) => match &message.content {
            pi_ai::types::UserContent::Text(text) => text.clone(),
            pi_ai::types::UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| block.as_text())
                .collect::<Vec<_>>()
                .join(" "),
        },
        AgentMessage::Assistant(message) => message
            .content
            .iter()
            .filter_map(|block| block.as_text())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// One step of the iterative tree assembly in `SessionManager::get_tree`.
enum Step {
    Visit(usize),
    Assemble(usize),
}

/// `SessionInfo` (`:229-243`): summary of one session file for listing.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub id: String,
    /// Working directory where the session was started. Empty for old sessions.
    pub cwd: String,
    /// User-defined display name from `session_info` entries.
    pub name: Option<String>,
    /// Path to the parent session, when this session was forked.
    pub parent_session_path: Option<String>,
    pub created: DateTime<Utc>,
    pub modified: DateTime<Utc>,
    pub message_count: usize,
    pub first_message: String,
    pub all_messages_text: String,
}

fn system_time_to_datetime(time: std::time::SystemTime) -> DateTime<Utc> {
    let millis = time
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    Utc.timestamp_millis_opt(millis)
        .single()
        .unwrap_or_default()
}

/// `buildSessionInfo` (`:800-890`), synchronous.
pub fn build_session_info(path: &Path) -> Option<SessionInfo> {
    let stats = fs::metadata(path).ok();
    let contents = fs::read_to_string(path).ok()?;
    let mut header: Option<SessionHeader> = None;
    let mut message_count = 0usize;
    let mut first_message = String::new();
    let mut all_messages: Vec<String> = Vec::new();
    let mut name: Option<String> = None;
    let mut last_activity: Option<i64> = None;

    for line in contents.lines() {
        let Some(record) = Record::parse_line(line) else {
            continue;
        };
        if header.is_none() {
            if record.kind() != "session" {
                return None;
            }
            header = Some(serde_json::from_value(Value::Object(record.object().clone())).ok()?);
            continue;
        }
        let entry = SessionEntry::from_record(&record);
        if let SessionEntry::SessionInfo(info) = &entry {
            name = info
                .name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string);
        }
        let SessionEntry::Message(message_entry) = &entry else {
            continue;
        };
        message_count += 1;
        let message = &message_entry.message;
        let activity = match message {
            AgentMessage::User(user) => Some(user.timestamp),
            AgentMessage::Assistant(assistant) => Some(assistant.timestamp),
            _ => None,
        };
        if let Some(activity) = activity {
            last_activity = Some(last_activity.map_or(activity, |last| last.max(activity)));
        }
        if matches!(
            message,
            AgentMessage::User(..) | AgentMessage::Assistant(..)
        ) {
            let text = extract_text_content(message);
            if !text.is_empty() {
                if first_message.is_empty() && matches!(message, AgentMessage::User(..)) {
                    first_message.clone_from(&text);
                }
                all_messages.push(text);
            }
        }
    }

    let header = header?;
    let created = match DateTime::parse_from_rfc3339(&header.timestamp) {
        Ok(dt) => dt.with_timezone(&Utc),
        // TS keeps an Invalid Date; the epoch keeps the ordering total.
        Err(_) => DateTime::<Utc>::default(),
    };
    let header_ms = match DateTime::parse_from_rfc3339(&header.timestamp) {
        Ok(dt) => dt.timestamp_millis(),
        Err(_) => i64::MIN,
    };
    let modified = match last_activity {
        Some(activity) if activity > 0 => Utc
            .timestamp_millis_opt(activity)
            .single()
            .unwrap_or_default(),
        _ if header_ms != i64::MIN => Utc
            .timestamp_millis_opt(header_ms)
            .single()
            .unwrap_or_default(),
        _ => stats.and_then(|stats| stats.modified().ok()).map_or_else(
            || Utc.timestamp_millis_opt(0).single().unwrap_or_default(),
            system_time_to_datetime,
        ),
    };

    Some(SessionInfo {
        path: path.to_path_buf(),
        id: header.id,
        cwd: header.cwd,
        name,
        parent_session_path: header.parent_session,
        created,
        modified,
        message_count,
        first_message: if first_message.is_empty() {
            "(no messages)".to_string()
        } else {
            first_message
        },
        all_messages_text: all_messages.join(" "),
    })
}

/// `sortSessionInfos` (`:928-930`): most recently modified first.
fn sort_session_infos(mut sessions: Vec<SessionInfo>) -> Vec<SessionInfo> {
    sessions.sort_by_key(|session| std::cmp::Reverse(session.modified));
    sessions
}

/// `listSessionsFromDir` (`:949-982`), synchronous and without progress.
pub fn list_sessions_in_dir(dir: &Path) -> Vec<SessionInfo> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    // TS sorts file names descending (`b.localeCompare(a)`); every path here
    // shares the directory, so the full path orders identically.
    files.sort_by_key(|path| std::cmp::Reverse(path.to_string_lossy().into_owned()));
    files
        .iter()
        .filter_map(|path| build_session_info(path))
        .collect()
}

// ---------------------------------------------------------------------------
// Context building (free functions, `:418-584`)
// ---------------------------------------------------------------------------

/// Reference to the model that produced the latest assistant response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub provider: String,
    pub model_id: String,
}

/// `SessionProjection` (`:216-221`).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionProjection {
    pub entries: Vec<ProjectedSessionEntry>,
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    pub model: Option<ModelRef>,
}

/// `ProjectedSessionEntry` (`:209-214`).
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedSessionEntry {
    pub source_entry: SessionEntry,
    /// Model-visible messages after context edits; empty for state-only
    /// entries and omissions.
    pub messages: Vec<AgentMessage>,
}

/// `SessionContext` (`:223-227`).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    pub model: Option<ModelRef>,
}

/// Tree node for `getTree()` (`:200-207`).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionTreeNode {
    pub entry: SessionEntry,
    pub children: Vec<SessionTreeNode>,
    /// Resolved label for this entry, if any.
    pub label: Option<String>,
    /// Timestamp of the latest label change for this entry, if any.
    pub label_timestamp: Option<String>,
}

/// `buildSessionPath` (`:390-416`). `leaf_id` mirrors the TS parameter: `None`
/// falls back to the last entry (the `undefined` case); an unknown id also
/// falls back; the caller passes no path handling for the explicit `null`
/// case, which the manager short-circuits to an empty path.
pub fn build_session_path(entries: &[SessionEntry], leaf_id: Option<&str>) -> Vec<SessionEntry> {
    let index: HashMap<&str, &SessionEntry> = entries
        .iter()
        .filter(|entry| !entry.id().is_empty())
        .map(|entry| (entry.id(), entry))
        .collect();
    let leaf = match leaf_id
        .filter(|id| !id.is_empty())
        .and_then(|id| index.get(id))
    {
        Some(leaf) => *leaf,
        None => match entries.last() {
            Some(last) => last,
            None => return Vec::new(),
        },
    };

    let mut path = Vec::new();
    let mut current = Some(leaf);
    while let Some(entry) = current {
        path.push(entry.clone());
        current = entry
            .parent_id()
            .and_then(|parent| index.get(parent))
            .copied();
    }
    path.reverse();
    path
}

/// `getSessionContextSettings` (`:418-433`).
fn session_context_settings(path: &[SessionEntry]) -> (String, Option<ModelRef>) {
    let mut thinking_level = "off".to_string();
    let mut model: Option<ModelRef> = None;
    for entry in path {
        match entry {
            SessionEntry::ThinkingLevelChange(change) => {
                thinking_level.clone_from(&change.thinking_level);
            }
            SessionEntry::ModelChange(change) => {
                model = Some(ModelRef {
                    provider: change.provider.clone(),
                    model_id: change.model_id.clone(),
                });
            }
            SessionEntry::Message(message) => {
                if let AgentMessage::Assistant(assistant) = &message.message {
                    model = Some(ModelRef {
                        provider: assistant.provider.clone(),
                        model_id: assistant.model.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    (thinking_level, model)
}

/// `sessionEntryToContextMessages` (`:439-466`): project one entry into
/// LLM/runtime messages. Plain custom entries are display/state entries and do
/// not participate in context.
pub fn session_entry_to_context_messages(entry: &SessionEntry) -> Vec<AgentMessage> {
    match entry {
        SessionEntry::Message(message) => vec![message.message.clone()],
        SessionEntry::CustomMessage(custom) => {
            vec![AgentMessage::Custom(pi_agent::CustomMessage {
                custom_type: custom.custom_type.clone(),
                content: custom.content.clone(),
                display: custom.display,
                details: custom.details.clone(),
                timestamp: iso_to_ms(&custom.base.timestamp),
            })]
        }
        // An empty summary contributes nothing (`entry.summary &&`, `:458`).
        SessionEntry::BranchSummary(summary) if !summary.summary.is_empty() => {
            vec![AgentMessage::BranchSummary(
                pi_agent::BranchSummaryMessage {
                    summary: summary.summary.clone(),
                    from_id: Some(summary.from_id.clone()),
                    timestamp: iso_to_ms(&summary.base.timestamp),
                },
            )]
        }
        SessionEntry::Compaction(compaction) => {
            let summary = AgentMessage::CompactionSummary(pi_agent::CompactionSummaryMessage {
                summary: compaction.summary.clone(),
                tokens_before: compaction.tokens_before,
                timestamp: iso_to_ms(&compaction.base.timestamp),
            });
            match &compaction.system_message {
                Some(system) => vec![AgentMessage::System(system.0.clone()), summary],
                None => vec![summary],
            }
        }
        _ => Vec::new(),
    }
}

/// `projectContextEntry` (`:520-541`): project one entry, applying its latest
/// context edit if any.
pub fn project_context_entry(
    entry: &SessionEntry,
    edit: Option<&ContextEditEntry>,
) -> Vec<AgentMessage> {
    let messages = session_entry_to_context_messages(entry);
    let Some(edit) = edit else {
        return messages;
    };
    let ContextReplacement::Replace { content } = &edit.replacement else {
        // `replacement: null` omits the target from model context.
        return Vec::new();
    };
    messages
        .into_iter()
        .map(|message| apply_content_replacement(message, content))
        .collect()
}

/// The `{ ...message, content }` spread of `projectContextEntry` (`:526-540`):
/// only user/assistant/toolResult/custom roles are replaced, and string
/// replacements for assistant and tool-result messages are normalized to one
/// text block.
fn apply_content_replacement(message: AgentMessage, replacement: &Value) -> AgentMessage {
    let role = message.role().to_string();
    if !matches!(
        role.as_str(),
        "user" | "assistant" | "toolResult" | "custom"
    ) {
        return message;
    }
    let mut value = message.to_value();
    let normalized = if matches!(role.as_str(), "assistant" | "toolResult")
        && replacement.is_string()
    {
        serde_json::json!([{ "type": "text", "text": replacement.as_str().unwrap_or_default() }])
    } else {
        replacement.clone()
    };
    if let Some(object) = value.as_object_mut() {
        object.insert("content".to_string(), normalized.clone());
    }
    AgentMessage::from_value(&value).unwrap_or_else(|_| AgentMessage::Unknown {
        role,
        raw: value.as_object().cloned().unwrap_or_default(),
    })
}

/// `getLatestCompactionEntry` (`:372-378`): the last `compaction` entry in the
/// list, or `None` when the list has none.
pub fn get_latest_compaction_entry(entries: &[SessionEntry]) -> Option<&CompactionEntry> {
    entries.iter().rev().find_map(|entry| match entry {
        SessionEntry::Compaction(compaction) => Some(compaction),
        _ => None,
    })
}

/// `buildContextEntries` (`:476-512`): the active, compaction-aware entry list.
pub fn build_context_entries_from(
    entries: &[SessionEntry],
    leaf_id: Option<&str>,
) -> Vec<SessionEntry> {
    let path = build_session_path(entries, leaf_id);
    let Some(compaction) = get_latest_compaction_entry(&path) else {
        return path;
    };
    let Some(compaction_idx) = path
        .iter()
        .position(|entry| entry.id() == compaction.base.id)
    else {
        return path;
    };

    let mut context_entries = vec![SessionEntry::Compaction(compaction.clone())];
    let mut found_first_kept = false;
    for entry in &path[..compaction_idx] {
        if entry.id() == compaction.first_kept_entry_id {
            found_first_kept = true;
        }
        let is_system_message = matches!(
            entry,
            SessionEntry::Message(message) if message.message.role() == "system"
        );
        if found_first_kept && !is_system_message {
            context_entries.push(entry.clone());
        }
    }
    context_entries.extend(path[compaction_idx + 1..].iter().cloned());
    context_entries
}

/// `buildSessionProjection` (`:544-574`): provenance-preserving, compaction
/// aware model context.
pub fn build_session_projection_from(
    entries: &[SessionEntry],
    leaf_id: Option<&str>,
) -> SessionProjection {
    let path = build_session_path(entries, leaf_id);
    let (thinking_level, model) = session_context_settings(&path);
    let context_entries = build_context_entries_from(entries, leaf_id);
    let mut edits: HashMap<String, ContextEditEntry> = HashMap::new();
    for entry in &context_entries {
        if let SessionEntry::ContextEdit(edit) = entry {
            edits.insert(edit.target_id.clone(), edit.clone());
        }
    }
    let projected_entries: Vec<ProjectedSessionEntry> = context_entries
        .into_iter()
        .enumerate()
        .map(|(index, source_entry)| {
            // An older compaction entry retained by the newest one's kept range
            // contributes no checkpoint or summary — only the newest, at index
            // zero, does (`:562-565`).
            let messages = if matches!(source_entry, SessionEntry::Compaction(..)) {
                if index == 0 {
                    session_entry_to_context_messages(&source_entry)
                } else {
                    Vec::new()
                }
            } else {
                let edit = edits.get(source_entry.id());
                project_context_entry(&source_entry, edit)
            };
            ProjectedSessionEntry {
                source_entry,
                messages,
            }
        })
        .collect();
    let messages = projected_entries
        .iter()
        .flat_map(|entry| entry.messages.iter().cloned())
        .collect();
    SessionProjection {
        entries: projected_entries,
        messages,
        thinking_level,
        model,
    }
}

/// `buildSessionContext` (`:577-584`).
pub fn build_session_context_from(
    entries: &[SessionEntry],
    leaf_id: Option<&str>,
) -> SessionContext {
    let projection = build_session_projection_from(entries, leaf_id);
    SessionContext {
        messages: projection.messages,
        thinking_level: projection.thinking_level,
        model: projection.model,
    }
}

// ---------------------------------------------------------------------------
// SessionManager
// ---------------------------------------------------------------------------

/// `NewSessionOptions` (`:52-55`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewSessionOptions {
    /// Explicit session id (validated before use).
    pub id: Option<String>,
    /// Path to the session this one was forked from.
    pub parent_session: Option<String>,
}

/// Manages conversation sessions as append-only trees stored in JSONL files
/// (`SessionManager`, `:995-2036`).
///
/// Each session entry has an id and parentId forming a tree structure. The
/// "leaf" pointer tracks the current position. Appending creates a child of
/// the current leaf. Branching moves the leaf to an earlier entry, allowing
/// new branches without modifying history.
#[derive(Debug)]
pub struct SessionManager {
    cwd: PathBuf,
    session_dir: PathBuf,
    persist: bool,
    session_id: String,
    session_file: Option<PathBuf>,
    /// All file entries, header first.
    entries: Vec<Record>,
    /// Entry id -> position in `entries` (pi's `byId` map).
    by_id: HashMap<String, usize>,
    /// Resolved labels, in first-set order (pi's `labelsById`).
    labels_by_id: HashMap<String, String>,
    label_order: Vec<String>,
    label_timestamps_by_id: HashMap<String, String>,
    leaf_id: Option<String>,
    flushed: bool,
}

impl SessionManager {
    fn construct(
        cwd: &Path,
        session_dir: &Path,
        session_file: Option<&Path>,
        persist: bool,
        options: &NewSessionOptions,
        preloaded: Option<Vec<Record>>,
    ) -> SessionResult<SessionManager> {
        let cwd = resolve_path(cwd);
        let session_dir = PathBuf::from(session_dir);
        if persist && !session_dir.as_os_str().is_empty() && !session_dir.exists() {
            fs::create_dir_all(&session_dir).map_err(|source| SessionError::Io {
                action: "create session dir",
                path: session_dir.clone(),
                source,
            })?;
        }

        let mut manager = SessionManager {
            cwd,
            session_dir,
            persist,
            session_id: String::new(),
            session_file: None,
            entries: Vec::new(),
            by_id: HashMap::new(),
            labels_by_id: HashMap::new(),
            label_order: Vec::new(),
            label_timestamps_by_id: HashMap::new(),
            leaf_id: None,
            flushed: false,
        };
        if let Some(file) = session_file {
            manager.set_session_file_with_preloaded(file, preloaded)?;
        } else if preloaded
            .as_ref()
            .is_some_and(|entries| !entries.is_empty())
        {
            manager.load_entries(preloaded.unwrap_or_default(), options)?;
        } else {
            manager.new_session(options)?;
        }
        Ok(manager)
    }

    /// `SessionManager.create(cwd, sessionDir?, options?)` (`:1773-1776`): a
    /// new persisted session. `session_dir` defaults to the per-cwd directory
    /// under [`crate::config::get_agent_dir`].
    pub fn create(
        cwd: &Path,
        session_dir: Option<&Path>,
        options: &NewSessionOptions,
    ) -> SessionResult<SessionManager> {
        let dir = match session_dir {
            Some(dir) => PathBuf::from(dir),
            None => default_session_dir(cwd)?,
        };
        Self::construct(cwd, &dir, None, true, options, None)
    }

    /// `SessionManager.open(path, sessionDir?, cwdOverride?)` (`:1784-1804`).
    pub fn open(
        path: &Path,
        session_dir: Option<&Path>,
        cwd_override: Option<&Path>,
    ) -> SessionResult<SessionManager> {
        let resolved = resolve_path(path);
        let mut preloaded: Option<Vec<Record>> = None;
        let mut header: Option<SessionHeader> = None;
        if cwd_override.is_none() && resolved.exists() {
            match read_session_header(&resolved) {
                Ok(found) => header = found,
                Err(SessionError::HeaderScanLimit(..)) => {
                    // The bounded scan is only a discovery optimization. A full
                    // load remains authoritative for legacy files with very
                    // large headers or prefixes.
                    preloaded = Some(load_entries_from_file(&resolved)?);
                    header = preloaded
                        .as_ref()
                        .and_then(|entries| entries.first())
                        .filter(|entry| entry.kind() == "session")
                        .and_then(|entry| {
                            serde_json::from_value(Value::Object(entry.object().clone())).ok()
                        });
                }
                Err(error) => return Err(error),
            }
        }
        let cwd = match cwd_override {
            Some(cwd) => resolve_path(cwd),
            None => header
                .map(|header| header.cwd)
                .filter(|cwd| !cwd.is_empty())
                .map_or_else(
                    || std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
                    PathBuf::from,
                ),
        };
        // If no session dir is provided, derive from the file's parent.
        let dir = session_dir.map_or_else(
            || resolved.parent().unwrap_or(Path::new("")).to_path_buf(),
            PathBuf::from,
        );
        Self::construct(
            &cwd,
            &dir,
            Some(&resolved),
            true,
            &NewSessionOptions::default(),
            preloaded,
        )
    }

    /// `SessionManager.continueRecent(cwd, sessionDir?)` (`:1811-1819`).
    pub fn continue_recent(
        cwd: &Path,
        session_dir: Option<&Path>,
    ) -> SessionResult<SessionManager> {
        let dir = match session_dir {
            Some(dir) => PathBuf::from(dir),
            None => default_session_dir(cwd)?,
        };
        let filter_cwd = session_dir.is_some()
            && !paths_equal(
                &dir,
                &default_session_dir_path(cwd, &crate::config::get_sessions_dir()),
            );
        let most_recent = find_most_recent_session(&dir, filter_cwd.then_some(cwd));
        Self::construct(
            cwd,
            &dir,
            most_recent.as_deref(),
            true,
            &NewSessionOptions::default(),
            None,
        )
    }

    /// `SessionManager.inMemory(cwd, options?, entries?)` (`:1822-1824`).
    pub fn in_memory(
        cwd: &Path,
        options: &NewSessionOptions,
        entries: Vec<Record>,
    ) -> SessionResult<SessionManager> {
        Self::construct(cwd, Path::new(""), None, false, options, Some(entries))
    }

    /// `SessionManager.forkFrom(sourcePath, targetCwd, sessionDir?, options?)`
    /// (`:1833-1889`): a new session in the target cwd with the full history
    /// from the source session.
    pub fn fork_from(
        source: &Path,
        target_cwd: &Path,
        session_dir: Option<&Path>,
        options: &NewSessionOptions,
    ) -> SessionResult<SessionManager> {
        let resolved_source = resolve_path(source);
        let resolved_target_cwd = resolve_path(target_cwd);
        let source_entries = load_entries_from_file(&resolved_source)?;
        if source_entries.is_empty() {
            return Err(SessionError::Fork(format!(
                "source session file is empty or invalid: {}",
                resolved_source.display()
            )));
        }
        if !source_entries.iter().any(|entry| entry.kind() == "session") {
            return Err(SessionError::Fork(format!(
                "source session has no header: {}",
                resolved_source.display()
            )));
        }

        let dir = match session_dir {
            Some(dir) => PathBuf::from(dir),
            None => default_session_dir(&resolved_target_cwd)?,
        };
        fs::create_dir_all(&dir).map_err(|source| SessionError::Io {
            action: "create session dir",
            path: dir.clone(),
            source,
        })?;

        if let Some(id) = &options.id {
            assert_valid_session_id(id)?;
        }
        let session_id = options.id.clone().unwrap_or_else(create_session_id);
        let timestamp = now_iso8601();
        let file_timestamp = timestamp.replace([':', '.'], "-");
        let session_file = dir.join(format!("{file_timestamp}_{session_id}.jsonl"));

        // New header pointing to the source as parent, with the updated cwd.
        let source = resolved_source.to_string_lossy().into_owned();
        let header = new_header_object(
            &session_id,
            &timestamp,
            &resolved_target_cwd,
            Some(source.as_str()),
        );
        let mut manager = SessionManager {
            cwd: resolved_target_cwd,
            session_dir: dir,
            persist: true,
            session_id,
            session_file: Some(session_file.clone()),
            entries: vec![header],
            by_id: HashMap::new(),
            labels_by_id: HashMap::new(),
            label_order: Vec::new(),
            label_timestamps_by_id: HashMap::new(),
            leaf_id: None,
            flushed: false,
        };
        for entry in &source_entries {
            if entry.kind() != "session" {
                manager.entries.push(entry.clone());
            }
        }
        // Write the header and all non-header entries through one file.
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&session_file)
            .map_err(|source| SessionError::Io {
                action: "create forked session file",
                path: session_file.clone(),
                source,
            })?;
        write_all_entries(file, &manager.entries, &session_file)?;
        manager.flushed = true;
        manager.build_index();
        Ok(manager)
    }

    /// `SessionManager.findById(cwd, id, sessionDir?)` (`:1897-1915`).
    pub fn find_by_id(cwd: &Path, id: &str, session_dir: Option<&Path>) -> Option<PathBuf> {
        let dir = match session_dir {
            Some(dir) => PathBuf::from(dir),
            None => default_session_dir(cwd).ok()?,
        };
        let filter_cwd = session_dir.is_some()
            && !paths_equal(
                &dir,
                &default_session_dir_path(cwd, &crate::config::get_sessions_dir()),
            );
        let resolved_cwd = resolve_path(cwd);
        let entries = fs::read_dir(&dir).ok()?;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let Some(header) = read_session_header_for_discovery(&path) else {
                continue;
            };
            if header.id != id {
                continue;
            }
            if filter_cwd && !session_cwd_matches(Some(&header.cwd), &resolved_cwd) {
                continue;
            }
            return Some(path);
        }
        None
    }

    /// `SessionManager.list(cwd, sessionDir?)` (`:1923-1938`): list the
    /// sessions of a directory, most recently active first. A custom session
    /// directory filters by the session's stored cwd.
    pub fn list(cwd: &Path, session_dir: Option<&Path>) -> Vec<SessionInfo> {
        let dir = match session_dir {
            Some(dir) => PathBuf::from(dir),
            None => match default_session_dir(cwd) {
                Ok(dir) => dir,
                Err(_) => return Vec::new(),
            },
        };
        let filter_cwd = session_dir.is_some()
            && !paths_equal(
                &dir,
                &default_session_dir_path(cwd, &crate::config::get_sessions_dir()),
            );
        let resolved_cwd = resolve_path(cwd);
        let sessions = if filter_cwd {
            list_sessions_in_dir(&dir)
                .into_iter()
                .filter(|session| session_cwd_matches(Some(&session.cwd), &resolved_cwd))
                .collect()
        } else {
            list_sessions_in_dir(&dir)
        };
        sort_session_infos(sessions)
    }

    /// `SessionManager.listAll(sessionDir?)` (`:1944-2035`): list sessions
    /// across all project directories of the agent sessions root, or of one
    /// explicit directory.
    pub fn list_all(session_dir: Option<&Path>) -> Vec<SessionInfo> {
        if let Some(dir) = session_dir {
            return sort_session_infos(list_sessions_in_dir(dir));
        }
        let sessions_root = crate::config::get_sessions_dir();
        let Ok(entries) = fs::read_dir(&sessions_root) else {
            return Vec::new();
        };
        let mut sessions = Vec::new();
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            sessions.extend(list_sessions_in_dir(&path));
        }
        sort_session_infos(sessions)
    }

    // -- lifecycle -----------------------------------------------------------

    /// `setSessionFile` (`:1032-1035`): switch to a different session file
    /// (used for resume and branching).
    pub fn set_session_file(&mut self, session_file: &Path) -> SessionResult<()> {
        self.set_session_file_with_preloaded(session_file, None)
    }

    fn set_session_file_with_preloaded(
        &mut self,
        session_file: &Path,
        preloaded: Option<Vec<Record>>,
    ) -> SessionResult<()> {
        let explicit = resolve_path(session_file);
        self.session_file = Some(explicit.clone());
        if explicit.exists() {
            let entries = match preloaded {
                Some(entries) => entries,
                None => load_entries_from_file(&explicit)?,
            };
            if entries.is_empty() {
                // If the file was empty, initialize it with a valid session
                // header. If it was non-empty but did not parse as a pi
                // session, fail without modifying it.
                if fs::metadata(&explicit).is_ok_and(|meta| meta.len() > 0) {
                    // Match TS: the path stays set and the error propagates; the
                    // caller discards the manager either way.
                    return Err(SessionError::NotASession(explicit));
                }
                self.new_session(&NewSessionOptions::default())?;
                self.session_file = Some(explicit);
                self.rewrite()?;
                self.flushed = true;
                return Ok(());
            }
            self.load_entries(entries, &NewSessionOptions::default())?;
            self.flushed = true;
        } else {
            // Preserve the explicit path from the --session flag.
            self.new_session(&NewSessionOptions::default())?;
            self.session_file = Some(explicit);
        }
        Ok(())
    }

    /// `newSession(options?)` (`:1065-1091`): a fresh header, every entry
    /// dropped, and — for a persisted store — a new file name in the same
    /// session directory. Returns the new file path when persisting.
    pub fn new_session(&mut self, options: &NewSessionOptions) -> SessionResult<Option<PathBuf>> {
        if let Some(id) = &options.id {
            assert_valid_session_id(id)?;
        }
        self.session_id = options.id.clone().unwrap_or_else(create_session_id);
        let timestamp = now_iso8601();
        let header = new_header_object(
            &self.session_id,
            &timestamp,
            &self.cwd,
            options.parent_session.as_deref(),
        );
        self.entries = vec![header];
        self.by_id.clear();
        self.labels_by_id.clear();
        self.label_order.clear();
        self.label_timestamps_by_id.clear();
        self.leaf_id = None;
        self.flushed = false;

        let mut session_file = None;
        if self.persist {
            let file_timestamp = timestamp.replace([':', '.'], "-");
            let file = self
                .session_dir
                .join(format!("{file_timestamp}_{}.jsonl", self.session_id));
            self.session_file = Some(file.clone());
            session_file = Some(file);
        }
        Ok(session_file)
    }

    fn load_entries(
        &mut self,
        entries: Vec<Record>,
        options: &NewSessionOptions,
    ) -> SessionResult<()> {
        if let Some(header) = entries.iter().find(|entry| entry.kind() == "session") {
            let session_id = header.id().unwrap_or_default().to_string();
            self.entries = entries;
            self.session_id = session_id;
            if migrate_to_current_version(&mut self.entries) {
                self.rewrite()?;
            }
        } else {
            self.new_session(options)?;
            self.entries.extend(entries);
        }
        self.build_index();
        Ok(())
    }

    /// `_buildIndex` (`:1111-1130`): `leafId` ends up as the id of the last
    /// non-header line.
    fn build_index(&mut self) {
        self.by_id.clear();
        self.labels_by_id.clear();
        self.label_order.clear();
        self.label_timestamps_by_id.clear();
        self.leaf_id = None;
        for (position, entry) in self.entries.iter().enumerate() {
            if entry.kind() == "session" {
                continue;
            }
            let id = entry.id().unwrap_or_default().to_string();
            if id.is_empty() {
                // pi sets leafId to the raw (undefined) entry id; appends then
                // start a new root.
                self.leaf_id = None;
                continue;
            }
            self.by_id.insert(id.clone(), position);
            self.leaf_id = Some(id);
            if entry.kind() == "label" {
                let target = entry.get_str("targetId").unwrap_or_default().to_string();
                // A truthy label sets it; an empty or absent one clears it.
                if let Some(label) = entry.get_str("label").filter(|label| !label.is_empty()) {
                    if !self.labels_by_id.contains_key(target.as_str()) {
                        self.label_order.push(target.clone());
                    }
                    self.labels_by_id.insert(target.clone(), label.to_string());
                    self.label_timestamps_by_id
                        .insert(target, entry.timestamp().unwrap_or_default().to_string());
                } else {
                    self.labels_by_id.remove(target.as_str());
                    self.label_timestamps_by_id.remove(target.as_str());
                    self.label_order.retain(|seen| seen != &target);
                }
            }
        }
    }

    /// `_rewriteFile` (`:1132-1142`): truncate and rewrite every entry.
    fn rewrite(&self) -> SessionResult<()> {
        if !self.persist {
            return Ok(());
        }
        let Some(path) = &self.session_file else {
            return Ok(());
        };
        let file = File::create(path).map_err(|source| SessionError::Io {
            action: "rewrite session file",
            path: path.clone(),
            source,
        })?;
        write_all_entries(file, &self.entries, path)
    }

    /// `_hasConversation` (`:1174-1178`): a session file is only created once a
    /// user or assistant message exists. Setup entries alone stay in memory so
    /// opening and closing pi without chatting leaves no file behind.
    fn has_conversation(&self) -> bool {
        self.entries.iter().any(|entry| {
            entry.kind() == "message"
                && entry
                    .get("message")
                    .and_then(|message| message.get("role"))
                    .and_then(Value::as_str)
                    .is_some_and(|role| role == "user" || role == "assistant")
        })
    }

    /// `_persist` (`:1180-1197`): lazy first flush with `O_EXCL`, plain
    /// appends afterwards.
    fn persist_last(&mut self) -> SessionResult<()> {
        if !self.persist {
            return Ok(());
        }
        let Some(path) = self.session_file.clone() else {
            return Ok(());
        };
        if !self.flushed {
            if !self.has_conversation() {
                return Ok(());
            }
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::create_dir_all(parent).map_err(|source| SessionError::Io {
                    action: "create session dir",
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|source| SessionError::Io {
                    action: "create session file",
                    path: path.clone(),
                    source,
                })?;
            let result = write_all_entries(file, &self.entries, &path);
            // pi flips `flushed` only after the whole buffer landed; a failed
            // first flush stays unflushed so the next append retries it.
            if result.is_ok() {
                self.flushed = true;
            }
            return result;
        }
        let last = self
            .entries
            .last()
            .ok_or_else(|| SessionError::Migration("no entries".to_string()))?;
        let line = last.to_line();
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|source| SessionError::Io {
                action: "append session entry",
                path: path.clone(),
                source,
            })?;
        file.write_all(line.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .map_err(|source| SessionError::Io {
                action: "append session entry",
                path: path.clone(),
                source,
            })
    }

    /// `_appendEntry` (`:1199-1214`): append, index, advance the leaf, persist.
    /// An entry that fails to write must not become the parent of later
    /// entries, so it is rolled back out of memory first.
    fn append_record(&mut self, record: Record) -> SessionResult<String> {
        let id = record.id().unwrap_or_default().to_string();
        let previous_leaf = self.leaf_id.clone();
        self.entries.push(record);
        let position = self.entries.len() - 1;
        if !id.is_empty() {
            self.by_id.insert(id.clone(), position);
        }
        self.leaf_id = (!id.is_empty()).then_some(id.clone());
        if let Err(error) = self.persist_last() {
            self.entries.pop();
            self.by_id.remove(id.as_str());
            self.leaf_id = previous_leaf;
            return Err(error);
        }
        Ok(id)
    }

    fn new_entry_base(&self) -> EntryBase {
        EntryBase {
            id: generate_entry_id(&|candidate| self.by_id.contains_key(candidate)),
            parent_id: self.leaf_id.clone(),
            timestamp: now_iso8601(),
        }
    }

    // -- appends -------------------------------------------------------------

    /// `appendMessage` (`:1222-1232`): append a message as child of the
    /// current leaf, then advance the leaf. Returns the entry id.
    pub fn append_message(&mut self, message: AgentMessage) -> SessionResult<String> {
        let entry = SessionMessageEntry {
            base: self.new_entry_base(),
            message,
            unknown: JsonObject::new(),
        };
        let id = entry.base.id.clone();
        self.append_record(SessionEntry::Message(entry).to_record())?;
        Ok(id)
    }

    /// `appendThinkingLevelChange` (`:1235-1245`).
    pub fn append_thinking_level_change(&mut self, thinking_level: &str) -> SessionResult<String> {
        let entry = ThinkingLevelChangeEntry {
            base: self.new_entry_base(),
            thinking_level: thinking_level.to_string(),
            unknown: JsonObject::new(),
        };
        let id = entry.base.id.clone();
        self.append_record(SessionEntry::ThinkingLevelChange(entry).to_record())?;
        Ok(id)
    }

    /// `appendModelChange` (`:1248-1259`).
    pub fn append_model_change(&mut self, provider: &str, model_id: &str) -> SessionResult<String> {
        let entry = ModelChangeEntry {
            base: self.new_entry_base(),
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            unknown: JsonObject::new(),
        };
        let id = entry.base.id.clone();
        self.append_record(SessionEntry::ModelChange(entry).to_record())?;
        Ok(id)
    }

    /// `appendUsage` (`:1262-1276`): append model-attributed usage that does
    /// not participate in LLM context. Returns the appended entry.
    pub fn append_usage(
        &mut self,
        kind: &str,
        provider: &str,
        model: &str,
        usage: Usage,
        note: Option<&str>,
    ) -> SessionResult<UsageEntry> {
        let entry = UsageEntry {
            base: self.new_entry_base(),
            kind: kind.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            usage,
            // `...(note ? { note } : {})`: an empty note is omitted.
            note: note.filter(|note| !note.is_empty()).map(str::to_string),
            unknown: JsonObject::new(),
        };
        self.append_record(SessionEntry::Usage(entry.clone()).to_record())?;
        Ok(entry)
    }

    /// `appendCompaction` (`:1279-1305`): append a compaction summary as child
    /// of the current leaf. Stores the complete prompt/tool checkpoint as its
    /// `systemMessage`.
    pub fn append_compaction(
        &mut self,
        summary: &str,
        first_kept_entry_id: Option<&str>,
        tokens_before: u64,
        details: Option<Value>,
        from_hook: Option<bool>,
        usage: Option<Usage>,
    ) -> SessionResult<String> {
        let timestamp = now_iso8601();
        let projection = self.build_session_projection()?;
        let system_message =
            pi_agent::current_system_message(&projection.messages).map(|mut system| {
                system.timestamp = iso_to_ms(&timestamp);
                TaggedSystemMessage(system)
            });
        let base = self.new_entry_base();
        let id = base.id.clone();
        let entry = CompactionEntry {
            base,
            summary: summary.to_string(),
            // `firstKeptEntryId: firstKeptEntryId ?? id` — a retain-none
            // compaction keeps itself.
            first_kept_entry_id: first_kept_entry_id.unwrap_or(&id).to_string(),
            tokens_before: tokens_before as f64,
            details,
            usage,
            from_hook,
            system_message,
            unknown: JsonObject::new(),
        };
        self.append_record(SessionEntry::Compaction(entry).to_record())?;
        Ok(id)
    }

    /// `AgentSession.compact` (`agent-session.ts:2936-3060`) minus the
    /// abort/event plumbing the UI owns: prepare → summarize → append.
    /// `estimatedTokensAfter` mirrors upstream's post-append projection
    /// estimate.
    #[allow(clippy::too_many_arguments)]
    pub async fn compact(
        &mut self,
        custom_instructions: Option<&str>,
        model: &Model,
        api_key: Option<&str>,
        access_token: Option<&str>,
        thinking_level: pi_agent::ThinkingLevel,
        stream_fn: &dyn pi_agent::StreamFn,
        signal: &pi_agent::AbortSignal,
        settings: super::compaction::CompactionSettings,
    ) -> Result<super::compaction::CompactionResult, super::compaction::CompactionError> {
        use super::compaction::{CompactionError, estimate_tokens, prepare_compaction};

        let path_entries = self
            .get_branch(None)
            .map_err(|error| CompactionError::Failed(error.to_string()))?;

        let already_compacted = matches!(path_entries.last(), Some(SessionEntry::Compaction(_)));
        let Some(preparation) = prepare_compaction(&path_entries, settings) else {
            // Check why we can't compact, like upstream (`:2962-2968`).
            return Err(CompactionError::Failed(
                if already_compacted {
                    "Already compacted"
                } else {
                    "Nothing to compact (session too small)"
                }
                .to_string(),
            ));
        };

        let mut result = super::compaction::compact(
            &preparation,
            model,
            api_key,
            access_token,
            custom_instructions,
            signal,
            thinking_level,
            stream_fn,
        )
        .await?;

        if signal.aborted() {
            return Err(CompactionError::Cancelled);
        }

        let details = super::compaction::details_json(&result);
        self.append_compaction(
            &result.summary,
            Some(&result.first_kept_entry_id),
            result.tokens_before,
            Some(details),
            Some(false),
            result.usage,
        )
        .map_err(|error| CompactionError::Failed(error.to_string()))?;
        result.estimated_tokens_after = self
            .build_session_projection()
            .ok()
            .map(|projection| projection.messages.iter().map(estimate_tokens).sum::<u64>());

        Ok(result)
    }

    /// `appendCustomEntry` (`:1308-1319`): append an extension state entry.
    pub fn append_custom_entry(
        &mut self,
        custom_type: &str,
        data: Option<Value>,
    ) -> SessionResult<String> {
        let entry = CustomEntry {
            custom_type: custom_type.to_string(),
            data,
            base: self.new_entry_base(),
            unknown: JsonObject::new(),
        };
        let id = entry.base.id.clone();
        self.append_record(SessionEntry::Custom(entry).to_record())?;
        Ok(id)
    }

    /// `appendSessionInfo` (`:1322-1333`): append a session info entry (e.g. a
    /// display name). Newline runs are collapsed to one space and the name is
    /// trimmed.
    pub fn append_session_info(&mut self, name: &str) -> SessionResult<String> {
        let sanitized = sanitize_session_name(name);
        let entry = SessionInfoEntry {
            base: self.new_entry_base(),
            name: Some(sanitized),
            unknown: JsonObject::new(),
        };
        let id = entry.base.id.clone();
        self.append_record(SessionEntry::SessionInfo(entry).to_record())?;
        Ok(id)
    }

    /// `appendCustomMessageEntry` (`:1357-1375`): append an extension message
    /// that participates in LLM context.
    pub fn append_custom_message_entry(
        &mut self,
        custom_type: &str,
        content: pi_ai::types::UserContent,
        display: bool,
        details: Option<Value>,
    ) -> SessionResult<String> {
        let entry = CustomMessageEntry {
            custom_type: custom_type.to_string(),
            content,
            display,
            details,
            base: self.new_entry_base(),
            unknown: JsonObject::new(),
        };
        let id = entry.base.id.clone();
        self.append_record(SessionEntry::CustomMessage(entry).to_record())?;
        Ok(id)
    }

    /// `appendContextEdit` (`:1378-1416`): append a branch-local edit to an
    /// earlier model-visible entry.
    pub fn append_context_edit(
        &mut self,
        target_id: &str,
        replacement: &ContextReplacement,
    ) -> SessionResult<String> {
        if let ContextReplacement::Replace { content } = &replacement {
            let valid = content.is_string() || content.is_array();
            if !valid {
                return Err(SessionError::InvalidContextEdit);
            }
        }
        let target_position = self
            .by_id
            .get(target_id)
            .copied()
            .ok_or_else(|| SessionError::EntryNotFound(target_id.to_string()))?;
        let target = SessionEntry::from_record(&self.entries[target_position]);
        if !self
            .get_branch(None)?
            .iter()
            .any(|entry| entry.id() == target_id)
        {
            return Err(SessionError::NotOnActiveBranch(target_id.to_string()));
        }
        let target_role = match &target {
            SessionEntry::Message(message) => message.message.role().to_string(),
            SessionEntry::CustomMessage(..) => "custom".to_string(),
            _ => {
                return Err(SessionError::NotEditableEntry(target_id.to_string()));
            }
        };
        // String replacements for assistant and tool-result targets are
        // normalized to one text block at write time (`:1400-1405`).
        let normalized = match (replacement, target_role.as_str()) {
            (ContextReplacement::Replace { content }, "assistant" | "toolResult")
                if content.is_string() =>
            {
                ContextReplacement::Replace {
                    content: serde_json::json!([{ "type": "text", "text": content.as_str().unwrap_or_default() }]),
                }
            }
            (replacement, _) => replacement.clone(),
        };
        let entry = ContextEditEntry {
            base: self.new_entry_base(),
            target_id: target_id.to_string(),
            replacement: normalized,
            unknown: JsonObject::new(),
        };
        let id = entry.base.id.clone();
        self.append_record(SessionEntry::ContextEdit(entry).to_record())?;
        Ok(id)
    }

    /// `appendLabelChange` (`:1459-1480`): set or clear a label on an entry.
    /// `None` or an empty string clears the label.
    pub fn append_label_change(
        &mut self,
        target_id: &str,
        label: Option<&str>,
    ) -> SessionResult<String> {
        if !self.by_id.contains_key(target_id) {
            return Err(SessionError::EntryNotFound(target_id.to_string()));
        }
        let entry = LabelEntry {
            base: self.new_entry_base(),
            target_id: target_id.to_string(),
            label: label.map(str::to_string),
            unknown: JsonObject::new(),
        };
        let id = entry.base.id.clone();
        let timestamp = entry.base.timestamp.clone();
        self.append_record(SessionEntry::Label(entry).to_record())?;
        if label.is_some_and(|label| !label.is_empty()) {
            if !self.labels_by_id.contains_key(target_id) {
                self.label_order.push(target_id.to_string());
            }
            self.labels_by_id
                .insert(target_id.to_string(), label.unwrap_or_default().to_string());
            self.label_timestamps_by_id
                .insert(target_id.to_string(), timestamp);
        } else {
            self.labels_by_id.remove(target_id);
            self.label_timestamps_by_id.remove(target_id);
            self.label_order.retain(|seen| seen != target_id);
        }
        Ok(id)
    }

    // -- tree traversal ------------------------------------------------------

    /// The backing JSONL file, if this session persists to disk (TS
    /// `SessionManager` exposes the same via its `path`-style state).
    pub fn session_file(&self) -> Option<&Path> {
        self.session_file.as_deref()
    }

    /// `getLeafId` (`:1422-1424`).
    pub fn get_leaf_id(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    /// `getLeafEntry` (`:1426-1428`).
    pub fn get_leaf_entry(&self) -> Option<SessionEntry> {
        self.leaf_id.as_deref().and_then(|id| self.get_entry(id))
    }

    /// `getEntry` (`:1430-1432`).
    pub fn get_entry(&self, id: &str) -> Option<SessionEntry> {
        let position = self.by_id.get(id).copied()?;
        Some(SessionEntry::from_record(&self.entries[position]))
    }

    /// `getChildren` (`:1437-1445`): all direct children of an entry, in file
    /// order.
    pub fn get_children(&self, parent_id: &str) -> Vec<SessionEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.kind() != "session" && entry.parent_id() == Some(parent_id))
            .map(SessionEntry::from_record)
            .collect()
    }

    /// `getLabel` (`:1450-1452`).
    pub fn get_label(&self, id: &str) -> Option<&str> {
        self.labels_by_id.get(id).map(String::as_str)
    }

    /// `getBranch(fromId?)` (`:1487-1497`): walk from the entry (or the leaf)
    /// to the root, returning all entries in path order. An unknown id yields
    /// an empty path, as `byId.get` returning `undefined` does in TS.
    pub fn get_branch(&self, from_id: Option<&str>) -> SessionResult<Vec<SessionEntry>> {
        let start = from_id
            .or(self.leaf_id.as_deref())
            .filter(|id| !id.is_empty());
        let Some(position) = start.and_then(|id| self.by_id.get(id).copied()) else {
            return Ok(Vec::new());
        };
        let mut path = Vec::new();
        let mut position = position;
        loop {
            let record = &self.entries[position];
            path.push(SessionEntry::from_record(record));
            match record
                .parent_id()
                .and_then(|parent| self.by_id.get(parent).copied())
            {
                Some(next) => position = next,
                None => break,
            }
        }
        path.reverse();
        Ok(path)
    }

    /// `buildContextEntries` (`:1503-1505`).
    pub fn build_context_entries(&self) -> Vec<SessionEntry> {
        match self.leaf_id.as_deref() {
            // An explicit null leaf means no active path at all.
            None => Vec::new(),
            Some(leaf) => {
                let entries = self.get_entries();
                build_context_entries_from(&entries, Some(leaf))
            }
        }
    }

    /// `buildSessionProjection` (`:1511-1513`).
    pub fn build_session_projection(&self) -> SessionResult<SessionProjection> {
        let Some(leaf) = self.leaf_id.as_deref() else {
            return Ok(SessionProjection {
                entries: Vec::new(),
                messages: Vec::new(),
                thinking_level: "off".to_string(),
                model: None,
            });
        };
        let entries = self.get_entries();
        Ok(build_session_projection_from(&entries, Some(leaf)))
    }

    /// `buildSessionContext` (`:1515-1518`).
    pub fn build_session_context(&self) -> SessionResult<SessionContext> {
        let projection = self.build_session_projection()?;
        Ok(SessionContext {
            messages: projection.messages,
            thinking_level: projection.thinking_level,
            model: projection.model,
        })
    }

    /// `getHeader` (`:1523-1526`).
    pub fn get_header(&self) -> Option<SessionHeader> {
        self.entries
            .iter()
            .find(|entry| entry.kind() == "session")
            .and_then(|entry| serde_json::from_value(Value::Object(entry.object().clone())).ok())
    }

    /// `getEntryCount` (`:1529-1531`): number of session entries, excluding
    /// the header.
    pub fn get_entry_count(&self) -> usize {
        self.by_id.len()
    }

    /// `getEntries` (`:1538-1540`): all session entries (excludes the header).
    /// The session is append-only: use the `append_*` methods to add entries
    /// and `branch` to change the leaf pointer.
    pub fn get_entries(&self) -> Vec<SessionEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.kind() != "session")
            .map(SessionEntry::from_record)
            .collect()
    }

    /// `getTree` (`:1547-1585`): the session as a tree of defensive copies.
    /// A well-formed session has exactly one root; orphaned entries are also
    /// returned as roots. Children are sorted oldest first.
    pub fn get_tree(&self) -> Vec<SessionTreeNode> {
        let entries = self.get_entries();
        // Nodes in entry order; children are linked by index and assembled
        // bottom-up so deep trees never recurse.
        let mut nodes: Vec<SessionTreeNode> = Vec::with_capacity(entries.len());
        let mut by_id: HashMap<String, usize> = HashMap::new();
        let mut children_of: Vec<Vec<usize>> = Vec::with_capacity(entries.len());
        for entry in &entries {
            let id = entry.id().to_string();
            let label = self.labels_by_id.get(&id).cloned();
            let label_timestamp = self.label_timestamps_by_id.get(&id).cloned();
            nodes.push(SessionTreeNode {
                entry: entry.clone(),
                children: Vec::new(),
                label,
                label_timestamp,
            });
            children_of.push(Vec::new());
            if !id.is_empty() {
                by_id.insert(id, nodes.len() - 1);
            }
        }
        let mut roots: Vec<usize> = Vec::new();
        for (index, node) in nodes.iter().enumerate() {
            let parent_id = node.entry.parent_id().map(str::to_string);
            match parent_id {
                // A root entry, or a self-parenting one.
                None => roots.push(index),
                Some(parent) if parent.as_str() == node.entry.id() => roots.push(index),
                Some(parent) => match by_id.get(&parent).copied() {
                    Some(parent_index) => children_of[parent_index].push(index),
                    // Orphan — treat as root.
                    None => roots.push(index),
                },
            }
        }

        // Assemble bottom-up: a node is built once all of its children are.
        let mut resolved: Vec<Option<SessionTreeNode>> = nodes.into_iter().map(Some).collect();
        let mut visited = vec![false; resolved.len()];
        let mut stack: Vec<Step> = roots
            .iter()
            .rev()
            .map(|&index| Step::Visit(index))
            .collect();
        while let Some(step) = stack.pop() {
            match step {
                Step::Visit(index) => {
                    if visited[index] {
                        continue;
                    }
                    visited[index] = true;
                    stack.push(Step::Assemble(index));
                    for &child in children_of[index].iter().rev() {
                        stack.push(Step::Visit(child));
                    }
                }
                Step::Assemble(index) => {
                    let Some(mut node) = resolved[index].take() else {
                        continue;
                    };
                    for child in children_of[index].drain(..) {
                        if let Some(child_node) = resolved[child].take() {
                            node.children.push(child_node);
                        }
                    }
                    // Sort children by timestamp (oldest first, newest at the
                    // bottom).
                    node.children
                        .sort_by_key(|child| iso_to_ms(child.entry.timestamp()));
                    resolved[index] = Some(node);
                }
            }
        }
        roots
            .iter()
            .filter_map(|&index| resolved[index].take())
            .collect()
    }

    // -- branching -----------------------------------------------------------

    /// `branch` (`:1597-1602`): start a new branch from an earlier entry by
    /// moving the leaf pointer. Existing entries are not modified.
    pub fn branch(&mut self, branch_from_id: &str) -> SessionResult<()> {
        if !self.by_id.contains_key(branch_from_id) {
            return Err(SessionError::EntryNotFound(branch_from_id.to_string()));
        }
        self.leaf_id = Some(branch_from_id.to_string());
        Ok(())
    }

    /// `resetLeaf` (`:1609-1611`): reset the leaf pointer to before any
    /// entries; the next append creates a new root.
    pub fn reset_leaf(&mut self) {
        self.leaf_id = None;
    }

    /// `branchWithSummary` (`:1618-1643`): like `branch`, but also appends a
    /// `branch_summary` entry capturing the abandoned path.
    pub fn branch_with_summary(
        &mut self,
        branch_from_id: Option<&str>,
        summary: &str,
        details: Option<Value>,
        from_hook: Option<bool>,
        usage: Option<Usage>,
    ) -> SessionResult<String> {
        if let Some(id) = branch_from_id {
            if !self.by_id.contains_key(id) {
                return Err(SessionError::EntryNotFound(id.to_string()));
            }
        }
        let from_id = self.leaf_id.clone().unwrap_or_else(|| "root".to_string());
        self.leaf_id = branch_from_id.map(str::to_string);
        let entry = BranchSummaryEntry {
            base: EntryBase {
                id: generate_entry_id(&|candidate| self.by_id.contains_key(candidate)),
                parent_id: branch_from_id.map(str::to_string),
                timestamp: now_iso8601(),
            },
            from_id,
            summary: summary.to_string(),
            details,
            usage,
            from_hook,
            unknown: JsonObject::new(),
        };
        let id = entry.base.id.clone();
        self.append_record(SessionEntry::BranchSummary(entry).to_record())?;
        Ok(id)
    }

    /// The labels `createBranchedSession` re-writes for its kept path, in
    /// first-set order with their original timestamps.
    fn labels_to_write(
        &self,
        path_entry_ids: &std::collections::HashSet<String>,
    ) -> Vec<(String, String, String)> {
        self.label_order
            .iter()
            .filter(|target| path_entry_ids.contains(*target))
            .filter_map(|target| {
                let label = self.labels_by_id.get(target)?;
                let timestamp = self.label_timestamps_by_id.get(target)?;
                Some((target.clone(), label.clone(), timestamp.clone()))
            })
            .collect()
    }

    /// `createBranchedSession`'s path filter (`:1660-1686`): drop the label
    /// entries and re-chain the rest — labels are real tree entries and later
    /// entries can be their children, so a label is replaced by the entry that
    /// follows it, which is what a compaction pointing at a label has to be
    /// remapped onto.
    fn path_without_labels(path: &[SessionEntry]) -> Vec<Record> {
        let mut replacement_by_label_id: HashMap<String, String> = HashMap::new();
        let mut pending_label_ids: Vec<String> = Vec::new();
        let mut path_parent: Option<String> = None;
        let mut kept: Vec<Record> = Vec::with_capacity(path.len());
        for entry in path {
            if entry.kind() == "label" {
                pending_label_ids.push(entry.id().to_string());
                continue;
            }
            let mut copy = entry.to_record();
            copy.set(
                "parentId",
                match &path_parent {
                    Some(parent) => Value::String(parent.clone()),
                    None => Value::Null,
                },
            );
            for label_id in pending_label_ids.drain(..) {
                if let Some(id) = copy.id() {
                    replacement_by_label_id.insert(label_id, id.to_string());
                }
            }
            if copy.kind() == "compaction" {
                if let Some(first_kept) = copy.get_str("firstKeptEntryId").map(str::to_string) {
                    // A compaction that keeps itself stays self-referencing.
                    if first_kept != copy.id().unwrap_or_default() {
                        if let Some(mapped) = replacement_by_label_id.get(&first_kept) {
                            copy.set("firstKeptEntryId", mapped.clone());
                        }
                    }
                }
            }
            path_parent = copy.id().map(str::to_string);
            kept.push(copy);
        }
        kept
    }

    /// `createBranchedSession(leafId)` (`:1650-1766`): make this session the
    /// branch ending at `leaf_id`, under a new id and — when persisting — a
    /// new file whose header points back at the file it came from. Returns the
    /// new file path, or `None` when not persisting.
    pub fn create_branched_session(&mut self, leaf_id: &str) -> SessionResult<Option<PathBuf>> {
        let path = self
            .get_branch(Some(leaf_id))
            .map_err(|_| SessionError::EntryNotFound(leaf_id.to_string()))?;
        if path.is_empty() {
            return Err(SessionError::EntryNotFound(leaf_id.to_string()));
        }

        let kept = Self::path_without_labels(&path);

        // Labels for the entries the new path keeps, in the order they were
        // first labelled (pi iterates `labelsById`, whose insertion order is
        // first-set order).
        let path_entry_ids: std::collections::HashSet<String> = kept
            .iter()
            .filter_map(|entry| entry.id().map(str::to_string))
            .collect();
        let labels_to_write = self.labels_to_write(&path_entry_ids);

        let previous_file = self.session_file.clone();
        let new_session_id = create_session_id();
        let timestamp = now_iso8601();
        let file_timestamp = timestamp.replace([':', '.'], "-");
        let new_session_file = self
            .session_dir
            .join(format!("{file_timestamp}_{new_session_id}.jsonl"));

        // pi: `parentSession: this.persist ? previousSessionFile : undefined`.
        let parent_session = if self.persist {
            previous_file
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
        } else {
            None
        };
        let header = new_header_object(
            &new_session_id,
            &timestamp,
            &self.cwd,
            parent_session.as_deref(),
        );

        let mut entries = vec![header];
        let mut used_ids: std::collections::HashSet<String> = path_entry_ids;
        entries.extend(kept);
        // The label entries are appended after the path, chained to it.
        let mut label_parent = entries
            .last()
            .and_then(|entry| entry.id())
            .map(str::to_string);
        for (target_id, label, label_timestamp) in labels_to_write {
            let mut entry = Record::new_with_kind("label");
            let id = generate_entry_id(&|candidate| used_ids.contains(candidate));
            used_ids.insert(id.clone());
            entry.set("id", id);
            entry.set(
                "parentId",
                match &label_parent {
                    Some(parent) => Value::String(parent.clone()),
                    None => Value::Null,
                },
            );
            entry.set("timestamp", label_timestamp);
            entry.set("targetId", target_id);
            entry.set("label", label);
            label_parent = entry.id().map(str::to_string);
            entries.push(entry);
        }

        self.entries = entries;
        self.session_id = new_session_id;
        self.build_index();
        if self.persist {
            self.session_file = Some(new_session_file.clone());
            // Same rule as the lazy first flush: a branch that already has a
            // conversation is written now, one that does not waits for the
            // first append.
            if self.has_conversation() {
                self.rewrite()?;
                self.flushed = true;
            } else {
                self.flushed = false;
            }
            return Ok(Some(new_session_file));
        }
        Ok(None)
    }

    // -- accessors -----------------------------------------------------------

    /// `isPersisted` (`:1144-1146`).
    pub fn is_persisted(&self) -> bool {
        self.persist
    }

    /// `getCwd` (`:1148-1150`).
    pub fn get_cwd(&self) -> &Path {
        &self.cwd
    }

    /// `getSessionDir` (`:1152-1154`).
    pub fn get_session_dir(&self) -> &Path {
        &self.session_dir
    }

    /// `usesDefaultSessionDir` (`:1156-1158`).
    pub fn uses_default_session_dir(&self) -> bool {
        paths_equal(
            &self.session_dir,
            &default_session_dir_path(&self.cwd, &crate::config::get_sessions_dir()),
        )
    }

    /// `getSessionId` (`:1160-1162`).
    pub fn get_session_id(&self) -> &str {
        &self.session_id
    }

    /// `getSessionFile` (`:1164-1166`).
    pub fn get_session_file(&self) -> Option<&Path> {
        self.session_file.as_deref()
    }

    /// `getSessionName` (`:1336-1347`): the latest `session_info` name; an
    /// empty name explicitly clears the session title.
    pub fn get_session_name(&self) -> Option<String> {
        for entry in self.entries.iter().rev() {
            if entry.kind() == "session_info" {
                return entry
                    .get_str("name")
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string);
            }
        }
        None
    }
}

/// `name.replace(/[\r\n]+/g, " ").trim()` (`appendSessionInfo`, `:1323`).
fn sanitize_session_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut in_break = false;
    for ch in name.chars() {
        if ch == '\r' || ch == '\n' {
            in_break = true;
        } else {
            if in_break {
                out.push(' ');
                in_break = false;
            }
            out.push(ch);
        }
    }
    if in_break {
        out.push(' ');
    }
    out.trim().to_string()
}

/// The header object pi writes (`newSession`, `:1071-1078`): `type`, `version`,
/// `id`, `timestamp`, `cwd`, and `parentSession` only when forked (an
/// `undefined` field is omitted by `JSON.stringify`).
fn new_header_object(
    session_id: &str,
    timestamp: &str,
    cwd: &Path,
    parent_session: Option<&str>,
) -> Record {
    let mut object = JsonObject::new();
    object.insert("type".into(), Value::String("session".into()));
    object.insert("version".into(), Value::from(CURRENT_SESSION_VERSION));
    object.insert("id".into(), Value::String(session_id.into()));
    object.insert("timestamp".into(), Value::String(timestamp.into()));
    object.insert(
        "cwd".into(),
        Value::String(cwd.to_string_lossy().into_owned()),
    );
    if let Some(parent) = parent_session {
        object.insert("parentSession".into(), Value::String(parent.into()));
    }
    Record::from_object(object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::types::{TextContent, UsageCost};

    fn user_message(text: &str) -> AgentMessage {
        AgentMessage::User(pi_ai::types::UserMessage {
            content: pi_ai::types::UserContent::Text(text.to_string()),
            timestamp: 1,
        })
    }

    fn assistant_message(text: &str) -> AgentMessage {
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
            usage: Usage {
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

    #[test]
    fn dir_name_matches_pis_munging() {
        assert_eq!(
            SessionLayout::dir_name(Path::new("/home/user")),
            "--home-user--"
        );
        assert_eq!(
            SessionLayout::dir_name(Path::new("/home/x/Downloads/best-ai-agent/euler")),
            "--home-x-Downloads-best-ai-agent-euler--"
        );
        // `path.resolve` normalizes `//` first, so only the *leading* slash is
        // ever dropped by the munging; a colon munges like a separator.
        assert_eq!(SessionLayout::dir_name(Path::new("/a//b")), "--a-b--");
        assert_eq!(SessionLayout::dir_name(Path::new("/C:/x")), "--C--x--");
    }

    #[test]
    fn file_name_only_rewrites_the_timestamp() {
        assert_eq!(
            SessionLayout::file_name(
                "2026-10-02T13:39:26.787Z",
                "01a0fcd7-59c2-7026-82aa-304c557532fa"
            ),
            "2026-10-02T13-39-26-787Z_01a0fcd7-59c2-7026-82aa-304c557532fa.jsonl"
        );
    }

    #[test]
    fn iso8601_matches_javascript() {
        assert_eq!(
            iso8601_millis(std::time::UNIX_EPOCH),
            "1970-01-01T00:00:00.000Z"
        );
        assert_eq!(
            iso8601_millis(
                std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_767_403_166_787)
            ),
            "2026-01-03T01:19:26.787Z"
        );
    }

    #[test]
    fn session_id_validation_matches_pi() {
        assert!(assert_valid_session_id("01a0fcd7-59c2-7026-82aa").is_ok());
        assert!(assert_valid_session_id("a").is_ok());
        assert!(assert_valid_session_id("abc-123_def.456").is_ok());
        assert!(assert_valid_session_id("").is_err());
        assert!(assert_valid_session_id("-abc").is_err());
        assert!(assert_valid_session_id("abc-").is_err());
        assert!(assert_valid_session_id("a/b").is_err());
    }

    #[test]
    fn lazy_first_flush_needs_a_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let mut session =
            SessionManager::create(dir.path(), Some(dir.path()), &NewSessionOptions::default())
                .unwrap();
        let _ = session
            .append_model_change("anthropic", "claude-sonnet-4-5")
            .unwrap();
        let _ = session.append_thinking_level_change("off").unwrap();
        let file = session.get_session_file().unwrap().to_path_buf();
        assert!(!file.exists(), "setup entries alone never touch disk");

        session
            .append_message(user_message("first question"))
            .unwrap();
        assert!(file.exists());
        let reopened = SessionManager::open(&file, Some(dir.path()), None).unwrap();
        assert_eq!(reopened.build_session_context().unwrap().messages.len(), 1);
    }

    #[test]
    fn failed_append_rolls_the_entry_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut session =
            SessionManager::create(dir.path(), Some(dir.path()), &NewSessionOptions::default())
                .unwrap();
        let first = session
            .append_message(user_message("first question"))
            .unwrap();
        let file = session.get_session_file().unwrap().to_path_buf();

        // Make the file read-only so the append fails.
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o444);
        }
        #[cfg(not(unix))]
        {
            permissions.set_readonly(true);
        }
        let _ = fs::set_permissions(&file, permissions);

        let result = session.append_message(assistant_message("lost answer"));
        if result.is_ok() {
            // Running privileged: the chmod guard does not apply.
            return;
        }
        assert_eq!(session.get_leaf_id(), Some(first.as_str()));
        assert_eq!(session.get_entries().len(), 1);
    }

    #[test]
    fn branch_with_summary_captures_from_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut session =
            SessionManager::create(dir.path(), Some(dir.path()), &NewSessionOptions::default())
                .unwrap();
        let _ = session.append_message(user_message("1")).unwrap();
        let _ = session.append_message(assistant_message("2")).unwrap();
        let id3 = session.append_message(user_message("3")).unwrap();

        let usage = Usage {
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
        };
        let id1 = session.get_entries()[0].id().to_string();
        let summary_id = session
            .branch_with_summary(
                Some(&id1),
                "Summary of abandoned work",
                None,
                Some(false),
                Some(usage),
            )
            .unwrap();

        assert_eq!(session.get_leaf_id(), Some(summary_id.as_str()));
        let SessionEntry::BranchSummary(entry) = session.get_entry(&summary_id).unwrap() else {
            panic!("expected branch summary");
        };
        assert_eq!(entry.from_id, id3);
        assert_eq!(entry.summary, "Summary of abandoned work");
        assert_eq!(entry.usage.map(|u| u.total_tokens), Some(100));
        assert_eq!(entry.base.parent_id, Some(id1));
    }
}
