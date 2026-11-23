//! Startup session resolution (`main.ts:299-499`): `resolveSessionPath`
//! (`:299-330`), `validateForkFlags` (`:346-360`),
//! `validateSessionIdFlags` (`:362-382`), and `createSessionManager`
//! (`:401-499`).
//!
//! Upstream resolves the session while assembling the runtime, after the
//! metadata commands have exited (`main.ts:940-952`). The port does the same
//! in [`resolve_startup_session`], passing the resolved manager into every
//! mode's runtime so `--resume`/`--fork` work in print, JSON, RPC and the TUI.
//!
//! Deviation (mechanism only): upstream's `--resume` opens the TUI session
//! picker (`cli/session-picker.ts`). The port's startup surface is a plain
//! numbered prompt on stderr (see [`pick_resume_session`]) because the TUI's
//! startup renderer is owned by the interactive mode; the in-session
//! `/resume` dialog remains the full TUI twin. The session lookups, fork
//! semantics and every diagnostic string below are the upstream ones.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::cli::args::{Args, normalize_session_name};
use crate::session::store::{
    NewSessionOptions, SessionInfo, SessionManager, assert_valid_session_id,
};
use crate::tools::path_utils::resolve_path;

/// `ResolvedSession` (`main.ts:294-299`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedSession {
    /// An explicit file path (or a `--session-dir`-relative path).
    Path(PathBuf),
    /// An exact/prefix id match in the current project.
    Local(PathBuf),
    /// An exact/prefix id match in another project.
    Global { path: PathBuf, cwd: PathBuf },
    /// Nothing matched; carries the original argument.
    NotFound(String),
}

/// `validateForkFlags` (`main.ts:346-360`) with upstream's exact error text.
pub fn validate_fork_flags(args: &Args) -> Result<(), String> {
    if args.fork.is_none() {
        return Ok(());
    }
    let conflicting = [
        args.session.as_ref().map(|_| "--session"),
        args.r#continue.then_some("--continue"),
        args.resume.then_some("--resume"),
        args.no_session.then_some("--no-session"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if conflicting.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "--fork cannot be combined with {}",
            conflicting.join(", ")
        ))
    }
}

