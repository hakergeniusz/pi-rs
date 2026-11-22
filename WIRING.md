# RPC command slice — host wiring contract

This file is the handoff from the RPC-command slice to the coordinator.
It records exactly what the ten remaining commands in
`crates/pi-coding-agent/src/modes/rpc.rs` need from a host, what the default
`LocalRpcHost` already does with existing port seams, and which upstream
paragraphs are intentionally **not** reproduced by the default.

Ground truth: `packages/coding-agent/src/modes/rpc/rpc-mode.ts` (1.5.2),
`rpc-types.ts`, `core/agent-session-runtime.ts`, `core/agent-session.ts`.
All code locations below are in those files unless stated otherwise.

## What is already wired

`create_bridge` installs `Box::new(LocalRpcHost)` on `RpcBridge::host`.
No change to `main.rs`, `lib.rs`, `Cargo.toml`, or other mode files is needed
for the commands to answer with upstream's wire shapes. `LocalRpcHost`
uses:

| Command | Seam used |
|---|---|
| `new_session` | `SessionManager::create` / `::in_memory` / `::new_session`, `extensions::emit_gate` (`session_before_switch`, `session_shutdown`, `session_start`) |
| `cycle_model` | `models_state::all()`, `apply_thinking_level`, `SessionManager::append_model_change` (handled in the RPC file, not the host) |
| `bash` | `extensions::user_bash` → `UserBashOutcome`, local `bash -c`, `SessionManager::append_message(AgentMessage::BashExecution)` |
| `abort_bash` | default no-op (synchronous local shell, nothing in flight) |
| `get_session_stats` | `SessionManager::get_entries`/`get_branch`/`build_session_projection`, `UsageTotals::add`, `estimate_projected_context_tokens`, `get_latest_compaction_entry` |
| `export_html` | `session::export_html::export_session_to_html` with embedded upstream templates |
| `switch_session` | `SessionManager::open`, the same three gate events as `new_session` |
| `fork` / `clone` | `SessionManager::get_entry`/`get_leaf_id`/`create_branched_session`/`new_session`, `session_before_fork` gate; all four upstream error strings are verbatim |
| `get_commands` | `pi_ext_js::ExtensionRunner::resolved_commands()` under `ext-js`; `[]` headless |

`run_rpc_mode` clears `bridge.queues` after a non-cancelled
`new_session`/`switch_session`/`fork`/`clone`, mirroring the fresh `Agent`
that upstream's `rebindSession` installs.

## The seam to override

```rust
pub trait RpcHost: Send {
    fn new_session(
        &self,
        runtime: &mut pi_coding_agent::modes::print_mode::PrintRuntime,
        parent_session: Option<&str>,
    ) -> Result<bool, String>;                 // Ok(cancelled)

    fn switch_session(
        &self,
        runtime: &mut pi_coding_agent::modes::print_mode::PrintRuntime,
        session_path: &str,
    ) -> Result<bool, String>;                 // Ok(cancelled)

    fn fork(
        &self,
        runtime: &mut pi_coding_agent::modes::print_mode::PrintRuntime,
        entry_id: &str,
        position: RpcForkPosition,             // Before | At
    ) -> Result<RpcForkResult, String>;        // { selected_text, cancelled }

    fn bash(
        &self,
        runtime: &mut pi_coding_agent::modes::print_mode::PrintRuntime,
        command: &str,
        exclude_from_context: bool,
        id: Option<&str>,
    ) -> Result<serde_json::Value, String>;    // verbatim BashResult

    fn abort_bash(&self) {}

    fn get_commands(
        &self,
        runtime: &pi_coding_agent::modes::print_mode::PrintRuntime,
    ) -> Result<Vec<serde_json::Value>, String>; // []RpcSlashCommand
}
```

