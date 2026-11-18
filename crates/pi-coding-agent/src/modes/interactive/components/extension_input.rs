//! `ExtensionInputComponent` — the extension `ctx.ui.input` dialog.
//!
//! Ground truth: `modes/interactive/components/extension-input.ts`
//! (1-92). Deviations: the countdown/timeout option stays pump-driven —
//! the mode owns the deadline and calls [`Self::set_countdown_seconds`]
//! (upstream arms a `CountdownTimer` inside the component); the tools
//! passthrough has no input counterpart.

use std::sync::{Arc, Mutex};

use pi_tui::ansi::truncate_to_width;
use pi_tui::component::{Component, ComponentMutation};
use pi_tui::components::input::{Input, InputOptions};
use pi_tui::components::text::Text;
use pi_tui::keybindings;
use pi_tui::keybindings::action;
use pi_tui::theme::ThemeColor;

use super::super::theme;
use super::header::DynamicBorder;

/// Events the input dialog publishes to the orchestrator.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtensionInputEvent {
    /// `onSubmit(value)` — enter on the input line.
    Submitted(String),
    /// `onCancel` — escape.
    Cancel,
}

/// One mounted text-input dialog (`ExtensionInputComponent`,
/// `extension-input.ts:22-92`).
pub struct ExtensionInputComponent {
    title: String,
    input: Input,
    events: Arc<Mutex<Vec<ExtensionInputEvent>>>,
    /// The countdown's seconds for the `(Ns)` title suffix; `None` keeps
    /// the plain title. The mode drives it (the pump ticks the deadline).
    countdown_seconds: Option<u32>,
}

impl ExtensionInputComponent {
    pub fn new(
        title: &str,
        _placeholder: Option<&str>,
        events: Arc<Mutex<Vec<ExtensionInputEvent>>>,
    ) -> Self {
        Self {
            title: title.to_string(),
            input: Input::new(InputOptions::default()),
            events,
            countdown_seconds: None,
        }
    }

    /// The countdown's title suffix (`extension-input.ts:70-74`: the
    /// title re-renders as `"{title} ({s}s)"` each second).
    pub fn set_countdown_seconds(&mut self, seconds: Option<u32>) {
        self.countdown_seconds = seconds;
    }
}

impl ExtensionInputComponent {
    fn render_title(&self, width: usize) -> Vec<String> {
        let theme = theme::theme();
        let text = match self.countdown_seconds {
            Some(seconds) => format!("{} ({}s)", self.title, seconds),
            None => self.title.clone(),
        };
        Text::new(theme.fg(ThemeColor::Accent, &text), 1, 0).render(width)
    }
}

impl Component for ExtensionInputComponent {
    /// The constructor's child order (`extension-input.ts:50-76`): border,
    /// blank, title, blank, input, blank, hints, blank, border.
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        lines.extend(Component::render(&DynamicBorder::new(), width));
        lines.push(String::new());
        lines.extend(self.render_title(width));
        lines.push(String::new());
        lines.extend(self.input.render(width));
        lines.push(String::new());
        lines.extend(
            Text::new(
                format!(
                    "{}  {}",
                    theme::key_hint(action::SELECT_CONFIRM, "submit"),
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

    /// `extension-input.ts:78-88`: confirm/`\n` submits the line's value,
    /// cancel cancels, everything else reaches the input.
    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        if keybindings::matches(data, action::SELECT_CONFIRM) || data == "\n" {
            let value = self.input.value().to_string();
            self.events
                .lock()
                .expect("extension input events")
                .push(ExtensionInputEvent::Submitted(value));
        } else if keybindings::matches(data, action::SELECT_CANCEL) {
            self.events
                .lock()
                .expect("extension input events")
                .push(ExtensionInputEvent::Cancel);
        } else {
            let _ = self.input.handle_key(data);
        }
        Some(ComponentMutation { consume: true })
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_tui::ansi::strip_terminal_sequences;

    fn dialog() -> (
        ExtensionInputComponent,
        Arc<Mutex<Vec<ExtensionInputEvent>>>,
    ) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            ExtensionInputComponent::new("Enter value", None, Arc::clone(&events)),
            events,
        )
    }

    /// The constructor's row order (`extension-input.ts:50-76`): border,
    /// blank, title, blank, input, blank, hints, blank, border.
    #[test]
    fn renders_border_title_input_hints_border() {
        let (dialog, _events) = dialog();
        let lines: Vec<String> = dialog
            .render(60)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect();
        assert!(lines[0].starts_with('─'), "top border: {:?}", lines[0]);
        assert_eq!(lines[1], "", "blank after the border");
        assert!(lines[2].contains("Enter value"), "{lines:?}");
        assert!(lines[4].contains('>'), "the input line rides a prompt");
        assert!(lines[6].contains("submit") && lines[6].contains("cancel"));
        assert!(lines.last().unwrap().starts_with('─'), "bottom border");
    }

    /// Confirm submits the typed value (`extension-input.ts:79-81`).
    #[test]
    fn enter_submits_the_value() {
        let (mut dialog, events) = dialog();
        let _ = dialog.handle_input("h");
        let _ = dialog.handle_input("i");
        let _ = dialog.handle_input("\r");
        assert_eq!(
            std::mem::take(&mut *events.lock().expect("events")),
            vec![ExtensionInputEvent::Submitted("hi".to_string())]
        );
    }

    /// Cancel publishes Cancel without touching the input
    /// (`extension-input.ts:82-84`).
    #[test]
    fn escape_cancels() {
        let (mut dialog, events) = dialog();
        let _ = dialog.handle_input("\x1b");
        assert_eq!(
            std::mem::take(&mut *events.lock().expect("events")),
            vec![ExtensionInputEvent::Cancel]
        );
    }

    /// The countdown suffix re-renders the title
    /// (`extension-input.ts:70-74`).
    #[test]
    fn countdown_renders_the_title_suffix() {
        let (mut dialog, _events) = dialog();
        dialog.set_countdown_seconds(Some(7));
        let lines: Vec<String> = dialog
            .render(60)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect();
        assert!(lines[2].contains("Enter value (7s)"), "{lines:?}");
    }
}
