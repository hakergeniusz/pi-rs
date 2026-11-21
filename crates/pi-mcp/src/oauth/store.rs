//! File-backed credentials for MCP OAuth servers: pi's `mcp-auth.json`.
//!
//! Ground truth, read in full before porting:
//! * `packages/coding-agent/src/extensions/mcp/oauth.ts:44-157` — the stored
//!   state map, its key format (server name **and** URL, plus the legacy
//!   URL-only key), `McpOAuthCredentialStore::for_server`, `tokens`, `remove`,
//!   and the per-server refresh lock.
//! * `packages/coding-agent/src/core/auth-storage.ts:36-116` — the
//!   `FileAuthStorageBackend` that `withLock` creates, locks and rewrites the
//!   file with (mode `0600`, parent dir `0700`, `{}` for a missing file).
//! * `packages/coding-agent/test/mcp-oauth-store.test.ts:1-55` — the upstream
//!   tests this module mirrors (cross-server isolation, legacy takeover,
//!   sign-out).
//! * `packages/mcp/src/oauth/provider.ts:10-25` — the `McpOAuthState` shape,
//!   reused from [`super::provider::McpOAuthState`] instead of redeclared.
//!
//! The store implements [`McpOAuthStateStore`] and [`McpOAuthServerStore`], so
//! a [`ServerOAuthStore`] from [`McpOAuthCredentialStore::for_server`] plugs
//! straight into [`super::auth_provider::McpAuthProvider`] and
//! [`super::client_settings::create_oauth_client_provider`].
//!
//! Deviations from upstream, all deliberate:
//! * The default path is the pi-rs XDG config root (`~/.config/pi-rs`, or
//!   `$XDG_CONFIG_HOME/pi-rs`, or `$PI_CODING_AGENT_DIR`) instead of `~/.pi`.
//!   The file name and its bytes are unchanged.
//! * `proper-lockfile`'s lock directory is reproduced with `create_dir` on a
//!   `<target>.lock` sibling plus a `lockfile.json` nonce, with the same stale
//!   (20 s) / wait (25 s) / retry (100 ms) numbers for the refresh lock and
//!   the backend lock's 10 s stale / ~200 ms wait. A lock held past the wait
//!   fails with upstream's exact `McpRefreshLockBusyError` text for refreshes.
//! * Writes go through a temp file plus `rename` while holding the lock, so a
//!   killed process cannot leave half a JSON document; the bytes written are
//!   `JSON.stringify(states, null, 2)\n` exactly (`serde_json` is built with
//!   `preserve_order`, matching JavaScript object insertion order).
//! * A malformed JSON file reports `Failed to read mcp-auth.json: …` (the
//!   wording of `auth-storage.ts:224`, adapted to this file name) because
//!   upstream simply lets V8's engine-specific `SyntaxError` text escape.
//!   A missing, empty, or non-object file is still treated as `{}` with no
//!   error, exactly like upstream's `parseStates`. Stored entries are
//!   deserialized into the typed [`McpOAuthState`] rather than blindly cast;
//!   a structurally invalid entry reports `Invalid mcp-auth.json state: …`.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::error::McpError;
use crate::oauth::client_settings::McpOAuthServerStore;
use crate::oauth::errors::OAuthError;
use crate::oauth::provider::{McpOAuthState, McpOAuthStateStore};
use crate::oauth::types::OAuthTokens;

type Result<T> = std::result::Result<T, McpError>;

/// The credential file's name (`oauth.ts:72`).
pub const MCP_AUTH_FILE_NAME: &str = "mcp-auth.json";
/// `$PI_CODING_AGENT_DIR`, the pi-rs override of `getAgentDir`.
const ENV_AGENT_DIR: &str = "PI_CODING_AGENT_DIR";
/// The pi-rs leaf under the XDG config root.
const CONFIG_DIR_NAME: &str = "pi-rs";

/// A refresh lock whose holder stopped renewing it is taken over after this
/// long (`REFRESH_LOCK_STALE_MS`, `oauth.ts:38`).
const REFRESH_LOCK_STALE: Duration = Duration::from_secs(20);
/// How long to wait for another process's refresh: longer than a stale lock
/// lives (`REFRESH_LOCK_WAIT_MS`, `oauth.ts:40`).
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(25);
/// Retry interval of the refresh lock (`REFRESH_LOCK_RETRY_MS`, `oauth.ts:42`).
const REFRESH_LOCK_RETRY: Duration = Duration::from_millis(100);
/// `proper-lockfile`'s default stale time for the backend's `lockSync`.
const BACKEND_LOCK_STALE: Duration = Duration::from_secs(10);
/// The backend's 10 attempts at 20 ms (`auth-storage.ts:82-96`).
const BACKEND_LOCK_WAIT: Duration = Duration::from_millis(200);
const BACKEND_LOCK_RETRY: Duration = Duration::from_millis(20);

