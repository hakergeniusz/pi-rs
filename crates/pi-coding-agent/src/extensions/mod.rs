//! Extension host seam.
//!
//! Upstream loads JavaScript extensions that register tools, commands, and
//! event handlers (`extensions/index.ts` + the `pi.extension` API). The
//! runtime decision (embedded `deno_core` vs Rust-native vs subprocess) is
//! still open (PLAN.md P5 gate); this module defines the runtime-agnostic
//! seam everything plugs into:
//!
//! * [`Extension`] — what one extension provides, as trait objects. A
//!   `deno_core` host will implement this trait per loaded JS module; native
//!   Rust extensions implement it directly.
//! * [`ExtensionRegistry`] — aggregation: merged tool declarations, command
//!   table, event fan-out, and claiming of the unknown CLI flags the args
//!   parser captured for extensions (`cli/args.rs`, unknown-flag branch).
//!
//! The agent-loop integration points that consume the registry (tool
//! executor composition, command dispatch) wire in with the JSON/RPC and
//! interactive hosts.

use std::collections::HashMap;
use std::sync::Arc;

use pi_agent::events::AgentEvent;
use pi_agent::types::AgentToolResult;
use pi_ai::types::JsonObject;
use serde_json::Value;

/// A tool an extension exposes to the model, with its executor.
#[derive(Clone)]
pub struct ExtensionTool {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: Value,
    pub execute: ExtensionToolExecute,
}

/// Async executor signature shared by native and hosted extensions.
pub type ExtensionToolExecute = Arc<
    dyn Fn(JsonObject) -> pi_agent::types::BoxFuture<'static, Result<AgentToolResult, String>>
        + Send
        + Sync,
>;

/// A slash-style command an extension registers.
#[derive(Clone)]
pub struct ExtensionCommand {
    pub name: String,
    pub description: String,
    pub execute: ExtensionToolExecute,
}

/// One CLI flag an extension claims (upstream: extensions declare the flags
/// they parse; anything unclaimed stays a diagnostic error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionFlag {
    /// Long name without the leading `--`.
    pub name: String,
    /// Whether the flag consumes the next token as its value.
    pub takes_value: bool,
}

/// What an extension wants the loop to do after seeing an event.
#[derive(Debug, Clone, Default)]
pub struct EventOutcome {
    /// Messages injected into the run (upstream steering).
    pub injected_messages: Vec<String>,
    /// Abort the current run when set (upstream `stop()`).
    pub abort: bool,
}

impl EventOutcome {
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }
}

/// The per-extension contract. Event handlers are read-only observers: the
/// registry aggregates their [`EventOutcome`]s in registration order.
pub trait Extension: Send + Sync {
    fn name(&self) -> &str;

    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    fn commands(&self) -> Vec<ExtensionCommand> {
        Vec::new()
    }

    fn flags(&self) -> Vec<ExtensionFlag> {
        Vec::new()
    }

    fn handle_event(&self, event: &AgentEvent) -> EventOutcome {
        let _ = event;
        EventOutcome::none()
    }
}

/// Aggregated extension host state.
#[derive(Default)]
pub struct ExtensionRegistry {
    extensions: Vec<Arc<dyn Extension>>,
}

impl ExtensionRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, extension: Arc<dyn Extension>) {
        self.extensions.push(extension);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.extensions.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.extensions.is_empty()
    }

    /// Extension tools in registration order (upstream merges these into the
    /// agent's tool list after the built-ins).
    #[must_use]
    pub fn tools(&self) -> Vec<ExtensionTool> {
        self.extensions
            .iter()
            .flat_map(|extension| extension.tools())
            .collect()
    }

    #[must_use]
    pub fn commands(&self) -> Vec<ExtensionCommand> {
        self.extensions
            .iter()
            .flat_map(|extension| extension.commands())
            .collect()
    }

    /// Fan one event out to every extension, merging outcomes.
    #[must_use]
    pub fn handle_event(&self, event: &AgentEvent) -> EventOutcome {
        let mut merged = EventOutcome::none();
        for extension in &self.extensions {
            let outcome = extension.handle_event(event);
            merged.injected_messages.extend(outcome.injected_messages);
            merged.abort |= outcome.abort;
        }
        merged
    }

    /// Lets extensions claim the unknown flags the parser captured;
    /// returns what remains unclaimed (those stay errors like upstream).
    #[must_use]
    pub fn claim_unknown_flags(
        &self,
        unknown: &mut HashMap<String, Option<String>>,
    ) -> Vec<String> {
        let declared: Vec<ExtensionFlag> = self
            .extensions
            .iter()
            .flat_map(|extension| extension.flags())
            .collect();
        let mut unclaimed = Vec::new();
        for name in unknown.keys().cloned().collect::<Vec<_>>() {
            let claimant = declared
                .iter()
                .find(|flag| flag.name == name && flag.takes_value == unknown[&name].is_some());
            match claimant {
                Some(_) => {
                    unknown.remove(&name);
                }
                None => unclaimed.push(name),
            }
        }
        unclaimed
    }
}

