# Wiring `pi-env` into the workspace

This slice creates **only** `crates/pi-env/**`. Nothing outside the crate was
edited: the coordinator applies the changes below.

## 1. Workspace membership — root `Cargo.toml`

Add to `[workspace] members`:

```toml
	"crates/pi-env",
```

Add to `[workspace.dependencies]` (only if another crate will depend on it):

```toml
pi-env = { path = "crates/pi-env", version = "0.1.0" }
```

## 2. Dependencies

`crates/pi-env/Cargo.toml` uses only workspace dependencies already pinned in
`Cargo.lock`: `base64`, `dirs`, `serde_json`, `sha2`, `tokio`, `uuid`. **No new
external crate and no `Cargo.lock` change is required.** All lints are
inherited via `[lints] workspace = true`.

## 3. Host wiring in `pi-coding-agent` (`pi remote`)

Wired by the follow-up `task/env-host` slice; see
`crates/pi-coding-agent/src/remote_env.rs`. The crate is imported as a direct
path dependency (`pi-env = { path = "../pi-env" }`) so the workspace root's
`[workspace.dependencies]` stays untouched. Equivalent to step 1 below:

1. Add `pi-env = { workspace = true }` to `crates/pi-coding-agent/Cargo.toml`.
2. Build a daemon connection and hand it to the environment. The crate is
   transport-agnostic: callers pass an `Arc<dyn Daemon>` (or a concrete
   `ProcessDaemon` / `SshConnection::daemon()`):
   ```rust,ignore
   use pi_env::{connection::{CommandSource, ConnectionOptions, ProcessDaemon}, env::{RemoteExecutionEnv, RemoteExecutionEnvOptions}};
   let daemon = Arc::new(ProcessDaemon::new(ConnectionOptions::new(
       CommandSource::fixed([local_daemon(package_root)?.display().to_string()]),
   )));
   let env = RemoteExecutionEnv::new(RemoteExecutionEnvOptions {
       daemon, id: "pi-env:local".into(), cwd: cwd.clone(),
       shell_path: None, shell_env: None, watch: RemoteWatchOptions::default(),
   });
   ```
   For a remote session use `ssh_connection(options).daemon()` (or
   `connect_ssh(options).await`) instead, then the same `RemoteExecutionEnv`.
3. `RemoteExecutionEnv` implements the `ExecutionEnv` trait;
   `LocalExecutionEnv::new(LocalExecutionEnvOptions { daemon, id, cwd })`
   adds daemon-backed `listFiles` for the local case (its `inner()` gives the
   full environment). `list_files_over` can also be called directly.
4. **Host clock (required):** there are no timers in the port. The host must
   call `ProcessDaemon::tick(now_ms)` for ping/keepalive and
   `RemoteWatcher::tick(now_ms)` for watch reconnect backoff, passing a
   monotonic millisecond value. Without these calls, keepalive and reconnect
   do not advance (watch events themselves still arrive via the connection).
5. PTY/process input (`RemoteTerminal::write`, `RemoteProcess::write`,
   `close_stdin`) spawns small fire-and-forget tasks; the host must run these
   on a Tokio runtime (the repo already has a process-wide runtime in
   `pi-coding-agent::runtime`).
6. SSH host-key onboarding is explicit: `scan_host_key`, `accept_hostKey` →
   `apply_accept` + `write_known_hosts`, `forget` → `apply_forget` +
   `write_known_hosts`. No key is accepted automatically.
7. `SshConnection` now implements `Daemon` by forwarding to its inner
   `ProcessDaemon`, so the host keeps one `Arc<dyn Daemon>` for both the local
   and SSH paths. `test_support` is public so host tests can inject
   `FakeDaemon` without a real transport.

## 4. Surface covered (upstream `packages/env/src`)