fn other(message: impl Into<String>) -> McpError {
    McpError::OAuth(OAuthError::Other(message.into()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// The pi-rs config root, without any dependency on `pi-coding-agent`
/// (`config.rs:139-155` of that crate): `$PI_CODING_AGENT_DIR` when set, else
/// `$XDG_CONFIG_HOME/pi-rs` (absolute only), else `~/.config/pi-rs`.
fn agent_dir() -> PathBuf {
    if let Some(dir) = std::env::var(ENV_AGENT_DIR)
        .ok()
        .filter(|dir| !dir.is_empty())
    {
        if let Some(rest) = dir.strip_prefix('~')
            && (rest.is_empty() || rest.starts_with('/'))
        {
            return home_dir().join(rest.trim_start_matches('/'));
        }
        return PathBuf::from(dir);
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|base| !base.is_empty() && Path::new(base).is_absolute())
        .map_or_else(|| home_dir().join(".config"), PathBuf::from);
    base.join(CONFIG_DIR_NAME)
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Where the credential file lives by default: `<agent-dir>/mcp-auth.json`
/// (`oauth.ts:72`), with the pi-rs XDG agent dir described above.
#[must_use]
pub fn default_store_path() -> PathBuf {
    agent_dir().join(MCP_AUTH_FILE_NAME)
}

/// Keys of one server's state (`storeKeys`, `oauth.ts:56-63`):
/// `mcp__<server-name>|<normalized-URL>` with `-` replaced by `_` in the
/// name, and, for state written by older versions, the bare normalized URL.
///
/// # Errors
/// On an unparsable server URL (`new URL` throwing upstream).
pub fn store_keys(name: &str, server_url: &str) -> Result<(String, String)> {
    let legacy_key = Url::parse(server_url)
        .map_err(|error| other(error.to_string()))?
        .to_string();
    Ok((
        format!("mcp__{}|{legacy_key}", name.replace('-', "_")),
        legacy_key,
    ))
}

/// Parse the file's contents (`parseStates`, `oauth.ts:46-50`): an absent,
/// empty, or non-object document is `{}`; values are kept verbatim.
///
/// # Errors
/// On malformed JSON.
fn parse_states(content: Option<&str>) -> Result<Map<String, Value>> {
    let Some(content) = content.filter(|text| !text.trim().is_empty()) else {
        return Ok(Map::new());
    };
    let parsed: Value = serde_json::from_str(content)
        .map_err(|error| other(format!("Failed to read {MCP_AUTH_FILE_NAME}: {error}")))?;
    Ok(match parsed {
        Value::Object(states) => states,
        _ => Map::new(),
    })
}

/// `JSON.stringify(states, null, 2)\n` (`serializeStates`, `oauth.ts:52-54`).
///
/// # Errors
/// On a value that cannot be serialized.
fn serialize_states(states: &Map<String, Value>) -> Result<String> {
    let mut text = serde_json::to_string_pretty(states)
        .map_err(|error| other(format!("Failed to write {MCP_AUTH_FILE_NAME}: {error}")))?;
    text.push('\n');
    Ok(text)
}

fn decode_state(value: &Value) -> Result<McpOAuthState> {
    serde_json::from_value(value.clone())
        .map_err(|error| other(format!("Invalid {MCP_AUTH_FILE_NAME} state: {error}")))
}

/// One nonce per lock acquisition, so a holder never removes another
/// process's lock after a stale takeover.
static NONCE: AtomicU64 = AtomicU64::new(0);

fn next_nonce() -> u64 {
    NONCE.fetch_add(1, Ordering::Relaxed) + 1
}

/// The in-process lock: upstream's `withLock` is synchronous and serializes
/// callers of one backend with the file lock itself.
static IN_PROCESS_LOCK: Mutex<()> = Mutex::new(());

struct FileLockGuard {
    dir: PathBuf,
    metadata: PathBuf,
    nonce: u64,
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        let owned = fs::read_to_string(&self.metadata)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| value.get("nonce").and_then(Value::as_u64))
            == Some(self.nonce);
        if owned {
            let _ = fs::remove_file(&self.metadata);
            let _ = fs::remove_dir(&self.dir);
        }
    }
}

#[derive(Debug)]
enum FileLockError {
    Busy,
    Io(std::io::Error),
}

/// One `proper-lockfile` attempt: create `<target>.lock` and write the
/// `lockfile.json` metadata (`oauth.ts:103-117`, `auth-storage.ts:81-96`).
fn try_lock(target: &Path, stale: Duration) -> std::result::Result<FileLockGuard, FileLockError> {
    let dir = PathBuf::from(format!("{}.lock", target.display()));
    match fs::create_dir(&dir) {
        Ok(()) => {
            let nonce = next_nonce();
            let metadata = dir.join("lockfile.json");
            let payload = json!({ "pid": std::process::id(), "nonce": nonce, "at": now_ms() });
            if let Err(error) = fs::write(&metadata, payload.to_string()) {
                let _ = fs::remove_dir(&dir);
                return Err(FileLockError::Io(error));
            }
            Ok(FileLockGuard {
                dir,
                metadata,
                nonce,
            })
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let is_stale = fs::metadata(&dir)
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age > stale));
            if is_stale {
                // The next attempt recreates the directory; a racing process
                // simply sees `AlreadyExists` again.
                let _ = fs::remove_dir_all(&dir);
            }
            Err(FileLockError::Busy)
        }
        Err(error) => Err(FileLockError::Io(error)),
    }
}

