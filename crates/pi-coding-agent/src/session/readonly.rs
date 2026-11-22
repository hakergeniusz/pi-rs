//! The type-level readonly view of a session manager.
//!
//! Ground truth: `packages/coding-agent/src/core/session-manager.ts:245-262`,
//! where `ReadonlySessionManager = Pick<SessionManager, ...>` is the structural
//! subset of read-only methods handed to extension hooks (`agent-session.ts`,
//! `bug-report.ts`, `compaction/branch-summarization.ts`) so they can inspect a
//! session without mutating or reloading it.
//!
//! Rust has no structural `Pick`, so this is a trait carrying exactly the
//! upstream method surface, implemented for [`SessionManager`] and (via a
//! blanket impl) for references to any implementor. Callers hold an
//! `&dyn ReadonlySessionManager` to get the same capability-limited view. The
//! trait is purely additive: `SessionManager`'s inherent methods are unchanged.
//! Production consumers: [`super::cwd::assert_session_cwd_exists`] (the
//! `/import` / `/resume` gate) and the interactive host's `readonly_session()`
//! seam, which extension-facing payloads use to read session state without
//! touching the mutable store.

use std::path::Path;

use super::{
    SessionEntry, SessionHeader, SessionManager, SessionProjection, SessionResult, SessionTreeNode,
};

/// The read-only surface of a session manager
/// (`session-manager.ts:245-262`). Every method corresponds to one of the
/// upstream `Pick` keys, in the same order.
pub trait ReadonlySessionManager {
    /// `getCwd` (`:1148-1150`).
    fn get_cwd(&self) -> &Path;
    /// `getSessionDir` (`:1152-1154`).
    fn get_session_dir(&self) -> &Path;
    /// `getSessionId` (`:1160-1162`).
    fn get_session_id(&self) -> &str;
    /// `getSessionFile` (`:1164-1166`).
    fn get_session_file(&self) -> Option<&Path>;
    /// `getLeafId` (`:1422-1424`).
    fn get_leaf_id(&self) -> Option<&str>;
    /// `getLeafEntry` (`:1426-1428`).
    fn get_leaf_entry(&self) -> Option<SessionEntry>;
    /// `getEntry` (`:1430-1432`).
    fn get_entry(&self, id: &str) -> Option<SessionEntry>;
    /// `getLabel` (`:1450-1452`).
    fn get_label(&self, id: &str) -> Option<&str>;
    /// `getBranch` (`:1487-1497`).
    fn get_branch(&self, from_id: Option<&str>) -> SessionResult<Vec<SessionEntry>>;
    /// `buildContextEntries` (`:1503-1505`).
    fn build_context_entries(&self) -> Vec<SessionEntry>;
    /// `buildSessionProjection` (`:1511-1513`).
    fn build_session_projection(&self) -> SessionResult<SessionProjection>;
    /// `getHeader` (`:1523-1526`).
    fn get_header(&self) -> Option<SessionHeader>;
    /// `getEntries` (`:1538-1540`).
    fn get_entries(&self) -> Vec<SessionEntry>;
    /// `getTree` (`:1547-1585`).
    fn get_tree(&self) -> Vec<SessionTreeNode>;
    /// `getSessionName` (`:1336-1347`).
    fn get_session_name(&self) -> Option<String>;
}

impl ReadonlySessionManager for SessionManager {
    fn get_cwd(&self) -> &Path {
        SessionManager::get_cwd(self)
    }

    fn get_session_dir(&self) -> &Path {
        SessionManager::get_session_dir(self)
    }

    fn get_session_id(&self) -> &str {
        SessionManager::get_session_id(self)
    }

    fn get_session_file(&self) -> Option<&Path> {
        SessionManager::get_session_file(self)
    }

    fn get_leaf_id(&self) -> Option<&str> {
        SessionManager::get_leaf_id(self)
    }

    fn get_leaf_entry(&self) -> Option<SessionEntry> {
        SessionManager::get_leaf_entry(self)
    }

    fn get_entry(&self, id: &str) -> Option<SessionEntry> {
        SessionManager::get_entry(self, id)
    }

    fn get_label(&self, id: &str) -> Option<&str> {
        SessionManager::get_label(self, id)
    }

    fn get_branch(&self, from_id: Option<&str>) -> SessionResult<Vec<SessionEntry>> {
        SessionManager::get_branch(self, from_id)
    }

    fn build_context_entries(&self) -> Vec<SessionEntry> {
        SessionManager::build_context_entries(self)
    }

    fn build_session_projection(&self) -> SessionResult<SessionProjection> {
        SessionManager::build_session_projection(self)
    }

