//! Extension discovery and loading.
//!
//! Ports the CLI loading path (`discoverAndLoadExtensions`,
//! `core/extensions/loader.ts:840-888`): project `.pi/extensions` first,
//! then user `agentDir/extensions`, then explicit paths (directories
//! resolve through the entry rules), with in-directory discovery one level
//! deep (`discoverExtensionsInDir`, `loader.ts:803-835`) and
//! errors-as-values (`loader.ts:646-668`). Trust gating is the caller's
//! composition: the pre-trust pass loads only non-project origins so
//! project extensions cannot handle `project_trust`
//! (`resource-loader.ts:533-553`).

use std::path::{Path, PathBuf};

use crate::error::LoadError;
use crate::resolver::{resolve_directory_entries, DirectoryEntry};
use crate::runner::ExtensionRunner;

/// Where a discovered extension path came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionOrigin {
    /// `cwd/.pi/extensions` — loaded only after the project is trusted.
    Project,
    /// `agentDir/extensions` — always loaded.
    User,
    /// Explicitly configured (`-e`, settings, builtin).
    Explicit,
}

/// One discovered extension entry point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPath {
    /// The path (or synthetic path) to load.
    pub path: PathBuf,
    /// Where it came from.
    pub origin: ExtensionOrigin,
}

/// Discovery for one directory (`discoverExtensionsInDir`): direct
/// `.ts`/`.js` files, then subdirectories through the entry rules
/// (`package.json` `pi.extensions` manifest, else `index.ts`/`index.js`).
/// Read failures return an empty list, as upstream's catch does.
pub fn discover_extensions_in_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut discovered = Vec::new();
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        let is_dir = path.is_dir();
        let is_file = path.is_file() || path.is_symlink();
        if is_file && is_extension_file(&path) {
            discovered.push(path);
            continue;
        }
        if is_dir {
            if let Some(entry) = resolve_directory_entries(&path) {
                match entry {
                    DirectoryEntry::Manifest(entries) => discovered.extend(entries),
                    DirectoryEntry::Index(index) => discovered.push(index),
                }
            }
        }
    }
    discovered
}

fn is_extension_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("ts" | "js")
    )
}

/// Discovery order of the real CLI path (`discoverAndLoadExtensions`):
/// 1. project `cwd/.pi/extensions`,
/// 2. user `agentDir/extensions`,
/// 3. explicit paths — a directory resolves through the entry rules, else
///    discovers its contents.
///
/// Duplicates (by resolved path) are dropped, keeping the first
/// occurrence.
pub fn discover_extensions(configured_paths: &[String], cwd: &Path, agent_dir: &Path) -> Vec<DiscoveredPath> {
    let mut all: Vec<DiscoveredPath> = Vec::new();
    let push = |path: PathBuf, origin: ExtensionOrigin, all: &mut Vec<DiscoveredPath>| {
        if !all.iter().any(|existing| existing.path == path) {
            all.push(DiscoveredPath { path, origin });
        }
    };
    let project_dir = cwd.join(".pi").join("extensions");
    for path in discover_extensions_in_dir(&project_dir) {
        push(path, ExtensionOrigin::Project, &mut all);
    }
    let user_dir = agent_dir.join("extensions");
    for path in discover_extensions_in_dir(&user_dir) {
        push(path, ExtensionOrigin::User, &mut all);
    }
    for configured in configured_paths {
        let resolved = if Path::new(configured).is_absolute() {
            PathBuf::from(configured)
        } else {
            cwd.join(configured)
        };
        if resolved.is_dir() {
            match resolve_directory_entries(&resolved) {
                Some(DirectoryEntry::Manifest(entries)) => {
                    for entry in entries {
                        push(entry, ExtensionOrigin::Explicit, &mut all);
                    }
                    continue;
                }
                Some(DirectoryEntry::Index(index)) => {
                    push(index, ExtensionOrigin::Explicit, &mut all);
                    continue;
                }
                None => {
                    for path in discover_extensions_in_dir(&resolved) {
                        push(path, ExtensionOrigin::Explicit, &mut all);
                    }
                    continue;
                }
            }
        }
        push(resolved, ExtensionOrigin::Explicit, &mut all);
    }
    all
}

