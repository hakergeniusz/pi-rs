//! Credential storage backed by `auth.json` (`core/auth-storage.ts`).
//!
//! Ground truth: `packages/coding-agent/src/core/auth-storage.ts` (the
//! `AuthStorage`/backend split, `:29-60`, `:61-203`, `:329-492`, and
//! `readStoredCredential`, `:498-508`) plus the value-resolution half of
//! `core/resolve-config-value.ts` that `read()` applies to api keys
//! (`auth-storage.ts:443-449`).
//!
//! File layout (`config.ts:649-651`): `<agentDir>/auth.json` — a JSON object
//! keyed by provider id, whose values are either
//! `{"type":"api_key","key":...,"env":{...}}` or
//! `{"type":"oauth","access":...,"refresh":...,"expires":...}`. The file is
//! created with owner-only permissions (`0o600`, `:37`) inside a `0o700`
//! directory (`:68-73`), written as `JSON.stringify(data, null, 2)`, and
//! guarded by a lock (pi takes `proper-lockfile`; this port creates the
//! `<path>.lock` directory the same library would, with the same 10-attempt /
//! 20 ms retry the sync path uses, `:81-96`).
//!
//! Deviations: the revision-keyed read-state cache and its reload coalescing
//! (`:331-345`, `:403-441`) are an async-IO optimization and are not ported —
//! every operation reads through the lock, which preserves the observable
//! semantics (reads swallow parse failures and report nothing, writes refuse
//! to touch a malformed file). `ReadOnlyAuthStorage` and the abort plumbing
//! are CLI/extension land and follow later.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::get_agent_dir;

/// Object with preserved key insertion order.
pub type JsonObject = Map<String, Value>;

/// `AuthStorageData` (`auth-storage.ts:17`): provider id -> credential object.
pub type AuthStorageData = Map<String, Value>;

/// A stored credential (`@earendil-works/pi-ai`'s `Credential`, as stored in
/// `auth.json`). Unknown fields ride along in the `unknown` bag so a
/// read-modify-write of one provider never drops another process's fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    /// `{"type":"api_key","key":...,"env":{...}}`.
    #[serde(rename = "api_key")]
    ApiKey {
        /// A literal key, a `$VAR`/`${VAR}` template, or a `!command` value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        /// Credential-scoped environment for template resolution
        /// (`auth-storage.ts:265-268`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<JsonObject>,
        #[serde(flatten)]
        unknown: JsonObject,
    },
    /// `{"type":"oauth","access":...,"refresh":...,"expires":...}` (expires is
    /// unix milliseconds).
    #[serde(rename = "oauth")]
    Oauth {
        access: String,
        refresh: String,
        expires: i64,
        #[serde(flatten)]
        unknown: JsonObject,
    },
}

impl Credential {
    /// The `type` discriminant.
    pub fn type_name(&self) -> &'static str {
        match self {
            Credential::ApiKey { .. } => "api_key",
            Credential::Oauth { .. } => "oauth",
        }
    }

    /// Decode a stored credential object, tolerantly: unknown `type` values
    /// and malformed payloads come back as `None`.
    pub fn from_value(value: &Value) -> Option<Credential> {
        serde_json::from_value(value.clone()).ok()
    }
}

/// `CredentialInfo` (`list()`, `:487-491`): metadata without resolving the
/// configured key values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialInfo {
    pub provider_id: String,
    pub credential_type: String,
}

/// Errors the credential store reports.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Filesystem failure, with the action and path for the message.
    #[error("{action} failed for {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The lock could not be acquired after the bounded retries.
    #[error("auth.json is locked: {0}")]
    Locked(PathBuf),
    /// The stored content is not valid JSON.
    #[error("Failed to parse auth.json: {0}")]
    Parse(String),
    /// The mutation callback failed.
    #[error("{0}")]
    Callback(String),
}

/// Result alias for the credential store.
pub type AuthResult<T> = Result<T, AuthError>;

// ---------------------------------------------------------------------------
// Config-value resolution (`core/resolve-config-value.ts`, template half)
// ---------------------------------------------------------------------------

fn is_env_var_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_env_var_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

