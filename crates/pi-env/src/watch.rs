//! Remote file watching with reconnect and overflow reporting.
//!
//! Ground truth: `packages/env/src/watch.ts:1-162`. Ported behaviour:
//! `RemoteWatchOptions` (`:6-14`), the lone `OVERFLOW` change (`:17`), the
//! reconnect constants (`:19-20`), the daemon-side watcher lifecycle
//! (`:22-135`) and the reconnect loop (`:137-161`), including the `ready` /
//! `change` / `error` event wire format and that a throwing callback must not
//! stop watching (`:73-80`).
//!
//! # Deviation: no timers
//!
//! Upstream's reconnect backoff uses `setTimeout` with unref'd timers and an
//! `AbortController` wake (`:137-161`). Here the request runs in a spawned
//! task, and [`RemoteWatcher::tick`] is the host-driven clock: the host calls
//! it with a monotonic millisecond value to advance the 1 s → 30 s exponential
//! backoff and to restart the watcher. Feeding daemon events directly is also
//! possible through [`RemoteWatcher::on_event`].

use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;
use tokio::sync::oneshot;

use crate::{
    JsonMap,
    connection::{Daemon, EventSink, RequestOptions},
    errors::{AbortSignal, FileError, FileErrorCode, RemoteError, to_file_error},
    files::{FileChange, WatchTarget},
};

/// First reconnect delay (`watch.ts:19`).
pub const RECONNECT_FIRST_MS: u64 = 1000;
/// Largest reconnect delay (`watch.ts:20`).
pub const RECONNECT_MAX_MS: u64 = 30_000;

/// How the daemon watches (`RemoteWatchOptions`, `watch.ts:6-14`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteWatchOptions {
    /// Force a mode; by default the daemon picks native or polling.
    pub mode: Option<WatchMode>,
    /// Snapshot interval in polling mode; the daemon defaults to 2000 ms.
    pub poll_interval_ms: Option<u64>,
    /// Most directories one watcher covers; the daemon defaults to 10,000.
    pub max_directories: Option<u64>,
}

/// The two watch modes (`RemoteWatchOptions.mode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WatchMode {
    /// Filesystem-native notifications.
    #[default]
    Native,
    /// Periodic snapshots (Windows, network filesystems).
    Polling,
}

impl WatchMode {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Polling => "polling",
        }
    }

    fn from_str(value: &str) -> Self {
        match value {
            "polling" => Self::Polling,
            _ => Self::Native,
        }
    }
}

/// A daemon watch event, parsed from the `onEvent` wire format
/// (`watch.ts:100-115`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    /// Coverage was established.
    Ready {
        /// The mode the daemon selected.
        mode: WatchMode,
    },
    /// One or more paths changed, or the daemon lost track of changes.
    Change {
        /// Polling mode, if the event reports it.
        mode: Option<WatchMode>,
        /// The daemon's `{ overflow: true }` form.
        overflow: bool,
        /// The daemon's `{ paths }` form.
        paths: Vec<String>,
    },
    /// Coverage failed irrecoverably.
    Error {
        /// `permission_denied` for that code, otherwise `invalid`.
        code: &'static str,
        /// Upstream stringifies the daemon message (including `undefined`).
        message: String,
    },
}

/// Parse a watch event; unknown kinds are ignored as upstream does.
#[must_use]
pub fn parse_watch_event(json: &JsonMap) -> Option<WatchEvent> {
    let string = |key: &str| -> Option<&str> { json.get(key).and_then(Value::as_str) };
    match string("kind")? {
        "ready" => Some(WatchEvent::Ready {
            // An absent `mode` stays absent on the ready event.
            mode: WatchMode::from_str(string("mode").unwrap_or("")),
        }),
        "change" => {
            // An absent mode leaves the watcher's current mode untouched
            // (`watch.ts:106`); only explicit `polling` switches it.
            let mode = string("mode").map(WatchMode::from_str);
            let overflow = json.get("overflow").and_then(Value::as_bool) == Some(true);
            let paths = json
                .get("paths")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            Some(WatchEvent::Change {
                mode,
                overflow,
                paths,
            })
        }
        "error" => {
            let code = if string("code") == Some("permission_denied") {
                "permission_denied"
            } else {
                "invalid"
            };
            let message = string("message").unwrap_or("undefined").to_owned();
            Some(WatchEvent::Error { code, message })
        }
        _ => None,
    }
}

