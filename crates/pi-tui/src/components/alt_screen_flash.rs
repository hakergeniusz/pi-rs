//! `AltScreenFlashContainer`: `packages/tui/src/components/alt-screen-flash.ts`.
//!
//! Transient messages the alt-screen renderer composites over the frame. The
//! container is a component in pi and lives in the component tree there; here
//! the alt screen owns it outright (it is never mounted, only composited), so
//! it is a plain struct with the same render output.
//!
//! The one difference is expiry. pi arms a `setTimeout` per entry, so a flash
//! disappears on its own; `pi-tui` owns no timer (the same constraint that
//! makes the wheel accelerator and the double-click window take a host clock).
//! [`AltScreenFlashContainer::expire`] is that timer, driven by the host
//! through [`crate::tui::Tui::flash`] and the input path.

use crate::ansi::truncate_to_width;

/// `DEFAULT_DURATION_MS`, `alt-screen-flash.ts:5`.
pub const DEFAULT_FLASH_DURATION_MS: u64 = 1000;

#[derive(Debug)]
struct FlashEntry {
    message: String,
    /// The host clock at which the entry goes away. pi's `setTimeout` argument
    /// is a delay; this is the deadline it lands on.
    expires_at_ms: u64,
}

/// Transient messages composited by the alternate-screen renderer.
/// `alt-screen-flash.ts:13-58`.
#[derive(Debug, Default)]
pub struct AltScreenFlashContainer {
    /// pi stamps every entry with an id (`:16`, `:23`) so its timer callback
    /// can find the entry it belongs to. Expiry here is a deadline comparison,
    /// so the id has no reader and is not carried over.
    entries: Vec<FlashEntry>,
}

impl AltScreenFlashContainer {
    /// An empty container.
    pub fn new() -> Self {
        Self::default()
    }

    /// `flash`, `:22-38`. `now_ms` is the host clock the deadline is computed
    /// from; a negative duration clamps to zero, as `Math.max(0, durationMs)`
    /// does.
    pub fn flash(&mut self, message: impl Into<String>, duration_ms: u64, now_ms: u64) {
        self.entries.push(FlashEntry {
            message: message.into(),
            expires_at_ms: now_ms.saturating_add(duration_ms),
        });
    }

    /// Drop every entry whose deadline has passed. Returns `true` when one
    /// went away, which is the render request pi's timer callback makes.
    pub fn expire(&mut self, now_ms: u64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.expires_at_ms > now_ms);
        self.entries.len() != before
    }

    /// `dispose`, `:40-43`.
    pub fn dispose(&mut self) {
        self.entries.clear();
    }

    /// The live messages, oldest first.
    pub fn messages(&self) -> Vec<&str> {
        self.entries
            .iter()
            .map(|entry| entry.message.as_str())
            .collect()
    }

    /// Whether nothing is currently flashing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `render`, `:49-56`: one inverse line per entry, padded by a space on
    /// each side and truncated to the width.
    pub fn render(&self, width: usize) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| {
                let message = truncate_to_width(&format!(" {} ", entry.message), width, "", false);
                format!("\x1b[7m{message}\x1b[27m")
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_one_inverse_line_per_entry() {
        let mut flashes = AltScreenFlashContainer::new();
        flashes.flash("Copied!", DEFAULT_FLASH_DURATION_MS, 0);
        flashes.flash("Copy failed", DEFAULT_FLASH_DURATION_MS, 0);
        assert_eq!(
            flashes.render(20),
            vec!["\x1b[7m Copied! \x1b[27m", "\x1b[7m Copy failed \x1b[27m"]
        );
    }

    /// `:52` truncates with an empty ellipsis and no padding.
    #[test]
    fn truncates_to_the_width() {
        let mut flashes = AltScreenFlashContainer::new();
        flashes.flash("a long message", DEFAULT_FLASH_DURATION_MS, 0);
        // pi's `finalizeTruncatedResult` (`utils.ts:155-175`) resets the style after
        // the truncated prefix, before the container's inverse-off.
        assert_eq!(flashes.render(8), vec!["\x1b[7m a long \x1b[0m\x1b[27m"]);
    }

    /// `:26-32`: the deadline is `now + duration`, and an entry that has passed
    /// it is gone exactly once.
    #[test]
    fn expires_on_the_host_clock() {
        let mut flashes = AltScreenFlashContainer::new();
        flashes.flash("Copied!", 5000, 1_000);
        assert!(!flashes.expire(1_000), "still inside the window");
        assert!(!flashes.expire(5_999), "one millisecond early");
        assert!(flashes.expire(6_000), "the deadline is inclusive-end");
        assert!(flashes.is_empty());
        assert!(!flashes.expire(9_999), "and it does not come back");
    }

    /// `dispose`, `:40-43`.
    #[test]
    fn dispose_clears_everything() {
        let mut flashes = AltScreenFlashContainer::new();
        flashes.flash("Copied!", DEFAULT_FLASH_DURATION_MS, 0);
        flashes.dispose();
        assert!(flashes.render(20).is_empty());
    }
}
