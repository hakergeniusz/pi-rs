//! Crash log: best-effort persistence of fatal errors and uncaught panics.
//!
//! 1:1 port of `core/crash-log.ts` (170 lines). Upstream keeps a bounded
//! `crashes.json` history so the next launch can point the user at `/bug`
//! (`modes/interactive/interactive-mode.ts:1261-1267`) and so `/bug` can
//! attach the records (`modes/interactive/bug-report.ts:167`,
//! `core/bug-report.ts:183-186`). This module ports the record schema
//! (`:7-16`), the read/write lifecycle (`:21-45`, `:123-170`), and the
//! stack-trace extension matching (`:47-121`). The Rust panic hook stands in
//! for the `uncaughtException` listener upstream installs at
//! `interactive-mode.ts:4610-4612` and implements at `:4542-4562`; upstream's
//! crash-log module itself only persists records.
//!
//! # Deviations (deliberate, documented)
//!
//! * **State dir, not config dir** (`:21-23`): upstream writes
//!   `<agentDir>/crashes.json`; pi-rs keeps crash history in the XDG **state**
//!   root — the same role as the debug log (`config.rs:239-253`) — unless
//!   `$PI_CODING_AGENT_DIR` is set, when the layout collapses to upstream's
//!   single-directory layout like every other derived path.
//! * **Redaction** (upstream has none): [`redact_secrets`] scrubs bearer
//!   tokens, secret-looking `key=value`/`key: value` pairs, and common API-key
//!   shapes from `message` and `stack` before writing, so an error echoing a
//!   header or URL cannot leak a credential into `crashes.json`.
//! * **No `args` field**: upstream's `CrashRecord` (`:7-16`) has no CLI-args
//!   field; none was added, so the on-disk schema stays byte-compatible.
//! * **Injectable paths**: every I/O function takes an explicit `path` (as
//!   upstream's optional `path` parameters do) and [`crash_log_path_for`] is a
//!   pure resolver, so tests use a temp dir and never touch a real data root.
//! * **Idempotent hook install**: upstream removes its listener before adding
//!   it again; [`install_panic_hook`] is a no-op while a hook is installed.
//! * **`kind` stored as a string**: upstream's TS union is a type-level
//!   restriction only — `JSON.stringify` round-trips whatever string is in the
//!   object. Storing `String` preserves that round-trip for hand-edited logs;
//!   [`CrashKind`] is the typed input for records this binary writes.

use std::borrow::Cow;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::config;
use crate::session::store::{iso_to_ms, now_iso8601};

/// `MAX_CRASH_RECORDS` (`crash-log.ts:18`): only the newest five records are
/// kept.
const MAX_CRASH_RECORDS: usize = 5;
/// `MAX_AGE` (`crash-log.ts:19`): seven days, in milliseconds.
const MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// `crashLogPath`'s file name (`crash-log.ts:22`).
const CRASH_LOG_FILE_NAME: &str = "crashes.json";
/// Upstream's `VERSION` (`config.ts:607`): the package version stamped into
/// every record (`crash-log.ts:132`).
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `CrashRecord["kind"]` (`crash-log.ts:10`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashKind {
    /// A process-level uncaught exception (`interactive-mode.ts:4558`).
    UncaughtException,
    /// A fatal runtime error during startup or a session operation
    /// (`interactive-mode.ts:2252`).
    FatalError,
}

impl CrashKind {
    /// The JSON string written into `CrashRecord.kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UncaughtException => "uncaught_exception",
            Self::FatalError => "fatal_error",
        }
    }
}

/// `CrashRecord` (`crash-log.ts:7-16`). Field order and null-vs-omitted
/// behavior mirror upstream's object literal and `JSON.stringify` exactly:
/// `stack` and `sessionFile` are always present (possibly `null`), while
/// `notified` is omitted until it is set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CrashRecord {
    pub timestamp: String,
    pub version: String,
    pub kind: String,
    pub message: String,
    pub stack: Option<String>,
    #[serde(rename = "sessionFile")]
    pub session_file: Option<String>,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notified: Option<bool>,
}

