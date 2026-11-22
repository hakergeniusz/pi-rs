//! Shared file types and `listFiles` over the daemon.
//!
//! Ground truth: `packages/env/src/list-files.ts:1-102` (`KINDS`,
//! `listFilesOver`, `LocalExecutionEnv`) and the `pi-durable/env` record types
//! the package imports at `:2-13` (`FileInfo`, `FileKind`, `FileListing`,
//! `FileListEntry`, `ListFilesOptions`, `FileChange`, `WatchTarget`;
//! `durable` is out of scope for the port, so the shapes live here). The
//! `RemoteFileInfo` conversion mirrors `remote-env.ts:63-81` (`toInfo`).
//!
//! The wire records are a kind byte (`f`/`d`/`l`), the UTF-8 path and a NUL
//! (`list-files.ts:58-67`). Validation of `maxEntries`/`maxBytes`/`timeoutMs`
//! uses JS's safe-integer bound rather than a plain `u64` check, matching
//! upstream's `Number.isSafeInteger`.

use std::sync::Arc;

use serde_json::Value;

use crate::{
    JsonMap,
    connection::{BoxFuture, Daemon, RemoteInfo},
    env::{RemoteExecutionEnv, RemoteExecutionEnvOptions},
    errors::{AbortSignal, FileError, FileErrorCode, abort_result, to_file_error},
};

/// The three kinds reported by `listFiles` (`list-files.ts:18`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// `f` — a regular file.
    File,
    /// `d` — a directory.
    Directory,
    /// `l` — a symbolic link.
    Symlink,
}

impl FileKind {
    /// Parse a daemon kind byte; unknown bytes are ignored by the listing.
    #[must_use]
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            b'f' => Some(Self::File),
            b'd' => Some(Self::Directory),
            b'l' => Some(Self::Symlink),
            _ => None,
        }
    }

    /// The upstream string spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
        }
    }
}

/// A file's metadata, shaped like `pi-durable/env`'s `FileInfo`.
#[derive(Clone, Debug, PartialEq)]
pub struct FileInfo {
    /// Basename of the file.
    pub name: String,
    /// Absolute path in the remote system's spelling.
    pub path: String,
    /// File kind.
    pub kind: FileKind,
    /// Size in bytes.
    pub size: u64,
    /// Modification time in milliseconds (fractional, like upstream).
    pub mtime_ms: f64,
}

/// One entry of a `listFiles` reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileListEntry {
    /// Path relative to the listed directory, in the daemon's spelling.
    pub path: String,
    /// Entry kind.
    pub kind: FileKind,
}

/// A `listFiles` result (`FileListing`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileListing {
    /// The entries found, in daemon order.
    pub entries: Vec<FileListEntry>,
    /// Whether the daemon stopped before reaching the whole tree.
    pub truncated: bool,
}

/// Options for `listFiles` (`ListFilesOptions`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListFilesOptions {
    /// Include hidden files.
    pub hidden: Option<bool>,
    /// Apply `.gitignore` rules.
    pub gitignore: Option<bool>,
    /// Additional exclusion patterns.
    pub exclude: Option<Vec<String>>,
    /// Maximum number of entries.
    pub max_entries: Option<u64>,
    /// Maximum number of bytes to scan.
    pub max_bytes: Option<u64>,
    /// Daemon-side timeout in milliseconds.
    pub timeout_ms: Option<u64>,
}

impl ListFilesOptions {
    fn to_json(&self, path: &str) -> Result<JsonMap, FileError> {
        for (name, value) in [
            ("maxEntries", self.max_entries),
            ("maxBytes", self.max_bytes),
            ("timeoutMs", self.timeout_ms),
        ] {
            if let Some(value) = value
                && value > JS_SAFE_INTEGER
            {
                return Err(FileError::new(
                    FileErrorCode::Invalid,
                    format!("{name} must be a non-negative safe integer"),
                    Some(path),
                ));
            }
        }
        let mut json = JsonMap::new();
        json.insert("path".into(), Value::String(path.to_owned()));
        if let Some(value) = self.hidden {
            json.insert("hidden".into(), Value::Bool(value));
        }
        if let Some(value) = self.gitignore {
            json.insert("gitignore".into(), Value::Bool(value));
        }
        if let Some(value) = &self.exclude {
            json.insert(
                "exclude".into(),
                Value::Array(value.iter().cloned().map(Value::String).collect()),
            );
        }
        if let Some(value) = self.max_entries {
            json.insert("maxEntries".into(), Value::from(value));
        }
        if let Some(value) = self.max_bytes {
            json.insert("maxBytes".into(), Value::from(value));
        }
        if let Some(value) = self.timeout_ms {
            json.insert("timeoutMs".into(), Value::from(value));
        }
        Ok(json)
    }
}

