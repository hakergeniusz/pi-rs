# P7 crash log — coordinator wiring

New module: `crates/pi-coding-agent/src/crash_log.rs` (branch `feat/p7-crash-log`).
No shared files were edited. No `Cargo.toml` changes: `serde`, `serde_json`,
`chrono`, `regex`, `tempfile` are already dependencies/dev-dependencies of
`pi-coding-agent`.

## 1. Register the module (required for anything to compile)

In `crates/pi-coding-agent/src/lib.rs`, add next to the other `pub mod` lines
(alphabetical, after `config`):

```rust
/// Crash log: fatal errors and uncaught panics (`core/crash-log.ts`).
pub mod crash_log;
```

## 2. Install the crash handler in `main.rs`

In `crates/pi-coding-agent/src/main.rs::main`, after `cwd_string` is computed
(the current line 97) and **before** `let outcome = match app_mode { … }`
(current line 130), so fatal errors from `build_runtime` are covered too:

```rust
    // P7 crash log: mirrors upstream's uncaughtException listener
    // (interactive-mode.ts:4610-4612). Keep the guard alive for the process
    // lifetime — std::process::exit deliberately skips its Drop (that is what
    // we want for a crash handler).
    let crash_path = pi_coding_agent::crash_log::crash_log_path();
    let _crash_hook = pi_coding_agent::crash_log::install_panic_hook(
        crash_path.clone(),
        pi_coding_agent::crash_log::CrashContext {
            session_file: None, // publish once a session exists (see below)
            cwd: cwd_string.clone(),
        },
    );
```

The binding must be named `_crash_hook` (or otherwise live to the end of
`main`) — dropping it uninstalls the hook. `install_panic_hook` is idempotent:
a second call returns `None`, so a future interactive-mode handler can safely
call it again.

When a session is created (`build_runtime` owns `SessionManager`), publish its
path at the interactive/rpc call sites, matching upstream reading
`this.session.sessionFile` at panic time (`interactive-mode.ts:2281`):

```rust
    pi_coding_agent::crash_log::set_panic_session_file(
        runtime.session.session_file().map(|p| p.to_string_lossy().into_owned()),
    );
```

`None` clears the slot and falls back to the `CrashContext` value. (If the
current `SessionManager` has no public accessor, leave this out — the record
will carry `sessionFile: null`, which is valid upstream behavior.)

## 3. Record fatal runtime errors (`interactive-mode.ts:2245-2259`)

The `Err(error)` arm of `match outcome` (current lines 138-141) receives a
`String`, so call `record_crash` directly (not `record_error`, which needs a
`std::error::Error`):

```rust
        Err(error) => {
            eprintln!("{error}");
            let recorded = pi_coding_agent::crash_log::record_crash(
                pi_coding_agent::crash_log::CrashInput {
                    kind: pi_coding_agent::crash_log::CrashKind::FatalError,
                    message: &error,
                    stack: None, // supply a backtrace here if one is captured
                    session_file: None,
                    cwd: &cwd_string,
                },
                &crash_path,
            );
            if recorded.is_some() {
                eprintln!(
                    "\n{}",
                    pi_coding_agent::crash_log::crash_report_instructions(None)
                );
            }
            std::process::exit(1);
        }
```

`crash_report_instructions(None)` is upstream's verbatim
`"To report this crash: start pi and run /bug. The crash details are attached automatically."`;
pass `Some(session_file)` to get the `pi -r` resume variant.

## 4. Show the previous crash at startup (interactive only)

Upstream does this once at `interactive-mode.ts:1261-1267`. In `main.rs`, after
the mode is resolved and before mode dispatch, gated on interactive mode:

```rust
    if app_mode == AppMode::Interactive {
        if let Some(crash) =
            pi_coding_agent::crash_log::take_unnotified_crash_now(&crash_path)
        {
            eprintln!(
                "{}",
                pi_coding_agent::crash_log::format_unnotified_crash_notice(
                    &crash.timestamp,
                    &crash.message,
                )
            );
        }
    }
```

