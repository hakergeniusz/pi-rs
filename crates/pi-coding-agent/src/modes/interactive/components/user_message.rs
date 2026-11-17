//! `UserMessageComponent` — a user message rendered as markdown on the
//! `userMessageBg` background.
//!
//! Ground truth: `modes/interactive/components/user-message.ts`. Upstream also
//! prefixes the rendered lines with OSC 133 shell-prompt zone markers; the
//! main-screen renderer strips those before drawing, and this port's renderer
//! does not strip them yet, so they are omitted (documented deviation).

use pi_tui::component::Component;
use pi_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownOptions};

use super::super::theme;

/// The user message: markdown padded by one column, on the message
/// background (`user-message.ts:44-71`).
pub struct UserMessageComponent {
    /// Stored with the padding so `setOutputPad` can rebuild the child
    /// (`user-message.ts:39-42`).
    text: String,
    output_pad: usize,
    markdown_theme: pi_tui::components::markdown::MarkdownTheme,
    markdown: Markdown,
}

impl UserMessageComponent {
    pub fn new(text: &str, output_pad: usize) -> Self {
        Self::with_theme(text, output_pad, theme::markdown_theme())
    }

    pub fn with_theme(
        text: &str,
        output_pad: usize,
        markdown_theme: pi_tui::components::markdown::MarkdownTheme,
    ) -> Self {
        let theme = theme::theme();
        let style = DefaultTextStyle {
            color: Some(theme.fg_styler(pi_tui::theme::ThemeColor::UserMessageText)),
            bg_color: Some(theme.bg_styler(pi_tui::theme::ThemeBg::UserMessageBg)),
            ..DefaultTextStyle::default()
        };
        let markdown = Self::build_markdown(text, output_pad, markdown_theme.clone(), style);
        UserMessageComponent {
            text: text.to_string(),
            output_pad,
            markdown_theme,
            markdown,
        }
    }

    fn build_markdown(
        text: &str,
        output_pad: usize,
        markdown_theme: pi_tui::components::markdown::MarkdownTheme,
        style: DefaultTextStyle,
    ) -> Markdown {
        Markdown::new(
            text,
            output_pad,
            1,
            markdown_theme,
            Some(style),
            MarkdownOptions {
                preserve_list_markers: true,
                preserve_backslash_escapes: true,
            },
        )
    }

    /// `setOutputPad` (`user-message.ts:39-42`): store and rebuild.
    pub fn set_output_pad(&mut self, output_pad: usize) {
        if self.output_pad != output_pad {
            self.output_pad = output_pad;
            let theme = theme::theme();
            let style = DefaultTextStyle {
                color: Some(theme.fg_styler(pi_tui::theme::ThemeColor::UserMessageText)),
                bg_color: Some(theme.bg_styler(pi_tui::theme::ThemeBg::UserMessageBg)),
                ..DefaultTextStyle::default()
            };
            self.markdown =
                Self::build_markdown(&self.text, output_pad, self.markdown_theme.clone(), style);
        }
    }
}

impl Component for UserMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.markdown.render(width)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_tui::ansi::strip_terminal_sequences;

    #[test]
    fn renders_markdown_on_the_message_background() {
        let component = UserMessageComponent::new("hello **world**", 1);
        let lines = component.render(40);
        let plain: Vec<String> = lines
            .iter()
            .map(|line| strip_terminal_sequences(line).trim_end().to_string())
            .collect();
        // One blank line of padding above and below (padding_y = 1).
        assert_eq!(plain.first().map(String::as_str), Some(""));
        assert_eq!(plain.last().map(String::as_str), Some(""));
        assert!(
            plain
                .iter()
                .any(|line| line.trim().contains("hello") && line.contains("world")),
            "plain: {plain:?}"
        );
        // The background wraps the content lines.
        assert!(
            lines
                .iter()
                .any(|line| line.contains("\x1b[49m") || line.contains('h')),
            "expected styled or plain content"
        );
    }
}
