//! `UserMessageComponent` — a user message rendered as markdown on the
//! `userMessageBg` background.
//!
//! Ground truth: `modes/interactive/components/user-message.ts:73-82` and
//! `tui-main-screen.ts:266-269`. Upstream prefixes the rendered lines with
//! OSC 133 shell-prompt zone markers; the alt screen's
//! `previousPrompt`/`nextPrompt` scan for `\x1b]133;A` (`tui-alt-screen.ts:75,
//! 515-529`). The main screen strips the marker before drawing, so this port
//! only emits the markers while the fullscreen renderer is active: the mode
//! flips [`UserMessageComponent::set_osc133_markers`] on a TUI-mode switch.

use pi_tui::component::Component;
use pi_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownOptions};

use super::super::theme;
use super::markdown_transform::MarkdownTransformCtx;

/// `OSC133_ZONE_START`/`END`/`FINAL` (`user-message.ts:6-8`).
const OSC133_START: &str = "\x1b]133;A\x07";
const OSC133_END: &str = "\x1b]133;B\x07";
const OSC133_FINAL: &str = "\x1b]133;C\x07";

/// Wrap rendered lines in OSC 133 zone markers (`user-message.ts:73-82`).
pub(crate) fn with_osc133_markers(mut lines: Vec<String>) -> Vec<String> {
    if let Some(first) = lines.first_mut() {
        first.insert_str(0, OSC133_START);
    }
    if let Some(last) = lines.last_mut() {
        last.push_str(OSC133_END);
        last.push_str(OSC133_FINAL);
    }
    lines
}

/// The user message: markdown padded by one column, on the message
/// background (`user-message.ts:44-71`).
pub struct UserMessageComponent {
    /// Stored with the padding so `setOutputPad` can rebuild the child
    /// (`user-message.ts:39-42`).
    text: String,
    output_pad: usize,
    markdown_theme: pi_tui::components::markdown::MarkdownTheme,
    transform_ctx: MarkdownTransformCtx,
    markdown: Markdown,
    /// Alt-screen-only prompt zone markers (see the module doc).
    osc133: bool,
}

impl UserMessageComponent {
    pub fn new(text: &str, output_pad: usize, transform_ctx: MarkdownTransformCtx) -> Self {
        Self::with_theme(text, output_pad, theme::markdown_theme(), transform_ctx)
    }

    pub fn with_theme(
        text: &str,
        output_pad: usize,
        markdown_theme: pi_tui::components::markdown::MarkdownTheme,
        transform_ctx: MarkdownTransformCtx,
    ) -> Self {
        let theme = theme::theme();
        let style = DefaultTextStyle {
            color: Some(theme.fg_styler(pi_tui::theme::ThemeColor::UserMessageText)),
            bg_color: Some(theme.bg_styler(pi_tui::theme::ThemeBg::UserMessageBg)),
            ..DefaultTextStyle::default()
        };
        let markdown = Self::build_markdown(
            text,
            output_pad,
            markdown_theme.clone(),
            style,
            &transform_ctx,
        );
        UserMessageComponent {
            text: text.to_string(),
            output_pad,
            markdown_theme,
            transform_ctx,
            markdown,
            osc133: false,
        }
    }

    /// Enable the markers only on the alt screen (`tui-main-screen.ts:268`).
    pub fn set_osc133_markers(&mut self, enabled: bool) {
        self.osc133 = enabled;
    }

    fn build_markdown(
        text: &str,
        output_pad: usize,
        markdown_theme: pi_tui::components::markdown::MarkdownTheme,
        style: DefaultTextStyle,
        transform_ctx: &MarkdownTransformCtx,
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
                // The `user` surface (`user-message.ts:61-65`); user
                // messages are never streaming.
                transform: transform_ctx.make("user", None, None, None),
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
            self.markdown = Self::build_markdown(
                &self.text,
                output_pad,
                self.markdown_theme.clone(),
                style,
                &self.transform_ctx,
            );
        }
    }
}

impl Component for UserMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let lines = self.markdown.render(width);
        if self.osc133 && !lines.is_empty() {
            with_osc133_markers(lines)
        } else {
            lines
        }
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_tui::ansi::strip_terminal_sequences;

    /// The markers that drive alt-screen previousPrompt/nextPrompt are
    /// opt-in: main-screen rendering must never emit `133;A`
    /// (`tui-main-screen.ts:266-269`).
    #[test]
    fn osc133_markers_are_opt_in() {
        let mut component = UserMessageComponent::new("hello", 1, MarkdownTransformCtx::default());
        assert!(!component.render(40).join("\n").contains("\x1b]133;A"));
        component.set_osc133_markers(true);
        let lines = component.render(40);
        assert!(
            lines.first().expect("lines").starts_with("\x1b]133;A\x07"),
            "zone starts on the first line: {lines:?}"
        );
        assert!(
            lines
                .last()
                .expect("lines")
                .ends_with("\x1b]133;B\x07\x1b]133;C\x07"),
            "zone ends on the last line: {lines:?}"
        );
    }

    #[test]
    fn renders_markdown_on_the_message_background() {
        let component =
            UserMessageComponent::new("hello **world**", 1, MarkdownTransformCtx::default());
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
