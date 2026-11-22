//! Scripted backends used by the crate's unit tests (no upstream file).
//!
//! The port deliberately keeps networking behind [`Daemon`](crate::connection::Daemon)
//! and [`SshRunner`](crate::ssh::SshRunner). These fakes return canned replies
//! and events, so no test opens a socket, spawns `ssh` or contacts a host.
//! Calls are recorded *before* the returned future is polled, which makes
//! fire-and-forget operations such as `RemoteProcess::write` deterministic.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex as StdMutex},
};

use serde_json::Value;

use crate::{
    JsonMap,
    connection::{BoxFuture, Daemon, Event, RemoteInfo, Reply, RequestOptions},
    errors::RemoteError,
    ssh::{CommandOutput, SshRunner},
};

/// Build the standard fake daemon info with a feature list.
#[must_use]
pub fn info_with_features(features: &[&str]) -> RemoteInfo {
    RemoteInfo {
        protocol: 1,
        version: "test".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        home: "/home/pi".into(),
        tmpdir: "/tmp".into(),
        separator: "/".into(),
        cwd: "/work".into(),
        drive_cwds: JsonMap::new(),
        pid: 1,
        features: Some(
            features
                .iter()
                .map(|feature| (*feature).to_owned())
                .collect(),
        ),
    }
}

/// A reply with a fixed session id.
#[must_use]
pub fn reply(json: JsonMap, payload: Vec<u8>) -> Reply {
    Reply {
        json,
        payload,
        session: 1,
    }
}

struct Scripted {
    op: String,
    outcome: Result<Reply, RemoteError>,
    events: Vec<Event>,
    start: Option<(u64, u64)>,
}

/// A daemon that replays scripted replies and events.
pub struct FakeDaemon {
    info: RemoteInfo,
    script: StdMutex<VecDeque<Scripted>>,
    calls: StdMutex<Vec<String>>,
    killed: StdMutex<Vec<(u64, u64)>>,
}

impl FakeDaemon {
    /// A daemon reporting `info`.
    #[must_use]
    pub fn new(info: RemoteInfo) -> Self {
        Self {
            info,
            script: StdMutex::new(VecDeque::new()),
            calls: StdMutex::new(Vec::new()),
            killed: StdMutex::new(Vec::new()),
        }
    }

    /// Queue a reply, its events and an optional `onStart` notification.
    pub fn push_reply(
        &self,
        op: &str,
        outcome: Reply,
        events: Vec<Event>,
        start: Option<(u64, u64)>,
    ) {
        self.script
            .lock()
            .expect("script lock")
            .push_back(Scripted {
                op: op.into(),
                outcome: Ok(outcome),
                events,
                start,
            });
    }

    /// Queue an error reply (available for future error-path tests).
    #[allow(dead_code)]
    pub fn push_error(&self, op: &str, error: RemoteError) {
        self.script
            .lock()
            .expect("script lock")
            .push_back(Scripted {
                op: op.into(),
                outcome: Err(error),
                events: Vec::new(),
                start: None,
            });
    }

    /// The operation names seen so far, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls lock").clone()
    }

    /// The `(id, session)` pairs passed to `kill`.
    #[must_use]
    pub fn killed(&self) -> Vec<(u64, u64)> {
        self.killed.lock().expect("killed lock").clone()
    }
}

impl Daemon for FakeDaemon {
    fn info(
        &self,
        _abort: Option<crate::connection::AbortSignal>,
    ) -> BoxFuture<'_, Result<RemoteInfo, RemoteError>> {
        self.calls.lock().expect("calls lock").push("info".into());
        let info = self.info.clone();
        Box::pin(async move { Ok(info) })
    }

    fn request(
        &self,
        op: &str,
        _json: JsonMap,
        options: RequestOptions,
    ) -> BoxFuture<'_, Result<Reply, RemoteError>> {
        self.calls.lock().expect("calls lock").push(op.to_owned());
        let scripted = {
            let mut script = self.script.lock().expect("script lock");
            let position = script.iter().position(|entry| entry.op == op);
            position.map(|index| script.remove(index).expect("index was found"))
        };
        match scripted {
            Some(entry) => Box::pin(async move {
                if let Some((id, session)) = entry.start {
                    if let Some(on_start) = &options.on_start {
                        on_start(id, session);
                    }
                }
                if let Some(on_event) = &options.on_event {
                    for event in entry.events {
                        on_event(event);
                    }
                }
                entry.outcome
            }),
            None => {
                let message = format!("No scripted reply for {op}");
                Box::pin(async move { Err(RemoteError::lost(message)) })
            }
        }
    }

    fn kill(&self, id: u64, session: u64) -> BoxFuture<'_, ()> {
        self.killed.lock().expect("killed lock").push((id, session));
        Box::pin(async {})
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// An SSH runner that replays canned command outputs.
pub struct FakeSshRunner {
    outputs: Arc<StdMutex<VecDeque<CommandOutput>>>,
    commands: Arc<StdMutex<Vec<(String, Vec<String>, Vec<u8>)>>>,
}

impl FakeSshRunner {
    /// An empty runner.
    #[must_use]
    pub fn new() -> Self {
        Self {
            outputs: Arc::new(StdMutex::new(VecDeque::new())),
            commands: Arc::new(StdMutex::new(Vec::new())),
        }
    }

    /// Queue one command output.
    pub fn push_output(&self, output: CommandOutput) {
        self.outputs.lock().expect("outputs lock").push_back(output);
    }

    /// Queue one stdout string with exit code 0.
    #[allow(dead_code)]
    pub fn push_stdout(&self, stdout: &str) {
        self.push_output(CommandOutput {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: Some(0),
        });
    }

    /// The commands seen so far: `(program, args, stdin)`.
    #[must_use]
    pub fn commands(&self) -> Vec<(String, Vec<String>, Vec<u8>)> {
        self.commands.lock().expect("commands lock").clone()
    }
}

impl Default for FakeSshRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl SshRunner for FakeSshRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        stdin: &[u8],
        _timeout_ms: u64,
    ) -> BoxFuture<'_, Result<CommandOutput, RemoteError>> {
        self.commands.lock().expect("commands lock").push((
            program.to_owned(),
            args.to_vec(),
            stdin.to_vec(),
        ));
        let outputs = self.outputs.clone();
        Box::pin(async move {
            Ok(outputs
                .lock()
                .expect("outputs lock")
                .pop_front()
                .unwrap_or_else(|| CommandOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: Some(0),
                }))
        })
    }
}

/// A JSON string value, for concise test fixtures.
#[allow(dead_code)]
#[must_use]
pub fn json_string(value: &str) -> Value {
    Value::String(value.into())
}
