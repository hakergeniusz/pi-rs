//! The package manager (P5 slice): extension sources behind `pi install` /
//! `pi remove` / `pi list`.
//!
//! Ground truth: `packages/coding-agent/src/core/package-manager.ts` with the
//! source parsing from `utils/git.ts` (`parseGitUrl`/`splitRef`/
//! `parseGenericGitUrl`/`buildGitSource`) and `utils/paths.ts`
//! (`isLocalPath`). This slice ports the git + local source arms and the
//! settings persistence verbatim (install `:1106-1160`, git install
//! `:2069-2126`, git update machinery `:2169-2231` + `:1774-1830`, remove
//! `:1135-1159` + `:2233-2252`, settings `:902-945`, list `:1078-1103`,
//! match keys `:1576-1648`); the npm arm (registry resolution, tarball
//! installs, the managed-install machinery, `package-manager.ts:1357-1570`
//! + `:2044-2068`) is deferred with an honest error — see HANDOFF.
//!
//! CLI texts live in [`crate::cli::package_command`].
//!
//! Deviations (mechanism only, HANDOFF has the ledger):
//! - Upstream reports progress through a callback the CLI renders; the port
//!   prints the start line directly in the CLI handler (the in-flight
//!   progress events only fire on the npm/update paths).
//! - `chalk` output is plain here — upstream's chalk degrades to plain on
//!   non-TTY stdout, and CLI subcommands run before any TUI exists.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::PROJECT_CONFIG_DIR_NAME;
use crate::config::settings::SettingsManager;

/// One git remote parse (`GitSource`, `package-manager.ts:186-192`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSource {
    pub repo: String,
    pub host: String,
    pub path: String,
    pub ref_: Option<String>,
    pub pinned: bool,
}

/// One parsed source (`parseSource`, `package-manager.ts:1650-1680`): an
/// npm spec (parsed for the match keys; installation is deferred), a git
/// remote, or a local filesystem path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedSource {
    Npm {
        spec: String,
        name: String,
        version: Option<String>,
    },
    Git(GitSource),
    Local(String),
}

/// A configured packages entry as `pi list` reports it
/// (`ConfiguredPackage`, `package-manager.ts:1078-1103`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredPackage {
    pub source: String,
    pub scope: Scope,
    /// An object entry in settings — upstream's resource-filter form
    /// (`typeof pkg === "object"`).
    pub filtered: bool,
    pub installed_path: Option<PathBuf>,
}

/// Where a package is configured (`SourceScope`, `:166` — the temporary
/// scope only exists on the npm/runtime path and is not ported).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    Project,
}

/// `isLocalPath` (`utils/paths.ts:94-109`): the known non-local prefixes
/// are package-manager sources; everything else is a filesystem path.
/// Re-exported from the extensions loader, which gates resource discovery
/// on the same predicate.
pub(crate) use crate::extensions::is_local_path;

/// `parseNpmSpec` (`:1938-1945`): the name is everything before the first
/// `@` past the leading scope marker (the greedy `[^@]+` run), the version
/// the rest — a trailing empty version leaves the spec unchanged (the
/// regex requires `.+`).
fn parse_npm_spec(spec: &str) -> (String, Option<String>) {
    let minimum = usize::from(spec.starts_with('@')) + 1;
    let bytes = spec.as_bytes();
    let mut index = minimum;
    while index < bytes.len() {
        if bytes[index] == b'@' {
            if index + 1 < bytes.len() {
                return (
                    spec[..index].to_string(),
                    Some(spec[index + 1..].to_string()),
                );
            }
            break;
        }
        index += 1;
    }
    (spec.to_string(), None)
}

/// Percent-decodes for the install-part validation
/// (`decodeForValidation`, `utils/git.ts:64-82`): `None` on invalid
/// escapes or non-UTF-8 output, which the validator treats as unsafe.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = bytes.get(index + 1..index + 3)?;
                let high = (hex[0] as char).to_digit(16)?;
                let low = (hex[1] as char).to_digit(16)?;
                out.push((high * 16 + low) as u8);
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// `hasUnsafeGitInstallPart` (`utils/git.ts:84-102`): path-traversal and
/// separator rejection over the raw and percent-decoded forms.
fn has_unsafe_git_install_part(value: &str, allow_slash: bool) -> bool {
    let Some(decoded) = percent_decode(value) else {
        return true;
    };
    for candidate in [value, decoded.as_str()] {
        if candidate.contains('\0') || candidate.contains('\\') || candidate.starts_with('/') {
            return true;
        }
        if !allow_slash && candidate.contains('/') {
            return true;
        }
        if candidate.split('/').any(|part| part == "..") {
            return true;
        }
    }
    false
}

/// `buildGitSource` (`utils/git.ts:104-124`): normalize the `.git`
/// suffix, require `host` and a two-segment path, reject unsafe parts.
fn build_git_source(
    repo: String,
    host: &str,
    path: &str,
    ref_: Option<String>,
) -> Option<GitSource> {
    if path.starts_with('/') {
        return None;
    }
    let normalized = path
        .trim_end_matches(".git")
        .trim_start_matches('/')
        .to_string();
    if host.is_empty() || normalized.is_empty() || normalized.split('/').count() < 2 {
        return None;
    }
    if has_unsafe_git_install_part(host, false) || has_unsafe_git_install_part(&normalized, true) {
        return None;
    }
    Some(GitSource {
        repo,
        host: host.to_string(),
        path: normalized,
        pinned: ref_.is_some(),
        ref_,
    })
}

/// `splitRef` (`utils/git.ts:21-72`): the scp-like `git@host:path@ref`
/// form, the `@ref` inside a `://` URL's path segment (the `URL` class
/// keeps query/hash out of `pathname`, so a trailing `#fragment` is NOT a
/// ref there), or the bare `host/path(@ref)` form.
fn split_ref(url: &str) -> (String, Option<String>) {
    // scp-like: `^git@([^:]+):(.+)$`.
    if let Some(rest) = url.strip_prefix("git@")
        && let Some((host, path_with_maybe_ref)) = rest.split_once(':')
    {
        if let Some(at) = path_with_maybe_ref.find('@') {
            let repo_path = &path_with_maybe_ref[..at];
            let ref_ = &path_with_maybe_ref[at + 1..];
            if !repo_path.is_empty() && !ref_.is_empty() {
                return (format!("git@{host}:{repo_path}"), Some(ref_.to_string()));
            }
        }
        return (url.to_string(), None);
    }
    if let Some(separator) = url.find("://") {
        // The path segment runs to the query or fragment; the ref rides
        // the path only (the `URL` class excludes query/hash from
        // `pathname`), and the rebuilt repo keeps the rest verbatim.
        let after_scheme = &url[separator + 3..];
        let host_end = after_scheme.find('/').unwrap_or(after_scheme.len());
        let path_and_rest = &after_scheme[host_end..];
        let path_end = path_and_rest
            .find(['?', '#'])
            .unwrap_or(path_and_rest.len());
        let path = &path_and_rest[..path_end];
        let tail = &path_and_rest[path_end..];
        let trimmed = path.trim_start_matches('/');
        if let Some(at) = trimmed.find('@') {
            let repo_path = &trimmed[..at];
            let ref_ = &trimmed[at + 1..];
            if !repo_path.is_empty() && !ref_.is_empty() {
                let mut repo = format!("{}/{}", &url[..separator + 3 + host_end], repo_path);
                repo.push_str(tail);
                return (
                    repo.trim_end_matches('/').to_string(),
                    Some(ref_.to_string()),
                );
            }
        }
        return (url.to_string(), None);
    }
    // Bare `host/path(@ref)`: host up to the first `/`.
    let Some(slash) = url.find('/') else {
        return (url.to_string(), None);
    };
    let host = &url[..slash];
    let path_with_maybe_ref = &url[slash + 1..];
    let Some(at) = path_with_maybe_ref.find('@') else {
        return (url.to_string(), None);
    };
    let repo_path = &path_with_maybe_ref[..at];
    let ref_ = &path_with_maybe_ref[at + 1..];
    if repo_path.is_empty() || ref_.is_empty() {
        return (url.to_string(), None);
    }
    (format!("{host}/{repo_path}"), Some(ref_.to_string()))
}