/// True when the whole string is a valid env var name
/// (`ENV_VAR_NAME_RE`, `resolve-config-value.ts:12`).
fn is_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if is_env_var_start(first) => {}
        _ => return false,
    }
    chars.all(is_env_var_char)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Literal(String),
    Env(String),
}

/// `parseConfigValueTemplate` (`resolve-config-value.ts:29-77`): `$VAR` and
/// `${VAR}` become env references; `$$` and `$!` escape a literal `$`/`!`;
/// anything else is literal.
fn parse_config_value_template(config: &str) -> Vec<Part> {
    let mut parts: Vec<Part> = Vec::new();
    let mut index = 0usize;
    let bytes = config.as_bytes();

    while index < config.len() {
        let Some(dollar) = config[index..].find('$') else {
            append_literal(&mut parts, &config[index..]);
            break;
        };
        let dollar = index + dollar;
        append_literal(&mut parts, &config[index..dollar]);
        // The next char after `$` — careful with multi-byte boundaries: a
        // `$` byte is always ASCII, so the byte after it is a char boundary.
        let next = bytes.get(dollar + 1).copied();
        match next {
            Some(b'$' | b'!') => {
                let ch = next.unwrap() as char;
                append_literal(&mut parts, &ch.to_string());
                index = dollar + 2;
            }
            Some(b'{') => {
                let Some(end) = config[dollar + 2..].find('}') else {
                    append_literal(&mut parts, "$");
                    index = dollar + 1;
                    continue;
                };
                let end = dollar + 2 + end;
                let name = &config[dollar + 2..end];
                if is_env_var_name(name) {
                    parts.push(Part::Env(name.to_string()));
                } else {
                    append_literal(&mut parts, &config[dollar..=end]);
                }
                index = end + 1;
            }
            _ => {
                // The longest `[A-Za-z_][A-Za-z0-9_]*` run after `$`.
                let mut end = dollar + 1;
                let after = &config[dollar + 1..];
                if after.chars().next().is_some_and(is_env_var_start) {
                    end += 1; // skip the first char
                    for ch in after.chars().skip(1) {
                        if is_env_var_char(ch) {
                            end += ch.len_utf8();
                        } else {
                            break;
                        }
                    }
                    parts.push(Part::Env(config[dollar + 1..end].to_string()));
                    index = end;
                    continue;
                }
                append_literal(&mut parts, "$");
                index = dollar + 1;
            }
        }
    }
    parts
}

fn append_literal(parts: &mut Vec<Part>, value: &str) {
    if value.is_empty() {
        return;
    }
    if let Some(Part::Literal(last)) = parts.last_mut() {
        last.push_str(value);
        return;
    }
    parts.push(Part::Literal(value.to_string()));
}

/// `parseConfigValueReference` (`resolve-config-value.ts:80-86`).
fn is_command_config_value(config: &str) -> bool {
    config.starts_with('!')
}

/// `resolveEnvConfigValue` (`resolve-config-value.ts:89-91`): the
/// credential-scoped map wins over the process environment, and an empty
/// string falls through like `||` does.
fn resolve_env_value(name: &str, env: Option<&JsonObject>) -> Option<String> {
    let scoped = env
        .and_then(|env| env.get(name))
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty());
    match scoped {
        Some(value) => Some(value.to_string()),
        None => std::env::var(name).ok().filter(|v| !v.is_empty()),
    }
}

/// `resolveTemplate` (`resolve-config-value.ts:103-118`): `None` when any
/// referenced variable is missing.
fn resolve_template(parts: &[Part], env: Option<&JsonObject>) -> Option<String> {
    let mut resolved = String::new();
    for part in parts {
        match part {
            Part::Literal(value) => resolved.push_str(value),
            Part::Env(name) => {
                let value = resolve_env_value(name, env)?;
                resolved.push_str(&value);
            }
        }
    }
    Some(resolved)
}