/// `validateSessionIdFlags` (`main.ts:362-382`) with upstream's exact errors.
pub fn validate_session_id_flags(args: &Args) -> Result<(), String> {
    let Some(id) = args.session_id.as_deref() else {
        return Ok(());
    };
    let conflicting = [
        args.session.as_ref().map(|_| "--session"),
        args.r#continue.then_some("--continue"),
        args.resume.then_some("--resume"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if !conflicting.is_empty() {
        return Err(format!(
            "--session-id cannot be combined with {}",
            conflicting.join(", ")
        ));
    }
    assert_valid_session_id(id).map_err(|error| error.to_string())
}

/// `resolveSessionPath` (`main.ts:299-330`): explicit paths first, then an
/// exact local id, a local prefix, an exact global id, a global prefix, in
/// that order.
pub fn resolve_session_path(
    session_arg: &str,
    cwd: &Path,
    session_dir: Option<&Path>,
) -> ResolvedSession {
    // Upstream's `resolveSessionPath` uses a case-sensitive `endsWith(".jsonl")`.
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    if session_arg.contains('/') || session_arg.contains('\\') || session_arg.ends_with(".jsonl") {
        return ResolvedSession::Path(resolve_path(session_arg, cwd));
    }
    if let Some(path) = SessionManager::find_by_id(cwd, session_arg, session_dir) {
        return ResolvedSession::Local(path);
    }
    if let Some(session) = SessionManager::list(cwd, session_dir)
        .into_iter()
        .find(|session| session.id.starts_with(session_arg))
    {
        return ResolvedSession::Local(session.path);
    }
    let all = SessionManager::list_all(session_dir);
    if let Some(session) = all
        .iter()
        .find(|session| session.id == session_arg)
        .or_else(|| {
            all.iter()
                .find(|session| session.id.starts_with(session_arg))
        })
    {
        return ResolvedSession::Global {
            path: session.path.clone(),
            cwd: PathBuf::from(&session.cwd),
        };
    }
    ResolvedSession::NotFound(session_arg.to_string())
}

/// Why startup did not produce a session manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupSessionError {
    /// Printed as `Error: <message>` to stderr, exit 1 (`openSessionOrExit`,
    /// `forkSessionOrExit`, and the not-found arms of `createSessionManager`).
    Error(String),
    /// A cancellation upstream prints with `chalk.dim` and exits 0 with
    /// ("No session selected" / "Aborted."; already printed by this module).
    Cancelled,
}

/// `createSessionManager` (`main.ts:401-499`) plus `openSessionOrExit`
/// (`:384-392`) and `forkSessionOrExit` (`:394-399`). Warnings that upstream
/// does not abort on are printed here in place.
pub fn resolve_startup_session(
    args: &Args,
    cwd: &Path,
    session_dir: Option<&Path>,
) -> Result<SessionManager, StartupSessionError> {
    let session_id = args.session_id.as_deref().and_then(normalize_session_name);
    let options = NewSessionOptions {
        id: session_id.clone(),
        parent_session: None,
    };

    // `main.ts:402-405`: metadata commands never open a persisted session.
    if args.no_session || args.help || args.list_models.is_some() {
        return SessionManager::in_memory(cwd, &options, Vec::new()).map_err(session_error);
    }

    // `main.ts:407-422`: --fork first; conflicts are validated before this
    // call in `main()` (mirroring `validateForkFlags` at `main.ts:713`).
    if let Some(fork_arg) = args.fork.as_deref() {
        if let Some(id) = session_id.as_deref()
            && let Some(existing) = SessionManager::find_by_id(cwd, id, session_dir)
        {
            let _ = existing;
            return Err(StartupSessionError::Error(format!(
                "Session already exists with id '{id}'"
            )));
        }
        let source = match resolve_session_path(fork_arg, cwd, session_dir) {
            ResolvedSession::Path(path)
            | ResolvedSession::Local(path)
            | ResolvedSession::Global { path, .. } => path,
            ResolvedSession::NotFound(arg) => {
                return Err(StartupSessionError::Error(format!(
                    "No session found matching '{arg}'"
                )));
            }
        };
        return SessionManager::fork_from(&source, cwd, session_dir, &options)
            .map_err(session_error);
    }

    // `main.ts:424-447`: --session opens local sessions, and offers to fork a
    // session found in a different project.
    if let Some(session_arg) = args.session.as_deref() {
        match resolve_session_path(session_arg, cwd, session_dir) {
            ResolvedSession::Path(path) | ResolvedSession::Local(path) => {
                return SessionManager::open(&path, session_dir, None).map_err(session_error);
            }
            ResolvedSession::Global { path, cwd: origin } => {
                println!("Session found in different project: {}", origin.display());
                if !prompt_confirm("Fork this session into current directory?") {
                    println!("Aborted.");
                    return Err(StartupSessionError::Cancelled);
                }
                return SessionManager::fork_from(
                    &path,
                    cwd,
                    session_dir,
                    &NewSessionOptions::default(),
                )
                .map_err(session_error);
            }
            ResolvedSession::NotFound(arg) => {
                return Err(StartupSessionError::Error(format!(
                    "No session found matching '{arg}'"
                )));
            }
        }
    }

    // `main.ts:462-475`: --resume picks a session through the startup UI.
    if args.resume {
        if let Some(path) = pick_resume_session(cwd, session_dir)? {
            return SessionManager::open(&path, session_dir, None).map_err(session_error);
        }
        println!("No session selected");
        return Err(StartupSessionError::Cancelled);
    }

    // `main.ts:477-479`.
    if args.r#continue {
        return SessionManager::continue_recent(cwd, session_dir).map_err(session_error);
    }

    // `main.ts:481-497`: an existing exact id opens; otherwise warn and
    // create with the requested id.
    if let Some(id) = session_id.as_deref() {
        let existing = SessionManager::find_by_id(cwd, id, session_dir);
        if let Some(path) = existing {
            return SessionManager::open(&path, session_dir, None).map_err(session_error);
        }
        eprintln!(
            "Warning: No project session found with id '{id}'; creating a new session with that id."
        );
    }
    SessionManager::create(cwd, session_dir, &options).map_err(session_error)
}

fn session_error(error: impl std::fmt::Display) -> StartupSessionError {
    StartupSessionError::Error(error.to_string())
}

