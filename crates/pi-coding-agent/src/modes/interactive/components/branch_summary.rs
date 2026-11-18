//! `BranchSummaryMessageComponent` — the `[branch]` chat row with
//! collapsed/expanded state.
//!
//! Ground truth: `modes/interactive/components/branch-summary-message.ts`
//! (1-96). Deviations: upstream toggles via a mouse click region; this port
//! toggles through the global ctrl+o expand state like the other expandable
//! rows (the mouse plumbing for chat rows is not wired yet — documented
//! deviation), and the `customMessageBg` box background is not applied (the
//! port renders rows without row backgrounds).

use pi_tui::component::Component;
use pi_tui::components::markdown::{
    DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTransform,
};
use pi_tui::theme::ThemeColor;
use std::sync::Arc;

use super::super::theme;
use super::markdown_transform::MarkdownTransformCtx;

/// The branch summary row (`branch-summary-message.ts:20-96`).
pub struct BranchSummaryMessageComponent {
    expanded: bool,
    output_pad: usize,
    /// Stored so `setOutputPad` can rebuild the markdown child (upstream
    /// resets the child's padding, `branch-summary-message.ts:42-44`).
    summary: String,
    transform_ctx: MarkdownTransformCtx,
    /// The expanded body (header + summary); renders at the real width like
    /// upstream's Markdown child.
    markdown: Markdown,
    /// The fixed prefix lines (label, blank) and the collapsed line; rebuilt
    /// on state changes, like upstream's `updateDisplay`.
    content: Vec<String>,
}

impl BranchSummaryMessageComponent {
    pub fn new(summary: &str, output_pad: usize, transform_ctx: MarkdownTransformCtx) -> Self {
        let mut component = Self {
            expanded: false,
            output_pad,
            summary: summary.to_string(),
            transform_ctx,
            markdown: Self::build_markdown(summary, output_pad, None),
            content: Vec::new(),
        };
        component.refresh_markdown();
        component.update_display();
        component
    }

    /// `setExpanded`.
    pub fn set_expanded(&mut self, expanded: bool) {
        if self.expanded != expanded {
            self.expanded = expanded;
            self.update_display();
        }
    }

    /// `setOutputPad` (`branch-summary-message.ts:42-44`).
    pub fn set_output_pad(&mut self, output_pad: usize) {
        if self.output_pad != output_pad {
            self.output_pad = output_pad;
            self.refresh_markdown();
            self.update_display();
        }
    }

    /// Rebuilds the expanded Markdown child from the current context.
    fn refresh_markdown(&mut self) {
        let transform = self
            .transform_ctx
            .make("summary", None, None, None)
            .map(|inner| {
                let header = "**Branch Summary**\n\n";
                Arc::new(move |markdown: &str, width: usize| -> String {
                    format!("{header}{}", inner(markdown, width))
                }) as MarkdownTransform
            });
        self.markdown = Self::build_markdown(&self.summary, self.output_pad, transform);
    }

    /// The expanded Markdown child: Pi's header, then the summary
    /// (`branch-summary-message.ts:55-70`). The transformers see only the
    /// summary content, not Pi's header; without a transformer the port
    /// keeps the combined header+summary text — the identical render.
    fn build_markdown(
        summary: &str,
        output_pad: usize,
        transform: Option<MarkdownTransform>,
    ) -> Markdown {
        let theme = theme::theme();
        let header = "**Branch Summary**\n\n";
        let style = DefaultTextStyle {
            color: Some(theme.fg_styler(ThemeColor::CustomMessageText)),
            ..DefaultTextStyle::default()
        };
        Markdown::new(
            if transform.is_some() {
                summary.to_string()
            } else {
                format!("{header}{summary}")
            },
            output_pad,
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

    fn update_display(&mut self) {
        let theme = theme::theme();
        let pad = " ".repeat(self.output_pad);
        // `theme.fg("customMessageLabel", bold("[branch]"))`.
        let label = format!(
            "{pad}{}",
            theme.fg(ThemeColor::CustomMessageLabel, "\x1b[1m[branch]\x1b[22m")
        );
        let mut content = vec![label, pad.clone()];

        if !self.expanded {
            // `Branch summary (` + keyText + ` to expand)`
            // (`branch-summary-message.ts:71-80`).
            let key = theme::key_text(theme::app_keys::TOOLS_EXPAND);
            content.push(format!(
                "{pad}{}{}{}",
                theme.fg(ThemeColor::CustomMessageText, "Branch summary ("),
                theme.fg(ThemeColor::Dim, &key),
                theme.fg(ThemeColor::CustomMessageText, " to expand)"),
            ));
        }

        self.content = content;
    }
}

impl Component for BranchSummaryMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = self.content.clone();
        if self.expanded {
            lines.extend(self.markdown.render(width));
        }
        lines
            .iter()
            .map(|line| pi_tui::ansi::truncate_to_width(line, width, "", false))
            .collect()
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
        assert!(plain[0].contains("[branch]"), "{plain:?}");
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
        let joined = plain_lines(&component, 80).join("\n");
        assert!(joined.contains("Branch Summary"), "{joined}");
        assert!(joined.contains("Goal"), "{joined}");
        assert!(joined.contains("finish it"), "{joined}");
    }

    #[test]
    fn expanding_and_collapsing_toggles_the_body() {
        let mut component =
            BranchSummaryMessageComponent::new("body", 1, MarkdownTransformCtx::default());
        component.set_expanded(true);
        assert!(plain_lines(&component, 80).len() > 3);
        component.set_expanded(false);
        assert_eq!(plain_lines(&component, 80).len(), 3);
    }
}
