//! `@file` argument processing: expand file references inside prompts into
//! inline content blocks.
//!
//! Port target: upstream `cli/file-processor.ts` (88 lines). The reference
//! checkout was unavailable at write time; the expansion format and the size
//! cap below carry `TODO(verify-vs-TS)` markers to reconcile when
//! `/tmp/ai-agent/pi` is back. Token rules match the args parser: a token
//! starts with `@` and continues to the next whitespace.

use std::path::{Path, PathBuf};

/// Files above this size are refused rather than inlined. The cap mirrors
/// the upstream attachment guard; verify the exact byte count against
/// `file-processor.ts`. TODO(verify-vs-TS)
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Expands `@path` tokens in one message into inline blocks
/// (`file-processor.ts`). Relative paths resolve against `cwd`. A token only
/// expands when the referenced file exists — prose that merely mentions an
/// `@word` stays untouched; explicit missing-file errors come from bare
/// `@file` arguments ([`expand_file_args`]).
///
/// The replacement block format: TODO(verify-vs-TS)
/// ```text
/// <path>:
/// <content>
/// ```
pub fn expand_file_references(message: &str, cwd: &Path) -> Result<String, String> {
    let mut result = String::with_capacity(message.len());
    let mut rest = message;
    let mut changed = false;
    while let Some(at) = rest.find('@') {
        let after = &rest[at + 1..];
        let token_end = after
            .find(|character: char| character.is_whitespace())
            .unwrap_or(after.len());
        let path = &after[..token_end];
        // An empty or trailing `@` is not a file reference; neither is a
        // token that does not name an existing file (prose stays prose).
        if path.is_empty() || !token_resolves(path, cwd) {
            result.push_str(&rest[..=at]);
            rest = after;
            continue;
        }
        result.push_str(&rest[..at]);
        let resolved = resolve_token(path, cwd)?;
        result.push_str(&resolved);
        changed = true;
        rest = &after[token_end..];
    }
    result.push_str(rest);
    if changed {
        Ok(result)
    } else {
        Ok(message.to_string())
    }
}

/// Expands every bare `@file` argument captured by the args parser into
/// prompt text blocks, in capture order (`cli.ts` startup path).
pub fn expand_file_args(file_args: &[String], cwd: &Path) -> Result<String, String> {
    let mut blocks = Vec::new();
    for path in file_args {
        blocks.push(resolve_token(path, cwd)?);
    }
    Ok(blocks.join("\n"))
}

fn token_resolves(path: &str, cwd: &Path) -> bool {
    let trimmed = path.trim();
    let absolute: PathBuf = if Path::new(trimmed).is_absolute() {
        PathBuf::from(trimmed)
    } else {
        cwd.join(trimmed)
    };
    absolute.is_file()
}

fn resolve_token(path: &str, cwd: &Path) -> Result<String, String> {
    let trimmed = path.trim();
    let absolute: PathBuf = if Path::new(trimmed).is_absolute() {
        PathBuf::from(trimmed)
    } else {
        cwd.join(trimmed)
    };
    let metadata = std::fs::metadata(&absolute).map_err(|_| {
        format!(
            "@file not found: {trimmed} (resolved {})",
            absolute.display()
        )
    })?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(format!(
            "@file too large: {trimmed} is {} bytes (limit {MAX_FILE_BYTES})",
            metadata.len()
        ));
    }
    let bytes = std::fs::read(&absolute)
        .map_err(|error| format!("@file unreadable: {trimmed}: {error}"))?;
    match String::from_utf8(bytes) {
        Ok(content) => Ok(format!("{trimmed}:\n{content}")),
        // Binary files cannot inline as text; upstream attaches images
        // instead (interactive mode, P6). TODO(verify-vs-TS)
        Err(_) => Ok(format!(
            "{trimmed}: [binary file: {} bytes]",
            metadata.len()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn expands_relative_and_absolute_paths() {
        let dir = tempdir();
        std::fs::write(dir.path().join("notes.txt"), "alpha\n").expect("write");
        let expanded = expand_file_references("review @notes.txt today", dir.path()).expect("ok");
        assert_eq!(expanded, "review notes.txt:\nalpha\n today");
        let expanded = expand_file_references(
            &format!("check @{}", dir.path().join("notes.txt").display()),
            Path::new("/"),
        )
        .expect("ok");
        assert!(expanded.contains("alpha\n"));
    }

    #[test]
    fn keeps_bare_at_signs_intact() {
        let message = "email me at @handle or @ tomorrow";
        let expanded = expand_file_references(message, Path::new("/")).expect("ok");
        assert_eq!(expanded, message);
    }

    #[test]
    fn inline_tokens_missing_on_disk_stay_untouched() {
        let dir = tempdir();
        let message = "compare @notes.txt with @notes.txt.bak";
        let expanded = expand_file_references(message, dir.path()).expect("ok");
        assert_eq!(expanded, message);
    }

    #[test]
    fn bare_file_args_error_on_missing_files() {
        let dir = tempdir();
        let error = expand_file_args(&["nope.txt".to_string()], dir.path()).unwrap_err();
        assert!(error.contains("not found"), "{error}");
    }

    #[test]
    fn binary_files_replace_content_with_a_note() {
        let dir = tempdir();
        std::fs::write(dir.path().join("blob.bin"), [0u8, 159, 146, 150]).expect("write");
        let expanded = expand_file_references("what is @blob.bin now?", dir.path()).expect("ok");
        assert!(expanded.contains("[binary file: 4 bytes]"), "{expanded}");
    }

    #[test]
    fn oversized_files_refuse_to_inline() {
        let dir = tempdir();
        let path = dir.path().join("huge.txt");
        std::fs::write(&path, "x".repeat((MAX_FILE_BYTES + 1) as usize)).expect("write");
        let error = expand_file_references("read @huge.txt", dir.path()).unwrap_err();
        assert!(error.contains("too large"), "{error}");
    }

    #[test]
    fn bare_file_args_join_in_order() {
        let dir = tempdir();
        std::fs::write(dir.path().join("a.txt"), "A").expect("write");
        std::fs::write(dir.path().join("b.txt"), "B").expect("write");
        let joined =
            expand_file_args(&["a.txt".to_string(), "b.txt".to_string()], dir.path()).expect("ok");
        assert_eq!(joined, "a.txt:\nA\nb.txt:\nB");
    }
}
