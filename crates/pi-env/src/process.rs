//! Plain-pipe processes in a pi-env daemon.
//!
//! Ground truth: `packages/env/src/process.ts:1-118`. Ported behaviour:
//! `WRITE_CHUNK` (`:4`), `ProcessOptions` (`:6-18`), `RemoteProcess`'s
//! `write`/`closeStdin`/`close` (`:26-75`), the "process" feature gate with
//! its exact error (`:81-84`), and the `onStart`/`onEvent` handshake that
//! surfaces stdout/stderr and the pid (`:85-118`).
//!
//! # Deviation
//!
//! Upstream's `exited` is a promise that settles when the daemon's request
//! ends while `openProcess` resolves earlier on `started`. The port awaits
//! the request in `open_process` and hands back an already-settled [`oneshot`]
//! receiver through [`RemoteProcess::exited`]; output still streams through
//! the event callback as frames arrive. This removes the need for a timer or
//! a second task per process.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::oneshot;

use crate::{
    JsonMap,
    connection::{AbortSignal, Daemon, Event, RequestOptions},
    errors::RemoteError,
};

/// Input larger than this goes to the daemon in several writes
/// (`process.ts:4`).
pub const WRITE_CHUNK: usize = 1024 * 1024;

/// Callback receiving raw stdout/stderr bytes in order.
pub type BytesSink = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Options for `openProcess` (`process.ts:6-18`).
pub struct ProcessOptions {
    /// Working directory of the program; it must exist.
    pub cwd: String,
    /// Program and arguments, run directly without a shell.
    pub argv: Vec<String>,
    /// Added to the daemon's environment, or the whole environment with
    /// `inherit_env: false`.
    pub env: Vec<(String, String)>,
    /// Whether to inherit the daemon's environment (`true` upstream).
    pub inherit_env: bool,
    /// The program's stdout, raw bytes in order.
    pub on_stdout: BytesSink,
    /// The program's stderr, raw bytes in order.
    pub on_stderr: Option<BytesSink>,
}

impl ProcessOptions {
    /// Options with upstream's defaults (`inheritEnv` and stderr unset).
    #[must_use]
    pub fn new(cwd: impl Into<String>, argv: Vec<String>, on_stdout: BytesSink) -> Self {
        Self {
            cwd: cwd.into(),
            argv,
            env: Vec::new(),
            inherit_env: true,
            on_stdout,
            on_stderr: None,
        }
    }

    fn to_json(&self) -> JsonMap {
        let mut json = JsonMap::new();
        json.insert("cwd".into(), Value::String(self.cwd.clone()));
        json.insert(
            "argv".into(),
            Value::Array(self.argv.iter().cloned().map(Value::String).collect()),
        );
        if !self.env.is_empty() {
            let env = self
                .env
                .iter()
                .cloned()
                .map(|(key, value)| (key, Value::String(value)))
                .collect();
            json.insert("env".into(), Value::Object(env));
        }
        if !self.inherit_env {
            json.insert("inheritEnv".into(), Value::Bool(false));
        }
        json
    }
}

/// A program running with plain pipes in the daemon (`RemoteProcess`,
/// `process.ts:26-75`).
pub struct RemoteProcess {
    /// The remote pid reported by the `started` event.
    pub pid: u64,
    exited: oneshot::Receiver<Result<i64, RemoteError>>,
    daemon: Arc<dyn Daemon>,
    id: u64,
    session: u64,
    abort: AbortSignal,
}

impl std::fmt::Debug for RemoteProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RemoteProcess {{ pid: {}, id: {} }}", self.pid, self.id)
    }
}

impl RemoteProcess {
    fn request(&self, op: &str, payload: Vec<u8>) {
        let mut json = JsonMap::new();
        json.insert("target".into(), Value::from(self.id));
        let daemon = self.daemon.clone();
        let session = self.session;
        tokio::spawn(async move {
            // Stdin already closed, the program ended, or a lost connection:
            // `exited` reports the end (`process.ts:69-73`).
            let _ = daemon
                .request(
                    op,
                    json,
                    RequestOptions {
                        payload,
                        session: Some(session),
                        ..Default::default()
                    },
                )
                .await;
        });
    }

    /// Write to the program's stdin in order, chunked at [`WRITE_CHUNK`].
    pub fn write(&self, data: &[u8]) {
        for chunk in data.chunks(WRITE_CHUNK) {
            self.request("processWrite", chunk.to_vec());
        }
    }

    /// Close the program's stdin after previously written input.
    pub fn close_stdin(&self) {
        self.request("processCloseStdin", Vec::new());
    }

    /// Kill the program and its process group (`process.ts:65-67`).
    pub fn close(&self) {
        self.abort.abort();
    }

    /// The exit code, or the daemon's failure (`exited`, `process.ts:32`).
    pub async fn exited(&mut self) -> Result<i64, RemoteError> {
        match self.exited.await {
            Ok(result) => result,
            Err(_) => Err(RemoteError::lost("pi-env connection lost")),
        }
    }
}