/// `getConfigValueEnvVarNames` (`resolve-config-value.ts:126-130`): the env
/// vars a template references, de-duplicated in order.
pub fn get_config_value_env_var_names(config: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for part in parse_config_value_template(config) {
        if let Part::Env(name) = part
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    names
}

/// `getMissingConfigValueEnvVarNames` (`resolve-config-value.ts:132-134`).
pub fn get_missing_config_value_env_var_names(
    config: &str,
    env: Option<&JsonObject>,
) -> Vec<String> {
    get_config_value_env_var_names(config)
        .into_iter()
        .filter(|name| resolve_env_value(name, env).is_none())
        .collect()
}

/// `isConfigValueConfigured` (`resolve-config-value.ts:138-140`).
pub fn is_config_value_configured(config: &str, env: Option<&JsonObject>) -> bool {
    get_missing_config_value_env_var_names(config, env).is_empty()
}

/// Command results are cached for the process lifetime
/// (`commandResultCache`, `resolve-config-value.ts:10`).
fn command_result_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `executeCommand` (`resolve-config-value.ts:202-216`): run the rest of a
/// `!...` value through the shell and use trimmed stdout. Non-zero exits and
/// timeouts (10s) yield `None`.
fn execute_command(command_config: &str) -> Option<String> {
    if let Some(cached) = command_result_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(command_config).cloned())
    {
        return cached;
    }
    let result = execute_command_uncached(command_config);
    if let Ok(mut cache) = command_result_cache().lock() {
        cache.insert(command_config.to_string(), result.clone());
    }
    result
}

fn execute_command_uncached(command_config: &str) -> Option<String> {
    let command = &command_config[1..]; // TS: `commandConfig.slice(1)`
    let mut child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(_) => break None,
        }
    }?;
    if !status.success() {
        return None;
    }
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut pipe, &mut stdout);
    }
    let value = stdout.trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// `resolveConfigValue` (`resolve-config-value.ts:155-166`): `!command`
/// values execute through the shell; otherwise `$ENV_VAR`/`${ENV_VAR}`
/// references interpolate, `$$` and `$!` escape, and the rest is literal.
/// `None` when a referenced variable is missing.
pub fn resolve_config_value(config: &str, env: Option<&JsonObject>) -> Option<String> {
    if is_command_config_value(config) {
        return execute_command(config);
    }
    resolve_template(&parse_config_value_template(config), env)
}

// ---------------------------------------------------------------------------
// Storage backends
// ---------------------------------------------------------------------------

/// One held `<path>.lock` directory. Released on drop.
struct FileLock {
    path: PathBuf,
}

impl FileLock {
    /// `acquireLockSyncWithRetry` (`auth-storage.ts:81-96`): up to 10 attempts
    /// with a 20 ms delay between them.
    fn acquire(path: &Path) -> AuthResult<FileLock> {
        const MAX_ATTEMPTS: usize = 10;
        let lock_path = PathBuf::from(format!("{}.lock", path.to_string_lossy()));
        let mut last_error = None;
        for attempt in 1..=MAX_ATTEMPTS {
            match std::fs::create_dir(&lock_path) {
                Ok(()) => return Ok(FileLock { path: lock_path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_error = Some(error);
                    if attempt != MAX_ATTEMPTS {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
                Err(error) => {
                    return Err(AuthError::Io {
                        action: "lock auth file",
                        path: lock_path,
                        source: error,
                    });
                }
            }
        }
        let _ = last_error;
        Err(AuthError::Locked(lock_path))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.path);
    }
}

/// The write options pi uses (`AUTH_FILE_WRITE_OPTIONS`, `:37`): a new file is
/// owner-only (`0o600`); an existing file's mode is untouched.
fn write_auth_file(path: &Path, content: &str) -> AuthResult<()> {
    use std::io::Write;
    #[cfg(unix)]
    let mut options = {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = std::fs::OpenOptions::new();
        options.mode(0o600);
        options
    };
    #[cfg(not(unix))]
    let mut options = std::fs::OpenOptions::new();
    let mut file = options
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|source| AuthError::Io {
            action: "write auth file",
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(content.as_bytes())
        .map_err(|source| AuthError::Io {
            action: "write auth file",
            path: path.to_path_buf(),
            source,
        })
}

/// `FileAuthStorageBackend.ensureParentDir` (`:68-73`): create the parent with
/// `0o700` where missing.
fn ensure_parent_dir(path: &Path) -> AuthResult<()> {
    let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) else {
        return Ok(());
    };
    if dir.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|source| AuthError::Io {
                action: "create auth dir",
                path: dir.to_path_buf(),
                source,
            })
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir).map_err(|source| AuthError::Io {
            action: "create auth dir",
            path: dir.to_path_buf(),
            source,
        })
    }
}