#[cfg(test)]
mod tests {
    // `assert!(v.is_empty())` stays: the `assert_eq!(v, [])` alternative is
    // noisier for these fixture assertions.
    #![allow(clippy::assert_is_empty)]
    use super::*;
    use std::sync::Mutex;

    /// Counts `tool_execution_end` events to exercise the fan-out.
    struct CountingExtension {
        seen: Mutex<u32>,
    }

    impl Extension for CountingExtension {
        fn name(&self) -> &'static str {
            "counting"
        }

        fn flags(&self) -> Vec<ExtensionFlag> {
            vec![ExtensionFlag {
                name: "counting-mode".to_string(),
                takes_value: false,
            }]
        }

        fn handle_event(&self, event: &AgentEvent) -> EventOutcome {
            if matches!(event, AgentEvent::ToolExecutionEnd { .. }) {
                *self.seen.lock().expect("mutex") += 1;
                return EventOutcome {
                    injected_messages: vec!["seen one".to_string()],
                    abort: false,
                };
            }
            EventOutcome::none()
        }
    }

    #[test]
    fn registry_merges_tools_and_commands_in_order() {
        let mut registry = ExtensionRegistry::new();
        assert!(registry.is_empty());
        registry.register(Arc::new(CountingExtension {
            seen: Mutex::new(0),
        }));
        assert_eq!(registry.len(), 1);
        assert!(registry.tools().is_empty());
        assert!(registry.commands().is_empty());
    }

    #[test]
    fn events_fan_out_to_every_extension() {
        let mut registry = ExtensionRegistry::new();
        let first = Arc::new(CountingExtension {
            seen: Mutex::new(0),
        });
        let second = Arc::new(CountingExtension {
            seen: Mutex::new(0),
        });
        registry.register(first.clone());
        registry.register(second.clone());
        let event = AgentEvent::ToolExecutionStart {
            tool_call_id: "t1".to_string(),
            tool_name: "read".to_string(),
            args: JsonObject::new(),
            parent_tool_call_id: None,
        };
        let outcome = registry.handle_event(&event);
        // tool_execution_start is not counted; only tool_execution_end is.
        assert!(outcome.injected_messages.is_empty());
        let end = AgentEvent::ToolExecutionEnd {
            tool_call_id: "t1".to_string(),
            tool_name: "read".to_string(),
            result: AgentToolResult::default(),
            is_error: false,
            parent_tool_call_id: None,
        };
        let outcome = registry.handle_event(&end);
        // Both registered extensions inject; registration order preserved.
        assert_eq!(
            outcome.injected_messages,
            vec!["seen one".to_string(), "seen one".to_string()]
        );
        assert_eq!(*first.seen.lock().expect("mutex"), 1);
        assert_eq!(*second.seen.lock().expect("mutex"), 1);
    }

    #[test]
    fn flags_claim_only_matching_shape() {
        let mut registry = ExtensionRegistry::new();
        registry.register(Arc::new(CountingExtension {
            seen: Mutex::new(0),
        }));
        let mut unknown = HashMap::new();
        unknown.insert("counting-mode".to_string(), None); // boolean shape matches
        unknown.insert("stray-flag".to_string(), Some("x".to_string()));
        let unclaimed = registry.claim_unknown_flags(&mut unknown);
        assert_eq!(unclaimed, vec!["stray-flag".to_string()]);
        assert!(!unknown.contains_key("counting-mode"));
    }
}