fn acquire_backend_lock(path: &Path) -> Result<FileLockGuard> {
    let deadline = Instant::now() + BACKEND_LOCK_WAIT;
    loop {
        match try_lock(path, BACKEND_LOCK_STALE) {
            Ok(guard) => return Ok(guard),
            Err(FileLockError::Io(error)) => return Err(McpError::Io(error)),
            Err(FileLockError::Busy) if Instant::now() < deadline => {
                thread::sleep(BACKEND_LOCK_RETRY);
            }
            Err(FileLockError::Busy) => {
                return Err(other(format!(
                    "{MCP_AUTH_FILE_NAME} is locked by another process"
                )));
            }
        }
    }
}

async fn acquire_refresh_lock(target: &Path) -> Result<FileLockGuard> {
    let deadline = Instant::now() + REFRESH_LOCK_WAIT;
    loop {
        match try_lock(target, REFRESH_LOCK_STALE) {
            Ok(guard) => return Ok(guard),
            Err(FileLockError::Io(error)) => return Err(McpError::Io(error)),
            Err(FileLockError::Busy) if Instant::now() < deadline => {
                tokio::time::sleep(REFRESH_LOCK_RETRY).await;
            }
            // `McpRefreshLockBusyError` (`errors.ts:61-65`), verbatim.
            Err(FileLockError::Busy) => {
                return Err(McpError::OAuth(OAuthError::RefreshLockBusy));
            }
        }
    }
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(parent)?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn ensure_file_exists(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?
            .write_all(b"{}")?;
    }
    #[cfg(not(unix))]
    fs::write(path, "{}")?;
    Ok(())
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Write the new bytes to a sibling and rename over the old file while the
/// lock is held: readers see either the whole old document or the whole new
/// one. The temporary file gets the same `0600` mode as upstream's writes.
fn write_atomically(path: &Path, content: &str) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(MCP_AUTH_FILE_NAME);
    let temp = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?
            .write_all(content.as_bytes())?;
    }
    #[cfg(not(unix))]
    fs::write(&temp, content.as_bytes())?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(McpError::Io(error));
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct Inner {
    path: PathBuf,
    /// `None` disables the cross-process refresh lock (upstream's
    /// `lockDir` being undefined).
    lock_dir: Option<PathBuf>,
}

/// pi's credential store (`McpOAuthCredentialStore`, `oauth.ts:66-157`):
/// one JSON file under the agent dir, locked load-modify-save operations, and
/// a per-server refresh lock. Cheap to clone; all clones share the file.
#[derive(Debug, Clone)]
pub struct McpOAuthCredentialStore {
    inner: Arc<Inner>,
}

impl Default for McpOAuthCredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

impl McpOAuthCredentialStore {
    /// The default store: `<agent-dir>/mcp-auth.json`, with the refresh lock
    /// files in the agent dir (`oauth.ts:71-74`).
    #[must_use]
    pub fn new() -> Self {
        let path = default_store_path();
        let lock_dir = path.parent().map(Path::to_path_buf);
        Self {
            inner: Arc::new(Inner { path, lock_dir }),
        }
    }