    fn get_header(&self) -> Option<SessionHeader> {
        SessionManager::get_header(self)
    }

    fn get_entries(&self) -> Vec<SessionEntry> {
        SessionManager::get_entries(self)
    }

    fn get_tree(&self) -> Vec<SessionTreeNode> {
        SessionManager::get_tree(self)
    }

    fn get_session_name(&self) -> Option<String> {
        SessionManager::get_session_name(self)
    }
}

/// A reference to a session manager is itself a readonly view, so callers can
/// pass `&manager` or `&dyn ReadonlySessionManager` to extension hooks.
impl<T: ReadonlySessionManager + ?Sized> ReadonlySessionManager for &T {
    fn get_cwd(&self) -> &Path {
        (**self).get_cwd()
    }

    fn get_session_dir(&self) -> &Path {
        (**self).get_session_dir()
    }

    fn get_session_id(&self) -> &str {
        (**self).get_session_id()
    }

    fn get_session_file(&self) -> Option<&Path> {
        (**self).get_session_file()
    }

    fn get_leaf_id(&self) -> Option<&str> {
        (**self).get_leaf_id()
    }

    fn get_leaf_entry(&self) -> Option<SessionEntry> {
        (**self).get_leaf_entry()
    }

    fn get_entry(&self, id: &str) -> Option<SessionEntry> {
        (**self).get_entry(id)
    }

    fn get_label(&self, id: &str) -> Option<&str> {
        (**self).get_label(id)
    }

    fn get_branch(&self, from_id: Option<&str>) -> SessionResult<Vec<SessionEntry>> {
        (**self).get_branch(from_id)
    }

    fn build_context_entries(&self) -> Vec<SessionEntry> {
        (**self).build_context_entries()
    }

    fn build_session_projection(&self) -> SessionResult<SessionProjection> {
        (**self).build_session_projection()
    }

    fn get_header(&self) -> Option<SessionHeader> {
        (**self).get_header()
    }

    fn get_entries(&self) -> Vec<SessionEntry> {
        (**self).get_entries()
    }

    fn get_tree(&self) -> Vec<SessionTreeNode> {
        (**self).get_tree()
    }

    fn get_session_name(&self) -> Option<String> {
        (**self).get_session_name()
    }
}

#[cfg(test)]
mod tests {
    use pi_agent::AgentMessage;

    use super::*;
    use crate::session::NewSessionOptions;

    fn user_message(text: &str) -> AgentMessage {
        AgentMessage::User(pi_ai::types::UserMessage {
            content: pi_ai::types::UserContent::Text(text.to_string()),
            timestamp: 1,
        })
    }

    #[test]
    fn readonly_view_exposes_upstream_read_surface() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager =
            SessionManager::create(dir.path(), Some(dir.path()), &NewSessionOptions::default())
                .unwrap();
        let first = manager.append_message(user_message("first")).unwrap();
        let second = manager.append_message(user_message("second")).unwrap();

        // The whole point of the type is that it can be used as a trait object.
        let view: &dyn ReadonlySessionManager = &manager;

        // Accessors forward the exact inherent values.
        assert_eq!(view.get_cwd(), manager.get_cwd());
        assert_eq!(view.get_session_dir(), manager.get_session_dir());
        assert_eq!(view.get_session_id(), manager.get_session_id());
        assert_eq!(view.get_session_file(), manager.get_session_file());
        assert_eq!(view.get_leaf_id(), Some(second.id()));
        assert!(view.get_leaf_entry().is_some());
        assert!(view.get_entry(first.id()).is_some());
        assert_eq!(view.get_label(first.id()), manager.get_label(first.id()));
        assert_eq!(view.get_branch(None).unwrap().len(), 2);
        assert_eq!(view.build_context_entries().len(), 2);
        assert_eq!(
            view.build_session_projection().unwrap().messages.len(),
            manager.build_session_context().unwrap().messages.len()
        );
        assert!(view.get_header().is_some());
        assert_eq!(view.get_entries().len(), manager.get_entries().len());
        assert_eq!(view.get_tree().len(), manager.get_tree().len());
        assert_eq!(view.get_session_name(), manager.get_session_name());
    }

    #[test]
    fn readonly_view_works_through_a_reference_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let manager =
            SessionManager::create(dir.path(), Some(dir.path()), &NewSessionOptions::default())
                .unwrap();
        let reference = &manager;
        let view: &dyn ReadonlySessionManager = &reference;
        assert_eq!(view.get_session_id(), manager.get_session_id());
    }
}
