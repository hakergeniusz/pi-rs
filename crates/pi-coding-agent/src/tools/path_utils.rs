//! Path resolution for the file tools.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/path-utils.ts`
//! plus `utils/paths.ts` (`normalizePath`, `resolvePath`), the options the
//! tools pass (`normalizeUnicodeSpaces: true`, `stripAtPrefix: true`).
//!
//! Two rules matter to the tools:
//!
//! * [`resolve_to_cwd`] — expand `~`, drop a leading `@`, normalize unicode
//!   spaces, then resolve against the working directory.
//! * [`resolve_read_path`] — the same, plus the variants a model cannot type:
//!   the macOS narrow-no-break-space before `AM`/`PM` and the curly
//!   apostrophe. Tried in pi's order, first hit wins.
//!
//! Deviation: pi also tries the NFD (decomposed) Unicode form for macOS
//! filenames. Rust has no Unicode normalization in `std` and the
//! `unicode-normalization` crate is not in the dependency set, so that variant
//! is skipped (`path-utils.ts:11-14`); it only differs for accented names on
//! case-preserving non-macOS filesystems.

use std::path::{Path, PathBuf};

/// pi's `NARROW_NO_BREAK_SPACE` (`path-utils.ts:5`).
const NARROW_NO_BREAK_SPACE: char = '\u{202F}';

