//! Changelog parsing and link normalization for `/changelog` and the
//! new-version notification.
//!
//! Ground truth: `utils/changelog.ts` plus `getChangelogPath`
//! (`config.ts:483-486`). Upstream reads the changelog shipped inside the
//! npm package; the Rust binary looks next to the executable (the bun-binary
//! convention, `config.ts:494-496`) — pi-rs ships no `CHANGELOG.md` yet, so
//! the typical result is no entries.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// One `## [x.y.z]` section of `CHANGELOG.md` (`ChangelogEntry`,
/// `changelog.ts:8-13`).
#[derive(Debug, Clone, PartialEq)]
pub struct ChangelogEntry {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub content: String,
}

impl ChangelogEntry {
    /// `entryVersion` (`changelog.ts:16-18`).
    pub fn version_string(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }
}

const GITHUB_REPO: &str = "earendil-works/pi";
const CHANGELOG_LINK_BASE_PATH: &str = "packages/coding-agent";

/// `URL_SCHEME_RE` (`changelog.ts:12`).
fn url_scheme_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)^[a-z][a-z0-9+.-]*:").expect("url scheme regex"))
}

/// `INLINE_MARKDOWN_LINK_RE` (`changelog.ts:13`): `![alt](target "title")`.
fn inline_markdown_link_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(!?\[[^\]\n]+\]\()([^\s)]+)((?:\s+[^)]*)?\))").expect("link regex")
    })
}

/// `^v?(\d+)\.(\d+)\.(\d+)` (`getNewEntries`, `changelog.ts:185`).
fn leading_version_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^v?(\d+)\.(\d+)\.(\d+)").expect("leading version regex"))
}

/// `##\s+\[?(\d+)\.(\d+)\.(\d+)\]?` (`parseChangelog`, `changelog.ts:150`).
fn version_header_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"##\s+\[?(\d+)\.(\d+)\.(\d+)\]?").expect("version header regex")
    })
}

/// `getChangelogPath` (`config.ts:483-486`).
pub fn get_changelog_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("CHANGELOG.md")))
        .unwrap_or_else(|| PathBuf::from("CHANGELOG.md"))
}

/// `normalizeTag` (`changelog.ts:20-24`).
fn normalize_tag(version: &str) -> String {
    if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    }
}

/// `splitLocalTarget` (`changelog.ts:26-41`): `(fragment, pathPart, query)`.
fn split_local_target(target: &str) -> (String, String, String) {
    let (before_hash, fragment) = match target.find('#') {
        Some(index) => (&target[..index], target[index..].to_string()),
        None => (target, String::new()),
    };
    match before_hash.find('?') {
        Some(index) => (
            fragment,
            before_hash[..index].to_string(),
            before_hash[index..].to_string(),
        ),
        None => (fragment, before_hash.to_string(), String::new()),
    }
}

/// `normalizePathPart` (`changelog.ts:44-46`).
fn normalize_path_part(value: &str) -> String {
    value.replace('\\', "/")
}

/// Node's `path.posix.normalize`: collapse `.`/`..` segments lexically,
/// keeping leading `..` segments that pop past the start.
fn posix_normalize(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.last().is_some_and(|last| *last != "..") {
                    segments.pop();
                } else {
                    segments.push("..");
                }
            }
            other => segments.push(other),
        }
    }
    let mut normalized = segments.join("/");
    if normalized.is_empty() && !path.ends_with('/') {
        return ".".to_string();
    }
    if path.ends_with('/') && !normalized.ends_with('/') {
        normalized.push('/');
    }
    normalized
}

/// `resolveRepositoryPath` (`changelog.ts:49-62`): the changelog-local path
/// resolved inside the repo, or `None` when it escapes it.
fn resolve_repository_path(target_path: &str) -> Option<String> {
    let normalized_target = normalize_path_part(target_path);
    let joined = if normalized_target.starts_with('/') {
        posix_normalize(normalized_target.trim_start_matches('/'))
    } else {
        posix_normalize(&format!("{CHANGELOG_LINK_BASE_PATH}/{normalized_target}"))
    };

    if joined == "." || joined.starts_with("../") || joined == ".." {
        return None;
    }

    Some(joined)
}

