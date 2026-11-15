//! `SettingsList`: label/value rows with a description pane.
//! `packages/tui/src/components/settings-list.ts`.
//!
//! Ported: the list rendering, selection, value cycling, the hint line, the
//! search box (`options.enableSearch`), and value submenus (a factory builds
//! a component the list renders in place of itself until its done callback
//! closes it).

use std::sync::{Arc, Mutex};

use crate::ansi::{truncate_to_width, visible_width, wrap_text_with_ansi};
use crate::component::Component;
use crate::components::input::{Input, InputOptions};
use crate::components::select_list::select_keys;
use crate::fuzzy::fuzzy_filter;
use crate::keys::matches_key;
use crate::mouse::{MouseButton, MouseEvent, MouseEventKind, MouseEventResult};

/// One setting. `settings-list.ts:9-25`.
#[derive(Clone, Default)]
pub struct SettingItem {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub current_value: String,
    /// When set, Enter/Space cycles these values.
    pub values: Option<Vec<String>>,
    /// When set, Enter opens this component instead of cycling; it stays up
    /// until the done callback closes it. `settings-list.ts:16-19`.
    pub submenu: Option<SubmenuFactory>,
}

impl std::fmt::Debug for SettingItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingItem")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("description", &self.description)
            .field("current_value", &self.current_value)
            .field("values", &self.values)
            .field("submenu", &self.submenu.is_some())
            .finish()
    }
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
            submenu: None,
        }
    }

    #[must_use]
    pub fn with_values(mut self, values: Vec<String>) -> Self {
        // The caller's `current_value` stands even when it is not the first
        // option — pi builds items with the live value and the value list
        // separately (`settings-list.ts:9-25`).
        self.values = Some(values);
        self
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    #[must_use]
    pub fn with_submenu(mut self, submenu: SubmenuFactory) -> Self {
        self.submenu = Some(submenu);
        self
    }
}

/// What a submenu reports when it finishes: the new value (`None` just
/// closes — a cancel), plus an item id to select and re-activate after
/// closing. `settings-list.ts:212-221`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubmenuClose {
    pub value: Option<String>,
    pub navigate_to: Option<String>,
}

/// The done callback pi passes into a submenu factory (`done` in
/// `settings-list.ts:212`). Rust note: the callback cannot borrow the list,
/// so it writes into a shared mailbox that the list drains right after
/// forwarding input — the same tick upstream's direct call lands in.
#[derive(Clone)]
pub struct SubmenuDone(Arc<Mutex<Option<SubmenuClose>>>);

impl Default for SubmenuDone {
    fn default() -> Self {
        Self::new()
    }
}

impl SubmenuDone {
    /// An empty mailbox. Hosts and tests create one to hand to a factory.
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    /// `done(selectedValue?, options?)` (`settings-list.ts:212-221`).
    pub fn close(&self, value: Option<&str>, navigate_to: Option<&str>) {
        *self.0.lock().expect("submenu done") = Some(SubmenuClose {
            value: value.map(str::to_string),
            navigate_to: navigate_to.map(str::to_string),
        });
    }

    /// Take the pending close, if any. Tests and hosts read what a submenu
    /// delivered.
    pub fn take_close(&self) -> Option<SubmenuClose> {
        self.0.lock().expect("submenu done").take()
    }
}

/// Builds the submenu component for an item:
/// `(currentValue, done) => Component` (`settings-list.ts:16-19`).
pub type SubmenuFactory = Arc<dyn Fn(&str, SubmenuDone) -> Box<dyn Component> + Send + Sync>;

/// A style hook: the label text plus whether the row is selected.
pub type SettingsStyleFn = Arc<dyn Fn(&str, bool) -> String + Send + Sync>;

/// Style hooks. `settings-list.ts:27-34`.
#[derive(Clone)]
pub struct SettingsListTheme {
    pub label: SettingsStyleFn,
    pub value: SettingsStyleFn,
    pub description: Arc<dyn Fn(&str) -> String + Send + Sync>,
    pub cursor: String,
    pub hint: Arc<dyn Fn(&str) -> String + Send + Sync>,
}