/// `Number.MAX_SAFE_INTEGER` — upstream rejects larger integers.
const JS_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// A watch target (`WatchTarget`). Paths must be resolved before reaching the
/// daemon, which `RemoteExecutionEnv::watch` does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchTarget {
    /// Absolute path to watch.
    pub path: String,
    /// Watch descendants recursively.
    pub recursive: bool,
    /// Exclude hidden entries.
    pub exclude_hidden: bool,
    /// Exclude these names.
    pub exclude_names: Vec<String>,
}

impl WatchTarget {
    /// The daemon's watch-target JSON (`watch.ts:91-95`).
    #[must_use]
    pub fn to_json(&self) -> JsonMap {
        let mut json = JsonMap::new();
        json.insert("path".into(), Value::String(self.path.clone()));
        json.insert("recursive".into(), Value::Bool(self.recursive));
        let mut exclude = JsonMap::new();
        exclude.insert("hidden".into(), Value::Bool(self.exclude_hidden));
        exclude.insert(
            "names".into(),
            Value::Array(
                self.exclude_names
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
        json.insert("exclude".into(), Value::Object(exclude));
        json
    }
}

/// A change delivered to a watcher (`FileChange`).
#[derive(Clone, Debug, PartialEq)]
pub enum FileChange {
    /// Changes were missed; rescan (`watch.ts:17`).
    Overflow,
    /// The changed paths.
    Paths(Vec<String>),
    /// Watching failed irrecoverably (`watch.ts:110-114`).
    Failed {
        /// The mapped file error.
        error: FileError,
    },
}

/// An info record reported by the daemon (`RemoteFileInfo`,
/// `remote-env.ts:64`).
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteFileInfo {
    /// Name as reported by the daemon.
    pub name: String,
    /// Kind spelling, including the unsupported `other`.
    pub kind: String,
    /// Size in bytes.
    pub size: u64,
    /// Seconds part of mtime.
    pub mtime_sec: f64,
    /// Nanoseconds part of mtime.
    pub mtime_nsec: f64,
}

impl RemoteFileInfo {
    /// Parse a daemon info object.
    #[must_use]
    pub fn from_json(json: &JsonMap) -> Self {
        let text = |key: &str| -> String {
            json.get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        };
        let number = |key: &str| -> f64 { json.get(key).and_then(Value::as_f64).unwrap_or(0.0) };
        Self {
            name: text("name"),
            kind: text("kind"),
            size: number("size") as u64,
            mtime_sec: number("mtimeSec"),
            mtime_nsec: number("mtimeNsec"),
        }
    }
}

/// `toInfo` (`remote-env.ts:72-81`): map a daemon record to a `FileInfo`,
/// rejecting `other` kinds as upstream does.
pub fn to_info(path: &str, remote: &RemoteFileInfo, windows: bool) -> Result<FileInfo, FileError> {
    let kind = match remote.kind.as_str() {
        "file" => FileKind::File,
        "directory" => FileKind::Directory,
        "symlink" => FileKind::Symlink,
        _ => return Err(FileError::invalid("Unsupported file type", Some(path))),
    };
    let name = path
        .rsplit(|character: char| {
            if windows {
                character == '\\' || character == '/'
            } else {
                character == '/'
            }
        })
        .next()
        .unwrap_or(path)
        .to_owned();
    let mtime_ms = remote.mtime_sec * 1000.0 + remote.mtime_nsec / 1e6;
    Ok(FileInfo {
        name,
        path: path.to_owned(),
        kind,
        size: remote.size,
        mtime_ms,
    })
}

/// `listFilesOver` (`list-files.ts:24-72`): list an absolute path through the
/// daemon, with upstream's feature gate and exact error strings.
pub fn list_files_over<'a>(
    daemon: &'a dyn Daemon,
    path: &str,
    options: Option<&ListFilesOptions>,
    abort: &'a AbortSignal,
) -> BoxFuture<'a, Result<FileListing, FileError>> {
    let path = path.to_owned();
    let options = options.cloned();
    Box::pin(async move {
        let options = options.unwrap_or_default();
        let json = options.to_json(&path)?;
        if let Some(failure) = abort_result(abort, Some(&path)) {
            return failure;
        }
        let info: RemoteInfo = daemon
            .info(Some(abort.clone()))
            .await
            .map_err(|error| to_file_error(&error, Some(&path)))?;
        if !info.has_feature("listFiles") {
            return Err(FileError::new(
                FileErrorCode::NotSupported,
                "The pi-env daemon does not list files; update it",
                Some(&path),
            ));
        }
        let reply = daemon
            .request(
                "listFiles",
                json,
                crate::connection::RequestOptions {
                    abort: Some(abort.clone()),
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| to_file_error(&error, Some(&path)))?;
        if let Some(failure) = abort_result(abort, Some(&path)) {
            return failure;
        }
        let mut entries = Vec::new();
        let mut start = 0;
        while start < reply.payload.len() {
            let Some(offset) = reply.payload[start..].iter().position(|byte| *byte == 0) else {
                break;
            };
            let end = start + offset;
            if let Some(kind) = FileKind::from_byte(reply.payload[start]) {
                let entry_path =
                    String::from_utf8_lossy(&reply.payload[start + 1..end]).into_owned();
                entries.push(FileListEntry {
                    path: entry_path,
                    kind,
                });
            }
            start = end + 1;
        }
        Ok(FileListing {
            entries,
            truncated: reply.json.get("truncated").and_then(Value::as_bool) == Some(true),
        })
    })
}

/// Options for [`LocalExecutionEnv`] (`LocalExecutionEnvOptions`).
pub struct LocalExecutionEnvOptions {
    /// A connection to this machine's daemon, used for `listFiles`.
    pub daemon: Arc<dyn Daemon>,
    /// File-namespace id.
    pub id: String,
    /// Working directory.
    pub cwd: String,
}

/// `NodeExecutionEnv` with `listFiles` through a local daemon, so local and
/// remote environments list files with the same code
/// (`list-files.ts:79-102`). The port has no `NodeExecutionEnv`; the local
/// environment delegates to a [`RemoteExecutionEnv`] pointed at the local
/// daemon — documented in `WIRING.md`.
pub struct LocalExecutionEnv {
    inner: RemoteExecutionEnv,
}

impl LocalExecutionEnv {
    /// Wrap a local daemon connection.
    #[must_use]
    pub fn new(options: LocalExecutionEnvOptions) -> Self {
        Self {
            inner: RemoteExecutionEnv::new(RemoteExecutionEnvOptions {
                daemon: options.daemon,
                id: options.id,
                cwd: options.cwd,
                shell_path: None,
                shell_env: None,
                watch: crate::watch::RemoteWatchOptions::default(),
            }),
        }
    }

