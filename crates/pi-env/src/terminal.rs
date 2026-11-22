//! Programmes running in a pseudo-terminal of a pi-env daemon.
//!
//! Ground truth: `packages/env/src/terminal.ts:1-127`. Ported behaviour:
//! `WRITE_CHUNK` (`:4`), `TerminalOptions` and `TerminalForeground`
//! (`:6-24`), `RemoteTerminal`'s `write`/`resize`/`close`/`foreground`
//! (`:30-90`), and `openTerminal`'s `onStart`/`onEvent` handshake
//! (`:92-127`).
//!
//! # Deviation
//!
//! As in [`crate::process`], `exited` is a settled oneshot rather than a live
//! promise; output still arrives through the event callback.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::oneshot;

use crate::{
    AbortSignal, JsonMap,
    connection::{Daemon, Event, RequestOptions},
    errors::RemoteError,
};

/// Input larger than this goes to the daemon in several writes
/// (`terminal.ts:4`).
pub const WRITE_CHUNK: usize = 1024 * 1024;

/// Callback receiving raw terminal output in order.
pub type BytesSink = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Options for `openTerminal` (`terminal.ts:6-18`).
pub struct TerminalOptions {
    /// Working directory of the program; it must exist.
    pub cwd: String,
    /// Program and arguments; `None` means the user's login shell.
    pub argv: Option<Vec<String>>,
    /// Added to the daemon's environment, or the whole environment when
    /// `inherit_env` is false. Set `TERM` here.
    pub env: Vec<(String, String)>,
    /// Whether to inherit the daemon's environment (`true` upstream).
    pub inherit_env: bool,
    /// Terminal columns.
    pub cols: u64,
    /// Terminal rows.
    pub rows: u64,
    /// The terminal's raw output, in order.
    pub on_data: BytesSink,
}

impl TerminalOptions {
    /// Options for the login shell with upstream's defaults.
    #[must_use]
    pub fn new(cwd: impl Into<String>, cols: u64, rows: u64, on_data: BytesSink) -> Self {
        Self {
            cwd: cwd.into(),
            argv: None,
            env: Vec::new(),
            inherit_env: true,
            cols,
            rows,
            on_data,
        }
    }

    fn to_json(&self) -> JsonMap {
        let mut json = JsonMap::new();
        json.insert("cwd".into(), Value::String(self.cwd.clone()));
        if let Some(argv) = &self.argv {
            json.insert(
                "argv".into(),
                Value::Array(argv.iter().cloned().map(Value::String).collect()),
            );
        }
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
        json.insert("cols".into(), Value::from(self.cols));
        json.insert("rows".into(), Value::from(self.rows));
        json
    }
}

/// The foreground job of a terminal (`terminal.ts:20-24`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalForeground {
    /// Process group id of the foreground job.
    pub pid: u64,
    /// The leader's name, when the system reports one.
    pub name: Option<String>,
}

/// A program running in a daemon PTY (`RemoteTerminal`, `terminal.ts:30-90`).
pub struct RemoteTerminal {
    /// The remote pid reported by the `started` event.
    pub pid: u64,
    exited: oneshot::Receiver<Result<i64, RemoteError>>,
    daemon: Arc<dyn Daemon>,
    id: u64,
    session: u64,
    abort: AbortSignal,
}

impl std::fmt::Debug for RemoteTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RemoteTerminal {{ pid: {}, id: {} }}", self.pid, self.id)
    }
}

impl RemoteTerminal {
    fn request(&self, op: &str, json: JsonMap, payload: Vec<u8>) {
        let mut body = json;
        body.insert("target".into(), Value::from(self.id));
        let daemon = self.daemon.clone();
        let session = self.session;
        let op = op.to_string();
        tokio::spawn(async move {
            // A terminal that ended or a lost connection: `exited` reports it
            // (`terminal.ts:84-89`).
            let _ = daemon
                .request(
                    &op,
                    body,
                    RequestOptions {
                        payload,
                        session: Some(session),
                        ..Default::default()
                    },
                )
                .await;
        });
    }

    /// Send input, chunked at [`WRITE_CHUNK`]; input after the end is dropped.
    pub fn write(&self, data: &[u8]) {
        for chunk in data.chunks(WRITE_CHUNK) {
            self.request("ptyWrite", JsonMap::new(), chunk.to_vec());
        }
    }

    /// Change the size; the program receives `SIGWINCH`.
    pub fn resize(&self, cols: u64, rows: u64) {
        let mut json = JsonMap::new();
        json.insert("cols".into(), Value::from(cols));
        json.insert("rows".into(), Value::from(rows));
        self.request("ptyResize", json, Vec::new());
    }

    /// Kill the program and everything in its session.
    pub fn close(&self) {
        self.abort.abort();
    }

    /// The job in front when it is not the program itself
    /// (`foreground`, `terminal.ts:73-82`).
    pub async fn foreground(&self) -> Result<Option<TerminalForeground>, RemoteError> {
        let mut json = JsonMap::new();
        json.insert("target".into(), Value::from(self.id));
        let reply = self
            .daemon
            .request(
                "ptyForeground",
                json,
                RequestOptions {
                    session: Some(self.session),
                    ..Default::default()
                },
            )
            .await?;
        let Some(foreground) = reply.json.get("foreground").and_then(Value::as_object) else {
            return Ok(None);
        };
        let pid = foreground.get("pid").and_then(Value::as_u64).unwrap_or(0);
        let name = foreground
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(Some(TerminalForeground { pid, name }))
    }

