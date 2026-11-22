//! Node-style error mapping and the error surfaces shared by the crate.
//!
//! Ground truth: `packages/env/src/errors.ts:1-34` (`abortResult`,
//! `toFileError`) and `packages/env/src/connection.ts:29-44,134-150,167-179`
//! (`Json`, `RemoteError`, `lost`/`closed`/`aborted` helpers). The PTY/process
//! execution error codes come from
//! `packages/env/src/remote-env.ts:855-966` (`ExecutionError`) and
//! `packages/env/src/ssh.ts:39-55` (the SSH error classes).
//!
//! Upstream throws class instances with a `message` and a Node-style `code`;
//! the port uses plain structs. `Display` writes only the message, matching
//! `Error.message`, while [`FileErrorCode::as_str`] and
//! [`ExecutionErrorCode::as_str`] expose the code used by callers.

use std::{
    error::Error,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use serde_json::Value;

use crate::JsonMap;

/// A cancellation token, the Rust counterpart of Node's `AbortSignal`
/// (`connection.ts:152-161`, `errors.ts:4-6`).
///
/// Upstream listeners fire synchronously; here callers check [`Self::aborted`]
/// at the checkpoints the daemon protocol defines (before and after a file
/// operation, between reads). Mid-flight cancellation is driven by the host,
/// not by a timer — see the deviations in `WIRING.md`.
#[derive(Clone, Default, Debug)]
pub struct AbortSignal {
    aborted: Arc<AtomicBool>,
}

impl AbortSignal {
    /// An uncancelled signal.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether cancellation was requested.
    #[must_use]
    pub fn aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }

    /// Request cancellation. Idempotent, like `AbortController.abort()`.
    pub fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
    }
}

/// A failure reported by the daemon, with its Node-style code
/// (`connection.ts:31-44`). `fields` is the raw error frame so callers can
/// read extra fields such as `spillPath`.
#[derive(Clone, Debug)]
pub struct RemoteError {
    /// The Node/daemon code, e.g. `ENOENT`, `spawn_error`, `not_supported`.
    pub code: String,
    /// Human-readable diagnostics.
    pub message: String,
    /// The path the failure applies to, when the daemon supplied one.
    pub path: Option<String>,
    /// The complete JSON object of the error frame.
    pub fields: JsonMap,
    /// Whether the *connection* was lost rather than the daemon refusing the
    /// request (`connection.ts:134-142`). Such a failure may be retried.
    pub lost: bool,
}

impl RemoteError {
    /// Construct an error with a code and message.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        let code = code.into();
        let message = message.into();
        let mut fields = JsonMap::new();
        fields.insert("code".into(), Value::String(code.clone()));
        fields.insert("message".into(), Value::String(message.clone()));
        Self {
            code,
            message,
            path: None,
            fields,
            lost: false,
        }
    }

    /// A connection-lost error (`lost(...)`, `connection.ts:135-137`).
    #[must_use]
    pub fn lost(message: impl Into<String>) -> Self {
        let mut error = Self::new("unknown", message);
        error.lost = true;
        error.fields.insert("lost".into(), Value::Bool(true));
        error
    }

    /// From an error frame's JSON (`new RemoteError(fields)`,
    /// `connection.ts:37-43`).
    #[must_use]
    pub fn from_fields(fields: JsonMap) -> Self {
        let message = fields
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Remote operation failed")
            .to_owned();
        let code = fields
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let path = fields
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let lost = fields.get("lost").and_then(Value::as_bool).unwrap_or(false);
        Self {
            code,
            message,
            path,
            fields,
            lost,
        }
    }

    /// A string field of the raw error object, e.g. `spillPath`
    /// (`remote-env.ts:958`).
    #[must_use]
    pub fn field_str(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(Value::as_str)
    }

    /// Whether an error means the connection was lost, not that the daemon
    /// refused a request (`isConnectionLost`, `connection.ts:139-142`).
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.lost
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for RemoteError {}

/// Whether an error means the connection was lost (`connection.ts:139-142`).
#[must_use]
pub fn is_connection_lost(error: &RemoteError) -> bool {
    error.is_lost()
}

/// The `FileError` codes produced by `toFileError` (`errors.ts:9-33`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileErrorCode {
    Aborted,
    NotFound,
    PermissionDenied,
    NotDirectory,
    IsDirectory,
    Invalid,
    Unknown,
    NotSupported,
}

impl FileErrorCode {
    /// The wire-facing code string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aborted => "aborted",
            Self::NotFound => "not_found",
            Self::PermissionDenied => "permission_denied",
            Self::NotDirectory => "not_directory",
            Self::IsDirectory => "is_directory",
            Self::Invalid => "invalid",
            Self::Unknown => "unknown",
            Self::NotSupported => "not_supported",
        }
    }
}

impl fmt::Display for FileErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A file-operation failure, the Rust counterpart of `FileError`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileError {
    /// The mapped code.
    pub code: FileErrorCode,
    /// Upstream's `message`.
    pub message: String,
    /// The path the failure applies to, when known.
    pub path: Option<String>,
}

impl FileError {
    /// Construct a failure with an explicit code.
    #[must_use]
    pub fn new(code: FileErrorCode, message: impl Into<String>, path: Option<&str>) -> Self {
        Self {
            code,
            message: message.into(),
            path: path.map(str::to_owned),
        }
    }

    /// An `invalid` failure (e.g. an unsupported file type,
    /// `remote-env.ts:72-81`).
    #[must_use]
    pub fn invalid(message: impl Into<String>, path: Option<&str>) -> Self {
        Self::new(FileErrorCode::Invalid, message, path)
    }

