//! The daemon wire protocol and the injectable connection abstraction.
//!
//! Ground truth: `packages/env/src/connection.ts:1-478`. Ported verbatim in
//! behaviour: the six frame types (`:5-13`), the 13-byte header layout
//! (`:124-132`), the `PI-ENV <token>` sync marker that skips login-shell noise
//! (`:416-436`), `RemoteInfo`/`nodePlatform` (`:46-74`), request correlation
//! with `onEvent`/`onStart` (`:100-117`, `:291-326`) and the lazy start with
//! `hello` (`:278-289`, `:332-399`).
//!
//! # Deviations
//!
//! * The daemon transport is the object-safe [`Daemon`] trait so tests can
//!   inject a scripted fake; [`ProcessDaemon`] is the real spawned-process
//!   implementation.
//! * Upstream's ping/keepalive and abort-grace timers (`:14-24`) have no Rust
//!   counterpart. [`Daemon::tick`] is the host-driven seam: the host supplies
//!   a monotonic millisecond clock and calls it; [`ProcessDaemon`] pings and
//!   declares a silent connection lost there. Abort is an [`AbortSignal`]
//!   checked at protocol checkpoints rather than a timer.
//! * `withStderr` (`:167-179`) is kept as a pure helper; the ANSI-stripping
//!   dependency is replaced by a conservative control-character filter.

use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
};

use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command as TokioCommand},
    sync::{Mutex as AsyncMutex, oneshot},
};

use crate::{
    JsonMap,
    errors::{AbortSignal, RemoteError},
};

/// A boxed future, used to keep [`Daemon`] object-safe without `async-trait`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Request frame (`connection.ts:6`).
pub const REQUEST: u8 = 1;
/// Result frame (`connection.ts:7`).
pub const RESULT: u8 = 2;
/// Error frame (`connection.ts:8`).
pub const ERROR: u8 = 3;
/// Progress-event frame (`connection.ts:9`).
pub const EVENT: u8 = 4;
/// Cancel/kill frame (`connection.ts:10`).
pub const CANCEL: u8 = 5;
/// Keepalive frame (`connection.ts:11`). Never emitted on a timer in the port.
pub const PING: u8 = 6;

/// Largest accepted frame (`connection.ts:13`).
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Daemon ping cadence; consumed by [`Daemon::tick`] (`connection.ts:14`).
pub const PING_INTERVAL_MS: u64 = 5000;
/// Silence that means the connection is gone (`connection.ts:16`).
pub const SILENCE_LIMIT_MS: u64 = 30_000;
/// Startup timeout documented by upstream; the host enforces it through
/// [`Daemon::tick`] (`connection.ts:18`).
pub const START_TIMEOUT_MS: u64 = 60_000;
/// Abort-grace window documented by upstream; cancellation is host-driven in
/// the port (`connection.ts:24`).
pub const ABORT_GRACE_MS: u64 = 1000;
/// How much stderr a startup error shows (`connection.ts:26`).
pub const STDERR_TAIL_CHARS: usize = 2000;
/// How many stderr lines a startup error shows (`connection.ts:27`).
pub const STDERR_TAIL_LINES: usize = 20;

/// One decoded protocol frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Frame type, one of [`REQUEST`], [`RESULT`], [`ERROR`], [`EVENT`],
    /// [`CANCEL`], [`PING`].
    pub kind: u8,
    /// Request id.
    pub id: u64,
    /// The JSON body.
    pub json: JsonMap,
    /// Raw payload bytes after the JSON body.
    pub payload: Vec<u8>,
}

