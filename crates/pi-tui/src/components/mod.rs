//! Base components.
//!
//! Ground truth: `packages/tui/src/components/`. Every component keeps pi's
//! layout maths; pi's render caches are dropped because this port's
//! differential renderer compares strings anyway, which is what the caches
//! fed.

pub mod alt_screen_flash;
pub mod box_component;
pub mod editor;
pub mod image;
pub mod input;
pub mod line_input;
pub mod loader;
pub mod markdown;
pub mod mouse_region;
pub mod scroll_view;
pub mod select_list;
pub mod settings_list;
pub mod spacer;
pub mod stack;
pub mod text;
pub mod truncated_text;