    /// The exit code, or the daemon's failure (`exited`, `terminal.ts:32`).
    pub async fn exited(&mut self) -> Result<i64, RemoteError> {
        // Awaiting by reference: the receiver stays in `self` for a second
        // poll to observe the same end.
        match (&mut self.exited).await {
            Ok(result) => result,
            Err(_) => Err(RemoteError::lost("pi-env connection lost")),
        }
    }
}

/// Start a program in a new pseudo-terminal (`terminal.ts:93-127`).
pub async fn open_terminal(
    daemon: Arc<dyn Daemon>,
    options: TerminalOptions,
) -> Result<RemoteTerminal, RemoteError> {
    let abort = AbortSignal::new();
    let start: Arc<Mutex<Option<(u64, u64, u64)>>> = Arc::new(Mutex::new(None));
    let on_start_state = start.clone();
    let on_start: Arc<dyn Fn(u64, u64) + Send + Sync> = Arc::new(move |id, session| {
        let mut state = on_start_state.lock().expect("start lock");
        let pid = state.as_ref().map_or(0, |&(_, _, pid)| pid);
        *state = Some((id, session, pid));
    });
    let on_data = options.on_data.clone();
    let on_event_state = start.clone();
    let on_event: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(move |event| match event.kind() {
        Some("output") => on_data(&event.payload),
        Some("started") => {
            let pid = event.json.get("pid").and_then(Value::as_u64).unwrap_or(0);
            let mut state = on_event_state.lock().expect("start lock");
            if let Some((id, session, _)) = *state {
                *state = Some((id, session, pid));
            }
        }
        _ => {}
    });
    let reply = daemon
        .request(
            "pty",
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
            Ok(RemoteTerminal {
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
            "pi-env daemon did not report a started terminal",
        )),
        (Err(error), Some((id, session, pid))) => {
            let _ = exit_sender.send(Err(error));
            Ok(RemoteTerminal {
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

    fn started_event(pid: u64) -> Event {
        let mut json = JsonMap::new();
        json.insert("kind".into(), Value::String("started".into()));
        json.insert("pid".into(), Value::from(pid));
        Event {
            json,
            payload: Vec::new(),
        }
    }

    fn output_event(bytes: &[u8]) -> Event {
        let mut json = JsonMap::new();
        json.insert("kind".into(), Value::String("output".into()));
        Event {
            json,
            payload: bytes.to_vec(),
        }
    }

    #[tokio::test]
    async fn streams_output_and_reports_foreground() {
        let daemon = Arc::new(FakeDaemon::new(info_with_features(&[])));
        let mut ok = JsonMap::new();
        ok.insert("exitCode".into(), Value::from(0));
        daemon.push_reply(
            "pty",
            reply(ok, Vec::new()),
            vec![started_event(9), output_event(b"prompt$ ")],
            Some((4, 2)),
        );
        let mut foreground = JsonMap::new();
        foreground.insert("pid".into(), Value::from(123));
        foreground.insert("name".into(), Value::String("vim".into()));
        let mut fg_reply = JsonMap::new();
        fg_reply.insert("foreground".into(), Value::Object(foreground));
        daemon.push_reply(
            "ptyForeground",
            reply(fg_reply, Vec::new()),
            Vec::new(),
            None,
        );
        let (data, seen) = sink();
        let terminal = open_terminal(daemon, TerminalOptions::new("/x", 80, 24, data))
            .await
            .expect("terminal");
        assert_eq!(terminal.pid, 9);
        assert_eq!(*seen.lock().expect("sink"), vec![b"prompt$ ".to_vec()]);
        assert_eq!(
            terminal.foreground().await.expect("foreground"),
            Some(TerminalForeground {
                pid: 123,
                name: Some("vim".into())
            })
        );
    }

    #[tokio::test]
    async fn foreground_is_none_without_a_job() {
        let daemon = Arc::new(FakeDaemon::new(info_with_features(&[])));
        let mut ok = JsonMap::new();
        ok.insert("exitCode".into(), Value::from(0));
        daemon.push_reply(
            "pty",
            reply(ok, Vec::new()),
            vec![started_event(9)],
            Some((4, 2)),
        );
        daemon.push_reply(
            "ptyForeground",
            reply(JsonMap::new(), Vec::new()),
            Vec::new(),
            None,
        );
        let (data, _) = sink();
        let terminal = open_terminal(daemon, TerminalOptions::new("/x", 80, 24, data))
            .await
            .expect("terminal");
        assert_eq!(terminal.foreground().await.expect("foreground"), None);
    }

    #[tokio::test]
    async fn write_resize_and_close_use_the_terminal_session() {
        let daemon = Arc::new(FakeDaemon::new(info_with_features(&[])));
        let mut ok = JsonMap::new();
        ok.insert("exitCode".into(), Value::from(0));
        daemon.push_reply(
            "pty",
            reply(ok, Vec::new()),
            vec![started_event(9)],
            Some((4, 2)),
        );
        daemon.push_reply(
            "ptyWrite",
            reply(JsonMap::new(), Vec::new()),
            Vec::new(),
            None,
        );
        daemon.push_reply(
            "ptyResize",
            reply(JsonMap::new(), Vec::new()),
            Vec::new(),
            None,
        );
        let (data, _) = sink();
        let terminal = open_terminal(daemon.clone(), TerminalOptions::new("/x", 80, 24, data))
            .await
            .expect("terminal");
        terminal.write(b"ls\n");
        terminal.resize(120, 40);
        terminal.close();
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(daemon.calls(), vec!["pty", "ptyWrite", "ptyResize"]);
    }
}