/// `promptConfirm` (`main.ts:333-344`): `<message> [y/N] ` on stdout, true
/// only for `y`/`yes` (case-insensitive).
fn prompt_confirm(message: &str) -> bool {
    print!("{message} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    let answer = line.trim().to_lowercase();
    answer == "y" || answer == "yes"
}

/// The port's startup counterpart of `selectSession` (`cli/session-picker.ts`):
/// a numbered prompt listing local sessions first, then sessions from other
/// projects. Returns `None` on cancel (empty line, `q`, or an unknown answer).
fn pick_resume_session(
    cwd: &Path,
    session_dir: Option<&Path>,
) -> Result<Option<PathBuf>, StartupSessionError> {
    if !std::io::stdin().is_terminal() {
        return Err(StartupSessionError::Error(
            "--resume requires an interactive terminal".to_string(),
        ));
    }
    let local = SessionManager::list(cwd, session_dir);
    let all = SessionManager::list_all(session_dir);
    let mut sessions: Vec<SessionInfo> = Vec::new();
    for candidate in local.into_iter().chain(all) {
        if !sessions
            .iter()
            .any(|session| session.path == candidate.path)
        {
            sessions.push(candidate);
        }
    }
    if sessions.is_empty() {
        return Ok(None);
    }
    eprintln!("Select a session to resume (empty or q to cancel):");
    for (index, session) in sessions.iter().enumerate() {
        let name = session.name.as_deref().unwrap_or("");
        let preview = session.first_message.chars().take(48).collect::<String>();
        eprintln!(
            "  {}) {}  {}  {}  {}",
            index + 1,
            session.id,
            name,
            session.cwd,
            preview.replace('\n', " ")
        );
    }
    eprint!("Select 1-{}: ", sessions.len());
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return Ok(None);
    }
    let answer = line.trim();
    if answer.is_empty() || answer.eq_ignore_ascii_case("q") {
        return Ok(None);
    }
    if let Ok(index) = answer.parse::<usize>()
        && let Some(session) = sessions.get(index.wrapping_sub(1))
    {
        return Ok(Some(session.path.clone()));
    }
    if let Some(session) = sessions
        .iter()
        .find(|session| session.id.starts_with(answer))
    {
        return Ok(Some(session.path.clone()));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_conflicts_follow_upstream_order() {
        let args = Args {
            fork: Some("abc".to_string()),
            session: Some("other".to_string()),
            r#continue: true,
            ..Args::default()
        };
        assert_eq!(
            validate_fork_flags(&args),
            Err("--fork cannot be combined with --session, --continue".to_string())
        );
    }

    #[test]
    fn fork_creates_a_new_session_with_a_parent_link() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source_dir = dir.path().join("source");
        let target_dir = dir.path().join("target");
        std::fs::create_dir_all(&source_dir).expect("source dir");
        std::fs::create_dir_all(&target_dir).expect("target dir");
        let mut source = SessionManager::create(
            &source_dir,
            Some(&source_dir),
            &NewSessionOptions::default(),
        )
        .expect("source session");
        // A fresh session materializes its header on the first conversation
        // message (`persist_last` defers until `has_conversation()`).
        source
            .append_message(pi_agent::agent_message::AgentMessage::User(
                pi_ai::types::UserMessage {
                    content: pi_ai::types::UserContent::Text("fixture".to_string()),
                    timestamp: 1,
                },
            ))
            .expect("append message");
        let source_file = source.session_file().expect("source file").to_path_buf();
        let args = Args {
            fork: Some(source_file.to_string_lossy().into_owned()),
            ..Args::default()
        };
        let forked = resolve_startup_session(&args, &target_dir, Some(&target_dir)).expect("fork");
        let forked_file = forked.session_file().expect("fork file");
        assert_ne!(forked_file, source_file.as_path());
        assert!(forked_file.exists());
        let content = std::fs::read_to_string(forked_file).expect("read fork");
        assert!(content.contains("parentSession"));
    }

    #[test]
    fn session_id_conflicts_and_validation() {
        let mut args = Args {
            session_id: Some("valid-id".to_string()),
            resume: true,
            ..Args::default()
        };
        assert_eq!(
            validate_session_id_flags(&args),
            Err("--session-id cannot be combined with --resume".to_string())
        );
        args.resume = false;
        assert_eq!(validate_session_id_flags(&args), Ok(()));
    }
}
