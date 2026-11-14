//! The interactive mode's components.
//!
//! Ground truth: `modes/interactive/components/` in pi-mono. Hosts compose
//! these into the TUI tree; the orchestrator lives in
//! [`super::interactive_mode`].

pub mod assistant_message;
pub mod footer;
pub mod header;
pub mod status_indicator;
pub mod tool_execution;
pub mod user_message;