`take_unnotified_crash_now` marks **all** pending records as notified, so the
notice appears once. (Upstream uses `Date#toLocaleString()`; the helper uses
the ISO timestamp — a documented deviation.)

## 5. `/bug` and cleanup (only when those commands land in the port)

- Attach history: `read_crash_log(&crash_path)` → `Vec<CrashRecord>`.
- After a successful report: `clear_crash_log(&crash_path)`.

## Public API / semantics

| Function | Mirrors | Notes |
|---|---|---|
| `crash_log_path()` / `crash_log_path_for(agent_override, state_root)` | `crashLogPath`, `crash-log.ts:21-23` | `crashes.json` in XDG **state** root; `$PI_CODING_AGENT_DIR` collapses to upstream's single-dir layout. Pure `*_for` is the injectable seam. |
| `read_crash_log(&Path)` | `:25-40` | unreadable/non-array → `[]`; keeps only objects with string `timestamp` + `message`. |
| `record_crash(CrashInput, &Path) -> Option<CrashRecord>` | `:124-144` | appends, trims to newest 5, `None` on write failure. `timestamp` is ISO-ms UTC, `version` is `CARGO_PKG_VERSION`, `notified` omitted. |
| `record_error(kind, &dyn Error, session_file, cwd, &Path)` | convenience | extracts `Display` message + `Error::backtrace()`. |
| `take_unnotified_crash(&Path, now_ms)` / `take_unnotified_crash_now(&Path)` | `:147-162` | newest un-notified within 7 days; marks all notified. |
| `clear_crash_log(&Path)` | `:164-170` | idempotent. |
| `install_panic_hook(PathBuf, CrashContext) -> Option<InstalledPanicHook>` | `interactive-mode.ts:4610-4612`, `:4542-4562` | idempotent; guard `Drop` restores prior hook. |
| `set_panic_session_file(Option<String>)` | `:2281` | dynamic session path read at panic time. |
| `find_extension_stack_matches(Option<&str>, &[ExtensionStackMetadata])` | `:70-121` | self-contained; map loaded extensions onto the `ExtensionStackMetadata` view. |
| `format_crash_extension_hint`, `crash_report_instructions`, `format_unnotified_crash_notice` | `interactive-mode.ts:316-331`, `:2290-2293`, `:1261-1267` | verbatim upstream strings (APP_NAME = `pi`). |
| `redact_secrets(&str) -> String` | **pi-rs deviation** | scrubs bearer tokens, secret-looking assignments, and common key shapes in `message`/`stack` before writing. |

## Deviations to carry into `HANDOFF.md` when this lands

1. Crash history lives in the XDG **state** dir (`~/.local/state/pi-rs/crashes.json`)
   rather than upstream's `~/.pi/agent/crashes.json`; the single-directory
   layout is restored by `$PI_CODING_AGENT_DIR`.
2. Secret redaction added (upstream writes error text verbatim).
3. No `args` field exists in upstream's `CrashRecord`, so none was added — the
   `crashes.json` schema stays byte-compatible. If the coordinator truly wants
   CLI args, that is a deliberate pi-rs extension, not a port.
4. Rust panic hook replaces the `process.on("uncaughtException")` listener; it
   records and prints the verbatim notices but does **not** call
   `process::exit` (unwinding/abort policy stays with the caller).
5. `toLocaleString()` → ISO timestamp in the startup notice.
6. Malformed records that pass the upstream `timestamp`/`message` filter are
   normalized with empty-string defaults on read (upstream keeps the raw
   wrong-typed values); this only affects hand-edited logs.

## Verification

12 unit tests in `crash_log.rs` cover path resolution, record/JSON shape,
read filtering, the 5-record cap, unnotified/7-day logic, clear, redaction,
extension stack matching + hint strings, notice strings, idempotent
install/reinstall, a `catch_unwind` panic captured as `uncaught_exception`, and
the post-install session-file slot. All I/O takes an injectable path and uses
`tempfile`, so no test touches a real data root. **Not compiled here** per the
no-cargo rule; the coordinator's RAM-container gate is authoritative.
