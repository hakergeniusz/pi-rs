//! `StatusIndicator` — the spinner shown while the agent works, embedded in
//! the editor's top border.
//!
//! Ground truth: `modes/interactive/components/status-indicator.ts`. The
//! loader is host-driven (`pi_tui::components::loader::Loader::tick`), so the
//! interactive loop advances the frames; upstream owns a `setInterval`.

use pi_tui::component::Component;
use pi_tui::components::loader::Loader;
use pi_tui::theme::ThemeColor;

use super::super::theme;

/// `StatusIndicatorKind` (`status-indicator.ts:7`). Retry/branch indicators
/// land with their loops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    /// `RetryStatusIndicator` (`status-indicator.ts:51-63`).
    Retry,
    Working,
    Compaction,
}

/// `StatusIndicator` (`status-indicator.ts:9-36`).
pub struct StatusIndicator {
    loader: Loader,
    pub kind: StatusKind,
}

impl StatusIndicator {
    /// `WorkingStatusIndicator` with the extension-driven options (E3
    /// slice 7): the message rides `workingMessage ?? "Working"` and the
    /// indicator rides `workingIndicatorOptions` (`interactive-mode.ts
    /// :2486-2499`).
    pub fn working_with(
        message: &str,
        indicator: Option<pi_tui::components::loader::LoaderIndicator>,
    ) -> Self {
        let spinner_theme = theme::theme();
        let message_theme = theme::theme();
        StatusIndicator {
            loader: Loader::new(
                move |text| spinner_theme.fg(ThemeColor::Accent, text),
                move |text| message_theme.fg(ThemeColor::Muted, text),
                message,
                indicator,
            ),
            kind: StatusKind::Working,
        }
    }

    /// The live working indicator's message (`setWorkingMessage`,
    /// `interactive-mode.ts:2781-2786` — upstream calls the loader's
    /// `setMessage` through the StatusIndicator).
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.loader.set_message(message);
    }

    /// The live working indicator's frames (`setWorkingIndicator`,
    /// `interactive-mode.ts:2514-2520`).
    pub fn set_indicator(
        &mut self,
        indicator: Option<pi_tui::components::loader::LoaderIndicator>,
    ) {
        self.loader.set_indicator(indicator);
    }

    pub fn working(message: &str) -> Self {
        let spinner_theme = theme::theme();
        let message_theme = theme::theme();
        StatusIndicator {
            loader: Loader::new(
                move |text| spinner_theme.fg(ThemeColor::Accent, text),
                move |text| message_theme.fg(ThemeColor::Muted, text),
                message,
                None,
            ),
            kind: StatusKind::Working,
        }
    }

    /// `RetryStatusIndicator` (`status-indicator.ts:51-63`): warning
    /// spinner, muted label. Upstream counts the seconds down; the port
    /// shows the initial figure.
    pub fn retry(message: &str) -> Self {
        let spinner_theme = theme::theme();
        let message_theme = theme::theme();
        StatusIndicator {
            loader: Loader::new(
                move |text| spinner_theme.fg(ThemeColor::Warning, text),
                move |text| message_theme.fg(ThemeColor::Muted, text),
                message,
                None,
            ),
            kind: StatusKind::Retry,
        }
    }

    /// `CompactionStatusIndicator` (`status-indicator.ts:85-100`): accent
    /// spinner, muted label.
    pub fn compaction(message: &str) -> Self {
        let spinner_theme = theme::theme();
        let message_theme = theme::theme();
        StatusIndicator {
            loader: Loader::new(
                move |text| spinner_theme.fg(ThemeColor::Accent, text),
                move |text| message_theme.fg(ThemeColor::Muted, text),
                message,
                None,
            ),
            kind: StatusKind::Compaction,
        }
    }

    /// `renderInBorder` (`status-indicator.ts:24-27`): the loader's message
    /// line, trimmed to the border width. The loader renders a blank line
    /// above the spinner line (`loader.ts:162-167`), so take line 1.
    pub fn render_in_border(&self, width: usize) -> String {
        let line = self
            .loader
            .render(width)
            .get(1)
            .cloned()
            .unwrap_or_default();
        let trimmed = line.trim().trim_end();
        pi_tui::ansi::truncate_to_width(trimmed, width, "", false)
    }

    /// `renderSpinnerInBorder` (`status-indicator.ts:29-31`).
    pub fn render_spinner_in_border(&mut self, width: usize) -> String {
        pi_tui::ansi::truncate_to_width(self.loader.current_frame(), width, "", false)
    }

    /// Advance the animation; the host calls this every `interval_ms`.
    pub fn tick(&mut self) -> bool {
        self.loader.tick()
    }

    pub fn interval_ms(&self) -> u64 {
        self.loader.interval_ms()
    }

    pub fn is_running(&self) -> bool {
        self.loader.is_running()
    }

    pub fn stop(&mut self) {
        self.loader.stop();
    }
}

/// `IdleStatus` (`status-indicator.ts:114-123`): two blank lines.
pub struct IdleStatus;

impl pi_tui::component::Component for IdleStatus {
    fn render(&self, width: usize) -> Vec<String> {
        let empty_line = " ".repeat(width);
        vec![empty_line.clone(), empty_line]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_tui::ansi::strip_terminal_sequences;

    #[test]
    fn border_render_strips_the_padding() {
        let mut indicator = StatusIndicator::working("Working...");
        indicator.loader.start();
        let line = strip_terminal_sequences(&indicator.render_in_border(40));
        assert!(line.contains("Working..."), "{line}");
        assert!(!line.starts_with(' '), "no leading pad: '{line}'");
    }

    #[test]
    fn spinner_frame_is_bare() {
        let mut indicator = StatusIndicator::working("x");
        let frame = strip_terminal_sequences(&indicator.render_spinner_in_border(10));
        assert_eq!(frame, "⠋");
    }
}
