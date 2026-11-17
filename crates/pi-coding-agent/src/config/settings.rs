//! pi's settings store: the global + project `settings.json` layers, their
//! merge, and the read-modify-write persistence.
//!
//! Ground truth: `packages/coding-agent/src/core/settings-manager.ts` — the
//! `Settings` shape (`:33-190`), the merge helpers (`:190-254`), the storage
//! backends (`:267-377`), the load path (`:471-498`), and the manager
//! (`:378-1533`). The data-layer surface is ported: scoped access, merging,
//! migration, and persistence, plus the typed per-key accessors the
//! `/settings` selector reads and writes (the `settings-manager.ts:828-1533`
//! getter/setter pairs, keyed by the `settings-schema.ts` names and the
//! `settings-defaults.ts` defaults). The HTTP idle timeout helpers port
//! `core/http-dispatcher.ts:4-43` (`DEFAULT_HTTP_IDLE_TIMEOUT_MS`,
//! `HTTP_IDLE_TIMEOUT_CHOICES`, `parseHttpIdleTimeoutMs`,
//! `formatHttpIdleTimeoutMs`) — the dispatcher machinery they configure is
//! not ported yet.
//!
//! What is faithful:
//!
//! * two scopes — global `<agentDir>/settings.json`, project
//!   `<cwd>/.pi/settings.json` (`:300-301`).
//! * the load path: skip the project scope untrusted (`:472-474`), strip a
//!   BOM, `JSON.parse`, then [`migrate_settings`] (`:471-490`).
//! * a malformed file is **not** fatal: that scope falls back to `{}` and the
//!   error is queued for [`SettingsManager::drain_errors`] (`:491-498`).
//! * four precedence rungs — defaults (in the getters), global, project, then
//!   [`SettingsManager::apply_overrides`] as a runtime-only layer (`:636-638`).
//!   Overrides are not persisted, and `save()` recomputes the merge, which
//!   drops them again — exactly as pi does.
//! * **no schema**. [`Settings`] is an untyped map because pi's is a
//!   TypeScript interface: unknown keys survive the merge and are written back
//!   verbatim, and that round-trip is load-bearing in the read-modify-write.
//! * the modified-field read-modify-write (`persist_scoped_settings`,
//!   `:711-734`): re-read the file, copy across only the fields this process
//!   marked modified, merge only the marked nested keys over the file's
//!   object, then write with a two-space indent and no trailing newline.
//!
//! One deliberate deviation, carried over from the phase-P3 spike: the file
//! storage takes no lockfile (pi chains `proper-lockfile.lockSync` with an
//! `ELOCKED` retry, `:304-360`). The read-modify-write of
//! [`SettingsManager::flush`] is ported verbatim, which is what keeps
//! concurrent writers from clobbering each other's unmodified fields; the
//! inter-process lock itself is deferred until the CLI needs it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};

use crate::session::json::format_f64;

/// pi's `Settings`: an interface, so at runtime a plain record
/// (`settings-manager.ts:33-190`). Untyped on purpose — see the module doc.
pub type Settings = Map<String, Value>;

/// Tools enabled at startup when `defaultTools` does not change them
/// (`settings-manager.ts:215`, `settings-defaults.ts`).
pub const DEFAULT_TOOL_NAMES: [&str; 4] = ["read", "bash", "edit", "write"];

/// `SETTINGS_DEFAULTS.compaction` (`settings-defaults.ts`).
pub const DEFAULT_COMPACTION_ENABLED: bool = true;
pub const DEFAULT_COMPACTION_RESERVE_TOKENS: i64 = 16_384;
pub const DEFAULT_COMPACTION_KEEP_RECENT_TOKENS: i64 = 20_000;

/// `DEFAULT_MAX_AGENT_RETRY_DELAY_MS` as used by `getRetrySettings`
/// (`settings-manager.ts:1000-1007`).
pub const DEFAULT_MAX_AGENT_RETRY_DELAY_MS: i64 = 60_000;

/// `SETTINGS_DEFAULTS.branchSummary` (`settings-defaults.ts`).
pub const DEFAULT_BRANCH_SUMMARY_RESERVE_TOKENS: i64 = 16_384;
pub const DEFAULT_BRANCH_SUMMARY_SKIP_PROMPT: bool = false;

/// `SETTINGS_DEFAULTS.retry` (`settings-defaults.ts`).
pub const DEFAULT_RETRY_ENABLED: bool = true;
pub const DEFAULT_RETRY_MAX_RETRIES: i64 = 3;
pub const DEFAULT_RETRY_BASE_DELAY_MS: i64 = 2000;

/// The `SETTINGS_DEFAULTS` values the selector-driven accessors resolve
/// (`settings-defaults.ts`); grouped by their `settings-schema.ts` key.
pub const DEFAULT_TRANSPORT: &str = "auto";
pub const DEFAULT_HIDE_THINKING_BLOCK: bool = false;
pub const DEFAULT_SHOW_CACHE_MISS_NOTICES: bool = false;
pub const DEFAULT_QUIET_STARTUP: QuietStartup = QuietStartup::Off;
pub const DEFAULT_DEFAULT_PROJECT_TRUST: DefaultProjectTrust = DefaultProjectTrust::Ask;
pub const DEFAULT_COLLAPSE_CHANGELOG: bool = false;
pub const DEFAULT_ENABLE_INSTALL_TELEMETRY: bool = true;
pub const DEFAULT_ENABLE_SKILL_COMMANDS: bool = true;
pub const DEFAULT_DOUBLE_ESCAPE_ACTION: &str = "tree";
pub const DEFAULT_EDITOR_PADDING_X: i64 = 0;
pub const DEFAULT_OUTPUT_PAD: i64 = 1;
pub const DEFAULT_AUTOCOMPLETE_MAX_VISIBLE: i64 = 5;
pub const DEFAULT_MERMAID_RENDERING_MODE: MermaidRenderingMode = MermaidRenderingMode::Streaming;
pub const DEFAULT_WARNINGS_ANTHROPIC_EXTRA_USAGE: bool = true;
/// `SETTINGS_DEFAULTS.terminal` (`settings-defaults.ts:35-40`).
pub const DEFAULT_TERMINAL_SHOW_IMAGES: bool = true;
pub const DEFAULT_TERMINAL_IMAGE_WIDTH_CELLS: i64 = 60;
pub const DEFAULT_TERMINAL_CLEAR_ON_SHRINK: bool = false;
pub const DEFAULT_TERMINAL_SHOW_TERMINAL_PROGRESS: bool = false;
/// `SETTINGS_DEFAULTS.images` (`settings-defaults.ts:41-44`).
pub const DEFAULT_IMAGES_AUTO_RESIZE: bool = true;
pub const DEFAULT_IMAGES_BLOCK_IMAGES: bool = false;
/// `SETTINGS_DEFAULTS.cacheWarming` (`settings-defaults.ts:61`).
pub const DEFAULT_CACHE_WARMING: &str = "idle";

/// `DEFAULT_HTTP_IDLE_TIMEOUT_MS` (`core/http-dispatcher.ts:4`).
pub const DEFAULT_HTTP_IDLE_TIMEOUT_MS: i64 = 300_000;

/// `HTTP_IDLE_TIMEOUT_CHOICES` (`core/http-dispatcher.ts:8-14`): the row
/// values of the HTTP idle timeout setting, label first.
pub const HTTP_IDLE_TIMEOUT_CHOICES: [(&str, i64); 5] = [
    ("30 sec", 30_000),
    ("1 min", 60_000),
    ("2 min", 120_000),
    ("5 min", 300_000),
    ("disabled", 0),
];

/// `CACHE_WARMING_MODES` (`settings-manager.ts:54`).
pub const CACHE_WARMING_MODES: [&str; 3] = ["off", "streaming", "idle"];

/// `parseHttpIdleTimeoutMs` (`core/http-dispatcher.ts:19-35`): a trimmed
/// case-insensitive `"disabled"` is 0, a numeric string or a non-negative
/// finite number floors, anything else (including an empty string) is
/// `None`.
pub fn parse_http_idle_timeout_ms(value: &Value) -> Option<i64> {
    let number = match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.eq_ignore_ascii_case("disabled") {
                return Some(0);
            }
            if trimmed.is_empty() {
                return None;
            }
            // `Number(trimmed)` in TS; an unparseable string is NaN.
            Some(trimmed.parse::<f64>().ok()?)
        }
        Value::Number(number) => Some(number.as_f64()?),
        _ => None,
    };
    let timeout = number?;
    if !timeout.is_finite() || timeout < 0.0 {
        return None;
    }
    Some(timeout.floor() as i64)
}

/// `formatHttpIdleTimeoutMs` (`core/http-dispatcher.ts:37-43`): a choice
/// label when the value is one, else the seconds (`45000` → `"45 sec"`).
pub fn format_http_idle_timeout_ms(timeout_ms: i64) -> String {
    if let Some((label, _)) = HTTP_IDLE_TIMEOUT_CHOICES
        .iter()
        .find(|(_, choice)| *choice == timeout_ms)
    {
        return (*label).to_string();
    }
    format!("{} sec", timeout_ms as f64 / 1000.0)
}

// ---------------------------------------------------------------------------
// merge helpers
// ---------------------------------------------------------------------------

/// `isMergeableObject` (`settings-manager.ts:190-193`): a non-null,
/// non-array object.
fn is_mergeable_object(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Object(_)))
}

/// `deepMergeObjects` (`settings-manager.ts:195-213`). Objects recurse,
/// everything else — **arrays included** — replaces wholesale.
pub fn deep_merge_objects(base: &Settings, overrides: &Settings) -> Settings {
    let mut result = base.clone();
    for (key, override_value) in overrides {
        let merged = match (result.get(key), override_value) {
            (Some(base_value), Value::Object(overrides_object))
                if is_mergeable_object(Some(base_value)) =>
            {
                let base_object = base_value.as_object().expect("mergeable base");
                Value::Object(deep_merge_objects(base_object, overrides_object))
            }
            _ => override_value.clone(),
        };
        // `Map::insert` keeps an existing key in place, which is what
        // `{ ...base }` then assignment does in JS.
        result.insert(key.clone(), merged);
    }
    result
}

/// `isToolModifier` (`settings-manager.ts:217-219`).
fn is_tool_modifier(entry: &str) -> bool {
    entry.starts_with('+') || entry.starts_with('-')
}

/// `mergeDefaultTools` (`settings-manager.ts:225-233`): a list with plain tool
/// names replaces the inherited one; a list of only `+name`/`-name` appends to
/// it. A malformed value replaces rather than throwing — settings files are
/// not validated.
pub fn merge_default_tools(base: Option<&Value>, overrides: Option<&Value>) -> Option<Value> {
    let Some(overrides) = overrides else {
        return base.cloned();
    };
    let (Some(base), Some(override_array)) = (base.and_then(Value::as_array), overrides.as_array())
    else {
        // Either side is not an array: the malformed override replaces.
        return Some(overrides.clone());
    };
    let all_modifiers = override_array.iter().filter_map(Value::as_str).count()
        == override_array.len()
        && override_array
            .iter()
            .filter_map(Value::as_str)
            .all(is_tool_modifier);
    if !all_modifiers {
        return Some(Value::Array(override_array.clone()));
    }
    let mut merged = base.clone();
    merged.extend(override_array.iter().cloned());
    Some(Value::Array(merged))
}

/// `resolveDefaultTools` (`settings-manager.ts:137-152`): plain names replace
/// [`DEFAULT_TOOL_NAMES`], then `+name` appends and `-name` removes, in order.
pub fn resolve_default_tools(entries: &[Value]) -> Vec<String> {
    let strings: Vec<&str> = entries.iter().filter_map(Value::as_str).collect();
    let plain: Vec<String> = strings
        .iter()
        .filter(|entry| !is_tool_modifier(entry))
        .map(|entry| (*entry).to_string())
        .collect();
    let mut tools = if !plain.is_empty() || entries.is_empty() {
        plain
    } else {
        DEFAULT_TOOL_NAMES
            .iter()
            .map(|name| (*name).to_string())
            .collect()
    };
    for entry in strings {
        if !is_tool_modifier(entry) {
            continue;
        }
        let name = &entry[1..];
        let index = tools.iter().position(|tool| tool == name);
        if entry.starts_with('+') && index.is_none() && !name.is_empty() {
            tools.push(name.to_string());
        } else if entry.starts_with('-')
            && let Some(index) = index
        {
            tools.remove(index);
        }
    }
    tools
}

/// `deepMergeSettings` (`settings-manager.ts:250-254`): the object merge, then
/// `defaultTools` through [`merge_default_tools`].
pub fn deep_merge_settings(base: &Settings, overrides: &Settings) -> Settings {
    let mut merged = deep_merge_objects(base, overrides);
    if let Some(value) =
        merge_default_tools(base.get("defaultTools"), overrides.get("defaultTools"))
    {
        merged.insert("defaultTools".to_string(), value);
    }
    merged
}

