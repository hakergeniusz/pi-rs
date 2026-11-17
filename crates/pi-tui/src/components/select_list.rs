//! `SelectList`: a keyboard-navigable list with a scrolling window.
//! `packages/tui/src/components/select-list.ts`.
//!
//! Key ids come from pi's `tui.select.*` defaults (`keybindings.ts:147-158`)
//! and are matched with [`crate::keys::matches_key`]. The keybindings manager
//! itself is 4b; this component hard-codes the defaults pi ships.

use crate::ansi::{truncate_to_width, visible_width};
use crate::component::Component;
use crate::keys::matches_key;
use crate::mouse::{MouseButton, MouseEvent, MouseEventKind, MouseEventResult};

/// `tui.select.*` defaults. `keybindings.ts:147-158`.
pub mod select_keys {
    /// Move the selection up, wrapping past the top.
    pub const UP: &str = "up";
    /// Move the selection down, wrapping past the bottom.
    pub const DOWN: &str = "down";
    /// Move the selection up a page.
    pub const PAGE_UP: &str = "pageUp";
    /// Move the selection down a page.
    pub const PAGE_DOWN: &str = "pageDown";
    /// Confirm the current selection.
    pub const CONFIRM: &str = "enter";
    /// Cancel the list.
    pub const CANCEL: [&str; 2] = ["escape", "ctrl+c"];
}

/// Default primary-column width when the layout sets no bounds.
pub const DEFAULT_PRIMARY_COLUMN_WIDTH: usize = 32;
/// Columns between the primary column and the description.
pub const PRIMARY_COLUMN_GAP: usize = 2;
/// The description renders only when more columns than this remain.
pub const MIN_DESCRIPTION_WIDTH: usize = 10;

/// One row. `select-list.ts:15-19`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectItem {
    /// The identity the host reads back; the filter matches its prefix.
    pub value: String,
    /// The rendered label; when empty, `value` is shown instead.
    pub label: String,
    /// Optional second-column text, shown on wide terminals.
    pub description: Option<String>,
}

impl SelectItem {
    /// An item with a `value` and a `label`; no description.
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            description: None,
        }
    }

    /// Builder: attach a description to the item.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// `select-list.ts:262-264`: the label wins, the value is the fallback.
    fn display_value(&self) -> &str {
        if self.label.is_empty() {
            &self.value
        } else {
            &self.label
        }
    }
}

/// Style hooks. `select-list.ts:24-30`.
#[derive(Clone)]
pub struct SelectListTheme {
    /// Style hook for the selected row's prefix; the renderer draws `→ ` itself.
    pub selected_prefix: std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>,
    /// Style hook for the whole selected row.
    pub selected_text: std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>,
    /// Style hook for the description column.
    pub description: std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>,
    /// Style hook for the `(selected/total)` scroll indicator.
    pub scroll_info: std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>,
    /// Style hook for the no-match message.
    pub no_match: std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>,
}

impl Default for SelectListTheme {
    fn default() -> Self {
        Self {
            selected_prefix: std::sync::Arc::new(|text: &str| text.to_string()),
            selected_text: std::sync::Arc::new(|text: &str| text.to_string()),
            description: std::sync::Arc::new(|text: &str| text.to_string()),
            scroll_info: std::sync::Arc::new(|text: &str| text.to_string()),
            no_match: std::sync::Arc::new(|text: &str| text.to_string()),
        }
    }
}

impl std::fmt::Debug for SelectListTheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SelectListTheme")
    }
}

/// Column bounds. `select-list.ts:36-41`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SelectListLayout {
    /// Lower bound for the primary column; `None` defers to the max bound.
    pub min_primary_column_width: Option<usize>,
    /// Upper bound for the primary column; `None` defers to the min bound.
    pub max_primary_column_width: Option<usize>,
}

/// What a key press did. `select-list.ts:22-26`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectListAction {
    /// The selection moved.
    SelectionChanged,
    /// Enter was pressed; the caller reads [`SelectList::selected_item`].
    Confirmed,
    /// Escape or Ctrl+C was pressed.
    Cancelled,
    /// The key was not a list key.
    None,
}

/// The list. `select-list.ts:42-...`.
pub struct SelectList {
    items: Vec<SelectItem>,
    filtered_items: Vec<usize>,
    selected_index: usize,
    max_visible: usize,
    theme: SelectListTheme,
    layout: SelectListLayout,
    /// Row the press landed on, so a click that drags onto another row still
    /// confirms the row the press started on. `select-list.ts:46`.
    mouse_pressed_index: Option<usize>,
    /// What the last mouse gesture asked the host to do. `select-list.ts:46-48`.
    pending_action: SelectListAction,
    /// Number of `onSelectionChange` notifications. Tests read this where pi
    /// reads a spy callback. `select-list.ts:48`.
    selection_change_count: u32,
}

