//! Undo stack for the editor.
//!
//! Ground truth: pi-mono `packages/tui/src/undo-stack.ts`.

/// An undo stack holding snapshots of type `T`.
#[derive(Debug, Clone, Default)]
pub struct UndoStack<T> {
    entries: Vec<T>,
}

impl<T> UndoStack<T> {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn push(&mut self, item: T) {
        self.entries.push(item);
    }

    pub fn pop(&mut self) -> Option<T> {
        self.entries.pop()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
