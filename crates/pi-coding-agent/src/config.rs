//! User config paths and the app identity (`config.ts`).
//!
//! Ground truth: `packages/coding-agent/src/config.ts:603-681` — the app
//! name/config-dir constants, the agent-dir environment overrides, and the
//! derived per-artifact paths under `~/.pi/agent/`. Everything
//! package-manager-specific in that file (Bun/npm install detection and
//! self-update command building, `:1-390`, `:391-576`) has no Rust
//! counterpart and is intentionally not ported.
//!
//! The global + project *settings* store lives in [`settings`]
//! (`core/settings-manager.ts`), and the credential store in [`crate::auth`]
//! (`core/auth-storage.ts`); both hang off [`get_agent_dir`] exactly as in TS.

pub mod settings;

use std::path::{Path, PathBuf};

/// `PACKAGE_NAME` (`config.ts:603`).
pub const PACKAGE_NAME: &str = "@earendil-works/pi-coding-agent";
/// `APP_NAME` (`config.ts:604`): the config-dir env prefix and debug log stem.
pub const APP_NAME: &str = "pi";
/// `CONFIG_DIR_NAME` (`config.ts:606`).
pub const CONFIG_DIR_NAME: &str = ".pi";
/// `ENV_AGENT_DIR` (`config.ts:610`), e.g. `PI_CODING_AGENT_DIR`.
pub const ENV_AGENT_DIR: &str = "PI_CODING_AGENT_DIR";
/// `ENV_SESSION_DIR` (`config.ts:611`).
pub const ENV_SESSION_DIR: &str = "PI_CODING_AGENT_SESSION_DIR";

/// The user's home directory (`os.homedir()`).
pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// `normalizePath` (`utils/paths.ts`) with the default tilde expansion: `~`
/// becomes the home directory, `~/...` is joined onto it. The Windows shell
/// path rewriting and `file://` URL handling are not portable here.
pub fn expand_tilde_path(path: &str) -> String {
    expand_tilde_path_with(path, &home_dir())
}

/// [`expand_tilde_path`] against an explicit home directory (the pure core).
pub fn expand_tilde_path_with(path: &str, home: &Path) -> String {
    if path == "~" {
        return home.to_string_lossy().into_owned();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest).to_string_lossy().into_owned();
    }
    path.to_string()
}

/// The agent dir for an optional `$PI_CODING_AGENT_DIR` value
/// (`getAgentDir`, `config.ts:630-636`).
pub fn agent_dir_for(env_dir: Option<&str>, home: &Path) -> PathBuf {
    match env_dir.filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(expand_tilde_path_with(dir, home)),
        None => home.join(CONFIG_DIR_NAME).join("agent"),
    }
}

/// The sessions root for an optional `$PI_CODING_AGENT_SESSION_DIR` value.
pub fn sessions_dir_for(env_dir: Option<&str>, agent_dir: &Path) -> PathBuf {
    match env_dir.filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(expand_tilde_path_with(dir, &home_dir())),
        None => agent_dir.join("sessions"),
    }
}

/// `getAgentDir` (`config.ts:630-636`): `$PI_CODING_AGENT_DIR` (tilde
/// expanded), else `~/.pi/agent`.
pub fn get_agent_dir() -> PathBuf {
    agent_dir_for(std::env::var(ENV_AGENT_DIR).ok().as_deref(), &home_dir())
}

/// `getSessionsDir` (`config.ts:674-676`). Honors `PI_CODING_AGENT_SESSION_DIR`
/// so the session root can move independently of the agent dir.
pub fn get_sessions_dir() -> PathBuf {
    sessions_dir_for(
        std::env::var(ENV_SESSION_DIR).ok().as_deref(),
        &get_agent_dir(),
    )
}

/// Path under the agent dir.
fn agent_dir_join(parts: &[&str]) -> PathBuf {
    let mut path = get_agent_dir();
    for part in parts {
        path.push(part);
    }
    path
}

/// `getCustomThemesDir` (`config.ts:639-641`).
pub fn get_custom_themes_dir() -> PathBuf {
    agent_dir_join(&["themes"])
}

/// `getModelsPath` (`config.ts:644-646`).
pub fn get_models_path() -> PathBuf {
    agent_dir_join(&["models.json"])
}

/// `getAuthPath` (`config.ts:649-651`).
pub fn get_auth_path() -> PathBuf {
    agent_dir_join(&["auth.json"])
}

/// `getSettingsPath` (`config.ts:654-656`).
pub fn get_settings_path() -> PathBuf {
    agent_dir_join(&["settings.json"])
}

/// `getToolsDir` (`config.ts:659-661`).
pub fn get_tools_dir() -> PathBuf {
    agent_dir_join(&["tools"])
}

/// `getBinDir` (`config.ts:664-666`): managed binaries (fd, rg).
pub fn get_bin_dir() -> PathBuf {
    agent_dir_join(&["bin"])
}

