//! Undo stack for the editor.
//!
//! Ground truth: pi-mono `packages/tui/src/undo-stack.ts`.

/// An undo stack holding snapshots of type `T`.
#[derive(Debug, Clone, Default)]
pub struct UndoStack<T> {
    entries: Vec<T>,
}

impl<T> UndoStack<T> {
    /// An empty stack.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Drop every snapshot.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Record a snapshot of the edited state.
    pub fn push(&mut self, item: T) {
        self.entries.push(item);
    }

    /// Take the newest snapshot off the stack.
    pub fn pop(&mut self) -> Option<T> {
        self.entries.pop()
    }

    /// How many snapshots are stacked.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is stacked.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
