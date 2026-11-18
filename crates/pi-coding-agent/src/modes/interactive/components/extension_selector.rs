//! `ExtensionSelectorComponent` — the generic option dialog.
//!
//! Ground truth: `modes/interactive/components/extension-selector.ts`
//! (1-107). Deviation: the countdown is pump-driven — the mode owns the
//! deadline and calls [`Self::set_countdown_seconds`] (upstream arms a
//! `CountdownTimer` inside the component); the tools-expanded passthrough
//! (`opts.onToggleToolsExpanded`) stays unwired (it lands with the
//! tools-expanded surface).

use std::sync::{Arc, Mutex};

use pi_tui::ansi::truncate_to_width;
use pi_tui::component::{Component, ComponentMutation};
use pi_tui::components::text::Text;
use pi_tui::keybindings::{self, action};
use pi_tui::theme::ThemeColor;

use super::super::theme;
use super::header::DynamicBorder;

/// Events the selector publishes to the orchestrator.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtensionSelectorEvent {
    /// `onSelect(option)` — the highlighted option was confirmed.
    Selected(String),
    /// `onCancel` — escape.
    Cancel,
}

/// One mounted option dialog (`ExtensionSelectorComponent`,
/// `extension-selector.ts:20-107`).
pub struct ExtensionSelectorComponent {
    title: String,
    options: Vec<String>,
    selected_index: usize,
    events: Arc<Mutex<Vec<ExtensionSelectorEvent>>>,
    /// The countdown's seconds for the `(Ns)` title suffix; `None` keeps
    /// the plain title. The mode drives it (the pump ticks the deadline),
    /// like the input dialog's countdown.
    countdown_seconds: Option<u32>,
}

impl ExtensionSelectorComponent {
    pub fn new(
        title: &str,
        options: Vec<String>,
        events: Arc<Mutex<Vec<ExtensionSelectorEvent>>>,
    ) -> Self {
        Self {
            title: title.to_string(),
            options,
            selected_index: 0,
            events,
            countdown_seconds: None,
        }
    }

    /// The countdown's title suffix (`extension-selector.ts:65-69`: the
    /// title re-renders as `"{title} ({s}s)"` each second).
    pub fn set_countdown_seconds(&mut self, seconds: Option<u32>) {
        self.countdown_seconds = seconds;
    }

    /// The option window, centered on the selection
    /// (`updateList`, `extension-selector.ts:88-105`).
    fn render_options(&self, width: usize) -> Vec<String> {
        let theme = theme::theme();
        let mut lines = Vec::new();
        let max_visible = 10;
        let start_index = self
            .selected_index
            .saturating_sub(max_visible / 2)
            .min(self.options.len().saturating_sub(max_visible));
        let end_index = (start_index + max_visible).min(self.options.len());

        for index in start_index..end_index {
            let is_selected = index == self.selected_index;
            let text = if is_selected {
                theme.fg(ThemeColor::Accent, "→ ")
                    + &theme.fg(ThemeColor::Accent, &self.options[index])
            } else {
                format!("  {}", theme.fg(ThemeColor::Text, &self.options[index]))
            };
            lines.extend(Text::new(text, 1, 0).render(width));
        }

        if start_index > 0 || end_index < self.options.len() {
            let scroll_info = format!("  ({}/{})", self.selected_index + 1, self.options.len());
            lines.extend(Text::new(theme.fg(ThemeColor::Muted, &scroll_info), 0, 0).render(width));
        }

        lines
    }
}

