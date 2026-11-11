//! `SettingsList`: label/value rows with a description pane.
//! `packages/tui/src/components/settings-list.ts`.
//!
//! Ported: the list rendering, selection, value cycling and the hint line.
//! Not ported (4b): the search box (it wraps pi's `Input`) and value
//! submenus (they open a full TUI overlay). The cycle-values path covers the
//! settings pi renders without a submenu.

use crate::ansi::{truncate_to_width, visible_width, wrap_text_with_ansi};
use crate::component::Component;
use crate::components::select_list::select_keys;
use crate::keys::matches_key;
use crate::mouse::{MouseButton, MouseEvent, MouseEventKind, MouseEventResult};

/// One setting. `settings-list.ts:9-25`.
#[derive(Debug, Clone)]
pub struct SettingItem {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub current_value: String,
    /// When set, Enter/Space cycles these values.
    pub values: Option<Vec<String>>,
}

impl SettingItem {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        current_value: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
            current_value: current_value.into(),
            values: None,
        }
    }

    #[must_use]
    pub fn with_values(mut self, values: Vec<String>) -> Self {
        self.current_value = values.first().cloned().unwrap_or_default();
        self.values = Some(values);
        self
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// A style hook: the label text plus whether the row is selected.
pub type SettingsStyleFn = std::sync::Arc<dyn Fn(&str, bool) -> String + Send + Sync>;

/// Style hooks. `settings-list.ts:27-34`.
#[derive(Clone)]
pub struct SettingsListTheme {
    pub label: SettingsStyleFn,
    pub value: SettingsStyleFn,
    pub description: std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>,
    pub cursor: String,
    pub hint: std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>,
}

impl Default for SettingsListTheme {
    fn default() -> Self {
        Self {
            label: std::sync::Arc::new(|text: &str, _selected: bool| text.to_string()),
            value: std::sync::Arc::new(|text: &str, _selected: bool| text.to_string()),
            description: std::sync::Arc::new(|text: &str| text.to_string()),
            cursor: "→ ".to_string(),
            hint: std::sync::Arc::new(|text: &str| text.to_string()),
        }
    }
}

impl std::fmt::Debug for SettingsListTheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsListTheme")
            .field("cursor", &self.cursor)
            .finish_non_exhaustive()
    }
}

/// What a key press did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsListAction {
    /// Selection moved.
    SelectionChanged,
    /// A value cycled: (id, new value).
    Changed {
        id: String,
        value: String,
    },
    /// A setting without values was activated: nothing to apply.
    Activated {
        id: String,
    },
    Cancelled,
    None,
}

/// The settings list. `settings-list.ts:36-...`.
pub struct SettingsList {
    items: Vec<SettingItem>,
    selected_index: usize,
    max_visible: usize,
    theme: SettingsListTheme,
    /// Row the press landed on; a click confirms it even if the pointer moved.
    /// `settings-list.ts:74`.
    mouse_pressed_index: Option<usize>,
    /// What the last mouse gesture asked the host to do.
    pending_action: SettingsListAction,
}

impl SettingsList {
    /// `settings-list.ts:55-70`.
    pub fn new(items: Vec<SettingItem>, max_visible: usize, theme: SettingsListTheme) -> Self {
        Self {
            items,
            selected_index: 0,
            max_visible,
            theme,
            mouse_pressed_index: None,
            pending_action: SettingsListAction::None,
        }
    }

    /// What the last mouse gesture asked for, where pi's `onChange` fires.
    pub fn take_action(&mut self) -> SettingsListAction {
        std::mem::replace(&mut self.pending_action, SettingsListAction::None)
    }

