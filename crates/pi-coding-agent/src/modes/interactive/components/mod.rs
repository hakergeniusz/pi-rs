//! The interactive mode's components.
//!
//! Ground truth: `modes/interactive/components/` in pi-mono. Hosts compose
//! these into the TUI tree; the orchestrator lives in
//! [`super::interactive_mode`].

pub mod armin;
pub mod assistant_message;
pub mod auth_selector;
pub mod branch_summary;
pub mod compaction_summary;
#[cfg(feature = "ext-js")]
pub mod component_proxy;
#[cfg(feature = "ext-js")]
pub mod custom_entry;
#[cfg(feature = "ext-js")]
pub mod custom_message;
pub mod dynamic_border;
pub mod easter_egg_3d;
#[cfg(feature = "ext-js")]
pub mod extension_custom;
pub mod extension_editor;
pub mod extension_input;
pub mod extension_selector;
pub mod footer;
pub mod header;
pub mod markdown_transform;
pub mod mcp_selector;
pub mod model_selector;
pub mod scoped_models_selector;
pub mod session_selector;
pub mod session_selector_search;
pub mod settings_selector;
pub mod settings_submenu;
pub mod status_indicator;
pub mod tool_execution;
pub mod tree_selector;
pub mod trust_selector;
pub mod user_message;
pub mod user_message_selector;
