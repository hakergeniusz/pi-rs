//! User config paths and the app identity (`config.ts`).
//!
//! Ground truth: `packages/coding-agent/src/config.ts:603-681` — the app
//! name/config-dir constants, the agent-dir environment overrides, and the
//! derived per-artifact paths. Everything package-manager-specific in that
//! file (Bun/npm install detection and self-update command building,
//! `:1-576`) has no Rust counterpart and is intentionally not ported.
//!
//! # Deviation: XDG base directories instead of `~/.pi`
//!
//! Upstream roots every user-level artifact under `~/.pi/agent`. pi-rs
//! follows the XDG Base Directory spec instead, so it never touches a real
//! TypeScript pi install:
//!
//! | role  | default                                         | artifacts                                    |
//! |-------|-------------------------------------------------|----------------------------------------------|
//! | config | `$XDG_CONFIG_HOME/pi-rs` else `~/.config/pi-rs` | settings, auth, models, themes, prompts, tools, skills, trust |
//! | data   | `$XDG_DATA_HOME/pi-rs` else `~/.local/share/pi-rs` | sessions                                  |
//! | cache  | `$XDG_CACHE_HOME/pi-rs` else `~/.cache/pi-rs`   | managed binaries (fd, rg)                    |
//! | state  | `$XDG_STATE_HOME/pi-rs` else `~/.local/state/pi-rs` | debug log                                |
//!
//! `$PI_CODING_AGENT_DIR` (upstream's override) collapses every role back
//! into one directory: with it set, the layout is exactly upstream's. Empty
//! or non-absolute XDG values fall back to the defaults, per spec.
//! Project-local `.pi/` directories are upstream-compatible and unaffected
//! (see [`PROJECT_CONFIG_DIR_NAME`]).
//!
//! The global + project *settings* store lives in [`settings`]
//! (`core/settings-manager.ts`), and the credential store in [`crate::auth`]
//! (`core/auth-storage.ts`); both hang off [`get_agent_dir`] as in TS.

pub mod settings;

use std::path::{Path, PathBuf};

/// `PACKAGE_NAME` (`config.ts:603`).
pub const PACKAGE_NAME: &str = "@earendil-works/pi-coding-agent";
/// `APP_NAME` (`config.ts:604`): the debug log stem.
pub const APP_NAME: &str = "pi";
/// The pi-rs leaf directory under each XDG base dir. Deviation: upstream
/// uses a single `~/.pi` root instead.
pub const CONFIG_DIR_NAME: &str = "pi-rs";
/// The project-local config directory, upstream-compatible: a checkout can
/// ship `.pi/settings.json`, `.pi/skills/`, `.pi/extensions/`.
pub const PROJECT_CONFIG_DIR_NAME: &str = ".pi";
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

/// An XDG base directory per the spec: `$env` when it is set, non-empty and
/// absolute; otherwise `home/<default_suffix>` (the pure core).
pub fn xdg_base_dir_for(env_value: Option<&str>, home: &Path, default_suffix: &str) -> PathBuf {
    match env_value.filter(|value| !value.is_empty() && Path::new(value).is_absolute()) {
        Some(base) => PathBuf::from(base),
        None => home.join(default_suffix),
    }
}

/// The config-role root for the explicit `$XDG_CONFIG_HOME` value.
pub fn config_dir_for(env_value: Option<&str>, home: &Path) -> PathBuf {
    xdg_base_dir_for(env_value, home, ".config").join(CONFIG_DIR_NAME)
}

/// The data-role root for the explicit `$XDG_DATA_HOME` value.
pub fn data_dir_for(env_value: Option<&str>, home: &Path) -> PathBuf {
    xdg_base_dir_for(env_value, home, ".local/share").join(CONFIG_DIR_NAME)
}

/// The cache-role root for the explicit `$XDG_CACHE_HOME` value.
pub fn cache_dir_for(env_value: Option<&str>, home: &Path) -> PathBuf {
    xdg_base_dir_for(env_value, home, ".cache").join(CONFIG_DIR_NAME)
}

/// The state-role root for the explicit `$XDG_STATE_HOME` value.
pub fn state_dir_for(env_value: Option<&str>, home: &Path) -> PathBuf {
    xdg_base_dir_for(env_value, home, ".local/state").join(CONFIG_DIR_NAME)
}

