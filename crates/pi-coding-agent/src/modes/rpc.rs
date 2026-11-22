//! RPC mode: drive the agent over JSONL on stdin/stdout.
//!
//! Port target: `rpc-entry.ts` (process entry) plus `modes/rpc/` —
//! `rpc-mode.ts` (the command loop, `:54-846`), `rpc-types.ts` (the envelope,
//! `:20-303`), and `jsonl.ts` (LF-only framing). Commands arrive as
//! `{"type":"<command>","id":"<optional string>",...}` lines
//! (`rpc-types.ts:20-74`); every reply is
//! `{"type":"response","command":"<command>","success":true,"data":...}` or
//! `...,"success":false,"error":"..."`, with the optional `id` echoed
//! (`rpc-mode.ts:64-77`); agent events cross as the shared JSON protocol
//! lines (`modes/json-event.ts`).
//!
//! Slice 4 carries the extension dialog subprotocol
//! (`modes/rpc/rpc-mode.ts:94-145, 759-824`): `extension_ui_request` lines go
//! out through the [`RpcUiSink`] the extension host binds at load,
//! `extension_ui_response` lines resolve the pending dialog, and a dialog's
//! `timeout` resolves to the method default. The reader thread owns stdin so
//! responses can arrive while the loop is inside a command — upstream routes
//! both through the same stdin handler on its event loop.
//!
//! The session/runtime commands (`rpc-mode.ts:456-463,501-507,582-650,
//! 614-622,701-732`) are served through [`RpcHost`]: a narrow seam for the
//! host-owned session replacement/fork/bash/command-table operations that
//! upstream routes through `runtimeHost` and `AgentSession`. The default
//! [`LocalRpcHost`] uses the port's `SessionManager`, `export_html`, and
//! extension seams; a host with a full runtime can override it. See
//! `WIRING.md` for the exact contract.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Mutex, mpsc};
#[cfg(feature = "ext-js")]
use std::time::Duration;

use pi_agent::agent_loop::{AgentLoopConfig, DefaultConvertToLlm};
use pi_agent::agent_message::{AgentMessage, BashExecutionMessage};
use pi_agent::events::{AgentEvent, AgentEventSink};
use pi_agent::queues::Queues;
use pi_agent::transcript::tool_declaration;
use pi_agent::types::{AbortSignal, AgentContext, QueueMode, ToolExecutionMode, ToolSpec};
use pi_ai::types::StopReason;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::modes::interactive::components::footer::UsageTotals;
use crate::modes::print_mode::ModelPattern;
use crate::modes::print_mode::{PrintRuntime, now_millis};
use crate::modes::print_mode::{resolve_model, thinking_level_from_str};
use crate::session::compaction::{
    ContextUsageEstimate, calculate_context_tokens, estimate_projected_context_tokens,
};
use crate::session::export_html::{ExportOptions, ExportState, export_session_to_html};
use crate::session::records::SessionEntry;
use crate::session::store::SessionTreeNode;
use crate::session::store::build_session_context_from;
use crate::session::store::get_latest_compaction_entry;

/// The RPC commands the port serves (`rpc-types.ts:20-74`); every variant
/// mirrors its `handleCommand` case in `rpc-mode.ts:401-739`. Optional
/// fields mirror the TS looseness: a missing string field is `undefined`
/// upstream and flows into the handler's own verbatim error paths.
#[derive(Debug, Clone, PartialEq)]
pub enum RpcCommand {
    /// `type: "prompt"` (`rpc-mode.ts:409-435`).
    Prompt {
        message: Option<String>,
        images: Vec<pi_ai::types::ImageContent>,
        /// Accepted like upstream; the port's runs are synchronous, so the
        /// streaming queue choice never applies.
        streaming_behavior: Option<String>,
    },
    /// `type: "steer"` (`rpc-mode.ts:437-440`): queued even while idle; the
    /// next run drains steering first (`agent-loop.ts:381-385`).
    Steer {
        message: Option<String>,
        images: Vec<pi_ai::types::ImageContent>,
    },
    /// `type: "follow_up"` (`rpc-mode.ts:442-445`).
    FollowUp {
        message: Option<String>,
        images: Vec<pi_ai::types::ImageContent>,
    },
    /// `type: "abort"` (`rpc-mode.ts:447-450`).
    Abort,
    /// `type: "clear_queue"` (`rpc-mode.ts:452-454`).
    ClearQueue,
    /// `type: "get_state"` (`rpc-mode.ts:469-485`).
    GetState,
    /// `type: "set_model"` (`rpc-mode.ts:491-499`).
    SetModel {
        provider: Option<String>,
        model_id: Option<String>,
    },
    /// `type: "get_available_models"` (`rpc-mode.ts:509-512`).
    GetAvailableModels,
    /// `type: "set_thinking_level"` (`rpc-mode.ts:518-521`).
    SetThinkingLevel { level: Option<String> },
    /// `type: "cycle_thinking_level"` (`rpc-mode.ts:523-529`).
    CycleThinkingLevel,
    /// `type: "get_available_thinking_levels"` (`rpc-mode.ts:531-534`).
    GetAvailableThinkingLevels,
    /// `type: "set_steering_mode"` (`rpc-mode.ts:540-543`).
    SetSteeringMode { mode: Option<String> },
    /// `type: "set_follow_up_mode"` (`rpc-mode.ts:545-548`).
    SetFollowUpMode { mode: Option<String> },
    /// `type: "compact"` (`rpc-mode.ts:554-557`).
    Compact { custom_instructions: Option<String> },
    /// `type: "set_auto_compaction"` (`rpc-mode.ts:559-562`).
    SetAutoCompaction { enabled: bool },
    /// `type: "set_auto_retry"` (`rpc-mode.ts:568-571`).
    SetAutoRetry { enabled: bool },
    /// `type: "abort_retry"` (`rpc-mode.ts:573-576`).
    AbortRetry,
    /// `type: "get_entries"` (`rpc-mode.ts:657-668`).
    GetEntries { since: Option<String> },
    /// `type: "get_tree"` (`rpc-mode.ts:670-673`).
    GetTree,
    /// `type: "get_last_assistant_text"` (`rpc-mode.ts:675-678`).
    GetLastAssistantText,
    /// `type: "set_session_name"` (`rpc-mode.ts:680-687`).
    SetSessionName { name: Option<String> },
    /// `type: "get_messages"` (`rpc-mode.ts:693-695`).
    GetMessages,
    /// `type: "get_fork_messages"` (`rpc-mode.ts:652-655`).
    GetForkMessages,
    /// `type: "new_session"` (`rpc-mode.ts:456-463`, `rpc-types.ts:27`).
    NewSession { parent_session: Option<String> },
    /// `type: "cycle_model"` (`rpc-mode.ts:501-507`, `rpc-types.ts:34`).
    CycleModel,
    /// `type: "bash"` (`rpc-mode.ts:582-603`, `rpc-types.ts:55`).
    Bash {
        command: Option<String>,
        exclude_from_context: bool,
    },
    /// `type: "abort_bash"` (`rpc-mode.ts:605-608`, `rpc-types.ts:56`).
    AbortBash,
    /// `type: "get_session_stats"` (`rpc-mode.ts:614-617`, `rpc-types.ts:59`).
    GetSessionStats,
    /// `type: "export_html"` (`rpc-mode.ts:619-622`, `rpc-types.ts:60`).
    ExportHtml { output_path: Option<String> },
    /// `type: "switch_session"` (`rpc-mode.ts:624-630`, `rpc-types.ts:61`).
    SwitchSession { session_path: Option<String> },
    /// `type: "fork"` (`rpc-mode.ts:632-638`, `rpc-types.ts:62`).
    Fork { entry_id: Option<String> },
    /// `type: "clone"` (`rpc-mode.ts:640-650`, `rpc-types.ts:63`).
    Clone,
    /// `type: "get_commands"` (`rpc-mode.ts:701-732`, `rpc-types.ts:74`).
    GetCommands,
}

/// `AgentSessionRuntime.fork`'s `position` option
/// (`agent-session-runtime.ts:264`): `"before"` forks from the selected user
/// message's parent and returns its text; `"at"` keeps the entry as the leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcForkPosition {
    Before,
    At,
}

/// `AgentSessionRuntime.fork`'s result (`:265`): the selected text for the
/// `"before"` position and whether an extension vetoed the operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcForkResult {
    pub selected_text: Option<String>,
    pub cancelled: bool,
}

/// The host-owned operations `rpc-mode.ts` delegates to `runtimeHost` and
/// `AgentSession` (`:328-345,456-463,501-507,582-650,701-732`). The port has
/// no `AgentSessionRuntime` in RPC mode, so [`LocalRpcHost`] reconstructs the
/// observable part with `SessionManager`'s fork/new/open primitives and the
/// extension seams. A coordinator with a full runtime can install its own
/// host on [`RpcBridge::host`] to restore runtime rebind, cwd rebuild, and
/// prompt-template/skill command sources; see `WIRING.md`.
pub trait RpcHost: Send {
    /// `runtimeHost.newSession(options)` (`:456-463`).
    fn new_session(
        &self,
        runtime: &mut PrintRuntime,
        parent_session: Option<&str>,
    ) -> Result<bool, String>;

    /// `runtimeHost.switchSession(sessionPath)` (`:624-630`).
    fn switch_session(
        &self,
        runtime: &mut PrintRuntime,
        session_path: &str,
    ) -> Result<bool, String>;

    /// `runtimeHost.fork(entryId, options)` (`:632-638,640-650`).
    fn fork(
        &self,
        runtime: &mut PrintRuntime,
        entry_id: &str,
        position: RpcForkPosition,
    ) -> Result<RpcForkResult, String>;

    /// `session.executeBash` plus the `user_bash` interception (`:582-603`).
    /// Returns the verbatim `BashResult` object and records it in the session.
    fn bash(
        &self,
        runtime: &mut PrintRuntime,
        command: &str,
        exclude_from_context: bool,
        id: Option<&str>,
    ) -> Result<Value, String>;

    /// `session.abortBash()` (`:605-608`). The default is the idle no-op —
    /// the port's local execution is synchronous, so no shell is in flight.
    #[allow(clippy::unused_self)] // a PTY host needs the receiver
    fn abort_bash(&self) {}

    /// The three command sources of `get_commands` (`:701-732`), in upstream
    /// order: extension, prompt, skill. Each item is an already-shaped
    /// `RpcSlashCommand` (`rpc-types.ts:81-90`).
    fn get_commands(&self, runtime: &PrintRuntime) -> Result<Vec<Value>, String>;
}

/// The default [`RpcHost`] — the same seams the interactive host uses, minus
/// its TUI/runtime rebind. Every error is upstream's verbatim text
/// (`agent-session-runtime.ts:275,293,313,318`).
pub struct LocalRpcHost;

impl RpcHost for LocalRpcHost {
    fn new_session(
        &self,
        runtime: &mut PrintRuntime,
        parent_session: Option<&str>,
    ) -> Result<bool, String> {
        if emit_gate(
            runtime,
            &serde_json::json!({"type": "session_before_switch", "reason": "new"}),
        )? {
            return Ok(true);
        }
        let cwd = runtime.session.get_cwd().to_path_buf();
        let session_dir = runtime.session.get_session_dir().to_path_buf();
        let previous = session_file_string(&runtime.session);
        let mut session = if runtime.session.is_persisted() {
            crate::session::store::SessionManager::create(
                &cwd,
                Some(&session_dir),
                &crate::session::store::NewSessionOptions::default(),
            )
        } else {
            crate::session::store::SessionManager::in_memory(
                &cwd,
                &crate::session::store::NewSessionOptions::default(),
                Vec::new(),
            )
        }
        .map_err(|error| error.to_string())?;
        if let Some(parent) = parent_session {
            session
                .new_session(&crate::session::store::NewSessionOptions {
                    parent_session: Some(parent.to_string()),
                    ..crate::session::store::NewSessionOptions::default()
                })
                .map_err(|error| error.to_string())?;
        }
        emit_shutdown(runtime, "new", None);
        runtime.session = session;
        emit_start(runtime, "new", previous.as_deref());
        Ok(false)
    }

    fn switch_session(
        &self,
        runtime: &mut PrintRuntime,
        session_path: &str,
    ) -> Result<bool, String> {
        let event = serde_json::json!({
            "type": "session_before_switch",
            "reason": "resume",
            "targetSessionFile": session_path,
        });
        if emit_gate(runtime, &event)? {
            return Ok(true);
        }
        let session_dir = runtime.session.get_session_dir().to_path_buf();
        let previous = session_file_string(&runtime.session);
        let session = crate::session::store::SessionManager::open(
            std::path::Path::new(session_path),
            Some(&session_dir),
            None,
        )
        .map_err(|error| error.to_string())?;
        let destination = session_file_string(&session);
        emit_shutdown(runtime, "resume", destination.as_deref());
        runtime.session = session;
        emit_start(runtime, "resume", previous.as_deref());
        Ok(false)
    }