/// The argument object of `recordCrash` (`crash-log.ts:125`), borrowed so the
/// caller keeps ownership of the error text and session context.
#[derive(Debug, Clone, Copy)]
pub struct CrashInput<'a> {
    pub kind: CrashKind,
    pub message: &'a str,
    pub stack: Option<&'a str>,
    pub session_file: Option<&'a str>,
    pub cwd: &'a str,
}

/// `crashLogPath` (`crash-log.ts:21-23`), with the XDG state-role deviation.
///
/// With `$PI_CODING_AGENT_DIR` set the layout is upstream's
/// `<agentDir>/crashes.json`; otherwise the file hangs off the state root.
pub fn crash_log_path_for(agent_override: Option<&Path>, state_root: &Path) -> PathBuf {
    match agent_override {
        Some(dir) => dir.join(CRASH_LOG_FILE_NAME),
        None => state_root.join(CRASH_LOG_FILE_NAME),
    }
}

/// `crashLogPath()` with pi-rs's resolved roots.
pub fn crash_log_path() -> PathBuf {
    crash_log_path_for(
        config::agent_dir_override().as_deref(),
        &config::get_state_dir(),
    )
}

/// `record_from_value`'s lenient field reader: upstream's `readCrashLog`
/// filters only on a string `timestamp` and `message`, so a malformed record
/// is kept with defaults rather than dropped (`crash-log.ts:29-35`).
fn record_from_value(value: &serde_json::Value) -> CrashRecord {
    CrashRecord {
        timestamp: value
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        version: value
            .get("version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        kind: value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        message: value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        stack: value
            .get("stack")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        session_file: value
            .get("sessionFile")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        cwd: value
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        notified: value.get("notified").and_then(serde_json::Value::as_bool),
    }
}

/// `readCrashLog` (`crash-log.ts:25-40`): an unreadable, non-array, or
/// malformed file yields an empty list; only entries with a string `timestamp`
/// and `message` survive.
pub fn read_crash_log(path: &Path) -> Vec<CrashRecord> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Vec::new();
    };
    let Some(entries) = value.as_array() else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|entry| {
            entry
                .get("timestamp")
                .is_some_and(serde_json::Value::is_string)
                && entry
                    .get("message")
                    .is_some_and(serde_json::Value::is_string)
        })
        .map(record_from_value)
        .collect()
}

/// `writeCrashLog` (`crash-log.ts:42-45`): `mkdir -p` the parent, then write
/// two-space-indented JSON with a trailing newline.
fn write_crash_log(records: &[CrashRecord], path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut json = serde_json::to_string_pretty(records)?;
    json.push('\n');
    std::fs::write(path, json)
}

/// `(?i)\b(api[_-]?key|token|…)\b(\s*[:=]\s*)(["']?)(value)` — the value part
/// of a secret-looking assignment (`crash_log.rs` redaction deviation).
static SECRET_ASSIGNMENT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r#"(?i)\b(api[_-]?key|apikey|token|access[_-]?token|secret|password|passwd|authorization|auth[_-]?token)\b(\s*[:=]\s*)(["']?)([^\s"',;&}\)]+)"#,
    )
    .expect("secret assignment regex")
});
/// `Bearer <token>` in an error message or stack frame.
static BEARER_TOKEN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{8,}")
        .expect("bearer token regex")
});
/// Common provider key shapes (OpenAI/Anthropic, GitHub, Slack).
const KEY_SHAPE: &str = r"\b(?:sk-ant-|sk-|gh[pousr]_|xox[baprs]-)[A-Za-z0-9_-]{8,}";
static KEY_SHAPES: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(KEY_SHAPE).expect("key shape regex"));

/// Scrub credential-shaped substrings before persisting. Deliberate pi-rs
/// deviation (upstream writes error text verbatim); see the module doc.
pub fn redact_secrets(text: &str) -> String {
    let text = BEARER_TOKEN.replace_all(text, "Bearer [REDACTED]");
    let text = KEY_SHAPES.replace_all(&text, "[REDACTED]");
    SECRET_ASSIGNMENT
        .replace_all(&text, "$1$2$3[REDACTED]")
        .into_owned()
}

