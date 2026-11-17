//! `UserMessageSelectorComponent` — the `/fork` dialog.
//!
//! Ground truth: `modes/interactive/components/user-message-selector.ts`
//! (1-171). Deviations: upstream mounts the pieces as pi-tui container
//! children with a `setTimeout(onCancel, 100)` auto-cancel for an empty
//! list; the port's selectors render synchronously and the orchestrator
//! refuses an empty list before mounting (`showUserMessageSelector`'s
//! guard, `interactive-mode.ts:5695-5698`), so the auto-cancel has no
//! ported counterpart.

use std::sync::{Arc, Mutex};

use pi_tui::ansi::truncate_to_width;
use pi_tui::component::{Component, ComponentMutation};
use pi_tui::keybindings;
use pi_tui::keybindings::action;
use pi_tui::theme::ThemeColor;

use super::header::DynamicBorder;
use crate::modes::interactive::theme;

/// Events the selector publishes to the orchestrator.
#[derive(Debug, Clone, PartialEq)]
pub enum UserMessageSelectorEvent {
    /// `onSelect(entryId)` — fork to the highlighted user message.
    Selected(String),
    /// `onCancel` — escape.
    Cancel,
}

/// One forkable user message (`UserMessageItem`, `user-message-selector.ts:13-17`).
pub struct UserMessageItem {
    /// Entry ID in the session.
    pub id: String,
    /// The message text.
    pub text: String,
}

/// The message list (`UserMessageList`, `user-message-selector.ts:22-120`).
struct UserMessageList {
    /// Chronological order (oldest to newest).
    messages: Vec<UserMessageItem>,
    selected_index: usize,
    /// Max messages visible (`user-message-selector.ts:28`).
    max_visible: usize,
}

impl UserMessageList {
    fn new(messages: Vec<UserMessageItem>, initial_selected_id: Option<&str>) -> Self {
        // Start with the selected message if provided, else the most recent
        // (`user-message-selector.ts:30-36`).
        let initial_index =
            initial_selected_id.and_then(|id| messages.iter().position(|message| message.id == id));
        let selected_index = initial_index.unwrap_or_else(|| messages.len().saturating_sub(1));
        Self {
            messages,
            selected_index,
            max_visible: 10,
        }
    }

    /// `getSelectPageIndex` (`pi-tui/keybindings.ts:327-339`): pageUp/pageDown
    /// move by a page, clamped.
    fn select_page_index(&self, data: &str) -> Option<usize> {
        let direction: isize = if keybindings::matches(data, action::SELECT_PAGE_UP) {
            -1
        } else if keybindings::matches(data, action::SELECT_PAGE_DOWN) {
            1
        } else {
            return None;
        };
        let next = self.selected_index as isize + direction * self.max_visible as isize;
        Some(next.clamp(0, self.messages.len() as isize - 1) as usize)
    }

    fn render(&self, width: usize) -> Vec<String> {
        let theme = theme::theme();
        let mut lines = Vec::new();
        if self.messages.is_empty() {
            lines.push(theme.fg(ThemeColor::Muted, "  No user messages found"));
            return lines;
        }

        // Visible range with scrolling (`user-message-selector.ts:51-55`).
        let start_index = self
            .selected_index
            .saturating_sub(self.max_visible / 2)
            .min(self.messages.len().saturating_sub(self.max_visible));
        let end_index = (start_index + self.max_visible).min(self.messages.len());

        // Two lines per message plus a blank line
        // (`user-message-selector.ts:57-79`).
        for (position, message) in self.messages[start_index..end_index].iter().enumerate() {
            let index = start_index + position;
            let is_selected = index == self.selected_index;

            // Normalize the message to a single line.
            let normalized_message = message.text.replace('\n', " ").trim().to_string();

            // First line: cursor + message.
            let cursor = if is_selected {
                theme.fg(ThemeColor::Accent, "› ")
            } else {
                "  ".to_string()
            };
            let max_msg_width = width.saturating_sub(2); // Account for cursor (2 chars)
            let truncated_msg = truncate_to_width(&normalized_message, max_msg_width, "", false);
            let message_line = if is_selected {
                format!("{cursor}{}", theme::bold(&truncated_msg))
            } else {
                format!("{cursor}{truncated_msg}")
            };

            lines.push(message_line);

            // Second line: metadata (position in history).
            let metadata = format!("  Message {} of {}", index + 1, self.messages.len());
            lines.push(theme.fg(ThemeColor::Muted, &metadata));
            lines.push(String::new()); // Blank line between messages
        }

        // Scroll indicator only when the window moved
        // (`user-message-selector.ts:82-85`).
        if start_index > 0 || end_index < self.messages.len() {
            let scroll_info = format!("  ({}/{})", self.selected_index + 1, self.messages.len());
            lines.push(theme.fg(ThemeColor::Muted, &scroll_info));
        }

        lines
    }
}