/// The XDG config root for pi-rs (`$XDG_CONFIG_HOME`/`~/.config` + leaf).
pub fn get_config_dir() -> PathBuf {
    config_dir_for(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        &home_dir(),
    )
}

/// The XDG data root for pi-rs (`$XDG_DATA_HOME`/`~/.local/share` + leaf).
pub fn get_data_dir() -> PathBuf {
    data_dir_for(std::env::var("XDG_DATA_HOME").ok().as_deref(), &home_dir())
}

/// The XDG cache root for pi-rs (`$XDG_CACHE_HOME`/`~/.cache` + leaf).
pub fn get_cache_dir() -> PathBuf {
    cache_dir_for(std::env::var("XDG_CACHE_HOME").ok().as_deref(), &home_dir())
}

/// The XDG state root for pi-rs (`$XDG_STATE_HOME`/`~/.local/state` + leaf).
pub fn get_state_dir() -> PathBuf {
    state_dir_for(std::env::var("XDG_STATE_HOME").ok().as_deref(), &home_dir())
}

/// `$PI_CODING_AGENT_DIR` when set and non-empty, tilde expanded.
pub fn agent_dir_override() -> Option<PathBuf> {
    std::env::var(ENV_AGENT_DIR)
        .ok()
        .as_deref()
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(expand_tilde_path_with(dir, &home_dir())))
}

/// The agent dir (config role) for the explicit env values (`getAgentDir`,
/// `config.ts:630-636`): `$PI_CODING_AGENT_DIR` (tilde expanded), else the
/// XDG config root. Deviation: the default is `~/.config/pi-rs`, not
/// `~/.pi/agent`.
pub fn agent_dir_for(env_dir: Option<&str>, xdg_config: Option<&str>, home: &Path) -> PathBuf {
    match env_dir.filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(expand_tilde_path_with(dir, home)),
        None => config_dir_for(xdg_config, home),
    }
}

/// `getAgentDir`: the config-role root — `$PI_CODING_AGENT_DIR` when set,
/// else [`get_config_dir`]. Settings, auth, models, themes, prompts, tools,
/// skills and the trust store all hang off it.
pub fn get_agent_dir() -> PathBuf {
    agent_dir_for(
        std::env::var(ENV_AGENT_DIR).ok().as_deref(),
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        &home_dir(),
    )
}

/// The sessions root for the explicit env values (`getSessionsDir`,
/// `config.ts:674-676`): `$PI_CODING_AGENT_SESSION_DIR`, else the agent
/// override's `sessions/` when `$PI_CODING_AGENT_DIR` is set (upstream's
/// single-dir layout), else the XDG data root's `sessions/`.
pub fn sessions_dir_for(
    session_env: Option<&str>,
    agent_override: Option<&Path>,
    data_root: &Path,
) -> PathBuf {
    if let Some(dir) = session_env.filter(|dir| !dir.is_empty()) {
        return PathBuf::from(expand_tilde_path_with(dir, &home_dir()));
    }
    if let Some(agent) = agent_override {
        return agent.join("sessions");
    }
    data_root.join("sessions")
}

/// `getSessionsDir`: honors `PI_CODING_AGENT_SESSION_DIR` so the session
/// root can move independently of the config root.
pub fn get_sessions_dir() -> PathBuf {
    sessions_dir_for(
        std::env::var(ENV_SESSION_DIR).ok().as_deref(),
        agent_dir_override().as_deref(),
        &get_data_dir(),
    )
}

/// Path under the agent dir (config role).
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

/// The managed-binaries dir for the explicit values (`getBinDir`,
/// `config.ts:664-666`): the agent override's `bin/`, else the XDG cache
/// root's `bin/` (fd and rg are re-fetchable, so they are cache, not config).
pub fn bin_dir_for(agent_override: Option<&Path>, cache_root: &Path) -> PathBuf {
    match agent_override {
        Some(dir) => dir.join("bin"),
        None => cache_root.join("bin"),
    }
}

/// `getBinDir`: managed binaries (fd, rg).
pub fn get_bin_dir() -> PathBuf {
    bin_dir_for(agent_dir_override().as_deref(), &get_cache_dir())
}

/// `getPromptsDir` (`config.ts:669-671`).
pub fn get_prompts_dir() -> PathBuf {
    agent_dir_join(&["prompts"])
}