    /// An `aborted` failure (`abortResult`, `errors.ts:4-6`).
    #[must_use]
    pub fn aborted(path: Option<&str>) -> Self {
        Self::new(FileErrorCode::Aborted, "aborted", path)
    }

    /// An `unknown` failure from a non-remote error (`errors.ts:11-14`).
    #[must_use]
    pub fn unknown(message: impl Into<String>, path: Option<&str>) -> Self {
        Self::new(FileErrorCode::Unknown, message, path)
    }
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for FileError {}

/// `abortResult` (`errors.ts:4-6`): `Some(Err(aborted))` when `signal` is
/// cancelled, otherwise `None`.
#[must_use]
pub fn abort_result<T>(signal: &AbortSignal, path: Option<&str>) -> Option<Result<T, FileError>> {
    if signal.aborted() {
        Some(Err(FileError::aborted(path)))
    } else {
        None
    }
}

/// `toFileError` (`errors.ts:9-33`): the Node error code to `FileError` code
/// mapping used by `NodeExecutionEnv`.
#[must_use]
pub fn to_file_error(error: &RemoteError, fallback_path: Option<&str>) -> FileError {
    let path = error.path.as_deref().or(fallback_path).map(str::to_owned);
    let code = match error.code.as_str() {
        "aborted" => FileErrorCode::Aborted,
        "ENOENT" => FileErrorCode::NotFound,
        "EACCES" | "EPERM" => FileErrorCode::PermissionDenied,
        "ENOTDIR" => FileErrorCode::NotDirectory,
        "EISDIR" => FileErrorCode::IsDirectory,
        "EINVAL" | "SYMLINK" | "NOT_REGULAR" => FileErrorCode::Invalid,
        _ => FileErrorCode::Unknown,
    };
    FileError {
        code,
        message: error.message.clone(),
        path,
    }
}

/// The `ExecutionError` codes used by `exec` (`remote-env.ts:855-966`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionErrorCode {
    Aborted,
    Timeout,
    CallbackError,
    ShellUnavailable,
    SpawnError,
    Unknown,
}

impl ExecutionErrorCode {
    /// The wire-facing code string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aborted => "aborted",
            Self::Timeout => "timeout",
            Self::CallbackError => "callback_error",
            Self::ShellUnavailable => "shell_unavailable",
            Self::SpawnError => "spawn_error",
            Self::Unknown => "unknown",
        }
    }
}

/// An execution failure (`ExecutionError`, `remote-env.ts:855-966`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionError {
    /// The failure code.
    pub code: ExecutionErrorCode,
    /// Upstream's `message`.
    pub message: String,
    /// A spill file path, when the daemon wrote oversized output to one.
    pub spill_path: Option<String>,
}

impl ExecutionError {
    /// Construct an execution failure.
    #[must_use]
    pub fn new(code: ExecutionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            spill_path: None,
        }
    }
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for ExecutionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_node_error_codes_exactly() {
        let cases = [
            ("aborted", FileErrorCode::Aborted),
            ("ENOENT", FileErrorCode::NotFound),
            ("EACCES", FileErrorCode::PermissionDenied),
            ("EPERM", FileErrorCode::PermissionDenied),
            ("ENOTDIR", FileErrorCode::NotDirectory),
            ("EISDIR", FileErrorCode::IsDirectory),
            ("EINVAL", FileErrorCode::Invalid),
            ("SYMLINK", FileErrorCode::Invalid),
            ("NOT_REGULAR", FileErrorCode::Invalid),
            ("EIO", FileErrorCode::Unknown),
        ];
        for (code, expected) in cases {
            let error = RemoteError::new(code, "boom");
            let mapped = to_file_error(&error, None);
            assert_eq!(mapped.code, expected);
            assert_eq!(mapped.message, "boom");
        }
    }

    #[test]
    fn remote_error_path_wins_over_fallback() {
        let mut fields = JsonMap::new();
        fields.insert("code".into(), Value::String("ENOENT".into()));
        fields.insert("message".into(), Value::String("missing".into()));
        fields.insert("path".into(), Value::String("/remote/x".into()));
        let error = RemoteError::from_fields(fields);
        let mapped = to_file_error(&error, Some("/fallback"));
        assert_eq!(mapped.code, FileErrorCode::NotFound);
        assert_eq!(mapped.path.as_deref(), Some("/remote/x"));
    }

    #[test]
    fn connection_lost_flag_survives_the_error_surface() {
        let error = RemoteError::lost("pi-env connection lost");
        assert!(is_connection_lost(&error));
        assert_eq!(error.code, "unknown");
        assert_eq!(error.fields.get("lost"), Some(&Value::Bool(true)));
    }

    #[test]
    fn abort_result_is_none_until_signalled() {
        let signal = AbortSignal::new();
        let none: Option<Result<u8, FileError>> = abort_result(&signal, Some("/x"));
        assert!(none.is_none());
        signal.abort();
        let some: Option<Result<u8, FileError>> = abort_result(&signal, Some("/x"));
        let failure = some.expect("aborted").expect_err("aborted");
        assert_eq!(failure.code, FileErrorCode::Aborted);
        assert_eq!(failure.path.as_deref(), Some("/x"));
        assert_eq!(failure.to_string(), "aborted");
    }

    #[test]
    fn file_error_code_strings_match_upstream() {
        assert_eq!(FileErrorCode::NotSupported.as_str(), "not_supported");
        assert_eq!(FileErrorCode::Invalid.to_string(), "invalid");
        assert_eq!(ExecutionErrorCode::CallbackError.as_str(), "callback_error");
    }
}