    /// A store at an explicit file path, with the refresh locks beside it.
    /// Useful for tests and for hosts that relocate the agent dir; unlike
    /// upstream's injected-backend constructor, the file lock stays enabled.
    #[must_use]
    pub fn with_path(path: PathBuf) -> Self {
        let lock_dir = path.parent().map(Path::to_path_buf);
        Self::with_lock_dir(path, lock_dir)
    }

    /// An explicit file path and lock dir. `None` for `lock_dir` mirrors
    /// upstream's no-cross-process-lock case (`oauth.ts:73`).
    #[must_use]
    pub fn with_lock_dir(path: PathBuf, lock_dir: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner { path, lock_dir }),
        }
    }

    /// The file this store reads and writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// The view of one server's state (`forServer`, `oauth.ts:76-94`).
    ///
    /// # Errors
    /// On an unparsable server URL.
    pub fn for_server(&self, name: &str, server_url: &str) -> Result<ServerOAuthStore> {
        let (key, legacy_key) = store_keys(name, server_url)?;
        let digest = Sha256::digest(key.as_bytes());
        let mut hash = String::with_capacity(16);
        for byte in &digest[..8] {
            let _ = write!(hash, "{byte:02x}");
        }
        Ok(ServerOAuthStore {
            store: self.clone(),
            key,
            legacy_key,
            refresh_hash: hash,
        })
    }

    /// The stored tokens without taking over legacy state (`tokens`,
    /// `oauth.ts:131-136`).
    ///
    /// # Errors
    /// On an unparsable URL, I/O failure, malformed JSON, or a bad state.
    pub fn tokens(&self, name: &str, server_url: &str) -> Result<Option<OAuthTokens>> {
        let (key, legacy_key) = store_keys(name, server_url)?;
        let tokens = self.with_lock(|states| {
            let value = states
                .get(&key)
                .or_else(|| states.get(&legacy_key))
                .and_then(|state| state.get("tokens"))
                .cloned();
            Ok((value, None))
        })?;
        tokens
            .map(|value| {
                serde_json::from_value(value)
                    .map_err(|error| other(format!("Invalid {MCP_AUTH_FILE_NAME} tokens: {error}")))
            })
            .transpose()
    }

    /// Remove one server's credentials, including legacy state it would have
    /// taken over (`remove`, `oauth.ts:138-148`). Returns whether anything
    /// was removed.
    ///
    /// # Errors
    /// On an unparsable URL, I/O failure, or malformed JSON.
    pub fn remove(&self, name: &str, server_url: &str) -> Result<bool> {
        let (key, legacy_key) = store_keys(name, server_url)?;
        self.with_lock(|states| {
            let stored = if states.contains_key(&key) {
                Some(&key)
            } else if states.contains_key(&legacy_key) {
                Some(&legacy_key)
            } else {
                None
            };
            let Some(stored) = stored else {
                return Ok((false, None));
            };
            let mut next = states.clone();
            next.remove(stored);
            Ok((true, Some(serialize_states(&next)?)))
        })
    }

    /// Lock, read, apply, and conditionally rewrite the whole document
    /// (`FileAuthStorageBackend.withLock`, `auth-storage.ts:98-116`).
    fn with_lock<T>(
        &self,
        apply: impl FnOnce(&Map<String, Value>) -> Result<(T, Option<String>)>,
    ) -> Result<T> {
        let _in_process = IN_PROCESS_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        ensure_parent_dir(&self.inner.path)?;
        let guard = acquire_backend_lock(&self.inner.path)?;
        ensure_file_exists(&self.inner.path)?;
        let current = fs::read_to_string(&self.inner.path)?;
        let states = parse_states(Some(&current))?;
        let (result, next) = apply(&states)?;
        if let Some(next) = next {
            write_atomically(&self.inner.path, &next)?;
        }
        drop(guard);
        Ok(result)
    }
}

/// One server's view of the shared store (`McpOAuthCredentialStore.forServer`,
/// `oauth.ts:76-94`): two keys, because servers sharing a URL keep separate
/// accounts while the legacy URL-only entry goes to the first loader.
#[derive(Debug, Clone)]
pub struct ServerOAuthStore {
    store: McpOAuthCredentialStore,
    key: String,
    legacy_key: String,
    refresh_hash: String,
}

