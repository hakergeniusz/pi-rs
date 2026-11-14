//! Project trust (port of `core/trust-manager.ts` + `core/project-trust.ts`).
//!
//! Trust decisions live in `<agentDir>/trust.json`: a JSON object mapping
//! canonical project paths to `true`/`false` (`null` deletes the entry), with
//! nearest-ancestor lookup and sorted keys on write (`trust-manager.ts:217-254`).
//! Gating only bites when the project carries trust-requiring resources —
//! the trust-requiring entries under `.pi/` or `.agents/skills` in the cwd or
//! an ancestor (`trust-manager.ts:30-39,193-215`); a project without them is
//! trusted by default. The decision ladder is `resolveProjectTrusted`
//! (`project-trust.ts:47-97`): `--approve`/`--no-approve` override, then the
//! resource check, then the store, then the `defaultProjectTrust` setting,
//! then — with no UI, i.e. print/json/rpc modes — **not trusted**. Upstream
//! prompts only through the interactive selector; the port's interactive
//! prompt arrives with P6. The extension `project_trust` hook lands with the
//! extension host (P5).
//!
//! All store/resource functions take the agent dir as a parameter so tests
//! never touch the process environment (the crate forbids `unsafe`, which
//! rules out `std::env::set_var`).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::config::PROJECT_CONFIG_DIR_NAME;

/// `TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES` (`trust-manager.ts:30-39`): the
/// `.pi/` entries whose presence makes a project trust-gated.
const TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES: [&str; 8] = [
    "settings.json",
    "mcp.json",
    "extensions",
    "skills",
    "prompts",
    "themes",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
];

/// The `defaultProjectTrust` setting (`settings-manager.ts`); `"ask"` is the
/// shipped default (`settings-defaults.ts:30`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultProjectTrust {
    Always,
    Never,
    Ask,
}

/// `ProjectTrustStore` (`trust-manager.ts:217-254`): the `trust.json` file in
/// the agent dir.
pub struct ProjectTrustStore {
    trust_path: PathBuf,
}

impl ProjectTrustStore {
    /// `new ProjectTrustStore(agentDir)`: `<agentDir>/trust.json`.
    pub fn new(agent_dir: &Path) -> Self {
        Self {
            trust_path: agent_dir.join("trust.json"),
        }
    }

    /// `get`: the nearest recorded ancestor decision (`findNearestTrustEntry`,
    /// `trust-manager.ts:50-66`).
    ///
    /// # Errors
    /// When `trust.json` exists but cannot be parsed.
    pub fn get(&self, cwd: &Path) -> Result<Option<bool>, String> {
        let data = self.read()?;
        Ok(find_nearest_trust_entry(&data, cwd))
    }

    /// `set(cwd, decision)`.
    ///
    /// # Errors
    /// Read or write failures carry upstream's wording.
    pub fn set(&self, cwd: &Path, decision: bool) -> Result<(), String> {
        self.set_many(&[(cwd.to_path_buf(), Some(decision))])
    }

    /// `setMany` (`trust-manager.ts:239-253`): keys are canonicalized,
    /// `None` decisions delete existing entries, and the file is written
    /// with sorted keys.
    ///
    /// # Errors
    /// Read or write failures carry upstream's wording.
    pub fn set_many(&self, updates: &[(PathBuf, Option<bool>)]) -> Result<(), String> {
        let mut data = self.read()?;
        for (path, decision) in updates {
            let key = canonical_dir(path);
            data.remove(&key);
            if let Some(decision) = decision {
                data.insert(key, *decision);
            }
        }
        self.write(&data)
    }

    fn read(&self) -> Result<BTreeMap<String, bool>, String> {
        let Ok(raw) = std::fs::read_to_string(&self.trust_path) else {
            // Missing file: an empty store, like `readTrustFile`'s existsSync
            // guard (`trust-manager.ts:107-109`).
            return Ok(BTreeMap::new());
        };
        let parsed: serde_json::Value = serde_json::from_str(raw.trim_start_matches('\u{feff}'))
            .map_err(|error| {
                format!(
                    "Failed to read trust store {}: {error}",
                    self.trust_path.display()
                )
            })?;
        let serde_json::Value::Object(fields) = parsed else {
            return Err(format!(
                "Invalid trust store {}: expected an object",
                self.trust_path.display()
            ));
        };
        let mut data = BTreeMap::new();
        for (key, value) in fields {
            let Some(decision) = value.as_bool() else {
                return Err(format!(
                    "Invalid trust store {}: value for {key:?} must be true, false, or null",
                    self.trust_path.display()
                ));
            };
            data.insert(key, decision);
        }
        Ok(data)
    }

