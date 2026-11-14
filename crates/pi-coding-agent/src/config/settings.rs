//! pi's settings store: the global + project `settings.json` layers, their
//! merge, and the read-modify-write persistence.
//!
//! Ground truth: `packages/coding-agent/src/core/settings-manager.ts` — the
//! `Settings` shape (`:33-190`), the merge helpers (`:190-254`), the storage
//! backends (`:267-377`), the load path (`:471-498`), and the manager
//! (`:378-1533`). Only the data-layer surface is ported here: scoped access,
//! merging, migration, and persistence. The per-key typed getters that only
//! the interactive UI reads (themes, TUI layout, wheel scrolling, ...) are
//! P6 land and are omitted.
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
        } else if entry.starts_with('-') {
            if let Some(index) = index {
                tools.remove(index);
            }
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
    if !settings.contains_key("transport") {
        if let Some(Value::Bool(websockets)) = settings.get("websockets").cloned() {
            settings.insert(
                "transport".to_string(),
                Value::String(if websockets { "websocket" } else { "sse" }.to_string()),
            );
            settings.remove("websockets");
        }
    }

    // old skills object -> enableSkillCommands + array skills
    if let Some(Value::Object(skills_settings)) = settings.get("skills").cloned() {
        if let Some(enabled) = skills_settings.get("enableSkillCommands").cloned() {
            if !settings.contains_key("enableSkillCommands") {
                settings.insert("enableSkillCommands".to_string(), enabled);
            }
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

/// `getCompactionSettings` (`settings-manager.ts:883-893`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: i64,
    pub keep_recent_tokens: i64,
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
            if write.scope == SettingsScope::Project {
                if let Err(message) = self.assert_project_trusted_for_write() {
                    self.record_error(write.scope, message);
                    continue;
                }
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
        if let Some(ordinary) = ordinary.filter(|value| !value.is_null()) {
            if !is_non_negative_safe_integer(ordinary) {
                return Err(format!(
                    "Invalid compaction.{field} setting: {}. Expected a non-negative safe integer.",
                    js_value_to_string(ordinary)
                ));
            }
        }
        let model_key = model.map(|(provider, model_id)| format!("{provider}/{model_id}"));
        let entry = match model_key.as_deref() {
            Some(model_key) => compaction
                .and_then(|compaction| compaction.get("modelOverrides"))
                .and_then(Value::as_object)
                .and_then(|overrides| overrides.get(model_key)),
            None => None,
        };
        if let Some(entry) = entry.filter(|entry| !entry.is_null()) {
            if !is_mergeable_object(Some(entry)) {
                return Err(format!(
                    "Invalid compaction.modelOverrides[\"{}\"] setting: {}. Expected an object.",
                    model_key.as_deref().unwrap_or(""),
                    js_value_to_string(entry)
                ));
            }
        }
        let override_value = entry
            .and_then(Value::as_object)
            .and_then(|entry| entry.get(field));
        if let Some(override_value) = override_value.filter(|value| !value.is_null()) {
            if !is_non_negative_safe_integer(override_value) {
                return Err(format!(
                    "Invalid compaction.modelOverrides[\"{}\"].{field} setting: {}. Expected a non-negative safe integer.",
                    model_key.as_deref().unwrap_or(""),
                    js_value_to_string(override_value)
                ));
            }
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

    /// `getCompactionSettings` (`:883-893`).
    pub fn get_compaction_settings(
        &self,
        model: Option<(&str, &str)>,
    ) -> Result<CompactionSettings, String> {
        Ok(CompactionSettings {
            enabled: self.get_compaction_enabled(),
            reserve_tokens: self.get_compaction_reserve_tokens(model)?,
            keep_recent_tokens: self.get_compaction_keep_recent_tokens(model)?,
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
            CompactionSettings {
                enabled: true,
                reserve_tokens: 1000,
                keep_recent_tokens: 500
            }
        );
        assert_eq!(
            manager.get_compaction_settings(None).unwrap(),
            CompactionSettings {
                enabled: true,
                reserve_tokens: 1000,
                keep_recent_tokens: DEFAULT_COMPACTION_KEEP_RECENT_TOKENS
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
}
