//! Local/remote execution environment for pi.
//!
//! Rust port of `@earendil-works/pi-env` (TypeScript source:
//! `packages/env/src`, pinned to upstream 1.5.2). The package is the bridge
//! between a pi session and a *pi-env daemon*: a small process that performs
//! file and process operations next to the files, reached either locally or
//! over `ssh`. The client wraps that daemon in an `ExecutionEnv` so local and
//! remote sessions share one file/process surface.
//!
//! # Module map (one Rust module per upstream file)
//!
//! * [`errors`] — `errors.ts`: `abortResult`, `toFileError`, plus the
//!   `RemoteError` surface declared in `connection.ts:31-44`.
//! * [`connection`] — `connection.ts`: frame codec, `RemoteInfo`,
//!   `nodePlatform`, `Connection`/`Daemon` request/response correlation and
//!   the real spawned-process transport.
//! * [`files`] — `list-files.ts` and the shared file record types:
//!   `listFilesOver`, `LocalExecutionEnv`, `FileInfo`, `FileChange`.
//! * [`env`] — `remote-env.ts`: `RemoteExecutionEnv`, path resolution and
//!   file/process operations through a daemon.
//! * [`process`] — `process.ts`: `openProcess`/`RemoteProcess` (plain pipes).
//! * [`terminal`] — `terminal.ts`: `openTerminal`/`RemoteTerminal` (PTY).
//! * [`watch`] — `watch.ts`: `RemoteWatcher` with reconnect and overflow.
//! * [`ssh`] — `ssh.ts`: `SshTarget`, `sshArguments`, host-key handling,
//!   platform detection, daemon deploy and the SSH connection shape.
//!
//! # Deviations (full ledger in `WIRING.md`)
//!
//! Upstream relies on Node timers (`setTimeout`, `setInterval`) for ping,
//! abort grace, reconnect backoff and watch polling. The port has **no
//! timers**: those state machines are driven by explicit host calls —
//! [`watch::RemoteWatcher::tick`] and [`connection::ProcessDaemon::tick`] —
//! and the host supplies a logical clock. Network and SSH are behind the
//! [`connection::Daemon`] and [`ssh::SshRunner`] traits so tests use fake
//! backends and never open a real connection.

// `RemoteError` is a transparent, by-value mirror of upstream's JS error
// object (`connection.ts:31-44`). Boxing its fields to satisfy this lint would
// change every public `Result<_, RemoteError>` signature in the port, so the
// deliberate 1:1 shape is kept and the lint is silenced crate-wide.
#![allow(clippy::result_large_err)]

/// Request/reply correlation, frame codec and the injectable daemon transport
/// (`connection.ts`).
pub mod connection;
/// Path resolution and file/process operations through a daemon
/// (`remote-env.ts`).
pub mod env;
/// The Node-style error mapping (`errors.ts`) and error surfaces.
pub mod errors;
/// Shared file types plus `listFilesOver` and `LocalExecutionEnv`
/// (`list-files.ts`).
pub mod files;
/// Plain-pipe remote processes (`process.ts`).
pub mod process;
/// The SSH target, host keys, platform probe and connection shape (`ssh.ts`).
pub mod ssh;
/// PTY remote terminals (`terminal.ts`).
pub mod terminal;
/// Scripted daemon used by the unit tests (test-only, no upstream file).
#[cfg(test)]
pub(crate) mod test_support;
/// Remote file watcher with host-driven reconnect (`watch.ts`).
pub mod watch;

/// A JSON object as used by the daemon wire protocol. `serde_json` is built
/// with `preserve_order`, matching JavaScript object key order.
pub type JsonMap = serde_json::Map<String, serde_json::Value>;
/// A single JSON value carried in a protocol frame.
pub type Json = serde_json::Value;

pub use connection::{
    CommandSource, ConnectionOptions, Daemon, Event, EventSink, ProcessDaemon, RemoteInfo, Reply,
    RequestOptions, StartSink, node_platform,
};
pub use env::{ExecutionEnv, RemoteExecutionEnv, RemoteExecutionEnvOptions};
pub use errors::{
    AbortSignal, ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, RemoteError,
    is_connection_lost,
};
pub use files::{
    FileChange, FileInfo, FileKind, FileListEntry, FileListing, ListFilesOptions,
    LocalExecutionEnv, LocalExecutionEnvOptions, WatchTarget, list_files_over,
};
pub use process::{ProcessOptions, RemoteProcess, open_process};
pub use ssh::{
    HostKeyChangedError, HostKeyUnknownError, RemotePlatform, SshConnectOptions, SshError,
    SshRunner, SshTarget, SystemSshRunner, connect_ssh, detect_platform, local_daemon,
    packaged_daemon, ssh_arguments, ssh_connection,
};
pub use terminal::{RemoteTerminal, TerminalForeground, TerminalOptions, open_terminal};
pub use watch::{RemoteWatchOptions, RemoteWatcher, WatchEvent};