    fn write(&self, data: &BTreeMap<String, bool>) -> Result<(), String> {
        if let Some(parent) = self.trust_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("Failed to write trust store: {error}"))?;
        }
        let mut serialized =
            serde_json::to_string_pretty(data).map_err(|error| error.to_string())?;
        serialized.push('\n');
        // Trust decisions gate code execution; write via a temp file + rename
        // so a crash cannot leave a truncated store.
        let temp_path = self.trust_path.with_extension("json.tmp");
        {
            let mut file = std::fs::File::create(&temp_path)
                .map_err(|error| format!("Failed to write trust store: {error}"))?;
            file.write_all(serialized.as_bytes())
                .map_err(|error| format!("Failed to write trust store: {error}"))?;
        }
        std::fs::rename(&temp_path, &self.trust_path)
            .map_err(|error| format!("Failed to write trust store: {error}"))
    }
}

/// `findNearestTrustEntry`: walk from the cwd upward; the first recorded
/// ancestor wins.
fn find_nearest_trust_entry(data: &BTreeMap<String, bool>, cwd: &Path) -> Option<bool> {
    let mut current = canonical_dir(cwd);
    loop {
        if let Some(decision) = data.get(&current) {
            return Some(*decision);
        }
        let parent = PathBuf::from(&current)
            .parent()
            .map_or_else(|| current.clone(), canonical_dir);
        if parent == current {
            return None;
        }
        current = parent;
    }
}

/// `normalizeCwd` (`trust-manager.ts:41-43`): resolve the real path when the
/// directory exists, else normalize lexically.
fn canonical_dir(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| normalize_lexical(path));
    // `pathsEqual` is exact on unix; the string form is the store key.
    canonical.to_string_lossy().to_string()
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// `hasTrustRequiringProjectResources` (`trust-manager.ts:193-215`): the
/// trust-requiring `.pi/` entries, or `.agents/skills` in the cwd or an
/// ancestor (the user-global `$HOME/.agents/skills` never counts).
pub fn has_trust_requiring_project_resources(cwd: &Path) -> bool {
    let config_dir = cwd.join(PROJECT_CONFIG_DIR_NAME);
    if TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES
        .iter()
        .any(|entry| config_dir.join(entry).exists())
    {
        return true;
    }
    let user_agents_skills_dir = dirs::home_dir().map_or_else(
        || PathBuf::from("/nonexistent/.agents/skills"),
        |home| home.join(".agents").join("skills"),
    );
    let mut current = cwd.to_path_buf();
    loop {
        let agents_skills_dir = current.join(".agents").join("skills");
        if agents_skills_dir != user_agents_skills_dir && agents_skills_dir.exists() {
            return true;
        }
        let Some(parent) = current.parent() else {
            return false;
        };
        if parent == current {
            return false;
        }
        current = parent.to_path_buf();
    }
}

/// `ResolveProjectTrustedOptions` (`project-trust.ts:15-23`), minus the
/// extension hook (P5) and the interactive UI (P6).
pub struct ResolveProjectTrustedOptions<'a> {
    pub cwd: &'a Path,
    pub store: &'a ProjectTrustStore,
    /// `--approve`/`--no-approve` (`Args.projectTrustOverride`).
    pub trust_override: Option<bool>,
    /// The `defaultProjectTrust` setting; `None` means "ask".
    pub default_project_trust: Option<DefaultProjectTrust>,
}