/// `isDirectoryTarget` (`changelog.ts:64-71`).
fn is_directory_target(original_path: &str, repository_path: &str) -> bool {
    if original_path.ends_with('/') {
        return true;
    }
    let basename = repository_path.rsplit('/').next().unwrap_or("");
    !basename.contains('.')
}

/// JS `encodeURI`: escapes only the bytes outside its unreserved set.
fn encode_uri(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        let unreserved = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b';' | b','
                    | b'/'
                    | b'?'
                    | b':'
                    | b'@'
                    | b'&'
                    | b'='
                    | b'+'
                    | b'$'
                    | b'-'
                    | b'_'
                    | b'.'
                    | b'!'
                    | b'~'
                    | b'*'
                    | b'\''
                    | b'('
                    | b')'
                    | b'#'
            );
        if unreserved {
            encoded.push(byte as char);
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// `normalizeChangelogLinkTarget` (`changelog.ts:73-107`): canonicalize a
/// changelog link to a pinned `blob`/`tree` URL for the entry's tag.
fn normalize_changelog_link_target(target: &str, tag: &str) -> String {
    // `LEGACY_REPO_RE` (`changelog.ts:11`): the pre-rename repo URLs,
    // anchored at the start with a `/`-or-end boundary. Rust's regex has no
    // lookahead, so the boundary is checked by hand.
    const LEGACY_BADLOGIC: &str = "https://github.com/badlogic/pi-mono";
    const LEGACY_EARENDIL: &str = "https://github.com/earendil-works/pi-mono";
    let repo_url = format!("https://github.com/{GITHUB_REPO}");
    let mut canonical_target = target.to_string();
    for prefix in [LEGACY_BADLOGIC, LEGACY_EARENDIL] {
        if let Some(rest) = target.strip_prefix(prefix) {
            if rest.is_empty() || rest.starts_with('/') {
                canonical_target = format!("{repo_url}{rest}");
            }
            break;
        }
    }

    // Floating `main`/`master` refs pin to the entry's tag
    // (`changelog.ts:79-90`).
    for route in ["blob", "tree"] {
        for branch in ["main", "master"] {
            let floating_ref_prefix = format!("{repo_url}/{route}/{branch}/");
            if let Some(rest) = canonical_target.strip_prefix(&floating_ref_prefix) {
                canonical_target = format!("{repo_url}/{route}/{tag}/{rest}");
            }
        }
    }

    if canonical_target.starts_with('#')
        || canonical_target.starts_with("//")
        || url_scheme_regex().is_match(&canonical_target)
    {
        return canonical_target;
    }

    let (fragment, path_part, query) = split_local_target(&canonical_target);
    if path_part.is_empty() {
        return canonical_target;
    }

    let Some(repository_path) = resolve_repository_path(&path_part) else {
        return canonical_target;
    };

    let route = if is_directory_target(&path_part, &repository_path) {
        "tree"
    } else {
        "blob"
    };
    format!(
        "{repo_url}/{route}/{tag}/{}{query}{fragment}",
        encode_uri(&repository_path)
    )
}

/// `normalizeChangelogLinks` (`changelog.ts:110-118`): rewrite every inline
/// markdown link's target.
pub fn normalize_changelog_links(markdown: &str, version: &str) -> String {
    let tag = normalize_tag(version);
    inline_markdown_link_regex()
        .replace_all(markdown, |caps: &regex::Captures<'_>| {
            format!(
                "{}{}{}",
                caps.get(1).map_or("", |m| m.as_str()),
                normalize_changelog_link_target(caps.get(2).map_or("", |m| m.as_str()), &tag),
                caps.get(3).map_or("", |m| m.as_str())
            )
        })
        .into_owned()
}