/// `stripBom` (`utils/text.ts`).
fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{feff}').unwrap_or(content)
}

/// `AuthStorage.parseStorageData` (`:364-369`): an empty file is `{}`.
fn parse_storage_data(content: Option<&str>) -> Result<AuthStorageData, AuthError> {
    let Some(content) = content.filter(|content| !content.is_empty()) else {
        return Ok(AuthStorageData::new());
    };
    let parsed: Value = serde_json::from_str(strip_bom(content))
        .map_err(|error| AuthError::Parse(error.to_string()))?;
    match parsed {
        Value::Object(object) => Ok(object),
        _ => Err(AuthError::Parse("expected an object".to_string())),
    }
}

enum Backend {
    File(PathBuf),
    Memory(Arc<Mutex<Option<String>>>),
}

/// Credential storage backed by a JSON file (`AuthStorage`, `:329-492`).
pub struct AuthStorage {
    backend: Backend,
}

impl AuthStorage {
    /// `AuthStorage.create(authPath?)` (`:349-352`): file-backed storage.
    /// Defaults to [`AuthStorage::default_path`], `<agentDir>/auth.json`. The
    /// constructor's `reload()` creates the backing file (`ensureFileExists`,
    /// `:75-79`) with owner-only permissions, and read failures degrade to an
    /// empty snapshot, so creation errors are swallowed here too.
    pub fn create(auth_path: &Path) -> AuthStorage {
        let storage = AuthStorage {
            backend: Backend::File(auth_path.to_path_buf()),
        };
        let _ = storage.read_latest_data();
        storage
    }

    /// The default file path: `getAgentDir()/auth.json` (`config.ts:649-651`).
    pub fn default_path() -> PathBuf {
        get_agent_dir().join("auth.json")
    }

    /// `AuthStorage.inMemory(data?)` (`:358-362`): seeds the same JSON
    /// round-trip, so an in-memory test exercises the identical load path.
    pub fn in_memory(data: &AuthStorageData) -> AuthStorage {
        let backend = Arc::new(Mutex::new(Some(
            serde_json::to_string_pretty(&Value::Object(data.clone())).unwrap_or_default(),
        )));
        AuthStorage {
            backend: Backend::Memory(backend),
        }
    }

