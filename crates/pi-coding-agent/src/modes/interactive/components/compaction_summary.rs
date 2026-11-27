//! `CompactionSummaryMessageComponent` — the `[compaction]` chat row with
//! collapsed/expanded state.
//!
//! Ground truth: `modes/interactive/components/compaction-summary-message.ts`.
//! Upstream toggles via a mouse click region; this port toggles through the
//! global ctrl+o expand state like the other expandable rows (the mouse
//! plumbing for chat rows is not wired yet — documented deviation), and the
//! `customMessageBg` box background is not applied (the port renders rows
//! without row backgrounds).

use pi_tui::component::Component;
use pi_tui::components::markdown::{
    DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTransform,
};
use pi_tui::theme::ThemeColor;

use super::super::theme;
use super::markdown_transform::MarkdownTransformCtx;
use std::sync::Arc;

/// `tokensBefore.toLocaleString()` (`compaction-summary-message.ts:51`): the
/// summary row uses grouped digits, unlike the footer's compact `1.2k` form.
fn format_token_count(count: u64) -> String {
    let digits = count.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// The compaction summary row (`compaction-summary-message.ts:12-99`).
pub struct CompactionSummaryMessageComponent {
    expanded: bool,
    summary: String,
    tokens_before: u64,
    output_pad: usize,
    transform_ctx: MarkdownTransformCtx,
    /// The expanded body (header + summary); renders at the real width like
    /// upstream's Markdown child.
    markdown: Markdown,
    /// The fixed prefix lines (label, blank) and the collapsed line; rebuilt
    /// on state changes, like upstream's `updateDisplay`.
    content: Vec<String>,
}

impl CompactionSummaryMessageComponent {
    pub fn new(
        summary: &str,
        tokens_before: u64,
        output_pad: usize,
        transform_ctx: MarkdownTransformCtx,
    ) -> Self {
        let mut component = Self {
            expanded: false,
            summary: summary.to_string(),
            tokens_before,
            output_pad,
            transform_ctx,
            markdown: Self::build_markdown(summary, tokens_before, output_pad, None),
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

    /// `setOutputPad`.
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
            .map(|inner| Self::wrap_with_header(self.tokens_before, inner));
        self.markdown = Self::build_markdown(
            &self.summary,
            self.tokens_before,
            self.output_pad,
            transform,
        );
    }

    /// The header wrapper (`compaction-summary-message.ts:64-66`):
    /// transformers see only the summary content, Pi's header goes back in
    /// front of the transformed markdown per render. `None` (no registered
    /// transformer) keeps the port's combined header+summary text — the
    /// identical render.
    fn wrap_with_header(tokens_before: u64, inner: MarkdownTransform) -> MarkdownTransform {
        let token_str = format_token_count(tokens_before);
        let header = format!("**Compacted from {token_str} tokens**\n\n");
        Arc::new(move |markdown: &str, width: usize| format!("{header}{}", inner(markdown, width)))
    }

    /// The expanded Markdown child: Pi's header, then the summary
    /// (`compaction-summary-message.ts:49-68`).
    fn build_markdown(
        summary: &str,
        tokens_before: u64,
        output_pad: usize,
        transform: Option<MarkdownTransform>,
    ) -> Markdown {
        let theme = theme::theme();
        let token_str = format_token_count(tokens_before);
        let header = format!("**Compacted from {token_str} tokens**\n\n");
        let style = DefaultTextStyle {
            color: Some(theme.fg_styler(ThemeColor::CustomMessageText)),
            ..DefaultTextStyle::default()
        };
        Markdown::new(
            // Without a transform the text is the combined header+summary;
            // with one, transformers see only the summary and the wrapper
            // re-adds the header.
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
        let token_str = format_token_count(self.tokens_before);
        let pad = " ".repeat(self.output_pad);
        // `theme.fg("customMessageLabel", bold("[compaction]"))`.
        let label = format!(
            "{pad}{}",
            theme.fg(
                ThemeColor::CustomMessageLabel,
                "\x1b[1m[compaction]\x1b[22m"
            )
        );
        let mut content = vec![label, pad.clone()];

        if !self.expanded {
            // `Compacted from N tokens ( + keyText + " to expand)"` — the
            // description sits outside the key hint
            // (`compaction-summary-message.ts:66-71`).
            let key = theme::key_text(theme::app_keys::TOOLS_EXPAND);
            content.push(format!(
                "{pad}{}{}{}",
                theme.fg(
                    ThemeColor::CustomMessageText,
                    &format!("Compacted from {token_str} tokens (")
                ),
                theme.fg(ThemeColor::Dim, &key),
                theme.fg(ThemeColor::CustomMessageText, " to expand)"),
            ));
        }

        self.content = content;
    }
}

impl Component for CompactionSummaryMessageComponent {
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

    fn plain_lines(component: &CompactionSummaryMessageComponent, width: usize) -> Vec<String> {
        component
            .render(width)
            .iter()
            .map(|line| strip_terminal_sequences(line).trim_end().to_string())
            .collect()
    }

    #[test]
    fn collapsed_row_labels_the_tokens_and_the_expand_key() {
        let component = CompactionSummaryMessageComponent::new(
            "summary text",
            1234,
            1,
            MarkdownTransformCtx::default(),
        );
        let plain = plain_lines(&component, 80);
        assert!(plain[0].contains("[compaction]"), "{plain:?}");
        let joined = plain.join("\n");
        assert!(
            joined.contains("Compacted from 1,234 tokens (ctrl+o to expand)"),
            "{joined}"
        );
        assert!(!joined.contains("1.2k"), "{joined}");
    }

    #[test]
    fn token_counts_use_locale_style_grouping() {
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1000), "1,000");
        assert_eq!(format_token_count(12_345), "12,345");
        assert_eq!(format_token_count(1_234_567), "1,234,567");
    }

    #[test]
    fn expanded_row_renders_the_summary_markdown() {
        let mut component = CompactionSummaryMessageComponent::new(
            "## Goal\nfinish it",
            96,
            1,
            MarkdownTransformCtx::default(),
        );
        component.set_expanded(true);
        let joined = plain_lines(&component, 80).join("\n");
        assert!(joined.contains("Compacted from 96 tokens"), "{joined}");
        assert!(joined.contains("Goal"), "{joined}");
        assert!(joined.contains("finish it"), "{joined}");
    }

    #[test]
    fn expanding_and_collapsing_toggles_the_body() {
        let mut component =
            CompactionSummaryMessageComponent::new("body", 10, 1, MarkdownTransformCtx::default());
        component.set_expanded(true);
        assert!(plain_lines(&component, 80).len() > 3);
        component.set_expanded(false);
        assert_eq!(plain_lines(&component, 80).len(), 3);
    }
}