`RpcForkPosition`, `RpcForkResult`, `LocalRpcHost` and `RpcHost` are all
`pub` from `pi_coding_agent::modes::rpc`. To install a full host, set
`bridge.host = Box::new(YourHost)` after `create_bridge` and before
`run_rpc_mode` in `main.rs::run_rpc` — the coordinator owns that file, the
RPC slice intentionally does not touch it.

## What the default deliberately leaves to the coordinator

These are upstream behaviors with no port equivalent yet. They are **not**
invented in the RPC file; a full host should implement them behind the trait
above.

1. **Runtime rebind / cwd rebuild** (`agent-session-runtime.ts:213-222,
   246-253,300-309,321-328`). Upstream replaces the whole `AgentSession` and
   its services (extensions, tools, trust) and calls `rebindSession()`
   (`rpc-mode.ts:317-371`) so the new session's event stream reaches the
   client. The default only swaps `PrintRuntime::session` and clears the RPC
   queues. A full host must rebuild `runtime` for the target session's cwd
   and re-bind the extension runner to `bridge.sink`.
2. **`bash_execution_update` events** (`agent-session.ts:4022-4025`). The
   `id` field of a `bash` request is forwarded to the host but the port's
   `AgentEvent` enum has no `bash_execution_update` variant. A full host with
   a PTY executor should emit those deltas (and support `abort_bash` by
   cancelling its live `AbortSignal`).
3. **Shell prefix / shell path** (`agent-session.ts:4011-4020`). Upstream
   reads `settings.shellCommandPrefix` and `settings.shellPath`. The default
   runs plain `bash -c`. `PrintRuntime` has no settings-manager field.
4. **Prompt templates and skills in `get_commands`** (`rpc-mode.ts:713-729`).
   `PrintRuntime` does not carry `session.promptTemplates` or the resource
   loader's skills; the default returns only extension commands. A full host
   should load them with `crate::prompt_templates::load_prompt_templates`
   and `crate::skills`, then append:
   - `{ name: template.name, description: template.description,
       source: "prompt", sourceInfo: <template.source/scope> }`
   - `{ name: "skill:" + skill.name, description: skill.description,
       source: "skill", sourceInfo: <skill.source/scope> }`
   in that order after the extension commands.
5. **Extension `sourceInfo` fidelity** (`source-info.ts:7-14`). The engine's
   `ResolvedCommand` carries only `extension_path`; the default derives the
   synthetic `SourceInfo` shape (`path`, `source`, `scope: "temporary"`,
   `origin: "top-level"`). A full host should pass the extension's real
   metadata through `RpcHost::get_commands`.
6. **Per-model thinking level on model switch**
   (`agent-session.ts:2828-2840`). `cycle_model` keeps the current level and
   lets `apply_thinking_level` clamp it against the new model; upstream reads
   `settings.getModelThinkingLevel` / `getDefaultThinkingLevel` first.
7. **`model_select` event** (`agent-session.ts:2728,2756`). No `AgentEvent`
   variant exists in the port, so `cycle_model` answers without one, exactly
   as `set_model` already does.

## Malformed-request guards (not upstream error paths)

The wire protocol is untyped (`rpc-types.ts` is compile-time only), so the
port guards required fields that TypeScript would pass as `undefined` into a
Node API:

| Request | Guard error |
|---|---|
| `bash` without `command` | `The "command" argument must be of type string. Received undefined` |
| `switch_session` without `sessionPath` | `Cannot read properties of undefined (reading 'startsWith')` (upstream's `normalizePath` deref, `utils/paths.ts:125`) |
| `fork` without `entryId` | `Invalid entry ID for forking` (upstream's own error, `agent-session-runtime.ts:275`) |

Every other error string in these ten handlers is copied verbatim from
upstream (`Model not found`, `Entry not found`, `Session name cannot be
empty`, `Cannot export in-memory session to HTML`, `Nothing to export yet -
start a conversation first`, `Invalid entry ID for forking`, `Persisted
session is missing a session file`, `This session has not been saved yet.
Send a message before cloning or forking it.`, `Failed to create forked
session`, `Cannot clone session: no current entry selected`).