impl SelectList {
    /// `select-list.ts:49-54`.
    pub fn new(
        items: Vec<SelectItem>,
        max_visible: usize,
        theme: SelectListTheme,
        layout: SelectListLayout,
    ) -> Self {
        let filtered_items = (0..items.len()).collect();
        Self {
            items,
            filtered_items,
            selected_index: 0,
            max_visible,
            theme,
            layout,
            mouse_pressed_index: None,
            pending_action: SelectListAction::None,
            selection_change_count: 0,
        }
    }

    /// `select-list.ts:56-60`.
    pub fn set_filter(&mut self, filter: &str) {
        let filter = filter.to_lowercase();
        self.filtered_items = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.value.to_lowercase().starts_with(&filter))
            .map(|(index, _)| index)
            .collect();
        self.selected_index = 0;
    }

    /// `select-list.ts:62-64`.
    pub fn set_selected_index(&mut self, index: usize) {
        if self.filtered_items.is_empty() {
            self.selected_index = 0;
            return;
        }
        self.selected_index = index.min(self.filtered_items.len() - 1);
    }

    /// `select-list.ts:271-274`.
    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.filtered_items
            .get(self.selected_index)
            .map(|index| &self.items[*index])
    }

    /// What the last mouse gesture asked for. A click sets
    /// [`SelectListAction::Confirmed`]; the host reads it after
    /// `handle_mouse` the way pi's `onSelect` callback fires.
    pub fn take_action(&mut self) -> SelectListAction {
        std::mem::replace(&mut self.pending_action, SelectListAction::None)
    }

    /// How many times the selection changed. `select-list.ts:118`, `:137`.
    pub fn selection_change_count(&self) -> u32 {
        self.selection_change_count
    }

    /// `notifySelectionChange`, `select-list.ts:265-267`.
    fn notify_selection_change(&mut self) {
        self.selection_change_count = self.selection_change_count.saturating_add(1);
    }

    /// The selected row's position in the filtered list.
    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// How many items survive the filter.
    pub fn filtered_len(&self) -> usize {
        self.filtered_items.len()
    }

    /// `select-list.ts:169-177`. The window is centred on the selection.
    fn visible_range(&self) -> (usize, usize) {
        let count = self.filtered_items.len();
        let start_index = self
            .selected_index
            .saturating_sub(self.max_visible / 2)
            .min(count.saturating_sub(self.max_visible));
        (start_index, (start_index + self.max_visible).min(count))
    }

    /// `select-list.ts:246-258`.
    fn primary_column_bounds(&self) -> (usize, usize) {
        let raw_min = self
            .layout
            .min_primary_column_width
            .or(self.layout.max_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        let raw_max = self
            .layout
            .max_primary_column_width
            .or(self.layout.min_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        (raw_min.min(raw_max).max(1), raw_min.max(raw_max).max(1))
    }

    /// `select-list.ts:239-244`.
    fn primary_column_width(&self) -> usize {
        let (min, max) = self.primary_column_bounds();
        let widest = self
            .filtered_items
            .iter()
            .map(|index| visible_width(self.items[*index].display_value()) + PRIMARY_COLUMN_GAP)
            .max()
            .unwrap_or(0);
        widest.clamp(min, max)
    }

    /// `select-list.ts:13`: descriptions collapse to one line.
    fn single_line(text: &str) -> String {
        text.split(['\r', '\n'])
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string()
    }

    /// `select-list.ts:179-217`.
    fn render_item(
        &self,
        item: &SelectItem,
        is_selected: bool,
        width: usize,
        description: Option<&String>,
        primary_column_width: usize,
    ) -> String {
        let prefix = if is_selected { "→ " } else { "  " };
        let prefix_width = visible_width(prefix);

        if let Some(description) = description && width > 40 {
            let effective_column = primary_column_width
                .min(width.saturating_sub(prefix_width + 4))
                .max(1);
            let max_primary_width = effective_column.saturating_sub(PRIMARY_COLUMN_GAP).max(1);
            let truncated_value =
                truncate_to_width(item.display_value(), max_primary_width, "", false);
            let value_width = visible_width(&truncated_value);
            let spacing = " ".repeat(effective_column.saturating_sub(value_width).max(1));
            let description_start = prefix_width + value_width + spacing.len();
            let remaining_width = width as i64 - description_start as i64 - 2;
            if remaining_width > MIN_DESCRIPTION_WIDTH as i64 {
                let truncated_desc =
                    truncate_to_width(description, remaining_width as usize, "", false);
                if is_selected {
                    return (self.theme.selected_text)(&format!(
                        "{prefix}{truncated_value}{spacing}{truncated_desc}"
                    ));
                }
                return format!(
                    "{prefix}{truncated_value}{}",
                    (self.theme.description)(&format!("{spacing}{truncated_desc}"))
                );
            }
        }

        let max_width = width.saturating_sub(prefix_width + 2);
        let truncated_value = truncate_to_width(item.display_value(), max_width, "", false);
        if is_selected {
            return (self.theme.selected_text)(&format!("{prefix}{truncated_value}"));
        }
        format!("{prefix}{truncated_value}")
    }
}

impl Component for SelectList {
    /// `select-list.ts:71-110`.
    fn render(&self, width: usize) -> Vec<String> {
        if self.filtered_items.is_empty() {
            return vec![(self.theme.no_match)("  No matching commands")];
        }
        let primary_column_width = self.primary_column_width();
        let (start_index, end_index) = self.visible_range();
        let mut lines: Vec<String> = Vec::new();
        for position in start_index..end_index {
            let item = &self.items[self.filtered_items[position]];
            let description = item
                .description
                .as_ref()
                .map(|text| Self::single_line(text));
            lines.push(self.render_item(
                item,
                position == self.selected_index,
                width,
                description.as_ref(),
                primary_column_width,
            ));
        }
        if start_index > 0 || end_index < self.filtered_items.len() {
            let scroll_text = format!(
                "  ({}/{})",
                self.selected_index + 1,
                self.filtered_items.len()
            );
            lines.push((self.theme.scroll_info)(&truncate_to_width(
                &scroll_text,
                width.saturating_sub(2),
                "",
                false,
            )));
        }
        lines
    }

    /// `select-list.ts:151-167`. Mouse handling is 5c; keys are ported.
    fn handle_input(&mut self, data: &str) -> Option<crate::component::ComponentMutation> {
        let action = self.handle_key(data);
        Some(crate::component::ComponentMutation {
            consume: action != SelectListAction::None,
        })
    }

    /// `SelectList.handleMouse`, `select-list.ts:109-142`.
    ///
    /// Hover deliberately does nothing: the visible range is centred on the
    /// selection, so a pointer passing over the list must not move it.
    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        if self.filtered_items.is_empty() {
            return None;
        }
        if event.kind == MouseEventKind::Wheel {
            let delta = event.wheel_delta?;
            if delta == 0 {
                return None;
            }
            let step = if delta < 0 { -1i64 } else { 1 };
            let previous_index = self.selected_index;
            let next =
                (self.selected_index as i64 + step).clamp(0, self.filtered_items.len() as i64 - 1);
            self.selected_index = next as usize;
            let changed = self.selected_index != previous_index;
            if changed {
                self.notify_selection_change();
            }
            return Some(MouseEventResult {
                handled: Some(true),
                render: Some(changed),
                ..MouseEventResult::EMPTY
            });
        }
        if event.button != MouseButton::Left
            || !matches!(event.kind, MouseEventKind::Press | MouseEventKind::Click)
        {
            return None;
        }
        let (start_index, end_index) = self.visible_range();
        if event.y < 0 {
            return None;
        }
        let item_index = start_index + event.y as usize;
        if item_index < start_index || item_index >= end_index {
            return None;
        }

        if event.kind == MouseEventKind::Press {
            self.mouse_pressed_index = Some(item_index);
            if self.selected_index != item_index {
                self.selected_index = item_index;
                self.notify_selection_change();
            }
            return Some(MouseEventResult::focus());
        }

        let clicked_index = self.mouse_pressed_index.take().unwrap_or(item_index);
        let changed = self.selected_index != clicked_index;
        self.selected_index = clicked_index;
        if changed {
            self.notify_selection_change();
        }
        self.pending_action = SelectListAction::Confirmed;
        Some(MouseEventResult::handled())
    }

    fn has_input_handler(&self) -> bool {
        true
    }
}

