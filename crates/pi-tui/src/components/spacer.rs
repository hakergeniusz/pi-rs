//! Spacer component — an empty, zero-size layout node.
//!
//! Ground truth: pi-mono `packages/tui/src/components/spacer.ts`.

use crate::component::Component;

/// A spacer that occupies no space and renders nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct Spacer;

impl Component for Spacer {
    fn render(&self, _width: usize) -> Vec<String> {
        Vec::new()
    }
}

impl Spacer {
    pub fn new() -> Self {
        Self
    }
}
