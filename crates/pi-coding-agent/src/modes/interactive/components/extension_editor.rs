//! `ExtensionEditorComponent` — the multi-line editor dialog.
//!
//! Ground truth: `modes/interactive/components/extension-editor.ts` (1-145).
//! Deviations: the ctrl+G external-editor support is not wired (the port has
//! no external-editor plumbing yet — P7), so the hint row omits it and
//! `app.editor.external` falls through to the editor like any other key.

use std::sync::{Arc, Mutex};

use pi_tui::ansi::truncate_to_width;
use pi_tui::component::{Component, ComponentMutation};
use pi_tui::components::editor::{Editor, EditorOptions};
use pi_tui::components::text::Text;
use pi_tui::keybindings::{self, action};
use pi_tui::theme::ThemeColor;

use super::super::theme;
use super::header::DynamicBorder;

/// Events the editor publishes to the orchestrator.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtensionEditorEvent {
    /// `onSubmit(value)` — enter on a non-empty buffer.
    Submitted(String),
    /// `onCancel` — escape.
    Cancel,
}

/// One mounted editor dialog (`ExtensionEditorComponent`,
/// `extension-editor.ts:26-145`).
pub struct ExtensionEditorComponent {
    title: String,
    editor: Editor,
    events: Arc<Mutex<Vec<ExtensionEditorEvent>>>,
}

impl ExtensionEditorComponent {
    pub fn new(
        title: &str,
        prefill: Option<&str>,
        events: Arc<Mutex<Vec<ExtensionEditorEvent>>>,
    ) -> Self {
        let mut editor = Editor::new(theme::editor_theme(), EditorOptions::default());
        if let Some(prefill) = prefill {
            editor.set_text(prefill);
        }
        Self {
            title: title.to_string(),
            editor,
            events,
        }
    }
}

impl Component for ExtensionEditorComponent {
    /// The constructor's child order (`extension-editor.ts:68-107`): border,
    /// blank, title, blank, editor, blank, hint, blank, border.
    fn render(&self, width: usize) -> Vec<String> {
        let theme = theme::theme();
        let mut lines = Vec::new();
        lines.extend(Component::render(&DynamicBorder::new(), width));
        lines.push(String::new());
        lines.extend(Text::new(theme.fg(ThemeColor::Accent, &self.title), 1, 0).render(width));
        lines.push(String::new());
        lines.extend(Component::render(&self.editor, width));
        lines.push(String::new());
        lines.extend(
            Text::new(
                format!(
                    "{}  {}  {}",
                    theme::key_hint(action::SELECT_CONFIRM, "submit"),
                    theme::key_hint(action::INPUT_NEW_LINE, "newline"),
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

    /// `handleInput` (`extension-editor.ts:110-126`): escape cancels, the
    /// editor consumes everything else and publishes submitted lines.
    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        if keybindings::matches(data, action::SELECT_CANCEL) {
            self.events
                .lock()
                .expect("extension editor events")
                .push(ExtensionEditorEvent::Cancel);
            return Some(ComponentMutation { consume: true });
        }
        let mutation = Component::handle_input(&mut self.editor, data);
        while let Some(text) = self.editor.take_submitted() {
            self.events
                .lock()
                .expect("extension editor events")
                .push(ExtensionEditorEvent::Submitted(text));
        }
        mutation
    }

    fn has_input_handler(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        Component::set_focused(&mut self.editor, focused);
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_tui::ansi::strip_terminal_sequences;

    #[test]
    fn renders_title_prefill_and_hints() {
        let component = ExtensionEditorComponent::new(
            "Custom summarization instructions",
            Some("focus on the refactors"),
            Arc::new(Mutex::new(Vec::new())),
        );
        let text = strip_terminal_sequences(&Component::render(&component, 80).join("\n"));
        assert!(text.contains("Custom summarization instructions"), "{text}");
        assert!(text.contains("focus on the refactors"), "{text}");
        assert!(text.contains("submit"), "{text}");
        assert!(text.contains("cancel"), "{text}");
    }

    #[test]
    fn typing_then_enter_submits_and_escape_cancels() {
        let events: Arc<Mutex<Vec<ExtensionEditorEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let mut component = ExtensionEditorComponent::new("Title", None, Arc::clone(&events));

        for ch in "summarize the work".chars() {
            Component::handle_input(&mut component, &ch.to_string());
        }
        Component::handle_input(&mut component, "\r");
        let taken = std::mem::take(&mut *events.lock().expect("events"));
        assert_eq!(
            taken,
            vec![ExtensionEditorEvent::Submitted(
                "summarize the work".to_string()
            )]
        );

        Component::handle_input(&mut component, "\x1b");
        let taken = std::mem::take(&mut *events.lock().expect("events"));
        assert_eq!(taken, vec![ExtensionEditorEvent::Cancel]);
    }
}