impl Component for ExtensionSelectorComponent {
    /// The constructor's child order (`extension-selector.ts:42-61`): border,
    /// blank, title, blank, list, hints, blank, border.
    fn render(&self, width: usize) -> Vec<String> {
        let theme = theme::theme();
        let mut lines = Vec::new();
        lines.extend(Component::render(&DynamicBorder::new(), width));
        lines.push(String::new());
        // The countdown re-renders the title as `"{title} ({s}s)"`
        // (`extension-selector.ts:65-69`).
        let title = match self.countdown_seconds {
            Some(seconds) => format!("{} ({}s)", self.title, seconds),
            None => self.title.clone(),
        };
        lines.extend(
            Text::new(theme.fg(ThemeColor::Accent, &theme::bold(&title)), 1, 0).render(width),
        );
        lines.push(String::new());
        lines.extend(self.render_options(width));
        lines.push(String::new());
        lines.extend(
            Text::new(
                format!(
                    "{}  {}  {}",
                    theme::raw_key_hint("↑↓", "navigate"),
                    theme::key_hint(action::SELECT_CONFIRM, "select"),
                    theme::key_hint(action::SELECT_CANCEL, "cancel"),
                ),
                1,
                0,
            )
            .render(width),
        );
        lines.push(String::new());
        lines.extend(Component::render(&DynamicBorder::new(), width));
        lines
            .into_iter()
            .map(|line| truncate_to_width(&line, width, "", false))
            .collect()
    }

    /// `handleInput` (`extension-selector.ts:100-113`): up/down (with the
    /// vim aliases), confirm (and a bare newline), cancel.
    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        if keybindings::matches(data, action::SELECT_UP) || data == "k" {
            self.selected_index = self.selected_index.saturating_sub(1);
        } else if keybindings::matches(data, action::SELECT_DOWN) || data == "j" {
            self.selected_index =
                (self.selected_index + 1).min(self.options.len().saturating_sub(1));
        } else if keybindings::matches(data, action::SELECT_CONFIRM) || data == "\n" {
            if let Some(option) = self.options.get(self.selected_index).cloned() {
                self.events
                    .lock()
                    .expect("extension selector events")
                    .push(ExtensionSelectorEvent::Selected(option));
            }
        } else if keybindings::matches(data, action::SELECT_CANCEL) {
            self.events
                .lock()
                .expect("extension selector events")
                .push(ExtensionSelectorEvent::Cancel);
        }
        Some(ComponentMutation { consume: true })
    }

    fn has_input_handler(&self) -> bool {
        true
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_tui::ansi::strip_terminal_sequences;

    fn plain(component: &ExtensionSelectorComponent, width: usize) -> String {
        strip_terminal_sequences(&Component::render(component, width).join("\n"))
    }

    #[test]
    fn renders_title_options_and_hints() {
        let component = ExtensionSelectorComponent::new(
            "Summarize branch?",
            vec![
                "No summary".to_string(),
                "Summarize".to_string(),
                "Summarize with custom prompt".to_string(),
            ],
            Arc::new(Mutex::new(Vec::new())),
        );
        let text = plain(&component, 80);
        assert!(text.contains("Summarize branch?"), "{text}");
        assert!(text.contains("No summary"), "{text}");
        assert!(text.contains("Summarize with custom prompt"), "{text}");
        assert!(text.contains("select"), "{text}");
        assert!(text.contains("cancel"), "{text}");
    }

    #[test]
    fn navigation_clamps_and_confirm_publishes_the_option() {
        let events: Arc<Mutex<Vec<ExtensionSelectorEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let mut component = ExtensionSelectorComponent::new(
            "Pick",
            vec!["a".to_string(), "b".to_string()],
            Arc::clone(&events),
        );

        // Up from the top clamps.
        Component::handle_input(&mut component, "\x1b[A");
        Component::handle_input(&mut component, "j"); // vim down
        Component::handle_input(&mut component, "j"); // clamps at the last row
        Component::handle_input(&mut component, "\r");
        let taken = std::mem::take(&mut *events.lock().expect("events"));
        assert_eq!(
            taken,
            vec![ExtensionSelectorEvent::Selected("b".to_string())]
        );

        Component::handle_input(&mut component, "\x1b"); // escape
        let taken = std::mem::take(&mut *events.lock().expect("events"));
        assert_eq!(taken, vec![ExtensionSelectorEvent::Cancel]);
    }
}
