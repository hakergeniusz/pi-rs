# MCP fidelity gaps — host wiring & remaining deviations

Slice `feat/mcp-gaps`. Files touched:

- `crates/pi-ext-js/src/mcp_manager.rs` — startup wait, notifications,
  stderr tail, `_meta` forwarding, faithful `tool_exposure` helper.
- `crates/pi-mcp/src/client.rs` — `McpClient::try_recv_inbound()`.
- `crates/pi-mcp/src/types.rs` — block-level `_meta`/`annotations` on
  `ContentBlock`, top-level `_meta` on `CallToolResult`, faithful
  `tool_name_pattern_matches`.
- `crates/pi-mcp/src/http.rs` — one additional synthetic-notification test.

## 1. Host wiring required

These mechanisms are implemented as *host-driven* APIs; the coordinator must
wire them into the session/event loop (this slice may not edit `main.rs`,
`lib.rs`, or `modes/**`).

### 1.1 `startupWaitMs` / bounded first-prompt wait

Upstream starts connections in the background at `session_start` and lets the
**first prompt** wait at most `startupWaitMs ?? 10_000` ms for servers whose
tools are `direct` (`index.ts:87-94`, `:145-148`, `:294-296`,
`:1154-1175`). The port's existing `McpManager::connect()` stays synchronous
and verbatim-compatible; the new path is:

```rust
use pi_ext_js::mcp_manager::{McpManager, StartupWait, StartupTick,
                            DEFAULT_STARTUP_WAIT_MS};

let manager = McpManager::default();
let mut startup = StartupWait::new(DEFAULT_STARTUP_WAIT_MS);

// At session_start, for each enabled server with a direct exposure:
startup.add(name, &config, &cwd, &manager);

// On every host event-loop tick before the first prompt:
match startup.tick(now) {                       // `now: std::time::Instant`
    StartupTick::Waiting { .. } => { /* render, tick again */ }
    StartupTick::Ready | StartupTick::TimedOut => {
        // Register the tools from each `settled` outcome:
        for (server, outcome) in startup.drain_settled() { /* ... */ }
        if /* TimedOut */ {
            // notify: "MCP servers are still connecting; their tools become
            // available once connected." (index.ts:1173)
        }
    }
}
```

`StartupWait` has **no timers and no sleeps** — the host supplies `now` on
each tick, exactly as requested. `StartupWait::new_with_start(wait_ms, now)`
exists for deterministic tests. The wait is one-shot (`waitedForStartup`).

Only servers whose configured exposure set contains `direct`
(`index.ts:145-148`) should be added; codemode/deferred servers are waited
for by a script/search instead.

### 1.2 `tools/list_changed` → tool re-registration

Upstream re-lists a server's tools when it sends
`notifications/tools/list_changed` (`runtime.ts:382-384`, `:432-442`). The
transports already route such notifications to `McpClient::try_recv_inbound`;
the worker drains them on every request and on an explicit host tick:

```rust
// On each host dispatch/event-loop tick:
manager.poll_notifications();
for change in manager.take_tool_changes() {
    if let Some(error) = change.error {
        // record `error` as the server's state error; upstream's exact text
        // ("Failed to refresh tools: …") is preserved.
    } else {
        // Re-register `change.tools` for `change.server`, computing each
        // tool's exposure with `pi_ext_js::mcp_manager::tool_exposure(&config, name)`.
    }
}
```

`tool_exposure` is the faithful counterpart of `getMcpToolExposure`
(`core/mcp-servers.ts:169-178`) and uses
`pi_mcp::types::tool_name_pattern_matches` — the port of upstream's
escaped-regex `toolPatternRegExp` (`core/mcp-servers.ts:17-34`).

### 1.3 No wiring needed

- **stderr tail on connect failures** happens inside `spawn_config` /
  `connect_with_spawn`; the composed connect error is returned verbatim from
  `McpManager::connect` and stored in the `Failed` entry used by lazy
  reconnect (`call_tool_on`). Upstream's format is preserved:
  `"{error}\n{stderr_tail}"` and
  `MCP server "{name}" failed to connect: {error}`.
- **`_meta`/annotations preservation** happens in `convert_mcp_result`: the
  script `structuredContent` is the whole `CallToolResult` minus top-level
  `_meta`; block-level `_meta`/`annotations` survive the decode.

## 2. Remaining deviations

1. **stderr capture uses a temp-file sink, not an in-memory buffer.**
   `pi-mcp`'s `StdioTransport` hardcodes `.stderr(Stdio::null())`
   (`transport.rs:62`), which is outside this slice's file ownership.
   `spawn_config` therefore wraps stdio servers as
   `/bin/sh -c 'exec "$@" 2>"$PI_MCP_STDERR_PATH"' sh <cmd> <args…>` and
   reads the last 2 000 chars on failure. `exec` keeps the server as the
   direct child (signals/exit codes unchanged), and the sink file is removed
   after use. On non-Unix targets stderr stays discarded (upstream still
   captures). Alternatively, if `pi-mcp/src/transport.rs` becomes editable,
   pipe stderr there and expose a `stderr_tail()` accessor; the manager's
   `stdio_stderr_tail`/`clear_stderr_sink` would then collapse to that call.
2. **`slice_tail_chars` cuts on a Unicode character boundary**, not
   upstream's UTF-16 code-unit boundary (`core/tools/truncate.ts:288-294`);
   stderr is ASCII in practice, so the observable tail is identical.
3. **The first-prompt wait is host-driven**, not a `setTimeout`. If the host
   never calls `StartupWait::tick`, the wait never elapses; upstream's timer
   would eventually fire. This is the deliberate "no timers" trade-off.
4. **Notification handling is poll-based.** The worker drains inbound
   messages when it wakes on a request and when the host calls
   `poll_notifications`. Upstream uses `client.onNotification` listeners.
   A host that never ticks will not see tool changes (the transports'
   inbound channels are bounded at 256 messages).
5. **`mcp_registry.rs`'s original registration path still uses its in-order
   literal scanner** for `toolExposure` patterns. That file is owned by
   another slice, so it was not edited. `mcp_manager::tool_exposure` is the
   faithful implementation; the coordinator should switch both the initial
   registration and re-registration to it (one-line change each) to fully
   retire the deviation recorded in `HANDOFF.md`.
6. `McpManager::connect()` remains synchronous for existing callers; the new
   background path is additive so no host code breaks before the wiring in
   §1 is applied.

## 3. Verification

Per slice rules, no cargo commands were run. `rustfmt --edition 2024` on all
four edited `.rs` files succeeded (full parse + format), so syntax is valid.
The coordinator's serial gate should run the full workspace suite. Tests
added: 2 in `types.rs` (meta round-trip, glob semantics), 9 in
`mcp_manager.rs` (stderr tail + truncation, list_changed success/failure,
startup wait synthetic + fixture, `_meta` forwarding, glob exposure, plain
call regression), 1 in `http.rs` (synthetic notification routing).