/// The debug log path for the explicit values (`getDebugLogPath`,
/// `config.ts:679-681`): the agent override's log, else the XDG state root's
/// (logs are state, per the XDG state directory spec).
pub fn debug_log_path_for(agent_override: Option<&Path>, state_root: &Path) -> PathBuf {
    match agent_override {
        Some(dir) => dir.join(format!("{APP_NAME}-debug.log")),
        None => state_root.join(format!("{APP_NAME}-debug.log")),
    }
}

/// `getDebugLogPath`: `<agentDir>/pi-debug.log` upstream; the XDG state root
/// here unless `$PI_CODING_AGENT_DIR` is set.
pub fn get_debug_log_path() -> PathBuf {
    debug_log_path_for(agent_dir_override().as_deref(), &get_state_dir())
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
    fn constants_document_the_xdg_deviation() {
        assert_eq!(APP_NAME, "pi");
        assert_eq!(CONFIG_DIR_NAME, "pi-rs");
        // Project-local directories stay upstream-compatible.
        assert_eq!(PROJECT_CONFIG_DIR_NAME, ".pi");
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
    fn xdg_base_dirs_need_absolute_non_empty_values() {
        let home = Path::new("/home/tester");
        assert_eq!(
            xdg_base_dir_for(Some("/xdg/cfg"), home, ".config"),
            PathBuf::from("/xdg/cfg")
        );
        // Empty means unset, and relative values are ignored, per spec.
        assert_eq!(
            xdg_base_dir_for(Some(""), home, ".config"),
            home.join(".config")
        );
        assert_eq!(
            xdg_base_dir_for(Some("relative"), home, ".config"),
            home.join(".config")
        );
        assert_eq!(
            xdg_base_dir_for(None, home, ".local/state"),
            home.join(".local/state")
        );
    }

    #[test]
    fn xdg_roots_carry_the_pi_rs_leaf() {
        let home = Path::new("/home/tester");
        assert_eq!(config_dir_for(Some("/c"), home), PathBuf::from("/c/pi-rs"));
        assert_eq!(config_dir_for(None, home), home.join(".config/pi-rs"));
        assert_eq!(data_dir_for(None, home), home.join(".local/share/pi-rs"));
        assert_eq!(cache_dir_for(None, home), home.join(".cache/pi-rs"));
        assert_eq!(state_dir_for(None, home), home.join(".local/state/pi-rs"));
    }

    #[test]
    fn agent_dir_override_wins_over_xdg() {
        let home = Path::new("/home/tester");
        assert_eq!(
            agent_dir_for(Some("/tmp/ai-agent/override"), Some("/c"), home),
            PathBuf::from("/tmp/ai-agent/override")
        );
        // An empty override falls back, like a falsy `process.env` entry.
        assert_eq!(
            agent_dir_for(Some(""), Some("/c"), home),
            PathBuf::from("/c/pi-rs")
        );
        assert_eq!(agent_dir_for(None, None, home), home.join(".config/pi-rs"));
        assert_eq!(
            agent_dir_for(Some("~/pi-agent"), Some("/c"), home),
            PathBuf::from("/home/tester/pi-agent")
        );
    }

    #[test]
    fn sessions_root_follows_the_role_map() {
        let data = Path::new("/data/pi-rs");
        let agent = Path::new("/agent-override");
        assert_eq!(
            sessions_dir_for(Some("/sessions"), Some(agent), data),
            PathBuf::from("/sessions")
        );
        // An empty session override falls through, like a falsy env entry.
        assert_eq!(
            sessions_dir_for(Some(""), Some(agent), data),
            agent.join("sessions")
        );
        assert_eq!(
            sessions_dir_for(None, Some(agent), data),
            agent.join("sessions")
        );
        assert_eq!(sessions_dir_for(None, None, data), data.join("sessions"));
    }

    #[test]
    fn bin_and_debug_log_follow_the_role_map() {
        let agent = Path::new("/agent-override");
        let cache = Path::new("/cache/pi-rs");
        let state = Path::new("/state/pi-rs");
        assert_eq!(bin_dir_for(None, cache), cache.join("bin"));
        assert_eq!(bin_dir_for(Some(agent), cache), agent.join("bin"));
        assert_eq!(debug_log_path_for(None, state), state.join("pi-debug.log"));
        assert_eq!(
            debug_log_path_for(Some(agent), state),
            agent.join("pi-debug.log")
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
        assert_eq!(get_prompts_dir(), agent.join("prompts"));
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