impl SelectList {
    /// The action a key produced, for hosts that drive their own loop.
    /// `select-list.ts:151-167`.
    pub fn handle_key(&mut self, data: &str) -> SelectListAction {
        if matches_key(data, select_keys::UP) {
            if !self.filtered_items.is_empty() {
                self.selected_index = if self.selected_index == 0 {
                    self.filtered_items.len() - 1
                } else {
                    self.selected_index - 1
                };
            }
            return SelectListAction::SelectionChanged;
        }
        if matches_key(data, select_keys::DOWN) {
            if !self.filtered_items.is_empty() {
                self.selected_index = if self.selected_index == self.filtered_items.len() - 1 {
                    0
                } else {
                    self.selected_index + 1
                };
            }
            return SelectListAction::SelectionChanged;
        }
        if matches_key(data, select_keys::PAGE_UP) || matches_key(data, select_keys::PAGE_DOWN) {
            if self.filtered_items.is_empty() {
                return SelectListAction::None;
            }
            let step = self.max_visible.max(1);
            let delta = if matches_key(data, select_keys::PAGE_UP) {
                -(step as i64)
            } else {
                step as i64
            };
            let next =
                (self.selected_index as i64 + delta).rem_euclid(self.filtered_items.len() as i64);
            self.selected_index = next as usize;
            return SelectListAction::SelectionChanged;
        }
        if matches_key(data, select_keys::CONFIRM) {
            return SelectListAction::Confirmed;
        }
        if select_keys::CANCEL.iter().any(|key| matches_key(data, key)) {
            return SelectListAction::Cancelled;
        }
        SelectListAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::strip_terminal_sequences;

    fn items() -> Vec<SelectItem> {
        vec![
            SelectItem::new("alpha", "Alpha"),
            SelectItem::new("beta", "Beta").with_description("The second one"),
            SelectItem::new("gamma", "Gamma"),
        ]
    }

    fn list(max_visible: usize) -> SelectList {
        SelectList::new(
            items(),
            max_visible,
            SelectListTheme::default(),
            SelectListLayout::default(),
        )
    }

    fn plain(list: &SelectList, width: usize) -> Vec<String> {
        list.render(width)
            .iter()
            .map(|line| crate::ansi::strip_terminal_sequences(line))
            .collect()
    }

    #[test]
    fn renders_every_item_with_a_cursor_prefix() {
        let lines = plain(&list(5), 30);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("→ Alpha"));
        assert!(lines[1].starts_with("  Beta"));
    }