/// Start a program with plain pipes in the daemon (`process.ts:81-118`).
pub async fn open_process(
    daemon: Arc<dyn Daemon>,
    options: ProcessOptions,
) -> Result<RemoteProcess, RemoteError> {
    if !daemon.info(None).await?.has_feature("process") {
        return Err(RemoteError::new(
            "not_supported",
            "The pi-env daemon cannot run processes; update it",
        ));
    }
    let abort = AbortSignal::new();
    let start: Arc<Mutex<Option<(u64, u64, u64)>>> = Arc::new(Mutex::new(None));
    let on_start_state = start.clone();
    let on_start: Arc<dyn Fn(u64, u64) + Send + Sync> = Arc::new(move |id, session| {
        let mut state = on_start_state.lock().expect("start lock");
        let pid = state.as_ref().map_or(0, |&(_, _, pid)| pid);
        *state = Some((id, session, pid));
    });
    let on_stdout = options.on_stdout.clone();
    let on_stderr = options.on_stderr.clone();
    let on_event_state = start.clone();
    let on_event: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(move |event| match event.kind() {
        Some("started") => {
            let pid = event.json.get("pid").and_then(Value::as_u64).unwrap_or(0);
            let mut state = on_event_state.lock().expect("start lock");
            if let Some((id, session, _)) = *state {
                *state = Some((id, session, pid));
            }
        }
        Some("output") => {
            let sink = if event.json.get("stream").and_then(Value::as_str) == Some("stderr") {
                on_stderr.as_ref()
            } else {
                Some(&on_stdout)
            };
            if let Some(sink) = sink {
                sink(&event.payload);
            }
        }
        _ => {}
    });
    let reply = daemon
        .request(
            "process",
            options.to_json(),
            RequestOptions {
                on_event: Some(on_event),
                on_start: Some(on_start),
                abort: Some(abort.clone()),
                ..Default::default()
            },
        )
        .await;
    let started = *start.lock().expect("start lock");
    let (exit_sender, exited) = oneshot::channel();
    match (reply, started) {
        (Ok(reply), Some((id, session, pid))) => {
            let code = reply
                .json
                .get("exitCode")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let _ = exit_sender.send(Ok(code));
            Ok(RemoteProcess {
                pid,
                exited,
                daemon,
                id,
                session,
                abort,
            })
        }
        (Ok(_), None) => Err(RemoteError::new(
            "unknown",
            "pi-env daemon did not report a started process",
        )),
        (Err(error), Some((id, session, pid))) => {
            // Failing after `started` is reported by `exited`, not by open.
            let _ = exit_sender.send(Err(error));
            Ok(RemoteProcess {
                pid,
                exited,
                daemon,
                id,
                session,
                abort,
            })
        }
        (Err(error), None) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeDaemon, info_with_features, reply};

    fn sink() -> (BytesSink, Arc<Mutex<Vec<Vec<u8>>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let shared = seen.clone();
        (
            Arc::new(move |bytes: &[u8]| shared.lock().expect("sink lock").push(bytes.to_vec())),
            seen,
        )
    }

    fn start_event(pid: u64) -> Event {
        let mut json = JsonMap::new();
        json.insert("kind".into(), Value::String("started".into()));
        json.insert("pid".into(), Value::from(pid));
        Event {
            json,
            payload: Vec::new(),
        }
    }

    fn output_event(stream: &str, bytes: &[u8]) -> Event {
        let mut json = JsonMap::new();
        json.insert("kind".into(), Value::String("output".into()));
        json.insert("stream".into(), Value::String(stream.into()));
        Event {
            json,
            payload: bytes.to_vec(),
        }
    }

    #[tokio::test]
    async fn refuses_daemons_without_the_process_feature() {
        let daemon = Arc::new(FakeDaemon::new(info_with_features(&[])));
        let (out, _) = sink();
        let error = open_process(daemon, ProcessOptions::new("/x", vec!["echo".into()], out))
            .await
            .expect_err("not supported");
        assert_eq!(error.code, "not_supported");
        assert_eq!(
            error.message,
            "The pi-env daemon cannot run processes; update it"
        );
    }

    #[tokio::test]
    async fn streams_output_tracks_pid_and_reports_exit_code() {
        let daemon = Arc::new(FakeDaemon::new(info_with_features(&["process"])));
        let mut ok = JsonMap::new();
        ok.insert("exitCode".into(), Value::from(3));
        daemon.push_reply(
            "process",
            reply(ok, Vec::new()),
            vec![
                start_event(77),
                output_event("stdout", b"hello"),
                output_event("stderr", b"oops"),
            ],
            Some((11, 1)),
        );
        let (out, seen_out) = sink();
        let (err, seen_err) = sink();
        let mut options = ProcessOptions::new("/x", vec!["run".into()], out);
        options.on_stderr = Some(err);
        let mut process = open_process(daemon, options).await.expect("process");
        assert_eq!(process.pid, 77);
        assert_eq!(*seen_out.lock().expect("sink"), vec![b"hello".to_vec()]);
        assert_eq!(*seen_err.lock().expect("sink"), vec![b"oops".to_vec()]);
        assert_eq!(process.exited().await.expect("exit"), 3);
    }

    #[tokio::test]
    async fn write_and_close_stdin_are_sent_to_the_daemon() {
        let daemon = Arc::new(FakeDaemon::new(info_with_features(&["process"])));
        let mut ok = JsonMap::new();
        ok.insert("exitCode".into(), Value::from(0));
        daemon.push_reply(
            "process",
            reply(ok, Vec::new()),
            vec![start_event(1)],
            Some((5, 1)),
        );
        daemon.push_reply(
            "processWrite",
            reply(JsonMap::new(), Vec::new()),
            Vec::new(),
            None,
        );
        daemon.push_reply(
            "processCloseStdin",
            reply(JsonMap::new(), Vec::new()),
            Vec::new(),
            None,
        );
        let (out, _) = sink();
        let process = open_process(
            daemon.clone(),
            ProcessOptions::new("/x", vec!["cat".into()], out),
        )
        .await
        .expect("process");
        process.write(b"input");
        process.close_stdin();
        // Let the spawned writer tasks complete before inspecting calls.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(
            daemon.calls(),
            vec!["info", "process", "processWrite", "processCloseStdin"]
        );
    }
}