/// Splits `scheme://rest` — the helper behind [`parse_generic_git_url`].
fn split_scheme(url: &str) -> Option<(&str, usize)> {
    let separator = url.find("://")?;
    let rest_start = separator + 3;
    Some((&url[..separator], rest_start))
}

/// The hosted shortcut hosts (`hostedGitInfo`'s domain table subset) —
/// `#committish` splitting on these hosts is what the generic parse
/// misses.
fn hosted_domain(host: &str) -> bool {
    matches!(host, "github.com" | "gitlab.com" | "bitbucket.org")
}

/// `parseGitUrl`'s guard regex: a remote URL scheme.
fn has_remote_scheme(url: &str) -> bool {
    ["http://", "https://", "ssh://", "git://"]
        .iter()
        .any(|scheme| url.len() >= scheme.len() && url[..scheme.len()].eq_ignore_ascii_case(scheme))
}

/// `parseGitUrl` (`utils/git.ts:172-231`): an explicit `git:` prefix or a
/// remote URL scheme passes the guard, the ref splits off, and the
/// hosted candidates plus the generic parse build the source. The hosted
/// layer and `parseGenericGitUrl` agree on every reachable form (host,
/// owner/project, `.git` trim, the `https://` default) — the hosted
/// candidates only add `#committish` handling, which this port applies
/// for the known hosts before the generic fallback.
pub fn parse_git_url(source: &str) -> Option<GitSource> {
    let trimmed = source.trim();
    let has_git_prefix = trimmed.starts_with("git:");
    let url = trimmed.strip_prefix("git:").map_or(trimmed, str::trim);

    if !has_git_prefix && !has_remote_scheme(url) {
        return None;
    }

    let (repo, ref_) = split_ref(url);

    // The hosted candidates (`repo#ref` first, then the raw url): a bare
    // or schemed URL of a known host carries `#committish` in the string
    // (upstream's `hostedGitInfo.fromUrl`).
    let hosted = match ref_.as_deref() {
        Some(ref_) => {
            hosted_candidate(&format!("{repo}#{ref_}")).or_else(|| hosted_candidate(&repo))
        }
        None => hosted_candidate(&repo),
    };
    if let Some(source) = hosted {
        return Some(source);
    }

    parse_generic_git_url(url)
}

/// One hosted candidate: `[git@|scheme://]host/owner/project[#ref]` over
/// the known hosts, mirroring `hostedGitInfo.fromUrl`'s outcome.
fn hosted_candidate(candidate: &str) -> Option<GitSource> {
    let (repo_part, committish) = match candidate.find('#') {
        Some(hash) => (&candidate[..hash], Some(&candidate[hash + 1..])),
        None => (candidate, None),
    };
    let (host, path) = if let Some(rest) = repo_part.strip_prefix("git@") {
        rest.split_once(':')?
    } else {
        let after_scheme = match repo_part.find("://") {
            Some(separator) => &repo_part[separator + 3..],
            None => repo_part,
        };
        let (host, path) = after_scheme.split_once('/')?;
        // `URL.hostname` drops a `git@` userinfo and the port.
        let host = host.trim_start_matches("git@");
        let host = host.split(':').next().unwrap_or(host);
        (host, path)
    };
    if !hosted_domain(host) {
        return None;
    }
    let path = path.trim_start_matches('/');
    let mut segments = path.split('/');
    let user = segments.next()?.trim_end_matches(".git").to_string();
    let project = segments.next()?.trim_end_matches(".git").to_string();
    if user.is_empty() || project.is_empty() {
        return None;
    }
    let ref_ = committish
        .filter(|committish| !committish.is_empty())
        .map(str::to_string);
    // The bare/shortcut forms default to `https://` (the
    // `useHttpsPrefix` branch); schemed and scp-like repos stay verbatim.
    let repo = if repo_part.starts_with("http://")
        || repo_part.starts_with("https://")
        || repo_part.starts_with("ssh://")
        || repo_part.starts_with("git://")
        || repo_part.starts_with("git@")
    {
        repo_part.to_string()
    } else {
        format!("https://{repo_part}")
    };
    build_git_source(repo, host, &format!("{user}/{project}"), ref_)
}

/// `parseGenericGitUrl` (`utils/git.ts:126-165`).
fn parse_generic_git_url(url: &str) -> Option<GitSource> {
    let (repo_without_ref, ref_) = split_ref(url);
    let mut repo = repo_without_ref.clone();
    let host;
    let path;
    if let Some(rest) = repo_without_ref.strip_prefix("git@") {
        let (host_part, path_part) = rest.split_once(':')?;
        host = host_part.to_string();
        path = path_part.to_string();
    } else if has_remote_scheme(&repo_without_ref) {
        let (_, rest_start) = split_scheme(&repo_without_ref)?;
        let rest = &repo_without_ref[rest_start..];
        let host_end = rest.find('/').unwrap_or(rest.len());
        // `URL.hostname` drops a `git@` userinfo and the port.
        let host_part = rest[..host_end].trim_start_matches("git@");
        host = host_part.split(':').next().unwrap_or(host_part).to_string();
        path = rest[host_end..].to_string();
    } else {
        let slash = repo_without_ref.find('/')?;
        let host_part = &repo_without_ref[..slash];
        if !host_part.contains('.') && host_part != "localhost" {
            return None;
        }
        host = host_part.to_string();
        path = repo_without_ref[slash..].to_string();
        repo = format!("https://{repo_without_ref}");
    }

    build_git_source(repo, &host, &path, ref_)
}

/// One installed-source resolution failure.
pub type PackageManagerResult<T> = Result<T, String>;

/// The manager over one cwd/agent-dir pair (`DefaultPackageManager`,
/// `package-manager.ts:158-166` + the git/local arms). Settings go through
/// [`SettingsManager`]'s `packages` key.
pub struct PackageManager<'a> {
    cwd: PathBuf,
    agent_dir: PathBuf,
    settings: &'a mut SettingsManager,
}

impl<'a> PackageManager<'a> {
    /// `DefaultPackageManager`'s constructor options (`:159-161`).
    pub fn new(cwd: &Path, agent_dir: &Path, settings: &'a mut SettingsManager) -> Self {
        PackageManager {
            cwd: cwd.to_path_buf(),
            agent_dir: agent_dir.to_path_buf(),
            settings,
        }
    }

