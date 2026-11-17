//! The JSONL session tree: records, append/load, branching, and migrations.
//!
//! Ground truth: `packages/coding-agent/src/core/session-manager.ts` (entry
//! types, `SessionManager`, context building, migrations) and
//! `docs/session-format.md` (the wire format).
//!
//! Entries form a tree via `id`/`parentId`; the "leaf" pointer tracks the
//! current position and `branch` moves it without rewriting history. Sessions
//! load from and persist to
//! `~/.local/share/pi-rs/sessions/--<cwd>--/<ts>_<id>.jsonl` under the XDG
//! defaults (upstream: `~/.pi/agent/sessions/...`; override the root with
//! `PI_CODING_AGENT_DIR` / `PI_CODING_AGENT_SESSION_DIR` via [`crate::config`]).
//!
//! Layout:
//!
//! * [`records`] — the canonical [`records::Record`] envelope (ordered map,
//!   byte-exact re-serialization) and the typed [`records::SessionEntry`]
//!   views over pi's 11 entry types.
//! * [`migrate`] — v1 → v2 → v3 migrations, run on load with a rewrite.
//! * [`store`] — [`store::SessionManager`], the per-cwd session layout, file
//!   loading/header discovery, listing, and the context-building functions.
//! * [`json`] — the `JSON.stringify`-compatible writer that keeps rewritten
//!   files byte-identical to what pi writes.
//! * [`cwd`] — the stored-cwd existence gate (`core/session-cwd.ts`).
//! * [`export`] — branch export to a portable JSONL file
//!   (`core/session-export.ts`).
//! * [`export_html`] — the self-contained HTML viewer export
//!   (`core/export-html/`, templates embedded byte-identically).

#![forbid(unsafe_code)]

use std::path::PathBuf;

pub mod branch_summarization;
pub mod cache_stats;
pub mod compaction;
pub mod cwd;
pub mod export;
pub mod export_html;
pub mod json;
pub mod migrate;
pub mod records;
pub mod store;

pub use migrate::{
    migrate_to_current_version, parse_session_entries, serialize_entries, version_of,
};
pub use records::{
    BranchSummaryEntry, CURRENT_SESSION_VERSION, CompactionEntry, ContextEditEntry,
    ContextReplacement, CustomEntry, CustomMessageEntry, EntryBase, LabelEntry, ModelChangeEntry,
    Record, SessionEntry, SessionHeader, SessionInfoEntry, SessionMessageEntry,
    TaggedSystemMessage, ThinkingLevelChangeEntry, UsageEntry,
};
pub use store::{
    ModelRef, NewSessionOptions, ProjectedSessionEntry, SessionContext, SessionInfo, SessionLayout,
    SessionManager, SessionProjection, SessionTreeNode, assert_valid_session_id,
    build_context_entries_from, build_session_context_from, build_session_path,
    build_session_projection_from, create_session_id, default_session_dir,
    default_session_dir_path, find_most_recent_session, get_latest_compaction_entry, iso_to_ms,
    iso8601_millis, list_sessions_in_dir, load_entries_from_file, now_iso8601, paths_equal,
    project_context_entry, read_session_header, resolve_path, session_entry_to_context_messages,
};

/// Errors the session layer reports. Messages match the ones the TS
/// implementation throws, which tests pin.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Filesystem failure, with the action and path for the message.
    #[error("{action} failed for {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A file exists, is non-empty, and does not parse as a pi session.
    #[error("Session file is not a valid pi session: {0}")]
    NotASession(PathBuf),
    /// `assertValidSessionId` (`session-manager.ts:268-274`).
    #[error(
        "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character"
    )]
    InvalidSessionId,
    /// `Entry ${id} not found` (`branch`, `appendLabelChange`, ...).
    #[error("Entry {0} not found")]
    EntryNotFound(String),
    /// `appendContextEdit` on a target off the active branch.
    #[error("Entry {0} is not on the active branch")]
    NotOnActiveBranch(String),
    /// `appendContextEdit` on a target that contributes no editable content.
    #[error("Entry {0} does not contribute editable model content")]
    NotEditableEntry(String),
    /// `appendContextEdit` with a replacement that is neither null nor an
    /// object with string/array content.
    #[error("Context edit replacement must be null or contain string/array content")]
    InvalidContextEdit,
    /// `SessionHeaderScanLimitError` (`session-manager.ts:610-615`).
    #[error("Session header exceeds 1048576-byte scan limit: {0}")]
    HeaderScanLimit(PathBuf),
    /// `forkFrom` failures.
    #[error("Cannot fork: {0}")]
    Fork(String),
    /// Record-level structural problems.
    #[error("{0}")]
    Migration(String),
    /// A record line failed to parse as JSON.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// A blank JSONL line, which `parseSessionEntryLine` skips.
    #[error("blank line")]
    BlankLine,
}

/// Result alias for the session layer.
pub type SessionResult<T> = Result<T, SessionError>;