/// `recordCrash` (`crash-log.ts:124-144`): best-effort append of one record,
/// trimming to the newest [`MAX_CRASH_RECORDS`]. `None` means nothing was
/// written (upstream's `undefined`), so callers can skip the /bug notice.
pub fn record_crash(input: CrashInput<'_>, path: &Path) -> Option<CrashRecord> {
    let mut records = read_crash_log(path);
    let record = CrashRecord {
        timestamp: now_iso8601(),
        version: VERSION.to_string(),
        kind: input.kind.as_str().to_string(),
        message: redact_secrets(input.message),
        stack: input.stack.map(redact_secrets),
        session_file: input.session_file.map(str::to_string),
        cwd: input.cwd.to_string(),
        notified: None,
    };
    records.push(record.clone());
    let start = records.len().saturating_sub(MAX_CRASH_RECORDS);
    write_crash_log(&records[start..], path).ok()?;
    Some(record)
}

/// Convenience for a Rust error: message from `Display`, stack from the
/// error's own backtrace (falling back to a fresh capture). Mirrors upstream's
/// `error instanceof Error ? … : String(error)` (`crash-log.ts:134-135`).
pub fn record_error(
    kind: CrashKind,
    error: &(dyn std::error::Error + 'static),
    session_file: Option<&str>,
    cwd: &str,
    path: &Path,
) -> Option<CrashRecord> {
    let stack = error.backtrace().map(ToString::to_string).or_else(capture_backtrace);
    record_crash(
        CrashInput {
            kind,
            message: &error.to_string(),
            stack: stack.as_deref(),
            session_file,
            cwd,
        },
        path,
    )
}

/// `takeUnnotifiedCrash` (`crash-log.ts:147-162`): the newest un-notified
/// record still within [`MAX_AGE_MS`], marking every un-notified record as
/// notified before returning it. `now_ms` is injected so age is testable.
pub fn take_unnotified_crash(path: &Path, now_ms: i64) -> Option<CrashRecord> {
    let mut records = read_crash_log(path);
    let index = records.iter().rposition(|record| {
        record.notified != Some(true) && now_ms - iso_to_ms(&record.timestamp) <= MAX_AGE_MS
    })?;
    let crash = records[index].clone();
    for record in &mut records {
        if record.notified != Some(true) {
            record.notified = Some(true);
        }
    }
    // Upstream deliberately ignores a failure here: showing the notice twice
    // is harmless (`crash-log.ts:158-160`).
    let _ = write_crash_log(&records, path);
    Some(crash)
}

/// [`take_unnotified_crash`] with `now = Date.now()`.
pub fn take_unnotified_crash_now(path: &Path) -> Option<CrashRecord> {
    take_unnotified_crash(path, chrono::Utc::now().timestamp_millis())
}

/// `clearCrashLog` (`crash-log.ts:164-170`): remove the file, ignoring errors.
pub fn clear_crash_log(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// `isSyntheticPath` (`core/source-info.ts:41-43`), inlined as upstream's
/// crash-log does through the import at `crash-log.ts:5`.
fn is_synthetic_path(path: &str) -> bool {
    path.starts_with("builtin:") || path.starts_with('<')
}

/// `normalizeStackPath` (`crash-log.ts:49-51`).
fn normalize_stack_path(value: &str) -> String {
    value.replace('\\', "/").trim_end_matches('/').to_string()
}

/// `^[a-z]:\/` (`crash-log.ts:56`): Windows paths are matched
/// case-insensitively.
fn is_windows_drive_path(target: &str) -> bool {
    let mut chars = target.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.next() == Some(':')
        && chars.next() == Some('/')
}

/// `stackContainsPath` (`crash-log.ts:53-68`).
fn stack_contains_path(stack: &str, target_path: &str, include_descendants: bool) -> bool {
    let target = normalize_stack_path(target_path);
    if target.is_empty() || is_synthetic_path(&target) {
        return false;
    }
    let case_insensitive = is_windows_drive_path(&target);
    let needle = if case_insensitive {
        Cow::Owned(target.to_lowercase())
    } else {
        Cow::Borrowed(target.as_str())
    };
    let haystack = if case_insensitive {
        Cow::Owned(stack.to_lowercase())
    } else {
        Cow::Borrowed(stack)
    };
    if include_descendants {
        let descendant = format!("{needle}/");
        return haystack.contains(descendant.as_str());
    }
    let mut search_from = 0;
    while search_from <= haystack.len() {
        let Some(relative) = haystack[search_from..].find(needle.as_ref()) else {
            break;
        };
        let index = search_from + relative;
        let next = haystack[index + needle.len()..].chars().next();
        if next.is_none()
            || next == Some(':')
            || next == Some(')')
            || next.is_some_and(char::is_whitespace)
        {
            return true;
        }
        search_from = index + needle.len();
    }
    false
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The subset of JS `decodeURI` used on stack lines (`crash-log.ts:79-86`):
/// decode `%XX` bytes while leaving RFC 3986 reserved characters escaped.
/// Invalid escapes are kept verbatim, like upstream's `try/catch` fallback.
fn decode_uri(input: &str) -> Cow<'_, str> {
    const RESERVED: &[u8] = b";/?:@&=+$,#";
    let bytes = input.as_bytes();
    if !bytes.contains(&b'%') {
        return Cow::Borrowed(input);
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            let decoded = (hi << 4) | lo;
            if RESERVED.contains(&decoded) {
                out.extend_from_slice(&bytes[index..index + 3]);
            } else {
                out.push(decoded);
            }
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

/// `^\s+at\s` for a normalized stack line (`crash-log.ts:79`).
fn is_stack_frame(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.len() < line.len() && trimmed.starts_with("at ")
}

/// `/\.[cm]?[jt]s$/` (`crash-log.ts:97`).
fn is_js_ts_file(source: &str) -> bool {
    [".js", ".mjs", ".cjs", ".ts", ".mts", ".cts"]
        .iter()
        .any(|extension| source.ends_with(*extension))
}

/// `/\/index\.[cm]?[jt]s$/` (`crash-log.ts:103`).
fn is_index_entry(path: &str) -> bool {
    [
        "/index.js",
        "/index.mjs",
        "/index.cjs",
        "/index.ts",
        "/index.mts",
        "/index.cts",
    ]
    .iter()
    .any(|suffix| path.ends_with(*suffix))
}

/// `/^(?:npm:|git:|https?:\/\/|ssh:\/\/)/` (`crash-log.ts:96`).
fn is_remote_source(source: &str) -> bool {
    source.starts_with("npm:")
        || source.starts_with("git:")
        || source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("ssh://")
}

/// `Extension` fields the matcher reads (`crash-log.ts:47`).
#[derive(Debug, Clone, Copy)]
pub struct ExtensionStackMetadata<'a> {
    pub path: &'a str,
    pub resolved_path: &'a str,
    pub source_info: ExtensionSourceInfo<'a>,
}

/// `Pick<Extension, "sourceInfo">` subset used by the matcher.
#[derive(Debug, Clone, Copy)]
pub struct ExtensionSourceInfo<'a> {
    pub source: &'a str,
    pub origin: SourceOrigin,
    pub base_dir: Option<&'a str>,
}

/// `SourceInfo["origin"]` (`core/source-info.ts:9`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceOrigin {
    Package,
    TopLevel,
}

/// `findExtensionStackMatches` (`crash-log.ts:70-121`): labels of loaded
/// extensions whose source files appear in a stack trace, in load order.
#[allow(clippy::option_if_let_else)] // mirrors the upstream ternary chain (crash-log.ts:104-108)
pub fn find_extension_stack_matches(
    stack: Option<&str>,
    extensions: &[ExtensionStackMetadata<'_>],
) -> Vec<String> {
    let Some(stack) = stack else {
        return Vec::new();
    };
    let normalized_stack = stack
        .lines()
        .skip(1)
        .filter(|line| is_stack_frame(line))
        .map(|line| decode_uri(line).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
        .replace('\\', "/");
    let mut matches = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for extension in extensions {
        let resolved_path = normalize_stack_path(extension.resolved_path);
        let single_file_package = extension.source_info.origin == SourceOrigin::Package
            && !is_remote_source(extension.source_info.source)
            && is_js_ts_file(extension.source_info.source);
        let package_root = if extension.source_info.origin == SourceOrigin::Package
            && !single_file_package
        {
            extension.source_info.base_dir
        } else {
            None
        };
        let slash_index = resolved_path.rfind('/');
        let directory_entry = is_index_entry(&resolved_path);
        let matched = if let Some(root) = package_root {
            stack_contains_path(&normalized_stack, root, true)
        } else if directory_entry && slash_index.is_some() {
            let directory = &resolved_path[..slash_index.expect("checked above")];
            stack_contains_path(&normalized_stack, directory, true)
        } else {
            stack_contains_path(&normalized_stack, &resolved_path, false)
        };
        if !matched {
            continue;
        }
        let label = if extension.source_info.origin == SourceOrigin::Package
            && !extension.source_info.source.is_empty()
        {
            extension.source_info.source
        } else {
            extension.path
        };
        if seen.insert(label.to_string()) {
            matches.push(label.to_string());
        }
    }
    matches
}

/// `formatCrashExtensionHint` (`interactive-mode.ts:316-331`), verbatim text
/// with pi-rs's `APP_NAME`.
pub fn format_crash_extension_hint(matches: &[String]) -> Option<String> {
    let matches: Vec<&str> = matches
        .iter()
        .map(String::as_str)
        .filter(|entry| !entry.is_empty())
        .collect();
    if matches.is_empty() {
        return None;
    }
    let quoted: Vec<String> = matches.iter().map(|entry| format!("`{entry}`")).collect();
    let labels = match quoted.len() {
        1 => quoted[0].clone(),
        2 => quoted.join(" and "),
        _ => format!(
            "{}, and {}",
            quoted[..quoted.len() - 1].join(", "),
            quoted[quoted.len() - 1]
        ),
    };
    let noun = if matches.len() == 1 {
        "extension"
    } else {
        "extensions"
    };
    let pronoun = if matches.len() == 1 { "it" } else { "them" };
    Some(format!(
        "A stack frame came from loaded {noun} {labels}, which may be involved. Try disabling {pronoun} with `{} config`, or run `{} -ne` to confirm.",
        config::APP_NAME,
        config::APP_NAME
    ))
}

/// `crashReportInstructions` (`interactive-mode.ts:2290-2293`), verbatim.
pub fn crash_report_instructions(session_file: Option<&str>) -> String {
    let resume = if session_file.is_some() {
        format!("run `{} -r` to resume the session, then", config::APP_NAME)
    } else {
        "start pi and".to_string()
    };
    format!(
        "To report this crash: {resume} run /bug. The crash details are attached automatically."
    )
}

/// The startup notice (`interactive-mode.ts:1261-1267`). Upstream formats the
/// timestamp with `toLocaleString()`, which has no portable Rust equivalent;
/// the ISO timestamp is used here instead.
pub fn format_unnotified_crash_notice(timestamp: &str, message: &str) -> String {
    format!(
        "{} crashed on {timestamp} ({message}). Run /bug to report it; the crash details are attached automatically.",
        config::APP_NAME
    )
}

/// Context captured by the installed hook (`interactive-mode.ts:2281-2282`).
#[derive(Debug, Clone, Default)]
pub struct CrashContext {
    pub session_file: Option<String>,
    pub cwd: String,
}

static PANIC_HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);
/// Session path learned after install (`interactive-mode.ts:2281`): the hook
/// reads it at panic time, so the binary can publish it once a session exists.
static PANIC_SESSION_FILE: Mutex<Option<String>> = Mutex::new(None);

/// Publish the active session file for the next recorded panic. `None` clears
/// it and falls back to the [`CrashContext`] passed at install time.
pub fn set_panic_session_file(session_file: Option<String>) {
    let mut slot = PANIC_SESSION_FILE
        .lock()
        .expect("crash session-file slot poisoned");
    *slot = session_file;
}

/// Guard for an installed panic hook. Dropping it restores the previous hook
/// and allows a new install (mirrors upstream's
/// `process.off("uncaughtException", …)` cleanup,
/// `interactive-mode.ts:4612`). Note that `std::process::exit` skips
/// destructors, so a normal CLI exit leaves the hook installed — which is the
/// desired behavior for a crash handler.
#[allow(clippy::type_complexity)]
pub struct InstalledPanicHook {
    previous: Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>,
}

impl Drop for InstalledPanicHook {
    fn drop(&mut self) {
        let _ = std::panic::take_hook();
        let previous = std::mem::replace(&mut self.previous, Box::new(|_| {}));
        std::panic::set_hook(previous);
        set_panic_session_file(None);
        PANIC_HOOK_INSTALLED.store(false, Ordering::SeqCst);
    }
}

/// Install the `uncaught_exception` handler (`interactive-mode.ts:4610-4612`,
/// `:4542-4562`). Returns `None` if a hook is already installed — upstream's
/// registration is idempotent in practice because it removes the old listener
/// first. The hook records the crash and prints upstream's verbatim notices;
/// it deliberately does not exit, leaving unwinding/abort policy to the
/// caller and to Rust's panic strategy.
pub fn install_panic_hook(path: PathBuf, context: CrashContext) -> Option<InstalledPanicHook> {
    if PANIC_HOOK_INSTALLED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return None;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        panic_hook_body(info, &path, &context);
    }));
    Some(InstalledPanicHook { previous })
}

