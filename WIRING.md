# `!` bash via PTY — coordinator wiring notes

Branch `feat/bash-pty`. Scope: the interactive `!` command only. The agent
tool seam (`crates/pi-coding-agent/src/tools/bash.rs::execute`) is unchanged
and stays on upstream's pipe path.

## What landed

- `crates/pi-coding-agent/src/tools/bash_pty.rs` (new): PTY execution with
  streaming, sanitization, rolling-buffer + temp-file truncation, interrupt,
  and exit-code capture. Declared as `#[path]` submodule of `tools/bash.rs`
  (`crate::tools::bash::bash_pty`) so `tools/mod.rs` needed no edit.
- `crates/pi-coding-agent/src/tools/bash.rs`: doc link + the `#[path]` module
  declaration. Nothing else in that file changed.
- `crates/pi-coding-agent/src/modes/interactive/interactive_mode.rs`:
  - `BashRun` state + `bash_run` field (`isBashRunning`), initialized to `None`;
  - `submit`'s `!` arm rejects a second command while one is running and
    restores the draft (upstream `interactive-mode.ts:3476-3480`);
  - `run_bash` starts a PTY run on a worker thread and mounts the live tool
    row up front;
  - `pump_bash_run` (called from `pump`) applies streamed chunks to that row
    and persists `AgentMessage::BashExecution` on `Done`, including
    `cancelled`, `truncated`, and `full_output_path`;
  - `dispatch_stdin_events` forwards raw bytes to the PTY while a run is
    active and maps `\x03` / `\x1b` to `interrupt_bash()` (which sends a real
    `^C` to the terminal and escalates to `kill` after 2 s).

## Host / crossterm hooks already wired (please verify when gating)

No new dependency and no root `Cargo.toml` / `Cargo.lock` changes. The host
loop already owns raw stdin on its own thread; the new hook in
`dispatch_stdin_events` is the only place raw bytes are diverted. Please
check these two points during the full gate:

1. **Raw mode / termios.** The editor's crossterm raw-mode state stays active
   while a command runs; raw bytes are forwarded into `script`'s stdin, which
   writes them to the PTY master. A child that switches the terminal into
   canonical/echo mode (e.g. `read`) may change what the host sees after the
   run. If `pi` gains a terminal handoff API, that is the place to restore
   the host's termios after `Done`. No `stty` calls are made by this slice.
2. **Ctrl-C top level.** The hook only consumes `\x03`/`\x1b` while
   `bash_run` is live; the existing twice-within-500 ms exit path is
   untouched. A bare `^C` with no command still follows the old path.

## Deliberate deviations from upstream 1.5.2

- **There is no PTY upstream.** `bash.ts:129` spawns with
  `stdio: [pipe|ignore, pipe, pipe]`; `!` is non-interactive. This slice adds
  a PTY so the command sees a real tty (stdin attached, `\n` → `\r\n`,
  job-control signals). The pipe path remains for the agent tool.
- **PTY via `script(1)` (util-linux), not FFI.** `openpty`/`grantpt`/`forkpty`
  need `unsafe`, which the workspace forbids (`unsafe_code = "forbid"`), and
  no PTY crate is a dependency. `script -q -e -c "<shell> -c '<cmd>'"
  /dev/null` allocates the pty, merges stdout/stderr, forwards stdin and
  signals, and returns the child's exit code via `-e`. `script` must be on
  `PATH`; otherwise the run records a clear error (the tests skip when it is
  absent). CI is Linux, where util-linux is standard.
- **termios:** `script` uses the system default pty line discipline; it does
  not inherit the host's raw mode or the host's window size verbatim (the
  pty gets `script`'s size). No ioctl/TIOCSWINSZ forwarding is done here —
  the coordinator can add a resize hook through `BashPtyControl` if needed.
- **No `splitIncompleteAnsiSuffix`.** Upstream (`bash-executor.ts:110-112`)
  carries an unfinished escape sequence across chunk boundaries; the port's
  shared `strip_ansi` helper works per chunk (same limitation as the
  extension shell bridge, `extensions/mod.rs:308-326`). A CSI split exactly
  at a read boundary can leak its prefix into the streamed row; the final
  recorded output re-derives from the same chunks.
- **Byte accounting is by `char`.** The existing extension bridge
  (`extensions/mod.rs:297-299`) counts chars where upstream counts UTF-16
  units; this module follows that precedent.
- **Shell configuration.** The port has no `shellPath` / command-prefix
  settings yet, so the PTY uses the default `/bin/bash` → `bash` → `sh`
  discovery (`utils/shell.ts:108-118`) with no prefix. The agent tool keeps
  its existing `ToolContext` shell handling.
- **Environment.** The child inherits the process environment; upstream's
  `getShellEnv()` scrub is not yet available on this seam.
- **Kill without `killpg`.** `BashPtyControl::kill` spawns `/bin/kill` for
  both `<pid>` and `-<pid>` (process group) instead of an FFI `killpg`.
  `interrupt()` writes `^C`, which is the authentic PTY SIGINT path.

## Tests

6 new tests in `tools/bash_pty.rs` cover: shell quoting, ANSI/binary/`\r`
sanitization, an `echo` stream with exit-code capture, a non-zero exit,
`sleep` interrupted via `BashPtyControl::interrupt`, and a 60 KB binary
output that is tail-truncated and spilled to a temp file. The existing 3
tests in `tools/bash.rs` are untouched.

Run the gate as usual (coordinator-side): `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace -- --test-threads=4`.