    #[allow(clippy::too_many_lines)] // mirrors the TS AgentSessionRuntime.fork; split when extending
    fn fork(
        &self,
        runtime: &mut PrintRuntime,
        entry_id: &str,
        position: RpcForkPosition,
    ) -> Result<RpcForkResult, String> {
        let position_str = match position {
            RpcForkPosition::Before => "before",
            RpcForkPosition::At => "at",
        };
        let event = serde_json::json!({
            "type": "session_before_fork",
            "entryId": entry_id,
            "position": position_str,
        });
        if emit_gate(runtime, &event)? {
            return Ok(RpcForkResult {
                selected_text: None,
                cancelled: true,
            });
        }
        let Some(selected) = runtime.session.get_entry(entry_id) else {
            return Err("Invalid entry ID for forking".to_string());
        };
        let (selected_text, target_leaf_id) = match position {
            RpcForkPosition::At => (None, Some(entry_id.to_string())),
            RpcForkPosition::Before => {
                let SessionEntry::Message(message) = &selected else {
                    return Err("Invalid entry ID for forking".to_string());
                };
                let AgentMessage::User(user) = &message.message else {
                    return Err("Invalid entry ID for forking".to_string());
                };
                (
                    Some(user_content_text(&user.content)),
                    selected.parent_id().map(str::to_string),
                )
            }
        };
        let previous = session_file_string(&runtime.session);
        let session_dir = runtime.session.get_session_dir().to_path_buf();
        if runtime.session.is_persisted() {
            let Some(current) = session_file_string(&runtime.session) else {
                return Err("Persisted session is missing a session file".to_string());
            };
            if target_leaf_id.is_none() {
                let options = crate::session::store::NewSessionOptions {
                    parent_session: Some(current),
                    ..crate::session::store::NewSessionOptions::default()
                };
                let session = crate::session::store::SessionManager::create(
                    runtime.session.get_cwd(),
                    Some(&session_dir),
                    &options,
                )
                .map_err(|error| error.to_string())?;
                let destination = session_file_string(&session);
                emit_shutdown(runtime, "fork", destination.as_deref());
                runtime.session = session;
                emit_start(runtime, "fork", previous.as_deref());
            } else if !std::path::Path::new(&current).exists() {
                return Err(
                    "This session has not been saved yet. Send a message before cloning or forking it."
                        .to_string(),
                );
            } else {
                let mut session = crate::session::store::SessionManager::open(
                    std::path::Path::new(&current),
                    Some(&session_dir),
                    None,
                )
                .map_err(|error| error.to_string())?;
                session
                    .create_branched_session(target_leaf_id.as_deref().unwrap_or_default())
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "Failed to create forked session".to_string())?;
                let destination = session_file_string(&session);
                emit_shutdown(runtime, "fork", destination.as_deref());
                runtime.session = session;
                emit_start(runtime, "fork", previous.as_deref());
            }
        } else {
            emit_shutdown(runtime, "fork", None);
            if let Some(leaf) = target_leaf_id {
                runtime
                    .session
                    .create_branched_session(&leaf)
                    .map_err(|error| error.to_string())?;
            } else {
                let options = crate::session::store::NewSessionOptions {
                    parent_session: previous.clone(),
                    ..crate::session::store::NewSessionOptions::default()
                };
                runtime
                    .session
                    .new_session(&options)
                    .map_err(|error| error.to_string())?;
            }
            emit_start(runtime, "fork", previous.as_deref());
        }
        Ok(RpcForkResult {
            selected_text,
            cancelled: false,
        })
    }

    fn bash(
        &self,
        runtime: &mut PrintRuntime,
        command: &str,
        exclude_from_context: bool,
        id: Option<&str>,
    ) -> Result<Value, String> {
        let cwd = runtime.session.get_cwd().to_string_lossy().into_owned();
        let intercepted = extension_user_bash(runtime, command, exclude_from_context, &cwd);
        let result = match intercepted {
            Some(Ok(
                crate::extensions::UserBashOutcome::Executed(result)
                | crate::extensions::UserBashOutcome::Result(result),
            )) => result,
            Some(Err(message)) => return Err(message),
            Some(Ok(crate::extensions::UserBashOutcome::Local)) | None => {
                local_bash(command, &runtime.session)?
            }
        };
        record_bash_result(runtime, command, &result, exclude_from_context, id);
        Ok(result)
    }

    fn get_commands(&self, runtime: &PrintRuntime) -> Result<Vec<Value>, String> {
        Ok(extension_commands(runtime))
    }
}

/// The session file as the string upstream puts on the wire (`:456-463`
/// carries no file, but `session_shutdown`/`session_start` do).
fn session_file_string(session: &crate::session::store::SessionManager) -> Option<String> {
    session
        .session_file()
        .map(|path| path.to_string_lossy().into_owned())
}

/// `session_before_*` gate vote (`extensions/mod.rs::emit_gate`).
#[allow(clippy::unnecessary_wraps)] // headless builds have no runner that can fail
fn emit_gate(runtime: &PrintRuntime, event: &Value) -> Result<bool, String> {
    #[cfg(feature = "ext-js")]
    {
        if let Some(runner) = &runtime.extension_runner {
            return crate::extensions::emit_gate(runner, event);
        }
    }
    #[cfg(not(feature = "ext-js"))]
    let _ = runtime;
    let _ = event;
    Ok(false)
}

/// `teardownCurrent`'s `session_shutdown` emission
/// (`agent-session-runtime.ts:167-175`).
fn emit_shutdown(runtime: &PrintRuntime, reason: &str, target: Option<&str>) {
    let mut event = serde_json::json!({"type": "session_shutdown", "reason": reason});
    if let Some(target) = target {
        event["targetSessionFile"] = serde_json::json!(target);
    }
    if let Err(error) = emit_gate(runtime, &event) {
        eprintln!("Warning: Extension error: session_shutdown: {error}");
    }
    #[cfg(feature = "ext-js")]
    if let Some(runner) = &runtime.extension_runner {
        if let Ok(mut runner) = runner.lock() {
            runner.shutdown_mcp();
        }
    }
}

/// The `session_start(reason, previousSessionFile)` emission of
/// `createRuntime` (`agent-session-runtime.ts:217-220,250-251,322-326`).
fn emit_start(runtime: &PrintRuntime, reason: &str, previous: Option<&str>) {
    let mut event = serde_json::json!({"type": "session_start", "reason": reason});
    if let Some(previous) = previous {
        event["previousSessionFile"] = serde_json::json!(previous);
    }
    if let Err(error) = emit_gate(runtime, &event) {
        eprintln!("Warning: Extension error: session_start: {error}");
    }
}

/// The `user_bash` interception (`interactive-mode.rs::extension_user_bash`);
/// headless builds answer `None` — execute locally.
#[cfg(feature = "ext-js")]
fn extension_user_bash(
    runtime: &PrintRuntime,
    command: &str,
    exclude: bool,
    cwd: &str,
) -> Option<Result<crate::extensions::UserBashOutcome, String>> {
    let runner = runtime.extension_runner.as_ref()?;
    crate::extensions::user_bash(runner, command, exclude, cwd)
}

/// Headless twin: no runner, no interception.
#[cfg(not(feature = "ext-js"))]
fn extension_user_bash(
    runtime: &PrintRuntime,
    command: &str,
    exclude: bool,
    cwd: &str,
) -> Option<Result<crate::extensions::UserBashOutcome, String>> {
    let _ = (runtime, command, exclude, cwd);
    None
}

/// The local `bash -c` operation (`createLocalBashOperations`,
/// `bash-executor.ts:137-180`), reduced to the synchronous shape the port
/// uses everywhere outside the PTY slice (P7). The result is the verbatim
/// `BashResult` object (`rpc-types.ts:193`).
fn local_bash(
    command: &str,
    session: &crate::session::store::SessionManager,
) -> Result<Value, String> {
    let output = std::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(session.get_cwd())
        .output()
        .map_err(|error| error.to_string())?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&stderr);
    }
    Ok(serde_json::json!({
        "output": text,
        "exitCode": output.status.code().unwrap_or(-1),
        "cancelled": false,
        "truncated": false,
    }))
}

