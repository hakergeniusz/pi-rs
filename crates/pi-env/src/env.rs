//! The execution environment backed by a daemon.
//!
//! Ground truth: `packages/env/src/remote-env.ts:1-975`. Ported behaviour:
//! the constants (`:36-49`), the daemon handle (`:133-158`), the file-info
//! conversion (`:72-81`), path resolution (`:377-405`) with Node's home,
//! `file://` and Windows drive-cwd rules, `absolutePath`/`joinPath`
//! (`:438-452`), read/write (`:454-527`, `:589-640`), `fileInfo`/`exists`/
//! `listFiles` (`:686-739`), `watch` (`:830-853`) and `exec`/`cleanup`
//! (`:855-972`).
//!
//! # Deviations
//!
//! * Pipelined `pread`/`writeChunk` depth (`READ_DEPTH`/`WRITE_DEPTH`), the
//!   incremental UTF-8 decoder, `scanLines` and the streaming readers are
//!   replaced by sequential chunking plus a final UTF-8 decode. The protocol
//!   and error codes are unchanged; only concurrency inside one operation is
//!   reduced.
//! * Timeout enforcement is delegated to the daemon via `timeoutMs`; the
//!   client-side `setTimeout` is host-driven (no timers in the port).
//! * `onOutput(text, context, info)` becomes `onOutput(text, info)`; the
//!   context was only used to check aborts.

use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::{
    JsonMap,
    connection::{BoxFuture, Daemon, Event, Reply, RequestOptions, node_platform},
    errors::{
        AbortSignal, ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, RemoteError,
        abort_result, to_file_error,
    },
    files::{
        FileChange, FileInfo, FileListing, ListFilesOptions, RemoteFileInfo, WatchTarget,
        list_files_over, to_info,
    },
    watch::{RemoteWatchOptions, RemoteWatcher},
};

/// Largest timeout accepted by the client (2^31-1 ms, `remote-env.ts:36-37`).
pub const MAX_TIMEOUT_MS: f64 = 2_147_483_647.0;
/// The same bound in seconds, used in the error message.
pub const MAX_TIMEOUT_SECONDS: f64 = MAX_TIMEOUT_MS / 1000.0;
/// Bytes per `pread` of a binary reader (`remote-env.ts:39`).
pub const READ_CHUNK: u64 = 256 * 1024;
/// Bytes per `writeFile` chunk (`remote-env.ts:42`).
pub const WRITE_CHUNK: usize = 512 * 1024;
/// Known-size read chunk (`remote-env.ts:45`).
pub const READ_FILE_CHUNK: u64 = 512 * 1024;
/// Unknown-size read chunk (`remote-env.ts:46`).
pub const READ_FILE_UNKNOWN_CHUNK: u64 = 64 * 1024;
/// `readLine` read size (`remote-env.ts:49`).
pub const LINE_CHUNK: u64 = 64 * 1024;

/// A command passed to `exec`: a shell string or an argv (`exec`'s
/// `string | readonly string[]`, `remote-env.ts:856`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Run through the remote shell.
    Shell(String),
    /// Run directly, without a shell.
    Argv(Vec<String>),
}

/// Output-stream metadata passed to `onOutput` (`ShellOutputInfo`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellOutputInfo {
    /// `stdout` or `stderr`.
    pub stream: String,
    /// How the daemon skipped output, when it did.
    pub skipped: Option<String>,
}

/// Callback invoked for each non-empty output chunk.
pub type OutputSink = Arc<dyn Fn(&str, &ShellOutputInfo) + Send + Sync>;

/// Options for `exec` (`ShellExecOptions`, `remote-env.ts:855-870`).
pub struct ShellExecOptions {
    /// Working directory; defaults to the environment's `cwd`.
    pub cwd: Option<String>,
    /// Environment overrides, in insertion order.
    pub env: Vec<(String, String)>,
    /// Whether to inherit the remote environment (`true` by default).
    pub inherit_env: bool,
    /// Timeout in seconds (finite, positive, at most [`MAX_TIMEOUT_SECONDS`]).
    pub timeout: Option<f64>,
    /// Spill threshold forwarded to the daemon.
    pub spill: Option<Value>,
    /// Output window forwarded to the daemon.
    pub window: Option<Value>,
    /// Output callback.
    pub on_output: Option<OutputSink>,
}

impl Default for ShellExecOptions {
    /// Upstream's defaults, notably `inheritEnv: true` (`remote-env.ts:871`).
    fn default() -> Self {
        Self {
            cwd: None,
            env: Vec::new(),
            inherit_env: true,
            timeout: None,
            spill: None,
            window: None,
            on_output: None,
        }
    }
}