impl Default for SettingsListTheme {
    fn default() -> Self {
        Self {
            label: Arc::new(|text: &str, _selected: bool| text.to_string()),
            value: Arc::new(|text: &str, _selected: bool| text.to_string()),
            description: Arc::new(|text: &str| text.to_string()),
            cursor: "→ ".to_string(),
            hint: Arc::new(|text: &str| text.to_string()),
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
    /// A value cycled or a submenu delivered one: (id, new value).
    Changed {
        id: String,
        value: String,
    },
    /// A setting without values or submenu was activated: nothing to apply.
    Activated {
        id: String,
    },
    Cancelled,
    None,
}

/// The settings list. `settings-list.ts:36-...`.
pub struct SettingsList {
    items: Vec<SettingItem>,
    /// The search-filtered view: indices into `items`, in relevance order.
    /// Upstream's `filteredItems` holds the same object references as
    /// `items`, so its value updates are visible through both; the port
    /// tracks indices for the same effect. `settings-list.ts:10`.
    filtered: Vec<usize>,
    selected_index: usize,
    max_visible: usize,
    theme: SettingsListTheme,
    search_enabled: bool,
    search_input: Option<Input>,
    /// The open submenu, rendered in place of the list. `settings-list.ts:18`.
    submenu_component: Option<Box<dyn Component>>,
    /// The done mailbox of the open submenu.
    submenu_done: Option<SubmenuDone>,
    /// The id of the item that opened the submenu — captured at open so the
    /// close applies to the right row even in the filtered view.
    submenu_item_id: Option<String>,
    /// The list index of the item that opened the submenu, restored on
    /// close. `settings-list.ts:19`.
    submenu_item_index: Option<usize>,
    /// After a close with `navigateTo`: select and re-activate that item.
    /// `settings-list.ts:20`.
    navigate_after_close: Option<String>,
    /// Row the press landed on; a click confirms it even if the pointer moved.
    /// `settings-list.ts:74`.
    mouse_pressed_index: Option<usize>,
    /// What the last mouse gesture asked the host to do.
    pending_action: SettingsListAction,
}

impl SettingsList {
    /// `settings-list.ts:21-32` (without `options.enableSearch`).
    pub fn new(items: Vec<SettingItem>, max_visible: usize, theme: SettingsListTheme) -> Self {
        Self {
            filtered: (0..items.len()).collect(),
            search_enabled: false,
            search_input: None,
            items,
            selected_index: 0,
            max_visible,
            theme,
            submenu_component: None,
            submenu_done: None,
            submenu_item_id: None,
            submenu_item_index: None,
            navigate_after_close: None,
            mouse_pressed_index: None,
            pending_action: SettingsListAction::None,
        }
    }

    /// `options.enableSearch` (`settings-list.ts:28-31`): a search `Input`
    /// above the list; typing filters it.
    #[must_use]
    pub fn with_search(mut self) -> Self {
        self.search_enabled = true;
        self.search_input = Some(Input::new(InputOptions::default()));
        self
    }

    /// What the last mouse gesture asked for, where pi's `onChange` fires.
    pub fn take_action(&mut self) -> SettingsListAction {
        std::mem::replace(&mut self.pending_action, SettingsListAction::None)
    }

    /// Update an item's currentValue. `settings-list.ts:34-39`.
    pub fn update_value(&mut self, id: &str, new_value: &str) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            item.current_value = new_value.to_string();
        }
    }

    /// Move selection to the item with the given id (no-op if not found).
    /// `settings-list.ts:41-47`: the search view is searched when enabled.
    pub fn select_item(&mut self, id: &str) {
        let items = self.display_items();
        if let Some(index) = items.iter().position(|item| item.id == id) {
            self.selected_index = index;
        }
    }

    /// Move the selection by index. Tests and hosts use this; pi exposes the
    /// same thing through mouse handling.
    pub fn set_selected_index(&mut self, index: usize) {
        self.selected_index = index.min(self.items.len().saturating_sub(1));
    }