/// The watch request JSON (`watch.ts:88-98`).
fn watch_request(targets: &[WatchTarget], options: &RemoteWatchOptions) -> JsonMap {
    let mut json = JsonMap::new();
    json.insert(
        "targets".into(),
        Value::Array(
            targets
                .iter()
                .map(WatchTarget::to_json)
                .map(Value::Object)
                .collect(),
        ),
    );
    if let Some(mode) = options.mode {
        json.insert("mode".into(), Value::String(mode.as_str().to_owned()));
    }
    if let Some(value) = options.poll_interval_ms {
        json.insert("pollIntervalMs".into(), Value::from(value));
    }
    if let Some(value) = options.max_directories {
        json.insert("maxDirectories".into(), Value::from(value));
    }
    json
}

#[derive(Default)]
struct WatcherState {
    targets: Vec<WatchTarget>,
    options: RemoteWatchOptions,
    mode: WatchMode,
    stopped: bool,
    ever_ready: bool,
    pending_reconnect: bool,
    retry_deadline: Option<u64>,
    reconnect_delay_ms: u64,
    abort: AbortSignal,
    ready: Option<oneshot::Sender<Result<(), RemoteError>>>,
}

const OVERFLOW: FileChange = FileChange::Overflow;

fn deliver(on_change: &Arc<dyn Fn(FileChange) + Send + Sync>, change: FileChange) {
    // A throwing callback must not stop watching (`watch.ts:73-80`).
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_change(change)));
}

fn signal_ready(state: &mut MutexGuard<'_, WatcherState>, result: Result<(), RemoteError>) {
    if let Some(sender) = state.ready.take() {
        let _ = sender.send(result);
    }
}

fn process_event(
    state: &Arc<Mutex<WatcherState>>,
    on_change: &Arc<dyn Fn(FileChange) + Send + Sync>,
    event: &crate::connection::Event,
) {
    let Some(parsed) = parse_watch_event(&event.json) else {
        return;
    };
    match parsed {
        WatchEvent::Ready { mode } => {
            let mut state = state.lock().expect("watcher state lock");
            state.mode = mode;
            state.ever_ready = true;
            state.reconnect_delay_ms = RECONNECT_FIRST_MS;
            signal_ready(&mut state, Ok(()));
        }
        WatchEvent::Change {
            mode,
            overflow,
            paths,
        } => {
            if let Some(WatchMode::Polling) = mode {
                state.lock().expect("watcher state lock").mode = WatchMode::Polling;
            }
            if overflow {
                deliver(on_change, OVERFLOW);
            } else {
                deliver(on_change, FileChange::Paths(paths));
            }
        }
        WatchEvent::Error { code, message } => {
            let error = FileError::new(
                if code == "permission_denied" {
                    FileErrorCode::PermissionDenied
                } else {
                    FileErrorCode::Invalid
                },
                message,
                None,
            );
            deliver(on_change, FileChange::Failed { error });
            let mut state = state.lock().expect("watcher state lock");
            state.stopped = true;
            signal_ready(&mut state, Ok(()));
        }
    }
}

fn request_failed(
    state: &Arc<Mutex<WatcherState>>,
    on_change: &Arc<dyn Fn(FileChange) + Send + Sync>,
    error: RemoteError,
) {
    let mut state = state.lock().expect("watcher state lock");
    if !state.ever_ready {
        signal_ready(&mut state, Err(error));
        return;
    }
    if state.stopped {
        return;
    }
    if error.is_lost() {
        state.pending_reconnect = true;
        state.retry_deadline = None;
        return;
    }
    deliver(
        on_change,
        FileChange::Failed {
            error: to_file_error(&error, None),
        },
    );
    state.stopped = true;
}