/// `session.recordBashResult` (`agent-session.ts:4041-4062`): append the
/// `bashExecution` entry so it rides in the model context exactly like
/// upstream. `id` only feeds `bash_execution_update` events; the port's event
/// enum has no such variant (documented in `WIRING.md`).
fn record_bash_result(
    runtime: &mut PrintRuntime,
    command: &str,
    result: &Value,
    exclude_from_context: bool,
    _id: Option<&str>,
) {
    let message = BashExecutionMessage {
        command: command.to_string(),
        output: result
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        exit_code: result.get("exitCode").and_then(Value::as_i64),
        cancelled: result
            .get("cancelled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        truncated: result
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        full_output_path: result
            .get("fullOutputPath")
            .and_then(Value::as_str)
            .map(str::to_string),
        exclude_from_context: Some(exclude_from_context),
        timestamp: now_millis(),
    };
    let _ = runtime
        .session
        .append_message(AgentMessage::BashExecution(message));
}

/// Extension commands for `get_commands` (`:704-711`). The port's
/// `ResolvedCommand` carries only `extension_path`, so `sourceInfo` is the
/// upstream synthetic shape derived from it; a host with the real extension
/// `SourceInfo` can override through [`RpcHost::get_commands`].
#[cfg(feature = "ext-js")]
fn extension_commands(runtime: &PrintRuntime) -> Vec<Value> {
    let Some(runner) = &runtime.extension_runner else {
        return Vec::new();
    };
    let runner = runner.lock().expect("extension runner mutex poisoned");
    runner
        .resolved_commands()
        .into_iter()
        .map(|command| {
            serde_json::json!({
                "name": command.invocation_name,
                "description": command.description,
                "source": "extension",
                "sourceInfo": synthetic_source_info(&command.extension_path),
            })
        })
        .collect()
}

/// Headless twin: no extension commands.
#[cfg(not(feature = "ext-js"))]
fn extension_commands(runtime: &PrintRuntime) -> Vec<Value> {
    let _ = runtime;
    Vec::new()
}

/// The synthetic `SourceInfo` (`source-info.ts:61-79`) for a path with no
/// metadata attached. `WIRING.md` records the host override for the real
/// per-extension metadata and for the prompt/skill command sources.
fn synthetic_source_info(path: &str) -> Value {
    let source = if let Some(rest) = path.strip_prefix("builtin:") {
        rest.split(':').next().unwrap_or("builtin")
    } else if let Some(inner) = path
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
    {
        inner.split(':').next().unwrap_or("temporary")
    } else {
        "temporary"
    };
    serde_json::json!({
        "path": path,
        "source": source,
        "scope": "temporary",
        "origin": "top-level",
    })
}

/// `session.getSessionStats()` (`agent-session.ts:4350-4402`,
/// `SessionStats` at `:357-374`). Aggregates over *all* entries, including
/// compacted-away history; `sessionFile`/`contextUsage` are `undefined` and
/// therefore absent from the wire when unavailable.
fn session_stats_json(runtime: &PrintRuntime) -> Value {
    let mut totals = UsageTotals::default();
    let mut total_messages = 0u64;
    let mut user_messages = 0u64;
    let mut assistant_messages = 0u64;
    let mut tool_results = 0u64;
    let mut tool_calls = 0u64;

    for entry in runtime.session.get_entries() {
        match &entry {
            SessionEntry::Usage(usage) => totals.add(&usage.usage),
            SessionEntry::BranchSummary(summary) => {
                if let Some(usage) = &summary.usage {
                    totals.add(usage);
                }
            }
            SessionEntry::Compaction(compaction) => {
                if let Some(usage) = &compaction.usage {
                    totals.add(usage);
                }
            }
            SessionEntry::Message(message) => {
                total_messages += 1;
                match &message.message {
                    AgentMessage::User(_) => user_messages += 1,
                    AgentMessage::ToolResult(result) => {
                        tool_results += 1;
                        if let Some(usage) = &result.usage {
                            totals.add(usage);
                        }
                    }
                    AgentMessage::Assistant(assistant) => {
                        assistant_messages += 1;
                        tool_calls += assistant
                            .content
                            .iter()
                            .filter(|block| {
                                matches!(block, pi_ai::types::AssistantContent::ToolCall(_))
                            })
                            .count() as u64;
                        totals.add(&assistant.usage);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let mut stats = serde_json::Map::new();
    if let Some(session_file) = runtime.session.session_file() {
        stats.insert(
            "sessionFile".to_string(),
            serde_json::json!(session_file.display().to_string()),
        );
    }
    stats.insert(
        "sessionId".to_string(),
        serde_json::json!(runtime.session.get_session_id()),
    );
    stats.insert("userMessages".to_string(), serde_json::json!(user_messages));
    stats.insert(
        "assistantMessages".to_string(),
        serde_json::json!(assistant_messages),
    );
    stats.insert("toolCalls".to_string(), serde_json::json!(tool_calls));
    stats.insert("toolResults".to_string(), serde_json::json!(tool_results));
    stats.insert(
        "totalMessages".to_string(),
        serde_json::json!(total_messages),
    );
    stats.insert(
        "tokens".to_string(),
        serde_json::json!({
            "input": totals.input,
            "output": totals.output,
            "cacheRead": totals.cache_read,
            "cacheWrite": totals.cache_write,
            "total": totals.tokens(),
        }),
    );
    stats.insert("cost".to_string(), serde_json::json!(totals.cost));
    let context_usage = context_usage_json(runtime);
    if context_usage != Value::Null {
        stats.insert("contextUsage".to_string(), context_usage);
    }
    Value::Object(stats)
}

/// `session.getContextUsage()` (`agent-session.ts:4404-4447`), including the
/// post-compaction `tokens: null` answer. A model without a positive context
/// window answers `undefined` (the caller drops the key).
fn context_usage_json(runtime: &PrintRuntime) -> Value {
    let context_window = runtime.model.context_window;
    if context_window == 0 {
        return Value::Null;
    }
    let Ok(branch) = runtime.session.get_branch(None) else {
        return Value::Null;
    };
    let Ok(projection) = runtime.session.build_session_projection() else {
        return Value::Null;
    };
    if let Some(compaction) = get_latest_compaction_entry(&branch) {
        let mut projected = std::collections::HashSet::new();
        for entry in &projection.entries {
            let has_usage = entry.messages.iter().any(|message| {
                let AgentMessage::Assistant(assistant) = message else {
                    return false;
                };
                assistant.stop_reason != StopReason::Aborted
                    && assistant.stop_reason != StopReason::Error
                    && calculate_context_tokens(&assistant.usage) > 0
            });
            if has_usage {
                projected.insert(entry.source_entry.id().to_string());
            }
        }
        if let Some(index) = branch
            .iter()
            .position(|entry| entry.id() == compaction.base.id.as_str())
        {
            let has_post_compaction = branch[index + 1..]
                .iter()
                .any(|entry| projected.contains(entry.id()));
            if !has_post_compaction {
                return serde_json::json!({
                    "tokens": null,
                    "contextWindow": context_window,
                    "percent": null,
                });
            }
        }
    }
    let estimate: ContextUsageEstimate = estimate_projected_context_tokens(&projection, &branch);
    #[allow(clippy::cast_precision_loss)] // upstream divides JS numbers directly
    let percent = (estimate.tokens as f64 / context_window as f64) * 100.0;
    serde_json::json!({
        "tokens": estimate.tokens,
        "contextWindow": context_window,
        "percent": percent,
    })
}

/// One stdin line classified for dispatch (`handleInputLine`,
/// `rpc-mode.ts:768-824`); the `extension_ui_response` routing happens in
/// the bridge reader thread, so everything reaching the loop is a command
/// attempt.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// A recognized command.
    Command(RpcCommand),
    /// `type` names no known command (`rpc-mode.ts:734-737`); the raw
    /// `type` string, [`None`] when absent — upstream echoes
    /// `Unknown command: undefined` for that case.
    Unknown(Option<String>),
}

/// `EXTENDED_THINKING_LEVELS` (`pi-ai/src/models.ts:1299`): the clamp's
/// walk order.
const EXTENDED_THINKING_LEVELS: [&str; 7] =
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// One stdout response line (`rpc-types.ts:116-245`): `type` is always
/// `"response"`; `success` carries `data` or `error`, and the optional `id`
/// echoes the request. JS drops `undefined` keys when stringifying
/// (`rpc-mode.ts:64-77`), so the port skips them too.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcResponse {
    #[serde(rename = "type")]
    pub kind: RpcResponseKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The one wire discriminant an RPC response carries (`rpc-types.ts:118`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RpcResponseKind {
    #[serde(rename = "response")]
    Response,
}

impl RpcResponse {
    /// The `success` helper (`rpc-mode.ts:64-73`): `data` is `undefined`
    /// for the data-less commands, so the key is dropped.
    fn success(id: Option<&str>, command: &str, data: Option<Value>) -> Self {
        Self {
            kind: RpcResponseKind::Response,
            id: id.map(str::to_string),
            command: Some(command.to_string()),
            success: true,
            data,
            error: None,
        }
    }

    /// The `error` helper (`rpc-mode.ts:75-77`): the `command` echo is the
    /// raw `type` string, dropped for the `undefined` cases.
    fn failure(id: Option<&str>, command: Option<&str>, error: impl Into<String>) -> Self {
        Self {
            kind: RpcResponseKind::Response,
            id: id.map(str::to_string),
            command: command.map(str::to_string),
            success: false,
            data: None,
            error: Some(error.into()),
        }
    }
}

/// `handleInputLine`'s parse step (`rpc-mode.ts:768-782`): [`Err`] is the
/// verbatim `parse` failure text; the `id` echo only happens for objects
/// that carry a string one. The command discriminant is the `type` value
/// itself (`rpc-types.ts:20-74`).
fn parse_request(line: &str) -> Result<(Incoming, Option<String>, Option<String>), String> {
    let value: Value =
        serde_json::from_str(line).map_err(|error| format!("Failed to parse command: {error}"))?;
    let id = value.get("id").and_then(Value::as_str).map(str::to_string);
    let type_name = value
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(type_name) = type_name else {
        return Ok((Incoming::Unknown(None), id, None));
    };
    let command = match type_name.as_str() {
        "prompt" => RpcCommand::Prompt {
            message: field_str(&value, "message"),
            images: field_images(&value),
            streaming_behavior: field_str(&value, "streamingBehavior"),
        },
        "steer" => RpcCommand::Steer {
            message: field_str(&value, "message"),
            images: field_images(&value),
        },
        "follow_up" => RpcCommand::FollowUp {
            message: field_str(&value, "message"),
            images: field_images(&value),
        },
        "abort" => RpcCommand::Abort,
        "clear_queue" => RpcCommand::ClearQueue,
        "get_state" => RpcCommand::GetState,
        "set_model" => RpcCommand::SetModel {
            provider: field_str(&value, "provider"),
            model_id: field_str(&value, "modelId"),
        },
        "get_available_models" => RpcCommand::GetAvailableModels,
        "set_thinking_level" => RpcCommand::SetThinkingLevel {
            level: field_str(&value, "level"),
        },
        "cycle_thinking_level" => RpcCommand::CycleThinkingLevel,
        "get_available_thinking_levels" => RpcCommand::GetAvailableThinkingLevels,
        "set_steering_mode" => RpcCommand::SetSteeringMode {
            mode: field_str(&value, "mode"),
        },
        "set_follow_up_mode" => RpcCommand::SetFollowUpMode {
            mode: field_str(&value, "mode"),
        },
        "compact" => RpcCommand::Compact {
            custom_instructions: field_str(&value, "customInstructions"),
        },
        "set_auto_compaction" => RpcCommand::SetAutoCompaction {
            enabled: field_bool(&value, "enabled"),
        },
        "set_auto_retry" => RpcCommand::SetAutoRetry {
            enabled: field_bool(&value, "enabled"),
        },
        "abort_retry" => RpcCommand::AbortRetry,
        "get_entries" => RpcCommand::GetEntries {
            since: field_str(&value, "since"),
        },
        "get_tree" => RpcCommand::GetTree,
        "get_last_assistant_text" => RpcCommand::GetLastAssistantText,
        "set_session_name" => RpcCommand::SetSessionName {
            name: field_str(&value, "name"),
        },
        "get_messages" => RpcCommand::GetMessages,
        "get_fork_messages" => RpcCommand::GetForkMessages,
        "new_session" => RpcCommand::NewSession {
            parent_session: field_str(&value, "parentSession"),
        },
        "cycle_model" => RpcCommand::CycleModel,
        "bash" => RpcCommand::Bash {
            command: field_str(&value, "command"),
            // `command.excludeFromContext ?? false` (`rpc-mode.ts:586`).
            exclude_from_context: field_bool(&value, "excludeFromContext"),
        },
        "abort_bash" => RpcCommand::AbortBash,
        "get_session_stats" => RpcCommand::GetSessionStats,
        "export_html" => RpcCommand::ExportHtml {
            output_path: field_str(&value, "outputPath"),
        },
        "switch_session" => RpcCommand::SwitchSession {
            session_path: field_str(&value, "sessionPath"),
        },
        "fork" => RpcCommand::Fork {
            entry_id: field_str(&value, "entryId"),
        },
        "clone" => RpcCommand::Clone,
        "get_commands" => RpcCommand::GetCommands,
        // Junk `type` values keep upstream's `Unknown command:` frame
        // (`rpc-mode.ts:734-737`).
        _ => {
            return Ok((
                Incoming::Unknown(Some(type_name.clone())),
                id,
                Some(type_name),
            ));
        }
    };
    Ok((Incoming::Command(command), id, Some(type_name)))
}

/// A string field, `None` when absent or not a string — the loose reading
/// upstream's untyped command objects give.
fn field_str(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// The `images` array (`rpc-types.ts:22-24`). A missing or non-array field
/// carries no images; non-image entries are skipped like the JS glue's
/// spread would fail late rather than at parse time.
fn field_images(value: &Value) -> Vec<pi_ai::types::ImageContent> {
    value
        .get("images")
        .and_then(Value::as_array)
        .map(|images| {
            images
                .iter()
                .filter_map(|image| serde_json::from_value(image.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// A boolean field, `false` when absent — the falsy reading upstream's
/// `command.enabled` gives (`rpc-mode.ts:559-571`).
fn field_bool(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// `clampThinkingLevel` (`pi-ai/src/models.ts:1318-1336`): keep a level the
/// model supports, else walk the extended order forward then backward from
/// the request.
fn clamp_thinking_level(available: &[&str], level: &str) -> String {
    if available.contains(&level) {
        return level.to_string();
    }
    let Some(index) = EXTENDED_THINKING_LEVELS
        .iter()
        .position(|candidate| *candidate == level)
    else {
        return available.first().copied().unwrap_or("off").to_string();
    };
    for candidate in &EXTENDED_THINKING_LEVELS[index..] {
        if available.contains(candidate) {
            return (*candidate).to_string();
        }
    }
    for candidate in &EXTENDED_THINKING_LEVELS[..index] {
        if available.contains(candidate) {
            return (*candidate).to_string();
        }
    }
    available.first().copied().unwrap_or("off").to_string()
}

/// `QueueMode`'s wire strings (`rpc-types.ts:43-44`).
fn queue_mode_str(mode: QueueMode) -> &'static str {
    match mode {
        QueueMode::All => "all",
        QueueMode::OneAtATime => "one-at-a-time",
    }
}

/// `session.setSteeringMode`'s unvalidated value (`agent-session.ts:2870`):
/// anything but `"all"` behaves one-at-a-time.
fn queue_mode_from(mode: Option<&str>) -> QueueMode {
    match mode {
        Some("all") => QueueMode::All,
        _ => QueueMode::OneAtATime,
    }
}

/// `contentText(content, "")` (`pi-ai/src/utils/text.ts:6-12`) over user
/// content: the text blocks joined, images skipped.
fn user_content_text(content: &pi_ai::types::UserContent) -> String {
    match content {
        pi_ai::types::UserContent::Text(text) => text.clone(),
        pi_ai::types::UserContent::Blocks(blocks) => {
            blocks.iter().fold(String::new(), |mut text, block| {
                if let pi_ai::types::InputContent::Text(part) = block {
                    text.push_str(&part.text);
                }
                text
            })
        }
    }
}

/// The queued steering/follow-up texts (`agent-session.ts:2582-2601`) read
/// through the queue contents.
fn queued_message_text(message: &AgentMessage) -> String {
    match message {
        AgentMessage::User(user) => user_content_text(&user.content),
        _ => String::new(),
    }
}

/// The live projection (`agent.state.messages`, `agent.ts:85-88`): the
/// transcript messages plus the leading system message upstream's agent
/// unshifts when the projection does not start with one — the same build
/// `run_agent_turn` runs the loop with.
fn session_messages(runtime: &PrintRuntime) -> Vec<AgentMessage> {
    let leaf_id = runtime.session.get_leaf_id().map(str::to_string);
    let mut messages =
        build_session_context_from(&runtime.session.get_entries(), leaf_id.as_deref()).messages;
    if messages.is_empty() {
        let specs: Vec<ToolSpec> = runtime.executor.tools();
        if let Some(system) = pi_agent::transcript::create_initial_system_message(
            Some(&runtime.system_prompt),
            &specs.iter().map(tool_declaration).collect::<Vec<_>>(),
        ) {
            messages.push(AgentMessage::System(system));
        }
    }
    messages
}

/// Sink streaming agent events to the RPC client as JSON lines, using the
/// same serialization as JSON mode. Each event takes the shared output
/// lock briefly: an extension dialog opened mid-run (context transform,
/// tool call) writes through the same stream between events without
/// deadlocking.
struct RpcEventSink {
    writer: RpcOut,
}

impl AgentEventSink for RpcEventSink {
    fn emit(&mut self, event: AgentEvent) -> pi_agent::types::BoxFuture<'_, ()> {
        let line =
            serde_json::Value::Object(crate::modes::json_event::event_to_json(&event)).to_string();
        let mut writer = self.writer.lock().expect("rpc output");
        if writeln!(writer, "{line}").is_err() {
            // Client closed the pipe: upstream aborts the run via its output
            // guard (P7 scope).
        }
        Box::pin(async {})
    }
}

/// Writes one event line from outside a run (`_emit` on the session's
/// subscriber, `rpc-mode.ts:362-367`): the queue/level/name changes a
/// command makes are session events the RPC client watches like any other.
fn emit_event(out: &RpcOut, event: &AgentEvent) -> Result<(), String> {
    let line =
        serde_json::Value::Object(crate::modes::json_event::event_to_json(event)).to_string();
    let mut writer = out.lock().expect("rpc output");
    writeln!(writer, "{line}").map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())
}

/// `_emitQueueUpdate` (`agent-session.ts:1107-1113`): the queued texts.
fn emit_queue_update(out: &RpcOut, queues: &Queues) -> Result<(), String> {
    let steering = queues
        .steering_all()
        .iter()
        .map(queued_message_text)
        .collect::<Vec<String>>();
    let follow_up = queues
        .follow_up_all()
        .iter()
        .map(queued_message_text)
        .collect::<Vec<String>>();
    emit_event(
        out,
        &AgentEvent::QueueUpdate {
            steering,
            follow_up,
        },
    )
}

/// `runRpcMode`: read request lines, dispatch, stream events. Blocks on the
/// current thread per request, like print mode.
/// A `Send` stdout writer for the bridge: [`std::io::Stdout`] itself is
/// `Send`, and the lock is taken per write, so the guard never crosses a
/// thread (`StdoutLock` is not `Send`).
pub struct RpcStdout(std::io::Stdout);

impl RpcStdout {
    #[must_use]
    pub fn new() -> Self {
        Self(std::io::stdout())
    }
}

impl Default for RpcStdout {
    fn default() -> Self {
        Self::new()
    }
}

impl Write for RpcStdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().flush()
    }
}

/// The RPC stream handle: every writer takes the lock per line, never
/// across a call that can write again (dialog requests interleave with
/// agent events on the same stream — `rpc-mode.ts`'s single `output`).
type RpcOut = Arc<Mutex<Box<dyn Write + Send>>>;

/// The RPC plumbing shared by the command loop and the dialog bridge.
pub struct RpcBridge {
    /// Command lines the reader thread forwards — everything that is not
    /// an `extension_ui_response` (`rpc-mode.ts:782-799` routes by type on
    /// one stdin stream).
    pub commands: Receiver<String>,
    /// The RPC stream, shared with the dialog sink.
    pub out: RpcOut,
    /// The agent queues, persistent across commands like upstream's
    /// `Agent` fields (`agent.ts:191-192`): `steer`/`follow_up` enqueue
    /// while idle and the next run drains steering first
    /// (`agent-loop.ts:381-395`).
    pub queues: Queues,
    /// The UI sink the extension host binds at load
    /// (`bindExtensions`'s `uiContext`, before `session_start`).
    #[cfg(feature = "ext-js")]
    pub sink: Arc<dyn pi_ext_js::UiSink>,
    /// Host-owned session replacement / fork / bash / command table
    /// ([`RpcHost`]). `create_bridge` installs [`LocalRpcHost`]; the
    /// coordinator can swap it before `run_rpc_mode` (see `WIRING.md`).
    pub host: Box<dyn RpcHost + Send>,
}

/// Wires the bridge: the reader thread owns the input and routes by
/// `type`, the sink shares the output. Dropping `commands` ends the loop
/// (stdin closed — `onInputEnd`, `rpc-mode.ts:826-839`).
pub fn create_bridge(input: Box<dyn BufRead + Send>, output: Box<dyn Write + Send>) -> RpcBridge {
    let out: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(output));
    let pending: Arc<Mutex<HashMap<String, SyncSender<Value>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (commands_tx, commands_rx) = mpsc::channel::<String>();
    let reader_out = Arc::clone(&out);
    let reader_pending = Arc::clone(&pending);
    std::thread::Builder::new()
        .name("rpc-stdin".to_string())
        .spawn(move || {
            for line in input.lines() {
                let Ok(line) = line else { break };
                // Every line reaches `handleInputLine` unchanged (`jsonl.ts`
                // emits blank ones too, and `JSON.parse` answers them with
                // the `parse` failure like any other junk line).
                let line = line.trim();
                // Extension UI responses resolve their dialog; everything
                // else is a command (`rpc-mode.ts:782-799`).
                let routed = serde_json::from_str::<Value>(line).ok().filter(|value| {
                    value.get("type").and_then(Value::as_str) == Some("extension_ui_response")
                });
                if let Some(response) = routed {
                    let id = response
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let sender = reader_pending.lock().expect("rpc ui pending").remove(&id);
                    if let Some(sender) = sender {
                        // A late response after the timeout finds no
                        // receiver and drops, like upstream's cleaned-up
                        // pending entry.
                        let _ = sender.send(response);
                    }
                    continue;
                }
                if commands_tx.send(line.to_string()).is_err() {
                    break;
                }
            }
            let _ = reader_out;
        })
        .expect("spawn the rpc stdin reader");
    RpcBridge {
        commands: commands_rx,
        out: Arc::clone(&out),
        queues: Queues::new(),
        #[cfg(feature = "ext-js")]
        sink: Arc::new(RpcUiSink {
            out,
            pending,
            receivers: Mutex::new(HashMap::new()),
            timeouts: Mutex::new(HashMap::new()),
        }),
        host: Box::new(LocalRpcHost),
    }
}

/// The `extension_ui_request` bridge (`createExtensionUIContext` +
/// `createDialogPromise`, `rpc-mode.ts:94-145`): dialog requests carry the
/// wire shape `{type, id, method, ...}`; the matching
/// `extension_ui_response` resolves them, and a `timeout` resolves to the
/// method default — the JS glue parses the empty payload into that
/// default, so [`Value::Null`] is the timeout answer. Fire-and-forget
/// methods (`notify`, `setStatus`, `setWidget`, `setTitle`,
/// `set_editor_text`) only write.
#[cfg(feature = "ext-js")]
struct RpcUiSink {
    out: Arc<Mutex<Box<dyn Write + Send>>>,
    pending: Arc<Mutex<HashMap<String, SyncSender<Value>>>>,
    /// One-slot receiver per dialog, registered at request time so a fast
    /// response can never race the wait.
    receivers: Mutex<HashMap<String, Receiver<Value>>>,
    timeouts: Mutex<HashMap<String, Option<u64>>>,
}

#[cfg(feature = "ext-js")]
impl pi_ext_js::UiSink for RpcUiSink {
    fn request(&self, request: &Value) {
        let id = request
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // Only dialog methods can be waited on (`createDialogPromise`,
        // rpc-mode.ts:94-145); fire-and-forget requests are write-only, so
        // they register nothing (a wait on one answers the default).
        let is_dialog = matches!(
            request.get("method").and_then(Value::as_str),
            Some("select" | "confirm" | "input" | "editor")
        );
        let timeout = request.get("timeout").and_then(Value::as_u64);
        if is_dialog {
            self.timeouts
                .lock()
                .expect("rpc ui timeouts")
                .insert(id.clone(), timeout);
            let (sender, receiver) = mpsc::sync_channel(1);
            self.pending
                .lock()
                .expect("rpc ui pending")
                .insert(id.clone(), sender);
            self.receivers
                .lock()
                .expect("rpc ui receivers")
                .insert(id, receiver);
        }
        let mut wire = serde_json::Map::new();
        wire.insert(
            "type".to_string(),
            Value::String("extension_ui_request".to_string()),
        );
        if let Some(object) = request.as_object() {
            for (key, value) in object {
                wire.insert(key.clone(), value.clone());
            }
        }
        let mut out = self.out.lock().expect("rpc output");
        let _ = writeln!(out, "{}", Value::Object(wire));
        let _ = out.flush();
    }

    fn wait_response(&self, id: &str, _method: &str) -> Value {
        let receiver = self.receivers.lock().expect("rpc ui receivers").remove(id);
        let timeout = self
            .timeouts
            .lock()
            .expect("rpc ui timeouts")
            .remove(id)
            .flatten();
        let Some(receiver) = receiver else {
            return Value::Null;
        };
        let response = match timeout {
            None => receiver.recv().ok(),
            Some(millis) => receiver.recv_timeout(Duration::from_millis(millis)).ok(),
        };
        response.unwrap_or(Value::Null)
    }
}

/// Serves commands until stdin closes (`onInputEnd`, `rpc-mode.ts:826-839`).
/// The extension UI stays bound for the whole run: dialogs opened by any
/// command's extension events cross the stream (`setUIContext` applies at
/// load, upstream's `rebindSession` re-binds per session — the port has
/// one session per run).
pub fn run_rpc_mode(runtime: &mut PrintRuntime, bridge: &mut RpcBridge) -> Result<i32, String> {
    while let Ok(line) = bridge.commands.recv() {
        let (incoming, id, type_name) = match parse_request(&line) {
            Ok(parsed) => parsed,
            Err(message) => {
                // The `parse` failure (`rpc-mode.ts:772-782`): no `id` echo,
                // the parse error message verbatim.
                write_line(
                    &bridge.out,
                    &RpcResponse::failure(None, Some("parse"), message),
                )?;
                continue;
            }
        };
        let incoming = match incoming {
            Incoming::Command(command) => command,
            Incoming::Unknown(raw_type) => {
                // `rpc-mode.ts:734-737`: the raw `type` echoes into both the
                // `command` field and the message; a missing one prints
                // `undefined` and drops the `command` key.
                write_line(
                    &bridge.out,
                    &RpcResponse::failure(
                        id.as_deref(),
                        raw_type.as_deref(),
                        format!(
                            "Unknown command: {}",
                            raw_type.as_deref().unwrap_or("undefined")
                        ),
                    ),
                )?;
                continue;
            }
        };
        match handle_command(runtime, bridge, incoming, id.as_deref()) {
            Ok(CommandOutcome::Success(data)) => {
                write_line(
                    &bridge.out,
                    &RpcResponse::success(id.as_deref(), type_name.as_deref().unwrap_or(""), data),
                )?;
            }
            Ok(CommandOutcome::Replied) => {}
            Err(error) => {
                write_line(
                    &bridge.out,
                    &RpcResponse::failure(id.as_deref(), type_name.as_deref(), error),
                )?;
            }
        }
    }
    clear_extension_ui(runtime);
    Ok(0)
}

/// Unbinds the dialog surface (`setUIContext(undefined)`) — the runner
/// returns to the headless matrix for whatever runs after the mode.
#[cfg(feature = "ext-js")]
fn clear_extension_ui(runtime: &PrintRuntime) {
    if let Some(runner) = &runtime.extension_runner {
        runner
            .lock()
            .expect("extension runner mutex poisoned")
            .clear_ui_context();
    }
}

#[cfg(not(feature = "ext-js"))]
fn clear_extension_ui(runtime: &PrintRuntime) {
    let _ = runtime;
}

fn write_line(out: &RpcOut, response: &RpcResponse) -> Result<(), String> {
    let line = serde_json::to_string(response).map_err(|error| error.to_string())?;
    let mut writer = out.lock().expect("rpc output");
    writeln!(writer, "{line}").map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())
}

/// What one command did (`handleCommand` returns an `RpcResponse |
/// undefined`, `rpc-mode.ts:401-739`).
enum CommandOutcome {
    /// Wrap in the standard success response; [`Some`] carries `data`.
    Success(Option<Value>),
    /// The command already wrote its response(s) itself — `prompt`'s
    /// preflight timing (`rpc-mode.ts:409-435`).
    Replied,
}

/// Runs one command against the runtime. `Err` carries the verbatim error
/// message the response echoes (`rpc-mode.ts:814-823`).
// The per-command bodies mirror upstream's dispatcher; kept in one function
// for protocol-auditability.
#[allow(clippy::too_many_lines)]
fn handle_command(
    runtime: &mut PrintRuntime,
    bridge: &mut RpcBridge,
    command: RpcCommand,
    id: Option<&str>,
) -> Result<CommandOutcome, String> {
    let out = Arc::clone(&bridge.out);
    match command {
        RpcCommand::Prompt {
            message, images, ..
        } => {
            // Upstream starts prompt handling immediately and emits the
            // authoritative response after preflight succeeds
            // (`rpc-mode.ts:409-435`); the port always starts a run, so the
            // disposition is `"started"` and the response precedes the event
            // stream. A missing `message` throws on `text.startsWith`
            // before preflight, like the TS.
            let Some(message) = message else {
                return Err(
                    "Cannot read properties of undefined (reading 'startsWith')".to_string()
                );
            };
            write_line(
                &out,
                &RpcResponse::success(
                    id,
                    "prompt",
                    Some(serde_json::json!({ "disposition": "started" })),
                ),
            )?;
            let now = now_millis();
            let prompt = if images.is_empty() {
                pi_agent::transcript::user_message(&message, now)
            } else {
                pi_agent::transcript::user_message_with_images(&message, &images, now)
            };
            // Post-preflight failures are swallowed upstream (the `.catch`
            // checks `preflightSucceeded`, `rpc-mode.ts:425-429`): the run's
            // failure already surfaced as its `agent_end`.
            if run_agent_turn(runtime, vec![prompt], &mut bridge.queues, Arc::clone(&out)).is_err()
            {
                // Swallowed, like the TS.
            }
            Ok(CommandOutcome::Replied)
        }
        RpcCommand::Steer { message, images } => {
            // `session.steer` queues even while idle (`_queueUserInput`,
            // `agent-session.ts:2341-2366`); the next run drains steering
            // first. No input handlers run in RPC mode, so the disposition
            // is always `"queued"`.
            let Some(message) = message else {
                return Err(
                    "Cannot read properties of undefined (reading 'startsWith')".to_string()
                );
            };
            let now = now_millis();
            let queued = pi_agent::transcript::user_message_with_images(&message, &images, now);
            bridge.queues.steer([queued]);
            emit_queue_update(&out, &bridge.queues)?;
            Ok(CommandOutcome::Success(Some(
                serde_json::json!({ "disposition": "queued" }),
            )))
        }
        RpcCommand::FollowUp { message, images } => {
            // `session.followUp` (`agent-session.ts:2388-2398`): the
            // follow-up queue, delivered only when the run would stop.
            let Some(message) = message else {
                return Err(
                    "Cannot read properties of undefined (reading 'startsWith')".to_string()
                );
            };
            let now = now_millis();
            let queued = pi_agent::transcript::user_message_with_images(&message, &images, now);
            bridge.queues.follow_up([queued]);
            emit_queue_update(&out, &bridge.queues)?;
            Ok(CommandOutcome::Success(Some(
                serde_json::json!({ "disposition": "queued" }),
            )))
        }
        RpcCommand::Abort => {
            // `session.abort()` (`rpc-mode.ts:447-450`): no run is in flight
            // under the thread-per-command model, so the abort request is a
            // no-op; the signal plumbing wires in with the concurrent
            // dispatch rework (P6/P7).
            Ok(CommandOutcome::Success(None))
        }
        RpcCommand::ClearQueue => {
            // `session.clearQueue` (`agent-session.ts:2580-2590`): report
            // the queued texts, drop them, and emit the emptied queue.
            let steering = bridge.queues.steering_all();
            let follow_up = bridge.queues.follow_up_all();
            bridge.queues.clear_all();
            emit_queue_update(&out, &bridge.queues)?;
            Ok(CommandOutcome::Success(Some(serde_json::json!({
                "steering": steering.iter().map(queued_message_text).collect::<Vec<String>>(),
                "followUp": follow_up.iter().map(queued_message_text).collect::<Vec<String>>(),
            }))))
        }
        RpcCommand::GetState => {
            // The `RpcSessionState` snapshot (`rpc-types.ts:96-109`,
            // `rpc-mode.ts:469-485`). Under the thread-per-command model a
            // state request only runs while idle, so `isStreaming` and
            // `isCompacting` are `false`. `sessionFile`/`sessionName` are
            // `undefined` when unset, so the keys are dropped.
            let mut state = serde_json::Map::new();
            state.insert(
                "model".to_string(),
                serde_json::to_value(&runtime.model).unwrap_or_default(),
            );
            state.insert(
                "thinkingLevel".to_string(),
                serde_json::json!(runtime.thinking_level.as_str()),
            );
            state.insert("isStreaming".to_string(), serde_json::json!(false));
            state.insert("isCompacting".to_string(), serde_json::json!(false));
            state.insert(
                "steeringMode".to_string(),
                serde_json::json!(queue_mode_str(bridge.queues.steering_mode())),
            );
            state.insert(
                "followUpMode".to_string(),
                serde_json::json!(queue_mode_str(bridge.queues.follow_up_mode())),
            );
            if let Some(session_file) = runtime.session.session_file() {
                state.insert(
                    "sessionFile".to_string(),
                    serde_json::json!(session_file.display().to_string()),
                );
            }
            state.insert(
                "sessionId".to_string(),
                serde_json::json!(runtime.session.get_session_id()),
            );
            if let Some(session_name) = runtime.session.get_session_name() {
                state.insert("sessionName".to_string(), serde_json::json!(session_name));
            }
            state.insert(
                "autoCompactionEnabled".to_string(),
                serde_json::json!(runtime.compaction_settings.enabled),
            );
            state.insert(
                "messageCount".to_string(),
                serde_json::json!(session_messages(runtime).len()),
            );
            state.insert(
                "pendingMessageCount".to_string(),
                serde_json::json!(
                    bridge.queues.steering_all().len() + bridge.queues.follow_up_all().len()
                ),
            );
            Ok(CommandOutcome::Success(Some(Value::Object(state))))
        }
        RpcCommand::SetModel { provider, model_id } => {
            // The snapshot lookup (`rpc-mode.ts:491-499`): a miss echoes the
            // request values into the verbatim message, `undefined`
            // included.
            let provider = provider.as_deref().unwrap_or("undefined");
            let model_id = model_id.as_deref().unwrap_or("undefined");
            let pattern = ModelPattern {
                provider: Some(provider.to_string()),
                id: model_id.to_string(),
                thinking: None,
            };
            let resolved = resolve_model(Some(&pattern), None, None)?;
            runtime.model = resolved.clone();
            // `session.setModel` appends the transcript record
            // (`agent-session.ts:2665`). The per-model thinking-level switch
            // and the auth precheck land with the settings/auth slices.
            runtime
                .session
                .append_model_change(&resolved.provider, &resolved.id)
                .map_err(|error| error.to_string())?;
            Ok(CommandOutcome::Success(Some(
                serde_json::to_value(&resolved).unwrap_or_default(),
            )))
        }
        RpcCommand::GetAvailableModels => {
            // `getAvailableSnapshot` (`rpc-mode.ts:509-512`).
            let models = crate::models_state::all();
            Ok(CommandOutcome::Success(Some(
                serde_json::json!({ "models": models }),
            )))
        }
        RpcCommand::SetThinkingLevel { level } => {
            apply_thinking_level(runtime, level.as_deref().unwrap_or(""), &out)?;
            Ok(CommandOutcome::Success(None))
        }
        RpcCommand::CycleThinkingLevel => {
            // `session.cycleThinkingLevel` (`agent-session.ts:2812-2825`):
            // a non-reasoning model answers `null`.
            if !runtime.model.reasoning {
                return Ok(CommandOutcome::Success(Some(Value::Null)));
            }
            let available = pi_ai::get_supported_thinking_levels(&runtime.model);
            let current = runtime.thinking_level.as_str();
            let next_index = available
                .iter()
                .position(|level| *level == current)
                .map_or(0, |index| (index + 1) % available.len());
            let Some(next) = available.get(next_index).copied() else {
                return Ok(CommandOutcome::Success(Some(Value::Null)));
            };
            apply_thinking_level(runtime, next, &out)?;
            Ok(CommandOutcome::Success(Some(
                serde_json::json!({ "level": next }),
            )))
        }
        RpcCommand::GetAvailableThinkingLevels => {
            // `getAvailableThinkingLevels` (`agent-session.ts:2829-2834`);
            // the port always has a model, so the settings fallback never
            // applies.
            let levels = pi_ai::get_supported_thinking_levels(&runtime.model);
            Ok(CommandOutcome::Success(Some(
                serde_json::json!({ "levels": levels }),
            )))
        }
        RpcCommand::SetSteeringMode { mode } => {
            bridge
                .queues
                .set_steering_mode(queue_mode_from(mode.as_deref()));
            Ok(CommandOutcome::Success(None))
        }
        RpcCommand::SetFollowUpMode { mode } => {
            bridge
                .queues
                .set_follow_up_mode(queue_mode_from(mode.as_deref()));
            Ok(CommandOutcome::Success(None))
        }
        RpcCommand::Compact {
            custom_instructions,
        } => {
            // `session.compact` (`rpc-mode.ts:554-557`): the manual
            // compaction; the result serializes as the `data` payload, a
            // refusal as the error response.
            let mut compaction =
                crate::modes::compaction::AutoCompaction::new(runtime.compaction_settings);
            let mut sink = RpcEventSink {
                writer: Arc::clone(&out),
            };
            let result = compaction.run_manual(
                custom_instructions.as_deref(),
                &mut runtime.session,
                &runtime.model,
                runtime.api_key.as_deref(),
                runtime.access_token.as_deref(),
                runtime.thinking_level,
                runtime.stream_fn.as_ref(),
                &mut sink,
            )?;
            Ok(CommandOutcome::Success(Some(
                serde_json::to_value(&result).unwrap_or_default(),
            )))
        }
        RpcCommand::SetAutoCompaction { enabled } => {
            runtime.compaction_settings.enabled = enabled;
            Ok(CommandOutcome::Success(None))
        }
        RpcCommand::SetAutoRetry { enabled } => {
            runtime.retry_settings.enabled = enabled;
            Ok(CommandOutcome::Success(None))
        }
        RpcCommand::AbortRetry => {
            // `session.abortRetry` (`rpc-mode.ts:573-576`): cancels an
            // active retry backoff. Under the thread-per-command model no
            // retry is in flight, so this is the idle no-op upstream also
            // answers with plain success.
            Ok(CommandOutcome::Success(None))
        }
        RpcCommand::GetEntries { since } => {
            // `get_entries` (`rpc-mode.ts:657-668`): the raw entries with
            // the leaf id; a `since` id cuts everything up to and including
            // it, with the verbatim refusal for an unknown id.
            let entries = runtime.session.get_entries();
            let mut records: Vec<Value> = entries
                .iter()
                .map(|entry| Value::Object(entry.to_record().object().clone()))
                .collect();
            if let Some(since) = &since {
                let Some(index) = entries
                    .iter()
                    .position(|entry| entry.id() == since.as_str())
                else {
                    return Err(format!("Entry not found: {since}"));
                };
                records.drain(..=index);
            }
            let leaf_id = runtime.session.get_leaf_id().map(str::to_string);
            Ok(CommandOutcome::Success(Some(serde_json::json!({
                "entries": records,
                "leafId": leaf_id,
            }))))
        }
        RpcCommand::GetTree => {
            // `get_tree` (`rpc-mode.ts:670-673`).
            let tree = runtime
                .session
                .get_tree()
                .iter()
                .map(tree_node_json)
                .collect::<Vec<Value>>();
            let leaf_id = runtime.session.get_leaf_id().map(str::to_string);
            Ok(CommandOutcome::Success(Some(serde_json::json!({
                "tree": tree,
                "leafId": leaf_id,
            }))))
        }
        RpcCommand::GetLastAssistantText => {
            // `getLastAssistantText` (`agent-session.ts:4528-4553`): the
            // last assistant reply that ended normally (a `stop`, or one
            // carrying text); its text blocks joined and trimmed.
            let text = session_messages(runtime).iter().rev().find_map(|message| {
                let AgentMessage::Assistant(assistant) = message else {
                    return None;
                };
                let has_text = assistant.content.iter().any(|block| {
                    matches!(&block, pi_ai::types::AssistantContent::Text(text) if !text.text.trim().is_empty())
                });
                // Skip messages without text from a turn that did not end
                // normally (`agent-session.ts:4537-4541`).
                if assistant.stop_reason != StopReason::Stop && !has_text {
                    return None;
                }
                let joined = assistant.content.iter().fold(String::new(), |mut text, block| {
                    if let pi_ai::types::AssistantContent::Text(part) = block {
                        text.push_str(&part.text);
                    }
                    text
                });
                let trimmed = joined.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            });
            Ok(CommandOutcome::Success(Some(
                serde_json::json!({ "text": text }),
            )))
        }
        RpcCommand::SetSessionName { name } => {
            // `set_session_name` (`rpc-mode.ts:680-687`): trim, refuse the
            // empty name, persist, and emit the change. `command.name.trim`
            // on `undefined` throws before the empty check.
            let Some(name) = name else {
                return Err("Cannot read properties of undefined (reading 'trim')".to_string());
            };
            let name = name.trim();
            if name.is_empty() {
                return Err("Session name cannot be empty".to_string());
            }
            runtime
                .session
                .append_session_info(name)
                .map_err(|error| error.to_string())?;
            emit_event(
                &out,
                &AgentEvent::SessionInfoChanged {
                    name: runtime.session.get_session_name(),
                },
            )?;
            Ok(CommandOutcome::Success(None))
        }
        RpcCommand::GetMessages => {
            // `get_messages` (`rpc-mode.ts:693-695`): the live projection —
            // `agent.state.messages` includes the leading system message.
            let messages: Vec<Value> = session_messages(runtime)
                .iter()
                .map(|message| serde_json::to_value(message).unwrap_or_default())
                .collect();
            Ok(CommandOutcome::Success(Some(
                serde_json::json!({ "messages": messages }),
            )))
        }
        RpcCommand::GetForkMessages => {
            // `getUserMessagesForForking` (`agent-session.ts:4342-4359`):
            // entry id plus `contentText` for every user message entry that
            // has text.
            let messages: Vec<Value> = runtime
                .session
                .get_entries()
                .iter()
                .filter_map(|entry| {
                    let crate::session::records::SessionEntry::Message(message) = entry else {
                        return None;
                    };
                    let pi_agent::AgentMessage::User(user) = &message.message else {
                        return None;
                    };
                    let text = user_content_text(&user.content);
                    if text.is_empty() {
                        return None;
                    }
                    Some(serde_json::json!({
                        "entryId": entry.id(),
                        "text": text,
                    }))
                })
                .collect();
            Ok(CommandOutcome::Success(Some(
                serde_json::json!({ "messages": messages }),
            )))
        }

        RpcCommand::NewSession { parent_session } => {
            // `new_session` (`rpc-mode.ts:456-463`): the response data is the
            // whole `{ cancelled }` result; a live replacement rebinds the
            // session — the port's fresh agent clears its queues.
            let cancelled = bridge
                .host
                .new_session(runtime, parent_session.as_deref())?;
            if !cancelled {
                bridge.queues = Queues::new();
            }
            Ok(CommandOutcome::Success(Some(
                serde_json::json!({ "cancelled": cancelled }),
            )))
        }
        RpcCommand::CycleModel => {
            // `session.cycleModel` (`agent-session.ts:2686-2759`): the port
            // has no scoped-model list, so this is `_cycleAvailableModel`.
            // A one-entry (or empty) snapshot answers `null`.
            let models = crate::models_state::all();
            let current = models.iter().position(|model| {
                model.provider == runtime.model.provider && model.id == runtime.model.id
            });
            let next_index = match (current, models.len()) {
                (_, 0) => None,
                (None, _) => Some(0),
                (Some(_), 1) => None,
                (Some(index), len) => Some((index + 1) % len),
            };
            let Some(next) = next_index.and_then(|index| models.get(index)) else {
                return Ok(CommandOutcome::Success(Some(Value::Null)));
            };
            let next = (*next).clone();
            runtime.model = next.clone();
            runtime
                .session
                .append_model_change(&next.provider, &next.id)
                .map_err(|error| error.to_string())?;
            // Clone before the mutable borrow: upstream switches to the
            // settings level, which the port does not carry in `PrintRuntime`.
            let current_level = runtime.thinking_level.as_str().to_string();
            apply_thinking_level(runtime, &current_level, &out)?;
            Ok(CommandOutcome::Success(Some(serde_json::json!({
                "model": next,
                "thinkingLevel": runtime.thinking_level.as_str(),
                "isScoped": false,
            }))))
        }
        RpcCommand::Bash {
            command,
            exclude_from_context,
        } => {
            // `bash` (`rpc-mode.ts:582-603`): `user_bash` first, then the
            // local operation; the extension result is recorded verbatim.
            let Some(command) = command else {
                // Port-side guard for a malformed request: upstream throws a
                // Node TypeError deep in `spawn` (WIRING.md).
                return Err(
                    "The \"command\" argument must be of type string. Received undefined"
                        .to_string(),
                );
            };
            let result = bridge
                .host
                .bash(runtime, &command, exclude_from_context, id)?;
            Ok(CommandOutcome::Success(Some(result)))
        }
        RpcCommand::AbortBash => {
            // `session.abortBash()` (`rpc-mode.ts:605-608`): cancels every
            // live shell. The port's local run is synchronous, so the default
            // host has nothing in flight; a host with a PTY can override.
            bridge.host.abort_bash();
            Ok(CommandOutcome::Success(None))
        }
        RpcCommand::GetSessionStats => {
            // `get_session_stats` (`rpc-mode.ts:614-617`).
            Ok(CommandOutcome::Success(Some(session_stats_json(runtime))))
        }
        RpcCommand::ExportHtml { output_path } => {
            // `session.exportToHtml(outputPath)` (`rpc-mode.ts:619-622`).
            // The port embeds upstream's templates byte-for-byte and reuses
            // its in-memory/unsaved error strings.
            let specs: Vec<ToolSpec> = runtime.executor.tools();
            let state = ExportState {
                system_prompt: Some(runtime.system_prompt.clone()),
                tools: Some(
                    specs
                        .iter()
                        .map(|spec| crate::session::export_html::ExportTool {
                            name: spec.name.clone(),
                            description: spec.description.clone(),
                            parameters: spec.parameters.clone(),
                        })
                        .collect(),
                ),
            };
            let options = ExportOptions {
                output_path: output_path.as_deref(),
                theme_name: None,
                tool_renderer: None,
            };
            let path = export_session_to_html(&runtime.session, Some(&state), &options)?;
            Ok(CommandOutcome::Success(Some(
                serde_json::json!({ "path": path.display().to_string() }),
            )))
        }
        RpcCommand::SwitchSession { session_path } => {
            // `switch_session` (`rpc-mode.ts:624-630`): the verbatim error for
            // a missing path is upstream's `normalizePath` deref.
            let Some(session_path) = session_path else {
                return Err(
                    "Cannot read properties of undefined (reading 'startsWith')".to_string()
                );
            };
            let cancelled = bridge.host.switch_session(runtime, &session_path)?;
            if !cancelled {
                bridge.queues = Queues::new();
            }
            Ok(CommandOutcome::Success(Some(
                serde_json::json!({ "cancelled": cancelled }),
            )))
        }
        RpcCommand::Fork { entry_id } => {
            // `fork` (`rpc-mode.ts:632-638`): no `position` on the wire, so
            // upstream's default `"before"` applies. `text` is dropped when
            // the result carries no `selectedText` (JS `undefined`).
            let Some(entry_id) = entry_id else {
                return Err("Invalid entry ID for forking".to_string());
            };
            let result = bridge
                .host
                .fork(runtime, &entry_id, RpcForkPosition::Before)?;
            if !result.cancelled {
                bridge.queues = Queues::new();
            }
            let mut data = serde_json::Map::new();
            if let Some(text) = result.selected_text {
                data.insert("text".to_string(), serde_json::json!(text));
            }
            data.insert("cancelled".to_string(), serde_json::json!(result.cancelled));
            Ok(CommandOutcome::Success(Some(Value::Object(data))))
        }
        RpcCommand::Clone => {
            // `clone` (`rpc-mode.ts:640-650`): fork the leaf `"at"`, with the
            // verbatim refusal when no entry is selected.
            let Some(leaf_id) = runtime.session.get_leaf_id().map(str::to_string) else {
                return Err("Cannot clone session: no current entry selected".to_string());
            };
            let result = bridge.host.fork(runtime, &leaf_id, RpcForkPosition::At)?;
            if !result.cancelled {
                bridge.queues = Queues::new();
            }
            Ok(CommandOutcome::Success(Some(
                serde_json::json!({ "cancelled": result.cancelled }),
            )))
        }
        RpcCommand::GetCommands => {
            // `get_commands` (`rpc-mode.ts:701-732`): extension, prompt,
            // skill order is owned by the host callback.
            let commands = bridge.host.get_commands(runtime)?;
            Ok(CommandOutcome::Success(Some(
                serde_json::json!({ "commands": commands }),
            )))
        }
    }
}

/// `session.setThinkingLevel` (`agent-session.ts:2789-2806`): clamp against
/// the model's levels, keep the runtime level, and persist the transcript
/// change plus the event — only when the level actually changes.
fn apply_thinking_level(
    runtime: &mut PrintRuntime,
    level: &str,
    out: &RpcOut,
) -> Result<(), String> {
    let available = pi_ai::get_supported_thinking_levels(&runtime.model);
    let effective = clamp_thinking_level(&available, level);
    let effective_level = thinking_level_from_str(&effective)?;
    let is_changing = effective != runtime.thinking_level.as_str();
    runtime.thinking_level = effective_level;
    if is_changing {
        runtime
            .session
            .append_thinking_level_change(&effective)
            .map_err(|error| error.to_string())?;
        emit_event(
            out,
            &AgentEvent::ThinkingLevelChanged {
                level: effective.clone(),
            },
        )?;
    }
    Ok(())
}

/// The `SessionTreeNode` wire shape (`session-manager.ts:200-207`):
/// `entry`/`children` with the optional `label`/`labelTimestamp` keys
/// dropped when unset.
fn tree_node_json(node: &SessionTreeNode) -> Value {
    let children = node
        .children
        .iter()
        .map(tree_node_json)
        .collect::<Vec<Value>>();
    let mut object = serde_json::json!({
        "entry": Value::Object(node.entry.to_record().object().clone()),
        "children": children,
    });
    if let Some(label) = &node.label {
        object["label"] = serde_json::json!(label);
    }
    if let Some(label_timestamp) = &node.label_timestamp {
        object["labelTimestamp"] = serde_json::json!(label_timestamp);
    }
    object
}

/// One full agent run, events streamed to `output` as they happen.
#[allow(clippy::too_many_lines)] // mirrors the TS run loop; split when extending
fn run_agent_turn(
    runtime: &mut PrintRuntime,
    prompts: Vec<AgentMessage>,
    queues: &mut Queues,
    output: RpcOut,
) -> Result<(), String> {
    let executor = runtime.executor.clone();
    let specs: Vec<ToolSpec> = executor.tools();

    let context = AgentContext {
        messages: session_messages(runtime),
        tools: specs.clone(),
    };

    let user_prompts = prompts;
    let mut sink = RpcEventSink { writer: output };
    let signal = Arc::new(AbortSignal::new());
    // Under a virtual selection every request routes first
    // (`_installAgentRequestProjection`).
    #[cfg(feature = "ext-js")]
    let routing = crate::virtual_models::TurnRouting::install(
        &runtime.model,
        runtime.extension_runner.as_ref(),
        runtime.api_key.clone(),
        &runtime.session,
    );
    let config = AgentLoopConfig {
        model: runtime.model.clone(),
        thinking_level: runtime.thinking_level,
        live_settings: None,
        session_id: None,
        api_key: runtime.api_key.clone(),
        access_token: runtime.access_token.clone(),
        tool_execution: ToolExecutionMode::Parallel,
        hooks: pi_agent::types::ToolCallHooks {
            before: None,
            after: None,
        },
        finish_turn: None,
        #[cfg(feature = "ext-js")]
        prepare_request: routing.as_ref().map(|routing| {
            Arc::clone(&routing.hook) as Arc<dyn pi_agent::agent_loop::PrepareRequest>
        }),
        #[cfg(not(feature = "ext-js"))]
        prepare_request: None,
        prepare_next_turn: None,
        transform_context: runtime.transform_context(),
        convert_to_llm: Arc::new(DefaultConvertToLlm),
    };

    #[cfg(feature = "ext-js")]
    let loop_stream_fn: Arc<dyn pi_agent::types::StreamFn> = match &routing {
        Some(routing) => routing.stream_fn(),
        None => Arc::clone(&runtime.stream_fn),
    };
    #[cfg(not(feature = "ext-js"))]
    let loop_stream_fn: Arc<dyn pi_agent::types::StreamFn> = Arc::clone(&runtime.stream_fn);
    // The auto-retry machine (`agent-session.ts` `_handlePostAgentRun`); the
    // RPC sink forwards the events like any other.
    let mut retry = crate::modes::retry::AgentRetry::new(runtime.retry_settings);

    let mut compaction = crate::modes::compaction::AutoCompaction::new(runtime.compaction_settings);
    let new_messages = loop {
        // `_compactBeforeNextAssistantResponse` (`agent-session.ts:839-848`):
        // the threshold gate keys on the model about to answer; under a
        // virtual selection upstream re-checks the ROUTED model after the
        // resolve (:901-904) — deferred with the prepare-hook rework.
        if !crate::virtual_models::is_virtual_model(&runtime.model)
            && compaction.exceeds_threshold_for_session(&runtime.session, &runtime.model)
        {
            compaction.run(
                pi_agent::events::CompactionReason::Threshold,
                false,
                &mut runtime.session,
                &runtime.model,
                runtime.api_key.as_deref(),
                runtime.access_token.as_deref(),
                runtime.thinking_level,
                runtime.stream_fn.as_ref(),
                &mut sink,
            );
        }
        let messages = {
            // `agent_end` carries the merged `willRetry`
            // (`agent-session.ts:1211`).
            let mut aware = crate::modes::retry::RetryAwareSink {
                inner: &mut sink,
                retry_attempt: retry.attempt(),
                settings: runtime.retry_settings,
                signal: Arc::clone(&signal),
            };
            crate::runtime::block_on(pi_agent::run_agent_loop(
                user_prompts.clone(),
                context.clone(),
                executor.as_ref(),
                &config,
                queues,
                &mut aware,
                &signal,
                loop_stream_fn.as_ref(),
            ))
        };
        let aborted = signal.aborted();
        if retry.will_retry(aborted, &messages) {
            let (attempt, max_attempts, delay_ms, error_message) = retry.schedule(&messages);
            // `_prepareRetry` (`agent-session.ts:3959-3963`): the failed
            // attempt persists raw and drops out of the projection.
            for entry in crate::modes::retry::omit_attempt(&messages, &mut runtime.session) {
                crate::runtime::block_on(
                    sink.emit(pi_agent::events::AgentEvent::EntryAppended { entry }),
                );
            }
            crate::runtime::block_on(sink.emit(pi_agent::events::AgentEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                error_message: error_message.clone(),
            }));
            if retry.sleep(delay_ms, &signal) {
                continue;
            }
            crate::runtime::block_on(sink.emit(pi_agent::events::AgentEvent::AutoRetryEnd {
                success: false,
                attempt,
                final_error: Some(error_message),
            }));
            break messages;
        }
        if retry.attempt() > 0 {
            let success = crate::modes::retry::run_succeeded(&messages);
            crate::runtime::block_on(sink.emit(pi_agent::events::AgentEvent::AutoRetryEnd {
                success,
                attempt: retry.attempt(),
                final_error: if success {
                    None
                } else {
                    Some(crate::modes::retry::last_error_text(&messages))
                },
            }));
            if success {
                retry.reset();
            }
        }
        // Upstream `_handlePostAgentRun` (`agent-session.ts:2062-2098`)
        // decides the retry first — compaction only runs when no retry was
        // scheduled.
        let mut full_messages = context.messages.clone();
        full_messages.extend(messages.iter().cloned());
        match compaction.post_run_decision(
            aborted,
            &messages,
            &full_messages,
            &runtime.session,
            &runtime.model,
        ) {
            crate::modes::compaction::Decision::None => break messages,
            crate::modes::compaction::Decision::Threshold => {
                compaction.run(
                    pi_agent::events::CompactionReason::Threshold,
                    false,
                    &mut runtime.session,
                    &runtime.model,
                    runtime.api_key.as_deref(),
                    runtime.access_token.as_deref(),
                    runtime.thinking_level,
                    runtime.stream_fn.as_ref(),
                    &mut sink,
                );
                break messages;
            }
            crate::modes::compaction::Decision::Overflow { will_retry } => {
                if will_retry {
                    // `_omitRecoveryAttempt` before the recovery compaction
                    // (`agent-session.ts:3209-3211`).
                    for entry in crate::modes::retry::omit_attempt(&messages, &mut runtime.session)
                    {
                        crate::runtime::block_on(
                            sink.emit(pi_agent::events::AgentEvent::EntryAppended { entry }),
                        );
                    }
                }
                // The overflow compact-and-retry re-runs the turn once
                // (`agent.continue()`).
                let continued = compaction.run(
                    pi_agent::events::CompactionReason::Overflow,
                    will_retry,
                    &mut runtime.session,
                    &runtime.model,
                    runtime.api_key.as_deref(),
                    runtime.access_token.as_deref(),
                    runtime.thinking_level,
                    runtime.stream_fn.as_ref(),
                    &mut sink,
                );
                if will_retry && continued {
                    continue;
                }
                break messages;
            }
            crate::modes::compaction::Decision::RecoveryFailed { message } => {
                // The verbatim one-shot failure (`agent-session.ts:3187-3206`).
                crate::runtime::block_on(sink.emit(pi_agent::events::AgentEvent::CompactionEnd {
                    reason: pi_agent::events::CompactionReason::Overflow,
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(message),
                }));
                break messages;
            }
        }
    };
    // `agent_settled` closes the run (`agent-session.ts:1141-1152`): the
    // run's abort request decides `aborted`.
    crate::runtime::block_on(sink.emit(pi_agent::events::AgentEvent::AgentSettled {
        aborted: signal.aborted(),
    }));
    #[cfg(feature = "ext-js")]
    if let Some(routing) = &routing {
        // `entry_appended` for each router-state record
        // (`agent-session.ts:894-897`).
        for entry in routing.persist_state(&mut runtime.session) {
            crate::runtime::block_on(
                sink.emit(pi_agent::events::AgentEvent::EntryAppended { entry }),
            );
        }
    }

    for message in &new_messages {
        runtime
            .session
            .append_message(message.clone())
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::BufReader;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A process pipe pair drives the bridge deterministically: the test
    /// writes lines on the writer side while the reader thread consumes.
    struct BridgeFixture {
        /// `None` once `take_bridge` moved it into the mode thread.
        bridge: Option<RpcBridge>,
        writer: std::io::PipeWriter,
        out: SharedBuf,
    }

    impl BridgeFixture {
        fn new() -> Self {
            let (reader, writer) = std::io::pipe().expect("pipe");
            let out = SharedBuf::default();
            let bridge = create_bridge(Box::new(BufReader::new(reader)), Box::new(out.clone()));
            Self {
                bridge: Some(bridge),
                writer,
                out,
            }
        }

        /// The bridge for drains that leave it in place.
        fn bridge(&self) -> &RpcBridge {
            self.bridge.as_ref().expect("bridge still in the fixture")
        }

        /// Moves the bridge into the mode thread; the fixture keeps the
        /// pipe writer and the shared output.
        fn take_bridge(&mut self) -> RpcBridge {
            self.bridge.take().expect("bridge still in the fixture")
        }

        /// Installs a host double before the bridge moves to the mode thread.
        fn set_host(&mut self, host: Box<dyn RpcHost + Send>) {
            self.bridge
                .as_mut()
                .expect("bridge still in the fixture")
                .host = host;
        }

        fn write_line(&self, line: &str) {
            use std::io::Write as _;
            (&self.writer)
                .write_all(format!("{line}\n").as_bytes())
                .expect("pipe write");
        }

        fn output_lines(&self) -> Vec<String> {
            self.out.string().lines().map(str::to_string).collect()
        }

        /// Waits for the output to reach `count` lines, polling like the
        /// reader thread does.
        fn wait_for_output(&self, count: usize) -> Vec<String> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let lines = self.output_lines();
                if lines.len() >= count {
                    return lines;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for {count} lines: {lines:?}"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }

    /// The bridge's output, shared so tests can assert on it after the run.
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("shared buf").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SharedBuf {
        fn string(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("shared buf")).into_owned()
        }
    }

    /// Runs [`run_rpc_mode`] on a thread over the fixture's bridge; the
    /// bridge's reader thread keeps consuming the pipe the test writes to.
    struct ModeThread(std::thread::JoinHandle<Result<i32, String>>);

    impl ModeThread {
        fn start(runtime: PrintRuntime, bridge: RpcBridge) -> Self {
            Self(
                std::thread::Builder::new()
                    .name("rpc-mode".to_string())
                    .spawn(move || {
                        let mut runtime = runtime;
                        let mut bridge = bridge;
                        run_rpc_mode(&mut runtime, &mut bridge)
                    })
                    .expect("spawn rpc mode"),
            )
        }

        fn join(self) {
            self.0
                .join()
                .expect("rpc mode thread")
                .expect("rpc mode ok");
        }
    }

    /// The runtime behind the fixtures: a scripted model over a read-only
    /// executor in `dir` (the interactive tests' fixture shape).
    fn test_runtime(
        dir: &std::path::Path,
        stream: Arc<dyn pi_agent::types::StreamFn>,
    ) -> PrintRuntime {
        let model = pi_ai::types::Model {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            api: "test".to_string(),
            provider: "test".to_string(),
            reasoning: true,
            context_window: 200_000,
            max_tokens: 8192,
            ..pi_ai::types::Model::default()
        };
        PrintRuntime {
            model,
            thinking_level: thinking_level_from_str("off").expect("level"),
            api_key: Some("test".to_string()),
            access_token: None,
            stream_fn: stream,
            executor: crate::tools::index::ToolRegistry::read_only().executor(dir),
            system_prompt: "test".to_string(),
            session: crate::session::SessionManager::in_memory(
                dir,
                &crate::session::store::NewSessionOptions::default(),
                Vec::new(),
            )
            .expect("session"),
            project_trusted: false,
            retry_settings: crate::config::settings::RetrySettings {
                enabled: true,
                max_retries: 3,
                base_delay_ms: 2000,
                max_agent_delay_ms: 60_000,
            },
            compaction_settings: crate::session::compaction::DEFAULT_COMPACTION_SETTINGS,
            #[cfg(feature = "ext-js")]
            extension_runner: None,
        }
    }

    /// The scripted model: every turn answers with text and stops, so the
    /// steering queued before the prompt is delivered inside the run
    /// (`agent-loop.ts:185`).
    struct TextScript {
        calls: AtomicUsize,
    }

    impl pi_agent::types::StreamFn for TextScript {
        fn stream(
            &self,
            _request: pi_agent::types::StreamRequest,
        ) -> futures::stream::BoxStream<'static, pi_ai::types::AssistantMessageEvent> {
            use pi_ai::types::AssistantMessageEvent;
            let turn = self.calls.fetch_add(1, Ordering::SeqCst);
            let text = if turn == 0 {
                "first reply"
            } else {
                "second reply"
            };
            let partial = serde_json::from_value::<pi_ai::types::AssistantMessage>(json!({
                "role": "assistant",
                "content": [{ "type": "text", "text": text }],
                "api": "test",
                "provider": "test",
                "model": "test-model",
                "usage": {
                    "input": 10, "output": 5, "cacheRead": 0, "cacheWrite": 0,
                    "totalTokens": 15,
                    "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0,
                              "cacheWrite": 0.0, "total": 0.0 }
                },
                "stopReason": "stop",
                "timestamp": 0
            }))
            .expect("assistant message");
            let end_event = AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: text.to_string(),
                partial: partial.clone(),
            };
            Box::pin(futures::stream::iter(vec![
                AssistantMessageEvent::Start {
                    partial: partial.clone(),
                },
                AssistantMessageEvent::TextStart {
                    content_index: 0,
                    partial: partial.clone(),
                },
                end_event,
                AssistantMessageEvent::Done {
                    reason: pi_ai::types::StopReason::Stop,
                    message: partial,
                },
            ]))
        }
    }

    /// The stream the state-only commands must never reach.
    struct NeverStream;

    impl pi_agent::types::StreamFn for NeverStream {
        fn stream(
            &self,
            _request: pi_agent::types::StreamRequest,
        ) -> futures::stream::BoxStream<'static, pi_ai::types::AssistantMessageEvent> {
            unreachable!("state-only commands never stream");
        }
    }

    #[test]
    fn response_frames_drop_absent_keys_and_echo_the_id() {
        // `rpc-mode.ts:64-77`: `JSON.stringify` drops the `undefined` keys.
        let success = RpcResponse::success(Some("req-1"), "abort", None);
        assert_eq!(
            serde_json::to_string(&success).expect("serialize"),
            r#"{"type":"response","id":"req-1","command":"abort","success":true}"#
        );
        let with_data =
            RpcResponse::success(None, "steer", Some(json!({ "disposition": "queued" })));
        assert_eq!(
            serde_json::to_string(&with_data).expect("serialize"),
            r#"{"type":"response","command":"steer","success":true,"data":{"disposition":"queued"}}"#
        );
        let failure =
            RpcResponse::failure(Some("req-2"), Some("set_model"), "Model not found: a/b");
        assert_eq!(
            serde_json::to_string(&failure).expect("serialize"),
            r#"{"type":"response","id":"req-2","command":"set_model","success":false,"error":"Model not found: a/b"}"#
        );
        // The parse failure carries `command: "parse"` and no id.
        let parse = RpcResponse::failure(None, Some("parse"), "Failed to parse command: boom");
        assert_eq!(
            serde_json::to_string(&parse).expect("serialize"),
            r#"{"type":"response","command":"parse","success":false,"error":"Failed to parse command: boom"}"#
        );
    }

    #[test]
    fn response_frames_round_trip_through_serde() {
        let original = RpcResponse::success(
            Some("req-9"),
            "get_state",
            Some(json!({ "sessionId": "s" })),
        );
        let line = serde_json::to_string(&original).expect("serialize");
        let parsed: RpcResponse = serde_json::from_str(&line).expect("deserialize");
        assert_eq!(parsed, original);
        let failure = RpcResponse::failure(None, None, "Unknown command: undefined");
        let line = serde_json::to_string(&failure).expect("serialize");
        let parsed: RpcResponse = serde_json::from_str(&line).expect("deserialize");
        assert_eq!(parsed, failure);
        assert_eq!(parsed.command, None);
    }

    #[test]
    fn parse_request_classifies_the_envelope() {
        // The discriminant is `type` itself (`rpc-types.ts:20-74`).
        let (incoming, id, type_name) =
            parse_request(r#"{"type":"prompt","id":"7","message":"hi"}"#).expect("parse");
        assert_eq!(
            incoming,
            Incoming::Command(RpcCommand::Prompt {
                message: Some("hi".to_string()),
                images: Vec::new(),
                streaming_behavior: None,
            })
        );
        assert_eq!(id.as_deref(), Some("7"));
        assert_eq!(type_name.as_deref(), Some("prompt"));

        let (incoming, id, _) = parse_request(
            r#"{"type":"steer","message":"m","images":[{"data":"d","mimeType":"image/png"}]}"#,
        )
        .expect("parse");
        match incoming {
            Incoming::Command(RpcCommand::Steer { message, images }) => {
                assert_eq!(message.as_deref(), Some("m"));
                assert_eq!(images.len(), 1);
            }
            _ => panic!("steer command"),
        }
        assert_eq!(id, None);

        // A junk `type` is the unknown frame; a missing one is `undefined`.
        let (incoming, _, type_name) =
            parse_request(r#"{"type":"bash","command":"ls","id":"x"}"#).expect("parse");
        assert_eq!(incoming, Incoming::Unknown(Some("bash".to_string())));
        assert_eq!(type_name.as_deref(), Some("bash"));
        let (incoming, id, type_name) = parse_request(r#"{"payload":true}"#).expect("parse");
        assert_eq!(incoming, Incoming::Unknown(None));
        assert_eq!(id, None);
        assert_eq!(type_name, None);

        // The verbatim parse failure prefix (`rpc-mode.ts:777`).
        let failure = parse_request("{oops").expect_err("parse failure");
        assert!(
            failure.starts_with("Failed to parse command: "),
            "{failure}"
        );
        // A non-string id is not echoed.
        let (_, id, _) = parse_request(r#"{"type":"abort","id":12}"#).expect("parse");
        assert_eq!(id, None);
    }

    #[test]
    fn bridge_routes_commands_and_ignores_unknown_ui_responses() {
        let mut fixture = BridgeFixture::new();
        fixture.write_line(r#"{"type":"get_state","id":1}"#);
        // No dialog is pending; the response drops on the floor.
        fixture.write_line(r#"{"type":"extension_ui_response","id":"ui-9","value":"x"}"#);
        fixture.write_line(r#"{"type":"get_state","id":2}"#);
        let bridge = fixture.take_bridge();
        let first = bridge.commands.recv().expect("command");
        assert!(first.contains(r#""type":"get_state""#));
        let second = bridge.commands.recv().expect("command");
        assert!(second.contains(r#""id":2"#));
        drop(fixture.writer);
        assert!(
            bridge.commands.recv().is_err(),
            "stdin close ends the command stream"
        );
    }

    #[test]
    fn rpc_mode_answers_state_text_and_unknown_frames() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
        let session_id = runtime.session.get_session_id().to_string();
        // Moved out of the fixture: the mode thread owns the bridge now.
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        fixture.write_line(r#"{"type":"get_state","id":"s1"}"#);
        fixture.write_line(r#"{"type":"get_last_assistant_text","id":"t1"}"#);
        fixture.write_line(r#"{"type":"bogus","id":"b1"}"#);
        fixture.write_line("{oops");
        fixture.write_line(r#"{"type":"abort","id":"a1"}"#);
        let lines = fixture.wait_for_output(5);
        drop(fixture.writer);
        mode.join();

        let parsed: Vec<Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("line"))
            .collect();
        assert_eq!(parsed.len(), 5, "one line per command: {lines:?}");

        // `get_state` carries the full `RpcSessionState` (`rpc-types.ts:96-109`).
        let state = &parsed[0];
        assert_eq!(state["type"], "response");
        assert_eq!(state["id"], "s1");
        assert_eq!(state["command"], "get_state");
        assert_eq!(state["success"], true);
        assert_eq!(state["data"]["thinkingLevel"], "off");
        assert_eq!(state["data"]["isStreaming"], false);
        assert_eq!(state["data"]["isCompacting"], false);
        assert_eq!(state["data"]["steeringMode"], "one-at-a-time");
        assert_eq!(state["data"]["followUpMode"], "one-at-a-time");
        assert_eq!(
            state["data"]["sessionId"].as_str(),
            Some(session_id.as_str())
        );
        assert_eq!(state["data"]["autoCompactionEnabled"], true);
        assert_eq!(state["data"]["pendingMessageCount"], 0);
        assert_eq!(
            state["data"]["model"]["provider"].as_str(),
            Some("test"),
            "the model object crosses: {}",
            state["data"]["model"]
        );
        assert!(
            state["data"].get("sessionFile").is_none(),
            "in-memory sessions carry no sessionFile"
        );
        assert!(
            state["data"].get("sessionName").is_none(),
            "unnamed sessions carry no sessionName"
        );

        // No assistant reply yet: `text` is `null`.
        let last = &parsed[1];
        assert_eq!(last["command"], "get_last_assistant_text");
        assert_eq!(last["data"]["text"], Value::Null);

        // The unknown frame echoes the raw type into both fields.
        let unknown = &parsed[2];
        assert_eq!(unknown["command"], "bogus");
        assert_eq!(unknown["success"], false);
        assert_eq!(unknown["error"], "Unknown command: bogus");
        assert_eq!(unknown["id"], "b1");

        // The parse failure names `parse` and carries no id.
        let parse = &parsed[3];
        assert_eq!(parse["command"], "parse");
        assert_eq!(parse["success"], false);
        assert!(
            parse["error"]
                .as_str()
                .unwrap_or_default()
                .starts_with("Failed to parse command: "),
            "{}",
            parse["error"]
        );
        assert!(parse.get("id").is_none());

        // The data-less success has no `data` key.
        let abort = &parsed[4];
        assert_eq!(abort["command"], "abort");
        assert_eq!(abort["success"], true);
        assert!(abort.get("data").is_none());
        assert_eq!(abort["id"], "a1");
    }

    #[test]
    fn rpc_mode_queues_steering_and_streams_the_run() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = test_runtime(
            dir.path(),
            Arc::new(TextScript {
                calls: AtomicUsize::new(0),
            }),
        );
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        // Steering while idle queues, emits the queue update, and answers
        // `"queued"` (`rpc-mode.ts:437-440`).
        fixture.write_line(r#"{"type":"steer","id":"s1","message":"steered text"}"#);
        let lines = fixture.wait_for_output(2);
        assert_eq!(
            lines[0],
            r#"{"type":"queue_update","steering":["steered text"],"followUp":[]}"#
        );
        assert_eq!(
            lines[1],
            r#"{"type":"response","id":"s1","command":"steer","success":true,"data":{"disposition":"queued"}}"#
        );

        // The prompt's response precedes the event stream
        // (`rpc-mode.ts:409-435`).
        fixture.write_line(r#"{"type":"prompt","id":"p1","message":"first prompt"}"#);
        let lines = fixture.wait_for_output(3);
        assert!(
            lines[2].contains(r#""command":"prompt""#) && lines[2].contains("started"),
            "{}",
            lines[2]
        );
        // Wait for the run to settle.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let lines = loop {
            let lines = fixture.output_lines();
            if lines.iter().any(|line| line.contains("agent_settled")) {
                break lines;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "run did not settle: {lines:?}",
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        drop(fixture.writer);
        mode.join();

        let values: Vec<Value> = lines
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let types: Vec<&str> = values
            .iter()
            .filter_map(|value| value["type"].as_str())
            .collect();
        // The queued steering text is delivered as a user message inside the
        // run, before the settle event (`agent-loop.ts:185`).
        let delivery_index = values
            .iter()
            .position(|value| {
                value["message"]["role"] == "user"
                    && value["message"]["content"][0]["text"] == "steered text"
            })
            .expect("the queued steering message is delivered");
        let settle_index = types
            .iter()
            .position(|kind| *kind == "agent_settled")
            .expect("agent_settled");
        assert!(
            delivery_index < settle_index,
            "the delivery happens inside the run"
        );
        assert_eq!(types.last().copied(), Some("agent_settled"));
        // The responses precede the first event.
        let agent_start = types
            .iter()
            .position(|kind| *kind == "agent_start")
            .expect("agent_start");
        assert!(agent_start > 2, "events stream after the responses");
    }

    #[test]
    fn rpc_mode_sets_thinking_level_with_the_clamp_and_the_event() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        fixture.write_line(r#"{"type":"set_thinking_level","id":"h1","level":"high"}"#);
        // A junk level clamps to the first available
        // (`pi-ai/src/models.ts:1318-1336`), which `off` differs from, so it
        // changes too.
        fixture.write_line(r#"{"type":"set_thinking_level","id":"h2","level":"bogus"}"#);
        let lines = fixture.wait_for_output(4);
        drop(fixture.writer);
        mode.join();

        let parsed: Vec<Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("line"))
            .collect();
        // The change emits the session event and a success without data.
        assert_eq!(parsed[0]["type"], "thinking_level_changed");
        assert_eq!(parsed[0]["level"], "high");
        assert_eq!(parsed[1]["command"], "set_thinking_level");
        assert_eq!(parsed[1]["success"], true);
        assert!(parsed[1].get("data").is_none());
        assert_eq!(parsed[2]["type"], "thinking_level_changed");
        assert_eq!(parsed[2]["level"], "off");
        assert_eq!(parsed[3]["command"], "set_thinking_level");
        assert_eq!(parsed[3]["success"], true);
    }

    #[test]
    fn rpc_mode_clears_the_queue_and_reports_the_texts() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        fixture.write_line(r#"{"type":"steer","message":"one"}"#);
        fixture.write_line(r#"{"type":"follow_up","message":"two"}"#);
        fixture.write_line(r#"{"type":"clear_queue","id":"c1"}"#);
        let lines = fixture.wait_for_output(6);
        drop(fixture.writer);
        mode.join();

        let parsed: Vec<Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("line"))
            .collect();
        assert_eq!(parsed[0]["type"], "queue_update");
        assert_eq!(parsed[0]["steering"], json!(["one"]));
        assert_eq!(parsed[1]["command"], "steer");
        assert_eq!(parsed[2]["type"], "queue_update");
        assert_eq!(parsed[2]["followUp"], json!(["two"]));
        assert_eq!(parsed[3]["command"], "follow_up");
        // The emptied queue update precedes the response, like the session's
        // own `_emitQueueUpdate` inside `clearQueue`
        // (`agent-session.ts:2588`).
        assert_eq!(parsed[4]["type"], "queue_update");
        assert_eq!(parsed[4]["steering"], json!([]));
        assert_eq!(parsed[4]["followUp"], json!([]));
        let clear = &parsed[5];
        assert_eq!(clear["command"], "clear_queue");
        assert_eq!(clear["data"]["steering"], json!(["one"]));
        assert_eq!(clear["data"]["followUp"], json!(["two"]));
    }

    #[test]
    fn rpc_mode_names_the_session_and_refuses_empty_verbatim() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        fixture.write_line(r#"{"type":"set_session_name","id":"n1","name":"  My Session  "}"#);
        fixture.write_line(r#"{"type":"set_session_name","id":"n2","name":"   "}"#);
        fixture.write_line(r#"{"type":"set_session_name","id":"n3"}"#);
        let lines = fixture.wait_for_output(4);
        drop(fixture.writer);
        mode.join();

        let parsed: Vec<Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("line"))
            .collect();
        // The name persists and `session_info_changed` carries it
        // (`agent-session.ts:4118-4121`).
        assert_eq!(parsed[0]["type"], "session_info_changed");
        assert_eq!(parsed[0]["name"], "My Session");
        assert_eq!(parsed[1]["command"], "set_session_name");
        assert_eq!(parsed[1]["success"], true);
        // The empty name refuses verbatim (`rpc-mode.ts:683`).
        assert_eq!(parsed[2]["success"], false);
        assert_eq!(parsed[2]["error"], "Session name cannot be empty");
        // A missing name throws on `trim` before the empty check.
        assert_eq!(parsed[3]["success"], false);
        assert_eq!(
            parsed[3]["error"],
            "Cannot read properties of undefined (reading 'trim')"
        );
    }

    #[cfg(feature = "ext-js")]
    #[test]
    fn sink_request_writes_wire_shape_and_response_resolves() {
        let fixture = BridgeFixture::new();
        fixture.bridge().sink.request(&json!({
            "id": "ui-1",
            "method": "select",
            "title": "Pick",
            "options": ["a", "b"],
        }));
        let lines = fixture.output_lines();
        assert_eq!(lines.len(), 1, "one request crossed");
        assert!(
            lines[0].starts_with(r#"{"type":"extension_ui_request","id":"ui-1","method":"select""#),
            "type leads the wire shape, like upstream's output object: {}",
            lines[0]
        );
        fixture.write_line(r#"{"type":"extension_ui_response","id":"ui-1","value":"a"}"#);
        let response = fixture.bridge().sink.wait_response("ui-1", "select");
        // The sink is transport-level: the whole frame crosses; the JS
        // glue's per-method parseResponse extracts the payload.
        assert_eq!(
            response,
            json!({"type":"extension_ui_response","id":"ui-1","value":"a"})
        );
    }

    #[cfg(feature = "ext-js")]
    #[test]
    fn sink_timeout_resolves_to_the_method_default() {
        let fixture = BridgeFixture::new();
        fixture.bridge().sink.request(&json!({
            "id": "ui-2",
            "method": "confirm",
            "title": "Proceed",
            "timeout": 40,
        }));
        let started = std::time::Instant::now();
        let response = fixture.bridge().sink.wait_response("ui-2", "confirm");
        assert_eq!(
            response,
            Value::Null,
            "the JS glue parses null to the default"
        );
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(40),
            "the timeout delayed the default"
        );
    }

    #[cfg(feature = "ext-js")]
    #[test]
    fn sink_fire_and_forget_requests_only_write() {
        let fixture = BridgeFixture::new();
        fixture.bridge().sink.request(&json!({
            "id": "ui-3",
            "method": "notify",
            "message": "hello",
            "notifyType": "info",
        }));
        assert_eq!(fixture.output_lines().len(), 1, "the notify crossed");
        // No waiter was registered: the wait returns the default at once.
        let response = fixture.bridge().sink.wait_response("ui-3", "notify");
        assert_eq!(response, Value::Null);
    }

    /// A host double for the cancel paths and the command table: no session
    /// files, no shells, no extensions.
    struct CancellingHost;

    impl RpcHost for CancellingHost {
        fn new_session(
            &self,
            _runtime: &mut PrintRuntime,
            _parent: Option<&str>,
        ) -> Result<bool, String> {
            Ok(true)
        }

        fn switch_session(&self, _runtime: &mut PrintRuntime, _path: &str) -> Result<bool, String> {
            Ok(true)
        }

        fn fork(
            &self,
            _runtime: &mut PrintRuntime,
            _entry_id: &str,
            _position: RpcForkPosition,
        ) -> Result<RpcForkResult, String> {
            Ok(RpcForkResult {
                selected_text: None,
                cancelled: true,
            })
        }

        fn bash(
            &self,
            _runtime: &mut PrintRuntime,
            _command: &str,
            _exclude: bool,
            _id: Option<&str>,
        ) -> Result<Value, String> {
            Ok(json!({
                "output": "hosted",
                "exitCode": 7,
                "cancelled": false,
                "truncated": false,
            }))
        }

        fn get_commands(&self, _runtime: &PrintRuntime) -> Result<Vec<Value>, String> {
            Ok(vec![json!({
                "name": "extension:deploy",
                "description": "Deploy",
                "source": "extension",
                "sourceInfo": {"path": "<inline:deploy>", "source": "inline",
                               "scope": "temporary", "origin": "top-level"},
            })])
        }
    }

    #[test]
    fn parse_request_recognizes_the_remaining_ten_commands() {
        let cases = [
            (
                r#"{"type":"new_session","id":"1","parentSession":"p"}"#,
                "new_session",
            ),
            (r#"{"type":"cycle_model","id":"2"}"#, "cycle_model"),
            (
                r#"{"type":"bash","id":"3","command":"ls","excludeFromContext":true}"#,
                "bash",
            ),
            (r#"{"type":"abort_bash","id":"4"}"#, "abort_bash"),
            (
                r#"{"type":"get_session_stats","id":"5"}"#,
                "get_session_stats",
            ),
            (
                r#"{"type":"export_html","id":"6","outputPath":"out.html"}"#,
                "export_html",
            ),
            (
                r#"{"type":"switch_session","id":"7","sessionPath":"s.jsonl"}"#,
                "switch_session",
            ),
            (r#"{"type":"fork","id":"8","entryId":"e1"}"#, "fork"),
            (r#"{"type":"clone","id":"9"}"#, "clone"),
            (r#"{"type":"get_commands","id":"10"}"#, "get_commands"),
        ];
        for (line, kind) in cases {
            let (incoming, id, type_name) = parse_request(line).expect("parse");
            assert_eq!(type_name.as_deref(), Some(kind), "{line}");
            assert!(id.is_some(), "{line}");
            match incoming {
                Incoming::Command(command) => {
                    // Round-trip through the success frame: every command echo
                    // is verbatim (`rpc-mode.ts:64-77`).
                    let frame = RpcResponse::success(id.as_deref(), kind, None);
                    assert_eq!(frame.command.as_deref(), Some(kind));
                }
                Incoming::Unknown(other) => panic!("{kind} parsed as unknown: {other:?}"),
            }
        }

        // Field values ride on the variants (`rpc-types.ts:27-74`).
        let (incoming, _, _) =
            parse_request(r#"{"type":"bash","command":"echo hi"}"#).expect("parse");
        assert_eq!(
            incoming,
            Incoming::Command(RpcCommand::Bash {
                command: Some("echo hi".to_string()),
                exclude_from_context: false,
            })
        );
        let (incoming, _, _) = parse_request(r#"{"type":"new_session"}"#).expect("parse");
        assert_eq!(
            incoming,
            Incoming::Command(RpcCommand::NewSession {
                parent_session: None
            })
        );
        let (incoming, _, _) = parse_request(r#"{"type":"clone"}"#).expect("parse");
        assert_eq!(incoming, Incoming::Command(RpcCommand::Clone));
    }

    #[test]
    fn rpc_mode_new_session_and_cycle_model_carry_their_data() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
        let old_id = runtime.session.get_session_id().to_string();
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        fixture.write_line(r#"{"type":"new_session","id":"n1"}"#);
        fixture.write_line(r#"{"type":"get_state","id":"s1"}"#);
        fixture.write_line(r#"{"type":"cycle_model","id":"c1"}"#);
        // Clamping the level to the new model may emit a
        // `thinking_level_changed` event before the response, so wait for the
        // response itself instead of a fixed line count.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let lines = loop {
            let lines = fixture.output_lines();
            if lines
                .iter()
                .any(|line| line.contains(r#""command":"cycle_model""#))
            {
                break lines;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "cycle_model: {lines:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        drop(fixture.writer);
        mode.join();

        let parsed: Vec<Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("line"))
            .collect();
        let find = |name: &str| {
            parsed
                .iter()
                .find(|value| value["command"] == name)
                .unwrap_or_else(|| panic!("{name} response: {parsed:?}"))
        };
        assert_eq!(find("new_session")["success"], true);
        assert_eq!(find("new_session")["data"]["cancelled"], false);
        assert_ne!(
            find("get_state")["data"]["sessionId"].as_str(),
            Some(old_id.as_str()),
            "a new session replaces the store"
        );
        let cycled = find("cycle_model");
        assert_eq!(cycled["success"], true);
        // The static catalog is non-empty unless another test composed a
        // subset; both upstream answers are valid frames.
        if !cycled["data"].is_null() {
            assert_eq!(cycled["data"]["isScoped"], false);
            assert!(cycled["data"].get("model").is_some());
            assert!(cycled["data"].get("thinkingLevel").is_some());
        }
    }

    #[test]
    fn rpc_mode_runs_bash_and_reports_session_stats() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        fixture.write_line(r#"{"type":"bash","id":"b1","command":"printf 'hello\\n'"}"#);
        fixture.write_line(r#"{"type":"get_session_stats","id":"st1"}"#);
        let lines = fixture.wait_for_output(2);
        drop(fixture.writer);
        mode.join();

        let parsed: Vec<Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("line"))
            .collect();
        let bash = &parsed[0];
        assert_eq!(bash["id"], "b1");
        assert_eq!(bash["command"], "bash");
        assert_eq!(bash["success"], true);
        assert_eq!(bash["data"]["output"], "hello\n");
        assert_eq!(bash["data"]["exitCode"], 0);
        assert_eq!(bash["data"]["cancelled"], false);
        assert_eq!(bash["data"]["truncated"], false);

        let stats = &parsed[1]["data"];
        assert_eq!(parsed[1]["command"], "get_session_stats");
        assert_eq!(stats["totalMessages"], 1, "the bashExecution was recorded");
        assert_eq!(stats["userMessages"], 0);
        assert_eq!(stats["toolCalls"], 0);
        assert!(stats["tokens"].is_object());
        assert!(stats["tokens"]["total"].is_number());
        assert!(stats["cost"].is_number());
        assert!(stats.get("sessionFile").is_none(), "in-memory session");
        assert!(stats["contextUsage"].is_object(), "the model has a window");
    }

    #[test]
    fn rpc_mode_reports_the_hosted_bash_result_and_abort() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
        fixture.set_host(Box::new(CancellingHost));
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        fixture.write_line(r#"{"type":"bash","id":"b2","command":"ignored"}"#);
        fixture.write_line(r#"{"type":"abort_bash","id":"a1"}"#);
        let lines = fixture.wait_for_output(2);
        drop(fixture.writer);
        mode.join();

        let parsed: Vec<Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("line"))
            .collect();
        assert_eq!(parsed[0]["data"]["output"], "hosted");
        assert_eq!(parsed[0]["data"]["exitCode"], 7);
        assert_eq!(parsed[1]["command"], "abort_bash");
        assert_eq!(parsed[1]["success"], true);
        assert!(parsed[1].get("data").is_none());
    }

    #[test]
    fn rpc_mode_refuses_export_and_missing_required_fields_verbatim() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        fixture.write_line(r#"{"type":"export_html","id":"e1"}"#);
        fixture.write_line(r#"{"type":"bash","id":"b1"}"#);
        fixture.write_line(r#"{"type":"switch_session","id":"s1"}"#);
        fixture.write_line(r#"{"type":"clone","id":"c1"}"#);
        let lines = fixture.wait_for_output(4);
        drop(fixture.writer);
        mode.join();

        let parsed: Vec<Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("line"))
            .collect();
        assert_eq!(parsed[0]["success"], false);
        assert_eq!(
            parsed[0]["error"],
            "Cannot export in-memory session to HTML"
        );
        assert_eq!(parsed[0]["command"], "export_html");
        assert_eq!(
            parsed[1]["error"],
            "The \"command\" argument must be of type string. Received undefined"
        );
        assert_eq!(
            parsed[2]["error"],
            "Cannot read properties of undefined (reading 'startsWith')"
        );
        assert_eq!(
            parsed[3]["error"],
            "Cannot clone session: no current entry selected"
        );
        assert_eq!(parsed[3]["command"], "clone");
    }

    #[test]
    fn rpc_mode_forks_clones_switches_and_lists_commands() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let mut runtime = test_runtime(dir.path(), Arc::new(NeverStream));
        runtime
            .session
            .append_message(pi_agent::transcript::user_message("fork me", now_millis()))
            .expect("append user message");
        let entry_id = runtime.session.get_entries()[0].id().to_string();
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        // Clone first: forking from the first user message resets an
        // in-memory session, so the leaf would be gone afterwards.
        fixture.write_line(r#"{"type":"clone","id":"c1"}"#);
        fixture.write_line(&format!(
            r#"{{"type":"fork","id":"f1","entryId":"{entry_id}"}}"#
        ));
        fixture.write_line(r#"{"type":"fork","id":"f2","entryId":"missing"}"#);
        fixture.write_line(r#"{"type":"get_commands","id":"g1"}"#);
        let lines = fixture.wait_for_output(4);
        drop(fixture.writer);
        mode.join();

        let parsed: Vec<Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("line"))
            .collect();
        assert_eq!(parsed[0]["command"], "clone");
        assert_eq!(parsed[0]["success"], true);
        assert_eq!(parsed[0]["data"]["cancelled"], false);
        assert_eq!(parsed[1]["command"], "fork");
        assert_eq!(parsed[1]["success"], true);
        assert_eq!(parsed[1]["data"]["text"], "fork me");
        assert_eq!(parsed[1]["data"]["cancelled"], false);
        assert_eq!(parsed[2]["success"], false);
        assert_eq!(parsed[2]["error"], "Invalid entry ID for forking");
        assert_eq!(parsed[3]["command"], "get_commands");
        assert_eq!(parsed[3]["success"], true);
        assert!(parsed[3]["data"]["commands"].is_array());
    }

    #[test]
    fn rpc_mode_honors_host_cancellations_and_the_host_command_table() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
        fixture.set_host(Box::new(CancellingHost));
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        fixture.write_line(r#"{"type":"new_session","id":"n1"}"#);
        fixture.write_line(r#"{"type":"switch_session","id":"s1","sessionPath":"x"}"#);
        fixture.write_line(r#"{"type":"fork","id":"f1","entryId":"e1"}"#);
        fixture.write_line(r#"{"type":"get_commands","id":"g1"}"#);
        let lines = fixture.wait_for_output(4);
        drop(fixture.writer);
        mode.join();

        let parsed: Vec<Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("line"))
            .collect();
        assert_eq!(parsed[0]["data"], json!({"cancelled": true}));
        assert_eq!(parsed[1]["data"], json!({"cancelled": true}));
        // A cancelled fork drops the `text` key like JS drops `undefined`.
        assert_eq!(parsed[2]["data"], json!({"cancelled": true}));
        assert_eq!(parsed[3]["data"]["commands"][0]["source"], "extension");
        assert_eq!(parsed[3]["data"]["commands"][0]["name"], "extension:deploy");
    }
}