/// `parseChangelog` (`changelog.ts:123-175`): scan for `## [x.y.z]` headers
/// and collect content until the next one or EOF.
pub fn parse_changelog(changelog_path: &Path) -> Vec<ChangelogEntry> {
    if !changelog_path.exists() {
        return Vec::new();
    }
    let content = match std::fs::read_to_string(changelog_path) {
        Ok(content) => content,
        Err(error) => {
            eprintln!("Warning: Could not parse changelog: {error}");
            return Vec::new();
        }
    };

    let mut entries: Vec<ChangelogEntry> = Vec::new();
    let mut current_version: Option<(u32, u32, u32)> = None;
    let mut current_lines: Vec<&str> = Vec::new();

    for line in content.split('\n') {
        // Check if this is a version header (## [x.y.z] ...)
        if line.starts_with("## ") {
            // Save previous entry if exists
            if let Some((major, minor, patch)) = current_version
                && !current_lines.is_empty()
            {
                entries.push(ChangelogEntry {
                    major,
                    minor,
                    patch,
                    content: current_lines.join("\n").trim().to_string(),
                });
            }

            // Try to parse version from this line; reset if it has none.
            if let Some(caps) = version_header_regex().captures(line) {
                current_version = Some((
                    caps[1].parse().unwrap_or(0),
                    caps[2].parse().unwrap_or(0),
                    caps[3].parse().unwrap_or(0),
                ));
                current_lines = vec![line];
            } else {
                current_version = None;
                current_lines.clear();
            }
        } else if current_version.is_some() {
            // Collect lines for current version
            current_lines.push(line);
        }
    }

    // Save last entry
    if let Some((major, minor, patch)) = current_version
        && !current_lines.is_empty()
    {
        entries.push(ChangelogEntry {
            major,
            minor,
            patch,
            content: current_lines.join("\n").trim().to_string(),
        });
    }

    entries
}

/// `compareVersions` (`changelog.ts:180-184`).
pub fn compare_versions(v1: &ChangelogEntry, v2: &ChangelogEntry) -> std::cmp::Ordering {
    v1.major
        .cmp(&v2.major)
        .then(v1.minor.cmp(&v2.minor))
        .then(v1.patch.cmp(&v2.patch))
}

