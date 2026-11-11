//! Truncated text component — renders text truncated to fit a width.
//!
//! Ground truth: pi-mono `packages/tui/src/components/truncated-text.ts`.

/// A text node that truncates with an ellipsis when it exceeds a width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncatedText {
    pub text: String,
    pub width: usize,
}

impl TruncatedText {
    pub fn new(text: String, width: usize) -> Self {
        Self { text, width }
    }

    pub fn render(&self) -> String {
        if self.text.len() <= self.width {
            self.text.clone()
        } else if self.width > 3 {
            format!("{}...", &self.text[..self.width - 3])
        } else {
            self.text[..self.width].to_string()
        }
    }
}