/// pi's `normalizePath` (`utils/paths.ts:109-136`) with the options the tools
/// pass: unicode spaces folded, `@` prefix stripped, tilde expanded.
pub fn normalize_path(input: &str) -> PathBuf {
    let mut normalized: String = input
        .chars()
        .map(|character| match character {
            // NBSP, the U+2000..U+200A run, narrow NBSP, medium math space,
            // and ideographic space all fold to a plain space.
            '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect();
    if let Some(stripped) = normalized.strip_prefix('@') {
        normalized = stripped.to_string();
    }
    let home = home_dir();
    if normalized == "~" {
        return PathBuf::from(home);
    }
    if let Some(rest) = normalized.strip_prefix("~/") {
        return PathBuf::from(home).join(rest);
    }
    if let Some(rest) = normalized.strip_prefix("file://") {
        return PathBuf::from(rest);
    }
    PathBuf::from(normalized)
}

/// `os.homedir()`.
fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// pi's `resolvePath` (`utils/paths.ts:145-151`): absolute paths resolve
/// against themselves, everything else against `cwd`.
pub fn resolve_path(input: &str, cwd: &Path) -> PathBuf {
    let normalized = normalize_path(input);
    if normalized.is_absolute() {
        normalize_lexically(&normalized)
    } else {
        normalize_lexically(&cwd.join(normalized))
    }
}

/// `path.resolve` without touching the filesystem: collapse `.` and `..`.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

/// pi's `pathExists` (`path-utils.ts:31-38`).
pub fn path_exists(path: &Path) -> bool {
    path.exists()
}

/// pi's `resolveToCwd` (`path-utils.ts:48-50`).
pub fn resolve_to_cwd(path: &str, cwd: &Path) -> PathBuf {
    resolve_path(path, cwd)
}

/// pi's `expandPath` (`path-utils.ts:40-42`): normalization without a base.
pub fn expand_path(path: &str) -> PathBuf {
    normalize_path(path)
}

/// pi's `tryMacOSScreenshotPath` (`path-utils.ts:7-9`): the space before
/// `AM`/`PM` in screenshot names becomes a narrow no-break space.
fn try_mac_os_screenshot_path(path: &str) -> String {
    let chars: Vec<char> = path.chars().collect();
    let mut out = String::with_capacity(path.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == ' ' && index + 3 <= chars.len() {
            let marker: String = chars[index + 1..index + 3].iter().collect();
            let is_marker = marker.eq_ignore_ascii_case("AM") || marker.eq_ignore_ascii_case("PM");
            if is_marker && chars.get(index + 3) == Some(&'.') {
                out.push(NARROW_NO_BREAK_SPACE);
                out.push_str(&marker);
                out.push('.');
                index += 4;
                continue;
            }
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

/// pi's `tryCurlyQuoteVariant` (`path-utils.ts:16-20`).
fn try_curly_quote_variant(path: &str) -> String {
    path.replace('\'', "\u{2019}")
}

/// pi's `resolveReadPath` (`path-utils.ts:52-84`): the resolved path, or the
/// first existing variant.
pub fn resolve_read_path(path: &str, cwd: &Path) -> PathBuf {
    let resolved = resolve_to_cwd(path, cwd);
    let resolved_str = resolved.to_string_lossy().into_owned();

    if path_exists(&resolved) {
        return resolved;
    }

    for variant in read_path_variants(&resolved_str) {
        if path_exists(&variant) {
            return variant;
        }
    }

    resolved
}

/// The spelling variants pi tries, in pi's order (`path-utils.ts:59-82`):
/// macOS AM/PM, then the curly apostrophe, then curly applied twice (pi's
/// combined NFD + curly step without the NFD half, which this port cannot do).
fn read_path_variants(resolved: &str) -> Vec<PathBuf> {
    let am_pm_variant = try_mac_os_screenshot_path(resolved);
    let curly_variant = try_curly_quote_variant(resolved);
    let nfd_curly_variant = try_curly_quote_variant(&curly_variant);

    let mut variants = Vec::new();
    if am_pm_variant != resolved {
        variants.push(PathBuf::from(am_pm_variant));
    }
    if curly_variant != resolved {
        variants.push(PathBuf::from(curly_variant));
    }
    if nfd_curly_variant != resolved {
        variants.push(PathBuf::from(nfd_curly_variant));
    }
    variants
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_resolve_against_the_working_directory() {
        let cwd = Path::new("/tmp/project");
        assert_eq!(
            resolve_to_cwd("src/main.rs", cwd),
            PathBuf::from("/tmp/project/src/main.rs")
        );
    }

    #[test]
    fn parent_segments_collapse() {
        let cwd = Path::new("/tmp/project/src");
        assert_eq!(
            resolve_to_cwd("../Cargo.toml", cwd),
            PathBuf::from("/tmp/project/Cargo.toml")
        );
    }

    #[test]
    fn a_leading_at_is_stripped() {
        let cwd = Path::new("/tmp/project");
        assert_eq!(
            resolve_to_cwd("@notes.md", cwd),
            PathBuf::from("/tmp/project/notes.md")
        );
    }

    #[test]
    fn unicode_spaces_become_ascii_spaces() {
        let cwd = Path::new("/tmp/project");
        assert_eq!(
            resolve_to_cwd("my\u{00A0}file.txt", cwd),
            PathBuf::from("/tmp/project/my file.txt")
        );
    }

    #[test]
    fn tilde_prefixed_filenames_stay_literal() {
        let cwd = Path::new("/tmp/project");
        assert_eq!(
            resolve_to_cwd("~draft.md", cwd),
            PathBuf::from("/tmp/project/~draft.md")
        );
        assert_eq!(
            resolve_to_cwd("@~draft.md", cwd),
            PathBuf::from("/tmp/project/~draft.md")
        );
    }

    #[test]
    fn the_home_directory_expands() {
        let home = home_dir();
        assert_eq!(expand_path("~"), PathBuf::from(&home));
    }

    #[test]
    fn existing_files_resolve_without_variants() {
        let dir = std::env::temp_dir().join("pi-path-utils-existing");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("test-file.txt");
        std::fs::write(&file, "content").expect("write");
        assert_eq!(resolve_read_path("test-file.txt", &dir), file);
        std::fs::remove_file(&file).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn a_curly_quote_variant_resolves_for_french_screenshot_names() {
        let dir = std::env::temp_dir().join("pi-path-utils-curly");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("Capture d\u{2019}\u{e9}cran.png");
        std::fs::write(&file, "").expect("write");
        assert_eq!(resolve_read_path("Capture d'\u{e9}cran.png", &dir), file);
        std::fs::remove_file(&file).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn an_am_pm_variant_resolves_for_macos_screenshot_names() {
        let dir = std::env::temp_dir().join("pi-path-utils-ampm");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("Screenshot 2024-01-01 at 10.30.00\u{202F}AM.png");
        std::fs::write(&file, "").expect("write");
        assert_eq!(
            resolve_read_path("Screenshot 2024-01-01 at 10.30.00 AM.png", &dir),
            file
        );
        std::fs::remove_file(&file).ok();
        std::fs::remove_dir(&dir).ok();
    }
}