/// Encode a frame exactly as `frame()` does in `connection.ts:124-132`: a
/// 4-byte big-endian length (counting everything after it), then type, id,
/// JSON length, JSON bytes and payload.
#[must_use]
pub fn encode_frame(kind: u8, id: u64, json: &JsonMap, payload: &[u8]) -> Vec<u8> {
    let body = serde_json::to_vec(json).unwrap_or_default();
    let total = 9 + body.len() + payload.len();
    let mut out = Vec::with_capacity(4 + total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.push(kind);
    out.extend_from_slice(&(id as u32).to_be_bytes());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(payload);
    out
}

/// Parse a frame from a byte buffer. `Ok(None)` means more bytes are needed;
/// a corrupt length or JSON body fails as a lost connection, matching
/// `connection.ts:437-463`.
pub fn decode_frame(bytes: &[u8]) -> Result<Option<(Frame, usize)>, RemoteError> {
    if bytes.len() < 4 {
        return Ok(None);
    }
    let total = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if !(9..=MAX_FRAME).contains(&total) {
        return Err(RemoteError::lost("Corrupt frame from pi-env"));
    }
    if bytes.len() < 4 + total {
        return Ok(None);
    }
    let raw = &bytes[4..4 + total];
    let kind = raw[0];
    let id = u32::from_be_bytes([raw[1], raw[2], raw[3], raw[4]]) as u64;
    let json_len = u32::from_be_bytes([raw[5], raw[6], raw[7], raw[8]]) as usize;
    if 9 + json_len > total {
        return Err(RemoteError::lost("Corrupt frame from pi-env"));
    }
    let json = serde_json::from_slice::<Value>(&raw[9..9 + json_len])
        .ok()
        .and_then(|value| value.as_object().cloned())
        .ok_or_else(|| RemoteError::lost("Corrupt frame from pi-env"))?;
    let payload = raw[9 + json_len..].to_vec();
    Ok(Some((
        Frame {
            kind,
            id,
            json,
            payload,
        },
        4 + total,
    )))
}

/// What `hello` reports about the remote machine (`connection.ts:46-63`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteInfo {
    /// Wire protocol version; the client speaks 1.
    pub protocol: u64,
    /// Daemon version string.
    pub version: String,
    /// Rust spelling of the OS (`linux`, `macos`, `windows`, …).
    pub os: String,
    /// Rust spelling of the architecture (`x86_64`, …).
    pub arch: String,
    /// Remote home directory in the remote system's spelling.
    pub home: String,
    /// Remote temp directory.
    pub tmpdir: String,
    /// Path separator: `/` or `\`.
    pub separator: String,
    /// Daemon working directory.
    pub cwd: String,
    /// Windows per-drive working directories (`C:` → `C:\work`).
    pub drive_cwds: JsonMap,
    /// Daemon pid.
    pub pid: u64,
    /// Operations added in protocol 1 that this daemon supports.
    pub features: Option<Vec<String>>,
}

impl RemoteInfo {
    /// Parse a `hello` reply body. Missing fields take upstream's defaults.
    #[must_use]
    pub fn from_json(json: &JsonMap) -> Self {
        let text = |key: &str| -> String {
            json.get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        };
        let number = |key: &str| -> u64 { json.get(key).and_then(Value::as_u64).unwrap_or(0) };
        let drive_cwds = json
            .get("driveCwds")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let features = json
            .get("features")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            });
        Self {
            protocol: number("protocol"),
            version: text("version"),
            os: text("os"),
            arch: text("arch"),
            home: text("home"),
            tmpdir: text("tmpdir"),
            separator: text("separator"),
            cwd: text("cwd"),
            drive_cwds,
            pid: number("pid"),
            features,
        }
    }

    /// Whether the daemon advertises `feature` (`features?.includes(...)`).
    #[must_use]
    pub fn has_feature(&self, feature: &str) -> bool {
        self.features
            .as_ref()
            .is_some_and(|list| list.iter().any(|name| name == feature))
    }
}