    #[test]
    fn empty_list_shows_the_no_match_message() {
        let mut list = list(5);
        list.set_filter("zzz");
        assert_eq!(plain(&list, 40), vec!["  No matching commands"]);
    }

    #[test]
    fn filter_prefixes_on_the_value_and_resets_selection() {
        let mut list = list(5);
        list.set_selected_index(2);
        list.set_filter("g");
        assert_eq!(list.filtered_len(), 1);
        assert_eq!(list.selected_index(), 0);
        assert_eq!(list.selected_item().unwrap().value, "gamma");
    }

    #[test]
    fn up_and_down_wrap_around() {
        let mut list = list(5);
        assert_eq!(
            list.handle_key("\x1b[A"),
            SelectListAction::SelectionChanged
        );
        assert_eq!(list.selected_index(), 2);
        assert_eq!(
            list.handle_key("\x1b[B"),
            SelectListAction::SelectionChanged
        );
        assert_eq!(list.selected_index(), 0);
    }

    #[test]
    fn enter_and_escape_are_recognised() {
        let mut list = list(5);
        assert_eq!(list.handle_key("\r"), SelectListAction::Confirmed);
        assert_eq!(list.handle_key("\x1b"), SelectListAction::Cancelled);
        assert_eq!(list.handle_key("\x03"), SelectListAction::Cancelled);
        assert_eq!(list.handle_key("z"), SelectListAction::None);
    }

    #[test]
    fn visible_window_scrolls_with_the_selection() {
        let mut list = list(2);
        assert_eq!(plain(&list, 30)[0].trim(), "→ Alpha");
        list.set_selected_index(2);
        let lines = plain(&list, 30);
        assert!(lines[0].trim().ends_with("Beta"));
        assert!(lines[1].trim().starts_with("→ Gamma"));
        assert!(lines[2].contains("(3/3)"));
    }

    #[test]
    fn scroll_indicator_shows_when_scrolled() {
        let mut list = list(2);
        list.set_selected_index(0);
        let lines = plain(&list, 30);
        assert!(lines.last().unwrap().contains("(1/3)"));
    }

    #[test]
    fn descriptions_get_their_own_column_on_wide_terminals() {
        let mut list = list(5);
        list.set_selected_index(1);
        let line = strip_terminal_sequences(&list.render(80)[1]);
        assert!(line.contains("Beta"));
        assert!(line.contains("The second one"));
    }

    #[test]
    fn narrow_terminals_drop_the_description_column() {
        let list = list(5);
        let line = strip_terminal_sequences(&list.render(20)[1]);
        assert!(!line.contains("The second one"));
    }

    #[test]
    fn long_labels_are_truncated_to_the_viewport() {
        let mut list = SelectList::new(
            vec![SelectItem::new("x".repeat(200), "y".repeat(200))],
            5,
            SelectListTheme::default(),
            SelectListLayout::default(),
        );
        let line = strip_terminal_sequences(&list.render(10)[0]);
        assert!(visible_width(&line) <= 10);
        list.set_filter("nope");
        assert_eq!(list.filtered_len(), 0);
    }
}