impl McpOAuthStateStore for ServerOAuthStore {
    fn load(&self) -> Pin<Box<dyn Future<Output = Result<Option<McpOAuthState>>> + Send + '_>> {
        let store = self.store.clone();
        let key = self.key.clone();
        let legacy_key = self.legacy_key.clone();
        Box::pin(async move {
            store.with_lock(|states| {
                if let Some(state) = states.get(&key) {
                    return decode_state(state).map(|state| (Some(state), None));
                }
                let mut next = states.clone();
                let Some(legacy) = next.remove(&legacy_key) else {
                    return Ok((None, None));
                };
                let state = decode_state(&legacy)?;
                // The legacy URL-only entry is MOVED to this server's
                // namespaced key; removing it without reinserting would drop
                // the credential for every later reader.
                next.insert(key.clone(), legacy);
                Ok((Some(state), Some(serialize_states(&next)?)))
            })
        })
    }

    fn save(&self, state: McpOAuthState) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        let store = self.store.clone();
        let key = self.key.clone();
        Box::pin(async move {
            store.with_lock(|states| {
                let mut next = states.clone();
                next.insert(
                    key.clone(),
                    serde_json::to_value(&state).map_err(|error| {
                        other(format!("Invalid {MCP_AUTH_FILE_NAME} state: {error}"))
                    })?,
                );
                Ok(((), Some(serialize_states(&next)?)))
            })
        })
    }
}