    /// `settings-list.ts:74-79`.
    pub fn update_value(&mut self, id: &str, new_value: &str) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            item.current_value = new_value.to_string();
        }
    }

    /// `settings-list.ts:81-87`.
    pub fn select_item(&mut self, id: &str) {
        if let Some(index) = self.items.iter().position(|item| item.id == id) {
            self.selected_index = index;
        }
    }

    /// Move the selection by index. Tests and hosts use this; pi exposes the
    /// same thing through mouse handling.
    pub fn set_selected_index(&mut self, index: usize) {
        self.selected_index = index.min(self.items.len().saturating_sub(1));
    }

    pub fn selected_item(&self) -> Option<&SettingItem> {
        self.items.get(self.selected_index)
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// `settings-list.ts:264-271`.
    fn visible_range(&self) -> (usize, usize) {
        let count = self.items.len();
        let start_index = self
            .selected_index
            .saturating_sub(self.max_visible / 2)
            .min(count.saturating_sub(self.max_visible));
        (start_index, (start_index + self.max_visible).min(count))
    }

    /// `settings-list.ts:313-320`.
    fn activate_item(&mut self) -> SettingsListAction {
        let Some(item) = self.items.get(self.selected_index) else {
            return SettingsListAction::None;
        };
        let id = item.id.clone();
        if let Some(values) = &item.values {
            if values.is_empty() {
                return SettingsListAction::None;
            }
            let current_index = values
                .iter()
                .position(|value| *value == item.current_value)
                .map_or(0, |index| index + 1);
            let next_index = current_index % values.len();
            let new_value = values[next_index].clone();
            if let Some(item) = self.items.get_mut(self.selected_index) {
                item.current_value.clone_from(&new_value);
            }
            self.pending_action = SettingsListAction::Changed {
                id: id.clone(),
                value: new_value.clone(),
            };
            return SettingsListAction::Changed {
                id,
                value: new_value,
            };
        }
        self.pending_action = SettingsListAction::Activated { id: id.clone() };
        SettingsListAction::Activated { id }
    }

    /// `settings-list.ts:222-249`.
    pub fn handle_key(&mut self, data: &str) -> SettingsListAction {
        if matches_key(data, select_keys::UP) {
            if self.items.is_empty() {
                return SettingsListAction::None;
            }
            self.selected_index = if self.selected_index == 0 {
                self.items.len() - 1
            } else {
                self.selected_index - 1
            };
            return SettingsListAction::SelectionChanged;
        }
        if matches_key(data, select_keys::DOWN) {
            if self.items.is_empty() {
                return SettingsListAction::None;
            }
            self.selected_index = if self.selected_index == self.items.len() - 1 {
                0
            } else {
                self.selected_index + 1
            };
            return SettingsListAction::SelectionChanged;
        }
        if matches_key(data, select_keys::CONFIRM) || data == " " {
            return self.activate_item();
        }
        if select_keys::CANCEL.iter().any(|key| matches_key(data, key)) {
            return SettingsListAction::Cancelled;
        }
        SettingsListAction::None
    }

    /// `settings-list.ts:322-328`.
    fn hint_line(&self, width: usize) -> String {
        truncate_to_width(
            &(self.theme.hint)("  Enter/Space to change · Esc to cancel"),
            width,
            "",
            false,
        )
    }
}

impl Component for SettingsList {
    /// `settings-list.ts:92-176`.
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        if self.items.is_empty() {
            lines.push((self.theme.hint)("  No settings available"));
            lines.push(String::new());
            lines.push(self.hint_line(width));
            return lines;
        }

        let (start_index, end_index) = self.visible_range();
        let max_label_width = self
            .items
            .iter()
            .map(|item| visible_width(&item.label))
            .max()
            .unwrap_or(0)
            .min(36);

        for position in start_index..end_index {
            let item = &self.items[position];
            let is_selected = position == self.selected_index;
            let prefix = if is_selected {
                self.theme.cursor.as_str()
            } else {
                "  "
            };
            let prefix_width = visible_width(prefix);
            let label_padded = format!(
                "{}{}",
                item.label,
                " ".repeat(max_label_width.saturating_sub(visible_width(&item.label)))
            );
            let label_text = (self.theme.label)(&label_padded, is_selected);
            let separator = "  ";
            let used_width = prefix_width + max_label_width + visible_width(separator);
            let value_max_width = (width as i64 - used_width as i64 - 2).max(0) as usize;
            let value_text = (self.theme.value)(
                &truncate_to_width(&item.current_value, value_max_width, "", false),
                is_selected,
            );
            lines.push(truncate_to_width(
                &format!("{prefix}{label_text}{separator}{value_text}"),
                width,
                "",
                false,
            ));
        }

        if start_index > 0 || end_index < self.items.len() {
            let scroll_text = format!("  ({}/{})", self.selected_index + 1, self.items.len());
            lines.push((self.theme.hint)(&truncate_to_width(
                &scroll_text,
                width.saturating_sub(2),
                "",
                false,
            )));
        }

        if let Some(description) = self.items[self.selected_index].description.as_ref() {
            lines.push(String::new());
            for line in wrap_text_with_ansi(description, width.saturating_sub(4).max(1)) {
                lines.push((self.theme.description)(&format!("  {line}")));
            }
        }

