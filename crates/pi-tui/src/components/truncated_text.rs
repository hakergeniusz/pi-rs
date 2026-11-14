//! Truncated text component — one line of text, truncated to fit.
//!
//! Ground truth: pi-mono `packages/tui/src/components/truncated-text.ts`.

use crate::ansi::{truncate_to_width, visible_width};
use crate::component::Component;

/// A text node that renders its first line, truncated with `...` when it
/// exceeds the width, inside `paddingX`/`paddingY` padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncatedText {
    text: String,
    padding_x: usize,
    padding_y: usize,
}

impl TruncatedText {
    /// `truncated-text.ts:12-16`.
    pub fn new(text: String, padding_x: usize, padding_y: usize) -> Self {
        Self {
            text,
            padding_x,
            padding_y,
        }
    }
}

impl Component for TruncatedText {
    /// `truncated-text.ts:22-62`.
    fn render(&self, width: usize) -> Vec<String> {
        let mut result: Vec<String> = Vec::new();

        // Empty line padded to width.
        let empty_line = " ".repeat(width);

        // Vertical padding above.
        for _ in 0..self.padding_y {
            result.push(empty_line.clone());
        }

        // Available width after horizontal padding.
        let available_width = width.saturating_sub(self.padding_x * 2).max(1);

        // Take only the first line (stop at newline).
        let single_line_text = match self.text.find('\n') {
            Some(newline_index) => &self.text[..newline_index],
            None => self.text.as_str(),
        };

        // Truncate text if needed (accounting for ANSI codes).
        let display_text = truncate_to_width(single_line_text, available_width, "...", false);

        // Horizontal padding, then pad the line to exactly `width`.
        let left_padding = " ".repeat(self.padding_x);
        let right_padding = " ".repeat(self.padding_x);
        let line_with_padding = format!("{left_padding}{display_text}{right_padding}");
        let padding_needed = width.saturating_sub(visible_width(&line_with_padding));
        result.push(format!("{line_with_padding}{}", " ".repeat(padding_needed)));

        // Vertical padding below.
        for _ in 0..self.padding_y {
            result.push(empty_line.clone());
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_pads_and_takes_the_first_line() {
        let text = TruncatedText::new("short".to_string(), 1, 1);
        let lines = text.render(10);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], " ".repeat(10));
        assert_eq!(lines[1], format!(" {} ", "short") + &" ".repeat(3));
        assert_eq!(lines[2], " ".repeat(10));

        let long = TruncatedText::new("0123456789abcdef".to_string(), 0, 0);
        assert_eq!(
            crate::ansi::strip_terminal_sequences(&long.render(6)[0]),
            "012..."
        );

        let multiline = TruncatedText::new("first\nsecond".to_string(), 0, 0);
        let line = crate::ansi::strip_terminal_sequences(&multiline.render(80)[0]);
        assert_eq!(line.trim_end(), "first");
        assert_eq!(line.len(), 80);
    }
}