    fn read_content(&self) -> AuthResult<Option<String>> {
        match &self.backend {
            Backend::File(path) => match std::fs::read_to_string(path) {
                Ok(content) => Ok(Some(content)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(source) => Err(AuthError::Io {
                    action: "read auth file",
                    path: path.clone(),
                    source,
                }),
            },
            Backend::Memory(slot) => Ok(slot.lock().expect("in-memory auth").clone()),
        }
    }

    /// Lock the backing store, hand the current content to `action`, and write
    /// back what it returns instead of `None` (`withLock`, `:98-116`).
    fn with_lock<R>(
        &self,
        action: impl FnOnce(Option<String>) -> AuthResult<(R, Option<String>)>,
    ) -> AuthResult<R> {
        if let Backend::File(path) = &self.backend {
            ensure_parent_dir(path)?;
            ensure_file_exists(path)?;
        }
        let _guard = match &self.backend {
            Backend::File(path) => Some(FileLock::acquire(path)?),
            Backend::Memory(..) => None,
        };
        let current = self.read_content()?;
        let (result, next) = action(current)?;
        if let Some(next) = next {
            match &self.backend {
                Backend::File(path) => write_auth_file(path, &next)?,
                Backend::Memory(slot) => *slot.lock().expect("in-memory auth") = Some(next),
            }
        }
        Ok(result)
    }

    /// `read` (`:443-449`): the stored credential for one provider, with an
    /// api key resolved through its `$VAR` template or `!command` value. A
    /// malformed or unreadable file yields `None`, matching pi's
    /// fall-back-to-last-good-snapshot behavior.
    pub fn read(&self, provider: &str) -> AuthResult<Option<Credential>> {
        let data = self.read_latest_data()?;
        let credential = data.get(provider).and_then(Credential::from_value);
        Ok(credential.map(resolve_credential_key))
    }

    /// `readLatestData` (`:403-441`), without the cache: file read and parse
    /// failures degrade to an empty map, as the `.catch(() =>
    /// this.readState.data)` chain does.
    fn read_latest_data(&self) -> AuthResult<AuthStorageData> {
        self.with_lock(|current| {
            let data = parse_storage_data(current.as_deref()).unwrap_or_default();
            Ok((data, None))
        })
    }

    /// `list` (`:487-491`): credential metadata without resolving configured
    /// key values.
    pub fn list(&self) -> AuthResult<Vec<CredentialInfo>> {
        let data = self.read_latest_data()?;
        Ok(data
            .iter()
            .filter_map(|(provider_id, credential)| {
                let value = Credential::from_value(credential)?;
                Some(CredentialInfo {
                    provider_id: provider_id.clone(),
                    credential_type: value.type_name().to_string(),
                })
            })
            .collect())
    }

    /// `modify` (`:451-473`): run `fn` against the current credential under
    /// the lock. Returning `None` leaves the current credential unchanged and
    /// returns it; returning a credential replaces it and persists the merged
    /// map (`{ ...currentData, [provider]: next }`). A malformed file makes
    /// this fail **without writing** (`:647-653` in the TS test).
    pub fn modify(
        &self,
        provider: &str,
        f: impl FnOnce(Option<Credential>) -> Result<Option<Credential>, String>,
    ) -> AuthResult<Option<Credential>> {
        self.with_lock(|current| {
            let current_data = parse_storage_data(current.as_deref())?;
            let current_credential = current_data.get(provider).and_then(Credential::from_value);
            let next = f(current_credential.clone()).map_err(AuthError::Callback)?;
            match next {
                None => Ok((current_credential, None)),
                Some(next) => {
                    let mut merged = current_data.clone();
                    merged.insert(
                        provider.to_string(),
                        serde_json::to_value(&next).unwrap_or(Value::Null),
                    );
                    let serialized = serde_json::to_string_pretty(&Value::Object(merged))
                        .map_err(|error| AuthError::Parse(error.to_string()))?;
                    Ok((Some(next), Some(serialized)))
                }
            }
        })
    }

    /// `delete` (`:475-484`): remove one credential while preserving others —
    /// and the file's key order, which `Map::remove` (a swap-remove) would
    /// scramble, so the map is rebuilt like JS `delete` does.
    pub fn delete(&self, provider: &str) -> AuthResult<()> {
        self.with_lock(|current| {
            let current_data = parse_storage_data(current.as_deref())?;
            let mut kept = AuthStorageData::new();
            for (existing, credential) in current_data {
                if existing != provider {
                    kept.insert(existing, credential);
                }
            }
            let serialized = serde_json::to_string_pretty(&Value::Object(kept))
                .map_err(|error| AuthError::Parse(error.to_string()))?;
            Ok(((), Some(serialized)))
        })
    }
}

/// `ensureFileExists` (`:75-79`): create the file with `{}` when missing.
fn ensure_file_exists(path: &Path) -> AuthResult<()> {
    if path.exists() {
        return Ok(());
    }
    write_auth_file(path, "{}")
}

/// `read`'s resolution step (`:443-449`): `{ ...credential, key:
/// resolveConfigValue(credential.key, credential.env) }` for api keys.
fn resolve_credential_key(credential: Credential) -> Credential {
    let Credential::ApiKey {
        key: Some(key),
        env,
        unknown,
    } = credential
    else {
        return credential;
    };
    let resolved = resolve_config_value(&key, env.as_ref());
    Credential::ApiKey {
        key: resolved,
        env,
        unknown,
    }
}

/// `readStoredCredential` (`:498-508`): one-off synchronous read of a stored
/// credential from an auth.json file, without instantiating a store or
/// resolving configured key values.
pub fn read_stored_credential(provider_id: &str, auth_path: &Path) -> Option<Credential> {
    let content = std::fs::read_to_string(auth_path).ok()?;
    let data = parse_storage_data(Some(&content)).ok()?;
    data.get(provider_id).and_then(Credential::from_value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn auth_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("auth.json")
    }

    fn write_auth_json(path: &Path, data: &Value) {
        std::fs::write(path, serde_json::to_string(data).unwrap()).unwrap();
    }

    #[test]
    fn reads_and_resolves_stored_api_key_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = auth_path(&dir);
        write_auth_json(
            &path,
            &json!({"anthropic": {"type": "api_key", "key": "$PI_TEST_AUTH_KEY"}}),
        );

        // The referenced var is unset here, so the key resolves to nothing —
        // TS: `{ ...credential, key: undefined }`.
        let storage = AuthStorage::create(&path);
        assert_eq!(
            storage.read("anthropic").unwrap(),
            Some(Credential::ApiKey {
                key: None,
                env: None,
                unknown: JsonObject::new()
            })
        );

        write_auth_json(
            &path,
            &json!({
                "anthropic": {
                    "type": "api_key",
                    "key": "$PI_TEST_AUTH_KEY",
                    "env": {"PI_TEST_AUTH_KEY": "scoped-value"}
                }
            }),
        );
        let credential = storage.read("anthropic").unwrap().unwrap();
        match &credential {
            Credential::ApiKey { key, env, .. } => {
                assert_eq!(key.as_deref(), Some("scoped-value"));
                assert_eq!(env.as_ref().map(JsonObject::len), Some(1));
            }
            Credential::Oauth { .. } => panic!("expected api key credential, got {credential:?}"),
        }
    }

