//! Interactive mode (P6): the real chat TUI.
//!
//! Ground truth: `modes/interactive/` in pi-mono — the theme layer
//! (`theme/`), the message/tool components (`components/`), and the
//! orchestrator (`interactive-mode.ts`). The main-screen (regular) renderer
//! is `pi_tui::Tui` itself; fullscreen (alt-screen) mode is P6 part 2.

pub mod components;
pub mod interactive_mode;
pub mod theme;
