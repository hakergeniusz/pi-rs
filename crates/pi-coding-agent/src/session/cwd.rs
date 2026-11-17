//! The stored-cwd existence gate: a session whose recorded working directory
//! no longer exists must not silently run in the wrong place.
//!
//! Ground truth: `packages/coding-agent/src/core/session-cwd.ts` (the whole
//! file, `:1-73`). Consumers: the `/import` flow
//! (`agent-session-runtime.ts:359-407` re-opens the copied file and asserts
//! the cwd, `interactive-mode.ts:6755-6803` prompts on the error) and the
//! `/resume` flow (`interactive-mode.ts:5955-5980`).

use std::path::{Path, PathBuf};

use super::SessionManager;

/// `SessionCwdIssue` (`session-cwd.ts:3-7`): the stored cwd that is gone, the
/// cwd to fall back to, and the session file that carries the stale cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCwdIssue {
    /// `sessionFile?` — absent for in-memory sessions, which can never have a
    /// stale stored cwd.
    pub session_file: Option<PathBuf>,
    pub session_cwd: PathBuf,
    pub fallback_cwd: PathBuf,
}

/// `getMissingSessionCwdIssue` (`session-cwd.ts:14-33`): `None` when the
/// session is in-memory, its cwd is recorded, or the recorded cwd exists.
pub fn get_missing_session_cwd_issue(
    session_manager: &SessionManager,
    fallback_cwd: &Path,
) -> Option<SessionCwdIssue> {
    let session_file = session_manager.get_session_file()?;
    let session_cwd = session_manager.get_cwd();
    if session_cwd.as_os_str().is_empty() || session_cwd.exists() {
        return None;
    }
    Some(SessionCwdIssue {
        session_file: Some(session_file.to_path_buf()),
        session_cwd: session_cwd.to_path_buf(),
        fallback_cwd: fallback_cwd.to_path_buf(),
    })
}

/// `formatMissingSessionCwdError` (`session-cwd.ts:35-38`).
pub fn format_missing_session_cwd_error(issue: &SessionCwdIssue) -> String {
    let session_file = issue
        .session_file
        .as_ref()
        .map_or_else(String::new, |file| {
            format!("\nSession file: {}", file.display())
        });
    format!(
        "Stored session working directory does not exist: {}{}\nCurrent working directory: {}",
        issue.session_cwd.display(),
        session_file,
        issue.fallback_cwd.display()
    )
}

/// `formatMissingSessionCwdPrompt` (`session-cwd.ts:40-42`).
pub fn format_missing_session_cwd_prompt(issue: &SessionCwdIssue) -> String {
    format!("Session cwd does not exist: {}", issue.session_cwd.display())
}

/// `MissingSessionCwdChoice` (`session-cwd.ts:45`): how to open a session
/// whose stored cwd is missing — run it in the current cwd once, or fork it
/// there. `None` (the selector's Cancel) cancels the flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingSessionCwdChoice {
    Continue,
    Fork,
}

/// One prompt row: the label plus the choice it maps to (`None` cancels).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSessionCwdOption {
    pub label: String,
    pub value: Option<MissingSessionCwdChoice>,
}

/// `getMissingSessionCwdOptions` (`session-cwd.ts:48-56`): prompt options for
/// a missing session cwd, naming the current cwd.
pub fn get_missing_session_cwd_options(issue: &SessionCwdIssue) -> Vec<MissingSessionCwdOption> {
    vec![
        MissingSessionCwdOption {
            label: format!(
                "Continue in {} (asks again next time)",
                issue.fallback_cwd.display()
            ),
            value: Some(MissingSessionCwdChoice::Continue),
        },
        MissingSessionCwdOption {
            label: format!(
                "Fork into {} (new session ID, original kept)",
                issue.fallback_cwd.display()
            ),
            value: Some(MissingSessionCwdChoice::Fork),
        },
        MissingSessionCwdOption {
            label: "Cancel".to_string(),
            value: None,
        },
    ]
}

/// `MissingSessionCwdError` (`session-cwd.ts:58-66`): carries the issue and
/// renders like `formatMissingSessionCwdError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSessionCwdError {
    pub issue: SessionCwdIssue,
}

impl std::fmt::Display for MissingSessionCwdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format_missing_session_cwd_error(&self.issue))
    }
}

impl std::error::Error for MissingSessionCwdError {}

