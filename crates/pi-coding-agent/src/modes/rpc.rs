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
//!
//! Prompt preflight (`rpc-mode.ts:409-435`, `agent-session.ts:2162-2263`):
//! the authoritative `prompt` response is emitted only after slash-command
//! resolution through the command registry and the model/auth gate succeed.
//! A failure before that point is the single `success: false` frame with no
//! disposition; a failure after it is swallowed (`:425-429`). The
//! delivery-time `queue_update` mirrors `_handleAgentEvent`
//! (`agent-session.ts:1181-1198`).
//!
//! The rest of the preflight contract (`agent-session.ts:2162-2263`) is
//! exposed through [`RpcHost`] callbacks with inert defaults: extension
//! command handler execution (`:2268-2292`), input handlers and their
//! `"handled"` disposition (`:2206-2216`), skill/template expansion
//! (`:2219-2224,2294-2323`), the no-model refusal (`:2242-2245`,
//! `auth-guidance.ts:18-21`), and the `model_select` extension event
//! (`:2624-2636,2644-2664,2733-2758`, `extensions/types.ts:1152-1160`).
//! Upstream sends `model_select` only to extension listeners — never to
//! `session.subscribe` — so the default emits it through the extension runner,
//! not onto RPC stdout. `WIRING.md` records the host contract.
//!
//! Command dispatch mirrors upstream's fire-and-forget `void handleInputLine`
//! (`rpc-mode.ts:768-824,832-834`): the stdin reader forwards each line to a
//! bounded, FIFO-ordered gate that spawns one scoped thread per command, so
//! the receive loop is never blocked by a slow command and several commands
//! can be in flight together. Execution itself is serialized on the shared
//! `&mut PrintRuntime` (the port's agent run is synchronous, unlike upstream's
//! `await`), preserving the response order the protocol tests require; an
//! `activeCommands` counter (`:90,801,807`) is exposed on [`RpcBridge`] for
//! hosts that gate extension shutdown on it. This is the bounded concurrent
//! path — see `HANDOFF.md` for the remaining delta versus upstream's fully
//! overlapping async commands.
//!
//! Session replacement (`new_session`/`fork`/`switch_session`/`clone`) routes
//! through [`rebind_session`] after a successful swap, mirroring upstream's
//! `rebindSession()` (`rpc-mode.ts:317-371`): the RPC dialog surface is
//! re-bound to the new session's runner and per-session queue state resets.
//!
//! The `blockImages` seam is re-read on every `convertToLlm` call
//! (`sdk.ts:296-331`) through [`dynamic_block_images_converter`], which loads
//! a shared atomic per invocation instead of capturing a startup snapshot.
//! RPC has no mid-session settings channel, so the flag is seeded from the
//! startup value; a host with a live `SettingsManager` can flip it. Every
//! dispatched command also pumps one host MCP tick
//! (`pi-ext-js/src/runner.rs:927`, `extensions/mod.rs:216`) so `session_start`
//! connections and `list_changed` re-registrations settle without a timer.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Condvar, Mutex, mpsc};
#[cfg(feature = "ext-js")]
use std::time::{Duration, Instant};

use futures::FutureExt;
use pi_agent::agent_loop::AgentLoopConfig;
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

/// `PromptDisposition` (`agent-session.ts:339`, emitted at `:2175,2195,2238`):
/// what preflight decided about a submitted prompt. It rides in the `prompt`
/// response's `data.disposition` (`rpc-mode.ts:420-423`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDisposition {
    /// An extension command or input handler consumed the prompt, so no run
    /// starts (`:2171-2178,2213-2216`).
    Handled,
    /// The prompt was queued during a run (`:2226-2240`). The port handles one
    /// command at a time, so this is reachable only through a host override.
    Queued,
    /// The prompt was accepted to start a run (`:2195`).
    Started,
}

impl PromptDisposition {
    /// The verbatim wire string (`rpc-commands.md`'s `disposition` values).
    #[allow(clippy::trivially_copy_pass_by_ref)] // trait-shaped method; callers use &self uniformly
    fn as_str(&self) -> &'static str {
        match *self {
            Self::Handled => "handled",
            Self::Queued => "queued",
            Self::Started => "started",
        }
    }
}

/// The auth facts `_preparePrompt` reads (`agent-session.ts:2242-2260`):
/// `hasConfiguredAuth(provider) || (await checkAuth(provider)) !== undefined`
/// and `isUsingOAuth(provider)`, the latter picking which verbatim refusal
/// text a missing credential gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpcAuthCheck {
    pub configured: bool,
    pub using_oauth: bool,
}

/// The outcome of `_tryExecuteExtensionCommand` (`agent-session.ts:2268-2292`):
/// upstream returns `true` both when the handler completes and when it throws
/// (the error goes to the extension runner, not the RPC response).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcExtensionCommandOutcome {
    /// No command of that name is registered; preflight continues.
    NotFound,
    /// A handler ran (or failed); the prompt is consumed.
    Handled,
}

/// What upstream's `_runInputHandlers` (`:2206-2217`) did to a submission. The
/// host owns any image transformation; the replacement text is what reaches
/// expansion and the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcInputDisposition {
    /// Continue with the original text (`None`) or a host replacement.
    Continue(Option<String>),
    /// An extension input handler consumed the input: `"handled"`, no run.
    Handled,
}

impl Default for RpcInputDisposition {
    /// `Continue(None)`: the original text reaches expansion and the run.
    fn default() -> Self {
        Self::Continue(None)
    }
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
    /// `RpcSlashCommand` (`rpc-types.ts:81-90`). Reused by prompt preflight to
    /// resolve a leading slash command (`:2171-2178`).
    fn get_commands(&self, runtime: &PrintRuntime) -> Result<Vec<Value>, String>;

    /// The model/auth gate of `_preparePrompt` (`:2242-2260`). The default
    /// resolves the port's credential ladder for `provider`; a host with a
    /// live `ModelRuntime` can override both facts. `configured: false` is a
    /// preflight failure: the caller emits upstream's verbatim refusal and no
    /// disposition, and no run starts. [`RpcAuthCheck::using_oauth`] selects
    /// the expired-credentials refusal over the missing-key one.
    fn check_auth(&self, runtime: &PrintRuntime, provider: &str) -> Result<RpcAuthCheck, String>;

    /// `_tryExecuteExtensionCommand` (`:2268-2292`): the leading `/name` and
    /// its args. The default only checks the command table (so a known
    /// extension command stays `"handled"`); a full host executes the handler
    /// here and lets it drive the session before answering.
    fn execute_extension_command(
        &self,
        runtime: &mut PrintRuntime,
        name: &str,
        _args: &str,
    ) -> Result<RpcExtensionCommandOutcome, String> {
        let registered = self.get_commands(runtime)?.into_iter().any(|command| {
            command.get("name").and_then(Value::as_str) == Some(name)
                && command.get("source").and_then(Value::as_str) == Some("extension")
        });
        Ok(if registered {
            RpcExtensionCommandOutcome::Handled
        } else {
            RpcExtensionCommandOutcome::NotFound
        })
    }

    /// `_runInputHandlers` (`:2206-2216`) with `source: "rpc"`
    /// (`rpc-mode.ts:419`). An input handler can consume the message (the
    /// `"handled"` disposition) or rewrite its text before expansion. The
    /// port has no RPC input-handler surface, so the default passes through.
    #[allow(clippy::unused_self)] // an input-handler host needs the receiver
    fn run_input_handlers(
        &self,
        _runtime: &mut PrintRuntime,
        _text: &str,
        _images: &[pi_ai::types::ImageContent],
        _source: &str,
    ) -> Result<RpcInputDisposition, String> {
        Ok(RpcInputDisposition::Continue(None))
    }

    /// The skill/template expansion of `:2219-2224,2294-2323` and
    /// `:2343-2344`. Upstream expands `/skill:name` bodies and
    /// `promptTemplates` into the text actually sent or queued. The default
    /// sends the original text; a full host expands with
    /// `crate::prompt_templates::expand_prompt_template` and the skill reader.
    #[allow(clippy::unused_self)] // a resource-owning host needs the receiver
    fn expand_prompt(
        &self,
        _runtime: &PrintRuntime,
        _text: &str,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }

    /// The `if (!this.model)` gate of `_preparePrompt` (`:2242-2245`). The
    /// port always resolves an initial model, so the default reports one; a
    /// host with a removable model can make the verbatim refusal reachable.
    #[allow(clippy::unused_self)] // a model-owning host needs the receiver
    fn has_model(&self, _runtime: &PrintRuntime) -> Result<bool, String> {
        Ok(true)
    }

    /// `_emitModelSelect`'s event (`:2624-2636`), built by
    /// [`model_select_event`]. Upstream emits to extension listeners only; the
    /// default forwards through the extension runner. A full host can also
    /// surface it on an event bus the RPC client subscribes to.
    fn emit_model_select(&self, runtime: &PrintRuntime, event: &Value) -> Result<(), String> {
        emit_extension_event(runtime, event)
    }

    /// `rebindSession`'s `onError` (`rpc-mode.ts:355-357`): a handler that
    /// throws during `_tryExecuteExtensionCommand` (`:2283-2291`) still
    /// consumes the prompt, while the error crosses as an `extension_error`
    /// frame. The default has no error channel; a full host forwards its
    /// `onError` here.
    #[allow(clippy::unused_self)] // the host owns the extension error channel
    fn emit_extension_error(&self, _runtime: &PrintRuntime, _event: &Value) -> Result<(), String> {
        Ok(())
    }
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

