//! The width-adaptive horizontal rule (`DynamicBorder`).
//!
//! Ground truth: `packages/coding-agent/src/modes/interactive/components/dynamic-border.ts`
//! (all 25 lines). Upstream's component is deliberately tiny: one line of
//! box-drawing `─`, as wide as the viewport, colored by a callback that
//! defaults to `theme.fg("border", …)`. The port keeps the same shape; the
//! only Rust-specific piece is [`pi_tui::theme::Styler`], the boxed closure
//! type used everywhere for theme color callbacks.

use pi_tui::component::Component;
use pi_tui::theme::{Styler, ThemeColor};

use super::super::theme;

/// A full-width rule that re-measures on every render (`dynamic-border.ts:11-24`).
///
/// The optional color callback mirrors upstream's constructor argument: pass
/// one when the default `border` token is wrong for the surface (warning
/// notifications, accent headers). In extensions loaded through jiti
/// upstream warns that the global `theme` may be missing, which is why the
/// callback is part of the public API; the port's theme is always available
/// but keeps the same escape hatch.
pub struct DynamicBorder {
    color: Styler,
}

impl DynamicBorder {
    /// `constructor(color = (str) => theme.fg("border", str))` (`dynamic-border.ts:14-16`).
    pub fn new() -> Self {
        Self::with_color(theme::theme().fg_styler(ThemeColor::Border))
    }

    /// Explicit color callback, equivalent to `new DynamicBorder(color)`.
    pub fn with_color(color: Styler) -> Self {
        Self { color }
    }

    /// A rule colored with the `warning` token, as used by the update
    /// notifications (`interactive-mode.ts:4835,4847,4858,4867`).
    pub fn warning() -> Self {
        Self::with_color(theme::theme().fg_styler(ThemeColor::Warning))
    }

    /// A rule colored with the `accent` token, as used by the announcement
    /// and session-selector surfaces (`earendil-announcement.ts:31,51`).
    pub fn accent() -> Self {
        Self::with_color(theme::theme().fg_styler(ThemeColor::Accent))
    }
}

impl Default for DynamicBorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for DynamicBorder {
    /// `render(width): ["─".repeat(Math.max(1, width))]` (`dynamic-border.ts:22-24`).
    fn render(&self, width: usize) -> Vec<String> {
        vec![(self.color)(&"─".repeat(width.max(1)))]
    }
}

// `invalidate()` is a no-op upstream (`:18-20`); this port has no render cache
// to invalidate either, so the default `Component` implementation is correct.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn renders_one_full_width_rule() {
        let lines = Component::render(&DynamicBorder::new(), 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(pi_tui::ansi::visible_width(&lines[0]), 80);
        assert!(
            pi_tui::ansi::strip_terminal_sequences(&lines[0])
                .chars()
                .all(|c| c == '─')
        );
    }

    #[test]
    fn width_is_adaptive_and_never_empty() {
        for width in [1, 7, 120] {
            let plain = pi_tui::ansi::strip_terminal_sequences(
                &Component::render(&DynamicBorder::new(), width)[0],
            );
            assert_eq!(plain.chars().count(), width);
        }
        // Upstream clamps to one cell; a zero-width terminal still draws a rule.
        let plain =
            pi_tui::ansi::strip_terminal_sequences(&Component::render(&DynamicBorder::new(), 0)[0]);
        assert_eq!(plain, "─");
    }

    #[test]
    fn color_callback_is_applied() {
        let border = DynamicBorder::with_color(Arc::new(|text| format!("<{text}>")));
        assert_eq!(Component::render(&border, 3), vec!["<───>".to_string()]);
    }

    #[test]
    fn warning_and_accent_rules_match_their_tokens() {
        let warning = Component::render(&DynamicBorder::warning(), 4)[0].clone();
        let accent = Component::render(&DynamicBorder::accent(), 4)[0].clone();
        assert_eq!(pi_tui::ansi::strip_terminal_sequences(&warning), "────");
        assert_eq!(pi_tui::ansi::strip_terminal_sequences(&accent), "────");
        assert_ne!(
            warning, accent,
            "distinct theme tokens produce distinct SGR"
        );
    }
}
