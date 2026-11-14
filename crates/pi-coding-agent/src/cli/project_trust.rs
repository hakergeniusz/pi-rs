//! Project trust gating: untrusted directories run with project-level
//! settings and skills disabled until the user trusts them.
//!
//! Port target: upstream `cli/project-trust.ts` (small) plus the trust check
//! inside `config.ts`/`settings-manager.ts` (untrusted layers are skipped —
//! already honored by `SettingsManager`). The interactive prompt is a
//! stdin/stdout confirm here; the TUI dialog lands with P6.
//! TODO(verify-vs-TS): reconcile the marker-file name and message wording
//! against the reference checkout.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::config;

/// Whether the working directory has been trusted for this user.
///
/// The decision is stored under the agent dir as one empty marker file per
/// trusted absolute path (slash-normalized), so trust survives restarts and
/// can be revoked by deleting the file.
pub fn is_trusted(cwd: &Path) -> bool {
    is_trusted_in(&config::get_agent_dir(), cwd)
}

/// [`is_trusted`] against an explicit agent dir (the testable core).
pub fn is_trusted_in(agent_dir: &Path, cwd: &Path) -> bool {
    marker_path_in(agent_dir, cwd).is_file()
}

/// Trusts `cwd` by writing the marker file. Fails on the error string path.
pub fn trust(cwd: &Path) -> Result<(), String> {
    trust_in(&config::get_agent_dir(), cwd)
}

/// [`trust`] against an explicit agent dir (the testable core).
pub fn trust_in(agent_dir: &Path, cwd: &Path) -> Result<(), String> {
    let path = marker_path_in(agent_dir, cwd);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create trust marker dir: {error}"))?;
    }
    std::fs::write(&path, "").map_err(|error| format!("cannot write trust marker: {error}"))
}

/// Revokes trust (used by tests and a future `pi trust remove`).
pub fn untrust(cwd: &Path) -> Result<(), String> {
    untrust_in(&config::get_agent_dir(), cwd)
}

/// [`untrust`] against an explicit agent dir (the testable core).
pub fn untrust_in(agent_dir: &Path, cwd: &Path) -> Result<(), String> {
    let path = marker_path_in(agent_dir, cwd);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot remove trust marker: {error}")),
    }
}

fn marker_path_in(agent_dir: &Path, cwd: &Path) -> PathBuf {
    let normalized = normalize(cwd);
    let encoded: String = normalized
        .to_string_lossy()
        .chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => character,
            _ => '_',
        })
        .collect();
    agent_dir.join("trusted-projects").join(encoded)
}

/// Slash-normalizes like the settings layer does before trust decisions.
fn normalize(path: &Path) -> PathBuf {
    let text = path.to_string_lossy().replace('\\', "/");
    let absolute = text.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in text.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let mut normalized = PathBuf::new();
    if absolute {
        normalized.push("/");
    }
    for part in parts {
        normalized.push(part);
    }
    if normalized.as_os_str().is_empty() {
        normalized.push("/");
    }
    normalized
}

/// Asks on stdin/stdout and persists the decision. Returns the final state.
/// A declined prompt does not persist anything; the caller runs untrusted.
pub fn prompt_for_trust(
    cwd: &Path,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<bool, String> {
    prompt_for_trust_in(&config::get_agent_dir(), cwd, input, output)
}

/// [`prompt_for_trust`] against an explicit agent dir (the testable core).
pub fn prompt_for_trust_in(
    agent_dir: &Path,
    cwd: &Path,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<bool, String> {
    if is_trusted_in(agent_dir, cwd) {
        return Ok(true);
    }
    writeln!(
        output,
        "Trust the project at {}? Its .pi settings, skills, and prompt templates will apply. (y/N)",
        cwd.display()
    )
    .map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())?;
    let mut answer = String::new();
    input
        .read_line(&mut answer)
        .map_err(|error| error.to_string())?;
    let confirmed = matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    if confirmed {
        trust_in(agent_dir, cwd)?;
    }
    Ok(confirmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn untrusted_by_default_then_trusted_after_marker() {
        let agent_dir = tempdir();
        let project = tempdir();
        assert!(!is_trusted_in(agent_dir.path(), project.path()));
        trust_in(agent_dir.path(), project.path()).expect("trust");
        assert!(is_trusted_in(agent_dir.path(), project.path()));
        untrust_in(agent_dir.path(), project.path()).expect("untrust");
        assert!(!is_trusted_in(agent_dir.path(), project.path()));
    }

    #[test]
    fn trust_marker_survives_a_second_check() {
        let agent_dir = tempdir();
        let project = tempdir();
        trust_in(agent_dir.path(), project.path()).expect("trust");
        // The marker file exists under trusted-projects/ with a flat name.
        assert!(agent_dir.path().join("trusted-projects").is_dir());
    }

    #[test]
    fn prompt_decline_leaves_untrusted() {
        let agent_dir = tempdir();
        let project = tempdir();
        let mut input = "n\n".as_bytes();
        let mut output = Vec::new();
        let trusted =
            prompt_for_trust_in(agent_dir.path(), project.path(), &mut input, &mut output)
                .expect("prompt");
        assert!(!trusted);
        assert!(!is_trusted_in(agent_dir.path(), project.path()));
    }

    #[test]
    fn prompt_accept_persists_trust() {
        let agent_dir = tempdir();
        let project = tempdir();
        let mut input = "y\n".as_bytes();
        let mut output = Vec::new();
        let trusted =
            prompt_for_trust_in(agent_dir.path(), project.path(), &mut input, &mut output)
                .expect("prompt");
        assert!(trusted);
        assert!(is_trusted_in(agent_dir.path(), project.path()));
    }

    #[test]
    fn normalize_collapses_dots_and_backslashes() {
        assert_eq!(
            normalize(Path::new("/a/b/../c")).display().to_string(),
            "/a/c"
        );
        assert_eq!(
            normalize(Path::new("C:\\a\\b")).display().to_string(),
            "C:/a/b"
        );
    }
}