    pub fn selected_item(&self) -> Option<&SettingItem> {
        self.display_items().get(self.selected_index).copied()
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// The items the list currently shows: the search view when enabled.
    /// `settings-list.ts:198-200`.
    fn display_items(&self) -> Vec<&SettingItem> {
        if self.search_enabled {
            self.filtered
                .iter()
                .filter_map(|index| self.items.get(*index))
                .collect()
        } else {
            self.items.iter().collect()
        }
    }

    /// `settings-list.ts:201-204`.
    fn visible_range(&self) -> (usize, usize) {
        let count = self.display_items().len();
        let start_index = self
            .selected_index
            .saturating_sub(self.max_visible / 2)
            .min(count.saturating_sub(self.max_visible));
        (start_index, (start_index + self.max_visible).min(count))
    }

    /// `settings-list.ts:205-231`: open the submenu, or cycle the values.
    fn activate_item(&mut self) -> SettingsListAction {
        let display = self.display_items();
        let (id, current_value, submenu) = {
            let Some(item) = display.get(self.selected_index) else {
                return SettingsListAction::None;
            };
            (
                item.id.clone(),
                item.current_value.clone(),
                item.submenu.clone(),
            )
        };
        if let Some(factory) = submenu {
            // Open submenu, passing current value so it can pre-select correctly.
            self.submenu_item_id = Some(id.clone());
            self.submenu_item_index = Some(self.selected_index);
            let done = SubmenuDone::new();
            self.submenu_done = Some(done.clone());
            self.submenu_component = Some(factory(&current_value, done));
            return SettingsListAction::None;
        }
        let Some(item) = self.items.iter_mut().find(|item| item.id == id) else {
            return SettingsListAction::None;
        };
        let Some(values) = &item.values else {
            self.pending_action = SettingsListAction::Activated { id: id.clone() };
            return SettingsListAction::Activated { id };
        };
        if values.is_empty() {
            return SettingsListAction::None;
        }
        let current_index = values
            .iter()
            .position(|value| *value == item.current_value)
            .map_or(0, |index| index + 1);
        let next_index = current_index % values.len();
        let new_value = values[next_index].clone();
        item.current_value.clone_from(&new_value);
        self.pending_action = SettingsListAction::Changed {
            id: id.clone(),
            value: new_value.clone(),
        };
        SettingsListAction::Changed {
            id,
            value: new_value,
        }
    }

    /// `settings-list.ts:232-247`: apply the close — the delivered value
    /// first (pi's `done` sets `currentValue` and fires `onChange` before
    /// closing), then either the navigation target or the selection restore.
    fn close_submenu(&mut self, close: SubmenuClose) -> SettingsListAction {
        let item_id = self.submenu_item_id.clone();
        self.submenu_component = None;
        self.submenu_done = None;
        self.submenu_item_id = None;
        let mut action = SettingsListAction::None;
        if let (Some(id), Some(value)) = (&item_id, &close.value) {
            self.update_value(id, value);
            action = SettingsListAction::Changed {
                id: id.clone(),
                value: value.clone(),
            };
        }
        if let Some(target) = close.navigate_to {
            self.navigate_after_close = Some(target);
        }
        if self.navigate_after_close.is_some() {
            let id = self.navigate_after_close.take().expect("navigate target");
            self.submenu_item_index = None;
            self.select_item(&id);
            // Open the target item's submenu automatically.
            self.activate_item();
        } else if let Some(index) = self.submenu_item_index.take() {
            // Restore selection to the item that opened the submenu.
            self.selected_index = index;
        }
        action
    }

    /// Drain the open submenu's done mailbox after forwarding input to it.
    fn drain_submenu(&mut self) -> SettingsListAction {
        let Some(done) = &self.submenu_done else {
            return SettingsListAction::None;
        };
        match done.take_close() {
            Some(close) => self.close_submenu(close),
            None => SettingsListAction::None,
        }
    }

    /// `settings-list.ts:248-251`.
    fn apply_filter(&mut self, query: &str) {
        let pairs: Vec<(usize, &SettingItem)> = self.items.iter().enumerate().collect();
        self.filtered = fuzzy_filter(&pairs, query, |(_, item)| item.label.clone())
            .into_iter()
            .map(|(index, _)| index)
            .collect();
        self.selected_index = 0;
    }

    /// `settings-list.ts:160-197`.
    pub fn handle_key(&mut self, data: &str) -> SettingsListAction {
        // If submenu is active, delegate all input to it. The submenu's
        // onCancel (triggered by escape) will call done() which closes it.
        if self.submenu_component.is_some() {
            if let Some(component) = self.submenu_component.as_mut() {
                component.handle_input(data);
            }
            return self.drain_submenu();
        }
        let display_count = self.display_items().len();
        if matches_key(data, select_keys::UP) {
            if display_count == 0 {
                return SettingsListAction::None;
            }
            self.selected_index = if self.selected_index == 0 {
                display_count - 1
            } else {
                self.selected_index - 1
            };
            return SettingsListAction::SelectionChanged;
        }
        if matches_key(data, select_keys::DOWN) {
            if display_count == 0 {
                return SettingsListAction::None;
            }
            self.selected_index = if self.selected_index == display_count - 1 {
                0
            } else {
                self.selected_index + 1
            };
            return SettingsListAction::SelectionChanged;
        }
        if matches_key(data, select_keys::PAGE_UP) || matches_key(data, select_keys::PAGE_DOWN) {
            if display_count == 0 {
                return SettingsListAction::None;
            }
            let step = self.max_visible.max(1);
            let delta = if matches_key(data, select_keys::PAGE_UP) {
                -(step as i64)
            } else {
                step as i64
            };
            let next = (self.selected_index as i64 + delta).rem_euclid(display_count as i64);
            self.selected_index = next as usize;
            return SettingsListAction::SelectionChanged;
        }
        let search_empty = self
            .search_input
            .as_ref()
            .is_none_or(|input| input.value().is_empty());
        if matches_key(data, select_keys::CONFIRM) || (data == " " && search_empty) {
            return self.activate_item();
        }
        if select_keys::CANCEL.iter().any(|key| matches_key(data, key)) {
            return SettingsListAction::Cancelled;
        }
        if self.search_enabled {
            if let Some(input) = self.search_input.as_mut() {
                input.handle_key(data);
            }
            let query = self
                .search_input
                .as_ref()
                .map_or(String::new(), |input| input.value().to_string());
            self.apply_filter(&query);
        }
        SettingsListAction::None
    }

    /// `settings-list.ts:252-257`.
    fn hint_line(&self, width: usize) -> String {
        let text = if self.search_enabled {
            "  Type to search · Enter/Space to change · Esc to cancel"
        } else {
            "  Enter/Space to change · Esc to cancel"
        };
        truncate_to_width(&(self.theme.hint)(text), width, "", false)
    }
}

impl Component for SettingsList {
    /// `settings-list.ts:51-116`.
    fn render(&self, width: usize) -> Vec<String> {
        // If submenu is active, render it instead.
        if let Some(submenu) = &self.submenu_component {
            return submenu.render(width);
        }
        let mut lines: Vec<String> = Vec::new();
        if self.search_enabled {
            if let Some(input) = &self.search_input {
                lines.extend(input.render(width));
            }
            lines.push(String::new());
        }
        let display_items = self.display_items();
        if self.items.is_empty() {
            lines.push((self.theme.hint)("  No settings available"));
            lines.push(String::new());
            lines.push(self.hint_line(width));
            return lines;
        }
        if display_items.is_empty() {
            lines.push(truncate_to_width(
                &(self.theme.hint)("  No matching settings"),
                width,
                "",
                false,
            ));
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

        for (position, item) in display_items
            .iter()
            .enumerate()
            .skip(start_index)
            .take(end_index.saturating_sub(start_index))
        {
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

        if start_index > 0 || end_index < display_items.len() {
            let scroll_text = format!("  ({}/{})", self.selected_index + 1, display_items.len());
            lines.push((self.theme.hint)(&truncate_to_width(
                &scroll_text,
                width.saturating_sub(2),
                "",
                false,
            )));
        }

        if let Some(description) = display_items[self.selected_index].description.as_ref() {
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

    /// `SettingsList.handleMouse`, `settings-list.ts:117-159`.
    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        // While a submenu is open it owns the pointer.
        if let Some(submenu) = self.submenu_component.as_mut() {
            let result = submenu.handle_mouse(event)?;
            let mut focused = MouseEventResult::focus();
            focused.handled = result.handled;
            focused.render = result.render;
            return Some(focused);
        }
        let row_offset = if self.search_enabled { 2 } else { 0 };
        if self.search_enabled {
            match event.y {
                0 => {
                    let input = self.search_input.as_mut()?;
                    let result = input.handle_mouse(event)?;
                    let mut focused = MouseEventResult::focus();
                    focused.handled = result.handled;
                    focused.render = result.render;
                    return Some(focused);
                }
                1 => return None,
                _ => {}
            }
        }
        let display_items = self.display_items();
        if display_items.is_empty() {
            return None;
        }
        if event.kind == MouseEventKind::Wheel {
            let delta = event.wheel_delta?;
            if delta == 0 {
                return None;
            }
            let step = if delta < 0 { -1i64 } else { 1 };
            let previous_index = self.selected_index;
            let next = (self.selected_index as i64 + step).clamp(0, display_items.len() as i64 - 1);
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
        let item_y = event.y - row_offset;
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

    // -- submenu -----------------------------------------------------------

    /// A submenu that echoes its key presses as values and closes on Esc.
    fn echo_factory() -> SubmenuFactory {
        Arc::new(|current_value: &str, done: SubmenuDone| {
            Box::new(EchoSubmenu {
                lines: vec![format!("submenu for {current_value}")],
                done,
            }) as Box<dyn Component>
        })
    }

    /// Minimal test submenu: renders one line, Enter closes with the typed
    /// key as the value, Esc closes without a value.
    struct EchoSubmenu {
        lines: Vec<String>,
        done: SubmenuDone,
    }

    impl Component for EchoSubmenu {
        fn render(&self, _width: usize) -> Vec<String> {
            self.lines.clone()
        }
        fn handle_input(&mut self, data: &str) -> Option<crate::component::ComponentMutation> {
            if data == "\x1b" {
                self.done.close(None, None);
            } else if data == "\r" {
                self.done.close(Some("picked"), None);
            } else if data == "n" {
                self.done.close(Some("picked"), Some("mode"));
            }
            Some(crate::component::ComponentMutation { consume: true })
        }
    }

    fn submenu_list() -> SettingsList {
        SettingsList::new(
            vec![
                SettingItem::new("theme", "Theme", "dark").with_submenu(echo_factory()),
                SettingItem::new("mode", "Mode", "auto")
                    .with_values(vec!["auto".into(), "manual".into()]),
            ],
            5,
            SettingsListTheme::default(),
        )
    }

    #[test]
    fn activating_a_submenu_renders_it_instead_of_the_list() {
        let mut settings = submenu_list();
        assert!(plain(&settings, 40)[0].contains("Theme"));
        settings.handle_key("\r"); // open the submenu on "Theme"
        let lines = plain(&settings, 40);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("submenu for dark"), "{lines:?}");
    }

    #[test]
    fn submenu_done_with_a_value_updates_the_item_and_fires_changed() {
        let mut settings = submenu_list();
        settings.handle_key("\r"); // open
        // Forwarded into the submenu; Enter makes it close with "picked".
        assert_eq!(
            settings.handle_key("\r"),
            SettingsListAction::Changed {
                id: "theme".into(),
                value: "picked".into()
            }
        );
        // The submenu closed and the selection restored to the opener.
        assert!(plain(&settings, 40)[0].contains("Theme"));
        assert_eq!(settings.selected_index(), 0);
        assert_eq!(settings.selected_item().unwrap().current_value, "picked");
    }

    #[test]
    fn submenu_done_without_a_value_just_closes() {
        let mut settings = submenu_list();
        settings.handle_key("\r"); // open
        assert_eq!(settings.handle_key("\x1b"), SettingsListAction::None);
        assert!(plain(&settings, 40)[0].contains("Theme"));
        assert_eq!(settings.selected_item().unwrap().current_value, "dark");
    }

    #[test]
    fn navigate_to_selects_and_reopens_the_target() {
        let open_mode = Arc::new(|current_value: &str, done: SubmenuDone| {
            Box::new(EchoSubmenu {
                lines: vec![format!("mode submenu {current_value}")],
                done,
            }) as Box<dyn Component>
        }) as SubmenuFactory;
        let mut settings = SettingsList::new(
            vec![
                SettingItem::new("theme", "Theme", "dark").with_submenu(echo_factory()),
                SettingItem::new("mode", "Mode", "auto").with_submenu(open_mode),
            ],
            5,
            SettingsListTheme::default(),
        );
        settings.handle_key("\r"); // open "theme" submenu
        // The test submenu closes with navigate_to "mode"; the list selects
        // it and opens its submenu in the same tick.
        settings.handle_key("n");
        let lines = plain(&settings, 40);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("mode submenu auto"), "{lines:?}");
        assert_eq!(settings.selected_index(), 1);
    }

    // -- search ------------------------------------------------------------

    #[test]
    fn search_filters_the_list_and_resets_the_selection() {
        let mut settings = list(5).with_search();
        settings.handle_key("st"); // types into the search input
        let lines = plain(&settings, 40);
        assert!(lines[0].contains("> st"), "{lines:?}");
        assert!(lines.iter().any(|line| line.contains("Stream")));
        assert!(!lines.iter().any(|line| line.contains("Theme")));
        assert_eq!(settings.selected_index(), 0);
        assert!(
            plain(&settings, 40)
                .last()
                .unwrap()
                .contains("Type to search")
        );
    }

    #[test]
    fn no_match_reports_it_and_keeps_the_hint() {
        let mut settings = list(5).with_search();
        settings.handle_key("zzz");
        // Wide enough that the hint is not truncated away.
        let lines = plain(&settings, 70);
        assert!(lines[2].contains("No matching settings"), "{lines:?}");
        assert!(lines.last().unwrap().contains("Esc to cancel"));
    }

    #[test]
    fn navigation_keys_go_to_the_list_while_searching() {
        let mut settings = list(5).with_search();
        // Down moves the selection without typing "B".
        assert_eq!(
            settings.handle_key("\x1b[B"),
            SettingsListAction::SelectionChanged
        );
        assert_eq!(settings.selected_index(), 1);
        // Space with an empty search activates; typing first makes it type.
        assert_eq!(settings.handle_key("t"), SettingsListAction::None);
        assert_eq!(
            settings.handle_key(" "),
            SettingsListAction::None,
            "space with search text types a space instead of activating"
        );
        assert!(
            settings
                .search_input
                .as_ref()
                .unwrap()
                .value()
                .ends_with("t ")
        );
    }

    #[test]
    fn mouse_rows_shift_below_the_search_box() {
        let mut settings = list(5).with_search();
        // Row 2 is the first item row (0: input, 1: blank).
        let event = MouseEvent::new(MouseEventKind::Click, 0, 4, 2, 40, 10);
        settings.handle_mouse(&event);
        assert_eq!(settings.selected_index(), 0);
        assert_eq!(
            settings.take_action(),
            SettingsListAction::Changed {
                id: "model".into(),
                value: "slow".into()
            }
        );
    }

    #[test]
    fn page_keys_move_by_max_visible() {
        let mut settings = SettingsList::new(items(), 2, SettingsListTheme::default());
        assert_eq!(
            settings.handle_key("\x1b[6~"),
            SettingsListAction::SelectionChanged
        );
        assert_eq!(settings.selected_index(), 2);
        assert_eq!(
            settings.handle_key("\x1b[5~"),
            SettingsListAction::SelectionChanged
        );
        assert_eq!(settings.selected_index(), 0);
    }
}
