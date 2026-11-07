//! Terminal UI framework with differential rendering.
//!
//! Rust port of `@earendil-works/pi-tui` (TS source: `packages/tui/src`).
//!
//! Key TS surfaces to port (P3):
//! - `terminal.ts` / `stdin-buffer.ts`: raw mode, synchronized output
//!   (CSI 2026), bracketed paste, mouse events
//! - `tui.ts` / `tui-main-screen.ts` / `tui-alt-screen.ts`: shared `TUI`
//!   interface, line-diff rendering (main screen) and viewport diff
//!   (alternate screen with app-owned scrolling)
//! - `components/`: `Text`, `TruncatedText`, `Input`, `Editor`, `Markdown`,
//!   `Loader`, `SelectList`, `SettingsList`, `MouseRegion`, `Spacer`, `Image`,
//!   `Box`, `Container`, `VStack`, `HStack`, `ScrollView`
//! - `autocomplete.ts`, `keybindings.ts`, `keys.ts` (48 KB of key matching),
//!   `colors.ts`/`oklab.ts`, `terminal-image.ts` (kitty/iTerm2 graphics)
//!
//! Implementation note: hand-rolled renderer on `crossterm`; `ratatui`'s
//! model does not match pi-tui's component/line-diff semantics.

/// Component render contract placeholder (real trait lands in P3).
pub trait Component {
    /// Render to lines for the given width.
    fn render(&self, width: u16) -> Vec<String>;
}