    #[test]
    fn resolves_command_backed_api_key_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = auth_path(&dir);
        write_auth_json(
            &path,
            &json!({"anthropic": {"type": "api_key", "key": "!printf 'command-key'"}}),
        );
        let storage = AuthStorage::create(&path);
        let credential = storage.read("anthropic").unwrap().unwrap();
        assert_eq!(
            credential,
            Credential::ApiKey {
                key: Some("command-key".to_string()),
                env: None,
                unknown: JsonObject::new()
            }
        );
    }

    #[test]
    fn returns_oauth_credentials_unchanged() {
        let credential = Credential::Oauth {
            access: "access-token".to_string(),
            refresh: "refresh-token".to_string(),
            expires: 1_700_000_000_000,
            unknown: JsonObject::new(),
        };
        let storage = AuthStorage::in_memory(
            &json!({"anthropic": serde_json::to_value(&credential).unwrap()})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(storage.read("anthropic").unwrap(), Some(credential));
    }

    #[test]
    fn creates_new_auth_files_with_owner_only_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = auth_path(&dir);
        let _storage = AuthStorage::create(&path);
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let dir_mode = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
            // The tempdir itself already exists, so the mode is untouched.
            assert!(dir_mode > 0);
        }
    }

    #[test]
    fn preserves_the_mode_of_an_existing_auth_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = auth_path(&dir);
        write_auth_json(
            &path,
            &json!({"anthropic": {"type": "api_key", "key": "old"}}),
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660)).unwrap();
        }
        let storage = AuthStorage::create(&path);
        storage
            .modify("anthropic", |_| {
                Ok(Some(Credential::ApiKey {
                    key: Some("new".to_string()),
                    env: None,
                    unknown: JsonObject::new(),
                }))
            })
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o660);
        }
    }

    #[test]
    fn modify_persists_while_preserving_unrelated_external_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = auth_path(&dir);
        write_auth_json(
            &path,
            &json!({"anthropic": {"type": "api_key", "key": "old"}}),
        );
        let storage = AuthStorage::create(&path);
        write_auth_json(
            &path,
            &json!({
                "anthropic": {"type": "api_key", "key": "old"},
                "openai": {"type": "api_key", "key": "external"}
            }),
        );
        storage
            .modify("anthropic", |_| {
                Ok(Some(Credential::ApiKey {
                    key: Some("new".to_string()),
                    env: None,
                    unknown: JsonObject::new(),
                }))
            })
            .unwrap();
        let stored: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            stored,
            json!({
                "anthropic": {"type": "api_key", "key": "new"},
                "openai": {"type": "api_key", "key": "external"}
            })
        );
    }

    #[test]
    fn modify_with_none_leaves_the_current_credential_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = auth_path(&dir);
        write_auth_json(
            &path,
            &json!({"anthropic": {"type": "api_key", "key": "stored"}}),
        );
        let storage = AuthStorage::create(&path);
        let current = storage.modify("anthropic", Ok).unwrap().unwrap();
        assert_eq!(
            current,
            Credential::ApiKey {
                key: Some("stored".to_string()),
                env: None,
                unknown: JsonObject::new()
            }
        );
        assert_eq!(
            storage.read("anthropic").unwrap(),
            Some(Credential::ApiKey {
                key: Some("stored".to_string()),
                env: None,
                unknown: JsonObject::new()
            })
        );
    }

    #[test]
    fn delete_removes_one_credential_while_preserving_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = auth_path(&dir);
        write_auth_json(
            &path,
            &json!({
                "anthropic": {"type": "api_key", "key": "anthropic-key"},
                "openai": {"type": "api_key", "key": "openai-key"}
            }),
        );
        let storage = AuthStorage::create(&path);
        write_auth_json(
            &path,
            &json!({
                "anthropic": {"type": "api_key", "key": "anthropic-key"},
                "openai": {"type": "api_key", "key": "openai-key"},
                "google": {"type": "api_key", "key": "external-key"}
            }),
        );
        storage.delete("anthropic").unwrap();
        let listed = storage.list().unwrap();
        assert_eq!(
            listed,
            vec![
                CredentialInfo {
                    provider_id: "openai".to_string(),
                    credential_type: "api_key".to_string()
                },
                CredentialInfo {
                    provider_id: "google".to_string(),
                    credential_type: "api_key".to_string()
                },
            ]
        );
        assert_eq!(storage.read("anthropic").unwrap(), None);
        assert_eq!(
            storage.read("openai").unwrap(),
            Some(Credential::ApiKey {
                key: Some("openai-key".to_string()),
                env: None,
                unknown: JsonObject::new()
            })
        );
    }

    #[test]
    fn in_memory_storage_implements_the_same_behavior() {
        let storage = AuthStorage::in_memory(
            &json!({"anthropic": {"type": "api_key", "key": "initial"}})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(
            storage.read("anthropic").unwrap(),
            Some(Credential::ApiKey {
                key: Some("initial".to_string()),
                env: None,
                unknown: JsonObject::new()
            })
        );
        storage
            .modify("anthropic", |_| {
                Ok(Some(Credential::ApiKey {
                    key: Some("updated".to_string()),
                    env: None,
                    unknown: JsonObject::new(),
                }))
            })
            .unwrap();
        assert_eq!(
            storage.read("anthropic").unwrap(),
            Some(Credential::ApiKey {
                key: Some("updated".to_string()),
                env: None,
                unknown: JsonObject::new()
            })
        );
        storage.delete("anthropic").unwrap();
        assert_eq!(storage.list().unwrap(), Vec::<CredentialInfo>::new());
    }

    #[test]
    fn does_not_overwrite_malformed_auth_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = auth_path(&dir);
        write_auth_json(
            &path,
            &json!({"anthropic": {"type": "api_key", "key": "stored"}}),
        );
        std::fs::write(&path, "{invalid-json").unwrap();
        let storage = AuthStorage::create(&path);
        let result = storage.modify("openai", |_| {
            Ok(Some(Credential::ApiKey {
                key: Some("new".to_string()),
                env: None,
                unknown: JsonObject::new(),
            }))
        });
        assert!(result.is_err());
        // The malformed content is untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{invalid-json");
        // Reads degrade to "no credential", like the last-good snapshot does.
        assert_eq!(storage.read("anthropic").unwrap(), None);
    }

    #[test]
    fn reads_a_stored_credential_without_a_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = auth_path(&dir);
        write_auth_json(
            &path,
            &json!({"anthropic": {"type": "api_key", "key": "$VAR"}}),
        );
        let credential = read_stored_credential("anthropic", &path).unwrap();
        assert_eq!(credential.type_name(), "api_key");
        // Unresolved: the raw template stays visible.
        match &credential {
            Credential::ApiKey { key, .. } => assert_eq!(key.as_deref(), Some("$VAR")),
            Credential::Oauth { .. } => panic!("unexpected credential {credential:?}"),
        }
        assert_eq!(read_stored_credential("openai", &path), None);
        assert_eq!(
            read_stored_credential("anthropic", &dir.path().join("missing.json")),
            None
        );
    }

    #[test]
    fn template_resolution_matches_the_ts_parser() {
        let env = json!({"PI_TEST_NAME": "value"})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            resolve_config_value("literal", None).as_deref(),
            Some("literal")
        );
        assert_eq!(
            resolve_config_value("$PI_TEST_NAME", Some(&env)).as_deref(),
            Some("value")
        );
        assert_eq!(
            resolve_config_value("${PI_TEST_NAME}", Some(&env)).as_deref(),
            Some("value")
        );
        assert_eq!(
            resolve_config_value("pre-$PI_TEST_NAME-post", Some(&env)).as_deref(),
            Some("pre-value-post")
        );
        assert_eq!(
            resolve_config_value("$$PI_TEST_NAME", Some(&env)).as_deref(),
            Some("$PI_TEST_NAME")
        );
        assert_eq!(
            resolve_config_value("$!PI_TEST_NAME", Some(&env)).as_deref(),
            Some("!PI_TEST_NAME")
        );
        assert_eq!(resolve_config_value("${PI_TEST_MISSING}", Some(&env)), None);
        assert_eq!(resolve_config_value("$PI_TEST_MISSING", Some(&env)), None);
        // Scoped env wins over the process env; empty scoped values fall through.
        let empty = json!({"PI_TEST_NAME": ""}).as_object().unwrap().clone();
        assert_eq!(
            resolve_config_value("$PI_TEST_NAME", Some(&empty)),
            std::env::var("PI_TEST_NAME").ok().filter(|v| !v.is_empty())
        );
        // Not a valid env var name: the `$` is literal.
        assert_eq!(
            resolve_config_value("$1NAME", Some(&env)).as_deref(),
            Some("$1NAME")
        );
        assert_eq!(
            resolve_config_value("${1NAME}", Some(&env)).as_deref(),
            Some("${1NAME}")
        );
        // Unterminated `${`: literal up to the `$`.
        assert_eq!(
            resolve_config_value("${PI_TEST_NAME", Some(&env)).as_deref(),
            Some("${PI_TEST_NAME")
        );
        // Scoped env values that are not strings do not resolve.
        let weird = json!({"PI_TEST_NAME": 5}).as_object().unwrap().clone();
        assert_eq!(resolve_config_value("$PI_TEST_NAME", Some(&weird)), None);
    }

    #[test]
    fn env_var_name_helpers_report_reference_coverage() {
        let env = json!({"PI_TEST_A": "a", "PI_TEST_B": "b"})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            get_config_value_env_var_names("$PI_TEST_A-$PI_TEST_B-$PI_TEST_A"),
            vec!["PI_TEST_A".to_string(), "PI_TEST_B".to_string()]
        );
        assert_eq!(
            get_missing_config_value_env_var_names("$PI_TEST_A-$PI_TEST_B", Some(&env)),
            Vec::<String>::new()
        );
        assert_eq!(
            get_missing_config_value_env_var_names("$PI_TEST_A-$PI_TEST_C", Some(&env)),
            vec!["PI_TEST_C".to_string()]
        );
        assert!(is_config_value_configured("$PI_TEST_A", Some(&env)));
        assert!(!is_config_value_configured("$PI_TEST_C", Some(&env)));
    }

    #[test]
    fn credential_serialization_matches_the_wire_shape() {
        let api = Credential::ApiKey {
            key: Some("k".to_string()),
            env: None,
            unknown: JsonObject::new(),
        };
        assert_eq!(
            serde_json::to_value(&api).unwrap(),
            json!({"type": "api_key", "key": "k"})
        );
        let oauth = Credential::Oauth {
            access: "a".to_string(),
            refresh: "r".to_string(),
            expires: 5,
            unknown: JsonObject::new(),
        };
        assert_eq!(
            serde_json::to_value(&oauth).unwrap(),
            json!({"type": "oauth", "access": "a", "refresh": "r", "expires": 5})
        );
        // Unknown fields round-trip through the bag.
        let raw = json!({"type": "api_key", "key": "k", "extra": true});
        let decoded = Credential::from_value(&raw).unwrap();
        let Credential::ApiKey { unknown, .. } = &decoded else {
            panic!()
        };
        assert_eq!(unknown.get("extra"), Some(&json!(true)));
        assert_eq!(serde_json::to_value(&decoded).unwrap(), raw);
    }
}