    /// `parseSource` (`:1650-1680`).
    #[must_use]
    pub fn parse_source(&self, source: &str) -> ParsedSource {
        if let Some(spec) = source.strip_prefix("npm:") {
            let spec = spec.trim().to_string();
            let (name, version) = parse_npm_spec(&spec);
            return ParsedSource::Npm {
                spec,
                name,
                version,
            };
        }
        if is_local_path(source) {
            return ParsedSource::Local(source.to_string());
        }
        if let Some(git) = parse_git_url(source) {
            return ParsedSource::Git(git);
        }
        ParsedSource::Local(source.to_string())
    }

    /// `getSourceMatchKeyForInput` (`:1580-1592`).
    fn source_match_key_for_input(&self, source: &str) -> String {
        match self.parse_source(source) {
            ParsedSource::Npm { name, .. } => format!("npm:{name}"),
            ParsedSource::Git(git) => format!("git:{}/{}", git.host, git.path),
            ParsedSource::Local(path) => {
                format!("local:{}", path_comparison_key(&self.resolve_path(&path)))
            }
        }
    }

    /// `getSourceMatchKeyForSettings` (`:1594-1605`): local paths resolve
    /// against the scope's base dir before the comparison key; the base
    /// dir's trust gate propagates (upstream throws from the same depth).
    fn source_match_key_for_settings(
        &self,
        source: &str,
        scope: Scope,
    ) -> PackageManagerResult<String> {
        match self.parse_source(source) {
            ParsedSource::Npm { name, .. } => Ok(format!("npm:{name}")),
            ParsedSource::Git(git) => Ok(format!("git:{}/{}", git.host, git.path)),
            ParsedSource::Local(path) => {
                let base = self.base_dir_for_scope(scope)?;
                Ok(format!(
                    "local:{}",
                    path_comparison_key(&resolve_path_from_base(&path, &base))
                ))
            }
        }
    }

    /// `packageSourcesMatch` (`:1643-1648`).
    fn package_sources_match(
        &self,
        existing: &Value,
        input: &str,
        scope: Scope,
    ) -> PackageManagerResult<bool> {
        let existing_source = package_source_string(existing);
        Ok(self.source_match_key_for_settings(&existing_source, scope)?
            == self.source_match_key_for_input(input))
    }

    /// `normalizePackageSourceForSettings` (`:1616-1629`): local paths
    /// store cwd/base-relative, everything else verbatim.
    fn normalize_package_source_for_settings(
        &self,
        source: &str,
        scope: Scope,
    ) -> PackageManagerResult<String> {
        let ParsedSource::Local(path) = self.parse_source(source) else {
            return Ok(source.to_string());
        };
        let base = self.base_dir_for_scope(scope)?;
        let resolved = self.resolve_path(&path);
        let rel = relative_path(&base, &resolved);
        Ok(if rel.is_empty() { ".".to_string() } else { rel })
    }

    /// `getBaseDirForScope` (`:2395-2401`).
    fn base_dir_for_scope(&self, scope: Scope) -> PackageManagerResult<PathBuf> {
        match scope {
            Scope::Project => {
                self.assert_project_trusted_for_scope(scope)?;
                Ok(self.cwd.join(PROJECT_CONFIG_DIR_NAME))
            }
            Scope::User => Ok(self.agent_dir.clone()),
        }
    }

    /// `resolvePath` (`:2403-2414`): tilde expansion, then lexical
    /// resolution against the base.
    fn resolve_path(&self, input: &str) -> PathBuf {
        resolve_path_from_base(input, &self.cwd)
    }

    /// `assertProjectTrustedForScope` (`:1948-1953`).
    fn assert_project_trusted_for_scope(&self, scope: Scope) -> PackageManagerResult<()> {
        if scope == Scope::Project && !self.settings.is_project_trusted() {
            return Err(
                "Project is not trusted; refusing to access project package storage".to_string(),
            );
        }
        Ok(())
    }

    /// `getGitInstallRoot` (`:2366-2375`): project scope needs trust and
    /// resolves under the project config dir; user scope under the agent
    /// dir.
    fn git_install_root(&self, scope: Scope) -> PackageManagerResult<PathBuf> {
        match scope {
            Scope::Project => {
                self.assert_project_trusted_for_scope(scope)?;
                Ok(self.cwd.join(PROJECT_CONFIG_DIR_NAME).join("git"))
            }
            Scope::User => Ok(self.agent_dir.join("git")),
        }
    }

    /// `getGitInstallPath` (`:2354-2364`).
    fn git_install_path(&self, source: &GitSource, scope: Scope) -> PackageManagerResult<PathBuf> {
        let root = self.git_install_root(scope)?;
        resolve_managed_path(&root, &[&source.host, &source.path])
    }

    /// `install` (`:1106-1128`): parse, gate the scope, then the
    /// per-source arm. The npm arm reports the honest deferral.
    pub fn install(&mut self, source: &str, local: bool) -> PackageManagerResult<()> {
        let parsed = self.parse_source(source);
        let scope = if local { Scope::Project } else { Scope::User };
        self.assert_project_trusted_for_scope(scope)?;
        match parsed {
            ParsedSource::Npm { .. } => Err(format!(
                "Failed to install {source}: npm sources are not implemented in pi-rs yet"
            )),
            ParsedSource::Git(git) => self.install_git(&git, scope),
            ParsedSource::Local(path) => {
                let resolved = self.resolve_path(&path);
                if !resolved.exists() {
                    return Err(format!("Path does not exist: {}", resolved.display()));
                }
                Ok(())
            }
        }
    }

    /// `installAndPersist` (`:1130-1133`).
    pub fn install_and_persist(&mut self, source: &str, local: bool) -> PackageManagerResult<()> {
        self.install(source, local)?;
        self.add_source_to_settings(source, local)?;
        Ok(())
    }

    /// `installGit` (`:2069-2126`): an existing checkout updates in place
    /// through [`Self::ensure_git_ref`]; a fresh one clones, checks out
    /// the ref, and installs dependencies — cleaning up on failure.
    fn install_git(&mut self, source: &GitSource, scope: Scope) -> PackageManagerResult<()> {
        let target_dir = self.git_install_path(source, scope)?;
        if target_dir.exists() {
            if let Some(ref_) = &source.ref_ {
                return self.ensure_git_ref(
                    &target_dir,
                    &["fetch", "origin", ref_.as_str()],
                    "FETCH_HEAD",
                );
            }
            let target = get_local_git_update_target(&target_dir);
            let fetch_args: Vec<&str> = target.fetch_args.iter().map(String::as_str).collect();
            return self.ensure_git_ref(&target_dir, &fetch_args, &target.ref_);
        }
        let git_root = self.git_install_root(scope).ok();
        if let Some(git_root) = &git_root {
            ensure_git_ignore(git_root);
        }
        if let Some(parent) = target_dir.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let _ = std::fs::remove_file(update_marker_path(&target_dir));

        let result = self.clone_git(source, &target_dir);
        if let Err(error) = result {
            let _ = std::fs::remove_dir_all(&target_dir);
            prune_empty_git_parents(&target_dir, git_root.as_deref());
            return Err(error);
        }
        Ok(())
    }

    fn clone_git(&mut self, source: &GitSource, target_dir: &Path) -> PackageManagerResult<()> {
        run_command(
            "git",
            &["clone", &source.repo, &target_dir.to_string_lossy()],
            None,
        )?;
        if let Some(ref_) = &source.ref_ {
            run_command("git", &["checkout", ref_], Some(target_dir))?;
        }
        if target_dir.join("package.json").exists() {
            let (command, mut args) = self.npm_command()?;
            args.extend(self.git_dependency_install_args()?);
            run_command(&command, &args, Some(target_dir))?;
        }
        Ok(())
    }