/// A watcher running next to the files in the daemon
/// (`RemoteWatcher`, `watch.ts:22-162`).
#[derive(Clone)]
pub struct RemoteWatcher {
    daemon: Arc<dyn Daemon>,
    state: Arc<Mutex<WatcherState>>,
    on_change: Arc<dyn Fn(FileChange) + Send + Sync>,
}

impl std::fmt::Debug for RemoteWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RemoteWatcher {{ mode: {:?} }}",
            self.state.lock().expect("watcher state lock").mode
        )
    }
}

impl RemoteWatcher {
    /// Watch resolved `targets`; the request reports coverage through a
    /// `ready` event (`watch.ts:49-59`).
    pub async fn open(
        daemon: Arc<dyn Daemon>,
        targets: Vec<WatchTarget>,
        on_change: Arc<dyn Fn(FileChange) + Send + Sync>,
        options: RemoteWatchOptions,
    ) -> Result<Self, RemoteError> {
        let watcher = Self {
            daemon,
            state: Arc::new(Mutex::new(WatcherState {
                targets,
                options,
                reconnect_delay_ms: RECONNECT_FIRST_MS,
                ..Default::default()
            })),
            on_change,
        };
        let ready = watcher.begin_watch();
        match ready.await {
            Ok(result) => {
                result?;
                Ok(watcher)
            }
            Err(_) => Err(RemoteError::lost("pi-env connection lost")),
        }
    }

    fn begin_watch(&self) -> oneshot::Receiver<Result<(), RemoteError>> {
        let (sender, receiver) = oneshot::channel();
        let (targets, options, abort) = {
            let mut state = self.state.lock().expect("watcher state lock");
            state.ready = Some(sender);
            state.pending_reconnect = false;
            state.retry_deadline = None;
            (
                state.targets.clone(),
                state.options.clone(),
                state.abort.clone(),
            )
        };
        let state = self.state.clone();
        let on_change = self.on_change.clone();
        let daemon = self.daemon.clone();
        // `state` and `on_change` are also used by the spawned task below,
        // so the event sink gets its own Arc handles.
        let on_event: EventSink = {
            let state = Arc::clone(&state);
            let on_change = Arc::clone(&on_change);
            Arc::new(move |event| process_event(&state, &on_change, &event))
        };
        tokio::spawn(async move {
            let request = daemon.request(
                "watch",
                watch_request(&targets, &options),
                RequestOptions {
                    on_event: Some(on_event),
                    abort: Some(abort),
                    ..Default::default()
                },
            );
            if let Err(error) = request.await {
                request_failed(&state, &on_change, error);
            }
        });
        receiver
    }

    /// Feed a daemon event directly. Tests and hosts that already read the
    /// daemon stream can call this instead of relying on the spawned request.
    pub fn on_event(&self, event: &crate::connection::Event) {
        process_event(&self.state, &self.on_change, event);
    }

    /// The mode the daemon selected (`mode`, `watch.ts:61-63`).
    #[must_use]
    pub fn mode(&self) -> WatchMode {
        self.state.lock().expect("watcher state lock").mode
    }