/// Node's names for the daemon's OS/architecture (`nodePlatform`,
/// `connection.ts:70-74`).
#[must_use]
pub fn node_platform(info: &RemoteInfo) -> (String, String) {
    let platform = match info.os.as_str() {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    };
    let arch = match info.arch.as_str() {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    (platform.to_owned(), arch.to_owned())
}

/// A progress event from a request (`connection.ts:108-110`).
#[derive(Clone, Debug)]
pub struct Event {
    /// The event JSON; `kind` is `output`, `started`, `ready`, `change`, …
    pub json: JsonMap,
    /// Binary attachment (terminal/process output, watch payload).
    pub payload: Vec<u8>,
}

impl Event {
    /// The `kind` field of the event.
    #[must_use]
    pub fn kind(&self) -> Option<&str> {
        self.json.get("kind").and_then(Value::as_str)
    }
}

/// Callback receiving request events. An `Arc` (not a closure borrow) keeps
/// the [`Daemon`] trait object-safe.
pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;
/// Callback receiving a request's id and daemon session (`onStart`).
pub type StartSink = Arc<dyn Fn(u64, u64) + Send + Sync>;
/// Callback receiving diagnostic text (`onLog`).
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Options for one daemon request (`connection.ts:100-117`).
#[derive(Default)]
pub struct RequestOptions {
    /// Binary payload sent with the request.
    pub payload: Vec<u8>,
    /// Progress events.
    pub on_event: Option<EventSink>,
    /// Request id/session notification.
    pub on_start: Option<StartSink>,
    /// Restrict the request to a daemon session (for handles).
    pub session: Option<u64>,
    /// Cancellation token.
    pub abort: Option<AbortSignal>,
}

/// A daemon reply (`connection.ts:87-92`).
#[derive(Clone, Debug)]
pub struct Reply {
    /// Reply JSON.
    pub json: JsonMap,
    /// Reply payload.
    pub payload: Vec<u8>,
    /// The daemon session that answered.
    pub session: u64,
}

/// How the daemon command is obtained (`command`, `connection.ts:76-85`).
/// `Dynamic` is the SSH path: it detects the platform and verifies/deploys the
/// binary at each start.
pub enum CommandSource {
    /// A fixed argv.
    Fixed(Vec<String>),
    /// Computed at every start.
    Dynamic(Arc<dyn Fn() -> BoxFuture<'static, Result<Vec<String>, RemoteError>> + Send + Sync>),
}

impl CommandSource {
    /// A fixed command.
    #[must_use]
    pub fn fixed(parts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::Fixed(parts.into_iter().map(Into::into).collect())
    }

    /// Resolve to argv for one start.
    pub async fn resolve(&self) -> Result<Vec<String>, RemoteError> {
        match self {
            Self::Fixed(parts) => Ok(parts.clone()),
            Self::Dynamic(build) => build().await,
        }
    }
}

/// Options for a [`ProcessDaemon`] (`connection.ts:76-85`).
pub struct ConnectionOptions {
    /// The command that starts the daemon; `serve --token <hex>` is appended.
    pub command: CommandSource,
    /// Receives daemon and transport diagnostics.
    pub on_log: Option<LogSink>,
}

impl ConnectionOptions {
    /// Options with a fixed command and no log sink.
    #[must_use]
    pub fn new(command: CommandSource) -> Self {
        Self {
            command,
            on_log: None,
        }
    }
}

/// The injectable daemon transport.
///
/// Every environment/process/terminal/watch operation is expressed against
/// this trait, so a test can supply a scripted backend instead of spawning a
/// process or opening an SSH connection.
pub trait Daemon: Send + Sync {
    /// Connect if needed and return the daemon's machine info.
    fn info(&self, abort: Option<AbortSignal>) -> BoxFuture<'_, Result<RemoteInfo, RemoteError>>;
    /// Send one request and await its reply.
    fn request(
        &self,
        op: &str,
        json: JsonMap,
        options: RequestOptions,
    ) -> BoxFuture<'_, Result<Reply, RemoteError>>;
    /// Kill a running request without aborting it (`connection.ts:272-276`).
    fn kill(&self, id: u64, session: u64) -> BoxFuture<'_, ()>;
    /// Stop the daemon and fail all pending requests (`connection.ts:266-270`).
    fn close(&self) -> BoxFuture<'_, ()>;
    /// Host-driven clock tick replacing the ping/keepalive timers. `now_ms`
    /// is monotonic; the default is a no-op for fakes.
    fn tick(&self, _now_ms: u64) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// The stderr tail helper (`withStderr`, `connection.ts:167-179`), without
/// Node's `stripVTControlCharacters` dependency: CSI escape sequences and
/// C0/C1 controls are dropped, newlines and tabs kept, and at most
/// [`STDERR_TAIL_LINES`] lines shown.
#[must_use]
pub fn with_stderr(message: &str, stderr: &str) -> String {
    let normalized = stderr.replace("\r\n", "\n").replace('\r', "\n");
    let mut text = String::new();
    let mut in_escape = false;
    let mut skip_introducer = false;
    for character in normalized.chars() {
        if in_escape {
            if skip_introducer {
                // The byte after ESC is the sequence introducer (e.g. `[` in CSI).
                skip_introducer = false;
                continue;
            }
            // The sequence ends at its final byte (0x40..=0x7e).
            if ('@'..='~').contains(&character) {
                in_escape = false;
            }
            continue;
        }
        if character == '\u{1b}' {
            in_escape = true;
            skip_introducer = true;
            continue;
        }
        let code = character as u32;
        if character == '\n'
            || character == '\t'
            || (code >= 0x20 && !(0x7f..=0x9f).contains(&code))
        {
            text.push(character);
        }
    }
    let lines = text
        .split('\n')
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return message.to_owned();
    }
    let start = lines.len().saturating_sub(STDERR_TAIL_LINES);
    format!("{message}\nremote stderr:\n{}", lines[start..].join("\n"))
}

/// Find `needle` in `haystack` without pulling in a regex dependency.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

struct Pending {
    result: oneshot::Sender<Result<Reply, RemoteError>>,
    on_event: Option<EventSink>,
}

#[derive(Default)]
struct InnerState {
    child: Option<Child>,
    pending: BTreeMap<u64, Pending>,
    next_id: u64,
    session_id: u64,
    ready: Option<RemoteInfo>,
    lost: Option<RemoteError>,
    closed: bool,
    synced: bool,
    stderr_tail: String,
    last_seen_ms: u64,
    last_ping_ms: u64,
}

struct ProcessInner {
    options: ConnectionOptions,
    state: StdMutex<InnerState>,
    stdin: AsyncMutex<Option<ChildStdin>>,
    now_ms: AtomicU64,
}

async fn write_frame(inner: &ProcessInner, bytes: Vec<u8>) -> Result<(), RemoteError> {
    let mut stdin = inner.stdin.lock().await;
    match stdin.as_mut() {
        Some(stream) => stream
            .write_all(&bytes)
            .await
            .map_err(|error| RemoteError::lost(error.to_string())),
        None => Err(RemoteError::lost("pi-env connection lost")),
    }
}

fn teardown(inner: &ProcessInner, error: &RemoteError) {
    let (pending, mut child) = {
        let mut state = inner.state.lock().expect("connection state lock");
        state.ready = None;
        state.lost = Some((*error).clone());
        let pending = std::mem::take(&mut state.pending)
            .into_values()
            .collect::<Vec<_>>();
        (pending, state.child.take())
    };
    if let Some(child) = &mut child {
        let _ = child.start_kill();
    }
    for request in pending {
        let _ = request.result.send(Err((*error).clone()));
    }
}

fn dispatch(inner: &ProcessInner, frame: Frame) {
    {
        let mut state = inner.state.lock().expect("connection state lock");
        state.last_seen_ms = inner.now_ms.load(Ordering::SeqCst);
    }
    if frame.kind == EVENT {
        let sink = {
            let state = inner.state.lock().expect("connection state lock");
            state
                .pending
                .get(&frame.id)
                .and_then(|pending| pending.on_event.clone())
        };
        if let Some(sink) = sink {
            sink(Event {
                json: frame.json,
                payload: frame.payload,
            });
        }
        return;
    }
    if frame.kind != RESULT && frame.kind != ERROR {
        return;
    }
    let session = inner
        .state
        .lock()
        .expect("connection state lock")
        .session_id;
    let pending = inner
        .state
        .lock()
        .expect("connection state lock")
        .pending
        .remove(&frame.id);
    if let Some(pending) = pending {
        let outcome = if frame.kind == RESULT {
            Ok(Reply {
                json: frame.json,
                payload: frame.payload,
                session,
            })
        } else {
            Err(RemoteError::from_fields(frame.json))
        };
        let _ = pending.result.send(outcome);
    }
}

async fn reader_loop(inner: Arc<ProcessInner>, stdout: ChildStdout, token: String) {
    let mut reader = BufReader::new(stdout);
    let marker = format!("PI-ENV {token}\n").into_bytes();
    let mut buffer = Vec::new();
    let mut synced = false;
    loop {
        let mut chunk = [0u8; 8192];
        let read = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        buffer.extend_from_slice(&chunk[..read]);
        if !synced {
            if let Some(index) = find_subslice(&buffer, &marker) {
                let noise = String::from_utf8_lossy(&buffer[..index]).trim().to_owned();
                if !noise.is_empty()
                    && let Some(log) = &inner.options.on_log
                {
                    log(&noise);
                }
                buffer.drain(..index + marker.len());
                synced = true;
                inner.state.lock().expect("connection state lock").synced = true;
            } else {
                if buffer.len() > 1024 * 1024 {
                    let keep = marker.len().saturating_sub(1);
                    let drop_count = buffer.len() - keep;
                    buffer.drain(..drop_count);
                }
                continue;
            }
        }
        loop {
            match decode_frame(&buffer) {
                Ok(Some((frame, consumed))) => {
                    buffer.drain(..consumed);
                    dispatch(&inner, frame);
                }
                Ok(None) => break,
                Err(error) => {
                    teardown(&inner, &error);
                    return;
                }
            }
        }
    }
    teardown(&inner, &RemoteError::lost("pi-env connection lost"));
}

/// A real connection to a daemon spawned as a child process. Starts lazily on
/// the first request, skips login-shell noise, speaks the framed protocol and
/// reconnects on the next request after a loss.
pub struct ProcessDaemon {
    inner: Arc<ProcessInner>,
    start_lock: Arc<AsyncMutex<()>>,
}

impl std::fmt::Debug for ProcessDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ProcessDaemon")
    }
}