        lines.push(String::new());
        lines.push(self.hint_line(width));
        lines
    }

    fn handle_input(&mut self, data: &str) -> Option<crate::component::ComponentMutation> {
        let action = self.handle_key(data);
        Some(crate::component::ComponentMutation {
            consume: action != SettingsListAction::None,
        })
    }

    /// `SettingsList.handleMouse`, `settings-list.ts:179-217`.
    ///
    /// pi forwards to a submenu component (`settings-list.ts:180-183`) and to
    /// the search `Input` on rows 0-1 (`settings-list.ts:185-190`). This port
    /// has had neither since 4b — see the module docs — so `row_offset` is
    /// always 0. When a submenu lands, its forward belongs here.
    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        if self.items.is_empty() {
            return None;
        }
        if event.kind == MouseEventKind::Wheel {
            let delta = event.wheel_delta?;
            if delta == 0 {
                return None;
            }
            let step = if delta < 0 { -1i64 } else { 1 };
            let previous_index = self.selected_index;
            let next = (self.selected_index as i64 + step).clamp(0, self.items.len() as i64 - 1);
            self.selected_index = next as usize;
            return Some(MouseEventResult {
                handled: Some(true),
                render: Some(self.selected_index != previous_index),
                ..MouseEventResult::EMPTY
            });
        }
        // Hover must not change selection: the visible range is centred on it.
        if event.button != MouseButton::Left
            || !matches!(event.kind, MouseEventKind::Press | MouseEventKind::Click)
        {
            return None;
        }
        let (start_index, end_index) = self.visible_range();
        let item_y = event.y;
        if item_y < 0 {
            return None;
        }
        let item_index = start_index + item_y as usize;
        if item_index < start_index || item_index >= end_index {
            return None;
        }
        if event.kind == MouseEventKind::Press {
            self.mouse_pressed_index = Some(item_index);
            self.selected_index = item_index;
            return Some(MouseEventResult::focus());
        }
        self.selected_index = self.mouse_pressed_index.take().unwrap_or(item_index);
        self.activate_item();
        Some(MouseEventResult::handled())
    }

    fn has_input_handler(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<SettingItem> {
        vec![
            SettingItem::new("model", "Model", "fast")
                .with_values(vec!["fast".into(), "slow".into()]),
            SettingItem::new("theme", "Theme", "dark")
                .with_description("Colour scheme for the interface"),
            SettingItem::new("stream", "Stream", "off")
                .with_values(vec!["off".into(), "on".into()]),
        ]
    }

    fn list(max_visible: usize) -> SettingsList {
        SettingsList::new(items(), max_visible, SettingsListTheme::default())
    }

    fn plain(list: &SettingsList, width: usize) -> Vec<String> {
        list.render(width)
            .iter()
            .map(|line| crate::ansi::strip_terminal_sequences(line))
            .collect()
    }

    #[test]
    fn renders_labels_and_values_aligned() {
        let lines = plain(&list(5), 40);
        // Labels align to the widest label (6 cells: "Stream").
        assert_eq!(lines[0].trim_end(), "→ Model   fast");
        assert!(lines[1].contains("Theme"));
    }

    #[test]
    fn description_and_hint_close_the_list() {
        let mut settings = list(5);
        settings.set_selected_index(1);
        let lines = plain(&settings, 40);
        let last = lines.last().unwrap();
        assert!(last.contains("Esc to cancel"));
        assert!(lines.iter().any(|line| line.contains("Colour scheme")));
    }

    #[test]
    fn empty_list_reports_no_settings() {
        let empty = SettingsList::new(Vec::new(), 5, SettingsListTheme::default());
        assert!(plain(&empty, 40)[0].contains("No settings available"));
    }

    #[test]
    fn enter_cycles_values() {
        let mut settings = list(5);
        assert_eq!(
            settings.handle_key("\r"),
            SettingsListAction::Changed {
                id: "model".into(),
                value: "slow".into()
            }
        );
        assert_eq!(settings.selected_item().unwrap().current_value, "slow");
        assert_eq!(
            settings.handle_key(" "),
            SettingsListAction::Changed {
                id: "model".into(),
                value: "fast".into()
            }
        );
    }

    #[test]
    fn settings_without_values_report_activation() {
        let mut settings = list(5);
        settings.set_selected_index(1);
        assert_eq!(
            settings.handle_key("\r"),
            SettingsListAction::Activated { id: "theme".into() }
        );
    }

    #[test]
    fn navigation_wraps() {
        let mut settings = list(5);
        assert_eq!(
            settings.handle_key("\x1b[A"),
            SettingsListAction::SelectionChanged
        );
        assert_eq!(settings.selected_index(), 2);
        assert_eq!(
            settings.handle_key("\x1b[B"),
            SettingsListAction::SelectionChanged
        );
        assert_eq!(settings.selected_index(), 0);
        assert_eq!(settings.handle_key("\x1b"), SettingsListAction::Cancelled);
    }

    #[test]
    fn update_value_and_select_item_reach_items_by_id() {
        let mut settings = list(5);
        settings.update_value("stream", "on");
        settings.select_item("stream");
        assert_eq!(settings.selected_index(), 2);
        assert_eq!(settings.selected_item().unwrap().current_value, "on");
    }

    #[test]
    fn scrolling_shows_a_position_indicator() {
        let mut settings = list(1);
        settings.set_selected_index(2);
        let lines = plain(&settings, 40);
        assert!(lines.iter().any(|line| line.contains("(3/3)")));
    }

    #[test]
    fn long_values_are_truncated() {
        let mut settings = SettingsList::new(
            vec![SettingItem::new("a", "A", "x".repeat(100))],
            5,
            SettingsListTheme::default(),
        );
        assert!(plain(&settings, 20)[0].len() <= 20);
        settings.update_value("a", "short");
        assert!(plain(&settings, 20)[0].contains("short"));
    }
}