/// `assertSessionCwdExists` (`session-cwd.ts:68-73`): the import/open flows
/// call this after opening the (copied) session file and abort when the
/// stored cwd is stale.
pub fn assert_session_cwd_exists(
    session_manager: &SessionManager,
    fallback_cwd: &Path,
) -> Result<(), MissingSessionCwdError> {
    match get_missing_session_cwd_issue(session_manager, fallback_cwd) {
        Some(issue) => Err(MissingSessionCwdError { issue }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::store::NewSessionOptions;
    use std::fs;

    /// `writeSessionFile` (`session-cwd.test.ts:18-30`): a minimal v3 header
    /// pointing at `cwd`.
    fn write_session_file(path: &Path, cwd: &Path) {
        fs::write(
            path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"session-id\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"{}\"}}\n",
                cwd.display()
            ),
        )
        .expect("write session file");
    }

    /// `detects missing session cwd from persisted sessions`
    /// (`session-cwd.test.ts:37-53`).
    #[test]
    fn detects_missing_session_cwd_from_persisted_sessions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fallback_cwd = dir.path().join("fallback");
        fs::create_dir_all(&fallback_cwd).expect("mkdir");
        let missing_cwd = fallback_cwd.join("does-not-exist");
        let session_file = dir.path().join("session.jsonl");
        write_session_file(&session_file, &missing_cwd);

        let session_manager = SessionManager::open(&session_file, None, None).expect("open");
        let issue = get_missing_session_cwd_issue(&session_manager, &fallback_cwd);
        assert_eq!(
            issue,
            Some(SessionCwdIssue {
                session_file: Some(session_file.clone()),
                session_cwd: missing_cwd,
                fallback_cwd: fallback_cwd.clone(),
            })
        );
        // The error renders the exact upstream message.
        let error = assert_session_cwd_exists(&session_manager, &fallback_cwd).expect_err("stale");
        assert_eq!(
            error.to_string(),
            format!(
                "Stored session working directory does not exist: {}\nSession file: {}\nCurrent working directory: {}",
                missing_cwd.display(),
                session_file.display(),
                fallback_cwd.display()
            )
        );
    }

    /// `supports overriding the effective cwd when opening a session`
    /// (`session-cwd.test.ts:54-72`): opening with the override records the
    /// fallback cwd, which then exists, so no issue is raised.
    #[test]
    fn cwd_override_suppresses_the_issue() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fallback_cwd = dir.path().join("fallback");
        fs::create_dir_all(&fallback_cwd).expect("mkdir");
        let missing_cwd = fallback_cwd.join("does-not-exist");
        let session_file = dir.path().join("session.jsonl");
        write_session_file(&session_file, &missing_cwd);

        let session_manager =
            SessionManager::open(&session_file, None, Some(&fallback_cwd)).expect("open");
        assert_eq!(session_manager.get_cwd(), fallback_cwd);
        assert!(session_manager.get_session_file().is_some());
        assert_eq!(
            get_missing_session_cwd_issue(&session_manager, &fallback_cwd),
            None
        );
    }

    /// A session whose stored cwd exists is fine (`getMissingSessionCwdIssue`'s
    /// `existsSync` branch).
    #[test]
    fn existing_cwd_and_in_memory_sessions_raise_no_issue() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_file = dir.path().join("session.jsonl");
        write_session_file(&session_file, dir.path());
        let session_manager = SessionManager::open(&session_file, None, None).expect("open");
        assert_eq!(get_missing_session_cwd_issue(&session_manager, dir.path()), None);

        // In-memory sessions carry no session file, so they are never stale.
        let in_memory = SessionManager::in_memory(
            dir.path(),
            &NewSessionOptions::default(),
            Vec::new(),
        )
        .expect("in-memory");
        assert!(in_memory.get_session_file().is_none());
        assert_eq!(get_missing_session_cwd_issue(&in_memory, dir.path()), None);
    }

    /// `getMissingSessionCwdOptions` labels name the fallback cwd, and the
    /// Cancel row maps to no choice.
    #[test]
    fn prompt_options_name_the_fallback_cwd() {
        let issue = SessionCwdIssue {
            session_file: None,
            session_cwd: PathBuf::from("/gone"),
            fallback_cwd: PathBuf::from("/here"),
        };
        let options = get_missing_session_cwd_options(&issue);
        assert_eq!(options.len(), 3);
        assert_eq!(options[0].label, "Continue in /here (asks again next time)");
        assert_eq!(options[0].value, Some(MissingSessionCwdChoice::Continue));
        assert_eq!(
            options[1].label,
            "Fork into /here (new session ID, original kept)"
        );
        assert_eq!(options[1].value, Some(MissingSessionCwdChoice::Fork));
        assert_eq!(options[2].label, "Cancel");
        assert_eq!(options[2].value, None);
        assert_eq!(
            format_missing_session_cwd_prompt(&issue),
            "Session cwd does not exist: /gone"
        );
    }
}
