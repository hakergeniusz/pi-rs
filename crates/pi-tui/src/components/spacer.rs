//! Spacer component — empty layout lines.
//!
//! Ground truth: pi-mono `packages/tui/src/components/spacer.ts`.

use crate::component::Component;

/// A spacer that renders `lines` empty lines (`spacer.ts:7-26`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Spacer {
    lines: usize,
}

impl Component for Spacer {
    fn render(&self, _width: usize) -> Vec<String> {
        vec![String::new(); self.lines]
    }
}

impl Spacer {
    /// Defaults to one line, like upstream's `constructor(lines = 1)`.
    pub fn new(lines: usize) -> Self {
        Self { lines }
    }

    /// `setLines` (`spacer.ts:16-18`).
    pub fn set_lines(&mut self, lines: usize) {
        self.lines = lines;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_the_requested_empty_lines() {
        assert_eq!(Component::render(&Spacer::new(1), 80), vec![String::new()]);
        assert_eq!(Component::render(&Spacer::new(3), 80).len(), 3);
        assert!(Component::render(&Spacer::new(0), 80).is_empty());
    }
}
