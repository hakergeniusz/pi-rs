//! Kill ring for the editor (Emacs-style kill/yank).
//!
//! Ground truth: pi-mono `packages/tui/src/kill-ring.ts`.

/// A kill ring: a rotating buffer of killed text.
///
/// Consecutive kills in the same direction accumulate into a single entry:
/// backward kills (`ctrl+w`, `prepend=true`) prepend to the previous entry;
/// forward kills (`ctrl+u`, `prepend=false`) append to it.
#[derive(Debug, Clone, Default)]
pub struct KillRing {
    entries: Vec<String>,
    index: usize,
}

impl KillRing {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            index: 0,
        }
    }

    /// Push a killed string onto the ring.
    ///
    /// * `prepend=true`: the kill was backward (e.g. `ctrl+w`) — prepend to
    ///   the previous entry if the previous action was also a kill.
    /// * `was_kill=true`: the previous action was a kill — merge with the
    ///   previous entry instead of creating a new one.
    pub fn push(&mut self, text: &str, prepend: bool, was_kill: bool) {
        if text.is_empty() {
            return;
        }

        if was_kill && !self.entries.is_empty() {
            let last = self.entries.last_mut().unwrap();
            if prepend {
                last.insert_str(0, text);
            } else {
                last.push_str(text);
            }
            return;
        }

        self.entries.push(text.to_string());
        self.index = self.entries.len();
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn peek(&self) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        let idx = self.index.min(self.entries.len());
        if idx == 0 {
            None
        } else {
            Some(self.entries[idx - 1].as_str())
        }
    }

    /// Rotate to the previous entry in the ring (circular).
    pub fn rotate(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.index = if self.index == 0 {
            self.entries.len()
        } else {
            self.index - 1
        };
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}