impl ProcessDaemon {
    /// A daemon started by `options.command`.
    #[must_use]
    pub fn new(options: ConnectionOptions) -> Self {
        Self {
            inner: Arc::new(ProcessInner {
                options,
                state: StdMutex::new(InnerState::default()),
                stdin: AsyncMutex::new(None),
                now_ms: AtomicU64::new(0),
            }),
            start_lock: Arc::new(AsyncMutex::new(())),
        }
    }

    #[allow(clippy::too_many_lines)] // mirrors the TS ensureStarted sequence; split when extending
    async fn ensure_started(&self) -> Result<RemoteInfo, RemoteError> {
        let _guard = self.start_lock.lock().await;
        {
            let state = self.inner.state.lock().expect("connection state lock");
            if state.closed {
                return Err(RemoteError::new("unknown", "Connection closed"));
            }
            if let Some(info) = &state.ready {
                return Ok(info.clone());
            }
        }
        let mut command = self
            .inner
            .options
            .command
            .resolve()
            .await
            .map_err(|error| RemoteError::new("spawn_error", error.message.clone()))?;
        if command.is_empty() {
            return Err(RemoteError::new("spawn_error", "No daemon command"));
        }
        let token = uuid::Uuid::new_v4().simple().to_string();
        command.push("serve".into());
        command.push("--token".into());
        command.push(token.clone());
        let mut child = TokioCommand::new(&command[0])
            .args(&command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| RemoteError::new("spawn_error", error.to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| RemoteError::new("spawn_error", "No daemon stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RemoteError::new("spawn_error", "No daemon stdout"))?;
        let stderr = child.stderr.take();
        {
            let mut state = self.inner.state.lock().expect("connection state lock");
            state.child = Some(child);
            state.ready = None;
            state.lost = None;
            state.synced = false;
            state.stderr_tail.clear();
            state.last_seen_ms = 0;
            state.last_ping_ms = 0;
            state.session_id += 1;
        }
        *self.inner.stdin.lock().await = Some(stdin);
        let inner = self.inner.clone();
        tokio::spawn(async move {
            reader_loop(inner, stdout, token).await;
        });
        if let (Some(stderr), Some(log)) = (stderr, self.inner.options.on_log.clone()) {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    log(&line);
                }
            });
        }
        let id = {
            let mut state = self.inner.state.lock().expect("connection state lock");
            let id = state.next_id;
            state.next_id += 1;
            id
        };
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = self.inner.state.lock().expect("connection state lock");
            state.pending.insert(
                id,
                Pending {
                    result: sender,
                    on_event: None,
                },
            );
        }
        let mut hello = JsonMap::new();
        hello.insert("protocol".into(), Value::from(1));
        write_frame(&self.inner, encode_frame(REQUEST, id, &hello, &[])).await?;
        let reply = receiver
            .await
            .map_err(|_| RemoteError::lost("pi-env connection lost"))??;
        if reply.json.get("protocol").and_then(Value::as_u64) != Some(1) {
            let error = RemoteError::lost(format!(
                "Unsupported protocol {}",
                reply
                    .json
                    .get("protocol")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            ));
            teardown(&self.inner, &error);
            return Err(error);
        }
        let info = RemoteInfo::from_json(&reply.json);
        let mut state = self.inner.state.lock().expect("connection state lock");
        state.ready = Some(info.clone());
        state.stderr_tail.clear();
        Ok(info)
    }
}