/// `getNewEntries` (`changelog.ts:193-205`): entries newer than
/// `lastVersion`, or `None` when it has no leading major.minor.patch to
/// compare against. Only the leading triple is read so a prerelease such as
/// `0.1.5-20261020.7` counts as `0.1.5` (#11703).
pub fn get_new_entries(
    entries: &[ChangelogEntry],
    last_version: &str,
) -> Option<Vec<ChangelogEntry>> {
    let caps = leading_version_regex().captures(last_version.trim())?;
    let last = ChangelogEntry {
        major: caps[1].parse().unwrap_or(0),
        minor: caps[2].parse().unwrap_or(0),
        patch: caps[3].parse().unwrap_or(0),
        content: String::new(),
    };

    Some(
        entries
            .iter()
            .filter(|entry| compare_versions(entry, &last) == std::cmp::Ordering::Greater)
            .cloned()
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// A synthetic changelog (`parseChangelog`, `changelog.ts:123-175`):
    /// versioned sections parse with trimmed content; a `##` line without a
    /// version splits the stream.
    #[test]
    fn parse_changelog_splits_version_sections() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("CHANGELOG.md");
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(
            b"# Changelog\n\
              \n\
              ## [1.2.3] - today\n\
              \n\
              - first entry line\n\
              - second line\n\
              \n\
              ## Notes\n\
              ignored\n\
              \n\
              ## [0.9.0]\n\
              older entry\n",
        )
        .expect("write");
        drop(file);

        let entries = parse_changelog(&path);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            ChangelogEntry {
                major: 1,
                minor: 2,
                patch: 3,
                content: "## [1.2.3] - today\n\n- first entry line\n- second line".to_string(),
            }
        );
        assert_eq!(entries[1].version_string(), "0.9.0");

        // A missing file parses as no entries.
        assert_eq!(parse_changelog(&dir.path().join("missing.md")), Vec::new());
    }

    /// `getNewEntries` (`changelog.ts:193-205`): strict newer-than on the
    /// leading triple; a non-version input yields `None`.
    #[test]
    fn get_new_entries_compares_leading_versions() {
        let entries = vec![
            ChangelogEntry {
                major: 1,
                minor: 2,
                patch: 0,
                content: String::new(),
            },
            ChangelogEntry {
                major: 1,
                minor: 1,
                patch: 9,
                content: String::new(),
            },
        ];
        let newer = get_new_entries(&entries, "v1.1.9").expect("entries");
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].version_string(), "1.2.0");
        // A prerelease of the same triple counts as that triple (#11703):
        // the 1.1.9 notes do not re-show, but strictly newer entries do.
        let prerelease = get_new_entries(&entries, "1.1.9-20261020.7").expect("entries");
        assert_eq!(prerelease.len(), 1);
        assert_eq!(prerelease[0].version_string(), "1.2.0");
        assert!(get_new_entries(&entries, "not a version").is_none());
    }

    /// `normalizeChangelogLinks` (`changelog.ts:110-118`): repo-relative
    /// paths pin to the entry's tag as `blob`/`tree` URLs; legacy repo URLs
    /// are rewritten; absolute links and fragments pass through.
    #[test]
    fn normalize_changelog_links_canonicalizes_targets() {
        // A file link resolves inside packages/coding-agent as a blob.
        assert_eq!(
            normalize_changelog_links("[readme](README.md)", "1.2.3"),
            "[readme](https://github.com/earendil-works/pi/blob/v1.2.3/packages/coding-agent/README.md)"
        );
        // A trailing slash marks a directory (tree) and survives into the
        // URL: `path.posix.normalize` keeps it, so the target ends `docs/`.
        assert_eq!(
            normalize_changelog_links("[docs](docs/)", "1.2.3"),
            "[docs](https://github.com/earendil-works/pi/tree/v1.2.3/packages/coding-agent/docs/)"
        );
        // A floating main ref pins to the tag.
        assert_eq!(
            normalize_changelog_links(
                "[pi](https://github.com/earendil-works/pi/blob/main/README.md)",
                "1.2.3"
            ),
            "[pi](https://github.com/earendil-works/pi/blob/v1.2.3/README.md)"
        );
        // The legacy repo URL is rewritten and pinned.
        assert_eq!(
            normalize_changelog_links(
                "[old](https://github.com/badlogic/pi-mono/tree/main/packages/coding-agent)",
                "1.2.3"
            ),
            "[old](https://github.com/earendil-works/pi/tree/v1.2.3/packages/coding-agent)"
        );
        // Fragments, scheme links, and images keep their shape.
        assert_eq!(
            normalize_changelog_links("[issues](#issues)", "1.2.3"),
            "[issues](#issues)"
        );
        assert_eq!(
            normalize_changelog_links("![logo](https://example.com/logo.png)", "1.2.3"),
            "![logo](https://example.com/logo.png)"
        );
        // Paths escaping the repo are left alone (`resolveRepositoryPath`
        // returns None above `../`).
        assert_eq!(
            normalize_changelog_links("[up](../../../outside.md)", "1.2.3"),
            "[up](../../../outside.md)"
        );
    }

    /// `encodeURI`-compatible escaping: spaces become `%20`, unreserved
    /// characters stay literal.
    #[test]
    fn encode_uri_escapes_like_the_js_global() {
        assert_eq!(encode_uri("a b.md"), "a%20b.md");
        assert_eq!(encode_uri("x(1)-y_z.txt"), "x(1)-y_z.txt");
        assert_eq!(encode_uri("n\u{f6}vel"), "n%C3%B6vel");
    }
}