// ---------------------------------------------------------------------------
// value helpers
// ---------------------------------------------------------------------------

/// `stripBom` (`utils/text.ts`).
fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{feff}').unwrap_or(content)
}

/// `String(number)` for the error messages pi formats a number into. V8 number
/// formatting via the session JSON writer (`3.0` prints as `3`).
fn js_number_to_string(number: f64) -> String {
    if number.is_finite() && number.fract() == 0.0 && number.abs() < 1e21 {
        format!("{}", number as i64)
    } else {
        format_f64(number)
    }
}

/// `String(value)` for the error messages pi builds from a raw setting value.
fn js_value_to_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(boolean) => boolean.to_string(),
        Value::Number(number) => number
            .as_f64()
            .map_or_else(|| number.to_string(), js_number_to_string),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// `Number.isSafeInteger(n) && n >= 0` for a JSON value.
fn is_non_negative_safe_integer(value: &Value) -> bool {
    value.as_f64().is_some_and(|number| {
        number.is_finite()
            && number.fract() == 0.0
            && (0.0..=9_007_199_254_740_991.0).contains(&number)
    })
}

/// A `??`-style string read: absent or non-string yields `None`.
fn opt_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) => Some(text.clone()),
        _ => None,
    }
}

/// JavaScript truthiness of a JSON value, for the `??` getters that accept any
/// type (`settings-manager.ts:1046`).
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(boolean) => *boolean,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// A `??`-style boolean read: absent or `null` falls back, anything else
/// coerces by JavaScript truthiness.
fn bool_or(value: Option<&Value>, default: bool) -> bool {
    match value {
        None | Some(Value::Null) => default,
        Some(value) => js_truthy(value),
    }
}

// ---------------------------------------------------------------------------
// migrations
// ---------------------------------------------------------------------------

/// `migrateSettings` (`settings-manager.ts:501-556`), in place, on every load
/// and on every read-modify-write.
pub fn migrate_settings(settings: &mut Settings) {
    // queueMode -> steeringMode
    if let Some(queue_mode) = settings.get("queueMode").cloned() {
        if !settings.contains_key("steeringMode") {
            settings.insert("steeringMode".to_string(), queue_mode);
        }
        settings.remove("queueMode");
    }

    // legacy websockets boolean -> transport enum
    if !settings.contains_key("transport")
        && let Some(Value::Bool(websockets)) = settings.get("websockets").cloned()
    {
        settings.insert(
            "transport".to_string(),
            Value::String(if websockets { "websocket" } else { "sse" }.to_string()),
        );
        settings.remove("websockets");
    }

    // old skills object -> enableSkillCommands + array skills
    if let Some(Value::Object(skills_settings)) = settings.get("skills").cloned() {
        if let Some(enabled) = skills_settings.get("enableSkillCommands").cloned()
            && !settings.contains_key("enableSkillCommands")
        {
            settings.insert("enableSkillCommands".to_string(), enabled);
        }
        let directories = skills_settings
            .get("customDirectories")
            .and_then(Value::as_array);
        match directories {
            Some(directories) if !directories.is_empty() => {
                settings.insert("skills".to_string(), Value::Array(directories.clone()));
            }
            _ => {
                settings.remove("skills");
            }
        }
    }

    // retry.maxDelayMs -> retry.provider.maxRetryDelayMs
    if let Some(Value::Object(retry)) = settings.get("retry").cloned() {
        let mut retry = retry;
        let provider = match retry.get("provider") {
            Some(Value::Object(provider)) => Some(provider.clone()),
            _ => None,
        };
        let delay = match retry.get("maxDelayMs") {
            Some(Value::Number(number)) => Some(number.clone()),
            _ => None,
        };
        if let Some(delay) = delay {
            let provider_has_delay = provider.as_ref().is_some_and(|provider| {
                !matches!(provider.get("maxRetryDelayMs"), None | Some(Value::Null))
            });
            if !provider_has_delay {
                let mut next_provider = provider.unwrap_or_default();
                next_provider.insert("maxRetryDelayMs".to_string(), Value::Number(delay));
                retry.insert("provider".to_string(), Value::Object(next_provider));
            }
        }
        retry.remove("maxDelayMs");
        settings.insert("retry".to_string(), Value::Object(retry));
    }
}

// ---------------------------------------------------------------------------
// storage
// ---------------------------------------------------------------------------

/// pi's `SettingsScope` (`settings-manager.ts:267`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SettingsScope {
    Global,
    Project,
}

impl SettingsScope {
    /// The scope name, as pi reports it.
    pub fn as_str(&self) -> &'static str {
        match self {
            SettingsScope::Global => "global",
            SettingsScope::Project => "project",
        }
    }
}

/// pi's `SettingsError` (`settings-manager.ts:277-281`), flattened to the
/// message the diagnostics wrapper prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsError {
    pub scope: SettingsScope,
    /// The file a reported error names. `None` for a store built over
    /// [`InMemorySettingsStorage`].
    pub path: Option<PathBuf>,
    pub message: String,
}

/// pi's `SettingsStorage` (`settings-manager.ts:273-275`): hands the current
/// file content to a callback and writes back whatever the callback returns
/// instead of `None`.
pub trait SettingsStorage: Send + Sync {
    fn with_lock(
        &self,
        scope: SettingsScope,
        action: &mut dyn FnMut(Option<String>) -> Result<Option<String>, String>,
    ) -> Result<(), String>;

    /// The file a scope reads, when the storage has files. Used only for
    /// diagnostics.
    fn path(&self, _scope: SettingsScope) -> Option<PathBuf> {
        None
    }
}

/// `FileSettingsStorage` (`settings-manager.ts:293-360`) as a **path holder**:
/// `with_lock` reads, calls, and writes back, creating the directory only on
/// the write path (`:345-348`). Reading an absent file creates nothing.
pub struct FileSettingsStorage {
    global_path: PathBuf,
    project_path: PathBuf,
}

impl FileSettingsStorage {
    /// `new FileSettingsStorage(cwd, agentDir)` (`:293-302`).
    pub fn new(cwd: &Path, agent_dir: &Path) -> FileSettingsStorage {
        FileSettingsStorage {
            global_path: agent_dir.join("settings.json"),
            project_path: cwd
                .join(crate::config::PROJECT_CONFIG_DIR_NAME)
                .join("settings.json"),
        }
    }

    /// The global settings file.
    pub fn global_path(&self) -> &Path {
        &self.global_path
    }

    /// The project settings file.
    pub fn project_path(&self) -> &Path {
        &self.project_path
    }
}

impl SettingsStorage for FileSettingsStorage {
    fn with_lock(
        &self,
        scope: SettingsScope,
        action: &mut dyn FnMut(Option<String>) -> Result<Option<String>, String>,
    ) -> Result<(), String> {
        let path = match scope {
            SettingsScope::Global => &self.global_path,
            SettingsScope::Project => &self.project_path,
        };
        let current = match std::fs::read_to_string(path) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.to_string()),
        };
        let Some(next) = action(current)? else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        std::fs::write(path, next).map_err(|error| error.to_string())
    }

    fn path(&self, scope: SettingsScope) -> Option<PathBuf> {
        Some(match scope {
            SettingsScope::Global => self.global_path.clone(),
            SettingsScope::Project => self.project_path.clone(),
        })
    }
}

/// `InMemorySettingsStorage` (`settings-manager.ts:362-377`), so a test
/// exercises the identical load path.
#[derive(Default)]
pub struct InMemorySettingsStorage {
    global: Mutex<Option<String>>,
    project: Mutex<Option<String>>,
}

impl SettingsStorage for InMemorySettingsStorage {
    fn with_lock(
        &self,
        scope: SettingsScope,
        action: &mut dyn FnMut(Option<String>) -> Result<Option<String>, String>,
    ) -> Result<(), String> {
        let slot = match scope {
            SettingsScope::Global => &self.global,
            SettingsScope::Project => &self.project,
        };
        let current = slot.lock().expect("in-memory settings").clone();
        let Some(next) = action(current)? else {
            return Ok(());
        };
        *slot.lock().expect("in-memory settings") = Some(next);
        Ok(())
    }
}

/// `resolvePath` (`utils/paths.ts`): tilde-expanded, then made absolute
/// against the process directory when it is relative.
pub fn resolve_path(input: &Path) -> PathBuf {
    let expanded = crate::config::expand_tilde_path(&input.to_string_lossy());
    let expanded = PathBuf::from(expanded);
    if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().unwrap_or_default().join(expanded)
    }
}

// ---------------------------------------------------------------------------
// manager
// ---------------------------------------------------------------------------

/// `SettingsManagerCreateOptions` (`settings-manager.ts:269-271`).
#[derive(Debug, Clone, Copy, Default)]
pub struct SettingsManagerCreateOptions {
    /// `None` is pi's `?? true`: trusted unless the caller says otherwise.
    pub project_trusted: Option<bool>,
}

/// One queued write: the snapshot plus the modified-field bookkeeping captured
/// at enqueue time (`settings-manager.ts:736-750`).
struct QueuedWrite {
    scope: SettingsScope,
    snapshot: Settings,
    fields: BTreeSet<String>,
    nested: BTreeMap<String, BTreeSet<String>>,
}

struct LoadResult {
    settings: Settings,
    error: Option<String>,
}

/// `getRetrySettings` (`settings-manager.ts:1000-1007`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetrySettings {
    pub enabled: bool,
    pub max_retries: i64,
    pub base_delay_ms: i64,
    pub max_agent_delay_ms: i64,
}

/// `getBranchSummarySettings` (`settings-manager.ts:976-981`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranchSummarySettings {
    pub reserve_tokens: i64,
    pub skip_prompt: bool,
}

/// The tree selector's display filter (`FilterMode`,
/// `tree-selector.ts:98`; `getTreeFilterMode`, `settings-manager.ts:1379-1383`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TreeFilterMode {
    /// Hide settings/bookkeeping entries.
    #[default]
    Default,
    /// Default minus tool results.
    NoTools,
    /// Just user messages.
    UserOnly,
    /// Just labeled entries.
    LabeledOnly,
    /// Show everything.
    All,
}

impl TreeFilterMode {
    /// The wire discriminants, in cycle order
    /// (`tree-selector.ts:1065-1076`).
    pub const MODES: [TreeFilterMode; 5] = [
        TreeFilterMode::Default,
        TreeFilterMode::NoTools,
        TreeFilterMode::UserOnly,
        TreeFilterMode::LabeledOnly,
        TreeFilterMode::All,
    ];

    /// The wire name → variant; an unknown name is `None` (the getter falls
    /// back to the default, `settings-manager.ts:1379-1383`).
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "default" => Some(Self::Default),
            "no-tools" => Some(Self::NoTools),
            "user-only" => Some(Self::UserOnly),
            "labeled-only" => Some(Self::LabeledOnly),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// The wire spelling (`settings-manager.ts:1380`, the `valid` list).
    pub fn as_name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::NoTools => "no-tools",
            Self::UserOnly => "user-only",
            Self::LabeledOnly => "labeled-only",
            Self::All => "all",
        }
    }

    /// The `[status]` suffix label (`getStatusLabels`, `tree-selector.ts:650-670`).
    pub fn status_label(self) -> &'static str {
        match self {
            Self::Default => "",
            Self::NoTools => " [no-tools]",
            Self::UserOnly => " [user]",
            Self::LabeledOnly => " [labeled]",
            Self::All => " [all]",
        }
    }
}

/// What the terminal shows when a fullscreen session ends
/// (`fullscreenExitOutput`, `settings-defaults.js:62`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FullscreenExitOutput {
    /// Repaint the transcript into the terminal's own scrollback, so leaving
    /// fullscreen reads like a normal program's output.
    #[default]
    Transcript,
    /// Leave the screen as the alt screen had it and print only the session
    /// resume hint.
    ResumeHint,
}

/// The fullscreen scrollbar mode (`fullscreenScrollbar`,
/// `settings-defaults.js:63`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FullscreenScrollbar {
    /// Show while scrolling, fade otherwise (`scroll-view.ts` default).
    #[default]
    Auto,
    /// Always visible.
    Always,
    /// Never visible.
    Hidden,
}

impl From<FullscreenScrollbar> for pi_tui::components::scroll_view::Scrollbar {
    fn from(scrollbar: FullscreenScrollbar) -> Self {
        match scrollbar {
            FullscreenScrollbar::Auto => Self::Auto,
            FullscreenScrollbar::Always => Self::Always,
            FullscreenScrollbar::Hidden => Self::Hidden,
        }
    }
}

/// The startup output volume (`quietStartup`,
/// `settings-schema.ts:265-270`): `false` verbose, `true` hide everything,
/// `"header"` keep only the startup header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QuietStartup {
    /// Verbose startup output (the default).
    #[default]
    Off,
    /// Hide all startup output.
    Quiet,
    /// Keep only the startup header.
    Header,
}