impl Daemon for ProcessDaemon {
    fn info(&self, abort: Option<AbortSignal>) -> BoxFuture<'_, Result<RemoteInfo, RemoteError>> {
        Box::pin(async move {
            if abort.as_ref().is_some_and(AbortSignal::aborted) {
                return Err(RemoteError::new("aborted", "aborted"));
            }
            self.ensure_started().await
        })
    }

    fn request(
        &self,
        op: &str,
        json: JsonMap,
        options: RequestOptions,
    ) -> BoxFuture<'_, Result<Reply, RemoteError>> {
        let op = op.to_owned();
        Box::pin(async move {
            if options.abort.as_ref().is_some_and(AbortSignal::aborted) {
                return Err(RemoteError::new("aborted", "aborted"));
            }
            self.ensure_started().await?;
            if let Some(session) = options.session {
                let state = self.inner.state.lock().expect("connection state lock");
                if state.ready.is_none() || state.session_id != session {
                    return Err(RemoteError::lost("pi-env connection lost"));
                }
            }
            let id = {
                let mut state = self.inner.state.lock().expect("connection state lock");
                let id = state.next_id;
                state.next_id += 1;
                id
            };
            let session = self
                .inner
                .state
                .lock()
                .expect("connection state lock")
                .session_id;
            let (sender, receiver) = oneshot::channel();
            {
                let mut state = self.inner.state.lock().expect("connection state lock");
                state.pending.insert(
                    id,
                    Pending {
                        result: sender,
                        on_event: options.on_event.clone(),
                    },
                );
            }
            if let Some(on_start) = &options.on_start {
                on_start(id, session);
            }
            let mut body = json;
            body.insert("op".into(), Value::String(op));
            write_frame(
                &self.inner,
                encode_frame(REQUEST, id, &body, &options.payload),
            )
            .await?;
            receiver
                .await
                .map_err(|_| RemoteError::lost("pi-env connection lost"))?
        })
    }

    fn kill(&self, id: u64, session: u64) -> BoxFuture<'_, ()> {
        // The state lock is taken here, never inside the boxed future: a
        // `MutexGuard` is not `Send` and must not sit across the write.
        let should_kill = {
            let state = self.inner.state.lock().expect("connection state lock");
            state.ready.is_some() && state.session_id == session
        };
        Box::pin(async move {
            if should_kill {
                let mut json = JsonMap::new();
                json.insert("mode".into(), Value::String("kill".into()));
                let _ = write_frame(&self.inner, encode_frame(CANCEL, id, &json, &[])).await;
            }
        })
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.inner
                .state
                .lock()
                .expect("connection state lock")
                .closed = true;
            teardown(
                &self.inner,
                &RemoteError::new("unknown", "Connection closed"),
            );
        })
    }

    fn tick(&self, now_ms: u64) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.inner.now_ms.store(now_ms, Ordering::SeqCst);
            let (last_seen, last_ping, ready) = {
                let state = self.inner.state.lock().expect("connection state lock");
                (
                    state.last_seen_ms,
                    state.last_ping_ms,
                    state.ready.is_some(),
                )
            };
            if !ready {
                return;
            }
            if now_ms.saturating_sub(last_seen) > SILENCE_LIMIT_MS {
                teardown(
                    &self.inner,
                    &RemoteError::lost("pi-env connection timed out"),
                );
                return;
            }
            if now_ms.saturating_sub(last_ping) >= PING_INTERVAL_MS {
                self.inner
                    .state
                    .lock()
                    .expect("connection state lock")
                    .last_ping_ms = now_ms;
                let _ = write_frame(&self.inner, encode_frame(PING, 0, &JsonMap::new(), &[])).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips_header_json_and_payload() {
        let mut json = JsonMap::new();
        json.insert("op".into(), Value::String("read".into()));
        json.insert("n".into(), Value::from(7));
        let encoded = encode_frame(REQUEST, 42, &json, b"abc");
        assert_eq!(
            encoded.len(),
            13 + serde_json::to_vec(&json).unwrap().len() + 3
        );
        let (frame, consumed) = decode_frame(&encoded).expect("frame").expect("complete");
        assert_eq!(consumed, encoded.len());
        assert_eq!(frame.kind, REQUEST);
        assert_eq!(frame.id, 42);
        assert_eq!(frame.json.get("op").and_then(Value::as_str), Some("read"));
        assert_eq!(frame.payload, b"abc");
    }

    #[test]
    fn frame_decode_waits_for_more_bytes() {
        let json = JsonMap::new();
        let encoded = encode_frame(EVENT, 1, &json, b"payload");
        for prefix in [0, 1, 3, 4, 12, encoded.len() - 1] {
            let result = decode_frame(&encoded[..prefix]).expect("incomplete is not an error");
            assert!(result.is_none());
        }
    }

    #[test]
    fn frame_decode_rejects_corrupt_lengths_and_json() {
        let mut bad_length = vec![0u8; 4];
        bad_length[3] = 8;
        assert_eq!(
            decode_frame(&bad_length).expect_err("corrupt").message,
            "Corrupt frame from pi-env"
        );
        let mut bad_json = encode_frame(RESULT, 1, &JsonMap::new(), &[]);
        bad_json[13] = b'X';
        assert_eq!(
            decode_frame(&bad_json).expect_err("corrupt").message,
            "Corrupt frame from pi-env"
        );
    }

    #[test]
    fn node_platform_maps_rust_names_to_node_names() {
        let mut json = JsonMap::new();
        json.insert("os".into(), Value::String("macos".into()));
        json.insert("arch".into(), Value::String("aarch64".into()));
        let info = RemoteInfo::from_json(&json);
        assert_eq!(
            node_platform(&info),
            ("darwin".to_owned(), "arm64".to_owned())
        );
    }

    #[test]
    fn remote_info_reports_features() {
        let mut json = JsonMap::new();
        json.insert(
            "features".into(),
            Value::Array(vec![Value::String("process".into())]),
        );
        let info = RemoteInfo::from_json(&json);
        assert!(info.has_feature("process"));
        assert!(!info.has_feature("listFiles"));
    }

    #[test]
    fn sync_marker_and_stderr_filter_are_deterministic() {
        let marker = format!("PI-ENV {}\n", "token").into_bytes();
        let stream = b"profile\0noisePI-ENV token\nrest".to_vec();
        let index = find_subslice(&stream, &marker).expect("marker");
        assert_eq!(&stream[index + marker.len()..], b"rest");
        assert_eq!(
            with_stderr("failed", "\u{1b}[31mboom\u{1b}[0m\n\n"),
            "failed\nremote stderr:\nboom"
        );
    }
}