    fn check_auth(&self, runtime: &PrintRuntime, provider: &str) -> Result<RpcAuthCheck, String> {
        // The ladder re-runs for the *selected* provider, so a `set_model`
        // switch is reflected even though the runtime's flag credential was
        // resolved for the startup model (`print_mode.rs:689-705`).
        let credential = crate::modes::print_mode::resolve_wire_credential(
            provider,
            runtime.api_key.as_deref(),
            &crate::config::get_auth_path(),
        );
        let config = crate::models_state::models_json_provider(provider);
        let extension = crate::models_state::extension_provider_config(provider);
        let using_oauth = runtime.access_token.is_some()
            || crate::oauth::provider_auth_methods(provider, config.as_ref(), extension.as_ref())
                .iter()
                .any(|method| matches!(method, crate::oauth::AuthMethod::OAuth(_)));
        Ok(RpcAuthCheck {
            configured: credential.is_some() || runtime.access_token.is_some(),
            using_oauth,
        })
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
    if let Some(runner) = &runtime.extension_runner
        && let Ok(mut runner) = runner.lock()
    {
        runner.shutdown_mcp();
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

/// Notification-only extension emit (`runner.ts:1103-1132`,
/// `agent-session.ts:2630-2635`): upstream's `model_select` reaches extension
/// listeners and nothing else. The RPC client never sees it unless an
/// extension handler re-emits a session event.
#[cfg(feature = "ext-js")]
fn emit_extension_event(runtime: &PrintRuntime, event: &Value) -> Result<(), String> {
    let Some(runner) = &runtime.extension_runner else {
        return Ok(());
    };
    let event_name = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut runner = runner.lock().expect("extension runner mutex poisoned");
    if !runner.has_handlers(&event_name) {
        return Ok(());
    }
    runner.emit(event).map_err(|error| error.to_string())?;
    Ok(())
}

/// Headless twin: no extension listeners to notify.
#[cfg(not(feature = "ext-js"))]
#[allow(clippy::unnecessary_wraps)] // mirrors the ext-js twin's signature
fn emit_extension_event(runtime: &PrintRuntime, event: &Value) -> Result<(), String> {
    let _ = (runtime, event);
    Ok(())
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

/// `_tryExecuteExtensionCommand`'s name parse (`:2268-2273`): the text after
/// the leading `/`, stopping at the first space.
fn slash_command_name(text: &str) -> &str {
    match text.find(' ') {
        Some(index) => &text[1..index],
        None => &text[1..],
    }
}

/// The matching args parse (`:2271-2272`): everything after the first space,
/// untrimmed — upstream hands it to the handler verbatim.
fn slash_command_args(text: &str) -> &str {
    match text.find(' ') {
        Some(index) => &text[index + 1..],
        None => "",
    }
}

/// `modelsAreEqual` (`packages/ai/src/models.ts:1340-1345`): type, id and
/// provider. `_emitModelSelect` (`:2629`) skips a no-op selection.
fn models_are_equal(a: &pi_ai::types::Model, b: &pi_ai::types::Model) -> bool {
    a.api == b.api && a.id == b.id && a.provider == b.provider
}

/// `_emitModelSelect`'s event shape (`agent-session.ts:2630-2635`,
/// `extensions/types.ts:1155-1160`). `previousModel` is absent when undefined,
/// as JS `JSON.stringify` would drop it.
fn model_select_event(model: &Value, previous_model: Option<&Value>, source: &str) -> Value {
    let mut event = serde_json::Map::new();
    event.insert(
        "type".to_string(),
        Value::String("model_select".to_string()),
    );
    event.insert("model".to_string(), model.clone());
    if let Some(previous) = previous_model {
        event.insert("previousModel".to_string(), previous.clone());
    }
    event.insert("source".to_string(), Value::String(source.to_string()));
    Value::Object(event)
}

/// `formatNoApiKeyFoundMessage` (`auth-guidance.ts:22-25`). The `unknown`
/// provider id is rendered as "the selected model" upstream.
fn format_no_api_key_message(provider: &str) -> String {
    let display = if provider == "unknown" {
        "the selected model"
    } else {
        provider
    };
    let docs = crate::prompt::get_docs_path();
    format!(
        "No API key found for {display}.\n\nUse /login to log into a provider via OAuth or API key. See:\n  {docs}/providers.md\n  {docs}/models.md"
    )
}

/// `formatNoModelSelectedMessage` (`auth-guidance.ts:18-21`), verbatim for the
/// no-model branch of `_preparePrompt` (`agent-session.ts:2242-2245`).
fn format_no_model_message() -> String {
    let docs = crate::prompt::get_docs_path();
    format!(
        "No model selected.\n\nUse /login to log into a provider via OAuth or API key. See:\n  {docs}/providers.md\n  {docs}/models.md\n\nThen use /model to select a model."
    )
}

/// An accepted prompt: its disposition plus the text actually sent to the
/// model (after host input-handler rewriting and skill/template expansion).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptPreflight {
    disposition: PromptDisposition,
    text: String,
}

/// The prompt preflight upstream runs before calling `preflightResult`
/// (`agent-session.ts:2162-2196,2202-2263`, `rpc-mode.ts:409-435`). Returns
/// the disposition to put on the wire and the text to run, or the verbatim
/// refusal a failed preflight reports as the single `success: false` response.
///
/// Order mirrors upstream: extension-command handler (`:2171-2178`), input
/// handlers (`:2206-2216`), skill/template expansion (`:2219-2224`), the
/// no-model/auth gates (`:2242-2260`). Every host-bound step has an inert
/// default, so the local host still sends the original text.
fn prompt_preflight(
    runtime: &mut PrintRuntime,
    host: &dyn RpcHost,
    message: &str,
    images: &[pi_ai::types::ImageContent],
) -> Result<PromptPreflight, String> {
    let mut text = message.to_string();

    // `_tryExecuteExtensionCommand` (`:2268-2292`): only when template
    // expansion is enabled (RPC does not disable it) and the text leads `/`.
    if text.starts_with('/') {
        let name = slash_command_name(&text);
        let args = slash_command_args(&text);
        match host.execute_extension_command(runtime, name, args) {
            Ok(RpcExtensionCommandOutcome::Handled) => {
                return Ok(PromptPreflight {
                    disposition: PromptDisposition::Handled,
                    text,
                });
            }
            Ok(RpcExtensionCommandOutcome::NotFound) => {}
            Err(error) => {
                // `:2283-2291`: a throwing handler still returns `true`; the
                // error goes to the extension error channel of `rebindSession`
                // (`rpc-mode.ts:355-357`), not to the prompt response.
                host.emit_extension_error(
                    runtime,
                    &serde_json::json!({
                        "type": "extension_error",
                        "extensionPath": format!("command:{name}"),
                        "event": "command",
                        "error": error,
                    }),
                )?;
                return Ok(PromptPreflight {
                    disposition: PromptDisposition::Handled,
                    text,
                });
            }
        }
    }

    // `_runInputHandlers` (`:2206-2217`) receives `source: "rpc"`.
    match host.run_input_handlers(runtime, &text, images, "rpc")? {
        RpcInputDisposition::Handled => {
            return Ok(PromptPreflight {
                disposition: PromptDisposition::Handled,
                text,
            });
        }
        RpcInputDisposition::Continue(Some(replacement)) => text = replacement,
        RpcInputDisposition::Continue(None) => {}
    }

    // Skill commands and prompt templates (`:2219-2224`). The host owns both
    // resource bodies; the default leaves the text untouched.
    if let Some(expanded) = host.expand_prompt(runtime, &text)? {
        text = expanded;
    }

    // `if (!this.model)` (`:2242-2245`). The port normally resolves an
    // initial model; a host that can clear it gets the verbatim refusal.
    if !host.has_model(runtime)? {
        return Err(format_no_model_message());
    }

    let provider = runtime.model.provider.as_str();
    let auth = host.check_auth(runtime, provider)?;
    if !auth.configured {
        return Err(if auth.using_oauth {
            format!(
                "Authentication failed for \"{provider}\". Credentials may have expired or network is unavailable. Run '/login {provider}' to re-authenticate."
            )
        } else {
            format_no_api_key_message(provider)
        });
    }
    Ok(PromptPreflight {
        disposition: PromptDisposition::Started,
        text,
    })
}

/// The `steer`/`follow_up` preparation of `_queueUserInput`
/// (`agent-session.ts:2325-2352`): reject extension commands, run input
/// handlers, then expand skills/templates. `Ok(None)` means a handler consumed
/// the input (`"handled"`); otherwise the text to enqueue is returned.
fn prepare_queued_input(
    runtime: &mut PrintRuntime,
    host: &dyn RpcHost,
    message: &str,
    images: &[pi_ai::types::ImageContent],
) -> Result<Option<String>, String> {
    // `_throwIfExtensionCommand` (`:2435-2447`) uses the same command table.
    if message.starts_with('/') {
        let name = slash_command_name(message);
        let is_extension = host.get_commands(runtime)?.into_iter().any(|command| {
            command.get("name").and_then(Value::as_str) == Some(name)
                && command.get("source").and_then(Value::as_str) == Some("extension")
        });
        if is_extension {
            return Err(format!(
                "Extension command \"/{name}\" cannot be queued. Use prompt() or execute the command when not streaming."
            ));
        }
    }
    let text = match host.run_input_handlers(runtime, message, images, "rpc")? {
        RpcInputDisposition::Handled => return Ok(None),
        RpcInputDisposition::Continue(Some(replacement)) => replacement,
        RpcInputDisposition::Continue(None) => message.to_string(),
    };
    Ok(Some(host.expand_prompt(runtime, &text)?.unwrap_or(text)))
}

/// `session.getSessionStats()` (`agent-session.ts:4350-4402`,
/// `SessionStats` at `:357-374`). Aggregates over *all* entries, including
/// compacted-away history; `sessionFile`/`contextUsage` are `undefined` and
/// therefore absent from the wire when unavailable.
fn session_stats_json(runtime: &PrintRuntime, has_model: bool) -> Value {
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
    if has_model {
        let context_usage = context_usage_json(runtime);
        if context_usage != Value::Null {
            stats.insert("contextUsage".to_string(), context_usage);
        }
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
    /// The delivery mirror (`agent-session.ts:1183-1198`).
    queued_texts: Arc<Mutex<QueuedTexts>>,
}

impl AgentEventSink for RpcEventSink {
    fn emit(&mut self, event: AgentEvent) -> pi_agent::types::BoxFuture<'_, ()> {
        // `_handleAgentEvent`'s delivery bookkeeping (`agent-session.ts:1181-1198`):
        // when a queued user message starts, upstream removes its text from the
        // queue mirrors and emits the reduced `queue_update` *before*
        // forwarding `message_start`. The loop drained the real queues already,
        // so the mirror is what still knows the text was queued.
        if let AgentEvent::MessageStart { message } = &event
            && let AgentMessage::User(user) = message
        {
            let text = user_content_text(&user.content);
            let mut mirror = self.queued_texts.lock().expect("rpc queued texts");
            if mirror.take_delivered(&text) {
                let update =
                    Value::Object(crate::modes::json_event::event_to_json(&mirror.as_event()))
                        .to_string();
                let mut writer = self.writer.lock().expect("rpc output");
                let _ = writeln!(writer, "{update}");
            }
        }
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

/// Upstream's `_steeringMessages` / `_followUpMessages` mirrors
/// (`agent-session.ts:1183-1198`): the agent loop drains the real queues
/// before it emits `message_start`, so a delivery-time `queue_update` removes
/// the delivered text from this mirror rather than from the drained queues.
#[derive(Debug, Default)]
pub struct QueuedTexts {
    pub steering: Vec<String>,
    pub follow_up: Vec<String>,
}

impl QueuedTexts {
    /// Steering wins, like upstream's check order (`:1186-1197`); returns
    /// whether a queued message matched and was removed.
    fn take_delivered(&mut self, text: &str) -> bool {
        for queue in [&mut self.steering, &mut self.follow_up] {
            if let Some(index) = queue.iter().position(|queued| queued == text) {
                queue.remove(index);
                return true;
            }
        }
        false
    }

    fn as_event(&self) -> AgentEvent {
        AgentEvent::QueueUpdate {
            steering: self.steering.clone(),
            follow_up: self.follow_up.clone(),
        }
    }
}

/// The `activeCommands` mirror of upstream's dispatch loop
/// (`rpc-mode.ts:90,801,807`) plus the high-water mark used to bound how many
/// command tasks a host can leave in flight. Shared with the bridge so a
/// coordinator can gate extension shutdown the way upstream's
/// `checkShutdownRequested` does (`:763-766`).
#[derive(Debug, Default)]
pub struct RpcDispatchStats {
    active: AtomicUsize,
    max_active: AtomicUsize,
    started: AtomicUsize,
}

impl RpcDispatchStats {
    /// Registers a command as in flight (`activeCommands++`, `:414,801`).
    fn enter(&self) {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        self.started.fetch_add(1, Ordering::SeqCst);
    }

    /// Retires a finished command (`activeCommands--`, `:431,807`).
    fn leave(&self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }

    /// Commands currently in flight — upstream's `activeCommands`.
    #[must_use]
    pub fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    /// Peak number of simultaneously in-flight commands.
    #[must_use]
    pub fn max_active(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }

    /// Total commands accepted by the dispatch loop.
    #[must_use]
    pub fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }
}

/// The RPC plumbing shared by the command loop and the dialog bridge.
pub struct RpcBridge {
    /// Command lines the reader thread forwards — everything that is not
    /// an `extension_ui_response` (`rpc-mode.ts:782-799` routes by type on
    /// one stdin stream). Behind a mutex so the receive loop can block on
    /// `recv()` without holding the bridge lock a worker needs.
    pub commands: Arc<Mutex<Receiver<String>>>,
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
    /// Host-owned session replacement / fork / bash / command table / auth
    /// ([`RpcHost`]). `create_bridge` installs [`LocalRpcHost`]; the
    /// coordinator can swap it before `run_rpc_mode` (see `WIRING.md`).
    pub host: Box<dyn RpcHost + Send>,
    /// Delivery mirror for the `queue_update` re-emission
    /// (`agent-session.ts:1181-1198`). Enqueues record text here; the event
    /// sink removes it at `message_start`.
    pub queued_texts: Arc<Mutex<QueuedTexts>>,
    /// In-flight command mirror — upstream's `activeCommands`
    /// (`rpc-mode.ts:90,801,807`). Also read by tests that assert overlapping
    /// commands can be accepted while an earlier command is still running.
    pub dispatch_stats: Arc<RpcDispatchStats>,
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
        commands: Arc::new(Mutex::new(commands_rx)),
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
        queued_texts: Arc::new(Mutex::new(QueuedTexts::default())),
        dispatch_stats: Arc::new(RpcDispatchStats::default()),
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

/// The most command tasks the dispatcher leaves in flight at once. Upstream
/// has no such cap (`void handleInputLine` spawns one promise per line), but
/// each port task owns a thread, so the bound keeps a hostile stdin from
/// exhausting them while still allowing commands to overlap.
const MAX_IN_FLIGHT: usize = 8;

/// Strict FIFO turnstile over the shared runtime. Upstream's
/// `void handleInputLine(line)` (`:833`) lets commands overlap on its event
/// loop; the port's agent run is synchronous over `&mut PrintRuntime`, so
/// execution is serialized here while the receive loop keeps reading. Holding
/// `next` across the whole command makes the serialization input-ordered, so
/// the response/event stream stays in command order as the protocol tests
/// require. A panicking command still advances the gate (RAII), so later
/// commands cannot deadlock behind it.
struct DispatchGate {
    next: Mutex<usize>,
    cv: Condvar,
}

impl DispatchGate {
    fn new() -> Self {
        Self {
            next: Mutex::new(0),
            cv: Condvar::new(),
        }
    }

    fn run_ordered<T>(&self, seq: usize, f: impl FnOnce() -> T) -> T {
        struct Advance<'a>(&'a DispatchGate);
        impl Drop for Advance<'_> {
            fn drop(&mut self) {
                let mut next = self.0.next.lock().expect("rpc dispatch gate");
                *next += 1;
                self.0.cv.notify_all();
            }
        }

        let mut next = self.next.lock().expect("rpc dispatch gate");
        while *next < seq {
            next = self.cv.wait(next).expect("rpc dispatch gate");
        }
        let advance = Advance(self);
        drop(next);
        let result = f();
        drop(advance);
        result
    }
}

/// Retires a command from the in-flight mirror even if its worker unwinds.
struct DispatchLeave<'a>(&'a RpcDispatchStats);

impl Drop for DispatchLeave<'_> {
    fn drop(&mut self) {
        self.0.leave();
    }
}

fn wait_for_capacity(stats: &RpcDispatchStats) {
    while stats.active() >= MAX_IN_FLIGHT {
        std::thread::yield_now();
    }
}

/// Serves commands until stdin closes (`onInputEnd`, `rpc-mode.ts:826-839`).
/// Each line is dispatched on its own scoped thread (`void handleInputLine`,
/// `:832-834`) behind a bounded, FIFO gate; the receive loop never waits for
/// a command to finish. The extension UI stays bound for the whole run:
/// dialogs opened by any command's extension events cross the stream, and
/// [`rebind_session`] re-binds it after a session replacement.
pub fn run_rpc_mode(
    runtime: &mut PrintRuntime,
    bridge: &mut RpcBridge,
    block_images: bool,
) -> Result<i32, String> {
    let gate = DispatchGate::new();
    let runtime = Arc::new(Mutex::new(&mut *runtime));
    let bridge = Arc::new(Mutex::new(&mut *bridge));
    // Seeded from the startup setting; `dynamic_block_images_converter`
    // re-reads it per convertToLlm call (sdk.ts:296-331).
    let block_flag = Arc::new(AtomicBool::new(block_images));
    let first_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let (stats, commands) = {
        let guard = bridge.lock().expect("rpc bridge");
        (
            Arc::clone(&guard.dispatch_stats),
            Arc::clone(&guard.commands),
        )
    };

    let result: Result<(), String> = std::thread::scope(|scope| {
        let mut seq = 0usize;
        loop {
            let line = commands.lock().expect("rpc commands").recv();
            let Ok(line) = line else { break };
            wait_for_capacity(&stats);
            stats.enter();
            let my_seq = seq;
            seq += 1;
            let runtime = Arc::clone(&runtime);
            let bridge = Arc::clone(&bridge);
            let flag = Arc::clone(&block_flag);
            let first_error = Arc::clone(&first_error);
            let stats = Arc::clone(&stats);
            let gate = &gate;
            scope.spawn(move || {
                let _leave = DispatchLeave(&stats);
                gate.run_ordered(my_seq, || {
                    let mut rt = runtime.lock().expect("rpc runtime");
                    let mut br = bridge.lock().expect("rpc bridge");
                    if let Err(error) = dispatch_line(&mut rt, &mut br, &line, &flag) {
                        let mut slot = first_error.lock().expect("rpc first error");
                        if slot.is_none() {
                            *slot = Some(error);
                        }
                    }
                });
            });
        }
        Ok(())
    });

    // All workers have joined at this point: unbind the dialog surface exactly
    // once (`setUIContext(undefined)` at shutdown).
    {
        let rt = runtime.lock().expect("rpc runtime");
        clear_extension_ui(&rt);
    }
    if let Some(error) = first_error.lock().expect("rpc first error").take() {
        return Err(error);
    }
    result?;
    Ok(0)
}

/// One input line end-to-end (`handleInputLine`, `rpc-mode.ts:768-824`): parse
/// the envelope and run one command, emitting its response. A parse failure
/// echoes no `id` (`:772-782`); an unknown command echoes its raw `type` into
/// both the `command` field and the message (`:734-737`). After every command
/// the host MCP pump advances one tick (`pi-ext-js/src/runner.rs:927`).
fn dispatch_line(
    runtime: &mut PrintRuntime,
    bridge: &mut RpcBridge,
    line: &str,
    block_flag: &Arc<AtomicBool>,
) -> Result<(), String> {
    let (incoming, id, type_name) = match parse_request(line) {
        Ok(parsed) => parsed,
        Err(message) => {
            write_line(
                &bridge.out,
                &RpcResponse::failure(None, Some("parse"), message),
            )?;
            tick_mcp(runtime);
            return Ok(());
        }
    };
    let incoming = match incoming {
        Incoming::Command(command) => command,
        Incoming::Unknown(raw_type) => {
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
            tick_mcp(runtime);
            return Ok(());
        }
    };
    match handle_command(runtime, bridge, incoming, id.as_deref(), block_flag) {
        Ok(CommandOutcome::Success(data)) => write_line(
            &bridge.out,
            &RpcResponse::success(id.as_deref(), type_name.as_deref().unwrap_or(""), data),
        ),
        Ok(CommandOutcome::Replied) => Ok(()),
        Err(error) => write_line(
            &bridge.out,
            &RpcResponse::failure(id.as_deref(), type_name.as_deref(), error),
        ),
    }?;
    tick_mcp(runtime);
    Ok(())
}

/// One host-driven MCP tick (`pi-ext-js/src/runner.rs:927`,
/// `extensions/mod.rs:216`): advance the bounded `session_start` wait,
/// register the tools of settled connections, and drain `list_changed`
/// re-registrations. The RPC loop has no timer, so it pumps once per command.
fn tick_mcp(runtime: &PrintRuntime) {
    #[cfg(feature = "ext-js")]
    if let Some(runner) = &runtime.extension_runner
        && let Ok(mut runner) = runner.lock()
    {
        let _ = runner.mcp_tick(Instant::now());
    }
    #[cfg(not(feature = "ext-js"))]
    let _ = runtime;
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

/// Rebinds the RPC surface after a live session replacement — upstream's
/// `rebindSession()` (`rpc-mode.ts:317-371`), installed via
/// `runtimeHost.setRebindSession` and called after `new_session`,
/// `switch_session`, `fork`, and `clone` (`:456-463,624-650`). Upstream
/// reassigns its local `session`, re-binds `bindExtensions` with a fresh UI
/// context (`mode: "rpc"`), and re-subscribes to session/agent events. The
/// port mutates `runtime.session` in place and emits events per command, so
/// the observable pieces are the UI rebind, the fresh-agent queue reset, and
/// an MCP tick so connections opened by the new `session_start` settle.
fn rebind_session(runtime: &mut PrintRuntime, bridge: &mut RpcBridge) {
    #[cfg(feature = "ext-js")]
    if let Some(runner) = &runtime.extension_runner {
        runner
            .lock()
            .expect("extension runner mutex poisoned")
            .set_ui_context(Arc::clone(&bridge.sink), "rpc");
    }
    // A replaced session starts with an empty `Agent`, hence empty queues
    // (`agent-session-runtime.ts:317-326`). The delivery mirror resets with it.
    bridge.queues = Queues::new();
    *bridge.queued_texts.lock().expect("rpc queued texts") = QueuedTexts::default();
    tick_mcp(runtime);
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
    block_flag: &Arc<AtomicBool>,
) -> Result<CommandOutcome, String> {
    let out = Arc::clone(&bridge.out);
    match command {
        RpcCommand::Prompt {
            message, images, ..
        } => {
            // Upstream starts prompt handling immediately and emits the
            // authoritative response only after preflight succeeds
            // (`rpc-mode.ts:409-435`): command resolution and the model/auth
            // gate. A missing `message` throws on `text.startsWith` before
            // preflight, like the TS.
            let Some(message) = message else {
                return Err(
                    "Cannot read properties of undefined (reading 'startsWith')".to_string()
                );
            };
            let preflight = prompt_preflight(runtime, bridge.host.as_ref(), &message, &images)?;
            write_line(
                &out,
                &RpcResponse::success(
                    id,
                    "prompt",
                    Some(serde_json::json!({ "disposition": preflight.disposition.as_str() })),
                ),
            )?;
            if preflight.disposition != PromptDisposition::Started {
                // `"handled"` (an extension command or input handler) and
                // `"queued"` (a host override during a run) both return
                // before `_startRun` (`:2175-2176,2214-2215,2238-2239`).
                return Ok(CommandOutcome::Replied);
            }
            let now = now_millis();
            let prompt = if images.is_empty() {
                pi_agent::transcript::user_message(&preflight.text, now)
            } else {
                pi_agent::transcript::user_message_with_images(&preflight.text, &images, now)
            };
            // Post-preflight failures are swallowed upstream (the `.catch`
            // checks `preflightSucceeded`, `rpc-mode.ts:425-429`): the run's
            // failure already surfaced as its `agent_end`.
            if run_agent_turn(
                runtime,
                vec![prompt],
                &mut bridge.queues,
                Arc::clone(&out),
                Arc::clone(&bridge.queued_texts),
                Arc::clone(block_flag),
            )
            .is_err()
            {
                // Swallowed, like the TS.
            }
            Ok(CommandOutcome::Replied)
        }
        RpcCommand::Steer { message, images } => {
            // `session.steer` queues even while idle (`_queueUserInput`,
            // `agent-session.ts:2341-2366`); the next run drains steering
            // first. Input handlers can consume it (`"handled"`).
            let Some(message) = message else {
                return Err(
                    "Cannot read properties of undefined (reading 'startsWith')".to_string()
                );
            };
            let Some(text) =
                prepare_queued_input(runtime, bridge.host.as_ref(), &message, &images)?
            else {
                return Ok(CommandOutcome::Success(Some(
                    serde_json::json!({ "disposition": "handled" }),
                )));
            };
            let now = now_millis();
            let queued = pi_agent::transcript::user_message_with_images(&text, &images, now);
            bridge
                .queued_texts
                .lock()
                .expect("rpc queued texts")
                .steering
                .push(text);
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
            let Some(text) =
                prepare_queued_input(runtime, bridge.host.as_ref(), &message, &images)?
            else {
                return Ok(CommandOutcome::Success(Some(
                    serde_json::json!({ "disposition": "handled" }),
                )));
            };
            let now = now_millis();
            let queued = pi_agent::transcript::user_message_with_images(&text, &images, now);
            bridge
                .queued_texts
                .lock()
                .expect("rpc queued texts")
                .follow_up
                .push(text);
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
            *bridge.queued_texts.lock().expect("rpc queued texts") = QueuedTexts::default();
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
            // `session.model` is `undefined` when nothing is selected, so
            // the key is dropped (`rpc-mode.ts:471`, `rpc-types.ts:97`).
            if bridge.host.has_model(runtime)? {
                state.insert(
                    "model".to_string(),
                    serde_json::to_value(&runtime.model).unwrap_or_default(),
                );
            }
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
            let previous = runtime.model.clone();
            let model_data = serde_json::to_value(&resolved).unwrap_or_default();
            runtime.model = resolved.clone();
            // `session.setModel` appends the transcript record
            // (`agent-session.ts:2652`). The per-model thinking-level switch
            // stays deferred; the auth check is re-run per provider by the
            // prompt preflight (`prompt_preflight`, `:2247-2260`).
            runtime
                .session
                .append_model_change(&resolved.provider, &resolved.id)
                .map_err(|error| error.to_string())?;
            // `_emitModelSelect(model, previous, "set")` (`:2663`): a no-op
            // selection is skipped by `modelsAreEqual` (`:2629`).
            if !models_are_equal(&previous, &resolved) {
                let previous_data = serde_json::to_value(&previous).unwrap_or_default();
                let event = model_select_event(&model_data, Some(&previous_data), "set");
                bridge.host.emit_model_select(runtime, &event)?;
            }
            Ok(CommandOutcome::Success(Some(model_data)))
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
                queued_texts: Arc::clone(&bridge.queued_texts),
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
                rebind_session(runtime, bridge);
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
                (_, 0) | (Some(_), 1) => None,
                (None, _) => Some(0),
                (Some(index), len) => Some((index + 1) % len),
            };
            let Some(next) = next_index.and_then(|index| models.get(index)) else {
                return Ok(CommandOutcome::Success(Some(Value::Null)));
            };
            let next = (*next).clone();
            let previous = runtime.model.clone();
            runtime.model = next.clone();
            runtime
                .session
                .append_model_change(&next.provider, &next.id)
                .map_err(|error| error.to_string())?;
            // Clone before the mutable borrow: upstream switches to the
            // settings level, which the port does not carry in `PrintRuntime`.
            let current_level = runtime.thinking_level.as_str().to_string();
            apply_thinking_level(runtime, &current_level, &out)?;
            // `_emitModelSelect(next, current, "cycle")` (`:2756`) fires after
            // the thinking-level switch but before the response.
            if !models_are_equal(&previous, &next) {
                let previous_data = serde_json::to_value(&previous).unwrap_or_default();
                let next_data = serde_json::to_value(&next).unwrap_or_default();
                let event = model_select_event(&next_data, Some(&previous_data), "cycle");
                bridge.host.emit_model_select(runtime, &event)?;
            }
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
            // `get_session_stats` (`rpc-mode.ts:614-617`): upstream omits
            // `contextUsage` when no model supplies a context window.
            let has_model = bridge.host.has_model(runtime)?;
            Ok(CommandOutcome::Success(Some(session_stats_json(
                runtime, has_model,
            ))))
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
                rebind_session(runtime, bridge);
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
                rebind_session(runtime, bridge);
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
                rebind_session(runtime, bridge);
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

/// The shared `convertToLlm` seam (`sdk.ts:296-331`) with the `blockImages`
/// value re-read on every call. Upstream checks `settingsManager.getBlockImages()`
/// inside the wrapper, not once when the run is built, so a change between two
/// provider requests is honored by the next one. The port's
/// [`crate::images::block_images_converter`] captures a `bool`; this adapter
/// wraps it over a shared atomic so the value is loaded per invocation. The
/// converter is synchronous (the sync `Fn` impl of `ConvertToLlm`), so polling
/// its already-ready future needs no nested executor.
pub(crate) fn dynamic_block_images_converter(
    flag: Arc<AtomicBool>,
) -> Arc<dyn pi_agent::ConvertToLlm> {
    Arc::new(move |messages: Vec<AgentMessage>| {
        let enabled = flag.load(Ordering::Relaxed);
        crate::images::block_images_converter(enabled)
            .call(messages)
            .now_or_never()
            .unwrap_or_default()
    })
}

/// One full agent run, events streamed to `output` as they happen.
#[allow(clippy::too_many_lines)] // mirrors the TS run loop; split when extending
fn run_agent_turn(
    runtime: &mut PrintRuntime,
    prompts: Vec<AgentMessage>,
    queues: &mut Queues,
    output: RpcOut,
    queued_texts: Arc<Mutex<QueuedTexts>>,
    block_flag: Arc<AtomicBool>,
) -> Result<(), String> {
    let executor = runtime.executor.clone();
    let specs: Vec<ToolSpec> = executor.tools();

    let context = AgentContext {
        messages: session_messages(runtime),
        tools: specs.clone(),
    };

    let user_prompts = prompts;
    let mut sink = RpcEventSink {
        writer: output,
        queued_texts,
    };
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
        // Same `blockImages` convertToLlm seam as print/JSON (sdk.ts:296-331),
        // but re-read per call instead of capturing the startup value.
        convert_to_llm: dynamic_block_images_converter(block_flag),
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
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
            Self::start_with_block_images(runtime, bridge, false)
        }

        fn start_with_block_images(
            runtime: PrintRuntime,
            bridge: RpcBridge,
            block_images: bool,
        ) -> Self {
            Self(
                std::thread::Builder::new()
                    .name("rpc-mode".to_string())
                    .spawn(move || {
                        let mut runtime = runtime;
                        let mut bridge = bridge;
                        run_rpc_mode(&mut runtime, &mut bridge, block_images)
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

    /// Blocks its first provider call until the test opens the gate, so a
    /// prompt worker stays in flight while another command is dispatched.
    struct GatedStream {
        gate: Arc<(Mutex<bool>, Condvar)>,
        entered: Arc<AtomicBool>,
        first: AtomicBool,
        inner: TextScript,
    }

    impl pi_agent::types::StreamFn for GatedStream {
        fn stream(
            &self,
            request: pi_agent::types::StreamRequest,
        ) -> futures::stream::BoxStream<'static, pi_ai::types::AssistantMessageEvent> {
            if self.first.swap(false, Ordering::SeqCst) {
                self.entered.store(true, Ordering::SeqCst);
                let (lock, cv) = &*self.gate;
                let mut open = lock.lock().expect("gate");
                while !*open {
                    open = cv.wait(open).expect("gate");
                }
            }
            self.inner.stream(request)
        }
    }

    /// Captures the normalized messages the agent loop sends to the provider
    /// so a test can assert the `blockImages` convertToLlm filter ran.
    #[derive(Default)]
    struct CapturingStream {
        seen: Arc<Mutex<Vec<Vec<AgentMessage>>>>,
    }

    impl pi_agent::types::StreamFn for CapturingStream {
        fn stream(
            &self,
            request: pi_agent::types::StreamRequest,
        ) -> futures::stream::BoxStream<'static, pi_ai::types::AssistantMessageEvent> {
            self.seen
                .lock()
                .expect("capture")
                .push(request.messages.clone());
            let partial = serde_json::from_value::<pi_ai::types::AssistantMessage>(json!({
                "role": "assistant",
                "content": [{ "type": "text", "text": "ok" }],
                "api": "test",
                "provider": "test",
                "model": "test-model",
                "usage": {
                    "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                    "totalTokens": 0,
                    "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0,
                              "cacheWrite": 0.0, "total": 0.0 }
                },
                "stopReason": "stop",
                "timestamp": 0
            }))
            .expect("assistant message");
            Box::pin(futures::stream::iter(vec![
                pi_ai::types::AssistantMessageEvent::Start {
                    partial: partial.clone(),
                },
                pi_ai::types::AssistantMessageEvent::TextStart {
                    content_index: 0,
                    partial: partial.clone(),
                },
                pi_ai::types::AssistantMessageEvent::TextEnd {
                    content_index: 0,
                    content: "ok".to_string(),
                    partial: partial.clone(),
                },
                pi_ai::types::AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message: partial,
                },
            ]))
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
            parse_request(r#"{"type":"not_a_command","id":"x"}"#).expect("parse");
        assert_eq!(
            incoming,
            Incoming::Unknown(Some("not_a_command".to_string()))
        );
        assert_eq!(type_name.as_deref(), Some("not_a_command"));
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
        let first = bridge
            .commands
            .lock()
            .expect("commands")
            .recv()
            .expect("command");
        assert!(first.contains(r#""type":"get_state""#));
        let second = bridge
            .commands
            .lock()
            .expect("commands")
            .recv()
            .expect("command");
        assert!(second.contains(r#""id":2"#));
        drop(fixture.writer);
        assert!(
            bridge.commands.lock().expect("commands").recv().is_err(),
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

        fn check_auth(
            &self,
            _runtime: &PrintRuntime,
            _provider: &str,
        ) -> Result<RpcAuthCheck, String> {
            Ok(RpcAuthCheck {
                configured: true,
                using_oauth: false,
            })
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
                Incoming::Command(_command) => {
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

    /// A host whose command table and auth facts are scripted; the unrelated
    /// operations delegate to the local host.
    struct PreflightHost {
        commands: Vec<Value>,
        auth: RpcAuthCheck,
    }

    impl PreflightHost {
        fn configured(commands: Vec<Value>) -> Self {
            Self {
                commands,
                auth: RpcAuthCheck {
                    configured: true,
                    using_oauth: false,
                },
            }
        }
    }

    impl RpcHost for PreflightHost {
        fn new_session(
            &self,
            runtime: &mut PrintRuntime,
            parent: Option<&str>,
        ) -> Result<bool, String> {
            LocalRpcHost.new_session(runtime, parent)
        }

        fn switch_session(&self, runtime: &mut PrintRuntime, path: &str) -> Result<bool, String> {
            LocalRpcHost.switch_session(runtime, path)
        }

        fn fork(
            &self,
            runtime: &mut PrintRuntime,
            entry_id: &str,
            position: RpcForkPosition,
        ) -> Result<RpcForkResult, String> {
            LocalRpcHost.fork(runtime, entry_id, position)
        }

        fn bash(
            &self,
            runtime: &mut PrintRuntime,
            command: &str,
            exclude: bool,
            id: Option<&str>,
        ) -> Result<Value, String> {
            LocalRpcHost.bash(runtime, command, exclude, id)
        }

        fn get_commands(&self, _runtime: &PrintRuntime) -> Result<Vec<Value>, String> {
            Ok(self.commands.clone())
        }

        fn check_auth(
            &self,
            _runtime: &PrintRuntime,
            _provider: &str,
        ) -> Result<RpcAuthCheck, String> {
            Ok(self.auth)
        }
    }

    /// Waits for the run tail in tests that must observe the event stream.
    fn wait_for_settle(fixture: &BridgeFixture) -> Vec<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let lines = fixture.output_lines();
            if lines.iter().any(|line| line.contains("agent_settled")) {
                return lines;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "run did not settle: {lines:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// A provider error after acceptance: it surfaces in the event stream,
    /// never as a second `prompt` response (`rpc-mode.ts:425-429`).
    struct FailingStream;

    impl pi_agent::types::StreamFn for FailingStream {
        fn stream(
            &self,
            _request: pi_agent::types::StreamRequest,
        ) -> futures::stream::BoxStream<'static, pi_ai::types::AssistantMessageEvent> {
            use pi_ai::types::AssistantMessageEvent;
            let partial = serde_json::from_value::<pi_ai::types::AssistantMessage>(json!({
                "role": "assistant",
                "content": [{ "type": "text", "text": "provider failed" }],
                "api": "test",
                "provider": "test",
                "model": "test-model",
                "usage": {
                    "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                    "totalTokens": 0,
                    "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0,
                              "cacheWrite": 0.0, "total": 0.0 }
                },
                "stopReason": "error",
                "errorMessage": "provider failed",
                "timestamp": 0
            }))
            .expect("assistant message");
            Box::pin(futures::stream::iter(vec![
                AssistantMessageEvent::Start {
                    partial: partial.clone(),
                },
                AssistantMessageEvent::Done {
                    reason: pi_ai::types::StopReason::Error,
                    message: partial,
                },
            ]))
        }
    }

    #[test]
    fn prompt_preflight_handles_a_known_extension_command_without_a_run() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        // A run would panic in `NeverStream`, so this also proves no run starts.
        let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
        fixture.set_host(Box::new(PreflightHost::configured(vec![json!({
            "name": "deploy",
            "description": "Deploy",
            "source": "extension",
        })])));
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        fixture.write_line(r#"{"type":"prompt","id":"p1","message":"/deploy now"}"#);
        let lines = fixture.wait_for_output(1);
        drop(fixture.writer);
        mode.join();

        assert_eq!(lines.len(), 1, "no run for a handled command: {lines:?}");
        let value: Value = serde_json::from_str(&lines[0]).expect("frame");
        assert_eq!(value["type"], "response");
        assert_eq!(value["id"], "p1");
        assert_eq!(value["command"], "prompt");
        assert_eq!(value["success"], true);
        assert_eq!(value["data"]["disposition"], "handled");
    }

    #[test]
    fn prompt_preflight_starts_template_skill_and_unknown_slash_commands() {
        for (name, source) in [
            ("review", "prompt"),
            ("skill:review", "skill"),
            ("missing", "unknown"),
            ("deploy", "extension"),
        ] {
            let mut fixture = BridgeFixture::new();
            let dir = tempfile::tempdir().expect("tempdir");
            let runtime = test_runtime(
                dir.path(),
                Arc::new(TextScript {
                    calls: AtomicUsize::new(0),
                }),
            );
            // The registry holds "deploy"; only an exact extension-command
            // match is handled. The other slash forms fall through to a run.
            fixture.set_host(Box::new(PreflightHost::configured(vec![json!({
                "name": "deploy",
                "description": "Deploy",
                "source": "extension",
            })])));
            let bridge = fixture.take_bridge();
            let mode = ModeThread::start(runtime, bridge);
            let command = format!("/{name} file.rs");

            let expected = if source == "extension" {
                "handled"
            } else {
                "started"
            };
            fixture.write_line(&format!(
                r#"{{"type":"prompt","id":"p1","message":"{command}"}}"#
            ));
            // A handled command starts no run, so there is no `agent_settled`
            // to wait for in that arm.
            let lines = if expected == "handled" {
                fixture.wait_for_output(1)
            } else {
                wait_for_settle(&fixture)
            };
            drop(fixture.writer);
            mode.join();

            let parsed: Vec<Value> = lines
                .iter()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            let response = parsed
                .iter()
                .find(|value| value["command"] == "prompt")
                .unwrap_or_else(|| panic!("prompt response for {command}: {parsed:?}"));
            assert_eq!(
                response["data"]["disposition"], expected,
                "{command} / {source}"
            );
        }
    }

    #[test]
    fn prompt_preflight_refuses_an_unconfigured_provider_before_acceptance() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
        fixture.set_host(Box::new(PreflightHost {
            commands: Vec::new(),
            auth: RpcAuthCheck {
                configured: false,
                using_oauth: false,
            },
        }));
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        fixture.write_line(r#"{"type":"prompt","id":"u1","message":"hello"}"#);
        let lines = fixture.wait_for_output(1);
        drop(fixture.writer);
        mode.join();

        assert_eq!(lines.len(), 1, "only the refusal crossed: {lines:?}");
        let value: Value = serde_json::from_str(&lines[0]).expect("frame");
        assert_eq!(value["id"], "u1");
        assert_eq!(value["command"], "prompt");
        assert_eq!(value["success"], false);
        assert_eq!(value["error"], format_no_api_key_message("test"));
        assert!(
            value.get("data").is_none(),
            "a refusal carries no disposition: {value}"
        );
    }

    #[test]
    fn prompt_preflight_uses_the_oauth_refusal_for_a_missing_oauth_credential() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
        fixture.set_host(Box::new(PreflightHost {
            commands: Vec::new(),
            auth: RpcAuthCheck {
                configured: false,
                using_oauth: true,
            },
        }));
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        fixture.write_line(r#"{"type":"prompt","id":"o1","message":"hello"}"#);
        let lines = fixture.wait_for_output(1);
        drop(fixture.writer);
        mode.join();

        let value: Value = serde_json::from_str(&lines[0]).expect("frame");
        assert_eq!(value["success"], false);
        assert_eq!(
            value["error"],
            "Authentication failed for \"test\". Credentials may have expired or network is unavailable. Run '/login test' to re-authenticate."
        );
        assert!(value.get("data").is_none());
    }

    #[test]
    fn prompt_response_precedes_the_run_and_a_failed_run_adds_no_second_response() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let mut runtime = test_runtime(dir.path(), Arc::new(FailingStream));
        runtime.retry_settings.enabled = false;
        fixture.set_host(Box::new(PreflightHost::configured(Vec::new())));
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        fixture.write_line(r#"{"type":"prompt","id":"f1","message":"will fail"}"#);
        let lines = wait_for_settle(&fixture);
        drop(fixture.writer);
        mode.join();

        let parsed: Vec<Value> = lines
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let response_index = parsed
            .iter()
            .position(|value| value["command"] == "prompt")
            .expect("prompt response");
        let agent_start = parsed
            .iter()
            .position(|value| value["type"] == "agent_start")
            .expect("agent_start");
        assert_eq!(response_index, 0, "preflight response is first: {parsed:?}");
        assert!(
            response_index < agent_start,
            "the response precedes the run"
        );
        assert_eq!(parsed[response_index]["data"]["disposition"], "started");
        assert_eq!(
            parsed
                .iter()
                .filter(|value| value["command"] == "prompt")
                .count(),
            1,
            "the post-preflight failure is swallowed, not a second response"
        );
        assert!(
            parsed.iter().any(|value| value["type"] == "agent_end"),
            "the provider failure surfaced as an event"
        );
    }

    #[test]
    fn prompt_reemits_queue_update_when_a_queued_message_is_delivered() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = test_runtime(
            dir.path(),
            Arc::new(TextScript {
                calls: AtomicUsize::new(0),
            }),
        );
        fixture.set_host(Box::new(PreflightHost::configured(Vec::new())));
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        fixture.write_line(r#"{"type":"steer","id":"s1","message":"queued steer"}"#);
        let lines = fixture.wait_for_output(2);
        assert_eq!(
            lines[0],
            r#"{"type":"queue_update","steering":["queued steer"],"followUp":[]}"#
        );
        fixture.write_line(r#"{"type":"prompt","id":"p1","message":"go"}"#);
        let lines = wait_for_settle(&fixture);
        drop(fixture.writer);
        mode.join();

        let parsed: Vec<Value> = lines
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let update_indices: Vec<usize> = parsed
            .iter()
            .enumerate()
            .filter(|(_, value)| value["type"] == "queue_update")
            .map(|(index, _)| index)
            .collect();
        assert_eq!(
            update_indices.len(),
            2,
            "one at enqueue, one at delivery: {parsed:?}"
        );
        assert_eq!(parsed[update_indices[1]]["steering"], json!([]));
        assert_eq!(parsed[update_indices[1]]["followUp"], json!([]));
        let delivery = parsed
            .iter()
            .position(|value| {
                value["type"] == "message_start"
                    && value["message"]["role"] == "user"
                    && value["message"]["content"][0]["text"] == "queued steer"
            })
            .expect("the queued message is delivered inside the run");
        assert!(
            update_indices[1] < delivery,
            "the reduced queue crosses before its message_start"
        );
    }

    /// Callback calls recorded by [`StubHost`], shared with the test thread.
    #[derive(Default)]
    struct CallbackLog {
        model_selects: Vec<Value>,
        errors: Vec<Value>,
        commands: Vec<(String, String)>,
        expansions: Vec<String>,
        inputs: Vec<String>,
    }

    /// A host that records every new callback and lets each test choose what
    /// it returns. Everything else delegates to [`LocalRpcHost`].
    struct StubHost {
        commands: Vec<Value>,
        outcome: RpcExtensionCommandOutcome,
        input: RpcInputDisposition,
        expansion: Option<String>,
        has_model: bool,
        handler_error: Option<String>,
        log: Arc<Mutex<CallbackLog>>,
    }

    impl StubHost {
        fn new(log: &Arc<Mutex<CallbackLog>>) -> Self {
            Self {
                commands: Vec::new(),
                outcome: RpcExtensionCommandOutcome::NotFound,
                input: RpcInputDisposition::Continue(None),
                expansion: None,
                has_model: true,
                handler_error: None,
                log: Arc::clone(log),
            }
        }
    }

    impl RpcHost for StubHost {
        fn new_session(
            &self,
            runtime: &mut PrintRuntime,
            parent: Option<&str>,
        ) -> Result<bool, String> {
            LocalRpcHost.new_session(runtime, parent)
        }

        fn switch_session(&self, runtime: &mut PrintRuntime, path: &str) -> Result<bool, String> {
            LocalRpcHost.switch_session(runtime, path)
        }

        fn fork(
            &self,
            runtime: &mut PrintRuntime,
            entry_id: &str,
            position: RpcForkPosition,
        ) -> Result<RpcForkResult, String> {
            LocalRpcHost.fork(runtime, entry_id, position)
        }

        fn bash(
            &self,
            runtime: &mut PrintRuntime,
            command: &str,
            exclude: bool,
            id: Option<&str>,
        ) -> Result<Value, String> {
            LocalRpcHost.bash(runtime, command, exclude, id)
        }

        fn get_commands(&self, _runtime: &PrintRuntime) -> Result<Vec<Value>, String> {
            Ok(self.commands.clone())
        }

        fn check_auth(
            &self,
            _runtime: &PrintRuntime,
            _provider: &str,
        ) -> Result<RpcAuthCheck, String> {
            Ok(RpcAuthCheck {
                configured: true,
                using_oauth: false,
            })
        }

        fn execute_extension_command(
            &self,
            _runtime: &mut PrintRuntime,
            name: &str,
            args: &str,
        ) -> Result<RpcExtensionCommandOutcome, String> {
            self.log
                .lock()
                .expect("callback log")
                .commands
                .push((name.to_string(), args.to_string()));
            if let Some(error) = &self.handler_error {
                return Err(error.clone());
            }
            Ok(self.outcome)
        }

        fn run_input_handlers(
            &self,
            _runtime: &mut PrintRuntime,
            text: &str,
            _images: &[pi_ai::types::ImageContent],
            source: &str,
        ) -> Result<RpcInputDisposition, String> {
            assert_eq!(source, "rpc", "RPC passes source: rpc (rpc-mode.ts:419)");
            self.log
                .lock()
                .expect("callback log")
                .inputs
                .push(text.to_string());
            Ok(self.input.clone())
        }

        fn expand_prompt(
            &self,
            _runtime: &PrintRuntime,
            text: &str,
        ) -> Result<Option<String>, String> {
            self.log
                .lock()
                .expect("callback log")
                .expansions
                .push(text.to_string());
            Ok(self.expansion.clone())
        }

        fn has_model(&self, _runtime: &PrintRuntime) -> Result<bool, String> {
            Ok(self.has_model)
        }

        fn emit_model_select(&self, _runtime: &PrintRuntime, event: &Value) -> Result<(), String> {
            self.log
                .lock()
                .expect("callback log")
                .model_selects
                .push(event.clone());
            Ok(())
        }

        fn emit_extension_error(
            &self,
            _runtime: &PrintRuntime,
            event: &Value,
        ) -> Result<(), String> {
            self.log
                .lock()
                .expect("callback log")
                .errors
                .push(event.clone());
            Ok(())
        }
    }

    #[test]
    fn model_select_event_shape_drops_undefined_previous() {
        let model = json!({"id": "m", "provider": "p", "api": "a"});
        let set = model_select_event(&model, None, "set");
        assert_eq!(
            set,
            json!({"type": "model_select", "model": model.clone(), "source": "set"})
        );
        let previous = json!({"id": "old"});
        let cycle = model_select_event(&model, Some(&previous), "cycle");
        assert_eq!(
            cycle,
            json!({
                "type": "model_select",
                "model": model,
                "previousModel": previous,
                "source": "cycle",
            })
        );
        // `_emitModelSelect` skips a no-op selection (`modelsAreEqual`).
        let mut a = pi_ai::types::Model {
            id: "m".to_string(),
            provider: "p".to_string(),
            api: "a".to_string(),
            ..pi_ai::types::Model::default()
        };
        let b = a.clone();
        assert!(models_are_equal(&a, &b));
        a.id = "n".to_string();
        assert!(!models_are_equal(&a, &b));
    }

    #[test]
    fn model_select_callback_fires_on_set_and_cycle_model() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
        let log = Arc::new(Mutex::new(CallbackLog::default()));
        fixture.set_host(Box::new(StubHost::new(&log)));
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        let models = crate::models_state::all();
        let target = models
            .iter()
            .find(|model| model.provider != "test")
            .expect("an embedded catalog model");
        fixture.write_line(&format!(
            r#"{{"type":"set_model","id":"sm","provider":"{}","modelId":"{}"}}"#,
            target.provider, target.id
        ));
        fixture.write_line(r#"{"type":"cycle_model","id":"cm"}"#);
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
                "model responses: {lines:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        drop(fixture.writer);
        mode.join();

        let parsed: Vec<Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("line"))
            .collect();
        let cycle = parsed
            .iter()
            .find(|value| value["command"] == "cycle_model")
            .expect("cycle response");
        let events = log.lock().expect("callback log").model_selects.clone();
        assert_eq!(events.len(), if cycle["data"].is_null() { 1 } else { 2 });
        assert_eq!(events[0]["type"], "model_select");
        assert_eq!(events[0]["source"], "set");
        assert_eq!(events[0]["model"]["id"].as_str(), Some(target.id.as_str()));
        assert_eq!(
            events[0]["previousModel"]["provider"].as_str(),
            Some("test")
        );
        if events.len() == 2 {
            assert_eq!(events[1]["source"], "cycle");
            assert_eq!(
                events[1]["previousModel"]["id"].as_str(),
                Some(target.id.as_str())
            );
        }
    }

    #[test]
    fn no_model_branch_refuses_prompt_and_drops_state_model() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        // A run would panic in `NeverStream`, so this also proves none starts.
        let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
        let log = Arc::new(Mutex::new(CallbackLog::default()));
        let mut host = StubHost::new(&log);
        host.has_model = false;
        fixture.set_host(Box::new(host));
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        fixture.write_line(r#"{"type":"prompt","id":"p1","message":"hello"}"#);
        fixture.write_line(r#"{"type":"get_state","id":"s1"}"#);
        let lines = fixture.wait_for_output(2);
        drop(fixture.writer);
        mode.join();

        let parsed: Vec<Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("line"))
            .collect();
        assert_eq!(parsed[0]["command"], "prompt");
        assert_eq!(parsed[0]["success"], false);
        assert_eq!(parsed[0]["error"], format_no_model_message());
        assert!(parsed[0].get("data").is_none());
        assert_eq!(parsed[1]["command"], "get_state");
        assert!(
            parsed[1]["data"].get("model").is_none(),
            "an absent model key is dropped like JS undefined: {}",
            parsed[1]
        );
    }

    #[test]
    fn expansion_callback_rewrites_prompt_and_queued_text() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = test_runtime(
            dir.path(),
            Arc::new(TextScript {
                calls: AtomicUsize::new(0),
            }),
        );
        let log = Arc::new(Mutex::new(CallbackLog::default()));
        let mut host = StubHost::new(&log);
        host.expansion = Some("expanded text".to_string());
        fixture.set_host(Box::new(host));
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        fixture.write_line(r#"{"type":"steer","id":"s1","message":"/template args"}"#);
        fixture.write_line(r#"{"type":"follow_up","id":"f1","message":"/template later"}"#);
        let lines = fixture.wait_for_output(4);
        assert!(
            lines[0].contains(r#""type":"queue_update""#),
            "the first frame is the enqueue update: {}",
            lines[0]
        );
        fixture.write_line(r#"{"type":"prompt","id":"p1","message":"/template args"}"#);
        let lines = wait_for_settle(&fixture);
        drop(fixture.writer);
        mode.join();

        let values: Vec<Value> = lines
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        assert!(values.iter().any(|value| value["type"] == "queue_update"
            && value["steering"] == json!(["expanded text"])));
        assert!(values.iter().any(|value| value["type"] == "queue_update"
            && value["followUp"] == json!(["expanded text"])));
        assert!(values.iter().any(|value| value["type"] == "message_start"
            && value["message"]["role"] == "user"
            && value["message"]["content"][0]["text"] == "expanded text"));
        assert_eq!(
            log.lock().expect("callback log").expansions,
            vec!["/template args", "/template later", "/template args"]
        );
    }

    #[test]
    fn input_handlers_can_consume_or_rewrite_submissions() {
        // Consumed prompt/steer/follow-up: three `"handled"` dispositions, no
        // run and no queue update.
        let log = Arc::new(Mutex::new(CallbackLog::default()));
        {
            let mut fixture = BridgeFixture::new();
            let dir = tempfile::tempdir().expect("tempdir");
            let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
            let mut host = StubHost::new(&log);
            host.input = RpcInputDisposition::Handled;
            fixture.set_host(Box::new(host));
            let bridge = fixture.take_bridge();
            let mode = ModeThread::start(runtime, bridge);
            fixture.write_line(r#"{"type":"prompt","id":"p1","message":"hello"}"#);
            fixture.write_line(r#"{"type":"steer","id":"s1","message":"hello"}"#);
            fixture.write_line(r#"{"type":"follow_up","id":"f1","message":"hello"}"#);
            let lines = fixture.wait_for_output(3);
            drop(fixture.writer);
            mode.join();
            let parsed: Vec<Value> = lines
                .iter()
                .map(|line| serde_json::from_str(line).expect("line"))
                .collect();
            for value in &parsed {
                assert_eq!(value["data"]["disposition"], "handled", "{value}");
            }
            assert_eq!(
                log.lock().expect("log").inputs,
                vec!["hello", "hello", "hello"]
            );
        }

        // A rewritten prompt reaches the run with the replacement text.
        {
            let mut fixture = BridgeFixture::new();
            let dir = tempfile::tempdir().expect("tempdir");
            let runtime = test_runtime(
                dir.path(),
                Arc::new(TextScript {
                    calls: AtomicUsize::new(0),
                }),
            );
            let mut host = StubHost::new(&log);
            host.input = RpcInputDisposition::Continue(Some("rewritten".to_string()));
            fixture.set_host(Box::new(host));
            let bridge = fixture.take_bridge();
            let mode = ModeThread::start(runtime, bridge);
            fixture.write_line(r#"{"type":"prompt","id":"p2","message":"original"}"#);
            let lines = wait_for_settle(&fixture);
            drop(fixture.writer);
            mode.join();
            let values: Vec<Value> = lines
                .iter()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            // No images: `agent.prompt(text)` emits `content` as a plain string,
            // not a content-block array (`UserMessage.content: string | ...[]`).
            assert!(values.iter().any(|value| value["type"] == "message_start"
                && value["message"]["role"] == "user"
                && value["message"]["content"] == json!("rewritten")));
        }
    }

    #[test]
    fn extension_command_handler_outcome_guards_execution_and_errors() {
        let log = Arc::new(Mutex::new(CallbackLog::default()));

        // A host-reported handler consumes the prompt even with no registry
        // entry: execution belongs to the host, not the default.
        {
            let mut fixture = BridgeFixture::new();
            let dir = tempfile::tempdir().expect("tempdir");
            let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
            let mut host = StubHost::new(&log);
            host.outcome = RpcExtensionCommandOutcome::Handled;
            fixture.set_host(Box::new(host));
            let bridge = fixture.take_bridge();
            let mode = ModeThread::start(runtime, bridge);
            fixture.write_line(r#"{"type":"prompt","id":"p1","message":"/deploy now"}"#);
            let lines = fixture.wait_for_output(1);
            drop(fixture.writer);
            mode.join();
            let value: Value = serde_json::from_str(&lines[0]).expect("frame");
            assert_eq!(value["data"]["disposition"], "handled");
            assert_eq!(
                log.lock().expect("log").commands,
                vec![("deploy".to_string(), "now".to_string())]
            );
        }

        // A throwing handler still consumes and reports through `onError`.
        {
            let mut fixture = BridgeFixture::new();
            let dir = tempfile::tempdir().expect("tempdir");
            let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
            let mut host = StubHost::new(&log);
            host.outcome = RpcExtensionCommandOutcome::Handled;
            host.handler_error = Some("boom".to_string());
            fixture.set_host(Box::new(host));
            let bridge = fixture.take_bridge();
            let mode = ModeThread::start(runtime, bridge);
            fixture.write_line(r#"{"type":"prompt","id":"p2","message":"/deploy now"}"#);
            let lines = fixture.wait_for_output(1);
            drop(fixture.writer);
            mode.join();
            let value: Value = serde_json::from_str(&lines[0]).expect("frame");
            assert_eq!(value["data"]["disposition"], "handled");
            // The host owns the `onError` sink (`rpc-mode.ts:355-357`); the
            // shape it receives is the verbatim extension_error frame.
            let errors = log.lock().expect("log").errors.clone();
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0]["type"], "extension_error");
            assert_eq!(errors[0]["extensionPath"], "command:deploy");
            assert_eq!(errors[0]["event"], "command");
            assert_eq!(errors[0]["error"], "boom");
        }

        // NotFound falls through to a run even when the name is registered:
        // it is the host's call whether a handler actually ran.
        {
            let mut fixture = BridgeFixture::new();
            let dir = tempfile::tempdir().expect("tempdir");
            let runtime = test_runtime(
                dir.path(),
                Arc::new(TextScript {
                    calls: AtomicUsize::new(0),
                }),
            );
            let mut host = StubHost::new(&log);
            host.commands = vec![json!({"name": "deploy", "source": "extension"})];
            fixture.set_host(Box::new(host));
            let bridge = fixture.take_bridge();
            let mode = ModeThread::start(runtime, bridge);
            fixture.write_line(r#"{"type":"prompt","id":"p3","message":"/deploy now"}"#);
            let lines = wait_for_settle(&fixture);
            drop(fixture.writer);
            mode.join();
            let values: Vec<Value> = lines
                .iter()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            let response = values
                .iter()
                .find(|value| value["command"] == "prompt")
                .expect("prompt response");
            assert_eq!(response["data"]["disposition"], "started");
            assert!(values.iter().any(|value| value["type"] == "agent_settled"));
        }
    }

    #[test]
    fn queued_texts_mirror_removes_one_delivery_at_a_time() {
        let mut mirror = QueuedTexts::default();
        mirror.steering.push("one".to_string());
        mirror.steering.push("two".to_string());
        mirror.follow_up.push("later".to_string());
        assert!(mirror.take_delivered("two"));
        assert_eq!(mirror.steering, vec!["one"]);
        assert!(mirror.take_delivered("later"));
        assert!(
            !mirror.take_delivered("later"),
            "a second delivery has no matching queue text"
        );
        let AgentEvent::QueueUpdate {
            steering,
            follow_up,
        } = mirror.as_event()
        else {
            panic!("the mirror is a queue_update");
        };
        assert_eq!(steering, vec!["one"]);
        assert_eq!(follow_up, Vec::<String>::new());
    }

    #[test]
    fn steer_rejects_extension_commands_verbatim() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = test_runtime(dir.path(), Arc::new(NeverStream));
        let log = Arc::new(Mutex::new(CallbackLog::default()));
        let mut host = StubHost::new(&log);
        host.commands = vec![json!({"name": "deploy", "source": "extension"})];
        fixture.set_host(Box::new(host));
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);
        fixture.write_line(r#"{"type":"steer","id":"s1","message":"/deploy now"}"#);
        fixture.write_line(r#"{"type":"follow_up","id":"f1","message":"/deploy now"}"#);
        let lines = fixture.wait_for_output(2);
        drop(fixture.writer);
        mode.join();
        let parsed: Vec<Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("line"))
            .collect();
        for value in &parsed {
            assert_eq!(value["success"], false);
            assert_eq!(
                value["error"],
                "Extension command \"/deploy\" cannot be queued. Use prompt() or execute the command when not streaming."
            );
        }
    }

    /// The RPC prompt path installs the same `blockImages` seam as print/JSON:
    /// assert on the normalized messages the provider actually receives, for
    /// both a blocking and a non-blocking settings snapshot.
    #[test]
    fn rpc_prompt_honors_block_images_convert_to_llm_snapshot() {
        fn prompt_user_blocks(messages: &[AgentMessage]) -> Vec<pi_ai::types::InputContent> {
            messages
                .iter()
                .find_map(|message| match message {
                    AgentMessage::User(user) => match &user.content {
                        pi_ai::types::UserContent::Blocks(blocks) => Some(blocks.clone()),
                        pi_ai::types::UserContent::Text(_) => None,
                    },
                    _ => None,
                })
                .expect("prompt user message with blocks")
        }

        let run = |block_images: bool| {
            let mut fixture = BridgeFixture::new();
            let dir = tempfile::tempdir().expect("tempdir");
            let seen = Arc::new(Mutex::new(Vec::new()));
            let stream = Arc::new(CapturingStream {
                seen: Arc::clone(&seen),
            });
            let runtime = test_runtime(dir.path(), stream);
            let bridge = fixture.take_bridge();
            let mode = ModeThread::start_with_block_images(runtime, bridge, block_images);
            fixture.write_line(
                r#"{"type":"prompt","id":"p1","message":"look","images":[{"data":"abc","mimeType":"image/png"}]}"#,
            );
            let _ = wait_for_settle(&fixture);
            drop(fixture.writer);
            mode.join();
            let calls = seen.lock().expect("capture").clone();
            assert_eq!(calls.len(), 1, "one provider call");
            prompt_user_blocks(&calls[0])
        };

        let blocked = run(true);
        assert!(
            blocked
                .iter()
                .any(|block| block.as_text() == Some(crate::images::IMAGE_READING_DISABLED)),
            "image replaced by the placeholder: {blocked:?}"
        );
        assert!(
            blocked
                .iter()
                .all(|block| !matches!(block, pi_ai::types::InputContent::Image(_))),
            "no image survives: {blocked:?}"
        );

        let passed = run(false);
        assert!(
            passed
                .iter()
                .any(|block| matches!(block, pi_ai::types::InputContent::Image(_))),
            "image preserved when blocking is off: {passed:?}"
        );
    }

    /// Upstream fires `void handleInputLine(line)` per line (`:833`), so the
    /// reader never waits for an in-flight prompt. The port's dispatch mirrors
    /// that with one task per command behind a FIFO gate: a second command is
    /// accepted and counted while the prompt's provider call is still blocked,
    /// and the wire stays in input order when the gate opens.
    #[test]
    fn rpc_mode_accepts_overlapping_commands_in_fifo_order() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let entered = Arc::new(AtomicBool::new(false));
        let stream = GatedStream {
            gate: Arc::clone(&gate),
            entered: Arc::clone(&entered),
            first: AtomicBool::new(true),
            inner: TextScript {
                calls: AtomicUsize::new(0),
            },
        };
        let runtime = test_runtime(dir.path(), Arc::new(stream));
        let stats = Arc::clone(&fixture.bridge().dispatch_stats);
        let bridge = fixture.take_bridge();
        let mode = ModeThread::start(runtime, bridge);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        fixture.write_line(r#"{"type":"prompt","id":"p1","message":"first"}"#);
        while !entered.load(Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < deadline,
                "prompt never reached the provider"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // The prompt worker is still in its (blocked) provider call. A second
        // line is read and dispatched anyway; both commands are now in flight.
        fixture.write_line(r#"{"type":"get_state","id":"s1"}"#);
        while stats.active() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "second command never became in flight: {}",
                stats.active()
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            stats.max_active() >= 2,
            "the dispatcher must overlap commands: {}",
            stats.max_active()
        );
        assert!(
            !fixture
                .output_lines()
                .iter()
                .any(|line| line.contains(r#""id":"s1""#)),
            "the FIFO gate holds the second response until the prompt retires"
        );

        {
            let (lock, cv) = &*gate;
            *lock.lock().expect("gate") = true;
            cv.notify_all();
        }
        let lines = loop {
            let lines = fixture.output_lines();
            if lines.iter().any(|line| line.contains(r#""id":"s1""#)) {
                break lines;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "state response never arrived: {lines:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        drop(fixture.writer);
        mode.join();

        let prompt = lines
            .iter()
            .position(|line| line.contains(r#""id":"p1""#))
            .expect("prompt response");
        let seen = lines
            .iter()
            .position(|line| line.contains(r#""id":"s1""#))
            .expect("state response");
        assert!(prompt < seen, "responses follow input order: {lines:?}");
        assert_eq!(stats.started(), 2);
        assert_eq!(stats.active(), 0, "both commands retired");
    }

    /// `sdk.ts:296-331` checks `getBlockImages()` inside `convertToLlm`, so the
    /// flag is read on every call, not captured when the run is built. Flipping
    /// the shared atomic between conversions must change the result.
    #[test]
    fn dynamic_block_images_converter_rereads_the_flag_per_call() {
        use pi_ai::types::{ImageContent, InputContent, TextContent, UserContent, UserMessage};
        let image = InputContent::Image(ImageContent {
            data: "abc".to_string(),
            mime_type: "image/png".to_string(),
        });
        let text = InputContent::Text(TextContent {
            text: "hello".to_string(),
            text_signature: None,
        });
        let message = AgentMessage::User(UserMessage {
            content: UserContent::Blocks(vec![image, text]),
            timestamp: 1,
        });
        let flag = Arc::new(AtomicBool::new(false));
        let converter = dynamic_block_images_converter(Arc::clone(&flag));
        let convert = || crate::runtime::block_on(converter.call(vec![message.clone()]));

        let has_image = |messages: &[AgentMessage]| {
            messages.iter().any(|message| match message {
                AgentMessage::User(user) => match &user.content {
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .any(|block| matches!(block, InputContent::Image(_))),
                    UserContent::Text(_) => false,
                },
                _ => false,
            })
        };
        assert!(has_image(&convert()), "flag off: the image survives");
        flag.store(true, Ordering::SeqCst);
        let blocked = convert();
        assert!(!has_image(&blocked), "flag on: the image is stripped");
        let AgentMessage::User(user) = &blocked[0] else {
            panic!("user message stays a user message");
        };
        let UserContent::Blocks(blocks) = &user.content else {
            panic!("blocks stay blocks");
        };
        assert_eq!(
            blocks[0].as_text(),
            Some(crate::images::IMAGE_READING_DISABLED)
        );
        flag.store(false, Ordering::SeqCst);
        assert!(
            has_image(&convert()),
            "flag off again: a fresh read takes effect"
        );
    }

    /// The port's `rebindSession()` clears the queues of the replaced session's
    /// `Agent` and rebinds the dialog surface. Called directly so the reset is
    /// asserted without depending on a hosted session-manager fixture.
    #[test]
    fn rebind_session_resets_per_session_state() {
        let mut fixture = BridgeFixture::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let mut runtime = test_runtime(dir.path(), Arc::new(NeverStream));
        {
            let bridge = fixture.bridge.as_mut().expect("bridge still in fixture");
            bridge
                .queues
                .steer([pi_agent::transcript::user_message("queued", 1)]);
            bridge
                .queued_texts
                .lock()
                .expect("queued texts")
                .steering
                .push("queued".to_string());
            rebind_session(&mut runtime, bridge);
            assert!(!bridge.queues.has_items(), "steering queue reset");
            let mirror = bridge.queued_texts.lock().expect("queued texts");
            assert_eq!(mirror.steering.len(), 0);
            assert_eq!(mirror.follow_up.len(), 0);
        }
    }
}
