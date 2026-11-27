//! `BranchSummaryMessageComponent` - the `[branch]` chat row with
//! collapsed/expanded state.
//!
//! Ground truth: `modes/interactive/components/branch-summary-message.ts:20-96`.
//! Upstream extends `Box` with `customMessageBg`, `paddingX = outputPad` and
//! `paddingY = 1`, then wraps a label, a spacer and either the summary
//! Markdown or the collapsed hint in a `MouseRegion`. The port reuses
//! [`BoxComponent`], [`pi_tui::components::spacer::Spacer`], [`Markdown`] and
//! [`MouseRegion`]; a left click toggles this row, and the host's global
//! ctrl+o still reaches `setExpanded`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pi_tui::component::{Component, Container};
use pi_tui::components::box_component::BoxComponent;
use pi_tui::components::markdown::{
    DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTransform,
};
use pi_tui::components::mouse_region::MouseRegion;
use pi_tui::components::spacer::Spacer;
use pi_tui::components::text::Text;
use pi_tui::mouse::{MouseButton, MouseEvent, MouseEventKind, MouseEventResult};
use pi_tui::theme::ThemeColor;

use super::super::theme;
use super::markdown_transform::MarkdownTransformCtx;

/// The branch summary row (`branch-summary-message.ts:20-96`).
pub struct BranchSummaryMessageComponent {
    expanded: bool,
    output_pad: usize,
    /// Stored so `setOutputPad` and a state change can rebuild the box
    /// (`branch-summary-message.ts:42-44`).
    summary: String,
    transform_ctx: MarkdownTransformCtx,
    /// Upstream's `Box`: `outputPad` horizontal padding, one row of vertical
    /// padding, and the `customMessageBg` background on every row.
    content_box: BoxComponent,
    /// The region callback cannot borrow the component, so `handle_mouse`
    /// seeds this with `expanded`, then reconciles after dispatch.
    click_toggle: Arc<AtomicBool>,
}

impl BranchSummaryMessageComponent {
    pub fn new(summary: &str, output_pad: usize, transform_ctx: MarkdownTransformCtx) -> Self {
        let mut component = Self {
            expanded: false,
            output_pad,
            summary: summary.to_string(),
            transform_ctx,
            content_box: BoxComponent::new(0, 0),
            click_toggle: Arc::new(AtomicBool::new(false)),
        };
        component.update_display();
        component
    }

    /// `setExpanded` (`branch-summary-message.ts:37-40`).
    pub fn set_expanded(&mut self, expanded: bool) {
        if self.expanded != expanded {
            self.expanded = expanded;
            self.update_display();
        }
    }

    /// `setOutputPad` (`branch-summary-message.ts:42-44`): upstream calls
    /// `Box.setPaddingX`; the port rebuilds the box with the new padding.
    pub fn set_output_pad(&mut self, output_pad: usize) {
        if self.output_pad != output_pad {
            self.output_pad = output_pad;
            self.update_display();
        }
    }

    /// The expanded body (`branch-summary-message.ts:55-70`). Upstream's
    /// Markdown sits inside the box, so its own padding is zero; the summary
    /// transformer sees only the summary text, and the header is prepended
    /// in the transform (or directly when there is no transformer).
    fn build_markdown(&self) -> Markdown {
        let theme = theme::theme();
        let transform = self
            .transform_ctx
            .make("summary", None, None, None)
            .map(|inner| {
                let header = "**Branch Summary**\n\n";
                Arc::new(move |markdown: &str, width: usize| -> String {
                    format!("{header}{}", inner(markdown, width))
                }) as MarkdownTransform
            });
        let style = DefaultTextStyle {
            color: Some(theme.fg_styler(ThemeColor::CustomMessageText)),
            ..DefaultTextStyle::default()
        };
        Markdown::new(
            if transform.is_some() {
                self.summary.clone()
            } else {
                format!("**Branch Summary**\n\n{}", self.summary)
            },
            0,
            0,
            theme::markdown_theme(),
            Some(style),
            MarkdownOptions {
                preserve_list_markers: true,
                preserve_backslash_escapes: true,
                transform,
            },
        )
    }

    /// `updateDisplay` (`branch-summary-message.ts:46-96`).
    fn update_display(&mut self) {
        let theme = theme::theme();
        let mut content_box = BoxComponent::with_bg(self.output_pad, 1, |text| {
            theme::theme().bg(pi_tui::theme::ThemeBg::CustomMessageBg, text)
        });
        // The label, spacer and body/body-hint are the MouseRegion's single
        // child, like upstream's `Container`.
        let mut container = Container::new();
        container.add_child(Box::new(Text::new(
            theme.fg(ThemeColor::CustomMessageLabel, "\x1b[1m[branch]\x1b[22m"),
            0,
            0,
        )));
        container.add_child(Box::new(Spacer::new(1)));
        if self.expanded {
            container.add_child(Box::new(self.build_markdown()));
        } else {
            // `Branch summary (` + keyText + ` to expand)`
            // (`branch-summary-message.ts:71-80`).
            let key = theme::key_text(theme::app_keys::TOOLS_EXPAND);
            container.add_child(Box::new(Text::new(
                format!(
                    "{}{}{}",
                    theme.fg(ThemeColor::CustomMessageText, "Branch summary ("),
                    theme.fg(ThemeColor::Dim, &key),
                    theme.fg(ThemeColor::CustomMessageText, " to expand)"),
                ),
                0,
                0,
            )));
        }
        let click_toggle = Arc::clone(&self.click_toggle);
        content_box.add_child(Box::new(MouseRegion::new(
            Box::new(container),
            Box::new(move |event| toggle_on_left_click(event, &click_toggle)),
        )));
        self.content_box = content_box;
    }
}