impl McpOAuthServerStore for ServerOAuthStore {
    fn with_refresh_lock<'a>(
        &'a self,
        f: Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let Some(lock_dir) = self.store.inner.lock_dir.as_ref() else {
                return f.await;
            };
            // `oauth.ts:104-106`: sha256 of the full store key, first 16 hex.
            let target = lock_dir.join(format!("mcp-auth-refresh-{}", self.refresh_hash));
            let guard = acquire_refresh_lock(&target).await?;
            let outcome = f.await;
            drop(guard);
            outcome
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://mcp.example.com/mcp";

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "pi-rs-mcp-auth-test-{}-{sequence}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("test temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn file(&self) -> PathBuf {
            self.path().join(MCP_AUTH_FILE_NAME)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn state(server_url: &str, token: &str) -> McpOAuthState {
        McpOAuthState {
            server_url: server_url.to_string(),
            tokens: Some(OAuthTokens {
                access_token: token.to_string(),
                token_type: "Bearer".to_string(),
                ..OAuthTokens::default()
            }),
            ..McpOAuthState::default()
        }
    }

    #[test]
    fn a_missing_file_starts_as_empty_and_is_created_with_upstream_bytes() {
        let dir = TempDir::new();
        let path = dir.file();
        let store = McpOAuthCredentialStore::with_path(path.clone());
        let server = store.for_server("work", URL).expect("server");
        futures::executor::block_on(async {
            assert!(server.load().await.expect("load").is_none());
            assert_eq!(fs::read_to_string(&path).expect("created file"), "{}");
            server.save(state(URL, "at")).await.expect("save");
        });
        assert!(path.exists());
        assert_eq!(
            store
                .tokens("work", URL)
                .expect("tokens")
                .map(|tokens| tokens.access_token),
            Some("at".to_string())
        );
    }

    #[test]
    fn a_save_uses_two_space_indent_camel_case_and_a_trailing_newline() {
        let dir = TempDir::new();
        let path = dir.file();
        let store = McpOAuthCredentialStore::with_path(path.clone());
        let server = store.for_server("work", URL).expect("server");
        futures::executor::block_on(server.save(state(URL, "at"))).expect("save");
        assert_eq!(
            fs::read_to_string(&path).expect("file"),
            concat!(
                "{\n",
                "  \"mcp__work|https://mcp.example.com/mcp\": {\n",
                "    \"serverUrl\": \"https://mcp.example.com/mcp\",\n",
                "    \"tokens\": {\n",
                "      \"access_token\": \"at\",\n",
                "      \"token_type\": \"Bearer\"\n",
                "    }\n",
                "  }\n",
                "}\n",
            )
        );
        let loaded = futures::executor::block_on(server.load())
            .expect("load")
            .expect("state");
        assert_eq!(loaded, state(URL, "at"));
    }

    #[test]
    fn sequential_saves_merge_instead_of_clobbering_and_a_fresh_store_sees_them() {
        let dir = TempDir::new();
        let path = dir.file();
        let store = McpOAuthCredentialStore::with_path(path.clone());
        let work = store.for_server("work", URL).expect("work");
        let personal = store.for_server("personal", URL).expect("personal");
        futures::executor::block_on(async {
            work.save(state(URL, "work-token"))
                .await
                .expect("work save");
            personal
                .save(state(URL, "personal-token"))
                .await
                .expect("personal save");
            work.save(state(URL, "work-token-2"))
                .await
                .expect("work again");
            let loaded = work.load().await.expect("load").expect("state");
            assert_eq!(
                loaded.tokens.map(|tokens| tokens.access_token).as_deref(),
                Some("work-token-2")
            );
        });
        let fresh = McpOAuthCredentialStore::with_path(path);
        assert_eq!(
            fresh
                .tokens("work", URL)
                .expect("tokens")
                .map(|tokens| tokens.access_token),
            Some("work-token-2".to_string())
        );
        assert_eq!(
            fresh
                .tokens("personal", URL)
                .expect("tokens")
                .map(|tokens| tokens.access_token),
            Some("personal-token".to_string())
        );
    }

    #[test]
    fn malformed_json_errors_but_a_non_object_document_is_read_as_empty() {
        let dir = TempDir::new();
        let path = dir.file();
        let store = McpOAuthCredentialStore::with_path(path.clone());
        let server = store.for_server("work", URL).expect("server");
        fs::write(&path, "{ not json").expect("corrupt file");
        let error = futures::executor::block_on(server.load()).expect_err("corrupt");
        assert!(
            error
                .to_string()
                .starts_with("Failed to read mcp-auth.json:")
        );
        // Upstream's `parseStates` falls back to `{}` for every non-object.
        fs::write(&path, "[]").expect("array file");
        assert!(
            futures::executor::block_on(server.load())
                .expect("array")
                .is_none()
        );
        fs::write(&path, "  \n").expect("blank file");
        assert!(
            futures::executor::block_on(server.load())
                .expect("blank")
                .is_none()
        );
        futures::executor::block_on(server.save(state(URL, "fresh"))).expect("save");
        let stored: Value = serde_json::from_str(&fs::read_to_string(&path).expect("file"))
            .expect("valid json after rewrite");
        assert!(stored.is_object());
    }

    #[test]
    fn legacy_url_state_goes_to_the_first_loader_and_servers_sharing_a_url_stay_separate() {
        let dir = TempDir::new();
        let path = dir.file();
        let mut seed = Map::new();
        seed.insert(
            URL.to_string(),
            serde_json::to_value(state(URL, "legacy-token")).expect("seed state"),
        );
        fs::write(&path, serde_json::to_string(&seed).expect("seed")).expect("seed file");
        let store = McpOAuthCredentialStore::with_path(path.clone());

        // Reading tokens does not take the legacy entry over.
        assert_eq!(
            store
                .tokens("work", URL)
                .expect("tokens")
                .map(|tokens| tokens.access_token),
            Some("legacy-token".to_string())
        );
        let keys: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("file")).expect("json");
        assert_eq!(keys.as_object().expect("object").len(), 1);

        let my_work = store.for_server("my_work", URL).expect("server");
        let loaded = futures::executor::block_on(my_work.load())
            .expect("load")
            .expect("state");
        assert_eq!(
            loaded.tokens.map(|tokens| tokens.access_token).as_deref(),
            Some("legacy-token")
        );
        // Names differing only in `-` and `_` share the namespace.
        assert_eq!(
            store
                .tokens("my-work", URL)
                .expect("tokens")
                .map(|tokens| tokens.access_token),
            Some("legacy-token".to_string())
        );
        let personal = store.for_server("personal", URL).expect("other server");
        assert!(
            futures::executor::block_on(personal.load())
                .expect("load")
                .is_none()
        );

        let stored: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("file")).expect("json");
        assert_eq!(
            stored
                .as_object()
                .expect("object")
                .keys()
                .next()
                .map(String::as_str),
            Some(format!("mcp__my_work|{URL}").as_str())
        );
        // Sign-out removes the migrated entry; a second sign-out is a no-op.
        assert!(store.remove("my_work", URL).expect("remove"));
        assert!(!store.remove("my_work", URL).expect("again"));
        assert!(!store.remove("work", URL).expect("legacy key is gone"));
    }

    #[test]
    fn store_keys_normalize_the_url_and_namespace_the_server_name() {
        let (key, legacy) = store_keys("my-work", "https://mcp.example.com").expect("keys");
        assert_eq!(legacy, "https://mcp.example.com/");
        assert_eq!(key, "mcp__my_work|https://mcp.example.com/");
        assert_eq!(
            store_keys("my_work", "https://mcp.example.com").expect("alias"),
            (key, legacy)
        );
        assert!(store_keys("work", "not a url").is_err());
    }
}
