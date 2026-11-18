//! Interactive mode (P6): the real chat TUI.
//!
//! Ground truth: `modes/interactive/` in pi-mono — the theme layer
//! (`theme/`), the message/tool components (`components/`), and the
//! orchestrator (`interactive-mode.ts`). The main-screen (regular) renderer
//! is `pi_tui::Tui` itself; fullscreen (alt-screen) mode mounts the chat
//! viewport ([`chat_viewport`]) as the alt screen's explicit layout root.

pub mod autocomplete;
pub mod chat_viewport;
pub mod components;
#[cfg(feature = "ext-js")]
pub mod extension_ui;
pub mod interactive_mode;
pub mod model_catalog_refresh;
pub mod theme;
