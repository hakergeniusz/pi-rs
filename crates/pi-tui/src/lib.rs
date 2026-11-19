//! pi's terminal UI, ported to Rust.
//!
//! Ground truth: `packages/tui/src/` in pi-mono. Module docs cite the pi
//! file and line range each piece mirrors.
//!
//! The renderer is pi's: render to ANSI lines, diff against the previous
//! frame, and write the minimal byte sequence inside a synchronized-output
//! block. `frame` adds a read-only cell grid so offline tests can assert
//! what the terminal would show.
//!
//! Nothing in this crate owns a thread or a timer. Hosts drive frames with
//! [`tui::Tui::request_render`] / [`tui::Tui::render_now`], spinner frames
//! with [`components::loader::Loader::tick`], wheel acceleration with
//! [`wheel_scroll::WheelScrollAccelerator::next`], and input timeouts with
//! [`stdin_buffer::StdinBuffer::pending_timeout_ms`].
//!
//! Mouse handling lives in [`mouse`] and [`wheel_scroll`]; the terminal query
//! protocol (OSC 10/11/4, CSI ?997, cell size) is driven from
//! [`tui::Tui`] with host-serviced deadlines.

#![forbid(unsafe_code)]
// Test assertions prefer the terse `assert!(x.is_empty())` over the lint-suggested
// `assert_eq!(x, [] as [T; 0])`; keep the pedantic lint off in test builds.
#![cfg_attr(test, allow(clippy::assert_is_empty))]

/// Transcript search for the alt screen: corpus, match finder, overlay.
pub mod alt_screen_search;
/// ANSI-aware string measurement, slicing, and escape stripping.
pub mod ansi;
/// Color values (indexed/rgb/oklch), mixing, 256-color downscaling, styling.
pub mod color;
/// The component contract: the [`Component`] trait and tree mutations.
pub mod component;
/// The built-in component library (editor, loader, markdown, scroll view, …).
pub mod components;
/// The offline frame-capture seam: a read-only cell grid of the last frame.
pub mod frame;
/// Fuzzy matching for autocomplete and selectors (port of `fuzzy.ts`).
pub mod fuzzy;
/// Key action names and pi's default key table.
pub mod keybindings;
/// Key decoding and matching in pi's key-name vocabulary.
pub mod keys;
/// Emacs-style kill/yank ring for the editor.
pub mod kill_ring;
/// The layout engine: a box tree with clips painted into screen lines.
pub mod layout;
/// Mouse primitives: SGR event parsing, [`MouseRegion`], event routing.
pub mod mouse;
/// Oklab and OKHSL <-> sRGB conversion for theme math.
pub mod oklab;
/// Program Status Protocol (OSC 7501): encode, query, and reply detection
/// (port of `program-status.ts`).
pub mod program_status;
/// stdin reassembly: complete sequences from chunked input, bracketed pastes.
pub mod stdin_buffer;
/// The `system` theme: pi colors derived from the terminal's own theme.
pub mod system_theme;
/// The [`Terminal`] trait, a crossterm backend, and an in-memory test backend.
pub mod terminal;
/// Terminal color queries: OSC 10/11/4 replies, CSI ?997 scheme, CSI 16 t cell size.
pub mod terminal_colors;
/// Terminal capabilities, Kitty/iTerm2 inline image encoding, and OSC 8 links.
pub mod terminal_image;
/// Theme tokens, JSON validation/loading, and the theme file watcher.
pub mod theme;
/// The differential renderer, overlays, and focus.
pub mod tui;
/// The alt-screen (fullscreen) renderer with overlays.
pub mod tui_alt_screen;
/// Undo/redo stack for the editor.
pub mod undo_stack;
/// Wheel-event acceleration for scrolling.
pub mod wheel_scroll;
/// Word-wise cursor navigation for the editor.
pub mod word_navigation;