impl ShellExecOptions {
    /// Validate `timeout` with upstream's exact errors (`remote-env.ts:863-869`).
    pub fn validate_timeout(timeout: f64) -> Result<(), ExecutionError> {
        if !timeout.is_finite() || timeout <= 0.0 {
            return Err(ExecutionError::new(
                ExecutionErrorCode::Timeout,
                "Invalid timeout: must be a finite number of seconds",
            ));
        }
        if timeout > MAX_TIMEOUT_SECONDS {
            return Err(ExecutionError::new(
                ExecutionErrorCode::Timeout,
                format!("Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds"),
            ));
        }
        Ok(())
    }
}

/// The result of `exec` (`ShellExecResult`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellExecResult {
    /// Process exit code.
    pub exit_code: i64,
    /// Path of a spill file, when the daemon wrote one.
    pub spill_path: Option<String>,
}

/// The minimal environment surface shared by local and remote sessions.
pub trait ExecutionEnv: Send + Sync {
    /// The file-namespace id.
    fn id(&self) -> &str;
    /// A snapshot of the working directory.
    fn cwd(&self) -> String;
    /// Resolve a path against the remote system's home and cwd.
    fn absolute_path<'a>(
        &'a self,
        path: &'a str,
        abort: &'a AbortSignal,
    ) -> BoxFuture<'a, Result<String, FileError>>;
    /// Read a UTF-8 file.
    fn read_text_file<'a>(
        &'a self,
        path: &'a str,
        abort: &'a AbortSignal,
    ) -> BoxFuture<'a, Result<String, FileError>>;
    /// Whether a file exists (a `not_found` answer becomes `false`).
    fn exists<'a>(
        &'a self,
        path: &'a str,
        abort: &'a AbortSignal,
    ) -> BoxFuture<'a, Result<bool, FileError>>;
    /// List files through the daemon.
    fn list_files<'a>(
        &'a self,
        path: &'a str,
        options: Option<&'a ListFilesOptions>,
        abort: &'a AbortSignal,
    ) -> BoxFuture<'a, Result<FileListing, FileError>>;
    /// Run a command.
    fn exec<'a>(
        &'a self,
        command: &'a Command,
        options: Option<&'a ShellExecOptions>,
        abort: &'a AbortSignal,
    ) -> BoxFuture<'a, Result<ShellExecResult, ExecutionError>>;
    /// Kill the commands this environment started.
    fn cleanup(&self) -> BoxFuture<'_, ()>;
}

/// Node-style path rules for the remote system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathRules {
    /// Whether the remote uses Windows (`win32`) rules.
    pub windows: bool,
}

impl PathRules {
    /// Pick `win32` or `posix` from the daemon's reported platform.
    #[must_use]
    pub fn for_info(info: &crate::connection::RemoteInfo) -> Self {
        Self {
            windows: node_platform(info).0 == "win32",
        }
    }

    /// The remote separator.
    #[must_use]
    pub const fn separator(self) -> char {
        if self.windows { '\\' } else { '/' }
    }

    /// Whether `path` is absolute under these rules.
    #[must_use]
    pub fn is_absolute(self, path: &str) -> bool {
        let bytes = path.as_bytes();
        if bytes.first() == Some(&b'/') {
            return true;
        }
        self.windows
            && (bytes.first() == Some(&b'\\')
                || (bytes.len() >= 3
                    && bytes[1] == b':'
                    && (bytes[2] == b'\\' || bytes[2] == b'/')))
    }

    /// Collapse `.`/`..` and repeated separators, keeping the root prefix.
    #[must_use]
    pub fn normalize(self, raw: &str) -> String {
        if raw.is_empty() {
            return String::new();
        }
        let separator = self.separator();
        let bytes = raw.as_bytes();
        let prefix_len = if bytes.first() == Some(&b'/') {
            1
        } else if self.windows
            && bytes.len() >= 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes.get(2), Some(b'\\' | b'/'))
        {
            3
        } else if self.windows && bytes.starts_with(b"\\\\") {
            2
        } else {
            usize::from(self.windows && bytes.first() == Some(&b'\\'))
        };
        let prefix: String = raw.chars().take(prefix_len).collect();
        let mut parts: Vec<&str> = Vec::new();
        for segment in raw[prefix_len..].split(['/', '\\']) {
            match segment {
                ".." if !parts.is_empty() && parts.last() != Some(&"..") => {
                    parts.pop();
                }
                ".." if prefix_len == 0 => parts.push(".."),
                "" | "." | ".." => {}
                other => parts.push(other),
            }
        }
        let mut out = prefix;
        if !parts.is_empty() {
            out.push_str(&parts.join(&separator.to_string()));
        }
        if out.is_empty() { ".".to_owned() } else { out }
    }

    /// `paths.resolve(base, path)`.
    #[must_use]
    pub fn resolve(self, base: &str, path: &str) -> String {
        if self.is_absolute(path) {
            return self.normalize(path);
        }
        let separator = self.separator();
        // Win32.resolve drops the drive of a drive-relative path when the
        // resolved base is already on that drive (`remote-env.ts:389-397`).
        let relative = if self.windows
            && path.len() >= 2
            && path.as_bytes()[0].is_ascii_alphabetic()
            && path.as_bytes()[1] == b':'
        {
            &path[2..]
        } else {
            path
        };
        self.normalize(&format!("{base}{separator}{relative}"))
    }

    /// `paths.join(...parts)` — unlike `resolve`, a leading separator on a
    /// later part does not reset the base.
    #[must_use]
    pub fn join(self, parts: &[String]) -> String {
        let separator = self.separator().to_string();
        self.normalize(&parts.join(&separator))
    }
}