/// The dialog (`UserMessageSelectorComponent`, `user-message-selector.ts:125-170`).
pub struct UserMessageSelectorComponent {
    message_list: UserMessageList,
    events: Arc<Mutex<Vec<UserMessageSelectorEvent>>>,
}

impl UserMessageSelectorComponent {
    pub fn new(
        messages: Vec<UserMessageItem>,
        initial_selected_id: Option<&str>,
        events: Arc<Mutex<Vec<UserMessageSelectorEvent>>>,
    ) -> Self {
        Self {
            message_list: UserMessageList::new(messages, initial_selected_id),
            events,
        }
    }

    /// `buildBaseLayout` — the constructor's child order
    /// (`user-message-selector.ts:136-159`).
    fn render_base(content: Vec<String>, width: usize) -> Vec<String> {
        let theme = theme::theme();
        let border = DynamicBorder::new();
        let mut lines = vec![String::new()];
        lines.extend(
            pi_tui::components::text::Text::new(theme::bold("Fork from Message"), 1, 0)
                .render(width),
        );
        lines.extend(pi_tui::components::text::Text::new(
            theme.fg(
                ThemeColor::Muted,
                "Select a user message to copy the active path up to that point into a new session",
            ),
            1,
            0,
        )
        .render(width));
        lines.push(String::new());
        lines.extend(border.render(width));
        lines.push(String::new());
        lines.extend(content);
        lines.push(String::new());
        lines.extend(border.render(width));
        lines
    }
}

impl Component for UserMessageSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        Self::render_base(self.message_list.render(width), width)
    }

    /// `handleInput` (`user-message-selector.ts:90-119`).
    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        if keybindings::matches(data, action::SELECT_UP) {
            // Go to the previous (older) message, wrap to the bottom at the top.
            self.message_list.selected_index = if self.message_list.selected_index == 0 {
                self.message_list.messages.len().saturating_sub(1)
            } else {
                self.message_list.selected_index - 1
            };
        } else if keybindings::matches(data, action::SELECT_DOWN) {
            // Go to the next (newer) message, wrap to the top at the bottom.
            let last = self.message_list.messages.len().saturating_sub(1);
            self.message_list.selected_index = if self.message_list.selected_index >= last {
                0
            } else {
                self.message_list.selected_index + 1
            };
        } else if let Some(page_index) = self.message_list.select_page_index(data) {
            if !self.message_list.messages.is_empty() {
                self.message_list.selected_index = page_index;
            }
        } else if keybindings::matches(data, action::SELECT_CONFIRM) {
            if let Some(message) = self
                .message_list
                .messages
                .get(self.message_list.selected_index)
            {
                self.events
                    .lock()
                    .expect("user message selector events")
                    .push(UserMessageSelectorEvent::Selected(message.id.clone()));
            }
        } else if keybindings::matches(data, action::SELECT_CANCEL) {
            self.events
                .lock()
                .expect("user message selector events")
                .push(UserMessageSelectorEvent::Cancel);
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