    /// `remove` (`:1135-1159`): parse, gate the scope, then the
    /// per-source arm. The npm arm reports the honest deferral.
    pub fn remove(&mut self, source: &str, local: bool) -> PackageManagerResult<()> {
        let parsed = self.parse_source(source);
        let scope = if local { Scope::Project } else { Scope::User };
        self.assert_project_trusted_for_scope(scope)?;
        match parsed {
            ParsedSource::Npm { .. } => Err(format!(
                "Failed to remove {source}: npm sources are not implemented in pi-rs yet"
            )),
            ParsedSource::Git(git) => self.remove_git(&git, scope),
            ParsedSource::Local(_) => Ok(()),
        }
    }

    /// `removeAndPersist` (`:1146-1150`): false when nothing matched.
    pub fn remove_and_persist(&mut self, source: &str, local: bool) -> PackageManagerResult<bool> {
        self.remove(source, local)?;
        self.remove_source_from_settings(source, local)
    }

    /// `removeGit` (`:2233-2238`): drop the checkout, the update marker, and
    /// the emptied parents.
    fn remove_git(&self, source: &GitSource, scope: Scope) -> PackageManagerResult<()> {
        let target_dir = self.git_install_path(source, scope)?;
        let _ = std::fs::remove_dir_all(&target_dir);
        let _ = std::fs::remove_file(update_marker_path(&target_dir));
        let install_root = self.git_install_root(scope).ok();
        prune_empty_git_parents(&target_dir, install_root.as_deref());
        Ok(())
    }

    /// `addSourceToSettings` (`:902-927`): false when the exact normalized
    /// source is already recorded; a match under another spelling is rewritten
    /// to the normalized form, keeping object entries object-shaped.
    pub fn add_source_to_settings(
        &mut self,
        source: &str,
        local: bool,
    ) -> PackageManagerResult<bool> {
        let scope = if local { Scope::Project } else { Scope::User };
        let normalized = self.normalize_package_source_for_settings(source, scope)?;
        let packages = self.packages_for_scope(scope);
        let mut match_index = None;
        for (index, existing) in packages.iter().enumerate() {
            if self.package_sources_match(existing, source, scope)? {
                match_index = Some(index);
                break;
            }
        }
        let Some(index) = match_index else {
            let mut next = packages;
            next.push(Value::String(normalized));
            self.set_packages_for_scope(scope, &next)?;
            return Ok(true);
        };
        if package_source_string(&packages[index]) == normalized {
            return Ok(false);
        }
        let mut next = packages;
        let existing = &next[index];
        next[index] = if existing.is_string() {
            Value::String(normalized)
        } else {
            let mut object = existing
                .as_object()
                .cloned()
                .expect("a non-string entry is an object");
            object.insert("source".to_string(), Value::String(normalized));
            Value::Object(object)
        };
        self.set_packages_for_scope(scope, &next)?;
        Ok(true)
    }

    /// `removeSourceFromSettings` (`:929-945`): false when nothing dropped.
    fn remove_source_from_settings(
        &mut self,
        source: &str,
        local: bool,
    ) -> PackageManagerResult<bool> {
        let scope = if local { Scope::Project } else { Scope::User };
        let packages = self.packages_for_scope(scope);
        let original = packages.len();
        let mut next = Vec::with_capacity(original);
        for existing in packages {
            if self.package_sources_match(&existing, source, scope)? {
                continue;
            }
            next.push(existing);
        }
        if next.len() == original {
            return Ok(false);
        }
        self.set_packages_for_scope(scope, &next)?;
        Ok(true)
    }