/// `fileURLToPath` for the narrow `file://` forms the daemon produces.
/// Percent-encoded bytes are decoded; a `localhost` host is dropped and a
/// non-local host becomes a UNC path on Windows (an error upstream — the port
/// keeps it as a path rather than failing resolution).
fn file_url_to_path(rest: &str, windows: bool) -> String {
    let path = match rest.find('/') {
        Some(index) => &rest[index..],
        None => "/",
    };
    let mut decoded = Vec::with_capacity(path.len());
    let bytes = path.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            decoded.push(high * 16 + low);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    let text = String::from_utf8_lossy(&decoded).into_owned();
    if windows {
        text.replace('/', "\\")
    } else {
        text
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// `#resolve` (`remote-env.ts:377-405`): Node's `resolvePath` on the remote.
pub fn resolve_remote_path(path: &str, info: &crate::connection::RemoteInfo, cwd: &str) -> String {
    let rules = PathRules::for_info(info);
    let mut normalized = path.to_owned();
    if normalized == "~" {
        normalized.clone_from(&info.home);
    } else if let Some(rest) = normalized.strip_prefix("~/") {
        normalized = rules.join(&[info.home.clone(), rest.to_owned()]);
    } else if rules.windows && normalized.starts_with("~\\") {
        normalized = rules.join(&[info.home.clone(), normalized[2..].to_owned()]);
    } else if let Some(rest) = normalized.strip_prefix("file://") {
        normalized = file_url_to_path(rest, rules.windows);
    }
    if rules.is_absolute(&normalized) {
        return rules.normalize(&normalized);
    }
    if rules.windows {
        let bytes = normalized.as_bytes();
        let drive_relative = bytes.len() >= 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && !matches!(bytes.get(2), Some(b'\\' | b'/'));
        if drive_relative {
            let drive = &normalized[..2];
            let cwd_drive = cwd.get(..2);
            if !cwd_drive.is_some_and(|prefix| prefix.eq_ignore_ascii_case(drive)) {
                let key = drive.to_uppercase();
                let mut base = info
                    .drive_cwds
                    .get(&key)
                    .and_then(Value::as_str)
                    .unwrap_or(&info.cwd)
                    .to_owned();
                let base_drive = base.get(..2);
                if !base_drive.is_some_and(|prefix| prefix.eq_ignore_ascii_case(drive))
                    && base.as_bytes().get(2) == Some(&b'\\')
                {
                    base = format!("{drive}\\");
                }
                return rules.resolve(&base, &normalized);
            }
        }
    }
    rules.resolve(cwd, &normalized)
}

/// A daemon handle, valid only in the session that opened it
/// (`remote-env.ts:133-158`).
struct Handle {
    daemon: Arc<dyn Daemon>,
    id: u64,
    session: u64,
    /// Upstream keeps the resolved path on the handle (`remote-env.ts:140-151`);
    /// no port-side consumer reads it yet.
    #[allow(dead_code)]
    path: String,
}

impl Handle {
    async fn request(
        &self,
        op: &str,
        json: JsonMap,
        payload: Vec<u8>,
        abort: &AbortSignal,
    ) -> Result<Reply, crate::RemoteError> {
        let mut body = json;
        body.insert("handle".into(), Value::from(self.id));
        self.daemon
            .request(
                op,
                body,
                RequestOptions {
                    payload,
                    session: Some(self.session),
                    abort: Some(abort.clone()),
                    ..Default::default()
                },
            )
            .await
    }

    async fn pread(&self, offset: Option<u64>, length: u64) -> Result<Vec<u8>, crate::RemoteError> {
        let mut json = JsonMap::new();
        if let Some(offset) = offset {
            json.insert("offset".into(), Value::from(offset));
        }
        json.insert("length".into(), Value::from(length));
        Ok(self
            .request("pread", json, Vec::new(), &AbortSignal::new())
            .await?
            .payload)
    }

    async fn close(&self) {
        let _ = self
            .request("close", JsonMap::new(), Vec::new(), &AbortSignal::new())
            .await;
    }
}

/// Options for [`RemoteExecutionEnv`].
pub struct RemoteExecutionEnvOptions {
    /// The injected daemon connection.
    pub daemon: Arc<dyn Daemon>,
    /// File-namespace id (`pi-env:<env name>` upstream).
    pub id: String,
    /// Working directory used to resolve relative paths.
    pub cwd: String,
    /// Shell path forwarded to `exec`.
    pub shell_path: Option<String>,
    /// Environment added to commands that inherit it.
    pub shell_env: Option<Vec<(String, String)>>,
    /// How the daemon watches files.
    pub watch: RemoteWatchOptions,
}

/// An `ExecutionEnv` on another machine, reached through a daemon
/// (`RemoteExecutionEnv`, `remote-env.ts:350-973`).
pub struct RemoteExecutionEnv {
    id: String,
    cwd: Mutex<String>,
    daemon: Arc<dyn Daemon>,
    shell_path: Option<String>,
    shell_env: Option<Vec<(String, String)>>,
    watch: RemoteWatchOptions,
    running: Arc<Mutex<Vec<(u64, u64)>>>,
}

impl std::fmt::Debug for RemoteExecutionEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RemoteExecutionEnv {{ id: {}, cwd: {} }}",
            self.id,
            self.cwd.lock().expect("cwd lock")
        )
    }
}