    /// The underlying environment.
    #[must_use]
    pub fn inner(&self) -> &RemoteExecutionEnv {
        &self.inner
    }

    /// `local.ts:93-101`: resolve the path, then list it over the daemon.
    pub async fn list_files(
        &self,
        path: &str,
        options: Option<&ListFilesOptions>,
        abort: &AbortSignal,
    ) -> Result<FileListing, FileError> {
        let resolved = self.inner.absolute_path(path, abort).await?;
        list_files_over(self.inner.daemon(), &resolved, options, abort).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeDaemon, info_with_features};

    #[test]
    fn kind_bytes_match_upstream() {
        assert_eq!(FileKind::from_byte(b'f'), Some(FileKind::File));
        assert_eq!(FileKind::from_byte(b'd'), Some(FileKind::Directory));
        assert_eq!(FileKind::from_byte(b'l'), Some(FileKind::Symlink));
        assert_eq!(FileKind::from_byte(b'?'), None);
        assert_eq!(FileKind::File.as_str(), "file");
    }

    #[test]
    fn to_info_maps_mtime_and_rejects_other_kinds() {
        let mut json = JsonMap::new();
        json.insert("kind".into(), Value::String("directory".into()));
        json.insert("size".into(), Value::from(9));
        json.insert("mtimeSec".into(), Value::from(100.0));
        json.insert("mtimeNsec".into(), Value::from(500_000.0));
        let info = to_info("/x/dir", &RemoteFileInfo::from_json(&json), false).expect("info");
        assert_eq!(info.name, "dir");
        assert_eq!(info.kind, FileKind::Directory);
        assert_eq!(info.size, 9);
        assert!((info.mtime_ms - 100_500.0).abs() < f64::EPSILON);
        json.insert("kind".into(), Value::String("other".into()));
        let failure =
            to_info("/x/other", &RemoteFileInfo::from_json(&json), false).expect_err("other");
        assert_eq!(failure.message, "Unsupported file type");
    }