    /// The scope's `packages` entries — `getGlobalSettings`/`getProjectSettings`
    /// with `?? []` (`:906-907`, `:1081`/`:1094`).
    ///
    /// Read without the trust gate; the gate fires where a path is resolved.
    fn packages_for_scope(&self, scope: Scope) -> Vec<Value> {
        let settings = match scope {
            Scope::User => self.settings.get_global_settings(),
            Scope::Project => self.settings.get_project_settings(),
        };
        settings
            .get("packages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    /// `setPackages`/`setProjectPackages` (`:921-926`, `:938-943`).
    fn set_packages_for_scope(
        &mut self,
        scope: Scope,
        packages: &[Value],
    ) -> PackageManagerResult<()> {
        let value = Value::Array(packages.to_vec());
        match scope {
            Scope::User => self.settings.set("packages", value),
            Scope::Project => self.settings.set_project("packages", value)?,
        }
        Ok(())
    }

    /// `listConfiguredPackages` (`:1078-1103`): user entries first, then
    /// project; `filtered` marks object-form entries; `installed_path` is the
    /// managed path when it exists.
    pub fn list_configured_packages(&self) -> PackageManagerResult<Vec<ConfiguredPackage>> {
        let mut configured = Vec::new();
        for (scope, packages) in [
            (Scope::User, self.packages_for_scope(Scope::User)),
            (Scope::Project, self.packages_for_scope(Scope::Project)),
        ] {
            for package in packages {
                let source = package_source_string(&package);
                let filtered = !package.is_string();
                let installed_path = self.get_installed_path(&source, scope)?;
                configured.push(ConfiguredPackage {
                    source,
                    scope,
                    filtered,
                    installed_path,
                });
            }
        }
        Ok(configured)
    }

    /// `getInstalledPath` (`:951-970`). The npm arm drops the legacy global
    /// fallback (`getLegacyGlobalNpmInstallPath` shells out to a package
    /// manager): the npm arm is deferred, so no legacy install could exist —
    /// part of the documented npm deferral.
    fn get_installed_path(
        &self,
        source: &str,
        scope: Scope,
    ) -> PackageManagerResult<Option<PathBuf>> {
        match self.parse_source(source) {
            ParsedSource::Npm { name, .. } => {
                let root = self.base_dir_for_scope(scope)?.join("npm");
                let path = resolve_managed_path(&root, &["node_modules", &name])?;
                Ok(path.exists().then_some(path))
            }
            ParsedSource::Git(git) => {
                let path = self.git_install_path(&git, scope)?;
                Ok(path.exists().then_some(path))
            }
            ParsedSource::Local(path) => {
                let base = self.base_dir_for_scope(scope)?;
                let path = resolve_path_from_base(&path, &base);
                Ok(path.exists().then_some(path))
            }
        }
    }

    /// `buildNoMatchingPackageMessage` (`:1603-1609`) over the configured
    /// sources. The `pi remove` CLI prints the plain form directly (upstream
    /// `package-manager-cli.ts` too); the suggestion form serves `resolve()`'s
    /// missing-source callback, which lands with the npm arm.
    #[allow(dead_code)] // resolve() deferral; tests cover it
    fn build_no_matching_package_message(&self, source: &str) -> PackageManagerResult<String> {
        let configured: Vec<String> = self
            .list_configured_packages()?
            .into_iter()
            .map(|package| package.source)
            .collect();
        let Some(suggestion) = self.find_suggested_configured_source(source, &configured) else {
            return Ok(format!("No matching package found for {source}"));
        };
        Ok(format!(
            "No matching package found for {source}. Did you mean {suggestion}?"
        ))
    }

    /// `findSuggestedConfiguredSource` (`:1611-1634`): the first configured
    /// source whose npm name/spec or git shorthand the input names.
    #[allow(dead_code)] // resolve() deferral; tests cover it
    fn find_suggested_configured_source(
        &self,
        source: &str,
        configured: &[String],
    ) -> Option<String> {
        let trimmed = source.trim();
        let mut suggestions: Vec<String> = Vec::new();
        for source_str in configured {
            match self.parse_source(source_str) {
                ParsedSource::Npm { spec, name, .. } => {
                    if (trimmed == name || trimmed == spec) && !suggestions.contains(source_str) {
                        suggestions.push(source_str.clone());
                    }
                }
                ParsedSource::Git(git) => {
                    let shorthand = format!("{}/{}", git.host, git.path);
                    let shorthand_with_ref =
                        git.ref_.as_ref().map(|ref_| format!("{shorthand}@{ref_}"));
                    if (trimmed == shorthand || shorthand_with_ref.as_deref() == Some(trimmed))
                        && !suggestions.contains(source_str)
                    {
                        suggestions.push(source_str.clone());
                    }
                }
                ParsedSource::Local(_) => {}
            }
        }
        suggestions.into_iter().next()
    }

    /// `getNpmCommand` (`:1954-1964`): the settings `npmCommand` array or npm.
    fn npm_command(&self) -> PackageManagerResult<(String, Vec<String>)> {
        let configured: Vec<String> = self
            .settings
            .get("npmCommand")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let Some((command, args)) = configured.split_first() else {
            return Ok(("npm".to_string(), Vec::new()));
        };
        if command.is_empty() {
            return Err(
                "Invalid npmCommand: first array entry must be a non-empty command".to_string(),
            );
        }
        Ok((command.clone(), args.to_vec()))
    }

    /// `getPackageManagerName` (`:1966-1992`): the direct command, the command
    /// wrapped behind `--`, or the single supported manager among the args.
    fn package_manager_name(&self) -> PackageManagerResult<String> {
        let (command, args) = self.npm_command()?;
        let direct = normalize_command_name(&command);
        let supported = ["npm", "pnpm", "bun"];
        if let Some(separator) = args.iter().rposition(|arg| arg == "--") {
            let wrapped = args
                .get(separator + 1)
                .map(String::as_str)
                .filter(|wrapped| !wrapped.is_empty());
            return Ok(wrapped.map_or_else(|| direct.clone(), normalize_command_name));
        }
        if supported.contains(&direct.as_str()) {
            return Ok(direct);
        }
        let mut wrapped: Vec<String> = Vec::new();
        for arg in &args {
            let name = normalize_command_name(arg);
            if supported.contains(&name.as_str()) && !wrapped.contains(&name) {
                wrapped.push(name);
            }
        }
        if wrapped.len() > 1 {
            return Err(format!(
                "Ambiguous npmCommand package managers: {}",
                wrapped.join(", ")
            ));
        }
        Ok(wrapped.into_iter().next().unwrap_or(direct))
    }

    /// `getGitDependencyInstallArgs` (`:1994-2009`).
    fn git_dependency_install_args(&self) -> PackageManagerResult<Vec<String>> {
        Ok(match self.package_manager_name()?.as_str() {
            "bun" => vec!["install", "--omit=dev", "--omit=peer"],
            "pnpm" => vec![
                "install",
                "--prod",
                "--config.auto-install-peers=false",
                "--config.strict-peer-dependencies=false",
                "--config.strict-dep-builds=false",
            ],
            "npm" => vec![
                "install",
                "--omit=dev",
                "--legacy-peer-deps",
                "--no-audit",
                "--no-fund",
            ],
            _ => vec!["install"],
        }
        .into_iter()
        .map(str::to_string)
        .collect())
    }

    /// `ensureGitRef` (`:2169-2203`): fetch only the ref we will reset to;
    /// short-circuit when the checkout is already there (repairing missing
    /// dependencies, or finishing a half-applied update through the marker);
    /// else mark, reset, and reinstall dependencies.
    fn ensure_git_ref(
        &self,
        target_dir: &Path,
        fetch_args: &[&str],
        ref_: &str,
    ) -> PackageManagerResult<()> {
        run_command("git", fetch_args, Some(target_dir))?;
        let local_head = run_command_capture("git", &["rev-parse", "HEAD"], Some(target_dir))?;
        let commit_ref = format!("{ref_}^{{commit}}");
        let target_head =
            run_command_capture("git", &["rev-parse", &commit_ref], Some(target_dir))?;
        let marker_path = update_marker_path(target_dir);
        if local_head.trim() == target_head.trim() {
            if marker_path.exists() {
                return self.clean_and_install_git_dependencies(target_dir, &marker_path);
            }
            return self.repair_missing_git_dependencies(target_dir);
        }
        std::fs::write(&marker_path, "").map_err(|error| error.to_string())?;
        run_command("git", &["reset", "--hard", &commit_ref], Some(target_dir))?;
        self.clean_and_install_git_dependencies(target_dir, &marker_path)
    }

    /// `cleanAndInstallGitDependencies` (`:2196-2219`): clean untracked files
    /// (extensions should be pristine), install dependencies for the new
    /// commit, and clear the marker only when the clean succeeded — a failed
    /// clean keeps the marker so the next update retries.
    fn clean_and_install_git_dependencies(
        &self,
        target_dir: &Path,
        marker_path: &Path,
    ) -> PackageManagerResult<()> {
        let cleaned = run_command("git", &["clean", "-fdx"], Some(target_dir)).is_ok();
        if target_dir.join("package.json").exists() {
            let (command, mut args) = self.npm_command()?;
            args.extend(self.git_dependency_install_args()?);
            run_command(&command, &args, Some(target_dir))?;
        }
        if cleaned {
            let _ = std::fs::remove_file(marker_path);
        }
        Ok(())
    }

    /// `repairMissingGitDependencies` (`:2188-2192`).
    fn repair_missing_git_dependencies(&self, target_dir: &Path) -> PackageManagerResult<()> {
        if !has_missing_git_dependencies(target_dir) {
            return Ok(());
        }
        let (command, mut args) = self.npm_command()?;
        args.extend(self.git_dependency_install_args()?);
        run_command(&command, &args, Some(target_dir))
    }
}

/// `resolvePathFromBase` (`:2416-2418`): tilde expansion, then lexical
/// resolution against the base.
fn resolve_path_from_base(input: &str, base: &Path) -> PathBuf {
    let trimmed = input.trim();
    let expanded = if trimmed == "~" {
        home_dir()
    } else if let Some(rest) = trimmed.strip_prefix("~/").or(trimmed.strip_prefix("~\\")) {
        home_dir().join(rest)
    } else {
        PathBuf::from(trimmed)
    };
    if expanded.is_absolute() {
        normalize_lexical(&expanded)
    } else {
        normalize_lexical(&base.join(expanded))
    }
}

/// `resolveManagedPath` (`:2386-2393`): refuse paths escaping the
/// install root.
fn resolve_managed_path(root: &Path, parts: &[&str]) -> PackageManagerResult<PathBuf> {
    let resolved_root = normalize_lexical(root);
    let mut resolved = resolved_root.clone();
    for part in parts {
        resolved.push(part);
    }
    let resolved = normalize_lexical(&resolved);
    if resolved != resolved_root && !resolved.starts_with(&resolved_root) {
        return Err(format!(
            "Refusing to use path outside package install root: {}",
            resolved.display()
        ));
    }
    Ok(resolved)
}

/// `getGitUpdateMarkerPath` (`:2192-2194`).
fn update_marker_path(target_dir: &Path) -> PathBuf {
    let marker = target_dir
        .file_name()
        .map(|name| format!(".{}.pi-update-incomplete", name.to_string_lossy()))
        .unwrap_or_default();
    match target_dir.parent() {
        Some(parent) => parent.join(marker),
        None => PathBuf::from(marker),
    }
}

/// Upstream `NETWORK_TIMEOUT_MS` (`:59`).
const NETWORK_TIMEOUT_MS: u64 = 10_000;

/// The shape `getLocalGitUpdateTarget` returns, minus the `head` field —
/// the update dance never reads it (upstream `:1774-1830` returns
/// `{ ref, head, fetchArgs }`; the port drops the dead field).
struct LocalGitUpdateTarget {
    ref_: String,
    fetch_args: Vec<String>,
}

/// The per-branch fetch args `getLocalGitUpdateTarget` builds twice.
fn local_fetch_args(branch: &str) -> Vec<String> {
    vec![
        "fetch".to_string(),
        "--prune".to_string(),
        "--no-tags".to_string(),
        "origin".to_string(),
        format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"),
    ]
}

/// `getLocalGitUpdateTarget` (`:1774-1830`): the upstream-tracking branch
/// when there is one, else `origin/HEAD` after refreshing it. The port's
/// failure paths all collapse into the `origin/HEAD` fallback, so nothing
/// can fail here (upstream throws inside the try and catches it).
fn get_local_git_update_target(installed_path: &Path) -> LocalGitUpdateTarget {
    let upstream_target = match run_command_capture(
        "git",
        &["rev-parse", "--abbrev-ref", "@{upstream}"],
        Some(installed_path),
    ) {
        Ok(upstream) => match upstream.trim().strip_prefix("origin/") {
            Some(branch) if !branch.is_empty() => Some(LocalGitUpdateTarget {
                ref_: "@{upstream}".to_string(),
                fetch_args: local_fetch_args(branch),
            }),
            _ => None,
        },
        Err(_) => None,
    };
    if let Some(target) = upstream_target {
        return target;
    }
    // The catch path: reset origin/HEAD (errors ignored upstream too), then
    // derive the branch from the symbolic ref, falling back to +HEAD.
    let _ = run_command(
        "git",
        &["remote", "set-head", "origin", "-a"],
        Some(installed_path),
    );
    let origin_head_ref = run_command_capture(
        "git",
        &["symbolic-ref", "refs/remotes/origin/HEAD"],
        Some(installed_path),
    )
    .unwrap_or_default();
    let branch = origin_head_ref
        .trim()
        .strip_prefix("refs/remotes/origin/")
        .unwrap_or_default();
    let fetch_args = if branch.is_empty() {
        vec![
            "fetch".to_string(),
            "--prune".to_string(),
            "--no-tags".to_string(),
            "origin".to_string(),
            "+HEAD:refs/remotes/origin/HEAD".to_string(),
        ]
    } else {
        local_fetch_args(branch)
    };
    LocalGitUpdateTarget {
        ref_: "origin/HEAD".to_string(),
        fetch_args,
    }
}

/// `hasMissingGitDependencies` (`:2122-2144`): a manifest dependency whose
/// `node_modules` entry is absent.
fn has_missing_git_dependencies(target_dir: &Path) -> bool {
    let manifest_path = target_dir.join("package.json");
    if !manifest_path.exists() {
        return false;
    }
    let Ok(content) = std::fs::read_to_string(&manifest_path) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_str::<Value>(strip_bom(&content)) else {
        return false;
    };
    let Some(dependencies) = manifest.get("dependencies").and_then(Value::as_object) else {
        return false;
    };
    let node_modules = target_dir.join("node_modules");
    dependencies.keys().any(|name| {
        let dependency_path = node_modules.join(name);
        if !dependency_path.starts_with(&node_modules) {
            return false;
        }
        !dependency_path.exists()
    })
}

/// `pruneEmptyGitParents` (`:2240-2260`): drop emptied ancestor directories
/// up to (not including) the install root.
fn prune_empty_git_parents(target_dir: &Path, install_root: Option<&Path>) {
    let Some(install_root) = install_root else {
        return;
    };
    let resolved_root = normalize_lexical(install_root);
    let Some(mut current) = target_dir.parent().map(Path::to_path_buf) else {
        return;
    };
    while current.starts_with(&resolved_root) && current != resolved_root {
        if !current.exists() {
            match current.parent() {
                Some(parent) => current = parent.to_path_buf(),
                None => break,
            }
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&current) else {
            break;
        };
        if entries.count() > 0 {
            break;
        }
        if std::fs::remove_dir_all(&current).is_err() {
            break;
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
}

/// `ensureGitIgnore` (`:2277-2286`): `*` + `!.gitignore` in the root.
fn ensure_git_ignore(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let ignore_path = dir.join(".gitignore");
    if !ignore_path.exists() {
        let _ = std::fs::write(&ignore_path, "*\n!.gitignore\n");
    }
}

/// Upstream `stripBom` (`utils/json.ts`): drop a leading BOM before parse.
fn strip_bom(value: &str) -> &str {
    value.strip_prefix('\u{feff}').unwrap_or(value)
}

/// `getPackageSourceString` (`:1576-1578`): a string entry verbatim, an
/// object entry's `source` field.
fn package_source_string(package: &Value) -> String {
    match package {
        Value::String(source) => source.clone(),
        Value::Object(object) => object
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// Node `path.resolve`: absolute, `.`/`..` collapsed lexically, symlinks
/// untouched.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

/// `getPathComparisonKey` (`utils/paths.ts:46`): the resolved path as a
/// string for comparison only. Upstream case-folds on Windows; the port
/// targets POSIX, so no folding (comparison use only, never display).
fn path_comparison_key(path: &Path) -> String {
    normalize_lexical(path).to_string_lossy().to_string()
}

/// Node `path.relative(base, resolved)` as a string; empty when equal.
fn relative_path(base: &Path, resolved: &Path) -> String {
    let base = normalize_lexical(base);
    let resolved = normalize_lexical(resolved);
    if base == resolved {
        return String::new();
    }
    let base_components: Vec<_> = base.components().collect();
    let resolved_components: Vec<_> = resolved.components().collect();
    let mut common = 0;
    while common < base_components.len()
        && common < resolved_components.len()
        && base_components[common].as_os_str() == resolved_components[common].as_os_str()
    {
        common += 1;
    }
    let mut result = PathBuf::new();
    for _ in common..base_components.len() {
        result.push("..");
    }
    for component in &resolved_components[common..] {
        result.push(component.as_os_str());
    }
    result.to_string_lossy().to_string()
}

/// `getHomeDir` (`:2424`): the `HOME` env var.
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// `getPackageManagerName`'s `normalizeCommandName` (`:1967`): the
/// basename with a trailing `.cmd`/`.exe` stripped, case-insensitively.
fn normalize_command_name(command: &str) -> String {
    let base = std::path::Path::new(command).file_name().map_or_else(
        || command.to_string(),
        |name| name.to_string_lossy().to_string(),
    );
    let lower = base.to_lowercase();
    // `/\.(cmd|exe)$/i`: `lower` already folds case, so a suffix strip is
    // the case-insensitive test.
    if lower.strip_suffix(".cmd").is_some() || lower.strip_suffix(".exe").is_some() {
        base[..base.len() - 4].to_string()
    } else {
        base
    }
}

/// `runCommand` (`:2939-2949`): fire, wait, verbatim failure text; no
/// timeout upstream.
fn run_command(
    command: &str,
    args: &[impl AsRef<str>],
    cwd: Option<&Path>,
) -> PackageManagerResult<()> {
    let joined = args
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<&str>>()
        .join(" ");
    let mut builder = Command::new(command);
    builder.args(args.iter().map(AsRef::as_ref));
    if let Some(cwd) = cwd {
        builder.current_dir(cwd);
    }
    let mut child = builder.spawn().map_err(|error| error.to_string())?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                let code = status
                    .code()
                    .map_or_else(|| "null".to_string(), |code| code.to_string());
                return Err(format!("{command} {joined} failed with code {code}"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// Drain one child pipe on a thread so the child never blocks on a full
/// buffer while the poll loop waits.
fn drain_pipe<R: std::io::Read + Send + 'static>(mut pipe: R) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buffer = String::new();
        let _ = std::io::Read::read_to_string(&mut pipe, &mut buffer);
        buffer
    })
}

/// `runCommandCapture` (`:2895-2937`): trimmed stdout on success, verbatim
/// failure and timeout texts, killed at the `NETWORK_TIMEOUT_MS` deadline.
#[allow(clippy::too_many_lines)] // mirrors the TS spawn/drain dance; split when extending
fn run_command_capture(
    command: &str,
    args: &[impl AsRef<str>],
    cwd: Option<&Path>,
) -> PackageManagerResult<String> {
    let joined = args
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<&str>>()
        .join(" ");
    let mut builder = Command::new(command);
    builder.args(args.iter().map(AsRef::as_ref));
    if let Some(cwd) = cwd {
        builder.current_dir(cwd);
    }
    builder.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = builder.spawn().map_err(|error| error.to_string())?;
    let stdout_pipe = child.stdout.take().expect("stdout is piped");
    let stderr_pipe = child.stderr.take().expect("stderr is piped");
    let stdout_thread = drain_pipe(stdout_pipe);
    let stderr_thread = drain_pipe(stderr_pipe);
    let deadline = Duration::from_millis(NETWORK_TIMEOUT_MS);
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if started.elapsed() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(error.to_string());
            }
        }
    };
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    let Some(status) = status else {
        return Err(format!("{command} {joined} did not report an exit status"));
    };
    if status.success() {
        return Ok(stdout.trim().to_string());
    }
    if timed_out {
        return Err(format!(
            "{command} {joined} timed out after {NETWORK_TIMEOUT_MS}ms"
        ));
    }
    let exit_status = status.code().map_or_else(
        || "signal unknown".to_string(),
        |code| format!("code {code}"),
    );
    Err(format!(
        "{command} {joined} failed with {exit_status}: {}",
        if stderr.is_empty() { &stdout } else { &stderr }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::{Settings, SettingsManagerCreateOptions};
    use serde_json::json;

    fn git_source(repo: &str, host: &str, path: &str, ref_: Option<&str>) -> GitSource {
        GitSource {
            repo: repo.to_string(),
            host: host.to_string(),
            path: path.to_string(),
            ref_: ref_.map(str::to_string),
            pinned: ref_.is_some(),
        }
    }

    /// The five help examples plus the ref/`.git`/guard shapes
    /// (`parseGitUrl` over `splitRef` + the hosted candidates).
    #[test]
    fn parse_git_url_covers_the_documented_examples() {
        assert_eq!(
            parse_git_url("git:github.com/user/repo"),
            Some(git_source(
                "https://github.com/user/repo",
                "github.com",
                "user/repo",
                None
            ))
        );
        assert_eq!(
            parse_git_url("git:git@github.com:user/repo"),
            Some(git_source(
                "git@github.com:user/repo",
                "github.com",
                "user/repo",
                None
            ))
        );
        assert_eq!(
            parse_git_url("https://github.com/user/repo"),
            Some(git_source(
                "https://github.com/user/repo",
                "github.com",
                "user/repo",
                None
            ))
        );
        assert_eq!(
            parse_git_url("ssh://git@github.com/user/repo"),
            Some(git_source(
                "ssh://git@github.com/user/repo",
                "github.com",
                "user/repo",
                None
            ))
        );
        assert_eq!(
            parse_git_url("git:github.com/user/repo#v1.2"),
            Some(git_source(
                "https://github.com/user/repo",
                "github.com",
                "user/repo",
                Some("v1.2")
            ))
        );
        // The `.git` suffix trims from the PATH; the repo stays verbatim
        // (`buildGitSource`'s `repo: args.repo`).
        assert_eq!(
            parse_git_url("git:github.com/user/repo.git"),
            Some(git_source(
                "https://github.com/user/repo.git",
                "github.com",
                "user/repo",
                None
            ))
        );
        // A pinned ref splits inside the scp-like path (`@v2` after the repo).
        assert_eq!(
            parse_git_url("git:git@github.com:user/repo@v2"),
            Some(git_source(
                "git@github.com:user/repo",
                "github.com",
                "user/repo",
                Some("v2")
            ))
        );
    }

    #[test]
    fn parse_git_url_rejects_the_guard_and_unsafe_shapes() {
        // No remote scheme and no `git:` prefix: not a git source.
        assert_eq!(parse_git_url("git@github.com:user/repo"), None);
        assert_eq!(parse_git_url("github.com/user/repo"), None);
        // Path traversal and separator injection are unsafe parts.
        assert_eq!(parse_git_url("git:github.com/../../etc/passwd"), None);
        assert_eq!(parse_git_url("git:github.com/user/repo\\evil"), None);
        // A single path segment is not a repo.
        assert_eq!(parse_git_url("git:github.com/only"), None);
    }

    #[test]
    fn parse_npm_spec_splits_at_the_first_version_separator() {
        assert_eq!(parse_npm_spec("foo"), ("foo".to_string(), None));
        assert_eq!(
            parse_npm_spec("foo@1.2"),
            ("foo".to_string(), Some("1.2".to_string()))
        );
        assert_eq!(
            parse_npm_spec("@scope/pkg@1.2.3"),
            ("@scope/pkg".to_string(), Some("1.2.3".to_string()))
        );
        assert_eq!(
            parse_npm_spec("@scope/pkg"),
            ("@scope/pkg".to_string(), None)
        );
        // A trailing `@` fails the regex's `.+` — the spec stays unchanged.
        assert_eq!(parse_npm_spec("foo@"), ("foo@".to_string(), None));
    }

    #[test]
    fn install_local_source_requires_an_existing_path() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let mut settings_manager = SettingsManager::in_memory(
            &Settings::new(),
            SettingsManagerCreateOptions {
                project_trusted: Some(false),
            },
        );
        let mut manager = PackageManager::new(cwd.path(), agent_dir.path(), &mut settings_manager);
        let error = manager.install("./no/such/dir", false).expect_err("error");
        assert_eq!(
            error,
            format!(
                "Path does not exist: {}",
                cwd.path().join("no/such/dir").display()
            )
        );
    }

    #[test]
    fn install_and_persist_records_the_normalized_source() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir(dir.path().join("ext")).expect("dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let mut settings_manager = SettingsManager::in_memory(
            &Settings::new(),
            SettingsManagerCreateOptions {
                project_trusted: Some(false),
            },
        );
        let mut manager = PackageManager::new(cwd.path(), agent_dir.path(), &mut settings_manager);
        let source = dir.path().join("ext").to_string_lossy().to_string();
        manager
            .install_and_persist(&source, false)
            .expect("install");
        let configured = manager.list_configured_packages().expect("list");
        assert_eq!(configured.len(), 1);
        assert_eq!(configured[0].scope, Scope::User);
        assert!(!configured[0].filtered);
        assert_eq!(
            configured[0].installed_path.as_ref().expect("path"),
            &dir.path().join("ext")
        );
        // A second install of the same source reports no change.
        assert!(!manager.add_source_to_settings(&source, false).expect("add"));
    }

    #[test]
    fn remove_drops_the_source_and_reports_no_match() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let mut settings_manager = SettingsManager::in_memory(
            &Settings::new(),
            SettingsManagerCreateOptions {
                project_trusted: Some(false),
            },
        );
        let mut manager = PackageManager::new(cwd.path(), agent_dir.path(), &mut settings_manager);
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir(dir.path().join("ext")).expect("dir");
        let source = dir.path().join("ext").to_string_lossy().to_string();
        manager
            .install_and_persist(&source, false)
            .expect("install");
        assert!(manager.remove_and_persist(&source, false).expect("remove"));
        assert_eq!(manager.list_configured_packages().expect("list").len(), 0);
        // A second remove finds nothing.
        assert!(!manager.remove_and_persist(&source, false).expect("remove"));
    }

    #[test]
    fn untrusted_project_scope_refuses_package_storage() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let mut settings_manager = SettingsManager::in_memory(
            &Settings::new(),
            SettingsManagerCreateOptions {
                project_trusted: Some(false),
            },
        );
        let mut manager = PackageManager::new(cwd.path(), agent_dir.path(), &mut settings_manager);
        let error = manager
            .install_and_persist("./ext", true)
            .expect_err("refused");
        assert_eq!(
            error,
            "Project is not trusted; refusing to access project package storage"
        );
    }

    #[test]
    fn project_scope_stores_relative_to_the_project_config_dir() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent");
        std::fs::create_dir(cwd.path().join("ext")).expect("dir");
        let mut settings_manager = SettingsManager::in_memory(
            &Settings::new(),
            SettingsManagerCreateOptions {
                project_trusted: Some(true),
            },
        );
        let mut manager = PackageManager::new(cwd.path(), agent_dir.path(), &mut settings_manager);
        manager.install_and_persist("./ext", true).expect("install");
        let configured = manager.list_configured_packages().expect("list");
        assert_eq!(configured.len(), 1);
        // `relative(baseDir, resolved)` from the project config dir.
        assert_eq!(configured[0].source, "../ext");
        assert_eq!(configured[0].scope, Scope::Project);
        assert_eq!(
            configured[0].installed_path.as_ref().expect("path"),
            &cwd.path().join("ext")
        );
    }