| Upstream file | Rust module | Public surface |
|---|---|---|
| `connection.ts` | `connection.rs` | frame codec (`encode_frame`/`decode_frame`), `RemoteInfo`, `node_platform`, `Daemon` trait, `ProcessDaemon`, `CommandSource`, `ConnectionOptions`, `RequestOptions`, `Reply`, `Event`, `with_stderr` |
| `errors.ts` | `errors.rs` | `RemoteError`, `FileError`/`FileErrorCode`, `ExecutionError`/`ExecutionErrorCode`, `to_file_error`, `abort_result`, `is_connection_lost`, `AbortSignal` |
| `list-files.ts` | `files.rs` | `list_files_over`, `LocalExecutionEnv`, `FileInfo`, `FileKind`, `FileListing`, `FileListEntry`, `ListFilesOptions`, `FileChange`, `WatchTarget` |
| `remote-env.ts` | `env.rs` | `RemoteExecutionEnv`, `ExecutionEnv`, `resolve_remote_path`, `PathRules`, `Command`, `ShellExecOptions`, `ShellExecResult`, `ShellOutputInfo` |
| `process.ts` | `process.rs` | `open_process`, `RemoteProcess`, `ProcessOptions` |
| `terminal.ts` | `terminal.rs` | `open_terminal`, `RemoteTerminal`, `TerminalOptions`, `TerminalForeground` |
| `watch.ts` | `watch.rs` | `RemoteWatcher`, `RemoteWatchOptions`, `WatchMode`, `WatchEvent`, `parse_watch_event` |
| `ssh.ts` | `ssh.rs` | `SshTarget`, `SshRunner`/`SystemSshRunner`, `ssh_arguments`, `run_ssh`, host-key scan/accept/forget, platform probes + `detect_platform`, `packaged_daemon`/`local_daemon`, `deploy_daemon`, `ssh_connection`/`connect_ssh` (+ `_with` injectable variants) |

`index.ts` re-exports are mirrored by `src/lib.rs`.

## 5. Deviations from upstream (all deliberate, 1:1 on observable strings)

1. **No timers.** Ping/keepalive, abort grace and watch reconnect backoff are
   host-driven through `Daemon::tick` / `RemoteWatcher::tick` with a supplied
   monotonic clock; upstream uses `setTimeout`/`setInterval` + unref'd timers.
2. **Injectable transports.** `Daemon` and `SshRunner` traits replace direct
   `spawn`/`ssh` calls so tests use fakes. Real implementations
   (`ProcessDaemon`, `SystemSshRunner`) are provided.
3. **Async shape.** `exited` is a settled `oneshot` returned from
   `RemoteProcess::exited`/`RemoteTerminal::exited` instead of a live promise;
   the request is awaited inside `open_process`/`open_terminal`. Output still
   streams via event callbacks.
4. **File I/O concurrency.** Pipelined `pread`/`writeChunk` depth, the
   incremental UTF-8 `StringDecoder`, `scanLines` and streaming binary/line
   readers are replaced by sequential chunking + a final UTF-8 decode.
5. **`onOutput(text, info)`** drops upstream's `Context` argument (it was only
   used for abort checks). Environment keys are assumed well-formed UTF-8
   (Rust strings cannot hold lone surrogates, so `toWellFormed` is a no-op).
6. **`LocalExecutionEnv`** delegates to `RemoteExecutionEnv` instead of
   extending Node's `NodeExecutionEnv` (out of scope); its `listFiles` is
   identical.
7. **SSH scratch paths / package root.** `scan_host_key` uses a UUID-named
   temp dir; `packaged_daemon`/`local_daemon` take an explicit package root
   instead of `import.meta.url`. No known-hosts serialization map (the port is
   single-threaded per host), and SSH/upload timeouts are forwarded to the
   runner rather than enforced by timers.
8. **ANSI stripping.** `with_stderr` uses a small CSI/control filter rather
   than Node's `stripVTControlCharacters`; the kept/filtered line set and
   truncation (`STDERR_TAIL_LINES`) match.
9. **Host-key regexes.** The two diagnostic patterns are matched with
   `contains` + a line check instead of regex; the classified codes and exact
   messages are unchanged.
10. **Path fallback.** `file://` resolution decodes percent escapes and drops
    a local host; a non-local host becomes a path rather than an error.

## 6. Tests

46 unit tests (inline `mod tests`, last in each file) covering: frame codec,
error-code mapping and strings, env resolution (home/relative/absolute/
`file://`/Windows drive-cwds), file listing parse + validation + feature gate,
read path, exec output/timeout validation/cleanup, process run + output +
feature gate, terminal output/foreground/input, watch ready/change/overflow/
error/reconnect, and SSH argument hardening, host-key diagnostics, platform
probes, deploy and host-key editing. All backends are scripted fakes — **no
real SSH, socket or daemon process is started in any test**. The real
`ProcessDaemon`/`SystemSshRunner` need a real daemon binary for an integration
smoke and are intentionally not exercised by unit tests.