    #[tokio::test]
    async fn list_files_parses_nul_records_and_truncated_flag() {
        let daemon = FakeDaemon::new(info_with_features(&["listFiles"]));
        let mut payload = Vec::new();
        payload.extend_from_slice(b"fmain.rs\0dtests\0lalias\0xignored\0");
        let mut reply = crate::connection::Reply {
            json: JsonMap::new(),
            payload,
            session: 1,
        };
        reply.json.insert("truncated".into(), Value::Bool(true));
        daemon.push_reply("listFiles", reply, Vec::new(), None);
        let result = list_files_over(&daemon, "/project", None, &AbortSignal::new())
            .await
            .expect("listing");
        assert!(result.truncated);
        assert_eq!(result.entries.len(), 3);
        assert_eq!(result.entries[0].path, "main.rs");
        assert_eq!(result.entries[0].kind, FileKind::File);
        assert_eq!(result.entries[1].kind, FileKind::Directory);
        assert_eq!(result.entries[2].kind, FileKind::Symlink);
        assert_eq!(
            daemon.calls(),
            vec!["info".to_owned(), "listFiles".to_owned()]
        );
    }

    #[tokio::test]
    async fn list_files_reports_missing_feature_with_exact_message() {
        let daemon = FakeDaemon::new(info_with_features(&[]));
        let failure = list_files_over(&daemon, "/project", None, &AbortSignal::new())
            .await
            .expect_err("not supported");
        assert_eq!(failure.code, FileErrorCode::NotSupported);
        assert_eq!(
            failure.message,
            "The pi-env daemon does not list files; update it"
        );
        assert_eq!(daemon.calls(), vec!["info".to_owned()]);
    }

    #[tokio::test]
    async fn list_files_validates_options_before_connecting() {
        let daemon = FakeDaemon::new(info_with_features(&["listFiles"]));
        let options = ListFilesOptions {
            max_entries: Some((1 << 53) + 1),
            ..Default::default()
        };
        let failure = list_files_over(&daemon, "/x", Some(&options), &AbortSignal::new())
            .await
            .expect_err("invalid");
        assert_eq!(
            failure.message,
            "maxEntries must be a non-negative safe integer"
        );
        assert!(daemon.calls().is_empty());
    }

    #[tokio::test]
    async fn list_files_honors_abort() {
        let daemon = FakeDaemon::new(info_with_features(&["listFiles"]));
        let abort = AbortSignal::new();
        abort.abort();
        let failure = list_files_over(&daemon, "/x", None, &abort)
            .await
            .expect_err("aborted");
        assert_eq!(failure.code, FileErrorCode::Aborted);
        assert!(daemon.calls().is_empty());
    }

    #[test]
    fn watch_target_serializes_exclusion_shape() {
        let target = WatchTarget {
            path: "/x".into(),
            recursive: true,
            exclude_hidden: true,
            exclude_names: vec!["node_modules".into()],
        };
        let json = target.to_json();
        let exclude = json
            .get("exclude")
            .and_then(Value::as_object)
            .expect("exclude");
        assert_eq!(exclude.get("hidden"), Some(&Value::Bool(true)));
        assert_eq!(
            exclude.get("names").and_then(Value::as_array).map(Vec::len),
            Some(1)
        );
    }
}