    #[test]
    fn no_matching_message_suggests_a_configured_source() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let settings = json!({
            "packages": ["git:github.com/user/repo"],
        });
        let settings = settings.as_object().cloned().expect("object");
        let mut settings_manager = SettingsManager::in_memory(
            &settings,
            SettingsManagerCreateOptions {
                project_trusted: Some(false),
            },
        );
        let manager = PackageManager::new(cwd.path(), agent_dir.path(), &mut settings_manager);
        assert_eq!(
            manager
                .build_no_matching_package_message("github.com/user/repo")
                .expect("message"),
            "No matching package found for github.com/user/repo. Did you mean git:github.com/user/repo?"
        );
        assert_eq!(
            manager
                .build_no_matching_package_message("npm:other")
                .expect("message"),
            "No matching package found for npm:other"
        );
    }

    /// A live git install: clone the local source repo, then re-install
    /// after a new commit — the update-in-place dance fetches and resets
    /// (`installGit`'s exists-branch through `getLocalGitUpdateTarget` +
    /// `ensureGitRef`). Skipped where `git` is unavailable.
    #[test]
    fn install_git_clones_and_updates_in_place() {
        if std::process::Command::new("git")
            .arg("--version")
            .status()
            .is_err()
        {
            return;
        }
        let run = |repo: &Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .env("GIT_AUTHOR_NAME", "pi-rs-test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "pi-rs-test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?} failed");
        };
        let repo = tempfile::tempdir().expect("repo");
        run(repo.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("README.md"), "one").expect("write");
        run(repo.path(), &["add", "."]);
        run(repo.path(), &["commit", "-q", "-m", "one"]);

        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let mut settings_manager = SettingsManager::in_memory(
            &Settings::new(),
            SettingsManagerCreateOptions {
                project_trusted: Some(false),
            },
        );
        let mut manager = PackageManager::new(cwd.path(), agent_dir.path(), &mut settings_manager);
        let source = GitSource {
            repo: repo.path().to_string_lossy().to_string(),
            host: "local".to_string(),
            path: "test/repo".to_string(),
            ref_: None,
            pinned: false,
        };
        manager.install_git(&source, Scope::User).expect("install");
        let installed = agent_dir.path().join("git/local/test/repo");
        assert_eq!(
            std::fs::read_to_string(installed.join("README.md")).expect("read"),
            "one"
        );

        // A new commit upstream: the re-install fetches and resets.
        std::fs::write(repo.path().join("README.md"), "two").expect("write");
        run(repo.path(), &["add", "."]);
        run(repo.path(), &["commit", "-q", "-m", "two"]);
        run(
            repo.path(),
            &[
                "remote",
                "add",
                "origin",
                repo.path().to_string_lossy().as_ref(),
            ],
        );
        run(
            repo.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        manager.install_git(&source, Scope::User).expect("update");
        assert_eq!(
            std::fs::read_to_string(installed.join("README.md")).expect("read"),
            "two"
        );

        // Removal deletes the checkout and prunes the emptied parents.
        let target_dir = manager
            .git_install_path(&source, Scope::User)
            .expect("path");
        let _ = std::fs::remove_dir_all(&target_dir);
        let _ = std::fs::remove_file(update_marker_path(&target_dir));
        prune_empty_git_parents(&target_dir, Some(&agent_dir.path().join("git")));
        assert!(!installed.exists());
        assert!(!installed.parent().expect("parent").exists());
    }

    #[test]
    fn npm_sources_report_the_honest_deferral() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let mut settings_manager = SettingsManager::in_memory(
            &Settings::new(),
            SettingsManagerCreateOptions {
                project_trusted: Some(false),
            },
        );
        let mut manager = PackageManager::new(cwd.path(), agent_dir.path(), &mut settings_manager);
        assert_eq!(
            manager
                .install("npm:@foo/bar", false)
                .expect_err("deferred"),
            "Failed to install npm:@foo/bar: npm sources are not implemented in pi-rs yet"
        );
        assert_eq!(
            manager.remove("npm:@foo/bar", false).expect_err("deferred"),
            "Failed to remove npm:@foo/bar: npm sources are not implemented in pi-rs yet"
        );
    }
}
