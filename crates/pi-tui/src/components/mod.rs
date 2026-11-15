//! Base components.
//!
//! Ground truth: `packages/tui/src/components/`. Every component keeps pi's
//! layout maths; pi's render caches are dropped because this port's
//! differential renderer compares strings anyway, which is what the caches
//! fed.

/// `AltScreenFlashContainer`: transient inverse-video messages over the alt screen.
pub mod alt_screen_flash;
/// `BoxComponent`: pi's `Box`, a padded container with an optional background.
pub mod box_component;
/// `Editor`: pi's multi-line text editor (state model, keys, autocomplete).
pub mod editor;
/// `Image`: an inline image, or the textual fallback.
pub mod image;
/// `Input`: single-line input with horizontal scrolling (search overlay, settings submenu).
pub mod input;
/// `Input`: pi's one-line `> ` prompt for extension UIs.
pub mod line_input;
/// `Loader`: a spinner line driven by the host clock.
pub mod loader;
/// `Markdown`: pi's assistant-message renderer with a hand-written parser.
pub mod markdown;
/// `MouseRegion`: mouse handling added around an unchanged child.
pub mod mouse_region;
/// `ScrollView`: a fixed viewport over one taller child.
pub mod scroll_view;
/// `SelectList`: a keyboard-navigable list with a scrolling window.
pub mod select_list;
/// `SettingsList`: label/value rows with a description pane.
pub mod settings_list;
/// `Spacer`: a component that renders empty lines.
pub mod spacer;
/// `Stack` / `VStack` / `HStack`: entry sizing for the layout engine.
pub mod stack;
/// `Text`: wrapped, padded text.
pub mod text;
/// `TruncatedText`: one line of text, truncated to fit.
pub mod truncated_text;