    /// Whether the watcher was stopped or failed irrecoverably.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.state.lock().expect("watcher state lock").stopped
    }

    /// Stop watching (`watch.ts:65-71`).
    pub fn stop(&self) {
        let mut state = self.state.lock().expect("watcher state lock");
        if state.stopped {
            return;
        }
        state.stopped = true;
        state.abort.abort();
        signal_ready(&mut state, Ok(()));
    }

    /// Advance the reconnect state machine using a host-supplied monotonic
    /// millisecond clock. Upstream does this with unref'd timers; the port has
    /// no timers (`watch.ts:137-161`).
    ///
    /// The body is synchronous, but the `async` signature is kept so hosts can
    /// await [`Daemon::tick`] and this call through one uniform interface.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn tick(&self, now_ms: u64) {
        let mut state = self.state.lock().expect("watcher state lock");
        if state.stopped || !state.pending_reconnect {
            return;
        }
        match state.retry_deadline {
            None => state.retry_deadline = Some(now_ms + state.reconnect_delay_ms),
            Some(deadline) if now_ms >= deadline => {
                state.retry_deadline = None;
                drop(state);
                self.begin_watch();
                let mut state = self.state.lock().expect("watcher state lock");
                if state.ever_ready && !state.stopped && !state.pending_reconnect {
                    // Changes while disconnected were not seen.
                    deliver(&self.on_change, OVERFLOW);
                    state.reconnect_delay_ms = RECONNECT_FIRST_MS;
                } else if state.pending_reconnect {
                    state.reconnect_delay_ms =
                        (state.reconnect_delay_ms.saturating_mul(2)).min(RECONNECT_MAX_MS);
                    state.retry_deadline = None;
                }
            }
            Some(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        connection::Event,
        test_support::{FakeDaemon, info_with_features, reply},
    };

    fn event(json: JsonMap, payload: Vec<u8>) -> Event {
        Event { json, payload }
    }

    fn change_event(mode: Option<&str>, overflow: bool, paths: &[&str]) -> Event {
        let mut json = JsonMap::new();
        json.insert("kind".into(), Value::String("change".into()));
        if let Some(mode) = mode {
            json.insert("mode".into(), Value::String(mode.into()));
        }
        json.insert("overflow".into(), Value::Bool(overflow));
        json.insert(
            "paths".into(),
            Value::Array(
                paths
                    .iter()
                    .map(|path| Value::String((*path).into()))
                    .collect(),
            ),
        );
        event(json, Vec::new())
    }

    #[test]
    fn parses_watch_events_including_overflow_and_error_codes() {
        let mut ready = JsonMap::new();
        ready.insert("kind".into(), Value::String("ready".into()));
        ready.insert("mode".into(), Value::String("polling".into()));
        assert_eq!(
            parse_watch_event(&ready),
            Some(WatchEvent::Ready {
                mode: WatchMode::Polling
            })
        );
        let change = change_event(Some("native"), true, &[]);
        assert_eq!(
            parse_watch_event(&change.json),
            Some(WatchEvent::Change {
                mode: Some(WatchMode::Native),
                overflow: true,
                paths: Vec::new()
            })
        );
        let mut error = JsonMap::new();
        error.insert("kind".into(), Value::String("error".into()));
        error.insert("code".into(), Value::String("permission_denied".into()));
        error.insert("message".into(), Value::String("nope".into()));
        assert_eq!(
            parse_watch_event(&error),
            Some(WatchEvent::Error {
                code: "permission_denied",
                message: "nope".into()
            })
        );
        error.insert("code".into(), Value::String("EIO".into()));
        assert_eq!(parse_watch_event(&error).map(|_| ()), Some(()));
    }

    #[test]
    fn watch_request_carries_targets_and_options() {
        let target = WatchTarget {
            path: "/x".into(),
            recursive: true,
            exclude_hidden: false,
            exclude_names: Vec::new(),
        };
        let options = RemoteWatchOptions {
            mode: Some(WatchMode::Polling),
            poll_interval_ms: Some(250),
            max_directories: Some(5),
        };
        let json = watch_request(&[target], &options);
        assert_eq!(json.get("mode").and_then(Value::as_str), Some("polling"));
        assert_eq!(
            json.get("pollIntervalMs").and_then(Value::as_u64),
            Some(250)
        );
        assert_eq!(json.get("maxDirectories").and_then(Value::as_u64), Some(5));
        assert_eq!(
            json.get("targets").and_then(Value::as_array).map(Vec::len),
            Some(1)
        );
    }

    #[tokio::test]
    async fn open_reports_ready_changes_and_overflow() {
        let daemon = Arc::new(FakeDaemon::new(info_with_features(&[])));
        let mut ready = JsonMap::new();
        ready.insert("kind".into(), Value::String("ready".into()));
        ready.insert("mode".into(), Value::String("polling".into()));
        daemon.push_reply(
            "watch",
            reply(JsonMap::new(), Vec::new()),
            vec![event(ready, Vec::new())],
            None,
        );
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let watcher = RemoteWatcher::open(
            daemon,
            vec![WatchTarget {
                path: "/x".into(),
                recursive: false,
                exclude_hidden: false,
                exclude_names: Vec::new(),
            }],
            Arc::new(move |change| sink.lock().expect("sink lock").push(change)),
            RemoteWatchOptions::default(),
        )
        .await
        .expect("watch");
        // Feed the changes explicitly so the assertions do not race the
        // spawned request task.
        watcher.on_event(&change_event(None, false, &["a", "b"]));
        watcher.on_event(&change_event(None, true, &[]));
        assert_eq!(watcher.mode(), WatchMode::Polling);
        assert_eq!(
            *seen.lock().expect("sink lock"),
            vec![FileChange::Paths(vec!["a".into(), "b".into()]), OVERFLOW]
        );
    }

    #[tokio::test]
    async fn error_event_reports_failed_change() {
        let daemon = Arc::new(FakeDaemon::new(info_with_features(&[])));
        let mut ready = JsonMap::new();
        ready.insert("kind".into(), Value::String("ready".into()));
        let mut error = JsonMap::new();
        error.insert("kind".into(), Value::String("error".into()));
        error.insert("code".into(), Value::String("permission_denied".into()));
        error.insert("message".into(), Value::String("denied".into()));
        daemon.push_reply(
            "watch",
            reply(JsonMap::new(), Vec::new()),
            vec![event(ready, Vec::new())],
            None,
        );
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let watcher = RemoteWatcher::open(
            daemon,
            Vec::new(),
            Arc::new(move |change| sink.lock().expect("sink lock").push(change)),
            RemoteWatchOptions::default(),
        )
        .await
        .expect("watch");
        watcher.on_event(&event(error, Vec::new()));
        assert!(watcher.is_stopped());
        let changes = seen.lock().expect("sink lock");
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            FileChange::Failed { error } => {
                assert_eq!(error.code, FileErrorCode::PermissionDenied);
                assert_eq!(error.message, "denied");
            }
            other => panic!("unexpected change: {other:?}"),
        }
    }

    #[tokio::test]
    async fn tick_drives_reconnect_without_timers() {
        let daemon = Arc::new(FakeDaemon::new(info_with_features(&[])));
        let mut ready = JsonMap::new();
        ready.insert("kind".into(), Value::String("ready".into()));
        daemon.push_reply(
            "watch",
            reply(JsonMap::new(), Vec::new()),
            vec![event(ready, Vec::new())],
            None,
        );
        let calls = daemon.clone();
        let watcher = RemoteWatcher::open(
            daemon,
            Vec::new(),
            Arc::new(|_| {}),
            RemoteWatchOptions::default(),
        )
        .await
        .expect("watch");
        // Simulate a dropped connection: the request task's error path marks
        // the watcher for reconnect; the host clock then advances it.
        request_failed(
            &watcher.state,
            &watcher.on_change,
            RemoteError::lost("gone"),
        );
        assert!(watcher.state.lock().expect("state").pending_reconnect);
        watcher.tick(0).await;
        watcher.tick(999).await;
        assert!(watcher.state.lock().expect("state").pending_reconnect);
        assert_eq!(
            watcher.state.lock().expect("state").retry_deadline,
            Some(1000)
        );
        watcher.tick(1000).await;
        // `begin_watch` records the request inside a spawned task.
        tokio::task::yield_now().await;
        assert!(
            calls
                .calls()
                .iter()
                .filter(|name| name.as_str() == "watch")
                .count()
                >= 2
        );
        assert_eq!(watcher.state.lock().expect("state").retry_deadline, None);
    }
}