/// `resolveProjectTrusted` (`project-trust.ts:47-97`). Returns whether
/// project-local resources may load.
///
/// # Errors
/// Store read/write failures.
pub fn resolve_project_trusted(options: &ResolveProjectTrustedOptions<'_>) -> Result<bool, String> {
    if let Some(trusted) = options.trust_override {
        return Ok(trusted);
    }
    if !has_trust_requiring_project_resources(options.cwd) {
        return Ok(options.store.get(options.cwd)?.unwrap_or(true));
    }
    if let Some(decision) = options.store.get(options.cwd)? {
        return Ok(decision);
    }
    match options
        .default_project_trust
        .unwrap_or(DefaultProjectTrust::Ask)
    {
        DefaultProjectTrust::Always => Ok(true),
        // "never" is direct policy; "ask" with no UI answers false because
        // upstream's selector is unreachable outside the interactive mode
        // (`project-trust.ts:87-89`).
        DefaultProjectTrust::Never | DefaultProjectTrust::Ask => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    // `assert!(v.is_none())` reads better than clippy's `assert_eq!` shape
    // for these option-flow assertions.
    #![allow(clippy::assert_is_empty)]

    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn store_roundtrips_through_trust_json() {
        let dir = tempdir();
        let agent_dir = dir.path().join("agent");
        let store = ProjectTrustStore::new(&agent_dir);
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).expect("mkdir");
        assert!(store.get(&project).expect("read").is_none());
        store.set(&project, false).expect("write");
        assert_eq!(store.get(&project).expect("read"), Some(false));
        store.set(&project, true).expect("write");
        assert_eq!(store.get(&project).expect("read"), Some(true));
        // Keys are canonical absolute paths in sorted order, 2-space pretty.
        let raw = std::fs::read_to_string(agent_dir.join("trust.json")).expect("read raw");
        let key = canonical_dir(&project);
        assert_eq!(
            raw,
            format!("{{\n  \"{key}\": true\n}}\n"),
            "trust.json shape"
        );
    }

    #[test]
    fn nearest_ancestor_decision_wins() {
        let dir = tempdir();
        let store = ProjectTrustStore::new(&dir.path().join("agent"));
        let parent = dir.path().join("parent");
        let child = parent.join("child");
        std::fs::create_dir_all(&child).expect("mkdir");
        store.set(&parent, true).expect("write");
        assert_eq!(store.get(&child).expect("read"), Some(true));
        // An explicit decision on the child overrides the parent.
        store.set(&child, false).expect("write");
        assert_eq!(store.get(&child).expect("read"), Some(false));
    }

    #[test]
    fn null_decision_deletes_the_entry() {
        let dir = tempdir();
        let store = ProjectTrustStore::new(&dir.path().join("agent"));
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).expect("mkdir");
        store.set(&project, true).expect("write");
        store.set_many(&[(project.clone(), None)]).expect("write");
        assert!(store.get(&project).expect("read").is_none());
        let raw = std::fs::read_to_string(&store.trust_path).expect("read raw");
        assert_eq!(raw, "{}\n");
    }

    #[test]
    fn invalid_trust_store_errors_like_upstream() {
        let dir = tempdir();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).expect("mkdir");
        std::fs::write(agent_dir.join("trust.json"), "[1,2]").expect("write");
        let store = ProjectTrustStore::new(&agent_dir);
        let error = store.get(dir.path()).unwrap_err();
        assert!(error.contains("expected an object"), "{error}");
    }

    #[test]
    fn pi_resources_require_trust() {
        let dir = tempdir();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(project.join(".pi")).expect("mkdir");
        assert!(!has_trust_requiring_project_resources(&project));
        std::fs::write(project.join(".pi").join("settings.json"), "{}").expect("write");
        assert!(has_trust_requiring_project_resources(&project));
    }

    #[test]
    fn agents_skills_in_ancestors_require_trust() {
        let dir = tempdir();
        let parent = dir.path().join("mono");
        let child = parent.join("packages").join("app");
        std::fs::create_dir_all(parent.join(".agents").join("skills")).expect("mkdir");
        std::fs::create_dir_all(&child).expect("mkdir");
        assert!(has_trust_requiring_project_resources(&child));
    }

    #[test]
    fn resolve_ladder_order() {
        let dir = tempdir();
        let agent_dir = dir.path().join("agent");
        let store = ProjectTrustStore::new(&agent_dir);
        let project = dir.path().join("proj");
        // No resources: trusted even with nothing stored.
        let trusted = resolve_project_trusted(&ResolveProjectTrustedOptions {
            cwd: &project,
            store: &store,
            trust_override: None,
            default_project_trust: None,
        })
        .expect("resolve");
        assert!(trusted);
        // The override wins over everything.
        let trusted = resolve_project_trusted(&ResolveProjectTrustedOptions {
            cwd: &project,
            store: &store,
            trust_override: Some(false),
            default_project_trust: Some(DefaultProjectTrust::Always),
        })
        .expect("resolve");
        assert!(!trusted);
        // With gated resources and no stored decision, "ask" resolves to
        // false outside the interactive mode.
        std::fs::create_dir_all(project.join(".pi").join("extensions")).expect("mkdir");
        let trusted = resolve_project_trusted(&ResolveProjectTrustedOptions {
            cwd: &project,
            store: &store,
            trust_override: None,
            default_project_trust: None,
        })
        .expect("resolve");
        assert!(!trusted);
        // A stored decision wins before the setting.
        store.set(&project, true).expect("write");
        let trusted = resolve_project_trusted(&ResolveProjectTrustedOptions {
            cwd: &project,
            store: &store,
            trust_override: None,
            default_project_trust: Some(DefaultProjectTrust::Never),
        })
        .expect("resolve");
        assert!(trusted);
    }
}
