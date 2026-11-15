//! `Text`: wrapped, padded text. `packages/tui/src/components/text.ts`.

use crate::ansi::{apply_background_to_line, visible_width, wrap_text_with_ansi};
use crate::component::Component;

/// A background painter applied to every rendered line.
pub type CustomBgFn = Box<dyn Fn(&str) -> String + Send + Sync>;

/// A background painter applied to every rendered line.
pub type BgFn<'a> = &'a dyn Fn(&str) -> String;

/// A text block wrapped to the width, with margins and an optional background.
///
/// Empty or whitespace-only content renders no lines at all.
pub struct Text {
    content: String,
    padding_x: usize,
    padding_y: usize,
    custom_bg: Option<CustomBgFn>,
}

impl std::fmt::Debug for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Text")
            .field("text", &self.content)
            .field("padding_x", &self.padding_x)
            .field("padding_y", &self.padding_y)
            .field("custom_bg", &self.custom_bg.is_some())
            .finish()
    }
}

impl Text {
    /// `text.ts:31-38`.
    pub fn new(text: impl Into<String>, padding_x: usize, padding_y: usize) -> Self {
        Self {
            content: text.into(),
            padding_x,
            padding_y,
            custom_bg: None,
        }
    }

    /// `text.ts:40-45`.
    pub fn with_bg(
        text: impl Into<String>,
        padding_x: usize,
        padding_y: usize,
        bg: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> Self {
        Self {
            content: text.into(),
            padding_x,
            padding_y,
            custom_bg: Some(Box::new(bg)),
        }
    }

    /// `text.ts:40-45`.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.content = text.into();
    }

    /// The current text.
    pub fn text(&self) -> &str {
        &self.content
    }

    fn apply(&self, line: &str) -> String {
        match &self.custom_bg {
            Some(bg) => bg(line),
            None => line.to_string(),
        }
    }
}

impl Default for Text {
    /// pi's default is `("", 1, 1)`. `text.ts:33`.
    fn default() -> Self {
        Self::new("", 1, 1)
    }
}

impl Component for Text {
    /// `text.ts:66-124`.
    fn render(&self, width: usize) -> Vec<String> {
        // No actual text renders nothing, not a padded blank line.
        if self.content.trim().is_empty() {
            return Vec::new();
        }

        // Tabs become three spaces. `text.ts:76`.
        let normalized = self.content.replace('\t', "   ");

        // Margins shrink so content plus padding always fits. `text.ts:79-81`.
        let padding_x = self.padding_x.min((width.saturating_sub(1)) / 2);
        let content_width = width.saturating_sub(padding_x * 2).max(1);

        let wrapped = wrap_text_with_ansi(&normalized, content_width);
        let left_margin = " ".repeat(padding_x);
        let right_margin = " ".repeat(padding_x);

        let mut lines: Vec<String> = Vec::new();
        for line in wrapped {
            let with_margins = format!("{left_margin}{line}{right_margin}");
            if let Some(bg) = &self.custom_bg {
                lines.push(apply_background_to_line(&with_margins, width, bg.as_ref()));
            } else {
                let pad = width.saturating_sub(visible_width(&with_margins));
                lines.push(format!("{}{}", with_margins, " ".repeat(pad)));
            }
        }

        // Top and bottom padding are blank (or background-filled) lines.
        // `text.ts:112-121`.
        let empty_line = " ".repeat(width);
        let padded = self.apply(&empty_line);
        let mut result: Vec<String> = vec![padded.clone(); self.padding_y];
        result.append(&mut lines);
        result.extend(std::iter::repeat_n(padded, self.padding_y));

        crate::ansi::flatten_lines(&mut result);
        if result.is_empty() {
            vec![String::new()]
        } else {
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::strip_terminal_sequences;

    fn plain(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect()
    }

    #[test]
    fn empty_text_renders_nothing() {
        assert!(Text::new("", 1, 1).render(20).is_empty());
        assert!(Text::new("   ", 1, 1).render(20).is_empty());
    }

    #[test]
    fn wraps_and_pads_to_width() {
        let text = Text::new("hello world", 1, 0);
        assert_eq!(plain(&text.render(7)), vec![" hello ", " world "]);
    }

    #[test]
    fn vertical_padding_adds_blank_lines() {
        let text = Text::new("x", 0, 1);
        assert_eq!(plain(&text.render(3)), vec!["   ", "x  ", "   "]);
    }

    #[test]
    fn tabs_expand_to_three_spaces() {
        let text = Text::new("a\tb", 0, 0);
        assert_eq!(plain(&text.render(10)), vec!["a   b     "]);
    }

    #[test]
    fn padding_shrinks_to_fit_narrow_widths() {
        let text = Text::new("x", 5, 0);
        // width 4 -> padding clamps to 1, content width 2.
        assert_eq!(plain(&text.render(4)), vec![" x  "]);
    }

    #[test]
    fn custom_background_is_applied_to_every_line() {
        let text = Text::with_bg("hi", 0, 1, |line| format!("<{line}>"));
        let rendered = text.render(6);
        assert_eq!(plain(&rendered), vec!["<      >", "<hi    >", "<      >"]);
    }
}