/// The row's click toggle (`branch-summary-message.ts:87-94`): a left click
/// flips the flag and reports the event handled; everything else passes.
fn toggle_on_left_click(event: &MouseEvent, flag: &AtomicBool) -> Option<MouseEventResult> {
    if event.kind != MouseEventKind::Click || event.button != MouseButton::Left {
        return None;
    }
    flag.store(!flag.load(Ordering::Relaxed), Ordering::Relaxed);
    Some(MouseEventResult::handled())
}

impl Component for BranchSummaryMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.content_box
            .render(width)
            .iter()
            .map(|line| pi_tui::ansi::truncate_to_width(line, width, "", false))
            .collect()
    }

    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        self.click_toggle.store(self.expanded, Ordering::Relaxed);
        let result = self.content_box.handle_mouse(event);
        if self.click_toggle.load(Ordering::Relaxed) != self.expanded {
            self.set_expanded(!self.expanded);
        }
        result
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_tui::ansi::strip_terminal_sequences;

    fn plain_lines(component: &BranchSummaryMessageComponent, width: usize) -> Vec<String> {
        component
            .render(width)
            .iter()
            .map(|line| strip_terminal_sequences(line).trim_end().to_string())
            .collect()
    }

    #[test]
    fn collapsed_row_labels_the_expand_key() {
        let component =
            BranchSummaryMessageComponent::new("summary text", 1, MarkdownTransformCtx::default());
        let plain = plain_lines(&component, 80);
        // Box paddingY=1 puts an empty row above and below the content
        // (`branch-summary-message.ts:46`).
        assert_eq!(plain.len(), 5, "{plain:?}");
        assert_eq!(plain[0], "", "top box padding: {plain:?}");
        assert_eq!(plain[4], "", "bottom box padding: {plain:?}");
        assert!(
            plain[1].starts_with(' ') && plain[1].contains("[branch]"),
            "{plain:?}"
        );
        let joined = plain.join("\n");
        assert!(
            joined.contains("Branch summary (ctrl+o to expand)"),
            "{joined}"
        );
    }

    #[test]
    fn expanded_row_renders_the_summary_markdown() {
        let mut component = BranchSummaryMessageComponent::new(
            "## Goal\nfinish it",
            1,
            MarkdownTransformCtx::default(),
        );
        component.set_expanded(true);
        let lines = plain_lines(&component, 80);
        let joined = lines.join("\n");
        assert!(joined.contains("Branch Summary"), "{joined}");
        assert!(joined.contains("Goal"), "{joined}");
        assert!(joined.contains("finish it"), "{joined}");
        assert_eq!(lines.first().map(String::as_str), Some(""));
        assert_eq!(lines.last().map(String::as_str), Some(""));
    }

    #[test]
    fn expanding_and_collapsing_toggles_the_body() {
        let mut component =
            BranchSummaryMessageComponent::new("body", 1, MarkdownTransformCtx::default());
        assert_eq!(plain_lines(&component, 80).len(), 5);
        component.set_expanded(true);
        assert!(plain_lines(&component, 80).len() > 5);
        component.set_expanded(false);
        assert_eq!(plain_lines(&component, 80).len(), 5);
    }

    #[test]
    fn a_click_toggles_the_row_expansion() {
        let mut component =
            BranchSummaryMessageComponent::new("body", 1, MarkdownTransformCtx::default());
        let lines = plain_lines(&component, 80);
        // The label is the first content row after the box's top padding.
        let click = MouseEvent::new(MouseEventKind::Click, 0, 1, 1, 80, lines.len());
        component.handle_mouse(&click).expect("handled");
        assert!(plain_lines(&component, 80).len() > 5, "a click expands");
        let lines = plain_lines(&component, 80);
        let click = MouseEvent::new(MouseEventKind::Click, 0, 1, 1, 80, lines.len());
        component.handle_mouse(&click).expect("handled");
        assert_eq!(
            plain_lines(&component, 80).len(),
            5,
            "a second click collapses"
        );
        let middle = MouseEvent::new(MouseEventKind::Click, 1, 1, 1, 80, lines.len());
        assert!(component.handle_mouse(&middle).is_none());
    }
}