/// The pre-trust split (`loadProjectTrustExtensions`,
/// `resource-loader.ts:533-541`): the bootstrap pass loads only
/// non-project origins, so only global/CLI extensions can decide
/// `project_trust`; the full pass adds the project origins when trusted.
pub fn split_for_trust(discovered: &[DiscoveredPath], project_trusted: bool) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let bootstrap: Vec<PathBuf> = discovered
        .iter()
        .filter(|entry| entry.origin != ExtensionOrigin::Project)
        .map(|entry| entry.path.clone())
        .collect();
    let full = if project_trusted {
        discovered.iter().map(|entry| entry.path.clone()).collect()
    } else {
        bootstrap.clone()
    };
    (bootstrap, full)
}

/// The load outcome: loaded extension paths plus per-path errors — values,
/// never throws (`LoadExtensionsResult.errors`).
#[derive(Debug, Default)]
pub struct LoadExtensionsResult {
    /// Paths that loaded, in load order.
    pub extensions: Vec<String>,
    /// Per-path failures with the upstream diagnostic message.
    pub errors: Vec<LoadError>,
}

/// Loads a path list into the runner, collecting errors as values
/// (`loadExtensionsInternal`, `loader.ts:688-728`: one failing extension
/// never stops the others).
pub fn load_paths(runner: &mut ExtensionRunner, paths: &[PathBuf]) -> LoadExtensionsResult {
    let mut result = LoadExtensionsResult::default();
    for path in paths {
        let display = path.to_string_lossy().into_owned();
        match runner.load_extension(&display, None) {
            Ok(()) => result.extensions.push(display),
            Err(error) => result.errors.push(LoadError {
                path: display,
                error: format!("Failed to load extension: {error}"),
            }),
        }
    }
    result
}

