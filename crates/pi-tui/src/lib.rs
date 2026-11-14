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

pub mod alt_screen_search;
pub mod ansi;
pub mod color;
pub mod component;
pub mod components;
pub mod frame;
pub mod fuzzy;
pub mod keybindings;
pub mod keys;
pub mod kill_ring;
pub mod layout;
pub mod mouse;
pub mod oklab;
pub mod stdin_buffer;
pub mod system_theme;
pub mod terminal;
pub mod terminal_colors;
pub mod terminal_image;
pub mod theme;
pub mod tui;
pub mod tui_alt_screen;
pub mod undo_stack;
pub mod wheel_scroll;
pub mod word_navigation;