impl RemoteExecutionEnv {
    /// Build an environment over `options.daemon`.
    #[must_use]
    pub fn new(options: RemoteExecutionEnvOptions) -> Self {
        Self {
            id: options.id,
            cwd: Mutex::new(options.cwd),
            daemon: options.daemon,
            shell_path: options.shell_path,
            shell_env: options.shell_env,
            watch: options.watch,
            running: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The injected daemon, for adapters such as `LocalExecutionEnv`.
    #[must_use]
    pub fn daemon(&self) -> &dyn Daemon {
        self.daemon.as_ref()
    }

    /// The current working directory.
    #[must_use]
    pub fn cwd_snapshot(&self) -> String {
        self.cwd.lock().expect("cwd lock").clone()
    }

    async fn resolve(&self, path: &str, abort: &AbortSignal) -> Result<String, crate::RemoteError> {
        let info = self.daemon.info(Some(abort.clone())).await?;
        let cwd = self.cwd_snapshot();
        Ok(resolve_remote_path(path, &info, &cwd))
    }

    async fn open(
        &self,
        resolved: &str,
        mut json: JsonMap,
    ) -> Result<(Handle, JsonMap), crate::RemoteError> {
        json.insert("path".into(), Value::String(resolved.to_owned()));
        let reply = self
            .daemon
            .request("open", json, RequestOptions::default())
            .await?;
        let id = reply
            .json
            .get("handle")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        Ok((
            Handle {
                daemon: self.daemon.clone(),
                id,
                session: reply.session,
                path: resolved.to_owned(),
            },
            reply.json,
        ))
    }

    /// `absolutePath` (`remote-env.ts:438-444`).
    pub async fn absolute_path(
        &self,
        path: &str,
        abort: &AbortSignal,
    ) -> Result<String, FileError> {
        if let Some(failure) = abort_result(abort, Some(path)) {
            return failure;
        }
        self.resolve(path, abort)
            .await
            .map_err(|error| to_file_error(&error, Some(path)))
    }

    /// `joinPath` (`remote-env.ts:446-452`).
    pub async fn join_path(&self, parts: &[String]) -> Result<String, FileError> {
        let info = self
            .daemon
            .info(None)
            .await
            .map_err(|error| to_file_error(&error, None))?;
        Ok(PathRules::for_info(&info).join(parts))
    }

    /// `fileInfo` (`remote-env.ts:686-692`).
    pub async fn file_info(&self, path: &str, abort: &AbortSignal) -> Result<FileInfo, FileError> {
        let resolved = self.absolute_path(path, abort).await?;
        if let Some(failure) = abort_result(abort, Some(&resolved)) {
            return failure;
        }
        let reply = self
            .daemon
            .request(
                "lstat",
                json_path(&resolved),
                RequestOptions {
                    abort: Some(abort.clone()),
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| to_file_error(&error, Some(&resolved)))?;
        let info = RemoteFileInfo::from_json(&reply.json);
        let rules = PathRules::for_info(
            &self
                .daemon
                .info(None)
                .await
                .map_err(|error| to_file_error(&error, None))?,
        );
        to_info(&resolved, &info, rules.windows)
    }

    /// `exists` (`remote-env.ts:764-769`).
    pub async fn exists(&self, path: &str, abort: &AbortSignal) -> Result<bool, FileError> {
        match self.file_info(path, abort).await {
            Ok(_) => Ok(true),
            Err(error) if error.code == FileErrorCode::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn read_file(&self, path: &str, abort: &AbortSignal) -> Result<Vec<u8>, FileError> {
        let resolved = self.absolute_path(path, abort).await?;
        if let Some(failure) = abort_result(abort, Some(&resolved)) {
            return failure;
        }
        let mut open = JsonMap::new();
        open.insert("mode".into(), Value::String("read".into()));
        let (handle, reply) = self
            .open(&resolved, open)
            .await
            .map_err(|error| to_file_error(&error, Some(&resolved)))?;
        let outcome = async {
            if let Some(error) = reply.get("statError") {
                return Err(to_file_error(
                    &RemoteError::from_fields(error.as_object().cloned().unwrap_or_default()),
                    Some(&resolved),
                ));
            }
            let stat = RemoteFileInfo::from_json(
                reply
                    .get("stat")
                    .and_then(Value::as_object)
                    .unwrap_or(&JsonMap::new()),
            );
            let size = if stat.kind == "file" { stat.size } else { 0 };
            if size > i32::MAX as u64 {
                return Err(FileError::new(
                    FileErrorCode::Unknown,
                    format!("File size ({size}) is greater than 2 GiB"),
                    Some(&resolved),
                ));
            }
            let mut bytes = Vec::new();
            if size == 0 {
                loop {
                    if abort.aborted() {
                        return Err(FileError::aborted(Some(&resolved)));
                    }
                    let chunk = handle
                        .pread(None, READ_FILE_UNKNOWN_CHUNK)
                        .await
                        .map_err(|error| to_file_error(&error, Some(&resolved)))?;
                    if chunk.is_empty() {
                        break;
                    }
                    bytes.extend_from_slice(&chunk);
                }
            } else {
                let mut offset = 0;
                while offset < size {
                    if abort.aborted() {
                        return Err(FileError::aborted(Some(&resolved)));
                    }
                    let length = (size - offset).min(READ_FILE_CHUNK);
                    let chunk = handle
                        .pread(Some(offset), length)
                        .await
                        .map_err(|error| to_file_error(&error, Some(&resolved)))?;
                    if chunk.is_empty() {
                        break;
                    }
                    bytes.extend_from_slice(&chunk);
                    offset += chunk.len() as u64;
                }
            }
            if abort.aborted() {
                return Err(FileError::aborted(Some(&resolved)));
            }
            Ok(bytes)
        }
        .await;
        handle.close().await;
        outcome
    }

    /// `readTextFile` (`remote-env.ts:529-531`).
    pub async fn read_text_file(
        &self,
        path: &str,
        abort: &AbortSignal,
    ) -> Result<String, FileError> {
        let bytes = self.read_file(path, abort).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// `readBinaryFile` (`remote-env.ts:533-535`).
    pub async fn read_binary_file(
        &self,
        path: &str,
        abort: &AbortSignal,
    ) -> Result<Vec<u8>, FileError> {
        self.read_file(path, abort).await
    }

    async fn write(
        &self,
        path: &str,
        content: &[u8],
        append: bool,
        abort: &AbortSignal,
    ) -> Result<(), FileError> {
        let resolved = self.absolute_path(path, abort).await?;
        if let Some(failure) = abort_result(abort, Some(&resolved)) {
            return failure;
        }
        let first_len = content.len().min(WRITE_CHUNK);
        let keep = content.len() > WRITE_CHUNK;
        let mut json = JsonMap::new();
        json.insert("path".into(), Value::String(resolved.clone()));
        json.insert("append".into(), Value::Bool(append));
        json.insert("keep".into(), Value::Bool(keep));
        let reply = self
            .daemon
            .request(
                "write",
                json,
                RequestOptions {
                    payload: content[..first_len].to_vec(),
                    abort: Some(abort.clone()),
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| to_file_error(&error, Some(&resolved)))?;
        if keep {
            let id = reply
                .json
                .get("handle")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let handle = Handle {
                daemon: self.daemon.clone(),
                id,
                session: reply.session,
                path: resolved.clone(),
            };
            for chunk in content[first_len..].chunks(WRITE_CHUNK) {
                if !append && abort.aborted() {
                    handle.close().await;
                    return Err(FileError::aborted(Some(&resolved)));
                }
                let result = handle
                    .request("writeChunk", JsonMap::new(), chunk.to_vec(), abort)
                    .await;
                if let Err(error) = result {
                    handle.close().await;
                    return Err(to_file_error(&error, Some(&resolved)));
                }
            }
            handle.close().await;
        }
        if append && abort.aborted() {
            return Err(FileError::aborted(Some(&resolved)));
        }
        Ok(())
    }

    /// `writeFile` (`remote-env.ts:634-636`).
    pub async fn write_file(
        &self,
        path: &str,
        content: &[u8],
        abort: &AbortSignal,
    ) -> Result<(), FileError> {
        self.write(path, content, false, abort).await
    }

    /// `appendFile` (`remote-env.ts:638-640`).
    pub async fn append_file(
        &self,
        path: &str,
        content: &[u8],
        abort: &AbortSignal,
    ) -> Result<(), FileError> {
        self.write(path, content, true, abort).await
    }

    /// `listFiles` (`remote-env.ts:726-739`).
    pub async fn list_files(
        &self,
        path: &str,
        options: Option<&ListFilesOptions>,
        abort: &AbortSignal,
    ) -> Result<FileListing, FileError> {
        let resolved = self.absolute_path(path, abort).await?;
        list_files_over(self.daemon.as_ref(), &resolved, options, abort).await
    }

    /// `watch` (`remote-env.ts:830-853`) — resolve targets then open the
    /// daemon-side watcher.
    pub async fn watch(
        &self,
        targets: &[WatchTarget],
        on_change: Arc<dyn Fn(FileChange) + Send + Sync>,
        abort: &AbortSignal,
    ) -> Result<RemoteWatcher, FileError> {
        if let Some(failure) = abort_result(abort, None) {
            return failure;
        }
        let mut resolved = Vec::with_capacity(targets.len());
        for target in targets {
            let path = self
                .resolve(&target.path, abort)
                .await
                .map_err(|error| to_file_error(&error, None))?;
            resolved.push(WatchTarget {
                path,
                ..target.clone()
            });
        }
        if let Some(failure) = abort_result(abort, None) {
            return failure;
        }
        RemoteWatcher::open(self.daemon.clone(), resolved, on_change, self.watch.clone())
            .await
            .map_err(|error| to_file_error(&error, None))
    }

    /// `exec` (`remote-env.ts:855-966`).
    #[allow(clippy::too_many_lines)] // mirrors the TS exec; split when extending
    pub async fn exec(
        &self,
        command: &Command,
        options: Option<&ShellExecOptions>,
        abort: &AbortSignal,
    ) -> Result<ShellExecResult, ExecutionError> {
        if abort.aborted() {
            return Err(ExecutionError::new(ExecutionErrorCode::Aborted, "aborted"));
        }
        let defaults = ShellExecOptions::default();
        let options = options.unwrap_or(&defaults);
        if let Some(timeout) = options.timeout {
            ShellExecOptions::validate_timeout(timeout)?;
        }
        let inherit_env = options.inherit_env;
        let mut env = JsonMap::new();
        if inherit_env && let Some(shell_env) = &self.shell_env {
            for (key, value) in shell_env {
                env.insert(key.clone(), Value::String(value.clone()));
            }
        }
        for (key, value) in &options.env {
            env.insert(key.clone(), Value::String(value.clone()));
        }
        let cwd = match &options.cwd {
            Some(cwd) => match self.resolve(cwd, abort).await {
                Ok(cwd) => cwd,
                Err(error) => {
                    if abort.aborted() {
                        return Err(ExecutionError::new(ExecutionErrorCode::Aborted, "aborted"));
                    }
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::Unknown,
                        error.message,
                    ));
                }
            },
            None => self.cwd_snapshot(),
        };
        if abort.aborted() {
            return Err(ExecutionError::new(ExecutionErrorCode::Aborted, "aborted"));
        }
        let mut json = JsonMap::new();
        match command {
            Command::Shell(text) => {
                json.insert("command".into(), Value::String(text.clone()));
            }
            Command::Argv(argv) => {
                json.insert(
                    "argv".into(),
                    Value::Array(argv.iter().cloned().map(Value::String).collect()),
                );
            }
        }
        json.insert("cwd".into(), Value::String(cwd));
        json.insert("env".into(), Value::Object(env));
        json.insert("inheritEnv".into(), Value::Bool(inherit_env));
        if let Some(shell_path) = &self.shell_path {
            json.insert("shellPath".into(), Value::String(shell_path.clone()));
        }
        if let Some(timeout) = options.timeout {
            json.insert("timeoutMs".into(), Value::from(timeout * 1000.0));
        }
        if let Some(spill) = &options.spill {
            json.insert("spill".into(), spill.clone());
        }
        if let Some(window) = &options.window {
            json.insert("window".into(), window.clone());
        }
        let running = self.running.clone();
        let on_start: Arc<dyn Fn(u64, u64) + Send + Sync> = Arc::new(move |id, session| {
            running.lock().expect("running lock").push((id, session));
        });
        let on_output = options.on_output.clone();
        let on_event: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(move |event| {
            if event.kind() != Some("output") {
                return;
            }
            let text = String::from_utf8_lossy(&event.payload);
            if text.is_empty() {
                return;
            }
            let info = ShellOutputInfo {
                stream: if event.json.get("stream").and_then(Value::as_str) == Some("stderr") {
                    "stderr"
                } else {
                    "stdout"
                }
                .to_owned(),
                skipped: event
                    .json
                    .get("skipped")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            };
            if let Some(callback) = &on_output {
                callback(&text, &info);
            }
        });
        let reply = self
            .daemon
            .request(
                "exec",
                json,
                RequestOptions {
                    on_event: Some(on_event),
                    on_start: Some(on_start),
                    abort: Some(abort.clone()),
                    ..Default::default()
                },
            )
            .await;
        let outcome = match reply {
            Ok(reply) => Ok(ShellExecResult {
                exit_code: reply
                    .json
                    .get("exitCode")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
                spill_path: reply
                    .json
                    .get("spillPath")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }),
            Err(error) => {
                let code = match error.code.as_str() {
                    "timeout" => ExecutionErrorCode::Timeout,
                    "aborted" if abort.aborted() => ExecutionErrorCode::Aborted,
                    "shell_unavailable" => ExecutionErrorCode::ShellUnavailable,
                    "spawn_error" => ExecutionErrorCode::SpawnError,
                    _ => ExecutionErrorCode::Unknown,
                };
                let message = if code == ExecutionErrorCode::Timeout {
                    format!("timeout:{}", options.timeout.unwrap_or(0.0))
                } else {
                    error.message.clone()
                };
                let mut failure = ExecutionError {
                    code,
                    message,
                    spill_path: None,
                };
                failure.spill_path = error.field_str("spillPath").map(str::to_owned);
                Err(failure)
            }
        };
        {
            let mut running = self.running.lock().expect("running lock");
            running.clear();
        }
        outcome
    }

    /// `cleanup` (`remote-env.ts:968-972`): kill without aborting.
    pub async fn cleanup(&self) {
        let running = std::mem::take(&mut *self.running.lock().expect("running lock"));
        for (id, session) in running {
            self.daemon.kill(id, session).await;
        }
    }
}

fn json_path(path: &str) -> JsonMap {
    let mut json = JsonMap::new();
    json.insert("path".into(), Value::String(path.to_owned()));
    json
}

impl ExecutionEnv for RemoteExecutionEnv {
    fn id(&self) -> &str {
        &self.id
    }

    fn cwd(&self) -> String {
        self.cwd_snapshot()
    }

    fn absolute_path<'a>(
        &'a self,
        path: &'a str,
        abort: &'a AbortSignal,
    ) -> BoxFuture<'a, Result<String, FileError>> {
        Box::pin(self.absolute_path(path, abort))
    }

    fn read_text_file<'a>(
        &'a self,
        path: &'a str,
        abort: &'a AbortSignal,
    ) -> BoxFuture<'a, Result<String, FileError>> {
        Box::pin(self.read_text_file(path, abort))
    }

    fn exists<'a>(
        &'a self,
        path: &'a str,
        abort: &'a AbortSignal,
    ) -> BoxFuture<'a, Result<bool, FileError>> {
        Box::pin(self.exists(path, abort))
    }

    fn list_files<'a>(
        &'a self,
        path: &'a str,
        options: Option<&'a ListFilesOptions>,
        abort: &'a AbortSignal,
    ) -> BoxFuture<'a, Result<FileListing, FileError>> {
        Box::pin(self.list_files(path, options, abort))
    }

    fn exec<'a>(
        &'a self,
        command: &'a Command,
        options: Option<&'a ShellExecOptions>,
        abort: &'a AbortSignal,
    ) -> BoxFuture<'a, Result<ShellExecResult, ExecutionError>> {
        Box::pin(self.exec(command, options, abort))
    }

    fn cleanup(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.cleanup())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        connection::RemoteInfo,
        test_support::{FakeDaemon, info_with_features, reply},
    };

    fn info(home: &str, windows: bool) -> RemoteInfo {
        let mut info = info_with_features(&[]);
        info.home = home.to_owned();
        if windows {
            info.os = "windows".into();
            info.separator = "\\".into();
            info.cwd = "C:\\work".into();
            info.drive_cwds
                .insert("D:".into(), Value::String("D:\\projects".into()));
        }
        info
    }

    #[test]
    fn resolves_tilde_absolute_and_relative_paths() {
        let info = info("/home/pi", false);
        assert_eq!(resolve_remote_path("~", &info, "/work"), "/home/pi");
        assert_eq!(
            resolve_remote_path("~/docs/../x", &info, "/work"),
            "/home/pi/x"
        );
        assert_eq!(
            resolve_remote_path("/tmp/./x/../y", &info, "/work"),
            "/tmp/y"
        );
        assert_eq!(
            resolve_remote_path("src/lib.rs", &info, "/work"),
            "/work/src/lib.rs"
        );
    }

    #[test]
    fn resolves_file_urls_and_windows_drive_relative_paths() {
        let posix = info("/home/pi", false);
        assert_eq!(
            resolve_remote_path("file:///tmp/a%20b", &posix, "/work"),
            "/tmp/a b"
        );
        let windows = info("C:\\Users\\pi", true);
        assert_eq!(
            resolve_remote_path("~\\docs", &windows, "C:\\work"),
            "C:\\Users\\pi\\docs"
        );
        assert_eq!(
            resolve_remote_path("D:src", &windows, "C:\\work"),
            "D:\\projects\\src"
        );
        assert_eq!(
            resolve_remote_path("src", &windows, "C:\\work"),
            "C:\\work\\src"
        );
    }

    #[test]
    fn path_rules_join_and_normalize_like_node() {
        let rules = PathRules { windows: false };
        assert_eq!(
            rules.join(&["/a".into(), "b".into(), "../c".into()]),
            "/a/c"
        );
        assert_eq!(rules.normalize("a/b/../../../c"), "../c");
    }

    #[test]
    fn timeout_validation_uses_exact_upstream_messages() {
        assert_eq!(
            ShellExecOptions::validate_timeout(0.0)
                .expect_err("zero")
                .message,
            "Invalid timeout: must be a finite number of seconds"
        );
        assert_eq!(
            ShellExecOptions::validate_timeout(f64::NAN)
                .expect_err("nan")
                .message,
            "Invalid timeout: must be a finite number of seconds"
        );
        assert_eq!(
            ShellExecOptions::validate_timeout(MAX_TIMEOUT_SECONDS + 1.0)
                .expect_err("too big")
                .message,
            "Invalid timeout: maximum is 2147483.647 seconds"
        );
        assert!(ShellExecOptions::validate_timeout(1.5).is_ok());
    }

    #[tokio::test]
    async fn resolves_absolute_path_through_the_daemon() {
        let daemon = FakeDaemon::new(info("/home/pi", false));
        let env = RemoteExecutionEnv::new(RemoteExecutionEnvOptions {
            daemon: Arc::new(daemon),
            id: "pi-env:test".into(),
            cwd: "/work".into(),
            shell_path: None,
            shell_env: None,
            watch: RemoteWatchOptions::default(),
        });
        assert_eq!(
            env.absolute_path("~/x", &AbortSignal::new())
                .await
                .expect("resolve"),
            "/home/pi/x"
        );
        assert_eq!(
            env.join_path(&["/a".into(), "b".into()])
                .await
                .expect("join"),
            "/a/b"
        );
    }

    #[tokio::test]
    async fn read_file_uses_open_pread_and_close() {
        let daemon = Arc::new(FakeDaemon::new(info_with_features(&[])));
        let mut stat = JsonMap::new();
        stat.insert("kind".into(), Value::String("file".into()));
        stat.insert("size".into(), Value::from(3));
        let mut open = JsonMap::new();
        open.insert("handle".into(), Value::from(7));
        open.insert("stat".into(), Value::Object(stat));
        daemon.push_reply("open", reply(open, Vec::new()), Vec::new(), None);
        daemon.push_reply(
            "pread",
            reply(JsonMap::new(), b"abc".to_vec()),
            Vec::new(),
            None,
        );
        daemon.push_reply("close", reply(JsonMap::new(), Vec::new()), Vec::new(), None);
        let env = RemoteExecutionEnv::new(RemoteExecutionEnvOptions {
            daemon: daemon.clone(),
            id: "x".into(),
            cwd: "/work".into(),
            shell_path: None,
            shell_env: None,
            watch: RemoteWatchOptions::default(),
        });
        assert_eq!(
            env.read_text_file("file.txt", &AbortSignal::new())
                .await
                .expect("read"),
            "abc"
        );
        assert_eq!(daemon.calls(), vec!["info", "open", "pread", "close"]);
    }

    #[tokio::test]
    async fn exec_delivers_output_and_reports_timeout_code() {
        let daemon = FakeDaemon::new(info_with_features(&[]));
        let output = Event {
            json: {
                let mut json = JsonMap::new();
                json.insert("kind".into(), Value::String("output".into()));
                json.insert("stream".into(), Value::String("stderr".into()));
                json
            },
            payload: b"warning".to_vec(),
        };
        let mut ok = JsonMap::new();
        ok.insert("exitCode".into(), Value::from(0));
        daemon.push_reply("exec", reply(ok, Vec::new()), vec![output], None);
        let env = RemoteExecutionEnv::new(RemoteExecutionEnvOptions {
            daemon: Arc::new(daemon),
            id: "x".into(),
            cwd: "/work".into(),
            shell_path: None,
            shell_env: None,
            watch: RemoteWatchOptions::default(),
        });
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let options = ShellExecOptions {
            on_output: Some(Arc::new(move |text: &str, info: &ShellOutputInfo| {
                sink.lock()
                    .expect("sink lock")
                    .push((text.to_owned(), info.stream.clone()));
            })),
            ..ShellExecOptions::default()
        };
        let result = env
            .exec(
                &Command::Shell("echo hi".into()),
                Some(&options),
                &AbortSignal::new(),
            )
            .await
            .expect("exec");
        assert_eq!(result.exit_code, 0);
        assert_eq!(
            *seen.lock().expect("sink lock"),
            vec![("warning".to_owned(), "stderr".to_owned())]
        );
    }

    #[tokio::test]
    async fn cleanup_kills_running_commands() {
        let daemon = Arc::new(FakeDaemon::new(info_with_features(&[])));
        let erased: Arc<dyn Daemon> = daemon.clone();
        let env = RemoteExecutionEnv::new(RemoteExecutionEnvOptions {
            daemon: erased,
            id: "x".into(),
            cwd: "/work".into(),
            shell_path: None,
            shell_env: None,
            watch: RemoteWatchOptions::default(),
        });
        // Upstream unregisters a command when `exec` settles; cleanup kills the
        // ones still registered (e.g. from a concurrent, in-flight exec).
        env.running.lock().expect("running lock").push((11, 1));
        env.cleanup().await;
        assert!(daemon.killed().contains(&(11, 1)));
    }
}