fn capture_backtrace() -> Option<String> {
    let backtrace = std::backtrace::Backtrace::force_capture().to_string();
    if backtrace.trim().is_empty() {
        None
    } else {
        Some(backtrace)
    }
}

fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_string();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    "Box<dyn Any>".to_string()
}

fn panic_hook_body(info: &std::panic::PanicHookInfo<'_>, path: &Path, context: &CrashContext) {
    let message = panic_message(info);
    let stack = capture_backtrace();
    // A lock held by the panicking thread must not deadlock the hook.
    let session_file = PANIC_SESSION_FILE
        .try_lock()
        .map_or_else(|_| context.session_file.clone(), |slot| slot.clone());
    let recorded = record_crash(
        CrashInput {
            kind: CrashKind::UncaughtException,
            message: &message,
            stack: stack.as_deref(),
            session_file: session_file.as_deref(),
            cwd: &context.cwd,
        },
        path,
    );
    eprintln!("{} exiting due to uncaughtException:", config::APP_NAME); // :4554
    eprintln!("{message}"); // :4555
    // Upstream derives the hint from the loaded extensions; the binary owns
    // that list, so it may print one itself before this hook runs. :4556-4557
    if recorded.is_some() {
        eprintln!("\n{}", crash_report_instructions(session_file.as_deref())); // :4559
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::AssertUnwindSafe;

    /// The panic hook is process-global; serialize the tests that touch it.
    static HOOK_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn fixture(message: &str) -> CrashInput<'_> {
        CrashInput {
            kind: CrashKind::FatalError,
            message,
            stack: None,
            session_file: Some("sessions/abc.jsonl"),
            cwd: "/work/project",
        }
    }

    /// `crashLogPath` (`crash-log.ts:21-23`) with the XDG state-role swap.
    #[test]
    fn crash_log_path_follows_the_state_role_and_agent_override() {
        let state = Path::new("/state/pi-rs");
        assert_eq!(
            crash_log_path_for(None, state),
            PathBuf::from("/state/pi-rs/crashes.json")
        );
        assert_eq!(
            crash_log_path_for(Some(Path::new("/agent")), state),
            PathBuf::from("/agent/crashes.json")
        );
    }

    /// `recordCrash` (`crash-log.ts:124-144`) writes the exact record shape:
    /// keys in upstream order, `null` stack/sessionFile when absent, no
    /// `notified`, two-space indent, and a trailing newline (`:42-45`).
    #[test]
    fn record_format_and_file_shape_match_upstream() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = crash_log_path_for(None, dir.path());
        let input = CrashInput {
            kind: CrashKind::FatalError,
            message: "boom",
            stack: None,
            session_file: None,
            cwd: "/work",
        };
        let record = record_crash(input, &path).expect("record written");
        assert_eq!(record.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(record.kind, "fatal_error");
        assert_eq!(record.session_file, None);
        assert_eq!(record.notified, None);
        let text = std::fs::read_to_string(&path).expect("read crash log");
        assert!(text.starts_with("[\n  {\n    \"timestamp\":"));
        assert!(text.contains("\"version\": \""));
        assert!(text.contains("\"kind\": \"fatal_error\""));
        assert!(text.contains("\"stack\": null"));
        assert!(text.contains("\"sessionFile\": null"));
        assert!(text.contains("\"cwd\": \"/work\""));
        assert!(!text.contains("\"notified\""));
        assert!(text.ends_with("  }\n]\n"));
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        let entries = value.as_array().expect("array");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["message"], "boom");
    }

    /// `readCrashLog`'s filter (`crash-log.ts:29-35`): only objects with string
    /// `timestamp` and `message` survive; a missing file or non-array is `[]`.
    #[test]
    fn read_crash_log_filters_malformed_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = crash_log_path_for(None, dir.path());
        assert_eq!(read_crash_log(&path), Vec::<CrashRecord>::new());
        std::fs::write(
            &path,
            r#"[{"timestamp":"t","message":"ok"},{"timestamp":5,"message":"bad"},{"timestamp":"t2"},7,"x"]"#,
        )
        .expect("write");
        let records = read_crash_log(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].message, "ok");
        std::fs::write(&path, "{}").expect("overwrite");
        assert_eq!(read_crash_log(&path), Vec::<CrashRecord>::new());
        std::fs::write(&path, "not json").expect("overwrite");
        assert_eq!(read_crash_log(&path), Vec::<CrashRecord>::new());
    }

    /// `MAX_CRASH_RECORDS` (`crash-log.ts:18`, `:139`): the oldest entries are
    /// dropped once more than five records exist.
    #[test]
    fn crash_log_keeps_only_the_newest_five_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = crash_log_path_for(None, dir.path());
        for index in 0..7 {
            record_crash(fixture(&format!("crash-{index}")), &path).expect("record");
        }
        let records = read_crash_log(&path);
        assert_eq!(records.len(), 5);
        assert_eq!(records[0].message, "crash-2");
        assert_eq!(records[4].message, "crash-6");
    }

    /// `takeUnnotifiedCrash` (`crash-log.ts:147-162`): newest first, notified
    /// records skipped, all pending records marked notified, and entries older
    /// than `MAX_AGE` ignored.
    #[test]
    fn take_unnotified_crash_marks_pending_records_and_respects_max_age() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = crash_log_path_for(None, dir.path());
        std::fs::write(
            &path,
            r#"[
              {"timestamp":"2026-11-21T00:00:00.000Z","version":"x","kind":"fatal_error","message":"old","stack":null,"sessionFile":null,"cwd":"/"},
              {"timestamp":"2026-11-21T12:00:00.000Z","version":"x","kind":"fatal_error","message":"new","stack":null,"sessionFile":null,"cwd":"/"},
              {"timestamp":"2026-11-21T12:01:00.000Z","version":"x","kind":"fatal_error","message":"announced","stack":null,"sessionFile":null,"cwd":"/","notified":true}
            ]"#,
        )
        .expect("write");
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-22T12:00:00.000Z")
            .expect("parse")
            .timestamp_millis();
        let crash = take_unnotified_crash(&path, now).expect("crash");
        assert_eq!(crash.message, "new");
        let records = read_crash_log(&path);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].notified, Some(true));
        assert_eq!(records[1].notified, Some(true));
        assert_eq!(records[2].notified, Some(true));
        assert_eq!(take_unnotified_crash(&path, now), None);
    }

    /// `clearCrashLog` (`crash-log.ts:164-170`) removes the file, twice.
    #[test]
    fn clear_crash_log_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = crash_log_path_for(None, dir.path());
        record_crash(fixture("boom"), &path).expect("record");
        assert!(path.exists());
        clear_crash_log(&path);
        assert!(!path.exists());
        clear_crash_log(&path);
        assert!(!path.exists());
    }

    /// The redaction deviation: no bearer token, key shape, or secret
    /// assignment survives into `crashes.json`.
    #[test]
    fn redaction_scrubs_credentials_from_message_and_stack() {
        let text = "Bearer sk-abcdefgh1234\nAuthorization: Bearer tok_live_9876543210\napi_key=supersecret123 password: hunter2";
        let redacted = redact_secrets(text);
        assert!(!redacted.contains("abcdefgh1234"));
        assert!(!redacted.contains("tok_live_9876543210"));
        assert!(!redacted.contains("supersecret123"));
        assert!(!redacted.contains("hunter2"));
        assert!(redacted.contains("[REDACTED]"));
        let dir = tempfile::tempdir().expect("tempdir");
        let path = crash_log_path_for(None, dir.path());
        let record = record_crash(
            CrashInput {
                message: &redacted,
                stack: Some(text),
                ..fixture("unused")
            },
            &path,
        )
        .expect("record");
        assert_eq!(record.stack.as_deref(), Some(redacted.as_str()));
        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(!raw.contains("supersecret123"));
        assert!(!raw.contains("abcdefgh1234"));
    }

    /// `stackContainsPath` / `findExtensionStackMatches`
    /// (`crash-log.ts:53-121`): only frames after the first, with leading
    /// whitespace, and pointing at an extension source, produce a label.
    #[test]
    fn extension_stack_matching_uses_source_paths() {
        let stack = "Error: boom\n    at run (/work/ext/index.js:3:5)\n    at main (/work/src/app.ts:1:1)\n at no-leading-space (/work/ext/index.js:9:9)";
        let package = ExtensionStackMetadata {
            path: "npm:my-ext",
            resolved_path: "/work/ext/index.js",
            source_info: ExtensionSourceInfo {
                source: "npm:my-ext",
                origin: SourceOrigin::Package,
                base_dir: None,
            },
        };
        let matches = find_extension_stack_matches(Some(stack), &[package]);
        assert_eq!(matches, vec!["npm:my-ext".to_string()]);
        let hint = format_crash_extension_hint(&matches).expect("hint");
        assert!(hint.contains("loaded extension `npm:my-ext`"));
        assert_eq!(format_crash_extension_hint(&[]), None);
        assert_eq!(find_extension_stack_matches(None, &[package]), Vec::<String>::new());
    }

    /// The two verbatim notice strings used by the binary wiring.
    #[test]
    fn notice_strings_match_upstream() {
        assert_eq!(
            crash_report_instructions(None),
            "To report this crash: start pi and run /bug. The crash details are attached automatically."
        );
        assert_eq!(
            crash_report_instructions(Some("s.jsonl")),
            "To report this crash: run `pi -r` to resume the session, then run /bug. The crash details are attached automatically."
        );
        assert_eq!(
            format_unnotified_crash_notice("2026-11-21T00:00:00.000Z", "boom"),
            "pi crashed on 2026-11-21T00:00:00.000Z (boom). Run /bug to report it; the crash details are attached automatically."
        );
    }

    /// Install is idempotent and the guard restores the slot for a reinstall
    /// (`interactive-mode.ts:4610-4612` + cleanup at `:4612`).
    #[test]
    fn panic_hook_install_is_idempotent_and_restorable() {
        let _serialized = HOOK_TEST_LOCK.lock().expect("hook test lock");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = crash_log_path_for(None, dir.path());
        let first =
            install_panic_hook(path.clone(), CrashContext::default()).expect("first install");
        assert!(install_panic_hook(path.clone(), CrashContext::default()).is_none());
        drop(first);
        let second = install_panic_hook(path, CrashContext::default()).expect("reinstall");
        drop(second);
    }

    /// A synthetic panic through the installed hook writes an
    /// `uncaught_exception` record with the panic payload as its message.
    #[test]
    fn synthetic_panic_is_recorded_as_uncaught_exception() {
        let _serialized = HOOK_TEST_LOCK.lock().expect("hook test lock");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = crash_log_path_for(None, dir.path());
        let context = CrashContext {
            session_file: Some("session.jsonl".to_string()),
            cwd: "/work".to_string(),
        };
        let hook = install_panic_hook(path.clone(), context).expect("install");
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            panic!("synthetic boom");
        }));
        drop(hook);
        assert!(result.is_err());
        let records = read_crash_log(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].kind, "uncaught_exception");
        assert!(records[0].message.contains("synthetic boom"));
        assert_eq!(records[0].session_file.as_deref(), Some("session.jsonl"));
        assert_eq!(records[0].cwd, "/work");
    }

    /// A session file published after install wins over the install-time
    /// context (`interactive-mode.ts:2281` reads it at panic time).
    #[test]
    fn panic_hook_reads_the_session_file_published_after_install() {
        let _serialized = HOOK_TEST_LOCK.lock().expect("hook test lock");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = crash_log_path_for(None, dir.path());
        let hook = install_panic_hook(
            path.clone(),
            CrashContext {
                cwd: "/work".to_string(),
                ..CrashContext::default()
            },
        )
        .expect("install");
        set_panic_session_file(Some("published.jsonl".to_string()));
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            panic!("dynamic session");
        }));
        drop(hook);
        assert!(result.is_err());
        let records = read_crash_log(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].session_file.as_deref(), Some("published.jsonl"));
    }
}