/// Discovery + loading in one call (`discoverAndLoadExtensions`).
pub fn discover_and_load(
    runner: &mut ExtensionRunner,
    configured_paths: &[String],
    cwd: &Path,
    agent_dir: &Path,
) -> LoadExtensionsResult {
    let discovered = discover_extensions(configured_paths, cwd, agent_dir);
    load_paths(runner, &discovered.iter().map(|entry| entry.path.clone()).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_tree(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-ext-js-loader-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir parent");
        }
        std::fs::write(path, contents).expect("write");
    }

    const FACTORY: &str = "export default function (pi) { pi.on(\"e\", () => 1); }\n";

    #[test]
    fn discovers_files_manifests_and_indexes() {
        let dir = temp_tree("discover");
        write(&dir.join("plain.ts"), FACTORY);
        write(&dir.join("pack").join("index.ts"), FACTORY);
        write(
            &dir.join("manifest").join("package.json"),
            r#"{"name":"m","pi":{"extensions":["deep/first.ts"]}}"#,
        );
        write(&dir.join("manifest").join("deep").join("first.ts"), FACTORY);
        write(&dir.join("ignored.txt"), "not an extension");

        let found = discover_extensions_in_dir(&dir);
        let names: Vec<String> = found
            .iter()
            .map(|path| path.strip_prefix(&dir).expect("prefix").to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "manifest/deep/first.ts".to_string(),
                "pack/index.ts".to_string(),
                "plain.ts".to_string()
            ]
        );
    }

    #[test]
    fn index_ts_wins_over_index_js_and_manifest_wins_over_index() {
        let dir = temp_tree("entries");
        write(&dir.join("both").join("index.js"), FACTORY);
        write(&dir.join("both").join("index.ts"), FACTORY);
        write(&dir.join("manifested").join("index.js"), FACTORY);
        write(
            &dir.join("manifested").join("package.json"),
            r#"{"pi":{"extensions":["alt.ts"]}}"#,
        );
        write(&dir.join("manifested").join("alt.ts"), FACTORY);

        assert_eq!(
            resolve_directory_entries(&dir.join("both")),
            Some(DirectoryEntry::Index(dir.join("both").join("index.ts")))
        );
        assert_eq!(
            resolve_directory_entries(&dir.join("manifested")),
            Some(DirectoryEntry::Manifest(vec![dir.join("manifested").join("alt.ts")]))
        );
    }

    #[test]
    fn manifest_with_missing_paths_falls_back_to_index() {
        let dir = temp_tree("manifest-miss");
        write(&dir.join("pkg").join("index.js"), FACTORY);
        write(
            &dir.join("pkg").join("package.json"),
            r#"{"pi":{"extensions":["missing.ts"]}}"#,
        );
        assert_eq!(
            resolve_directory_entries(&dir.join("pkg")),
            Some(DirectoryEntry::Index(dir.join("pkg").join("index.js")))
        );
    }

    #[test]
    fn discovery_order_project_user_explicit_with_dedup() {
        let root = temp_tree("order");
        let cwd = root.join("project");
        let agent = root.join("agent");
        write(&cwd.join(".pi").join("extensions").join("project_ext.ts"), FACTORY);
        write(&agent.join("extensions").join("user_ext.ts"), FACTORY);
        let explicit = root.join("extra.ts");
        write(&explicit, FACTORY);

        let found = discover_extensions(&[explicit.to_string_lossy().into_owned()], &cwd, &agent);
        let origins: Vec<(String, ExtensionOrigin)> = found
            .iter()
            .map(|entry| (entry.path.to_string_lossy().into_owned(), entry.origin))
            .collect();
        assert_eq!(
            origins,
            vec![
                (cwd.join(".pi/extensions/project_ext.ts").to_string_lossy().into_owned(), ExtensionOrigin::Project),
                (agent.join("extensions/user_ext.ts").to_string_lossy().into_owned(), ExtensionOrigin::User),
                (explicit.to_string_lossy().into_owned(), ExtensionOrigin::Explicit),
            ]
        );

        // The same path configured explicitly does not load twice.
        let project_path = cwd.join(".pi/extensions/project_ext.ts").to_string_lossy().into_owned();
        let found = discover_extensions(&[project_path], &cwd, &agent);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn trust_split_excludes_project_before_the_decision() {
        let discovered = vec![
            DiscoveredPath {
                path: PathBuf::from("/p/proj.ts"),
                origin: ExtensionOrigin::Project,
            },
            DiscoveredPath {
                path: PathBuf::from("/u/user.ts"),
                origin: ExtensionOrigin::User,
            },
        ];
        let (bootstrap, full) = split_for_trust(&discovered, false);
        assert_eq!(bootstrap, vec![PathBuf::from("/u/user.ts")]);
        assert_eq!(full, vec![PathBuf::from("/u/user.ts")]);
        let (_, full) = split_for_trust(&discovered, true);
        assert_eq!(full, vec![PathBuf::from("/p/proj.ts"), PathBuf::from("/u/user.ts")]);
    }

    #[test]
    fn load_errors_are_values_and_ones_failure_does_not_stop_the_rest() {
        let dir = temp_tree("errors");
        let good = dir.join("good.ts");
        let bad = dir.join("bad.ts");
        let missing = dir.join("missing.ts");
        write(&good, FACTORY);
        write(&bad, "export default function (pi) { pi.registerTool({ name: \"x\", description: \"\", parameters: null }); }\n");

        let mut runner = crate::runner::ExtensionRunner::new(crate::engine::HostCore::default()).expect("runner");
        let result = load_paths(&mut runner, &[bad, good.clone(), missing]);
        assert_eq!(result.extensions, vec![good.to_string_lossy().into_owned()]);
        assert_eq!(result.errors.len(), 2);
        assert!(result.errors[0].error.contains("Failed to load extension:"));
        assert!(result.errors[0].error.contains("must define an object parameter schema"));
        assert!(result.errors[1].path.ends_with("missing.ts"));
        // The good extension still works.
        assert!(runner.has_handlers("e"));
    }
}