impl QuietStartup {
    /// `getQuietStartup` (`settings-manager.ts:1009-1012`): only `true` and
    /// `"header"` survive; anything else — `false` included — is the
    /// default.
    fn from_setting(value: &Value) -> Option<Self> {
        match value {
            Value::Bool(true) => Some(Self::Quiet),
            Value::String(text) if text == "header" => Some(Self::Header),
            _ => None,
        }
    }

    /// The stored JSON form (`settings-schema.ts:265-270`).
    fn to_value(self) -> Value {
        match self {
            Self::Off => Value::Bool(false),
            Self::Quiet => Value::Bool(true),
            Self::Header => Value::String("header".to_string()),
        }
    }
}

/// The fallback when no extension or saved trust decision decides project
/// trust (`defaultProjectTrust`, `settings-schema.ts:271-276`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DefaultProjectTrust {
    /// Ask every time (the default).
    #[default]
    Ask,
    /// Trust without asking.
    Always,
    /// Never trust.
    Never,
}

impl DefaultProjectTrust {
    /// The wire spelling (`settings-manager.ts:1020-1023`).
    pub fn as_name(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Always => "always",
            Self::Never => "never",
        }
    }

    /// `getDefaultProjectTrust` (`settings-manager.ts:1020-1023`): only
    /// `"always"` and `"never"` survive; note the read is **global only**
    /// (`this.globalSettings.defaultProjectTrust`).
    fn from_setting(value: &Value) -> Option<Self> {
        match value.as_str()? {
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

/// The Mermaid rendering mode (`markdown.mermaid`,
/// `settings-schema.ts:135-142`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MermaidRenderingMode {
    /// Render Mermaid code blocks as plain code.
    Off,
    /// Render once the message completes.
    Final,
    /// Render while the message streams (the default).
    #[default]
    Streaming,
}

impl MermaidRenderingMode {
    /// `getMermaidRenderingMode` (`settings-manager.ts:1435-1438`): only
    /// `"off"` and `"final"` survive; `"streaming"` never needs storing — it
    /// is the default.
    fn from_setting(value: &Value) -> Option<Self> {
        match value.as_str()? {
            "off" => Some(Self::Off),
            "final" => Some(Self::Final),
            _ => None,
        }
    }

    /// The wire spelling.
    pub fn as_name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Final => "final",
            Self::Streaming => "streaming",
        }
    }
}

/// The settings store. `SettingsManager` (`settings-manager.ts:378-1533`).
pub struct SettingsManager {
    storage: Arc<dyn SettingsStorage>,
    global_settings: Settings,
    project_settings: Settings,
    settings: Settings,
    project_trusted: bool,
    /// Global fields modified this session (`:387`).
    modified_fields: BTreeSet<String>,
    modified_nested_fields: BTreeMap<String, BTreeSet<String>>,
    /// The project twin (`:389-390`).
    modified_project_fields: BTreeSet<String>,
    modified_project_nested_fields: BTreeMap<String, BTreeSet<String>>,
    /// A corrupt global file is never overwritten (`:739-741`).
    global_settings_load_error: Option<String>,
    project_settings_load_error: Option<String>,
    write_queue: Vec<QueuedWrite>,
    errors: Vec<SettingsError>,
    settings_paths: BTreeMap<SettingsScope, PathBuf>,
}

impl SettingsManager {
    /// `SettingsManager.create(cwd, agentDir, options?)` (`:417-427`).
    /// `agent_dir` is a parameter, never [`crate::config::get_agent_dir`], so
    /// a test can point the whole store at a temp directory.
    pub fn create(
        cwd: &Path,
        agent_dir: &Path,
        options: SettingsManagerCreateOptions,
    ) -> SettingsManager {
        let resolved_cwd = resolve_path(cwd);
        let resolved_agent_dir = resolve_path(agent_dir);
        let storage = Arc::new(FileSettingsStorage::new(&resolved_cwd, &resolved_agent_dir));
        let mut paths = BTreeMap::new();
        paths.insert(
            SettingsScope::Global,
            resolved_agent_dir.join("settings.json"),
        );
        paths.insert(
            SettingsScope::Project,
            resolved_cwd
                .join(crate::config::PROJECT_CONFIG_DIR_NAME)
                .join("settings.json"),
        );
        SettingsManager::from_storage_with_paths(storage, options, paths)
    }

    /// `SettingsManager.fromStorage(storage, options?)` (`:429-433`): no file
    /// paths, so a reported error names the scope instead of a file.
    pub fn from_storage(
        storage: Arc<dyn SettingsStorage>,
        options: SettingsManagerCreateOptions,
    ) -> SettingsManager {
        SettingsManager::from_storage_with_paths(storage, options, BTreeMap::new())
    }

    /// `SettingsManager.inMemory(settings?, options?)` (`:465-469`): seeds the
    /// same JSON round-trip, so an in-memory test exercises the identical load
    /// path.
    pub fn in_memory(
        settings: &Settings,
        options: SettingsManagerCreateOptions,
    ) -> SettingsManager {
        let storage = Arc::new(InMemorySettingsStorage::default());
        let mut initial = settings.clone();
        migrate_settings(&mut initial);
        let serialized = serde_json::to_string_pretty(&Value::Object(initial)).unwrap_or_default();
        let _ = storage.with_lock(SettingsScope::Global, &mut |_current| {
            Ok(Some(serialized.clone()))
        });
        SettingsManager::from_storage(storage, options)
    }

    /// `fromStorageWithPaths` (`:435-463`): both scopes are loaded
    /// independently, and a failure in one queues an error rather than
    /// poisoning the manager.
    fn from_storage_with_paths(
        storage: Arc<dyn SettingsStorage>,
        options: SettingsManagerCreateOptions,
        settings_paths: BTreeMap<SettingsScope, PathBuf>,
    ) -> SettingsManager {
        let project_trusted = options.project_trusted.unwrap_or(true);
        let global_load =
            SettingsManager::try_load_from_storage(&*storage, SettingsScope::Global, true);
        let project_load = SettingsManager::try_load_from_storage(
            &*storage,
            SettingsScope::Project,
            project_trusted,
        );

        let mut errors = Vec::new();
        if let Some(message) = &global_load.error {
            errors.push(SettingsError {
                scope: SettingsScope::Global,
                path: settings_paths.get(&SettingsScope::Global).cloned(),
                message: message.clone(),
            });
        }
        if let Some(message) = &project_load.error {
            errors.push(SettingsError {
                scope: SettingsScope::Project,
                path: settings_paths.get(&SettingsScope::Project).cloned(),
                message: message.clone(),
            });
        }

        let settings = deep_merge_settings(&global_load.settings, &project_load.settings);
        SettingsManager {
            storage,
            global_settings: global_load.settings,
            project_settings: project_load.settings,
            settings,
            project_trusted,
            modified_fields: BTreeSet::new(),
            modified_nested_fields: BTreeMap::new(),
            modified_project_fields: BTreeSet::new(),
            modified_project_nested_fields: BTreeMap::new(),
            global_settings_load_error: global_load.error,
            project_settings_load_error: project_load.error,
            write_queue: Vec::new(),
            errors,
            settings_paths,
        }
    }

    /// `loadFromStorage` (`:471-490`).
    fn load_from_storage(
        storage: &dyn SettingsStorage,
        scope: SettingsScope,
        project_trusted: bool,
    ) -> Result<Settings, String> {
        if scope == SettingsScope::Project && !project_trusted {
            return Ok(Settings::new());
        }
        let mut content: Option<String> = None;
        storage.with_lock(scope, &mut |current| {
            content = current;
            // `undefined` means "do not write", so this is a pure read.
            Ok(None)
        })?;
        let Some(content) = content.filter(|content| !content.is_empty()) else {
            return Ok(Settings::new());
        };
        let parsed: Value =
            serde_json::from_str(strip_bom(&content)).map_err(|error| error.to_string())?;
        let Value::Object(mut settings) = parsed else {
            return Err("settings file is not a JSON object".to_string());
        };
        migrate_settings(&mut settings);
        Ok(settings)
    }

    /// `tryLoadFromStorage` (`:491-498`): a malformed file degrades to `{}`.
    fn try_load_from_storage(
        storage: &dyn SettingsStorage,
        scope: SettingsScope,
        project_trusted: bool,
    ) -> LoadResult {
        match SettingsManager::load_from_storage(storage, scope, project_trusted) {
            Ok(settings) => LoadResult {
                settings,
                error: None,
            },
            Err(error) => LoadResult {
                settings: Settings::new(),
                error: Some(error),
            },
        }
    }

    // -- reads of the merged layers ------------------------------------------

    /// `getSettings` (`:558-560`): a copy of global+project+overrides.
    pub fn get_settings(&self) -> Settings {
        self.settings.clone()
    }

    /// `getGlobalSettings` (`:562-564`).
    pub fn get_global_settings(&self) -> Settings {
        self.global_settings.clone()
    }

    /// `getProjectSettings` (`:566-568`).
    pub fn get_project_settings(&self) -> Settings {
        self.project_settings.clone()
    }

