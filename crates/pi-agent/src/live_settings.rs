//! The settings a run reads fresh per request, and the out-of-band handle a
//! caller writes them through.
//!
//! Ground truth: pi-mono `packages/agent/src/agent.ts` (`model`,
//! `thinkingLevel` on `MutableAgentState`, `:68-72`) and
//! `packages/agent/src/agent-loop.ts:186-199`, where the loop's per-turn
//! snapshot replaces `config.model` / `config.reasoning`. pi's session fills
//! that snapshot from the state it reads live
//! (`agent-session.ts:899-900`: `model: this.agent.state.model`,
//! `thinkingLevel: this.agent.state.thinkingLevel`), so a `/thinking` or
//! `/model` typed mid-run (`interactive-mode.ts:5066-5081`, `:5115-5136`) lands
//! on the **next provider request of the same run**.
//!
//! [`QueueHandle`] solves the same reachability problem for the queues: pi's
//! `agent.steer(...)` is a plain method JS lets any caller reach while `prompt`
//! runs, and a Rust `prompt` holds `&mut Agent` for the whole run, so the port
//! needs a `&self` path to the same state. This module is that path for the
//! model and the reasoning level.
//!
//! Two shapes, because the two getters differ:
//!
//! * `thinking_level` is the single truth and lives here.
//!   [`crate::state::AgentState`] holds the handle and delegates, so every
//!   `&mut self` setter in the crate keeps its signature and writes this cell.
//! * the model cannot move wholesale: `AgentState::model()` returns `&Model`
//!   (`types.ts:391`), and a mutex cannot lend out a reference. So a model set
//!   through the handle is recorded as a **pending override** that the loop
//!   applies to its next request and the agent applies to `state.model` when
//!   the run settles. Unobservable difference from pi: a caller that can reach
//!   the agent mid-run (only [`crate::agent::Agent` itself, since pi-cli's
//!   session has set its agent aside for the run) reads the pre-switch model
//!   from `AgentState::model` until the run ends; pi would already show the new
//!   one. The request stream, the transcript, and everything a host renders
//!   from the loop are identical.

use std::sync::{Arc, Mutex, MutexGuard};

use pi_ai::types::Model;

use crate::types::ThinkingLevel;

/// pi's live per-run settings.
#[derive(Debug)]
struct LiveSettings {
    thinking_level: ThinkingLevel,
    /// A model set from outside the agent while a run holds it; the loop takes
    /// it at the next turn, and the agent folds it into `state.model` when the
    /// run settles.
    pending_model: Option<Model>,
}

/// The out-of-band handle to `agent.state.thinkingLevel` / `agent.state.model`.
///
/// `Clone + Send + Sync`, holding nothing but the two cells, so a caller can
/// write while a `prompt` future owns `&mut Agent` - the reachability pi gets
/// for free from `this.session.setThinkingLevel(...)`
/// (`interactive-mode.ts:5085`). Take it with
/// [`Agent::live_settings`](crate::agent::Agent::live_settings) before starting
/// a run and keep it; every `&mut self` setter on the agent writes the same
/// cells.
#[derive(Debug, Clone)]
pub struct SettingsHandle {
    settings: Arc<Mutex<LiveSettings>>,
}

impl SettingsHandle {
    /// pi's `createMutableAgentState` starts `thinkingLevel` at `"off"`
    /// (`agent.ts:87`) and leaves `model` to the caller.
    pub fn new() -> SettingsHandle {
        SettingsHandle {
            settings: Arc::new(Mutex::new(LiveSettings {
                thinking_level: ThinkingLevel::Off,
                pending_model: None,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, LiveSettings> {
        self.settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `Agent.state.thinkingLevel` (`types.ts:393`): the level the next request
    /// will ask for, read live by the loop.
    pub fn thinking_level(&self) -> ThinkingLevel {
        self.lock().thinking_level
    }

    /// `session.setThinkingLevel(level)` (`agent-session.ts:2565`), reachable
    /// from outside the agent while a run is in flight.
    pub fn set_thinking_level(&self, level: ThinkingLevel) {
        self.lock().thinking_level = level;
    }

    /// Record a model for the running turn. pi writes `agent.state.model`
    /// directly (`agent-session.ts:2437`); the port cannot, so this is the
    /// same write deferred to the next turn boundary - see the module docs.
    pub fn set_model(&self, model: Model) {
        self.lock().pending_model = Some(model);
    }

    /// The model set from outside the agent while a run held it, if any. Read, not
    /// taken: the agent folds it into `state.model` when the run settles, so
    /// every turn of the run keeps reading the same override.
    pub fn pending_model(&self) -> Option<Model> {
        self.lock().pending_model.clone()
    }

    /// Drop a pending model without applying it: the agent wrote the model
    /// itself, through `&mut Agent`, so there is nothing left to defer.
    pub fn clear_pending_model(&self) {
        self.lock().pending_model = None;
    }

    /// Take the pending model, leaving no override behind: the agent calls this
    /// when a run settles, so `state.model` catches up with what the loop
    /// already streamed.
    pub fn take_pending_model(&self) -> Option<Model> {
        self.lock().pending_model.take()
    }
}

impl Default for SettingsHandle {
    fn default() -> SettingsHandle {
        SettingsHandle::new()
    }
}