/// `getPromptsDir` (`config.ts:669-671`).
pub fn get_prompts_dir() -> PathBuf {
    agent_dir_join(&["prompts"])
}

/// `getDebugLogPath` (`config.ts:679-681`): `<agentDir>/<APP_NAME>-debug.log`.
pub fn get_debug_log_path() -> PathBuf {
    agent_dir_join(&[&format!("{APP_NAME}-debug.log")])
}

/// `getShareViewerUrl` (`config.ts:619-623`): the share viewer URL for a gist
/// id, honoring `PI_SHARE_VIEWER_URL`.
pub fn get_share_viewer_url(gist_id: &str) -> String {
    let base_url = std::env::var("PI_SHARE_VIEWER_URL")
        .unwrap_or_else(|_| "https://pi.dev/session/".to_string());
    format!("{base_url}#{gist_id}")
}

/// Resolve a path against a base directory the way `resolvePath`
/// (`utils/paths.ts:103-107`) does: tilde-expanded, then made absolute against
/// the base when relative, and lexically normalized (symlinks are *not*
/// resolved).
pub fn resolve_config_path(input: &Path, base_dir: &Path) -> PathBuf {
    let expanded = expand_tilde_path_with(&input.to_string_lossy(), &home_dir());
    let expanded = PathBuf::from(expanded);
    if expanded.is_absolute() {
        crate::session::resolve_path(&expanded)
    } else {
        crate::session::resolve_path(&base_dir.join(expanded))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_the_ts_defaults() {
        assert_eq!(APP_NAME, "pi");
        assert_eq!(CONFIG_DIR_NAME, ".pi");
        assert_eq!(ENV_AGENT_DIR, "PI_CODING_AGENT_DIR");
        assert_eq!(ENV_SESSION_DIR, "PI_CODING_AGENT_SESSION_DIR");
    }

    #[test]
    fn tilde_expansion_matches_normalize_path() {
        let home = Path::new("/home/tester");
        assert_eq!(expand_tilde_path_with("~", home), "/home/tester");
        assert_eq!(
            expand_tilde_path_with("~/projects", home),
            "/home/tester/projects"
        );
        assert_eq!(expand_tilde_path_with("/absolute", home), "/absolute");
        assert_eq!(expand_tilde_path_with("relative", home), "relative");
        // A mid-path tilde is literal, like pi's.
        assert_eq!(expand_tilde_path_with("/home/~user", home), "/home/~user");
    }

    #[test]
    fn agent_dir_env_override_wins_over_home() {
        let home = Path::new("/home/tester");
        assert_eq!(
            agent_dir_for(Some("/tmp/ai-agent/override"), home),
            PathBuf::from("/tmp/ai-agent/override")
        );
        // An empty override falls back, like a falsy `process.env` entry.
        assert_eq!(
            agent_dir_for(Some(""), home),
            home.join(CONFIG_DIR_NAME).join("agent")
        );
        assert_eq!(
            agent_dir_for(None, home),
            home.join(CONFIG_DIR_NAME).join("agent")
        );
        assert_eq!(
            agent_dir_for(Some("~/pi-agent"), home),
            PathBuf::from("/home/tester/pi-agent")
        );
    }

    #[test]
    fn default_paths_hang_off_the_agent_dir() {
        let agent = Path::new("/agent");
        assert_eq!(sessions_dir_for(Some(""), agent), agent.join("sessions"));
        assert_eq!(
            sessions_dir_for(Some("/sessions"), agent),
            PathBuf::from("/sessions")
        );
    }

    #[test]
    fn derived_paths_hang_off_get_agent_dir() {
        // Derive from whatever env is in place, and only assert the suffix
        // relationships, which hold for the default and the override alike.
        let agent = get_agent_dir();
        assert_eq!(get_custom_themes_dir(), agent.join("themes"));
        assert_eq!(get_models_path(), agent.join("models.json"));
        assert_eq!(get_auth_path(), agent.join("auth.json"));
        assert_eq!(get_settings_path(), agent.join("settings.json"));
        assert_eq!(get_tools_dir(), agent.join("tools"));
        assert_eq!(get_bin_dir(), agent.join("bin"));
        assert_eq!(get_prompts_dir(), agent.join("prompts"));
        assert_eq!(get_debug_log_path(), agent.join("pi-debug.log"));
    }

    #[test]
    fn resolve_config_path_makes_relative_paths_absolute() {
        let base = Path::new("/base/dir");
        assert_eq!(
            resolve_config_path(Path::new("/abs"), base),
            PathBuf::from("/abs")
        );
        assert_eq!(
            resolve_config_path(Path::new("rel/ative"), base),
            PathBuf::from("/base/dir/rel/ative")
        );
        assert_eq!(
            resolve_config_path(Path::new("~/x"), base),
            home_dir().join("x")
        );
    }
}