    /// Read one merged setting.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.settings.get(key)
    }

    pub fn is_project_trusted(&self) -> bool {
        self.project_trusted
    }

    /// `setProjectTrusted` (`:577-605`): untrusting drops the project layer
    /// outright; trusting re-reads it and reports a failure as an error.
    pub fn set_project_trusted(&mut self, trusted: bool) {
        if self.project_trusted == trusted {
            return;
        }
        self.project_trusted = trusted;
        self.modified_project_fields.clear();
        self.modified_project_nested_fields.clear();
        if !trusted {
            self.project_settings = Settings::new();
            self.project_settings_load_error = None;
            self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
            return;
        }
        let load =
            SettingsManager::try_load_from_storage(&*self.storage, SettingsScope::Project, true);
        self.project_settings = load.settings;
        if let Some(message) = load.error.clone() {
            self.project_settings_load_error = Some(message.clone());
            self.record_error(SettingsScope::Project, message);
        } else {
            self.project_settings_load_error = None;
        }
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
    }

    /// `reload` (`:607-632`): a resource reload, not a file watch. A layer
    /// whose re-read failed keeps the value it had, and the modified-field
    /// bookkeeping is cleared either way.
    pub fn reload(&mut self) {
        self.flush();
        let global_load =
            SettingsManager::try_load_from_storage(&*self.storage, SettingsScope::Global, true);
        match global_load.error.clone() {
            None => {
                self.global_settings = global_load.settings;
                self.global_settings_load_error = None;
            }
            Some(message) => {
                self.global_settings_load_error = Some(message.clone());
                self.record_error(SettingsScope::Global, message);
            }
        }
        self.modified_fields.clear();
        self.modified_nested_fields.clear();
        self.modified_project_fields.clear();
        self.modified_project_nested_fields.clear();
        let project_load = SettingsManager::try_load_from_storage(
            &*self.storage,
            SettingsScope::Project,
            self.project_trusted,
        );
        match project_load.error.clone() {
            None => {
                self.project_settings = project_load.settings;
                self.project_settings_load_error = None;
            }
            Some(message) => {
                self.project_settings_load_error = Some(message.clone());
                self.record_error(SettingsScope::Project, message);
            }
        }
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
    }

    /// `applyOverrides` (`:636-638`): the fourth rung, runtime only. The next
    /// `save()` recomputes the merge from the two file layers and drops it,
    /// which is what keeps `--use-theme` (`main.ts:679-681`) out of the file.
    pub fn apply_overrides(&mut self, overrides: &Settings) {
        self.settings = deep_merge_settings(&self.settings, overrides);
    }

    // -- write plumbing ------------------------------------------------------

    /// `markModified` (`:641-650`).
    fn mark_modified(&mut self, field: &str, nested_key: Option<&str>) {
        self.modified_fields.insert(field.to_string());
        if let Some(nested_key) = nested_key {
            self.modified_nested_fields
                .entry(field.to_string())
                .or_default()
                .insert(nested_key.to_string());
        }
    }

    /// `markProjectModified` (`:652-661`).
    fn mark_project_modified(&mut self, field: &str) {
        self.modified_project_fields.insert(field.to_string());
    }

    /// `assertProjectTrustedForWrite` (`:664-666`).
    fn assert_project_trusted_for_write(&self) -> Result<(), String> {
        if self.project_trusted {
            Ok(())
        } else {
            Err("Project is not trusted; refusing to write project settings".to_string())
        }
    }

    /// `recordError` (`:667-669`).
    fn record_error(&mut self, scope: SettingsScope, message: String) {
        self.errors.push(SettingsError {
            scope,
            path: self.settings_paths.get(&scope).cloned(),
            message,
        });
    }

    /// `clearModifiedScope` (`:671-681`).
    fn clear_modified_scope(&mut self, scope: SettingsScope) {
        match scope {
            SettingsScope::Global => {
                self.modified_fields.clear();
                self.modified_nested_fields.clear();
            }
            SettingsScope::Project => {
                self.modified_project_fields.clear();
                self.modified_project_nested_fields.clear();
            }
        }
    }

    /// `persistScopedSettings` (`:711-734`): the read-modify-write that makes
    /// concurrent writers safe. Start from the **file**, copy across only the
    /// fields this process marked modified, and for a marked nested key merge
    /// just that key over the file's object — an unmarked sibling written by
    /// another process survives.
    fn persist_scoped_settings(
        storage: &dyn SettingsStorage,
        scope: SettingsScope,
        snapshot: &Settings,
        fields: &BTreeSet<String>,
        nested: &BTreeMap<String, BTreeSet<String>>,
    ) -> Result<(), String> {
        storage.with_lock(scope, &mut |current| {
            let mut file_settings = match current.as_deref().filter(|content| !content.is_empty()) {
                Some(content) => {
                    let parsed: Value = serde_json::from_str(strip_bom(content))
                        .map_err(|error| error.to_string())?;
                    let Value::Object(object) = parsed else {
                        return Err("settings file is not a JSON object".to_string());
                    };
                    let mut object = object;
                    migrate_settings(&mut object);
                    object
                }
                None => Settings::new(),
            };
            for field in fields {
                let value = snapshot.get(field);
                let marked_nested = nested.get(field);
                match (marked_nested, value) {
                    (Some(nested_modified), Some(Value::Object(in_memory_nested))) => {
                        let mut merged_nested = match file_settings.get(field) {
                            Some(Value::Object(base)) => base.clone(),
                            _ => Settings::new(),
                        };
                        for nested_key in nested_modified {
                            match in_memory_nested.get(nested_key) {
                                Some(value) => {
                                    merged_nested.insert(nested_key.clone(), value.clone());
                                }
                                None => {
                                    merged_nested.remove(nested_key);
                                }
                            }
                        }
                        file_settings.insert(field.clone(), Value::Object(merged_nested));
                    }
                    // A field cleared to `undefined` disappears from the file,
                    // exactly as `JSON.stringify` drops it.
                    (_, Some(value)) => {
                        file_settings.insert(field.clone(), value.clone());
                    }
                    (_, None) => {
                        file_settings.remove(field);
                    }
                }
            }
            // `JSON.stringify(settings, null, 2)`: two-space indent, no
            // trailing newline. `serde_json::to_string_pretty` is the same
            // two-space form.
            let serialized = serde_json::to_string_pretty(&Value::Object(file_settings))
                .map_err(|error| error.to_string())?;
            Ok(Some(serialized))
        })
    }

    /// `save` (`:736-751`): re-merge, bail out if the global file failed to
    /// parse, snapshot the modified-field bookkeeping, enqueue.
    fn save(&mut self) {
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
        if self.global_settings_load_error.is_some() {
            return;
        }
        self.write_queue.push(QueuedWrite {
            scope: SettingsScope::Global,
            snapshot: self.global_settings.clone(),
            fields: self.modified_fields.clone(),
            nested: self.modified_nested_fields.clone(),
        });
    }

    /// `saveProjectSettings` (`:752-769`).
    fn save_project_settings(&mut self, settings: Settings) -> Result<(), String> {
        self.assert_project_trusted_for_write()?;
        self.project_settings = settings;
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
        if self.project_settings_load_error.is_some() {
            return Ok(());
        }
        self.write_queue.push(QueuedWrite {
            scope: SettingsScope::Project,
            snapshot: self.project_settings.clone(),
            fields: self.modified_project_fields.clone(),
            nested: BTreeMap::new(),
        });
        Ok(())
    }

    /// `updateProjectSettings` (`:786-794`).
    fn update_project_settings(&mut self, field: &str, value: Value) -> Result<(), String> {
        self.assert_project_trusted_for_write()?;
        let mut project_settings = self.project_settings.clone();
        project_settings.insert(field.to_string(), value);
        self.mark_project_modified(field);
        self.save_project_settings(project_settings)
    }

    /// `flush` (`:777-779`): run the queued writes, in order. A failure becomes
    /// a queued error and leaves the modified-field bookkeeping in place.
    pub fn flush(&mut self) {
        let queue = std::mem::take(&mut self.write_queue);
        for write in queue {
            if write.scope == SettingsScope::Project
                && let Err(message) = self.assert_project_trusted_for_write()
            {
                self.record_error(write.scope, message);
                continue;
            }
            let result = SettingsManager::persist_scoped_settings(
                &*self.storage,
                write.scope,
                &write.snapshot,
                &write.fields,
                &write.nested,
            );
            match result {
                Ok(()) => self.clear_modified_scope(write.scope),
                Err(message) => self.record_error(write.scope, message),
            }
        }
    }

    /// How many writes are waiting for a [`SettingsManager::flush`].
    pub fn pending_writes(&self) -> usize {
        self.write_queue.len()
    }

    /// `drainErrors` (`:781-785`): returns and clears.
    pub fn drain_errors(&mut self) -> Vec<SettingsError> {
        std::mem::take(&mut self.errors)
    }

    /// The file a scope reads and writes: `<agentDir>/settings.json` or
    /// `<cwd>/.pi/settings.json` (`:300-301`).
    pub fn settings_path_for(&self, scope: SettingsScope) -> Option<&Path> {
        self.settings_paths.get(&scope).map(PathBuf::as_path)
    }

    // -- generic setters -----------------------------------------------------

    /// The shape every global setter shares: write into the global layer, mark
    /// the field, save. `Value::Null` removes the field — pi assigns
    /// `undefined`, which `JSON.stringify` then drops.
    fn set_global(&mut self, field: &str, value: Value) {
        if value.is_null() {
            self.global_settings.remove(field);
        } else {
            self.global_settings.insert(field.to_string(), value);
        }
        self.mark_modified(field, None);
        self.save();
    }

    /// The shape every nested global setter shares: ensure the parent object,
    /// write into it, mark the field **and** the nested key, save.
    fn set_global_nested(&mut self, field: &str, nested_key: &str, value: Value) {
        if !matches!(self.global_settings.get(field), Some(Value::Object(_))) {
            self.global_settings
                .insert(field.to_string(), Value::Object(Settings::new()));
        }
        if let Some(Value::Object(parent)) = self.global_settings.get_mut(field) {
            parent.insert(nested_key.to_string(), value);
        }
        self.mark_modified(field, Some(nested_key));
        self.save();
    }

    /// Set a global setting (pi's typed setters all funnel through this
    /// shape).
    pub fn set(&mut self, field: &str, value: Value) {
        self.set_global(field, value);
    }

    /// Set a project setting; refuses when the project is untrusted.
    pub fn set_project(&mut self, field: &str, value: Value) -> Result<(), String> {
        self.update_project_settings(field, value)
    }

    /// A read of a nested object field, e.g. `compaction.enabled`.
    fn nested(&self, field: &str, nested_key: &str) -> Option<&Value> {
        self.settings.get(field)?.as_object()?.get(nested_key)
    }

    // -- model and provider --------------------------------------------------

    /// `getDefaultProvider` (`:802-804`).
    pub fn get_default_provider(&self) -> Option<String> {
        opt_string(self.settings.get("defaultProvider"))
    }

    /// `setDefaultProvider` (`:806`-adjacent setter shape).
    pub fn set_default_provider(&mut self, provider: &str) {
        self.set_global("defaultProvider", Value::String(provider.to_string()));
    }

    /// `getDefaultModel` (`:806-808`).
    pub fn get_default_model(&self) -> Option<String> {
        opt_string(self.settings.get("defaultModel"))
    }

    /// `setDefaultModel` setter shape.
    pub fn set_default_model(&mut self, model_id: &str) {
        self.set_global("defaultModel", Value::String(model_id.to_string()));
    }

    /// `setDefaultModelAndProvider` (`:816-821`): both fields in one write.
    pub fn set_default_model_and_provider(&mut self, provider: &str, model_id: &str) {
        self.set_global("defaultProvider", Value::String(provider.to_string()));
        self.set_global("defaultModel", Value::String(model_id.to_string()));
    }

    /// `getEnabledModels` (`settings-manager.ts:1352-1354`): the scope
    /// patterns `/scoped-models` persists, or `None` when unset. String
    /// entries only — a malformed value degrades to the string list, like
    /// the other array getters.
    pub fn get_enabled_models(&self) -> Option<Vec<String>> {
        let patterns = self.settings.get("enabledModels")?;
        Some(
            patterns
                .as_array()
                .map(|array| {
                    array
                        .iter()
                        .filter(|entry| entry.is_string())
                        .filter_map(|entry| entry.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        )
    }

    /// `setEnabledModels` (`settings-manager.ts:1363-1367`): the global
    /// layer's pattern list; `None` writes `undefined`, which
    /// `JSON.stringify` drops (`set_global`'s null removal).
    pub fn set_enabled_models(&mut self, patterns: Option<Vec<String>>) {
        let value = match patterns {
            Some(patterns) => Value::Array(patterns.into_iter().map(Value::String).collect()),
            None => Value::Null,
        };
        self.set_global("enabledModels", value);
    }

    /// `getDefaultThinkingLevel` (`:867-869`).
    pub fn get_default_thinking_level(&self) -> Option<String> {
        opt_string(self.settings.get("defaultThinkingLevel"))
    }

    /// `setDefaultThinkingLevel` setter shape.
    pub fn set_default_thinking_level(&mut self, level: &str) {
        self.set_global("defaultThinkingLevel", Value::String(level.to_string()));
    }

    // -- compaction ----------------------------------------------------------

    /// `getCompactionEnabled` (`:833-835`).
    pub fn get_compaction_enabled(&self) -> bool {
        bool_or(
            self.nested("compaction", "enabled"),
            DEFAULT_COMPACTION_ENABLED,
        )
    }

    /// `setCompactionEnabled` (`:918-924`).
    pub fn set_compaction_enabled(&mut self, enabled: bool) {
        self.set_global_nested("compaction", "enabled", Value::Bool(enabled));
    }

    /// `getCompactionTokenSetting` (`:887-917`): model override, then the
    /// ordinary setting, then the built-in default — with the two error
    /// messages pi raises for a present-but-wrong value.
    fn compaction_token_setting(
        &self,
        field: &str,
        model: Option<(&str, &str)>,
    ) -> Result<i64, String> {
        let compaction = self.settings.get("compaction").and_then(Value::as_object);
        let ordinary = compaction.and_then(|compaction| compaction.get(field));
        if let Some(ordinary) = ordinary.filter(|value| !value.is_null())
            && !is_non_negative_safe_integer(ordinary)
        {
            return Err(format!(
                "Invalid compaction.{field} setting: {}. Expected a non-negative safe integer.",
                js_value_to_string(ordinary)
            ));
        }
        let model_key = model.map(|(provider, model_id)| format!("{provider}/{model_id}"));
        let entry = match model_key.as_deref() {
            Some(model_key) => compaction
                .and_then(|compaction| compaction.get("modelOverrides"))
                .and_then(Value::as_object)
                .and_then(|overrides| overrides.get(model_key)),
            None => None,
        };
        if let Some(entry) = entry.filter(|entry| !entry.is_null())
            && !is_mergeable_object(Some(entry))
        {
            return Err(format!(
                "Invalid compaction.modelOverrides[\"{}\"] setting: {}. Expected an object.",
                model_key.as_deref().unwrap_or(""),
                js_value_to_string(entry)
            ));
        }
        let override_value = entry
            .and_then(Value::as_object)
            .and_then(|entry| entry.get(field));
        if let Some(override_value) = override_value.filter(|value| !value.is_null())
            && !is_non_negative_safe_integer(override_value)
        {
            return Err(format!(
                "Invalid compaction.modelOverrides[\"{}\"].{field} setting: {}. Expected a non-negative safe integer.",
                model_key.as_deref().unwrap_or(""),
                js_value_to_string(override_value)
            ));
        }
        if let Some(value) = override_value.and_then(Value::as_f64) {
            return Ok(value.floor() as i64);
        }
        if let Some(value) = ordinary.and_then(Value::as_f64) {
            return Ok(value.floor() as i64);
        }
        Ok(match field {
            "reserveTokens" => DEFAULT_COMPACTION_RESERVE_TOKENS,
            "keepRecentTokens" => DEFAULT_COMPACTION_KEEP_RECENT_TOKENS,
            _ => 0,
        })
    }

    /// `getCompactionReserveTokens` (`:919-921`).
    pub fn get_compaction_reserve_tokens(
        &self,
        model: Option<(&str, &str)>,
    ) -> Result<i64, String> {
        self.compaction_token_setting("reserveTokens", model)
    }

    /// `getCompactionKeepRecentTokens` (`:923-925`).
    pub fn get_compaction_keep_recent_tokens(
        &self,
        model: Option<(&str, &str)>,
    ) -> Result<i64, String> {
        self.compaction_token_setting("keepRecentTokens", model)
    }

    /// `getCompactionSettings` (`:883-893`). The token values were validated
    /// as non-negative safe integers, so the narrowing cast cannot truncate.
    pub fn get_compaction_settings(
        &self,
        model: Option<(&str, &str)>,
    ) -> Result<crate::session::compaction::CompactionSettings, String> {
        Ok(crate::session::compaction::CompactionSettings {
            enabled: self.get_compaction_enabled(),
            reserve_tokens: u64::try_from(self.get_compaction_reserve_tokens(model)?)
                .unwrap_or_default(),
            keep_recent_tokens: u64::try_from(self.get_compaction_keep_recent_tokens(model)?)
                .unwrap_or_default(),
        })
    }

    // -- branch summary ------------------------------------------------------

    /// `getBranchSummarySettings` (`:895-901`).
    pub fn get_branch_summary_settings(&self) -> BranchSummarySettings {
        BranchSummarySettings {
            reserve_tokens: self
                .nested("branchSummary", "reserveTokens")
                .and_then(Value::as_f64)
                .map_or(DEFAULT_BRANCH_SUMMARY_RESERVE_TOKENS, |value| {
                    value.floor() as i64
                }),
            skip_prompt: bool_or(
                self.nested("branchSummary", "skipPrompt"),
                DEFAULT_BRANCH_SUMMARY_SKIP_PROMPT,
            ),
        }
    }

    // -- tree ----------------------------------------------------------------

    /// `getTreeFilterMode` (`settings-manager.ts:1379-1383`): an unknown
    /// mode falls back to the default.
    pub fn get_tree_filter_mode(&self) -> TreeFilterMode {
        self.settings
            .get("treeFilterMode")
            .and_then(Value::as_str)
            .and_then(TreeFilterMode::from_name)
            .unwrap_or_default()
    }

    /// `setTreeFilterMode` (`settings-manager.ts:1385-1390`): the global
    /// layer carries the value.
    pub fn set_tree_filter_mode(&mut self, mode: TreeFilterMode) {
        self.set_global("treeFilterMode", Value::String(mode.as_name().to_string()));
    }

    // -- retry ---------------------------------------------------------------

    /// `getRetryEnabled` (`:903-905`).
    pub fn get_retry_enabled(&self) -> bool {
        bool_or(self.nested("retry", "enabled"), DEFAULT_RETRY_ENABLED)
    }

    /// `setRetryEnabled` (`:907-913`).
    pub fn set_retry_enabled(&mut self, enabled: bool) {
        self.set_global_nested("retry", "enabled", Value::Bool(enabled));
    }

    /// `getRetrySettings` (`:915-923`).
    pub fn get_retry_settings(&self) -> RetrySettings {
        let retry = self.settings.get("retry").and_then(Value::as_object);
        let number_at = |key: &str| -> Option<i64> {
            retry
                .and_then(|retry| retry.get(key))
                .and_then(Value::as_f64)
                .map(|value| value.floor() as i64)
        };
        RetrySettings {
            enabled: self.get_retry_enabled(),
            max_retries: number_at("maxRetries").unwrap_or(DEFAULT_RETRY_MAX_RETRIES),
            base_delay_ms: number_at("baseDelayMs").unwrap_or(DEFAULT_RETRY_BASE_DELAY_MS),
            max_agent_delay_ms: number_at("maxAgentDelayMs")
                .unwrap_or(DEFAULT_MAX_AGENT_RETRY_DELAY_MS),
        }
    }

    // -- default tools -------------------------------------------------------

    /// `getDefaultTools` (`:1357-1362`): the resolved `defaultTools`
    /// selection, or `None` when no settings layer sets it. A present-but
    /// malformed value resolves over an empty list, which yields the defaults.
    pub fn get_default_tools(&self) -> Option<Vec<String>> {
        let tools = self.settings.get("defaultTools")?;
        let entries = tools
            .as_array()
            .map(|array| {
                array
                    .iter()
                    .filter(|entry| entry.is_string())
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Some(resolve_default_tools(&entries))
    }

    /// `setDefaultTools` setter shape (`:1364-1367`).
    pub fn set_default_tools(&mut self, tools: Option<Vec<String>>) {
        let value = match tools {
            Some(tools) => Value::Array(tools.into_iter().map(Value::String).collect()),
            None => Value::Null,
        };
        self.set_global("defaultTools", value);
    }

    // -- TUI mode and fullscreen ---------------------------------------------

    /// `getTuiMode` (`settings-manager.js:998-1000`): anything but
    /// `"regular"` resolves to the default, fullscreen — the interface layout
    /// boots on the alt screen unless the user or a flag opts out.
    pub fn get_tui_mode(&self) -> crate::cli::args::TuiMode {
        match self.settings.get("tuiMode").and_then(Value::as_str) {
            Some("regular") => crate::cli::args::TuiMode::Regular,
            _ => crate::cli::args::TuiMode::Fullscreen,
        }
    }

    /// `setTuiMode` setter shape (`settings-manager.js:1002-1004`).
    pub fn set_tui_mode(&mut self, mode: crate::cli::args::TuiMode) {
        let value = match mode {
            crate::cli::args::TuiMode::Regular => "regular",
            crate::cli::args::TuiMode::Fullscreen => "fullscreen",
        };
        self.set_global("tuiMode", Value::String(value.to_string()));
    }

    /// `getFullscreenExitOutput` (`settings-manager.js:1006-1014`): what the
    /// terminal shows when a fullscreen session ends. Anything but
    /// `"resume-hint"` resolves to the default, the transcript repaint.
    pub fn get_fullscreen_exit_output(&self) -> FullscreenExitOutput {
        match self
            .settings
            .get("fullscreenExitOutput")
            .and_then(Value::as_str)
        {
            Some("resume-hint") => FullscreenExitOutput::ResumeHint,
            _ => FullscreenExitOutput::Transcript,
        }
    }

    /// `setFullscreenExitOutput` setter shape (`settings-manager.js:1010-1012`).
    pub fn set_fullscreen_exit_output(&mut self, output: FullscreenExitOutput) {
        let value = match output {
            FullscreenExitOutput::Transcript => "transcript",
            FullscreenExitOutput::ResumeHint => "resume-hint",
        };
        self.set_global("fullscreenExitOutput", Value::String(value.to_string()));
    }

    /// `getFullscreenScrollbar` (`settings-manager.js:1016-1023`): anything
    /// but `"always"` or `"hidden"` resolves to `"auto"`.
    pub fn get_fullscreen_scrollbar(&self) -> FullscreenScrollbar {
        match self
            .settings
            .get("fullscreenScrollbar")
            .and_then(Value::as_str)
        {
            Some("always") => FullscreenScrollbar::Always,
            Some("hidden") => FullscreenScrollbar::Hidden,
            _ => FullscreenScrollbar::Auto,
        }
    }

    /// `setFullscreenScrollbar` setter shape (`settings-manager.js:1018-1022`).
    pub fn set_fullscreen_scrollbar(&mut self, scrollbar: FullscreenScrollbar) {
        let value = match scrollbar {
            FullscreenScrollbar::Auto => "auto",
            FullscreenScrollbar::Always => "always",
            FullscreenScrollbar::Hidden => "hidden",
        };
        self.set_global("fullscreenScrollbar", Value::String(value.to_string()));
    }

    /// `getFullscreenCopyOnSelect` (`settings-manager.js:1025-1031`).
    pub fn get_fullscreen_copy_on_select(&self) -> bool {
        self.settings
            .get("fullscreenCopyOnSelect")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    }

    /// `setFullscreenCopyOnSelect` setter shape (`settings-manager.js:1029`).
    pub fn set_fullscreen_copy_on_select(&mut self, enabled: bool) {
        self.set_global("fullscreenCopyOnSelect", Value::Bool(enabled));
    }

    /// `getFullscreenWheelScrollLines` (`settings-manager.js:1033-1041`): a
    /// finite number clamps to the 1–100 whole lines upstream accepts,
    /// anything else (including `"auto"`) is the accelerated default.
    pub fn get_fullscreen_wheel_scroll_lines(&self) -> pi_tui::wheel_scroll::WheelScrollLines {
        match self
            .settings
            .get("fullscreenWheelScrollLines")
            .and_then(Value::as_f64)
        {
            Some(lines) if lines.is_finite() => {
                pi_tui::wheel_scroll::WheelScrollLines::Fixed(lines.clamp(1.0, 100.0).floor())
            }
            _ => pi_tui::wheel_scroll::WheelScrollLines::Auto,
        }
    }

    /// `setFullscreenWheelScrollLines` setter shape
    /// (`settings-manager.js:1043-1049`).
    pub fn set_fullscreen_wheel_scroll_lines(
        &mut self,
        lines: pi_tui::wheel_scroll::WheelScrollLines,
    ) {
        let value = match lines {
            pi_tui::wheel_scroll::WheelScrollLines::Auto => Value::String("auto".to_string()),
            pi_tui::wheel_scroll::WheelScrollLines::Fixed(count) => {
                Value::from(count.clamp(1.0, 100.0).floor())
            }
        };
        self.set_global("fullscreenWheelScrollLines", value);
    }

    // -- the /settings selector accessors ------------------------------------
    //
    // The `settings-manager.ts:828-1533` getter/setter pairs the
    // `SettingsSelectorComponent` rows read and write, in the selectors'
    // row order. Rows whose machinery is not ported yet persist the flag
    // only; the consumer lands with its subsystem.

    // -- images --------------------------------------------------------------

    /// `getShowImages` (`settings-manager.ts:1208-1210`).
    pub fn get_show_images(&self) -> bool {
        bool_or(
            self.nested("terminal", "showImages"),
            DEFAULT_TERMINAL_SHOW_IMAGES,
        )
    }

    /// `setShowImages` (`settings-manager.ts:1212-1219`).
    pub fn set_show_images(&mut self, show: bool) {
        self.set_global_nested("terminal", "showImages", Value::Bool(show));
    }

    /// `getImageWidthCells` (`settings-manager.ts:1221-1227`): a present
    /// non-number falls back to the default; a number clamps to >= 1.
    pub fn get_image_width_cells(&self) -> i64 {
        self.nested("terminal", "imageWidthCells")
            .and_then(Value::as_f64)
            .filter(|width| width.is_finite())
            .map_or(DEFAULT_TERMINAL_IMAGE_WIDTH_CELLS, |width| {
                width.floor().max(1.0) as i64
            })
    }

    /// `setImageWidthCells` (`settings-manager.ts:1229-1236`).
    pub fn set_image_width_cells(&mut self, width: i64) {
        let clamped = width.max(1);
        self.set_global_nested("terminal", "imageWidthCells", Value::from(clamped));
    }

    /// `getImageAutoResize` (`settings-manager.ts:1326-1328`).
    pub fn get_image_auto_resize(&self) -> bool {
        bool_or(
            self.nested("images", "autoResize"),
            DEFAULT_IMAGES_AUTO_RESIZE,
        )
    }

    /// `setImageAutoResize` (`settings-manager.ts:1330-1337`).
    pub fn set_image_auto_resize(&mut self, enabled: bool) {
        self.set_global_nested("images", "autoResize", Value::Bool(enabled));
    }

    /// `getBlockImages` (`settings-manager.ts:1339-1341`).
    pub fn get_block_images(&self) -> bool {
        bool_or(
            self.nested("images", "blockImages"),
            DEFAULT_IMAGES_BLOCK_IMAGES,
        )
    }

    /// `setBlockImages` (`settings-manager.ts:1343-1350`).
    pub fn set_block_images(&mut self, blocked: bool) {
        self.set_global_nested("images", "blockImages", Value::Bool(blocked));
    }

    // -- skills and transport ------------------------------------------------

    /// `getEnableSkillCommands` (`settings-manager.ts:1184-1186`).
    pub fn get_enable_skill_commands(&self) -> bool {
        bool_or(
            self.settings.get("enableSkillCommands"),
            DEFAULT_ENABLE_SKILL_COMMANDS,
        )
    }

    /// `setEnableSkillCommands` (`settings-manager.ts:1188-1192`).
    pub fn set_enable_skill_commands(&mut self, enabled: bool) {
        self.set_global("enableSkillCommands", Value::Bool(enabled));
    }

    /// `getTransport` (`settings-manager.ts:810-812`): the raw merged value
    /// when set — pi does not validate here — else the default.
    pub fn get_transport(&self) -> String {
        opt_string(self.settings.get("transport")).unwrap_or_else(|| DEFAULT_TRANSPORT.to_string())
    }

    /// `setTransport` (`settings-manager.ts:814-818`).
    pub fn set_transport(&mut self, transport: &str) {
        self.set_global("transport", Value::String(transport.to_string()));
    }

    // -- http and cache ------------------------------------------------------

    /// `getHttpIdleTimeoutMs` (`settings-manager.ts:928-930` via
    /// `parseTimeoutSetting`, `:149-158`): a present-but-unparseable value
    /// throws upstream, so the port reports it as `Err`.
    pub fn get_http_idle_timeout_ms(&self) -> Result<i64, String> {
        match self.settings.get("httpIdleTimeoutMs") {
            None => Ok(DEFAULT_HTTP_IDLE_TIMEOUT_MS),
            Some(value) => parse_http_idle_timeout_ms(value).ok_or_else(|| {
                format!(
                    "Invalid httpIdleTimeoutMs setting: {}",
                    js_value_to_string(value)
                )
            }),
        }
    }

    /// `setHttpIdleTimeoutMs` (`settings-manager.ts:932-939`): a negative
    /// timeout throws upstream, so the port reports it as `Err`.
    pub fn set_http_idle_timeout_ms(&mut self, timeout_ms: i64) -> Result<(), String> {
        if timeout_ms < 0 {
            return Err(format!("Invalid httpIdleTimeoutMs setting: {timeout_ms}"));
        }
        self.set_global("httpIdleTimeoutMs", Value::from(timeout_ms));
        Ok(())
    }

    /// `getCacheWarmingMode` (`settings-manager.ts:941-945`): the **global**
    /// layer only, because warming costs money; an unknown value is the
    /// default.
    pub fn get_cache_warming_mode(&self) -> String {
        self.global_settings
            .get("cacheWarming")
            .and_then(Value::as_str)
            .filter(|mode| CACHE_WARMING_MODES.contains(mode))
            .unwrap_or(DEFAULT_CACHE_WARMING)
            .to_string()
    }

    /// `setCacheWarmingMode` (`settings-manager.ts:947-951`).
    pub fn set_cache_warming_mode(&mut self, mode: &str) {
        self.set_global("cacheWarming", Value::String(mode.to_string()));
    }

    // -- notices, changelog, telemetry ---------------------------------------

    /// `getShowCacheMissNotices` (`settings-manager.ts:970-972`).
    pub fn get_show_cache_miss_notices(&self) -> bool {
        bool_or(
            self.settings.get("showCacheMissNotices"),
            DEFAULT_SHOW_CACHE_MISS_NOTICES,
        )
    }

    /// `setShowCacheMissNotices` (`settings-manager.ts:992-996`).
    pub fn set_show_cache_miss_notices(&mut self, show: bool) {
        self.set_global("showCacheMissNotices", Value::Bool(show));
    }

    /// `getCollapseChangelog` (`settings-manager.ts:1051-1053`).
    pub fn get_collapse_changelog(&self) -> bool {
        bool_or(
            self.settings.get("collapseChangelog"),
            DEFAULT_COLLAPSE_CHANGELOG,
        )
    }

    /// `setCollapseChangelog` (`settings-manager.ts:1055-1059`).
    pub fn set_collapse_changelog(&mut self, collapse: bool) {
        self.set_global("collapseChangelog", Value::Bool(collapse));
    }

    /// `getEnableInstallTelemetry` (`settings-manager.ts:1061-1063`). The
    /// ping itself is not ported yet; the flag persists.
    pub fn get_enable_install_telemetry(&self) -> bool {
        bool_or(
            self.settings.get("enableInstallTelemetry"),
            DEFAULT_ENABLE_INSTALL_TELEMETRY,
        )
    }

    /// `setEnableInstallTelemetry` (`settings-manager.ts:1065-1069`).
    pub fn set_enable_install_telemetry(&mut self, enabled: bool) {
        self.set_global("enableInstallTelemetry", Value::Bool(enabled));
    }

    // -- quiet startup and project trust -------------------------------------

    /// `getQuietStartup` (`settings-manager.ts:1009-1012`).
    pub fn get_quiet_startup(&self) -> QuietStartup {
        self.settings
            .get("quietStartup")
            .and_then(QuietStartup::from_setting)
            .unwrap_or(DEFAULT_QUIET_STARTUP)
    }

    /// `setQuietStartup` (`settings-manager.ts:1014-1018`).
    pub fn set_quiet_startup(&mut self, quiet: QuietStartup) {
        self.set_global("quietStartup", quiet.to_value());
    }

    /// `getDefaultProjectTrust` (`settings-manager.ts:1020-1023`): the
    /// **global** layer only.
    pub fn get_default_project_trust(&self) -> DefaultProjectTrust {
        self.global_settings
            .get("defaultProjectTrust")
            .and_then(DefaultProjectTrust::from_setting)
            .unwrap_or(DEFAULT_DEFAULT_PROJECT_TRUST)
    }

    /// `setDefaultProjectTrust` (`settings-manager.ts:1025-1029`).
    pub fn set_default_project_trust(&mut self, trust: DefaultProjectTrust) {
        self.set_global(
            "defaultProjectTrust",
            Value::String(trust.as_name().to_string()),
        );
    }

    // -- editor, output, terminal ---------------------------------------------

    /// `getDoubleEscapeAction` (`settings-manager.ts:1369-1371`).
    pub fn get_double_escape_action(&self) -> String {
        opt_string(self.settings.get("doubleEscapeAction"))
            .unwrap_or_else(|| DEFAULT_DOUBLE_ESCAPE_ACTION.to_string())
    }

    /// `setDoubleEscapeAction` (`settings-manager.ts:1373-1377`).
    pub fn set_double_escape_action(&mut self, action: &str) {
        self.set_global("doubleEscapeAction", Value::String(action.to_string()));
    }

    /// `getShowHardwareCursor` (`settings-manager.ts:1391-1393`): the
    /// setting, else the `PI_HARDWARE_CURSOR=1` environment opt-in.
    pub fn get_show_hardware_cursor(&self) -> bool {
        self.settings
            .get("showHardwareCursor")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| std::env::var("PI_HARDWARE_CURSOR").is_ok_and(|v| v == "1"))
    }

    /// `setShowHardwareCursor` (`settings-manager.ts:1395-1399`).
    pub fn set_show_hardware_cursor(&mut self, enabled: bool) {
        self.set_global("showHardwareCursor", Value::Bool(enabled));
    }

    /// `getEditorPaddingX` (`settings-manager.ts:1401-1403`).
    pub fn get_editor_padding_x(&self) -> i64 {
        self.settings
            .get("editorPaddingX")
            .and_then(Value::as_i64)
            .unwrap_or(DEFAULT_EDITOR_PADDING_X)
    }

    /// `setEditorPaddingX` (`settings-manager.ts:1405-1409`): clamped to
    /// 0–3.
    pub fn set_editor_padding_x(&mut self, padding: i64) {
        self.set_global("editorPaddingX", Value::from(padding.clamp(0, 3)));
    }

    /// `getOutputPad` (`settings-manager.ts:1411-1413`): anything but a
    /// stored `0` resolves to the default, 1.
    pub fn get_output_pad(&self) -> i64 {
        match self.settings.get("outputPad") {
            Some(value) if value.as_i64() == Some(0) => 0,
            _ => DEFAULT_OUTPUT_PAD,
        }
    }

    /// `setOutputPad` (`settings-manager.ts:1415-1419`).
    pub fn set_output_pad(&mut self, padding: i64) {
        self.set_global("outputPad", Value::from(padding));
    }

    /// `getAutocompleteMaxVisible` (`settings-manager.ts:1421-1423`).
    pub fn get_autocomplete_max_visible(&self) -> i64 {
        self.settings
            .get("autocompleteMaxVisible")
            .and_then(Value::as_i64)
            .unwrap_or(DEFAULT_AUTOCOMPLETE_MAX_VISIBLE)
    }

    /// `setAutocompleteMaxVisible` (`settings-manager.ts:1425-1429`):
    /// clamped to 3–20.
    pub fn set_autocomplete_max_visible(&mut self, max_visible: i64) {
        self.set_global(
            "autocompleteMaxVisible",
            Value::from(max_visible.clamp(3, 20)),
        );
    }

    /// `getClearOnShrink` (`settings-manager.ts:1238-1245`): the setting
    /// first, then the `PI_CLEAR_ON_SHRINK=1` environment opt-in, then the
    /// default.
    pub fn get_clear_on_shrink(&self) -> bool {
        if let Some(value) = self
            .nested("terminal", "clearOnShrink")
            .filter(|v| !v.is_null())
        {
            return js_truthy(value);
        }
        if std::env::var("PI_CLEAR_ON_SHRINK").is_ok_and(|v| v == "1") {
            return true;
        }
        DEFAULT_TERMINAL_CLEAR_ON_SHRINK
    }

    /// `setClearOnShrink` (`settings-manager.ts:1247-1254`).
    pub fn set_clear_on_shrink(&mut self, enabled: bool) {
        self.set_global_nested("terminal", "clearOnShrink", Value::Bool(enabled));
    }

    /// `getShowTerminalProgress` (`settings-manager.ts:1256-1258`).
    pub fn get_show_terminal_progress(&self) -> bool {
        bool_or(
            self.nested("terminal", "showTerminalProgress"),
            DEFAULT_TERMINAL_SHOW_TERMINAL_PROGRESS,
        )
    }

    /// `setShowTerminalProgress` (`settings-manager.ts:1260-1267`).
    pub fn set_show_terminal_progress(&mut self, enabled: bool) {
        self.set_global_nested("terminal", "showTerminalProgress", Value::Bool(enabled));
    }

    // -- markdown and warnings ------------------------------------------------

    /// `getMermaidRenderingMode` (`settings-manager.ts:1435-1438`).
    pub fn get_mermaid_rendering_mode(&self) -> MermaidRenderingMode {
        self.nested("markdown", "mermaid")
            .and_then(MermaidRenderingMode::from_setting)
            .unwrap_or(DEFAULT_MERMAID_RENDERING_MODE)
    }

    /// `setMermaidRenderingMode` (`settings-manager.ts:1440-1445`).
    pub fn set_mermaid_rendering_mode(&mut self, mode: MermaidRenderingMode) {
        self.set_global_nested(
            "markdown",
            "mermaid",
            Value::String(mode.as_name().to_string()),
        );
    }

    /// `getWarnings` (`settings-manager.ts:1447-1449`): the whole
    /// `warnings` object — unknown keys ride along, exactly like the TS
    /// spread.
    pub fn get_warnings(&self) -> Settings {
        self.settings
            .get("warnings")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }

    /// `setWarnings` (`settings-manager.ts:1451-1455`).
    pub fn set_warnings(&mut self, warnings: &Settings) {
        self.set_global("warnings", Value::Object(warnings.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn merge(base: &Value, over: &Value) -> Value {
        let base = base.as_object().unwrap().clone();
        let over = over.as_object().unwrap().clone();
        Value::Object(deep_merge_settings(&base, &over))
    }

    #[test]
    fn deep_merge_replaces_arrays_and_recurses_objects() {
        let merged = merge(
            &json!({"a": {"x": 1, "y": 2}, "list": [1, 2], "keep": true}),
            &json!({"a": {"y": 3}, "list": [3]}),
        );
        assert_eq!(merged["a"], json!({"x": 1, "y": 3}));
        assert_eq!(merged["list"], json!([3]));
        assert_eq!(merged["keep"], json!(true));
    }

    #[test]
    fn default_tools_modifiers_append_onto_the_inherited_list() {
        let merged = merge(
            &json!({"defaultTools": ["read", "bash"]}),
            &json!({"defaultTools": ["-bash", "+grep"]}),
        );
        // Modifier-only lists are appended verbatim onto the inherited one;
        // resolution (the `-bash` removal) happens in `resolveDefaultTools`.
        assert_eq!(
            merged["defaultTools"],
            json!(["read", "bash", "-bash", "+grep"])
        );
        let plain = merge(
            &json!({"defaultTools": ["read", "bash"]}),
            &json!({"defaultTools": ["edit"]}),
        );
        assert_eq!(plain["defaultTools"], json!(["edit"]));
    }

    #[test]
    fn resolve_default_tools_applies_modifiers_in_order() {
        let entries = vec![
            json!("+grep"),
            json!("+read"),
            json!("-read"),
            json!("+edit"),
        ];
        // Modifier-only entries resolve against the defaults, in order.
        assert_eq!(
            resolve_default_tools(&entries),
            vec![
                "bash".to_string(),
                "edit".to_string(),
                "write".to_string(),
                "grep".to_string()
            ]
        );
        // Plain names replace the defaults.
        let plain = vec![json!("write")];
        assert_eq!(resolve_default_tools(&plain), vec!["write".to_string()]);
        // An explicitly empty list disables everything (TS resolves `[]` to
        // `[]`); the defaults only kick in for modifier-only lists.
        assert_eq!(resolve_default_tools(&[]), Vec::<String>::new());
    }

    #[test]
    fn migrate_settings_moves_legacy_keys() {
        let mut settings = json!({
            "queueMode": "all",
            "websockets": true,
            "skills": {"enableSkillCommands": false, "customDirectories": ["/skills"]},
            "retry": {"maxDelayMs": 5000}
        })
        .as_object()
        .unwrap()
        .clone();
        migrate_settings(&mut settings);
        assert_eq!(settings["steeringMode"], json!("all"));
        assert!(settings.get("queueMode").is_none());
        assert_eq!(settings["transport"], json!("websocket"));
        assert_eq!(settings["enableSkillCommands"], json!(false));
        assert_eq!(settings["skills"], json!(["/skills"]));
        assert_eq!(
            settings["retry"]["provider"]["maxRetryDelayMs"],
            json!(5000)
        );
        assert!(settings["retry"].get("maxDelayMs").is_none());
    }

    #[test]
    fn malformed_files_degrade_to_empty_and_queue_an_error() {
        let storage = Arc::new(InMemorySettingsStorage::default());
        let _ = storage.with_lock(SettingsScope::Global, &mut |_| {
            Ok(Some("{not json".to_string()))
        });
        let mut manager =
            SettingsManager::from_storage(storage, SettingsManagerCreateOptions::default());
        assert_eq!(manager.get_settings(), Settings::new());
        let errors = manager.drain_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].scope, SettingsScope::Global);
    }

    #[test]
    fn project_layer_merges_over_global_layer() {
        let storage = Arc::new(InMemorySettingsStorage::default());
        let global = json!({"defaultProvider": "anthropic", "nested": {"a": 1, "b": 2}});
        let _ = storage.with_lock(SettingsScope::Global, &mut |_| {
            Ok(Some(serde_json::to_string_pretty(&global).unwrap()))
        });
        let project = json!({"nested": {"b": 3}});
        let _ = storage.with_lock(SettingsScope::Project, &mut |_| {
            Ok(Some(serde_json::to_string_pretty(&project).unwrap()))
        });
        let manager =
            SettingsManager::from_storage(storage, SettingsManagerCreateOptions::default());
        assert_eq!(manager.get_default_provider().as_deref(), Some("anthropic"));
        assert_eq!(manager.get("nested"), Some(&json!({"a": 1, "b": 3})));
    }

    #[test]
    fn untrusted_project_settings_are_skipped() {
        let storage = Arc::new(InMemorySettingsStorage::default());
        let project = json!({"defaultProvider": "openai"});
        let _ = storage.with_lock(SettingsScope::Project, &mut |_| {
            Ok(Some(serde_json::to_string_pretty(&project).unwrap()))
        });
        let manager = SettingsManager::from_storage(
            storage,
            SettingsManagerCreateOptions {
                project_trusted: Some(false),
            },
        );
        assert!(!manager.is_project_trusted());
        assert_eq!(manager.get_default_provider(), None);
    }

    #[test]
    fn read_modify_write_keeps_the_file_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("settings.json");
        std::fs::write(
            &global,
            serde_json::to_string_pretty(&json!({"theme": "dark", "keep": "me"})).unwrap(),
        )
        .unwrap();
        let mut manager = SettingsManager::create(
            dir.path(),
            dir.path(),
            SettingsManagerCreateOptions::default(),
        );
        manager.set("theme", Value::String("light".to_string()));
        manager.flush();

        let stored: Value =
            serde_json::from_str(&std::fs::read_to_string(&global).unwrap()).unwrap();
        // Only the modified field changes; the unmarked sibling survives.
        assert_eq!(stored["theme"], json!("light"));
        assert_eq!(stored["keep"], json!("me"));
        // Two-space indent, no trailing newline, like `JSON.stringify(_, null, 2)`.
        let raw = std::fs::read_to_string(&global).unwrap();
        assert!(!raw.ends_with('\n'));
        assert!(raw.contains("\n  \"theme\""));
    }

    #[test]
    fn compaction_token_settings_validate_like_the_ts_getter() {
        let storage = Arc::new(InMemorySettingsStorage::default());
        let settings = json!({
            "compaction": {
                "reserveTokens": 1000,
                "modelOverrides": {"anthropic/claude-test": {"keepRecentTokens": 500}}
            }
        });
        let _ = storage.with_lock(SettingsScope::Global, &mut |_| {
            Ok(Some(serde_json::to_string_pretty(&settings).unwrap()))
        });
        let manager =
            SettingsManager::from_storage(storage, SettingsManagerCreateOptions::default());
        assert_eq!(
            manager
                .get_compaction_settings(Some(("anthropic", "claude-test")))
                .unwrap(),
            crate::session::compaction::CompactionSettings {
                enabled: true,
                reserve_tokens: 1000,
                keep_recent_tokens: 500
            }
        );
        assert_eq!(
            manager.get_compaction_settings(None).unwrap(),
            crate::session::compaction::CompactionSettings {
                enabled: true,
                reserve_tokens: 1000,
                keep_recent_tokens: 20_000
            }
        );

        let storage = Arc::new(InMemorySettingsStorage::default());
        let _ = storage.with_lock(SettingsScope::Global, &mut |_| {
            Ok(Some(
                serde_json::to_string_pretty(&json!({"compaction": {"reserveTokens": -5}}))
                    .unwrap(),
            ))
        });
        let manager =
            SettingsManager::from_storage(storage, SettingsManagerCreateOptions::default());
        assert_eq!(
            manager.get_compaction_reserve_tokens(None).unwrap_err(),
            "Invalid compaction.reserveTokens setting: -5. Expected a non-negative safe integer."
        );
    }

    #[test]
    fn branch_summary_and_retry_settings_default_like_pi() {
        let manager =
            SettingsManager::in_memory(&Settings::new(), SettingsManagerCreateOptions::default());
        assert_eq!(
            manager.get_branch_summary_settings(),
            BranchSummarySettings {
                reserve_tokens: DEFAULT_BRANCH_SUMMARY_RESERVE_TOKENS,
                skip_prompt: false
            }
        );
        assert_eq!(
            manager.get_retry_settings(),
            RetrySettings {
                enabled: true,
                max_retries: 3,
                base_delay_ms: 2000,
                max_agent_delay_ms: DEFAULT_MAX_AGENT_RETRY_DELAY_MS
            }
        );
    }

    #[test]
    fn default_tools_resolve_through_the_merge() {
        let mut manager = SettingsManager::in_memory(
            &json!({"defaultTools": ["+grep"]})
                .as_object()
                .unwrap()
                .clone(),
            SettingsManagerCreateOptions::default(),
        );
        assert_eq!(
            manager.get_default_tools(),
            Some(vec![
                "read".to_string(),
                "bash".to_string(),
                "edit".to_string(),
                "write".to_string(),
                "grep".to_string()
            ])
        );
        manager.set_default_tools(Some(vec!["edit".to_string()]));
        manager.flush();
        assert_eq!(manager.get_default_tools(), Some(vec!["edit".to_string()]));
    }

    // -- TUI mode and fullscreen ---------------------------------------------

    fn manager_with_global(value: &Value) -> SettingsManager {
        let storage = Arc::new(InMemorySettingsStorage::default());
        let _ = storage.with_lock(SettingsScope::Global, &mut |_| {
            Ok(Some(serde_json::to_string_pretty(value).unwrap()))
        });
        SettingsManager::from_storage(storage, SettingsManagerCreateOptions::default())
    }

    /// `getTuiMode` (`settings-manager.js:998-1000`): only `"regular"` opts
    /// out of the fullscreen default.
    #[test]
    fn tui_mode_defaults_to_fullscreen_and_accepts_regular() {
        let manager = manager_with_global(&json!({}));
        assert_eq!(
            manager.get_tui_mode(),
            crate::cli::args::TuiMode::Fullscreen
        );
        let manager = manager_with_global(&json!({"tuiMode": "regular"}));
        assert_eq!(manager.get_tui_mode(), crate::cli::args::TuiMode::Regular);
        // A malformed value resolves to the default, not an error.
        let manager = manager_with_global(&json!({"tuiMode": "neither"}));
        assert_eq!(
            manager.get_tui_mode(),
            crate::cli::args::TuiMode::Fullscreen
        );
    }

    /// `getFullscreenExitOutput` / `getFullscreenScrollbar`
    /// (`settings-manager.js:1006-1023`): unknown values fall back to the
    /// defaults.
    #[test]
    fn fullscreen_exit_output_and_scrollbar_validate_like_upstream() {
        let manager = manager_with_global(&json!({}));
        assert_eq!(
            manager.get_fullscreen_exit_output(),
            FullscreenExitOutput::Transcript
        );
        assert_eq!(
            manager.get_fullscreen_scrollbar(),
            FullscreenScrollbar::Auto
        );
        let manager = manager_with_global(&json!({
            "fullscreenExitOutput": "resume-hint",
            "fullscreenScrollbar": "hidden"
        }));
        assert_eq!(
            manager.get_fullscreen_exit_output(),
            FullscreenExitOutput::ResumeHint
        );
        assert_eq!(
            manager.get_fullscreen_scrollbar(),
            FullscreenScrollbar::Hidden
        );
        assert_eq!(
            pi_tui::components::scroll_view::Scrollbar::from(FullscreenScrollbar::Always),
            pi_tui::components::scroll_view::Scrollbar::Always
        );
    }

    /// `getFullscreenCopyOnSelect` / `getFullscreenWheelScrollLines`
    /// (`settings-manager.js:1025-1041`): copy defaults on, wheel lines clamp
    /// to whole lines in 1-100 and `"auto"` wins otherwise.
    #[test]
    fn fullscreen_copy_on_select_and_wheel_lines_resolve_like_upstream() {
        let manager = manager_with_global(&json!({}));
        assert!(manager.get_fullscreen_copy_on_select());
        assert_eq!(
            manager.get_fullscreen_wheel_scroll_lines(),
            pi_tui::wheel_scroll::WheelScrollLines::Auto
        );
        let manager = manager_with_global(&json!({
            "fullscreenCopyOnSelect": false,
            "fullscreenWheelScrollLines": 250.7
        }));
        assert!(!manager.get_fullscreen_copy_on_select());
        assert_eq!(
            manager.get_fullscreen_wheel_scroll_lines(),
            pi_tui::wheel_scroll::WheelScrollLines::Fixed(100.0)
        );
        // A non-number is "auto", not an error.
        let manager = manager_with_global(&json!({"fullscreenWheelScrollLines": "auto"}));
        assert_eq!(
            manager.get_fullscreen_wheel_scroll_lines(),
            pi_tui::wheel_scroll::WheelScrollLines::Auto
        );
    }

    /// The fullscreen setters round-trip through the global layer.
    #[test]
    fn fullscreen_setters_persist_their_values() {
        let mut manager = manager_with_global(&json!({}));
        manager.set_tui_mode(crate::cli::args::TuiMode::Regular);
        manager.set_fullscreen_exit_output(FullscreenExitOutput::ResumeHint);
        manager.set_fullscreen_scrollbar(FullscreenScrollbar::Always);
        manager.set_fullscreen_copy_on_select(false);
        manager
            .set_fullscreen_wheel_scroll_lines(pi_tui::wheel_scroll::WheelScrollLines::Fixed(5.0));
        assert_eq!(manager.get_tui_mode(), crate::cli::args::TuiMode::Regular);
        assert_eq!(
            manager.get_fullscreen_exit_output(),
            FullscreenExitOutput::ResumeHint
        );
        assert_eq!(
            manager.get_fullscreen_scrollbar(),
            FullscreenScrollbar::Always
        );
        assert!(!manager.get_fullscreen_copy_on_select());
        assert_eq!(
            manager.get_fullscreen_wheel_scroll_lines(),
            pi_tui::wheel_scroll::WheelScrollLines::Fixed(5.0)
        );
    }

    /// `getTreeFilterMode` (`settings-manager.ts:1379-1383`): every wire
    /// spelling parses, an unknown one falls back to the default, and the
    /// setter round-trips (`setTreeFilterMode`, `:1385-1390`).
    #[test]
    fn tree_filter_mode_parses_and_round_trips() {
        let manager = manager_with_global(&json!({}));
        assert_eq!(manager.get_tree_filter_mode(), TreeFilterMode::Default);
        for (name, expected) in [
            ("default", TreeFilterMode::Default),
            ("no-tools", TreeFilterMode::NoTools),
            ("user-only", TreeFilterMode::UserOnly),
            ("labeled-only", TreeFilterMode::LabeledOnly),
            ("all", TreeFilterMode::All),
        ] {
            let manager = manager_with_global(&json!({ "treeFilterMode": name }));
            assert_eq!(manager.get_tree_filter_mode(), expected);
        }
        let manager = manager_with_global(&json!({ "treeFilterMode": "bogus" }));
        assert_eq!(manager.get_tree_filter_mode(), TreeFilterMode::Default);

        let mut manager = manager_with_global(&json!({}));
        manager.set_tree_filter_mode(TreeFilterMode::NoTools);
        assert_eq!(manager.get_tree_filter_mode(), TreeFilterMode::NoTools);
    }

    /// `getStatusLabels` (`tree-selector.ts:650-670`): the selector's
    /// `(n/total)`-line suffixes, keyed by mode.
    #[test]
    fn tree_filter_mode_status_labels_match_the_selector() {
        assert_eq!(TreeFilterMode::Default.status_label(), "");
        assert_eq!(TreeFilterMode::NoTools.status_label(), " [no-tools]");
        assert_eq!(TreeFilterMode::UserOnly.status_label(), " [user]");
        assert_eq!(TreeFilterMode::LabeledOnly.status_label(), " [labeled]");
        assert_eq!(TreeFilterMode::All.status_label(), " [all]");
    }

    // -- the /settings selector accessors ------------------------------------

    /// `getShowImages`/`setShowImages` and the image rows' nested settings
    /// keys (`settings-manager.ts:1208-1236`), with a round-trip through the
    /// file.
    #[test]
    fn image_settings_round_trip_through_the_file() {
        let mut manager = manager_with_global(&json!({}));
        assert!(manager.get_show_images());
        assert_eq!(manager.get_image_width_cells(), 60);
        assert!(manager.get_image_auto_resize());
        assert!(!manager.get_block_images());

        manager.set_show_images(false);
        manager.set_image_width_cells(120);
        manager.set_image_auto_resize(false);
        manager.set_block_images(true);
        manager.flush();
        let global = manager.get_global_settings();
        assert_eq!(
            global.get("terminal"),
            Some(&json!({
                "showImages": false,
                "imageWidthCells": 120,
            }))
        );
        assert_eq!(
            global.get("images"),
            Some(&json!({"autoResize": false, "blockImages": true}))
        );

        // The clamps: a width below 1 floors at 1
        // (`settings-manager.ts:1233`).
        manager.set_image_width_cells(0);
        assert_eq!(manager.get_image_width_cells(), 1);
        // A malformed stored value degrades to the default
        // (`settings-manager.ts:1223-1225`).
        let manager = manager_with_global(&json!({ "terminal": { "imageWidthCells": "big" } }));
        assert_eq!(manager.get_image_width_cells(), 60);
    }

    /// `getQuietStartup`/`setQuietStartup` (`settings-manager.ts:1009-1018`):
    /// only `true` and `"header"` survive, and each variant stores its own
    /// JSON form.
    #[test]
    fn quiet_startup_round_trips_the_three_forms() {
        let manager = manager_with_global(&json!({ "quietStartup": true }));
        assert_eq!(manager.get_quiet_startup(), QuietStartup::Quiet);
        let manager = manager_with_global(&json!({ "quietStartup": "header" }));
        assert_eq!(manager.get_quiet_startup(), QuietStartup::Header);
        // `false`, a bogus string, and a number all fall back to the
        // default (`settings-manager.ts:1010-1011`).
        for bogus in [json!(false), json!("no"), json!(3)] {
            let manager = manager_with_global(&json!({ "quietStartup": bogus }));
            assert_eq!(manager.get_quiet_startup(), QuietStartup::Off, "{bogus}");
        }

        let mut manager = manager_with_global(&json!({}));
        manager.set_quiet_startup(QuietStartup::Header);
        assert_eq!(
            manager.get_global_settings().get("quietStartup"),
            Some(&json!("header"))
        );
        manager.set_quiet_startup(QuietStartup::Quiet);
        assert_eq!(
            manager.get_global_settings().get("quietStartup"),
            Some(&json!(true))
        );
        manager.set_quiet_startup(QuietStartup::Off);
        assert_eq!(
            manager.get_global_settings().get("quietStartup"),
            Some(&json!(false))
        );
    }

    /// `getDefaultProjectTrust`/`setDefaultProjectTrust`
    /// (`settings-manager.ts:1020-1029`): the global layer only, and only
    /// `"always"`/`"never"` survive.
    #[test]
    fn default_project_trust_reads_the_global_layer_only() {
        let manager = manager_with_global(&json!({ "defaultProjectTrust": "always" }));
        assert_eq!(
            manager.get_default_project_trust(),
            DefaultProjectTrust::Always
        );
        let manager = manager_with_global(&json!({ "defaultProjectTrust": "bogus" }));
        assert_eq!(
            manager.get_default_project_trust(),
            DefaultProjectTrust::Ask
        );

        let mut manager = manager_with_global(&json!({}));
        manager.set_default_project_trust(DefaultProjectTrust::Never);
        assert_eq!(
            manager.get_global_settings().get("defaultProjectTrust"),
            Some(&json!("never"))
        );
    }

    /// `getHttpIdleTimeoutMs`/`setHttpIdleTimeoutMs`
    /// (`settings-manager.ts:928-939`, `core/http-dispatcher.ts:19-43`):
    /// strings parse (`"disabled"`, numeric), an invalid value errors like
    /// upstream's throw, and the label formatting matches the choices.
    #[test]
    fn http_idle_timeout_parses_validates_and_formats() {
        let manager = manager_with_global(&json!({}));
        assert_eq!(manager.get_http_idle_timeout_ms(), Ok(300_000));
        for (value, expected) in [
            (json!("disabled"), 0),
            (json!("DISABLED"), 0),
            (json!(" 60000 "), 60_000),
            (json!(45_000.5), 45_000),
        ] {
            let manager = manager_with_global(&json!({ "httpIdleTimeoutMs": value }));
            assert_eq!(manager.get_http_idle_timeout_ms(), Ok(expected), "{value}");
        }
        // Unparseable values throw upstream (`settings-manager.ts:154-157`).
        for bogus in [json!("nope"), json!(null), json!(-1)] {
            let manager = manager_with_global(&json!({ "httpIdleTimeoutMs": bogus }));
            assert!(
                manager.get_http_idle_timeout_ms().is_err(),
                "{bogus} should be invalid"
            );
        }
        // The setter rejects a negative timeout like upstream's throw.
        let mut manager = manager_with_global(&json!({}));
        assert!(
            manager
                .set_http_idle_timeout_ms(-1)
                .err()
                .is_some_and(|message| message.contains("Invalid httpIdleTimeoutMs"))
        );
        manager.set_http_idle_timeout_ms(60_000);
        assert_eq!(manager.get_http_idle_timeout_ms(), Ok(60_000));

        assert_eq!(parse_http_idle_timeout_ms(&json!("")), None);
        assert_eq!(format_http_idle_timeout_ms(0), "disabled");
        assert_eq!(format_http_idle_timeout_ms(120_000), "2 min");
    }

    /// `getCacheWarmingMode`/`setCacheWarmingMode`
    /// (`settings-manager.ts:941-951`): the **global** layer only, an
    /// unknown value is the default, and the modes list matches
    /// `CACHE_WARMING_MODES`.
    #[test]
    fn cache_warming_reads_the_global_layer_only() {
        assert_eq!(CACHE_WARMING_MODES, ["off", "streaming", "idle"]);
        let manager = manager_with_global(&json!({ "cacheWarming": "off" }));
        assert_eq!(manager.get_cache_warming_mode(), "off");
        let manager = manager_with_global(&json!({ "cacheWarming": "bogus" }));
        assert_eq!(manager.get_cache_warming_mode(), "idle");

        let mut manager = manager_with_global(&json!({}));
        manager.set_cache_warming_mode("off");
        manager.flush();
        assert_eq!(
            manager.get_global_settings().get("cacheWarming"),
            Some(&json!("off"))
        );
    }

    /// The persist-only selector rows: telemetry, changelog, transport,
    /// notices, skill commands, and the editor/output/terminal values
    /// round-trip through the file with upstream's keys and clamps
    /// (`settings-manager.ts:1051-1069,1184-1192,1369-1429`).
    #[test]
    fn selector_flag_settings_round_trip_through_the_file() {
        let mut manager = manager_with_global(&json!({}));
        manager.set_enable_install_telemetry(false);
        manager.set_collapse_changelog(true);
        manager.set_transport("websocket");
        manager.set_show_cache_miss_notices(true);
        manager.set_enable_skill_commands(false);
        manager.set_double_escape_action("fork");
        manager.set_show_terminal_progress(true);
        manager.set_editor_padding_x(9);
        manager.set_output_pad(0);
        manager.set_autocomplete_max_visible(2);
        manager.flush();
        let global = manager.get_global_settings();
        assert_eq!(global.get("enableInstallTelemetry"), Some(&json!(false)));
        assert_eq!(global.get("collapseChangelog"), Some(&json!(true)));
        assert_eq!(global.get("transport"), Some(&json!("websocket")));
        assert_eq!(global.get("showCacheMissNotices"), Some(&json!(true)));
        assert_eq!(global.get("enableSkillCommands"), Some(&json!(false)));
        assert_eq!(global.get("doubleEscapeAction"), Some(&json!("fork")));
        assert_eq!(global.get("showTerminalProgress"), Some(&json!(true)));
        // The clamps (`settings-manager.ts:1406,1426`).
        assert_eq!(global.get("editorPaddingX"), Some(&json!(3)));
        assert_eq!(global.get("outputPad"), Some(&json!(0)));
        assert_eq!(global.get("autocompleteMaxVisible"), Some(&json!(3)));

        assert_eq!(manager.get_editor_padding_x(), 3);
        assert_eq!(manager.get_output_pad(), 0);
        assert_eq!(manager.get_autocomplete_max_visible(), 3);
        assert_eq!(manager.get_transport(), "websocket");
        assert_eq!(manager.get_double_escape_action(), "fork");

        // An absent `outputPad` resolves to 1 (`settings-manager.ts:1412`).
        let manager = manager_with_global(&json!({}));
        assert_eq!(manager.get_output_pad(), 1);
    }

    /// The selector values survive the storage round trip: set → flush → a
    /// fresh manager over the same storage re-reads them.
    #[test]
    fn selector_values_persist_to_storage_and_reload() {
        let storage = Arc::new(InMemorySettingsStorage::default());
        let mut manager =
            SettingsManager::from_storage(storage.clone(), SettingsManagerCreateOptions::default());
        manager.set_enable_install_telemetry(false);
        manager.set_collapse_changelog(true);
        manager.set_image_width_cells(80);
        manager.set_block_images(true);
        manager.set_quiet_startup(QuietStartup::Header);
        manager.set_editor_padding_x(2);
        manager.flush();

        let reloaded =
            SettingsManager::from_storage(storage, SettingsManagerCreateOptions::default());
        assert!(!reloaded.get_enable_install_telemetry());
        assert!(reloaded.get_collapse_changelog());
        assert_eq!(reloaded.get_image_width_cells(), 80);
        assert!(reloaded.get_block_images());
        assert_eq!(reloaded.get_quiet_startup(), QuietStartup::Header);
        assert_eq!(reloaded.get_editor_padding_x(), 2);
    }

    /// `getWarnings`/`setWarnings` (`settings-manager.ts:1447-1455`): the
    /// whole object round-trips, unknown keys included, and the submenu's
    /// default applies when the key is absent.
    #[test]
    fn warnings_round_trip_unknown_keys_included() {
        let manager = manager_with_global(&json!({}));
        assert_eq!(manager.get_warnings(), Settings::new());
        assert_eq!(
            manager
                .get_warnings()
                .get("anthropicExtraUsage")
                .and_then(Value::as_bool)
                .unwrap_or(DEFAULT_WARNINGS_ANTHROPIC_EXTRA_USAGE),
            true
        );

        let mut manager = manager_with_global(&json!({}));
        let mut warnings = manager.get_warnings();
        warnings.insert("anthropicExtraUsage".to_string(), Value::Bool(false));
        warnings.insert("customKey".to_string(), json!("rides along"));
        manager.set_warnings(&warnings);
        manager.flush();
        let stored = manager.get_global_settings().get("warnings").unwrap();
        assert_eq!(stored.get("anthropicExtraUsage"), Some(&json!(false)));
        assert_eq!(stored.get("customKey"), Some(&json!("rides along")));
    }

    /// `getMermaidRenderingMode`/`setMermaidRenderingMode`
    /// (`settings-manager.ts:1435-1445`): nested under `markdown`, and only
    /// `"off"`/`"final"` are read back — `"streaming"` is the default.
    #[test]
    fn mermaid_mode_nests_under_markdown() {
        let manager = manager_with_global(&json!({}));
        assert_eq!(
            manager.get_mermaid_rendering_mode(),
            MermaidRenderingMode::Streaming
        );
        let manager = manager_with_global(&json!({ "markdown": { "mermaid": "off" } }));
        assert_eq!(
            manager.get_mermaid_rendering_mode(),
            MermaidRenderingMode::Off
        );
        let manager = manager_with_global(&json!({ "markdown": { "mermaid": "bogus" } }));
        assert_eq!(
            manager.get_mermaid_rendering_mode(),
            MermaidRenderingMode::Streaming
        );

        let mut manager = manager_with_global(&json!({}));
        manager.set_mermaid_rendering_mode(MermaidRenderingMode::Final);
        assert_eq!(
            manager.get_global_settings().get("markdown"),
            Some(&json!({"mermaid": "final"}))
        );
    }
}
